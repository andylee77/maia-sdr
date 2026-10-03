//! P25 Phase 1 LSM (CQPSK) demodulation in software, from a 50 kSPS lane's IQ: a port of
//! SDRTrunk's `P25P1DecoderLSM` (decimate to 25 kSPS, baseband low-pass, RRC) and
//! `P25P1DemodulatorLSM` (AGC, differential demodulation of each symbol and of the sample half a
//! symbol before it, a Costas loop, Gardner timing on linearly interpolated samples), with the
//! soft sync detection of `P25P1MessageFramer.processWithSoftSyncDetect`. Names follow the Java so
//! the two can be read side by side.
//!
//! Two additions from the gateware LSM (change 059), for a traffic channel's gaps between
//! transmissions, where SDRTrunk's decision-directed loop walks on the noise and can stay at its
//! limit when the next transmission starts:
//! - **Hold:** below the AGC's idle gate the gain does not follow, and after a few such symbols
//!   the carrier loop and the symbol timing stop too, until the signal is back.
//! - **Clamp:** the carrier loop's limit is 0.65 rad, not π/3, so neither end absorbs near the
//!   operating point.

use super::c4fm::{sync_symbols, DibitSink, SoftSync, DEMOD_RATE_HZ, SYMBOL_RATE};
use crate::dsp::fsk4::{ideal_phase, linear, to_symbol, Fir};
use crate::dsp::taps::{HALFBAND_63, LPF_LSM_25K, RRC_TAPS_25K};

/// The carrier loop's limit, ±497 Hz (the gateware's `MAX_PLL_ABS`).
const MAX_PLL: f32 = 0.65;
/// SDRTrunk's limit, π/3 (±800 Hz).
const MAX_PLL_SDRTRUNK: f32 = std::f32::consts::FRAC_PI_3;
/// The AGC's idle gate, the gateware's `mag_update_threshold` (256 in Q1.15): a symbol sample
/// weaker than this is gap noise.
const SIGNAL_GATE: f32 = 256.0 / 32768.0;
/// Gated symbols in a row that start the hold, and symbols above the gate that end it.
const HOLD_ENTER_SYMBOLS: u32 = 4;
const HOLD_EXIT_SYMBOLS: u32 = 8;
const PLL_GAIN: f32 = 0.1;
const MAX_PHASE_ERROR: f32 = 0.3;
const OBJECTIVE_MAGNITUDE: f32 = 1.0;
const MAX_SAMPLE_GAIN: f32 = 500.0;
const SAMPLE_GAIN_SLEW: f32 = 0.05;
/// SDRTrunk `P25P1MessageFramer.SYNC_DETECTION_THRESHOLD`.
const SYNC_DETECTION_THRESHOLD: f32 = 60.0;

/// The loop's correction per symbol against the carrier's offset: `offset = SIGN · pll · 4800 /
/// 2π` is signal minus NCO. The loop reads NCO minus signal, as the gateware LSM's did (measured
/// on signals at known offsets, `lsm_tests`).
const CARRIER_SIGN: f32 = -1.0;

/// A carrier loop's state.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct Carrier {
    /// The phase correction per symbol, radians.
    pub pll: f32,
    /// The carrier offset it tracks: signal minus NCO, Hz.
    pub offset_hz: f32,
    /// The correction's limit, radians per symbol.
    pub limit: f32,
    /// No signal on the channel: the loops hold.
    pub held: bool,
}

impl Carrier {
    /// Half the limit or more: the loop ran off on noise (a parked lane keeps demodulating
    /// after the carrier drops).
    pub fn hot(&self) -> bool {
        self.pll.abs() >= self.limit / 2.0
    }
}

/// Decode statistics (diagnostics / tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct LsmStats {
    pub symbols: u64,
    /// Symbols whose soft sync score passed the threshold.
    pub sync_detections: u64,
}

/// The hold's hysteresis (the gateware's `LsmSignalHold`). It starts held: a freshly tuned
/// channel waits for the signal before its loops adapt.
struct Hold {
    held: bool,
    run: u32,
}

impl Hold {
    fn new() -> Self {
        Hold { held: true, run: 0 }
    }

    fn symbol(&mut self, gated: bool) {
        let (counts, limit) = if self.held { (!gated, HOLD_EXIT_SYMBOLS) } else { (gated, HOLD_ENTER_SYMBOLS) };
        if !counts {
            self.run = 0;
        } else if self.run + 1 >= limit {
            self.held = !self.held;
            self.run = 0;
        } else {
            self.run += 1;
        }
    }
}

/// SDRTrunk `P25P1DemodulatorLSM`: filtered 25 kSPS samples in, dibits and sync detections out.
pub struct LsmDemodulator {
    sample_point: f64,
    samples_per_symbol: f64,
    samples_per_half_symbol: f64,
    /// The last `buffer_reserve` samples of the previous block, then the current block.
    buffer_i: Vec<f32>,
    buffer_q: Vec<f32>,
    buffer_reserve: usize,
    pll: f32,
    sample_gain: f32,
    previous_middle_i: f32,
    previous_middle_q: f32,
    previous_current_i: f32,
    previous_current_q: f32,
    previous_symbol_i: f32,
    previous_symbol_q: f32,
    sync_symbols: [f32; 24],
    soft_sync: SoftSync,
    max_pll: f32,
    /// None: SDRTrunk's loop, which never holds.
    hold: Option<Hold>,
    pub stats: LsmStats,
}

impl LsmDemodulator {
    /// SDRTrunk `setSamplesPerSymbol`, which takes a float.
    pub fn new(samples_per_symbol: f32) -> Self {
        LsmDemodulator { max_pll: MAX_PLL, hold: Some(Hold::new()), ..Self::sdrtrunk(samples_per_symbol) }
    }

    /// SDRTrunk's loop exactly: no hold, the π/3 limit.
    fn sdrtrunk(samples_per_symbol: f32) -> Self {
        let reserve = samples_per_symbol.ceil() as usize;
        LsmDemodulator {
            sample_point: samples_per_symbol as f64,
            samples_per_symbol: samples_per_symbol as f64,
            samples_per_half_symbol: (samples_per_symbol / 2.0) as f64,
            buffer_i: vec![0.0; reserve],
            buffer_q: vec![0.0; reserve],
            buffer_reserve: reserve,
            pll: 0.0,
            sample_gain: 1.0,
            previous_middle_i: 0.0,
            previous_middle_q: 0.0,
            previous_current_i: 0.0,
            previous_current_q: 0.0,
            previous_symbol_i: 0.7,
            previous_symbol_q: 0.7,
            sync_symbols: sync_symbols(),
            soft_sync: SoftSync::new(),
            max_pll: MAX_PLL_SDRTRUNK,
            hold: None,
            stats: LsmStats::default(),
        }
    }

    /// The carrier loop's phase correction per symbol, radians.
    #[cfg(test)]
    pub fn pll(&self) -> f32 {
        self.pll
    }

    /// The carrier loop, as the receivers report it.
    pub fn carrier(&self) -> Carrier {
        Carrier {
            pll: self.pll,
            offset_hz: CARRIER_SIGN * self.pll * SYMBOL_RATE as f32 / std::f32::consts::TAU,
            limit: self.max_pll,
            held: self.hold.as_ref().is_some_and(|h| h.held),
        }
    }

    #[cfg(test)]
    pub fn sample_gain(&self) -> f32 {
        self.sample_gain
    }

    pub fn process(&mut self, i: &[f32], q: &[f32], sink: &mut impl DibitSink) {
        let carried = self.buffer_i.len() - self.buffer_reserve;
        self.buffer_i.drain(..carried);
        self.buffer_q.drain(..carried);
        self.buffer_i.extend_from_slice(i);
        self.buffer_q.extend_from_slice(q);

        let samples_per_symbol = self.samples_per_symbol;
        let samples_per_half_symbol = self.samples_per_half_symbol;
        let ted_gain = samples_per_symbol / 4.0;
        let max_timing_adjustment = samples_per_symbol / 25.0;
        let max_pll = self.max_pll;
        let mut sample_point = self.sample_point;
        let mut pll = self.pll;
        let mut sample_gain = self.sample_gain;
        let mut previous_symbol_i = self.previous_symbol_i;
        let mut previous_symbol_q = self.previous_symbol_q;
        let mut previous_middle_i = self.previous_middle_i;
        let mut previous_middle_q = self.previous_middle_q;
        let mut previous_current_i = self.previous_current_i;
        let mut previous_current_q = self.previous_current_q;
        let (bi, bq) = (&self.buffer_i, &self.buffer_q);

        let mut buffer_pointer = 0usize;
        while buffer_pointer < i.len() {
            buffer_pointer += 1;
            sample_point -= 1.0;
            if sample_point >= 1.0 {
                continue;
            }
            // The middle sample sits between the previous symbol and this one.
            let bp = buffer_pointer;
            let mut i_middle = linear(bi[bp], bi[bp + 1], sample_point);
            let mut q_middle = linear(bq[bp], bq[bp + 1], sample_point);
            let pointer = bp as f64 + sample_point + samples_per_half_symbol;
            let offset = pointer.floor() as usize;
            let residual = pointer - offset as f64;
            let mut i_current = linear(bi[offset], bi[offset + 1], residual);
            let mut q_current = linear(bq[offset], bq[offset + 1], residual);

            // Gain from the symbol sample's magnitude, applied to both samples. Below the gate the
            // gain stays, and a run of such symbols holds the loops.
            let magnitude = ((i_current as f64).powi(2) + (q_current as f64).powi(2)).sqrt() as f32;
            let gated = self.hold.is_some() && !(magnitude >= SIGNAL_GATE);
            if let Some(h) = self.hold.as_mut() {
                h.symbol(gated);
            }
            let held = self.hold.as_ref().is_some_and(|h| h.held);
            if !gated && magnitude > 0.0 && magnitude.is_finite() {
                let required_gain = constrain(OBJECTIVE_MAGNITUDE / magnitude, MAX_SAMPLE_GAIN);
                sample_gain += (required_gain - sample_gain) * SAMPLE_GAIN_SLEW;
                sample_gain = sample_gain.min(required_gain).min(MAX_SAMPLE_GAIN);
            }
            i_middle *= sample_gain;
            q_middle *= sample_gain;
            i_current *= sample_gain;
            q_current *= sample_gain;

            let pll_i = (pll as f64).cos() as f32;
            let pll_q = (pll as f64).sin() as f32;

            // Each sample against the one a symbol earlier, rotated by the carrier loop.
            let mut i_middle_demodulated = (previous_middle_i * i_middle) - (-previous_middle_q * q_middle);
            let mut q_middle_demodulated = (previous_middle_i * q_middle) + (-previous_middle_q * i_middle);
            let pll_temp = (i_middle_demodulated * pll_i) - (q_middle_demodulated * pll_q);
            q_middle_demodulated = (q_middle_demodulated * pll_i) + (i_middle_demodulated * pll_q);
            i_middle_demodulated = pll_temp;

            let mut i_symbol = (previous_current_i * i_current) - (-previous_current_q * q_current);
            let mut q_symbol = (previous_current_i * q_current) + (-previous_current_q * i_current);
            let pll_temp = (i_symbol * pll_i) - (q_symbol * pll_q);
            q_symbol = (q_symbol * pll_i) + (i_symbol * pll_q);
            i_symbol = pll_temp;

            let soft_symbol = (q_symbol as f64).atan2(i_symbol as f64) as f32;

            // Gardner timing error.
            if !held {
                let timing_adjustment = (((previous_symbol_i - i_symbol) * i_middle_demodulated)
                    + ((previous_symbol_q - q_symbol) * q_middle_demodulated)) as f64;
                sample_point += constrain_f64(timing_adjustment, max_timing_adjustment) * ted_gain;
            }

            // The carrier loop does not move on a zero soft symbol.
            let hard_symbol = if soft_symbol != 0.0 {
                let hard_symbol = to_symbol(soft_symbol);
                if !held {
                    let phase_error = constrain(soft_symbol - ideal_phase(hard_symbol), MAX_PHASE_ERROR);
                    pll = constrain(pll - phase_error * PLL_GAIN, max_pll);
                }
                hard_symbol
            } else {
                0
            };

            sink.push_dibit(hard_symbol);
            self.stats.symbols += 1;
            if self.soft_sync.process(soft_symbol, &self.sync_symbols) > SYNC_DETECTION_THRESHOLD {
                self.stats.sync_detections += 1;
                sink.sync_detected();
            }

            previous_symbol_i = i_symbol;
            previous_symbol_q = q_symbol;
            previous_middle_i = i_middle;
            previous_middle_q = q_middle;
            previous_current_i = i_current;
            previous_current_q = q_current;
            sample_point += samples_per_symbol;
        }

        self.sample_point = sample_point;
        self.pll = pll;
        self.sample_gain = sample_gain;
        self.previous_symbol_i = previous_symbol_i;
        self.previous_symbol_q = previous_symbol_q;
        self.previous_middle_i = previous_middle_i;
        self.previous_middle_q = previous_middle_q;
        self.previous_current_i = previous_current_i;
        self.previous_current_q = previous_current_q;
    }
}

/// SDRTrunk `constrain`: clamp to ±limit; NaN or infinity becomes 0.
fn constrain(value: f32, limit: f32) -> f32 {
    if value.is_finite() { value.clamp(-limit, limit) } else { 0.0 }
}

fn constrain_f64(value: f64, limit: f64) -> f64 {
    if value.is_finite() { value.clamp(-limit, limit) } else { 0.0 }
}

/// The whole LSM receive chain: 50 kSPS IQ in, dibits out.
pub struct LsmDecoder {
    dec_i: Fir,
    dec_q: Fir,
    lpf_i: Fir,
    lpf_q: Fir,
    rrc_i: Fir,
    rrc_q: Fir,
    pub demod: LsmDemodulator,
    scratch: [Vec<f32>; 4],
}

impl Default for LsmDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl LsmDecoder {
    pub fn new() -> Self {
        LsmDecoder {
            dec_i: Fir::new(&HALFBAND_63, true),
            dec_q: Fir::new(&HALFBAND_63, true),
            lpf_i: Fir::new(&LPF_LSM_25K, false),
            lpf_q: Fir::new(&LPF_LSM_25K, false),
            rrc_i: Fir::new(&RRC_TAPS_25K, false),
            rrc_q: Fir::new(&RRC_TAPS_25K, false),
            demod: LsmDemodulator::new(DEMOD_RATE_HZ as f32 / SYMBOL_RATE as f32),
            scratch: Default::default(),
        }
    }

    /// SDRTrunk's chain exactly, without the hold and with the π/3 limit (to compare).
    #[cfg(test)]
    pub fn sdrtrunk() -> Self {
        LsmDecoder { demod: LsmDemodulator::sdrtrunk(DEMOD_RATE_HZ as f32 / SYMBOL_RATE as f32), ..Self::new() }
    }

    /// Feed interleaved 16-bit IQ (the DDC ring format: I, Q per sample), scaled to ±1.0 full
    /// scale as SDRTrunk's samples are.
    pub fn process_iq_i16(&mut self, iq: &[i16], sink: &mut impl DibitSink) {
        let n = iq.len() / 2;
        let i: Vec<f32> = (0..n).map(|k| iq[2 * k] as f32 / 32768.0).collect();
        let q: Vec<f32> = (0..n).map(|k| iq[2 * k + 1] as f32 / 32768.0).collect();
        self.process_iq(&i, &q, sink);
    }

    /// Samples at ±1.0 full scale: the AGC's 500x limit is what keeps a quiet channel's noise
    /// small, so the carrier loop does not wander onto its limit before a signal comes.
    pub fn process_iq(&mut self, i: &[f32], q: &[f32], sink: &mut impl DibitSink) {
        let [a, b, c, d] = &mut self.scratch;
        a.clear();
        b.clear();
        self.dec_i.process(i, a);
        self.dec_q.process(q, b);
        c.clear();
        d.clear();
        self.lpf_i.process(a, c);
        self.lpf_q.process(b, d);
        a.clear();
        b.clear();
        self.rrc_i.process(c, a);
        self.rrc_q.process(d, b);
        self.demod.process(a, b, sink);
    }
}

#[cfg(test)]
#[path = "lsm_tests.rs"]
mod tests;

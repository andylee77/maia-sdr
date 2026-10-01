//! Change 071b: P25 Phase 1 C4FM demodulation in software, from the
//! control (or traffic) DDC's 50 kSPS IQ.
//!
//! A port of SDRTrunk's C4FM decoder:
//! - `P25P1DecoderC4FM.receive`: decimate to 25 kSPS, baseband low-pass,
//!   RRC pulse shaping on I and Q, then `DifferentialDemodulatorFloat`
//!   (phase of the current sample against the one ~0.8 symbol earlier,
//!   8-tap MMSE interpolated).
//! - `P25P1DemodulatorC4FM.process`: symbol timing driven by soft sync
//!   detection (a primary and a half-symbol lagging detector), a sync
//!   optimiser that searches the best timing, an equaliser (phase
//!   balance + gain) learned from each sync pattern, and NID validation
//!   through BCH before a timing correction is accepted.
//!
//! The HDL LSM chain decodes C4FM too, but on C4FM sites it passes only
//! ~40-60 % of TSBKs (FPL, SLERS on unit A); SDRTrunk decodes them with
//! this demodulator. Names follow the Java so the two can be read side by
//! side.

use super::c4fm_filters::{HALFBAND_63, LPF_C4FM_25K};
use super::c4fm_interp_taps::TAPS as MMSE_TAPS;
use super::control_channel::NacTracker;
use super::fec::bch::decode_nid;
use crate::lsm::filters::RRC_TAPS_25K;

pub const SYMBOL_RATE: f64 = 4800.0;
/// Rate of the IQ this decoder takes (the P25 DDC output).
pub const INPUT_RATE_HZ: f64 = 50_000.0;
/// Rate after the half-band (SDRTrunk decimates to below 38.4 kSPS).
pub const DEMOD_RATE_HZ: f64 = 25_000.0;

/// Where demodulated dibits go: the P25 framer.
pub trait DibitSink {
    fn push_dibit(&mut self, dibit: u8);
    /// The demodulator found a frame sync ending at the last dibit pushed.
    fn sync_detected(&mut self);
    /// A NID or data unit is being read (SDRTrunk `isAssembling`).
    fn is_assembling(&self) -> bool;
}

impl DibitSink for super::control_channel::ControlChannelDecoder {
    fn push_dibit(&mut self, dibit: u8) {
        self.process_dibit(dibit);
    }
    fn sync_detected(&mut self) {
        super::control_channel::ControlChannelDecoder::sync_detected(self);
    }
    fn is_assembling(&self) -> bool {
        super::control_channel::ControlChannelDecoder::is_assembling(self)
    }
}

// ── Dibits (SDRTrunk `Dibit`) ───────────────────────────────────────
// Values: 0 = +1 (D00), 1 = +3 (D01), 2 = -1 (D10), 3 = -3 (D11).

const PI: f32 = std::f32::consts::PI;
const TWO_PI: f32 = 2.0 * PI;

pub(crate) fn ideal_phase(dibit: u8) -> f32 {
    match dibit & 3 {
        0 => PI / 4.0,
        1 => 3.0 * PI / 4.0,
        2 => -PI / 4.0,
        _ => -3.0 * PI / 4.0,
    }
}

/// Hard decision on a soft symbol (SDRTrunk `toSymbol`).
pub fn to_symbol(sample: f32) -> u8 {
    const BOUNDARY: f32 = PI / 2.0;
    if sample > 0.0 {
        if sample > BOUNDARY { 1 } else { 0 }
    } else if sample < -BOUNDARY {
        3
    } else {
        2
    }
}

fn bit_errors(a: u8, b: u8) -> u32 {
    ((a ^ b) & 3).count_ones()
}

/// The 24 sync dibits, first transmitted first (SDRTrunk
/// `syncPatternToDibits`: 0x5575F5FF77FF, 01 = +3, 11 = -3).
const SYNC_PATTERN: u64 = 0x5575_F5FF_77FF;
fn sync_dibits() -> [u8; 24] {
    let mut d = [0u8; 24];
    for x in 0..24 {
        let v = (SYNC_PATTERN >> (2 * x)) & 3;
        d[23 - x] = if v == 1 { 1 } else { 3 };
    }
    d
}

pub(crate) fn linear(x1: f32, x2: f32, mu: f64) -> f32 {
    if mu < 0.0 {
        x1
    } else if mu > 1.0 {
        x2
    } else {
        x1 + (x2 - x1) * mu as f32
    }
}

// ── Front end ───────────────────────────────────────────────────────

/// Streaming real FIR with an optional decimation by 2. Block form: the
/// last `taps - 1` inputs are kept and prepended to the next block; zero
/// taps (every other one of a half-band) are skipped. Shared with the DMR
/// receiver (change 075).
pub(crate) struct Fir {
    /// (index into the reversed window, tap) of the non-zero taps.
    taps: Vec<(usize, f32)>,
    len: usize,
    work: Vec<f32>,
    decimate: bool,
    /// Decimation phase: the next input sample produces an output.
    emit: bool,
}

impl Fir {
    pub(crate) fn new(taps: &[f32], decimate: bool) -> Self {
        let len = taps.len();
        // y[n] = sum t[k] x[n-k]: with the window w = x[n-len+1 ..= n],
        // x[n-k] = w[len-1-k].
        let nz = taps.iter().enumerate().filter(|(_, t)| **t != 0.0).map(|(k, t)| (len - 1 - k, *t)).collect();
        Fir { taps: nz, len, work: vec![0.0; len - 1], decimate, emit: true }
    }

    pub(crate) fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let hist = self.len - 1;
        self.work.truncate(hist);
        self.work.extend_from_slice(x);
        for n in 0..x.len() {
            let produce = !self.decimate || self.emit;
            self.emit = !self.emit;
            if !produce {
                continue;
            }
            let w = &self.work[n..n + self.len];
            let mut acc = 0.0f32;
            for &(i, t) in &self.taps {
                acc += t * w[i];
            }
            out.push(acc);
        }
        let keep = self.work.len() - hist;
        self.work.drain(..keep);
    }
}

/// SDRTrunk `DifferentialDemodulatorFloatScalar`. Shared with the DMR receiver.
pub(crate) struct DifferentialDemod {
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
    overlap: usize,
    interp_offset: usize,
    mu: f32,
}

impl DifferentialDemod {
    pub(crate) fn new(samples_per_symbol: f64) -> Self {
        let mu = (samples_per_symbol % 1.0) as f32;
        let mut interp_offset = samples_per_symbol.floor() as i64 - 4;
        let mut overlap = samples_per_symbol.floor() as usize + 4;
        while interp_offset < 0 {
            interp_offset += 1;
            overlap += 1;
        }
        DifferentialDemod {
            i_buf: vec![0.0; overlap],
            q_buf: vec![0.0; overlap],
            overlap,
            interp_offset: interp_offset as usize,
            mu,
        }
    }

    fn mmse(samples: &[f32], offset: usize, mu: f32) -> f32 {
        let t = &MMSE_TAPS[((128.0 * mu) as usize).min(128)];
        let s = &samples[offset..offset + 8];
        t[7] * s[0] + t[6] * s[1] + t[5] * s[2] + t[4] * s[3]
            + t[3] * s[4] + t[2] * s[5] + t[1] * s[6] + t[0] * s[7]
    }

    pub(crate) fn demodulate(&mut self, i: &[f32], q: &[f32], out: &mut Vec<f32>) {
        let len = i.len();
        let keep = self.i_buf.len() - self.overlap;
        self.i_buf.drain(..keep);
        self.q_buf.drain(..keep);
        self.i_buf.extend_from_slice(i);
        self.q_buf.extend_from_slice(q);
        for x in 0..len {
            let i_prev = self.i_buf[x];
            let q_prev_conj = -self.q_buf[x];
            let off = self.interp_offset + x;
            let i_cur = Self::mmse(&self.i_buf, off, self.mu);
            let q_cur = Self::mmse(&self.q_buf, off, self.mu);
            let di = i_prev * i_cur - q_prev_conj * q_cur;
            let dq = i_prev * q_cur + i_cur * q_prev_conj;
            out.push(dq.atan2(di));
        }
    }
}

// ── Demodulator (SDRTrunk `P25P1DemodulatorC4FM`) ───────────────────

const EQUALIZER_LOOP_GAIN: f32 = 0.15;
const EQUALIZER_MAXIMUM_PLL: f32 = PI / 3.0;
const EQUALIZER_MAXIMUM_GAIN: f32 = 1.25;
const EQUALIZER_RECALIBRATE_THRESHOLD: f32 = PI / 8.0;
const SOFT_SYMBOL_QUADRANT_BOUNDARY: f32 = PI / 2.0;
const SYNC_THRESHOLD_DETECTION: f32 = 80.0;
const SYNC_THRESHOLD_OPTIMIZED: f32 = 80.0;
const SYNC_THRESHOLD_EQUALIZED: f32 = 110.0;
const BUFFER_WORKSPACE_LENGTH: usize = 1024;
const DIBIT_LENGTH_NID: usize = 33;
const DIBIT_LENGTH_SYNC: usize = 24;

/// A candidate timing and equaliser correction from a sync detection.
#[derive(Debug, Clone, Copy)]
struct Correction {
    additional_offset: f64,
    timing_adjustment: f64,
    pll_adjustment: f32,
    gain_adjustment: f32,
    detection_score: f32,
    optimization_score: f32,
    /// A valid NID was decoded at this timing.
    nid_valid: bool,
}

impl Correction {
    fn timing(&self) -> f64 {
        self.additional_offset + self.timing_adjustment
    }
    fn is_valid(&self) -> bool {
        self.nid_valid && self.optimization_score > SYNC_THRESHOLD_EQUALIZED
    }
    fn high_quality_detection(&self) -> bool {
        self.detection_score > SYNC_THRESHOLD_EQUALIZED
    }
}

/// SDRTrunk `P25P1SoftSyncDetectorScalar`.
struct SoftSync {
    symbols: [f32; 48],
    ptr: usize,
}

impl SoftSync {
    fn new() -> Self {
        SoftSync { symbols: [0.0; 48], ptr: 0 }
    }
    fn process(&mut self, s: f32, pattern: &[f32; 24]) -> f32 {
        self.symbols[self.ptr] = s;
        self.symbols[self.ptr + 24] = s;
        self.ptr = (self.ptr + 1) % 24;
        let mut score = 0.0;
        for x in 0..24 {
            score += pattern[x] * self.symbols[self.ptr + x];
        }
        score
    }
}

/// Decode statistics (diagnostics / tests).
#[derive(Debug, Clone, Copy, Default)]
pub struct C4fmStats {
    pub symbols: u64,
    pub sync_candidates: u64,
    pub syncs_valid: u64,
    pub nid_fail: u64,
}

pub struct C4fmDemodulator {
    sync_symbols: [f32; 24],
    sync_dibits: [u8; 24],
    nac_tracker: NacTracker,
    sync_primary: SoftSync,
    sync_lagging: SoftSync,
    fine_sync: bool,
    max_fine_sync_timing_adjustment: f64,
    noise_std_threshold: f64,
    sample_point: f64,
    sample_point_adjustment: f64,
    sample_point_adjustment_increment: f64,
    sample_point_adjustment_max: f64,
    samples_per_symbol: f64,
    buffer: Vec<f32>,
    lagging_sync_offset: f32,
    optimize_fine_increment: f64,
    buffer_pointer: usize,
    buffer_reload_threshold: usize,
    symbols_since_last_sync: u64,
    // Equaliser
    eq_initialized: bool,
    eq_pll: f32,
    eq_gain: f32,
    pub stats: C4fmStats,
}

impl C4fmDemodulator {
    pub fn new(samples_per_symbol: f64) -> Self {
        let sync_dibits = sync_dibits();
        let mut sync_symbols = [0.0f32; 24];
        for x in 0..24 {
            sync_symbols[x] = ideal_phase(sync_dibits[x]);
        }
        let sps = samples_per_symbol;
        let buffer_len = BUFFER_WORKSPACE_LENGTH
            + ((DIBIT_LENGTH_SYNC + DIBIT_LENGTH_NID + 2) as f64 * sps).ceil() as usize;
        let reload = buffer_len - (sps * (DIBIT_LENGTH_NID + 1) as f64).ceil() as usize;
        C4fmDemodulator {
            sync_symbols,
            sync_dibits,
            nac_tracker: NacTracker::default(),
            sync_primary: SoftSync::new(),
            sync_lagging: SoftSync::new(),
            fine_sync: false,
            max_fine_sync_timing_adjustment: sps * 0.2,
            noise_std_threshold: (ideal_phase(1) as f64) * 2.0 / sps * 1.2,
            sample_point: sps,
            sample_point_adjustment: 0.0,
            sample_point_adjustment_increment: sps / 100.0,
            sample_point_adjustment_max: sps / 2.0,
            samples_per_symbol: sps,
            buffer: vec![0.0; buffer_len],
            lagging_sync_offset: (sps / 2.0) as f32,
            optimize_fine_increment: sps / 200.0,
            buffer_pointer: reload,
            buffer_reload_threshold: reload,
            symbols_since_last_sync: 0,
            eq_initialized: false,
            eq_pll: 0.0,
            eq_gain: 1.219,
            stats: C4fmStats::default(),
        }
    }

    /// Frequency offset the equaliser has learned (radians per symbol).
    pub fn pll(&self) -> f32 {
        self.eq_pll
    }

    /// Forget the equaliser (retune).
    pub fn reset_pll(&mut self) {
        self.eq_initialized = false;
        self.eq_pll = 0.0;
    }

    fn buf(&self, i: usize) -> f32 {
        self.buffer.get(i).copied().unwrap_or(0.0)
    }

    /// Demodulate differential-phase samples; dibits go to `sink`.
    pub fn process(&mut self, samples: &[f32], sink: &mut impl DibitSink) {
        let mut fine_sync = self.fine_sync;
        let mut sample_point = self.sample_point;
        let sps = self.samples_per_symbol;
        let mut buffer_pointer = self.buffer_pointer;
        let reload = self.buffer_reload_threshold;
        let mut since_sync = self.symbols_since_last_sync;
        let mut samples_pointer = 0usize;
        let len = self.buffer.len();

        while samples_pointer < samples.len() {
            if buffer_pointer >= reload {
                let copy = BUFFER_WORKSPACE_LENGTH.min(samples.len() - samples_pointer);
                self.buffer.copy_within(copy.., 0);
                self.buffer[len - copy..].copy_from_slice(&samples[samples_pointer..samples_pointer + copy]);
                samples_pointer += copy;
                buffer_pointer -= copy;
                // Unwrap phases across the +-pi seam.
                for x in (len - copy)..len {
                    if self.buffer[x - 1] > 1.5 && self.buffer[x] < -1.5 {
                        self.buffer[x] += TWO_PI;
                    } else if self.buffer[x - 1] < -1.5 && self.buffer[x] > 1.5 {
                        self.buffer[x] -= TWO_PI;
                    }
                }
            }

            while buffer_pointer < reload {
                buffer_pointer += 1;
                sample_point -= 1.0;
                if sample_point >= 1.0 {
                    continue;
                }
                since_sync += 1;
                self.stats.symbols += 1;
                let soft = self.equalized_symbol(self.buf(buffer_pointer), self.buf(buffer_pointer + 1), sample_point);
                let symbol = to_symbol(soft);
                sample_point += self.timing_adjustment(soft, symbol, buffer_pointer);
                sink.push_dibit(symbol);

                let score_primary = self.sync_primary.process(soft, &self.sync_symbols);
                let mut candidate: Option<Correction> = None;
                let offset = buffer_pointer as f64 + sample_point;
                if fine_sync {
                    if since_sync > 1 && score_primary > SYNC_THRESHOLD_DETECTION {
                        candidate = self.optimize(0.0, score_primary, offset);
                    }
                } else {
                    // Also look half a symbol late (lagging detector).
                    let lag = offset - self.lagging_sync_offset as f64;
                    let li = lag.floor().max(0.0) as usize;
                    let soft_lag = self.equalized_symbol(self.buf(li), self.buf(li + 1), lag - li as f64);
                    let score_lag = self.sync_lagging.process(soft_lag, &self.sync_symbols);
                    if since_sync > 1 {
                        if score_primary > SYNC_THRESHOLD_DETECTION && score_primary > score_lag {
                            candidate = self.optimize(0.0, score_primary, offset);
                        }
                        if candidate.is_none() && score_lag > SYNC_THRESHOLD_DETECTION {
                            candidate = self.optimize(-(self.lagging_sync_offset as f64), score_lag, offset);
                        }
                    }
                }

                if let Some(mut c) = candidate {
                    self.stats.sync_candidates += 1;
                    self.validate_nid(&mut c, offset);
                    // A high detection score that fails NID validation with
                    // the optimised timing: retry without the timing change.
                    if !c.is_valid() && c.high_quality_detection() {
                        c = self.correction(c.additional_offset, 0.0, c.detection_score, c.detection_score, offset);
                        self.validate_nid(&mut c, offset);
                    }
                    if c.is_valid() {
                        self.stats.syncs_valid += 1;
                        if fine_sync {
                            let mut adjustment = c.timing();
                            if adjustment < 1.0 {
                                adjustment = adjustment
                                    .clamp(-self.max_fine_sync_timing_adjustment, self.max_fine_sync_timing_adjustment);
                            }
                            sample_point += adjustment;
                        } else {
                            sample_point += c.timing();
                        }
                        self.sample_point_adjustment = 0.0;
                        self.apply(&c);
                    }
                    if c.is_valid() || c.high_quality_detection() {
                        sink.sync_detected();
                        fine_sync = true;
                        since_sync = 0;
                    }
                }
                if fine_sync != sink.is_assembling() {
                    fine_sync = sink.is_assembling();
                }
                sample_point += sps;
            }
        }
        self.buffer_pointer = buffer_pointer;
        self.fine_sync = fine_sync;
        self.sample_point = sample_point;
        self.symbols_since_last_sync = since_sync;
    }

    fn validate_nid(&mut self, c: &mut Correction, buffer_offset: f64) {
        let sps = self.samples_per_symbol;
        let mut pointer = buffer_offset + c.timing() + sps;
        let mut nid: u64 = 0;
        for x in 0..DIBIT_LENGTH_NID {
            let integral = pointer.floor().max(0.0) as usize;
            let fractional = pointer - integral as f64;
            let soft = self.equalized_symbol_with(self.buf(integral), self.buf(integral + 1), fractional, c);
            if x != 11 {
                nid = (nid << 2) | to_symbol(soft) as u64;
            }
            pointer += sps;
        }
        let tracked = self.nac_tracker.dominant();
        let Some(d) = decode_nid(nid) else {
            self.stats.nid_fail += 1;
            return;
        };
        self.nac_tracker.track(d.nac);
        if tracked != 0 && tracked != d.nac {
            return;
        }
        // A known data unit (SDRTrunk: not PLACE_HOLDER).
        c.nid_valid = matches!(d.duid, 0x0 | 0x3 | 0x5 | 0x7 | 0xA | 0xC | 0xF);
    }

    fn is_noisy(&self, offset: f64) -> bool {
        let start = (offset - 23.0 * self.samples_per_symbol).floor().max(0.0) as usize;
        let end = (offset.ceil().max(0.0) as usize).min(self.buffer.len() - 1);
        if end <= start + 1 {
            return false;
        }
        let n = (end - start) as f64;
        let (mut sum, mut sum_sq) = (0.0f64, 0.0f64);
        for i in start..end {
            let d = (self.buffer[i] - self.buffer[i + 1]) as f64;
            sum += d;
            sum_sq += d * d;
        }
        let mean = sum / n;
        let var = (sum_sq - n * mean * mean) / (n - 1.0);
        var.max(0.0).sqrt() > self.noise_std_threshold
    }

    // ── Equaliser (SDRTrunk inner class `Equalizer`) ────────────────

    fn optimize(&mut self, additional_offset: f64, detection_score: f32, buffer_offset: f64) -> Option<Correction> {
        let offset = buffer_offset + additional_offset;
        if self.is_noisy(offset) {
            return None;
        }
        // SDRTrunk reads the heap field here, i.e. the fine-sync state as of
        // the previous `process` call.
        let fine_sync = self.fine_sync;
        let sps = self.samples_per_symbol;
        let mut step = sps / if fine_sync { 16.0 } else { 8.0 };
        let step_min = self.optimize_fine_increment;
        let mut adjustment = 0.0f64;
        let adjustment_max = if fine_sync { sps } else { sps / 2.0 };
        let mut score_center = self.score(offset, self.eq_pll, self.eq_gain);
        let mut score_left = self.score(offset - step, self.eq_pll, self.eq_gain);
        let mut score_right = self.score(offset + step, self.eq_pll, self.eq_gain);
        while step > step_min && adjustment.abs() <= adjustment_max {
            if score_left > score_right && score_left > score_center {
                adjustment -= step;
                score_right = score_center;
                score_center = score_left;
                score_left = self.score(offset + adjustment - step, self.eq_pll, self.eq_gain);
            } else if score_right > score_left && score_right > score_center {
                adjustment += step;
                score_left = score_center;
                score_center = score_right;
                score_right = self.score(offset + adjustment + step, self.eq_pll, self.eq_gain);
            } else {
                step *= 0.5;
                if step > step_min {
                    score_left = self.score(offset + adjustment - step, self.eq_pll, self.eq_gain);
                    score_right = self.score(offset + adjustment + step, self.eq_pll, self.eq_gain);
                }
            }
        }
        if score_center < SYNC_THRESHOLD_OPTIMIZED {
            return None;
        }
        Some(self.correction(additional_offset, adjustment, detection_score, score_center, buffer_offset))
    }

    fn equalize(&self, symbol: f32) -> f32 {
        (symbol + self.eq_pll) * self.eq_gain
    }

    /// Intra-sync timing nudge (SDRTrunk `getAdjustment`).
    fn timing_adjustment(&mut self, soft: f32, symbol: u8, buffer_pointer: usize) -> f64 {
        let falling = self.buf(buffer_pointer) > self.buf(buffer_pointer + 1);
        let inc = self.sample_point_adjustment_increment;
        let adjustment = if soft < ideal_phase(symbol) {
            if falling { -inc } else { inc }
        } else if falling {
            inc
        } else {
            -inc
        };
        if (self.sample_point_adjustment + adjustment).abs() <= self.sample_point_adjustment_max {
            self.sample_point_adjustment += adjustment;
            return adjustment;
        }
        0.0
    }

    fn equalized_symbol(&self, s1: f32, s2: f32, mu: f64) -> f32 {
        linear(self.equalize(s1), self.equalize(s2), mu)
    }

    fn equalized_symbol_with(&self, s1: f32, s2: f32, mu: f64, c: &Correction) -> f32 {
        let wrap = |mut s: f32| {
            s = (s + self.eq_pll + c.pll_adjustment) * (self.eq_gain + c.gain_adjustment);
            if s > PI {
                s -= TWO_PI;
            } else if s < -PI {
                s += TWO_PI;
            }
            s
        };
        linear(wrap(s1), wrap(s2), mu)
    }

    /// Correlation of the 24 symbols ending at `offset` with the sync
    /// pattern, under a balance / gain.
    fn score(&self, offset: f64, balance: f32, gain: f32) -> f32 {
        let sps = self.samples_per_symbol;
        let max_pointer = self.buffer.len() - 1;
        let mut pointer = offset - sps * 23.0;
        let mut score = 0.0f32;
        for x in 0..24 {
            let bp = pointer.floor();
            let soft = if bp >= 0.0 && (bp as usize) < max_pointer {
                let i = bp as usize;
                (linear(self.buffer[i], self.buffer[i + 1], pointer - bp) + balance) * gain
            } else {
                0.0
            };
            score += soft * self.sync_symbols[x];
            pointer += sps;
        }
        score
    }

    fn apply(&mut self, c: &Correction) {
        if self.eq_initialized && c.pll_adjustment.abs() > EQUALIZER_RECALIBRATE_THRESHOLD {
            self.eq_initialized = false;
        }
        if self.eq_initialized {
            self.eq_pll += c.pll_adjustment * EQUALIZER_LOOP_GAIN;
            self.eq_gain += c.gain_adjustment * EQUALIZER_LOOP_GAIN;
        } else {
            self.eq_pll += c.pll_adjustment;
            self.eq_gain += c.gain_adjustment;
        }
        self.eq_pll = self.eq_pll.clamp(-EQUALIZER_MAXIMUM_PLL, EQUALIZER_MAXIMUM_PLL);
        self.eq_gain = self.eq_gain.clamp(1.0, EQUALIZER_MAXIMUM_GAIN);
        self.eq_initialized = true;
    }

    /// Balance and gain corrections measured on the sync pattern
    /// (SDRTrunk `getCorrection`).
    fn correction(
        &self,
        additional_offset: f64,
        timing_correction: f64,
        detection_score: f32,
        optimization_score: f32,
        offset: f64,
    ) -> Correction {
        let sps = self.samples_per_symbol;
        let resample = |pos: f64| -> Option<f32> {
            let i = pos.floor();
            if i < 0.0 || (i as usize) + 1 >= self.buffer.len() {
                return None;
            }
            let i = i as usize;
            Some((linear(self.buffer[i], self.buffer[i + 1], pos - i as f64) + self.eq_pll) * self.eq_gain)
        };
        let mut start = offset + additional_offset + timing_correction;
        let last = resample(start).unwrap_or(0.0);
        let sym23 = self.sync_symbols[23];
        let mut balance_plus3 = 0.0f32;
        let mut balance_minus3 = last - sym23;
        let mut gain_acc = sym23.abs() - last.abs();
        let mut _bit_errors = bit_errors(self.sync_dibits[23], to_symbol(last));
        start -= 23.0 * sps;
        for x in 0..23 {
            if let Some(s) = resample(start) {
                let symbol = self.sync_symbols[x];
                _bit_errors += bit_errors(self.sync_dibits[x], to_symbol(s));
                if symbol > 0.0 {
                    balance_plus3 += s - symbol;
                } else {
                    balance_minus3 += s - symbol;
                }
                gain_acc += symbol.abs() - s.abs();
            }
            start += sps;
        }
        // 11 x +3 and 13 x -3 symbols in the sync pattern.
        balance_plus3 /= -11.0;
        balance_minus3 /= -13.0;
        let balance = ((balance_plus3 + balance_minus3) / 2.0)
            .clamp(-SOFT_SYMBOL_QUADRANT_BOUNDARY, SOFT_SYMBOL_QUADRANT_BOUNDARY);
        gain_acc /= 24.0 * ideal_phase(1);
        let optimization_score = if !self.eq_initialized {
            self.score(offset + timing_correction, self.eq_pll + balance, self.eq_gain + gain_acc)
        } else {
            optimization_score
        };
        Correction {
            additional_offset,
            timing_adjustment: timing_correction,
            pll_adjustment: balance,
            gain_adjustment: gain_acc,
            detection_score,
            optimization_score,
            nid_valid: false,
        }
    }
}

/// The whole C4FM receive chain: 50 kSPS IQ in, dibits out.
pub struct C4fmDecoder {
    dec_i: Fir,
    dec_q: Fir,
    lpf_i: Fir,
    lpf_q: Fir,
    rrc_i: Fir,
    rrc_q: Fir,
    diff: DifferentialDemod,
    pub demod: C4fmDemodulator,
    scratch: [Vec<f32>; 4],
}

impl Default for C4fmDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl C4fmDecoder {
    pub fn new() -> Self {
        let sps = DEMOD_RATE_HZ / SYMBOL_RATE;
        C4fmDecoder {
            dec_i: Fir::new(&HALFBAND_63, true),
            dec_q: Fir::new(&HALFBAND_63, true),
            lpf_i: Fir::new(&LPF_C4FM_25K, false),
            lpf_q: Fir::new(&LPF_C4FM_25K, false),
            rrc_i: Fir::new(&RRC_TAPS_25K, false),
            rrc_q: Fir::new(&RRC_TAPS_25K, false),
            diff: DifferentialDemod::new(sps),
            demod: C4fmDemodulator::new(sps),
            scratch: Default::default(),
        }
    }

    /// Feed interleaved 16-bit IQ (the DDC ring format: I, Q per sample).
    pub fn process_iq_i16(&mut self, iq: &[i16], sink: &mut impl DibitSink) {
        let n = iq.len() / 2;
        let i: Vec<f32> = (0..n).map(|k| iq[2 * k] as f32).collect();
        let q: Vec<f32> = (0..n).map(|k| iq[2 * k + 1] as f32).collect();
        self.process_iq(&i, &q, sink);
    }

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
        c.clear();
        self.diff.demodulate(a, b, c);
        let phases = std::mem::take(c);
        self.demod.process(&phases, sink);
        self.scratch[2] = phases;
    }

    /// A retune: the equaliser's frequency offset no longer applies.
    pub fn reset(&mut self) {
        self.demod.reset_pll();
    }
}

#[cfg(test)]
#[path = "c4fm_tests.rs"]
mod tests;

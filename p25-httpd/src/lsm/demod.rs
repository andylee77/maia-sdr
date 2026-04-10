//! P25 LSM demodulator loop.
//!
//! Phase 6D port of `demod_lsm()` from `tools/p25_lsm_demod.py` lines 328-475,
//! which is itself a line-by-line port of SDRTrunk's
//! `P25P1DemodulatorLSM.process(float[] i, float[] q)`. Variable names match
//! the Java source so a side-by-side diff against either reference is
//! straightforward.
//!
//! Algorithm overview (per symbol):
//!
//! 1. Linear-interpolate I/Q to the fractional sample point (Gardner timing).
//! 2. AGC: scale toward unit magnitude, slewed at 5 % per symbol.
//! 3. Differential demod `z[k] * conj(z[k-1])` on both midpoint and symbol
//!    samples.
//! 4. Rotate by the tracked PLL phase.
//! 5. atan2 slicer maps the demodulated symbol to a hard dibit (4-PSK).
//! 6. Gardner TED on the 2-D demodulated symbols steers the timing loop.
//! 7. Decision-directed PI phase loop nudges the PLL toward the ideal
//!    constellation point.
//!
//! Constants are reproduced verbatim from `P25P1DemodulatorLSM.java` and
//! `Dibit.java`. Do not edit them without consulting the Java source.

use super::Complex32;
use std::f32::consts::PI;

/// P25 Phase 1 baud rate.
pub const P25_SYMBOL_RATE: f32 = 4800.0;
/// PLL proportional step coefficient.
const PLL_GAIN: f32 = 0.1;
/// Maximum per-symbol phase error magnitude before applying the PLL gain.
const PLL_MAX_ERROR: f32 = 0.3;
/// Hard PLL phase clamp (~±800 Hz at 4800 sym/s).
const MAX_PLL_ABS: f32 = PI / 3.0;
/// AGC target magnitude.
const OBJECTIVE_MAGNITUDE: f32 = 1.0;
/// AGC update fraction per symbol.
const AGC_SLEW: f32 = 0.05;
/// Maximum AGC gain (runaway clamp).
const AGC_MAX: f32 = 500.0;

/// Output of one demod run over an IQ buffer.
///
/// Shapes match `DemodResult` in the Python reference. The `n_symbols`
/// length applies to all four `*_per_symbol` arrays.
#[derive(Debug, Clone)]
pub struct DemodResult {
    /// Complex constellation points after differential demod + PLL rotation.
    pub soft_symbols: Vec<Complex32>,
    /// `atan2(q, i)` of `soft_symbols` — used by the soft sync detector.
    pub soft_phases: Vec<f32>,
    /// Hard dibit decisions (0..3) one per symbol.
    pub hard_dibits: Vec<u8>,
    /// Tracked PLL phase per symbol (radians, clamped to ±π/3).
    pub pll_trace: Vec<f32>,
    /// `samplePoint` per symbol (Gardner timing offset, samples).
    pub timing_trace: Vec<f32>,
    /// `sample_rate / 4800` — fractional samples per P25 symbol.
    pub samples_per_symbol: f32,
}

impl DemodResult {
    pub fn n_symbols(&self) -> usize {
        self.hard_dibits.len()
    }
}

/// Persistent demod state for streaming use across multiple input chunks.
///
/// `demod_lsm` builds a fresh state internally; this struct is exposed
/// for the live ring-DMA path which feeds the demod with successive
/// 32 KB sub-buffers and must preserve PLL/timing/AGC state across boundaries.
#[derive(Debug, Clone)]
pub struct DemodState {
    pub sample_point: f32,
    pub pll: f32,
    pub sample_gain: f32,
    pub prev_middle_i: f32,
    pub prev_middle_q: f32,
    pub prev_current_i: f32,
    pub prev_current_q: f32,
    pub prev_sym_i: f32,
    pub prev_sym_q: f32,
}

impl DemodState {
    /// Initial state matching the Python reference / SDRTrunk Java
    /// initialisers (`prev_sym_i = prev_sym_q = 0.7` is the SDRTrunk default).
    pub fn new(samples_per_symbol: f32) -> Self {
        DemodState {
            sample_point: samples_per_symbol,
            pll: 0.0,
            sample_gain: 1.0,
            prev_middle_i: 0.0,
            prev_middle_q: 0.0,
            prev_current_i: 0.0,
            prev_current_q: 0.0,
            prev_sym_i: 0.7,
            prev_sym_q: 0.7,
        }
    }
}

/// Map a soft phase to a 4-PSK quadrant. Mirrors `Dibit.toDibit()`.
#[inline]
fn to_dibit(soft_symbol: f32) -> u8 {
    if soft_symbol > 0.0 {
        if soft_symbol > PI / 2.0 {
            0b01
        } else {
            0b00
        }
    } else if soft_symbol < -PI / 2.0 {
        0b11
    } else {
        0b10
    }
}

/// Ideal phase of each dibit (Dibit.java) — used by the decision-directed
/// PLL update step.
#[inline]
fn dibit_phase(dibit: u8) -> f32 {
    match dibit {
        0b00 => PI / 4.0,         // +1
        0b01 => 3.0 * PI / 4.0,   // +3
        0b10 => -PI / 4.0,        // -1
        0b11 => -3.0 * PI / 4.0,  // -3
        _ => 0.0,
    }
}

/// Linear interpolation. Pulled out as a `#[inline]` for parity with the
/// Python `lerp()` helper and to keep the loop body readable.
#[inline]
fn lerp(a: f32, b: f32, mu: f32) -> f32 {
    a + (b - a) * mu
}

/// Demodulate filtered LSM I/Q into dibits.
///
/// Direct port of `demod_lsm()` in `tools/p25_lsm_demod.py`. Reads the
/// entire incoming buffer in one pass — no streaming state handover, since
/// this is the offline batch entry point used by the unit tests. The
/// streaming live path uses `demod_lsm_with_state` so it can preserve
/// `DemodState` across iq_dma sub-buffer boundaries.
pub fn demod_lsm(iq: &[Complex32], sample_rate: f32) -> DemodResult {
    let sps = sample_rate / P25_SYMBOL_RATE;
    let mut state = DemodState::new(sps);
    demod_lsm_with_state(iq, sample_rate, &mut state)
}

/// Streaming variant of `demod_lsm` — preserves loop state across calls.
///
/// Each invocation processes `iq` from the start; the caller is responsible
/// for chunking the input so the same `state` is threaded through
/// successive calls.
pub fn demod_lsm_with_state(
    iq: &[Complex32],
    sample_rate: f32,
    state: &mut DemodState,
) -> DemodResult {
    let sps = sample_rate / P25_SYMBOL_RATE;
    let half_sps = sps / 2.0;
    let ted_gain = sps / 4.0;
    let max_timing_adj = sps / 25.0;

    let n = iq.len();
    let buf_i: Vec<f32> = iq.iter().map(|c| c.re).collect();
    let buf_q: Vec<f32> = iq.iter().map(|c| c.im).collect();

    let mut soft_symbols: Vec<Complex32> = Vec::new();
    let mut soft_phases: Vec<f32> = Vec::new();
    let mut hard_dibits: Vec<u8> = Vec::new();
    let mut pll_trace: Vec<f32> = Vec::new();
    let mut timing_trace: Vec<f32> = Vec::new();

    // Walk the buffer, decrementing sample_point each step. When it goes
    // below 1, we land on a fractional sample for the symbol decision.
    // Java/Python use bp = bufferPointer; we keep the same name.
    let stop = if n > (sps.ceil() as usize) + 2 {
        n - (sps.ceil() as usize) - 2
    } else {
        0
    };
    let mut bp: usize = 0;
    while bp < stop {
        bp += 1;
        state.sample_point -= 1.0;

        if state.sample_point >= 1.0 {
            continue;
        }

        // ----- midpoint sample (between prev and current symbol) -----
        let i_mid_raw = lerp(buf_i[bp], buf_i[bp + 1], state.sample_point);
        let q_mid_raw = lerp(buf_q[bp], buf_q[bp + 1], state.sample_point);

        // ----- current symbol sample, half a symbol ahead of midpoint -----
        let ptr = bp as f32 + state.sample_point + half_sps;
        let offset = ptr.floor() as usize;
        let residual = ptr - offset as f32;
        if offset + 1 >= n {
            break;
        }
        let i_cur_raw = lerp(buf_i[offset], buf_i[offset + 1], residual);
        let q_cur_raw = lerp(buf_q[offset], buf_q[offset + 1], residual);

        // ----- AGC: scale toward unit magnitude, slewed -----
        let magnitude = (i_cur_raw * i_cur_raw + q_cur_raw * q_cur_raw).sqrt();
        if magnitude > 0.0 && magnitude.is_finite() {
            let mut required_gain = OBJECTIVE_MAGNITUDE / magnitude;
            if required_gain > AGC_MAX {
                required_gain = AGC_MAX;
            }
            state.sample_gain += (required_gain - state.sample_gain) * AGC_SLEW;
            if state.sample_gain > required_gain {
                state.sample_gain = required_gain;
            }
            if state.sample_gain > AGC_MAX {
                state.sample_gain = AGC_MAX;
            }
        }
        let i_mid = i_mid_raw * state.sample_gain;
        let q_mid = q_mid_raw * state.sample_gain;
        let i_cur = i_cur_raw * state.sample_gain;
        let q_cur = q_cur_raw * state.sample_gain;

        // ----- current PLL state as a complex rotation -----
        let pll_i = state.pll.cos();
        let pll_q = state.pll.sin();

        // ----- differential demod of MIDDLE sample -----
        // z_mid * conj(z_prev_mid):
        let mut i_mid_demod =
            (state.prev_middle_i * i_mid) + (state.prev_middle_q * q_mid);
        let mut q_mid_demod =
            (state.prev_middle_i * q_mid) - (state.prev_middle_q * i_mid);
        // rotate by PLL
        let tmp = (i_mid_demod * pll_i) - (q_mid_demod * pll_q);
        q_mid_demod = (q_mid_demod * pll_i) + (i_mid_demod * pll_q);
        i_mid_demod = tmp;

        // ----- differential demod of SYMBOL sample -----
        let mut i_sym = (state.prev_current_i * i_cur) + (state.prev_current_q * q_cur);
        let mut q_sym = (state.prev_current_i * q_cur) - (state.prev_current_q * i_cur);
        let tmp = (i_sym * pll_i) - (q_sym * pll_q);
        q_sym = (q_sym * pll_i) + (i_sym * pll_q);
        i_sym = tmp;

        // ----- slice -----
        let soft_symbol = q_sym.atan2(i_sym);

        // ----- Gardner TED on 2D demodulated symbols -----
        let mut timing_adj = (state.prev_sym_i - i_sym) * i_mid_demod
            + (state.prev_sym_q - q_sym) * q_mid_demod;
        if timing_adj > max_timing_adj {
            timing_adj = max_timing_adj;
        } else if timing_adj < -max_timing_adj {
            timing_adj = -max_timing_adj;
        }
        timing_adj *= ted_gain;
        state.sample_point += timing_adj;

        // ----- decision-directed PLL update -----
        let hard = if soft_symbol != 0.0 {
            let h = to_dibit(soft_symbol);
            let mut phase_error = soft_symbol - dibit_phase(h);
            if phase_error > PLL_MAX_ERROR {
                phase_error = PLL_MAX_ERROR;
            } else if phase_error < -PLL_MAX_ERROR {
                phase_error = -PLL_MAX_ERROR;
            }
            state.pll -= phase_error * PLL_GAIN;
            if state.pll > MAX_PLL_ABS {
                state.pll = MAX_PLL_ABS;
            } else if state.pll < -MAX_PLL_ABS {
                state.pll = -MAX_PLL_ABS;
            }
            h
        } else {
            0b00
        };

        // ----- record outputs -----
        soft_symbols.push(Complex32::new(i_sym, q_sym));
        soft_phases.push(soft_symbol);
        hard_dibits.push(hard);
        pll_trace.push(state.pll);
        timing_trace.push(state.sample_point);

        // ----- shuffle history for next iteration -----
        state.prev_sym_i = i_sym;
        state.prev_sym_q = q_sym;
        state.prev_middle_i = i_mid;
        state.prev_middle_q = q_mid;
        state.prev_current_i = i_cur;
        state.prev_current_q = q_cur;

        // Add another symbol period to the countdown
        state.sample_point += sps;
    }

    DemodResult {
        soft_symbols,
        soft_phases,
        hard_dibits,
        pll_trace,
        timing_trace,
        samples_per_symbol: sps,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_dibit_quadrants() {
        // Match Dibit.java mapping:
        //   +1 (   0..π/2) -> 00
        //   +3 ( π/2..π  ) -> 01
        //   -1 (-π/2..0  ) -> 10
        //   -3 (-π..-π/2 ) -> 11
        assert_eq!(to_dibit(PI / 4.0), 0b00);
        assert_eq!(to_dibit(3.0 * PI / 4.0), 0b01);
        assert_eq!(to_dibit(-PI / 4.0), 0b10);
        assert_eq!(to_dibit(-3.0 * PI / 4.0), 0b11);
    }

    #[test]
    fn dibit_phase_inverse_of_to_dibit() {
        for d in 0..4u8 {
            assert_eq!(to_dibit(dibit_phase(d)), d);
        }
    }

    /// Smoke test: feed a constant DC carrier and verify the demod runs to
    /// completion without panicking, produces the expected number of
    /// symbols (approximately len/sps), and returns finite values.
    /// Doesn't validate algorithmic correctness — that's covered by the
    /// integration test against the Python reference output.
    #[test]
    fn demod_runs_on_constant_input() {
        let sample_rate = 31_250.0_f32;
        let n = 4096;
        // Constant rotating carrier so AGC has something to track.
        let iq: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f32 / sample_rate;
                let phi = 2.0 * PI * 1000.0 * t; // 1 kHz tone
                Complex32::new(phi.cos(), phi.sin())
            })
            .collect();
        let result = demod_lsm(&iq, sample_rate);
        let sps = sample_rate / P25_SYMBOL_RATE;
        let expected = (n as f32 / sps) as usize;
        // Allow ±2 symbol slop for the loop's edge handling.
        assert!(
            result.n_symbols() >= expected - 2 && result.n_symbols() <= expected + 2,
            "expected ~{expected} symbols, got {}",
            result.n_symbols()
        );
        for &p in &result.pll_trace {
            assert!(p.is_finite() && p.abs() <= MAX_PLL_ABS + 1e-6);
        }
        for &s in &result.soft_phases {
            assert!(s.is_finite());
        }
    }
}

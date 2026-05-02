//! Streaming software DDC: NCO mixer + Kaiser-windowed LPF + integer
//! decimation. Same algorithm as `lsm/software_decode_tests.rs::ddc_to_62k5`,
//! restructured to be called incrementally on chunks of input samples
//! while preserving NCO phase and FIR history across calls.
//!
//! Design choices:
//!   * NCO accumulator in `f64` so multi-second runs don't lose phase
//!     precision. Wrapped to ±τ when |phase| crosses 1e6 to avoid
//!     denormal-style precision rot.
//!   * LPF is a length-65 Kaiser sinc, designed once at construction
//!     for a cutoff of `min(input_rate, output_rate) / 2.4`. At
//!     8 MSPS in / 62.5 kSPS out this is ~26 kHz, with plenty of margin
//!     for the ~6.25 kHz P25 channel and ~2× the LSM symbol rate.
//!   * Output decimation is integer-only — for our input/output ratios
//!     (8e6/62500 = 128) this is equivalent to the offline harness's
//!     linear-interp rate convert (which always lands on integer
//!     positions when the ratio is integer).
//!
//! Future optimisations (not done in first cut, NEON-friendly):
//!   * Polyphase decimating FIR (compute one output per `decim`
//!     inputs instead of computing all LPF outputs and discarding
//!     `decim - 1` of them).
//!   * Two-stage decimation (e.g. 4× then 32×) to amortise the LPF
//!     length.
//! For now correctness > speed.

use crate::lsm::Complex32;

/// Stop-band attenuation target for the Kaiser LPF design (dB).
const KAISER_ATTEN_DB: f64 = 60.0;
/// LPF length. 65 taps mirrors `software_decode_tests.rs::ddc_to_62k5`.
const LPF_TAPS: usize = 65;

pub struct StreamingSoftwareDdc {
    input_rate: f64,
    output_rate: f64,

    /// Decimation factor = round(input_rate / output_rate). For
    /// 8e6 / 62.5e3 this is exactly 128.
    decim: usize,

    /// NCO target offset (Hz). Stored for `nco_offset_hz()` getter
    /// and re-derivation of the per-step rotator on `set_nco_offset`.
    nco_offset_hz: f64,

    /// Streaming complex-NCO state (f32 for NEON-vectorizable path).
    /// `nco` is the running phasor (|nco| ≈ 1); each input sample
    /// mixes by complex multiply with `nco`, then `nco *= phase_inc`
    /// advances by one sample's worth of phase. Replaces per-sample
    /// `sin_cos()` (saturated cores) and the f64 cmul path (didn't
    /// vectorize on Cortex-A9 NEON).
    nco: NcoF32,
    phase_inc: NcoF32,

    /// Sample counter since last NCO normalization. We renormalize
    /// every NCO_RENORM_INTERVAL samples to keep |nco| from drifting
    /// (rounding loss accumulates as a slow exponential decay/grow).
    nco_renorm_counter: u64,

    /// Real-coefficient LPF taps. Owned (not `&'static`) because the
    /// design parameters depend on the input/output rates passed at
    /// construction.
    lpf_taps: Vec<f32>,

    /// Last `LPF_TAPS - 1` mixed samples, kept across `process` calls
    /// so the FIR output is continuous.
    fir_history: Vec<Complex32>,

    /// Modulo counter for decimation. Emit one output sample for every
    /// `decim` LPF outputs.
    decim_phase: usize,

    /// Reusable scratch buffer for `history || mixed_input`. Allocating
    /// fresh each `process()` was burning ~60 MB/s allocator churn at
    /// 8 MSPS / 256 K-sample chunks; reusing keeps the hot path
    /// allocation-free after warm-up.
    ext_scratch: Vec<Complex32>,
}

/// Internal f32 complex used for the NCO. f32 (not f64) because
/// Cortex-A9's NEON path only vectorizes f32 — using f64 here meant
/// the NCO advance went through scalar VFP and pegged the cores.
/// We renormalize every NCO_RENORM_INTERVAL samples to keep |nco|
/// from drifting above ~1 ppm between renorms (sqrt(N) × ε_f32 ≈
/// sqrt(4096) × 1e-7 = 6e-6, well under our PLL's pull-in range).
#[derive(Clone, Copy)]
struct NcoF32 {
    re: f32,
    im: f32,
}

/// Renormalize the NCO every this-many input samples. At 8 MSPS this
/// is once every ~512 µs — fast enough that f32 drift stays below
/// ~7 ppm worst-case but rare enough that the sqrt cost is negligible
/// (~25 cycles per 4096 samples = vanishing).
const NCO_RENORM_INTERVAL: u64 = 4_096;

impl StreamingSoftwareDdc {
    /// Build a fresh DDC. `nco_offset_hz` is the offset (input-rate
    /// referenced) of the target signal from DC; positive numbers shift
    /// a signal at +offset_hz down to DC.
    pub fn new(input_rate: f64, output_rate: f64, nco_offset_hz: f64) -> Self {
        assert!(input_rate > 0.0 && output_rate > 0.0);
        assert!(input_rate >= output_rate);

        let decim = (input_rate / output_rate).round() as usize;
        let cutoff_hz = input_rate.min(output_rate) / 2.4;
        let lpf_taps =
            kaiser_lpf(LPF_TAPS, cutoff_hz / input_rate, KAISER_ATTEN_DB);

        let phase_step = -std::f64::consts::TAU * nco_offset_hz / input_rate;
        // One-shot trig at construction is fine; only the streaming
        // per-sample advance needs to be f32.
        let phase_inc = NcoF32 {
            re: phase_step.cos() as f32,
            im: phase_step.sin() as f32,
        };

        // Initialize decimation phase so the first output lands on
        // the SAME logical input position as the offline batch DDC
        // (`ddc_to_62k5`'s filtered[m * ratio]). The batch path uses a
        // centered FIR (group delay 0) and decimates at multiples of
        // `ratio`. Our streaming FIR is causal (group delay `half`),
        // so we shift the decimation grid by `half` samples to land on
        // the same output values. Without this, output samples are
        // 32-input-samples (8 µs at 4 MSPS) earlier than batch — a
        // sub-sample offset at 62.5 kSPS that *should* be tolerated by
        // Costas/Gardner but in practice broke sync detection (0 hard
        // syncs vs 88 in batch on the same RF).
        let half = LPF_TAPS / 2;
        let initial_decim_phase = half % decim;

        StreamingSoftwareDdc {
            input_rate,
            output_rate,
            decim,
            nco_offset_hz,
            nco: NcoF32 { re: 1.0, im: 0.0 },
            phase_inc,
            nco_renorm_counter: 0,
            lpf_taps,
            fir_history: vec![Complex32::new(0.0, 0.0); LPF_TAPS - 1],
            decim_phase: initial_decim_phase,
            ext_scratch: Vec::new(),
        }
    }

    /// Retune the NCO. Called when the operator's grant freq changes.
    /// FIR + decim phase are NOT reset — the LPF carries enough history
    /// (`LPF_TAPS - 1` ≈ 8 µs at 8 MSPS) that the discontinuity is
    /// invisible at the LSM input rate. Costas/Gardner downstream
    /// handle the carrier-phase jump as if it were a fade.
    pub fn set_nco_offset(&mut self, nco_offset_hz: f64) {
        self.nco_offset_hz = nco_offset_hz;
        let phase_step = -std::f64::consts::TAU * nco_offset_hz / self.input_rate;
        self.phase_inc = NcoF32 {
            re: phase_step.cos() as f32,
            im: phase_step.sin() as f32,
        };
    }

    pub fn nco_offset_hz(&self) -> f64 {
        self.nco_offset_hz
    }

    pub fn input_rate(&self) -> f64 {
        self.input_rate
    }

    pub fn output_rate(&self) -> f64 {
        self.output_rate
    }

    /// Process one chunk of input. Returns approximately
    /// `input.len() / decim` output samples, depending on where the
    /// chunk boundary lands in the decimation phase.
    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let n_taps = self.lpf_taps.len();
        let h = n_taps - 1;
        let n = input.len();
        let mut out = Vec::with_capacity(n / self.decim + 1);
        if n == 0 {
            return out;
        }

        // Build extended buffer = history || mixed_input. Reuse the
        // pre-allocated scratch — at 8 MSPS / 256 K-sample chunks the
        // 2 MB allocation per call was ~60 MB/s of allocator churn.
        let ext = &mut self.ext_scratch;
        ext.clear();
        ext.reserve(h + n);
        ext.extend_from_slice(&self.fir_history);

        // Streaming complex-NCO (f32 for NEON path on Cortex-A9):
        // mix each input sample by the running phasor `nco`, then
        // advance `nco *= phase_inc`. No per-sample transcendentals.
        // Periodic |nco| renormalization keeps drift sub-1 ppm.
        let mut nco_re = self.nco.re;
        let mut nco_im = self.nco.im;
        let inc_re = self.phase_inc.re;
        let inc_im = self.phase_inc.im;
        let mut renorm_counter = self.nco_renorm_counter;
        for &s in input {
            ext.push(Complex32::new(
                s.re * nco_re - s.im * nco_im,
                s.re * nco_im + s.im * nco_re,
            ));
            // nco *= phase_inc  (complex multiply in f32)
            let new_re = nco_re * inc_re - nco_im * inc_im;
            let new_im = nco_re * inc_im + nco_im * inc_re;
            nco_re = new_re;
            nco_im = new_im;
            renorm_counter += 1;
            if renorm_counter >= NCO_RENORM_INTERVAL {
                let mag2 = nco_re * nco_re + nco_im * nco_im;
                let scale = 1.0_f32 / mag2.sqrt();
                nco_re *= scale;
                nco_im *= scale;
                renorm_counter = 0;
            }
        }
        self.nco = NcoF32 { re: nco_re, im: nco_im };
        self.nco_renorm_counter = renorm_counter;

        // Apply LPF + decimate. Only compute outputs at decimation
        // grid positions: input sample i has FIR output at ext index
        // `h + i`; we emit if `(decim_phase + i) % decim == 0`.
        let taps = &self.lpf_taps;
        let mut decim_phase = self.decim_phase;
        for i in 0..n {
            if decim_phase == 0 {
                let mut acc_re = 0.0_f32;
                let mut acc_im = 0.0_f32;
                let base = h + i;
                for k in 0..n_taps {
                    let t = taps[k];
                    let x = ext[base - k];
                    acc_re += t * x.re;
                    acc_im += t * x.im;
                }
                out.push(Complex32::new(acc_re, acc_im));
            }
            decim_phase += 1;
            if decim_phase >= self.decim {
                decim_phase = 0;
            }
        }
        self.decim_phase = decim_phase;

        // Save the last `h` samples of ext as new history.
        let total = ext.len();
        for k in 0..h {
            self.fir_history[k] = ext[total - h + k];
        }

        out
    }

    /// Reset all streaming state (NCO phasor, FIR history, decim phase).
    /// Called after a long IRQ stall or any time input continuity is
    /// broken (DMA overflow latched, capture mode aborts mid-stream).
    pub fn reset(&mut self) {
        self.nco = NcoF32 { re: 1.0, im: 0.0 };
        self.nco_renorm_counter = 0;
        let half = LPF_TAPS / 2;
        self.decim_phase = half % self.decim;
        for h in self.fir_history.iter_mut() {
            *h = Complex32::new(0.0, 0.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Kaiser-windowed sinc LPF design (port of the offline harness's helpers)
// ---------------------------------------------------------------------------

/// Kaiser-windowed sinc LPF. Same convention as the offline harness:
/// `cutoff_normalised` is `cutoff_hz / input_rate` (i.e. cutoff as a
/// fraction of the sampling rate, range 0..0.5). `attenuation_db`
/// shapes the Kaiser β.
fn kaiser_lpf(n_taps: usize, cutoff_normalised: f64, attenuation_db: f64) -> Vec<f32> {
    assert!(cutoff_normalised > 0.0 && cutoff_normalised < 0.5);
    let beta = if attenuation_db > 50.0 {
        0.1102 * (attenuation_db - 8.7)
    } else if attenuation_db >= 21.0 {
        0.5842 * (attenuation_db - 21.0).powf(0.4)
            + 0.07886 * (attenuation_db - 21.0)
    } else {
        0.0
    };

    let n = n_taps as i32;
    let half = (n - 1) as f64 / 2.0;
    let two_pi_fc = std::f64::consts::TAU * cutoff_normalised;

    let mut taps = vec![0.0_f32; n_taps];
    let i0_beta = bessel_i0(beta);
    for k in 0..n_taps {
        let m = k as f64 - half;
        let sinc = if m.abs() < 1.0e-12 {
            two_pi_fc / std::f64::consts::PI
        } else {
            (two_pi_fc * m).sin() / (std::f64::consts::PI * m)
        };
        let kaiser_arg = 1.0 - (m / half).powi(2);
        let win = if kaiser_arg < 0.0 {
            0.0
        } else {
            bessel_i0(beta * kaiser_arg.sqrt()) / i0_beta
        };
        taps[k] = (sinc * win) as f32;
    }
    // Normalise to unit DC gain.
    let dc: f64 = taps.iter().map(|&t| t as f64).sum();
    if dc.abs() > 1.0e-12 {
        for t in taps.iter_mut() {
            *t = ((*t as f64) / dc) as f32;
        }
    }
    taps
}

/// Modified Bessel function of the first kind, order 0. Truncated power
/// series; converges fast for the |x| ≤ ~20 range we hit (Kaiser β at
/// 60 dB attenuation is ~5.65).
fn bessel_i0(x: f64) -> f64 {
    let half_x = x / 2.0;
    let mut sum = 1.0;
    let mut term = 1.0;
    for k in 1..50 {
        term *= (half_x / k as f64).powi(2);
        sum += term;
        if term < 1.0e-20 * sum {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Streaming chunks must produce the same output as one big call.
    #[test]
    fn streaming_matches_batch() {
        let n = 8 * 8000; // 8 ms at 8 MSPS
        let input: Vec<Complex32> = (0..n)
            .map(|i| Complex32::new((i as f32 * 0.001).sin(),
                                    (i as f32 * 0.001).cos()))
            .collect();

        // Batch: one big call.
        let mut a = StreamingSoftwareDdc::new(8e6, 62.5e3, 1234.0);
        let big = a.process(&input);

        // Streaming: 73 chunks of 877 samples (deliberately unaligned).
        let mut b = StreamingSoftwareDdc::new(8e6, 62.5e3, 1234.0);
        let mut small: Vec<Complex32> = Vec::new();
        for chunk in input.chunks(877) {
            small.extend(b.process(chunk));
        }

        assert_eq!(big.len(), small.len());
        for i in 0..big.len() {
            let dre = (big[i].re - small[i].re).abs();
            let dim = (big[i].im - small[i].im).abs();
            // Allow tiny FP drift from the chunked NCO phase wrap.
            assert!(dre < 1e-3 && dim < 1e-3,
                "diverged at i={i}: big={:?} small={:?}", big[i], small[i]);
        }
    }

    #[test]
    fn output_rate_correct() {
        // Feeding exactly 1 second of input at 8 MSPS should produce
        // (close to) 62500 output samples. Allow ±1 for decim phase
        // alignment at the start.
        let n = 8_000_000;
        let input = vec![Complex32::new(1.0, 0.0); n];
        let mut ddc = StreamingSoftwareDdc::new(8e6, 62.5e3, 0.0);
        let out = ddc.process(&input);
        let expected = 62_500;
        assert!((out.len() as i64 - expected as i64).abs() <= 1,
            "expected ~{expected} samples, got {}", out.len());
    }

    #[test]
    fn nco_centers_target() {
        // Generate a 100 kHz tone at 8 MSPS, NCO-shift it to DC,
        // verify the DDC output is dominated by DC.
        let n = 8 * 1000; // 1 ms
        let f_tone = 100_000.0_f64;
        let fs = 8e6_f64;
        let input: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let p = std::f64::consts::TAU * f_tone * t;
                Complex32::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();

        let mut ddc = StreamingSoftwareDdc::new(fs, 62_500.0, f_tone);
        let out = ddc.process(&input);

        // After steady-state (skip first ~10 samples for FIR transient),
        // |re| should dominate |im|. Magnitude of im should be tiny.
        let tail = &out[10..];
        let mean_re_abs: f32 = tail.iter().map(|c| c.re.abs()).sum::<f32>()
            / tail.len() as f32;
        let mean_im_abs: f32 = tail.iter().map(|c| c.im.abs()).sum::<f32>()
            / tail.len() as f32;
        // After mix to DC, output is purely real (cos contribution).
        // Allow some leakage from FIR transients.
        assert!(mean_re_abs > 0.5, "mean |re| = {mean_re_abs} too low");
        assert!(mean_im_abs < 0.1, "mean |im| = {mean_im_abs} too high");
    }
}

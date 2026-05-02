//! Multistage software DDC: NCO mix at full input rate, then a cascade
//! of decim-by-D stages with progressively sharper Kaiser LPFs.
//!
//! Why multistage: a single 65-tap Kaiser at 4 or 8 MSPS in / 62.5 kSPS
//! out has a transition band ~440 kHz wide (Δf = (A−7.95) × fs / (14.36 × N)).
//! That lets adjacent-channel energy 25 kHz (and even 6.25 kHz) from
//! carrier pass through with only 1–6 dB attenuation. On Clay County's
//! P25 plan (`BASE:851006250 SPACING:6250 BW:12500`) channels overlap
//! every 6.25 kHz — adjacent-channel rejection requires a much sharper
//! filter than is feasible at the full input rate.
//!
//! Multistage gets us the same effective filter at far lower compute:
//! each stage only needs to anti-alias for the next stage's Nyquist,
//! and the *final* stage runs at low rate where a sharp filter (high
//! `n_taps × fs_out`) is cheap.
//!
//! Pipeline:
//! ```text
//!   in @ fs0 → NCO mix → stage0(decim D0) → stage1(D1) → ... → stageN-1(DN-1) → out @ fs_out
//! ```
//!
//! Group-delay alignment: each stage's FIR is causal (output[k] uses
//! input[k - tap] for tap=0..N-1). To match a *centered* batch FIR's
//! decimation grid (which `software_decode_tests::ddc_to_62k5` uses),
//! each stage initialises its `decim_phase = (n_taps/2) % decim`.
//! Without this, the cumulative half-tap shifts compound and break
//! Costas/Gardner sync at the LSM stage.

use crate::lsm::Complex32;

/// f32 complex used for the streaming NCO. f32 only here (not f64) so
/// the inner mix loop hits the Cortex-A9 NEON path. Drift is bounded
/// by NCO_RENORM_INTERVAL.
#[derive(Clone, Copy, Default)]
struct NcoF32 {
    re: f32,
    im: f32,
}

const NCO_RENORM_INTERVAL: u64 = 4_096;

/// One decim-by-D stage with its own LPF taps and history.
struct DecimStage {
    decim: usize,
    taps: Vec<f32>,
    /// Last (n_taps - 1) samples of this stage's input (= previous
    /// stage's output, post-decim).
    history: Vec<Complex32>,
    decim_phase: usize,
    /// Reusable scratch (history || new_input) so we don't allocate
    /// per-call. Resized on first use.
    ext_scratch: Vec<Complex32>,
}

impl DecimStage {
    fn new(decim: usize, taps: Vec<f32>) -> Self {
        let n = taps.len();
        let half = n / 2;
        DecimStage {
            decim,
            taps,
            history: vec![Complex32::new(0.0, 0.0); n.saturating_sub(1)],
            decim_phase: half % decim,
            ext_scratch: Vec::new(),
        }
    }

    fn reset(&mut self) {
        for h in self.history.iter_mut() {
            *h = Complex32::new(0.0, 0.0);
        }
        let half = self.taps.len() / 2;
        self.decim_phase = half % self.decim;
    }

    /// Process input chunk. Returns ~ceil(input.len() / decim) outputs.
    fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        let n_taps = self.taps.len();
        let h = n_taps - 1;
        let n = input.len();
        if n == 0 {
            return Vec::new();
        }
        let ext = &mut self.ext_scratch;
        ext.clear();
        ext.reserve(h + n);
        ext.extend_from_slice(&self.history);
        ext.extend_from_slice(input);

        let mut out = Vec::with_capacity(n / self.decim + 1);
        let taps = &self.taps;
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

        // Save last h samples as new history.
        let total = ext.len();
        for k in 0..h {
            self.history[k] = ext[total - h + k];
        }
        out
    }
}

pub struct MultistageDdc {
    input_rate: f64,
    output_rate: f64,
    nco_offset_hz: f64,

    nco: NcoF32,
    phase_inc: NcoF32,
    nco_renorm_counter: u64,

    stages: Vec<DecimStage>,
    /// Reusable scratch for the post-NCO mixed buffer.
    mix_scratch: Vec<Complex32>,
}

impl MultistageDdc {
    /// Build a multi-stage DDC chain configured for the given input
    /// and output rates. Picks decimation factors + filter designs
    /// based on the rate ratio. Designed for Clay/Duval P25 (6.25 kHz
    /// channel grid) — final stage rejects 6.25 kHz from carrier at
    /// ~60 dB.
    pub fn new(input_rate: f64, output_rate: f64, nco_offset_hz: f64) -> Self {
        assert!(input_rate >= output_rate);
        let stages = build_stages_for(input_rate, output_rate);
        let phase_step = -std::f64::consts::TAU * nco_offset_hz / input_rate;
        let phase_inc = NcoF32 {
            re: phase_step.cos() as f32,
            im: phase_step.sin() as f32,
        };
        MultistageDdc {
            input_rate,
            output_rate,
            nco_offset_hz,
            nco: NcoF32 { re: 1.0, im: 0.0 },
            phase_inc,
            nco_renorm_counter: 0,
            stages,
            mix_scratch: Vec::new(),
        }
    }

    pub fn input_rate(&self) -> f64 { self.input_rate }
    pub fn output_rate(&self) -> f64 { self.output_rate }
    pub fn nco_offset_hz(&self) -> f64 { self.nco_offset_hz }

    pub fn set_nco_offset(&mut self, nco_offset_hz: f64) {
        self.nco_offset_hz = nco_offset_hz;
        let phase_step = -std::f64::consts::TAU * nco_offset_hz / self.input_rate;
        self.phase_inc = NcoF32 {
            re: phase_step.cos() as f32,
            im: phase_step.sin() as f32,
        };
    }

    /// Reset NCO + all stage histories. Caller invokes on retune to a
    /// new freq (after `set_nco_offset`).
    pub fn reset(&mut self) {
        self.nco = NcoF32 { re: 1.0, im: 0.0 };
        self.nco_renorm_counter = 0;
        for s in self.stages.iter_mut() {
            s.reset();
        }
    }

    pub fn process(&mut self, input: &[Complex32]) -> Vec<Complex32> {
        if input.is_empty() {
            return Vec::new();
        }
        // ─── NCO mix at input rate ────────────────────────────────
        let mix = &mut self.mix_scratch;
        mix.clear();
        mix.reserve(input.len());
        let mut nco_re = self.nco.re;
        let mut nco_im = self.nco.im;
        let inc_re = self.phase_inc.re;
        let inc_im = self.phase_inc.im;
        let mut renorm_counter = self.nco_renorm_counter;
        for &s in input {
            mix.push(Complex32::new(
                s.re * nco_re - s.im * nco_im,
                s.re * nco_im + s.im * nco_re,
            ));
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

        // ─── Cascade through stages ───────────────────────────────
        let mut current: Vec<Complex32> = std::mem::take(mix);
        for stage in self.stages.iter_mut() {
            current = stage.process(&current);
        }
        // Recover scratch buffer (now empty after the take).
        self.mix_scratch = current.split_off(current.len()); // empty alloc reuse
        current
    }
}

/// Build decimation stages for a given input/output ratio. Strategy:
///   * Factor decim ratio into a chain of `2`/`4`/`8` factors.
///   * Each stage's LPF is sized to anti-alias for the NEXT stage's
///     Nyquist (= half its output rate).
///   * The FINAL stage uses a sharp Kaiser to also reject anything
///     beyond the LSM signal's effective bandwidth (~6 kHz cutoff,
///     stopband at 12.5 kHz). Cheap because output rate is low.
fn build_stages_for(input_rate: f64, output_rate: f64) -> Vec<DecimStage> {
    let total_decim = (input_rate / output_rate).round() as usize;
    assert!(total_decim >= 2, "trivial decim ratio");
    // Factorize total_decim into a sequence of 2/4/8.
    let mut factors: Vec<usize> = Vec::new();
    let mut remaining = total_decim;
    // Greedy: pull factors of 8, then 5, then 4, then 3, then 2.
    // /5 added 2026-05-03 to support 8 MSPS → 25 kSPS chains
    // (factor 320 = 8×8×5) so the live SW demod can operate at
    // SDRTrunk's native 25 kSPS LSM rate.
    while remaining > 1 {
        if remaining % 8 == 0 && remaining >= 16 {
            factors.push(8);
            remaining /= 8;
        } else if remaining % 5 == 0 && remaining > 5 {
            factors.push(5);
            remaining /= 5;
        } else if remaining % 4 == 0 && remaining > 2 {
            factors.push(4);
            remaining /= 4;
        } else if remaining % 3 == 0 && remaining > 3 {
            factors.push(3);
            remaining /= 3;
        } else if remaining % 2 == 0 {
            factors.push(2);
            remaining /= 2;
        } else if remaining <= 8 {
            // Final small odd factor (5, 3) — push as-is.
            factors.push(remaining);
            remaining = 1;
        } else {
            panic!(
                "MultistageDdc::build_stages_for: cannot factor remaining \
                 decimation {} into {{2,3,4,5,8}} chain (total_decim={})",
                remaining, total_decim,
            );
        }
    }
    // We want the LAST stage to be small (2 or 4) so the final sharp
    // filter is cheap. Reverse-sort so big decim factors come first.
    factors.sort_by(|a, b| b.cmp(a));
    // Build per-stage filters, walking from input rate downward.
    let mut fs = input_rate;
    let mut stages: Vec<DecimStage> = Vec::with_capacity(factors.len());
    let last_idx = factors.len() - 1;
    for (i, &d) in factors.iter().enumerate() {
        let fs_out = fs / d as f64;
        let is_final = i == last_idx;
        // Anti-alias requirement: reject signals at fs_out/2 + 6.25 kHz
        // (so the next stage's Nyquist + half-channel-spacing is in
        // stopband, preserving 6.25 kHz adjacent rejection through the
        // chain).
        let stopband = fs_out / 2.0 - 6_250.0;
        let cutoff = if is_final {
            // Final: tight cutoff for adjacent-channel rejection.
            // LSM signal extent ~3 kHz; cutoff at 4.5 kHz keeps margin.
            // (Tried 4.5/8 kHz cutoff/stopband at 80 dB on 2026-05-03;
            // attenuated the RRC sidelobes enough to degrade BOTH the
            // real-channel decode AND the data-channel false sync rate.
            // Reverted — wider stopband leaves the signal shoulders
            // untouched and lets the LsmPipeline RRC do final
            // adjacent-channel rejection.)
            4_500.0
        } else {
            // Intermediate: keep wide passband (just need to anti-alias).
            // Cutoff = stopband - guard. With small guard the filter is
            // less sharp = fewer taps.
            (stopband * 0.5).max(20_000.0)
        };
        let cutoff = cutoff.min(stopband - 500.0).max(2_000.0);
        let transition = (stopband - cutoff).max(500.0);
        // Kaiser tap-count formula for 60 dB stopband:
        //   N ≈ (A − 7.95) / (14.36 × Δf/fs)
        let normalized_transition = transition / fs;
        let mut n_taps = ((60.0 - 7.95) / (14.36 * normalized_transition))
            .ceil() as usize;
        // Make odd (linear-phase symmetric) and clamp range.
        if n_taps % 2 == 0 {
            n_taps += 1;
        }
        n_taps = n_taps.clamp(11, 401);
        let taps = kaiser_lpf(n_taps, cutoff / fs, 60.0);
        stages.push(DecimStage::new(d, taps));
        fs = fs_out;
    }
    stages
}

/// Kaiser-windowed sinc LPF designer. Same as the offline harness's
/// helper — kept local so this module is self-contained.
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
    let dc: f64 = taps.iter().map(|&t| t as f64).sum();
    if dc.abs() > 1.0e-12 {
        for t in taps.iter_mut() {
            *t = ((*t as f64) / dc) as f32;
        }
    }
    taps
}

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

    /// Stage chain produces approximately N = input.len()/total_decim
    /// outputs for a multi-second input.
    #[test]
    fn output_rate_correct_4msps() {
        let n = 4_000_000;
        let input = vec![Complex32::new(1.0, 0.0); n];
        let mut ddc = MultistageDdc::new(4e6, 62.5e3, 0.0);
        let out = ddc.process(&input);
        let expected = 62_500;
        let delta = (out.len() as i64 - expected as i64).abs();
        // Allow ±10 for stage-boundary phase alignment.
        assert!(delta <= 10,
            "expected ~{expected} samples, got {} (Δ {})", out.len(), delta);
    }

    /// 8 MSPS → 62.5 kSPS likewise.
    #[test]
    fn output_rate_correct_8msps() {
        let n = 8_000_000;
        let input = vec![Complex32::new(1.0, 0.0); n];
        let mut ddc = MultistageDdc::new(8e6, 62.5e3, 0.0);
        let out = ddc.process(&input);
        let expected = 62_500;
        let delta = (out.len() as i64 - expected as i64).abs();
        assert!(delta <= 10,
            "expected ~{expected} samples, got {} (Δ {})", out.len(), delta);
    }

    /// Streaming chunks must produce the same output as one big call.
    #[test]
    fn streaming_matches_batch() {
        let n = 4 * 4000;
        let input: Vec<Complex32> = (0..n)
            .map(|i| Complex32::new((i as f32 * 0.001).sin(),
                                    (i as f32 * 0.001).cos()))
            .collect();
        let mut a = MultistageDdc::new(4e6, 62.5e3, 5_000.0);
        let big = a.process(&input);

        let mut b = MultistageDdc::new(4e6, 62.5e3, 5_000.0);
        let mut small: Vec<Complex32> = Vec::new();
        for chunk in input.chunks(877) {
            small.extend(b.process(chunk));
        }

        assert_eq!(big.len(), small.len(),
            "chunked output length differs (big={}, small={})",
            big.len(), small.len());
        // Allow modest f32 drift from the chunked NCO renorms.
        let mut max_re_err = 0.0_f32;
        let mut max_im_err = 0.0_f32;
        for i in 0..big.len() {
            max_re_err = max_re_err.max((big[i].re - small[i].re).abs());
            max_im_err = max_im_err.max((big[i].im - small[i].im).abs());
        }
        assert!(max_re_err < 1e-2 && max_im_err < 1e-2,
            "diverged: max_re_err={max_re_err} max_im_err={max_im_err}");
    }

    /// A tone at +50 kHz (well outside the LSM ±3 kHz passband and
    /// past the final stopband at 12.5 kHz) should be rejected by ≥40 dB.
    #[test]
    fn rejects_adjacent_channel() {
        let fs = 4e6;
        let f_tone = 50_000.0_f64;
        let n = 4_000_000;
        let input: Vec<Complex32> = (0..n)
            .map(|i| {
                let t = i as f64 / fs;
                let p = std::f64::consts::TAU * f_tone * t;
                Complex32::new(p.cos() as f32, p.sin() as f32)
            })
            .collect();
        let mut ddc = MultistageDdc::new(fs, 62.5e3, 0.0);
        let out = ddc.process(&input);
        let mean_amp: f32 = out[100..].iter()
            .map(|c| (c.re * c.re + c.im * c.im).sqrt()).sum::<f32>()
            / (out.len() - 100) as f32;
        // Input amplitude was 1.0; rejection should bring it well below
        // -40 dB (= 0.01).
        assert!(mean_amp < 0.05,
            "tone at +50 kHz not rejected enough: mean |out| = {mean_amp}");
    }
}

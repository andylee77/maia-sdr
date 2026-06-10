//! Halfband DDC -- direct port of SDRTrunk's `HalfBandTunerChannelSource`
//! filter chain. Used as the offline SW oracle for HDL-vs-SW dibit
//! comparison (Track 2 forensics, 2026-05-03).
//!
//! Why a port: our existing `multistage_ddc.rs` (Kaiser cascade) decodes
//! some calls bit-exact with SDRTrunk on the wideband WAV but fails on
//! others depending on adjacent-channel content. SDRTrunk's
//! `HalfBandTunerChannelSource` decodes the same wideband perfectly --
//! its halfband cascade has sharper transition + better stopband
//! rejection at the cost of fixed power-of-2 decimation rates.
//!
//! Pipeline (4 MSPS in, 25 kSPS out -- the rate `LsmPipeline` consumes):
//!
//! ```text
//!   ComplexMixer (NCO at -nco_offset_hz, f64 phase, 4 MSPS)
//!     -> Halfband /2 cascade x7  (4M -> 2M -> 1M -> 500k -> 250k -> 125k -> 62.5k -> 31.25k)
//!     -> Linear interp 5:4        (31.25k -> 25k)
//! ```
//!
//! Filter design exactly mirrors SDRTrunk's `RealDecimateX{2,4,...,128}Filter`
//! cascade:
//!   * /2 stage at LOWEST rate uses 63-tap Hamming halfband (sharpest)
//!   * /4 stage uses 23-tap Blackman
//!   * /8, /16 stages use 15-tap Blackman
//!   * /32, /64, /128 stages use 11-tap Blackman
//!
//! See SDRTrunk source:
//!   * `dsp/filter/halfband/RealHalfBandDecimationFilter.java` -- scalar impl
//!   * `dsp/filter/decimate/RealDecimateX{N}Filter.java` -- per-stage tap
//!     count + window choice
//!   * `dsp/filter/FilterFactory.java::getHalfBand()` -- coefficient design
//!   * `dsp/window/WindowFactory.java::getHamming|getBlackman` -- window math

use crate::lsm::Complex32;
use std::f64::consts::PI;

// SDRTrunk's per-stage tap count + window. Index 0 = innermost stage
// (lowest sample rate, sharpest filter); each subsequent stage runs
// at 2x the sample rate of the previous and tolerates a wider transition
// band, so taps get shorter going outward.
const STAGE_TAPS: &[(usize, Window)] = &[
    (63, Window::Hamming),    // X2  -- last stage applied (lowest rate)
    (23, Window::Blackman),   // X4
    (15, Window::Blackman),   // X8
    (15, Window::Blackman),   // X16
    (11, Window::Blackman),   // X32
    (11, Window::Blackman),   // X64
    (11, Window::Blackman),   // X128 -- first stage applied (highest rate)
];

#[derive(Clone, Copy)]
enum Window { Hamming, Blackman }

fn window(w: Window, n: usize) -> Vec<f32> {
    let mut out = vec![0.0; n];
    let denom = (n - 1) as f64;
    for x in 0..n {
        out[x] = match w {
            Window::Hamming => {
                (0.54 - 0.46 * (2.0 * PI * x as f64 / denom).cos()) as f32
            }
            Window::Blackman => {
                // SDRTrunk's exact 3-term coefficients
                let a0 = 0.426590713672_f64;
                let a1 = 0.496560619089_f64;
                let a2 = 0.0768486672399_f64;
                (a0
                    - a1 * (2.0 * PI * x as f64 / denom).cos()
                    + a2 * (4.0 * PI * x as f64 / denom).cos()) as f32
            }
        };
    }
    out
}

/// Halfband filter design matching SDRTrunk `FilterFactory.getHalfBand()`.
/// Length must be N=4m+3 (7, 11, 15, 19, 23, ..., 63). Center tap = 0.5,
/// odd-index taps = 0, even-index taps = sinc-windowed.
fn halfband_taps(length: usize, w: Window) -> Vec<f32> {
    assert!((length - 3) % 4 == 0,
            "halfband length must be N=4m+3 (got {length})");
    let win = window(w, length);
    let mut taps = vec![0.0_f32; length];
    let half_len = length as i64 / 2;
    for x in 0..length {
        let offset = x as i64 - half_len;
        if offset == 0 {
            taps[x] = 0.5;
        } else if x % 2 == 0 {
            // sin(offset * π/2) is ±1 for odd offset (which we have when
            // x even and length odd). All non-center even-x taps fall
            // here.
            let off_f = offset as f64;
            taps[x] = ((off_f * PI / 2.0).sin() / (off_f * PI)) as f32 * win[x];
        }
        // odd-x taps stay 0 (halfband structure)
    }
    taps
}

/// Single /2 halfband decimator -- direct port of SDRTrunk's
/// `RealHalfBandDecimationFilter`. State is a sliding sample buffer
/// of length `(taps - 1)` so successive calls are continuous.
struct HalfbandDecimator {
    coeffs: Vec<f32>,
    overlap: usize, // = coeffs.len() - 1
    history: Vec<f32>, // last `overlap` samples carried into next call
}

impl HalfbandDecimator {
    fn new(taps: Vec<f32>) -> Self {
        assert!((taps.len() + 1) % 4 == 0,
                "halfband filter length must be N=4m+3");
        let overlap = taps.len() - 1;
        Self {
            coeffs: taps,
            overlap,
            history: vec![0.0; overlap],
        }
    }

    /// Process N input samples (N must be even), produce N/2 output
    /// samples. Mirrors `decimateReal()` in SDRTrunk's scalar
    /// implementation -- exploits halfband symmetry by summing mirrored
    /// taps, plus center * 0.5.
    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        assert!(input.len() % 2 == 0, "halfband input must have even length");
        // Total buffer = previous overlap + new input
        let buf_len = input.len() + self.overlap;
        let mut buf = Vec::with_capacity(buf_len);
        buf.extend_from_slice(&self.history);
        buf.extend_from_slice(input);

        let mut out = Vec::with_capacity(input.len() / 2);
        let half = self.overlap / 2;
        for ptr in (0..input.len()).step_by(2) {
            let mut acc = 0.0_f32;
            // Even-coefficient indices only (odd are zero in halfband).
            // Symmetric pair: coeffs[i] is the same as coeffs[N-1-i],
            // so multiply ONCE by sum of mirrored input samples.
            let mut i = 0;
            while i < half {
                acc += self.coeffs[i] *
                    (buf[ptr + i] + buf[ptr + (self.overlap - i)]);
                i += 2;
            }
            // Center tap (always 0.5)
            acc += buf[ptr + half] * 0.5;
            out.push(acc);
        }

        // Save last `overlap` samples for next call.
        self.history.copy_from_slice(&buf[input.len()..]);
        out
    }
}

/// Cascade of N halfband /2 stages. Inputs at high rate, outputs at
/// rate / 2^N. Stages applied in order: stage[0] runs at INPUT rate
/// (highest), stage[N-1] runs at output rate * 2 (lowest before final
/// decimation).
///
/// SDRTrunk allocates short taps to high-rate stages (where transition
/// band is wide relative to Nyquist) and long taps to low-rate stages
/// (where adjacent-channel rejection matters). We mirror that.
struct HalfbandCascade {
    stages: Vec<HalfbandDecimator>,
}

impl HalfbandCascade {
    /// Build a cascade producing total decimation 2^n_stages. Tap
    /// counts read from STAGE_TAPS in REVERSE (highest-rate stage uses
    /// the LAST entry of STAGE_TAPS; lowest-rate stage uses index 0,
    /// the 63-tap Hamming).
    fn new(n_stages: usize) -> Self {
        assert!(n_stages >= 1 && n_stages <= STAGE_TAPS.len(),
                "n_stages {n_stages} out of range 1..={}",
                STAGE_TAPS.len());
        // stages[0] runs first at the highest rate.
        // STAGE_TAPS[0] = innermost (lowest-rate, longest taps) stage.
        // So stages[0] gets STAGE_TAPS[n_stages-1], stages[n_stages-1]
        // gets STAGE_TAPS[0].
        let mut stages = Vec::with_capacity(n_stages);
        for s in 0..n_stages {
            let cfg_idx = n_stages - 1 - s;
            let (length, w) = STAGE_TAPS[cfg_idx];
            stages.push(HalfbandDecimator::new(halfband_taps(length, w)));
        }
        Self { stages }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut current = input.to_vec();
        for stage in self.stages.iter_mut() {
            current = stage.process(&current);
        }
        current
    }
}

/// Heterodyne DDC: NCO mixer + halfband cascade. Direct port of
/// SDRTrunk's `HalfBandTunerChannelSource`. f64 phase accumulator
/// (no per-sample drift, unlike `multistage_ddc.rs`'s f32 NCO).
pub struct HalfbandDdc {
    input_rate: f64,
    /// Sum after `n_stages` /2 decimations applied to `input_rate`.
    output_rate: f64,
    /// Phase increment per input sample, radians. f64 to avoid drift
    /// over long captures (60+ sec at 4 MSPS = 250M+ samples).
    phase_step: f64,
    phase: f64,
    /// Separate I and Q halfband cascades -- SDRTrunk runs them as
    /// distinct real-valued filters, NOT a complex filter.
    i_cascade: HalfbandCascade,
    q_cascade: HalfbandCascade,
}

impl HalfbandDdc {
    /// `input_rate`: incoming IQ rate, e.g. 4_000_000.
    /// `n_stages`: number of /2 halfband decimations (1..=7).
    /// `nco_offset_hz`: how far to shift the spectrum (negative offset
    /// brings a signal at LO+offset to DC).
    pub fn new(input_rate: f64, n_stages: usize, nco_offset_hz: f64) -> Self {
        let output_rate = input_rate / (1u64 << n_stages) as f64;
        let phase_step = -2.0 * PI * nco_offset_hz / input_rate;
        Self {
            input_rate,
            output_rate,
            phase_step,
            phase: 0.0,
            i_cascade: HalfbandCascade::new(n_stages),
            q_cascade: HalfbandCascade::new(n_stages),
        }
    }

    pub fn output_rate(&self) -> f64 { self.output_rate }
    #[allow(dead_code)]
    pub fn input_rate(&self) -> f64 { self.input_rate }

    /// Mix and decimate one chunk. Caller chooses chunk size; the
    /// halfband cascade handles continuity across chunks via its
    /// internal history buffers. Chunk length must be a multiple of
    /// 2^n_stages so each stage gets an even-length input.
    pub fn process(&mut self, iq: &[Complex32]) -> Vec<Complex32> {
        let n = iq.len();
        let stride: usize = 1 << self.i_cascade.stages.len();
        assert!(n % stride == 0,
                "HalfbandDdc::process input len {n} not multiple of {stride}");

        // Mix to baseband. f64 phase accumulator -> per-sample cos/sin
        // -> complex multiply.
        let mut i_in = Vec::with_capacity(n);
        let mut q_in = Vec::with_capacity(n);
        let mut phase = self.phase;
        for s in iq {
            let (sin_p, cos_p) = phase.sin_cos();
            let (cos_p, sin_p) = (cos_p as f32, sin_p as f32);
            // (s.re + j*s.im) * (cos_p + j*sin_p)
            i_in.push(s.re * cos_p - s.im * sin_p);
            q_in.push(s.re * sin_p + s.im * cos_p);
            phase += self.phase_step;
            // Periodic phase wrap to keep magnitude finite (every 256
            // samples is safe; rem_euclid keeps phase in [0, 2π)).
            if phase.abs() > 1e9 {
                phase = phase.rem_euclid(2.0 * PI);
            }
        }
        self.phase = phase;

        // Halfband cascades on I and Q (each runs as a real filter).
        let i_out = self.i_cascade.process(&i_in);
        let q_out = self.q_cascade.process(&q_in);

        i_out.into_iter().zip(q_out.into_iter())
            .map(|(i, q)| Complex32 { re: i, im: q })
            .collect()
    }
}

/// Convenience: 4 MSPS -> 25 kSPS using SDRTrunk's halfband cascade
/// (/128 -> 31.25 kSPS) plus a 5:4 linear interp resample.
///
/// 5:4 ratio chosen because P25 signal at ~6 kHz bandwidth is well
/// inside the 31.25 kSPS Nyquist (15.625 kHz), so a gentle linear
/// interp introduces negligible distortion vs the halfband math.
///
/// Output length will be `input.len() * 25_000 / input_rate`. Caller
/// must pass chunk lengths that are multiples of 128 (so the cascade
/// produces an integer output).
pub fn halfband_ddc_to_25k(
    iq_in: &[Complex32],
    input_rate: f64,
    nco_offset_hz: f64,
) -> Vec<Complex32> {
    // Stage count = log2(input_rate / 31_250). For 4 MSPS that's 7.
    let n_stages = (input_rate / 31_250.0).log2().round() as usize;
    let mut ddc = HalfbandDdc::new(input_rate, n_stages, nco_offset_hz);
    let stride = 1usize << n_stages;
    // Truncate input to a multiple of stride (drop trailing partial chunk).
    let usable = (iq_in.len() / stride) * stride;
    let post_halfband = ddc.process(&iq_in[..usable]);

    // 5:4 linear-interp resample 31.25 kSPS -> 25 kSPS.
    // Output sample n maps to input position n * 31.25 / 25 = n * 1.25.
    let in_rate = ddc.output_rate();
    let out_rate = 25_000.0;
    let ratio = in_rate / out_rate; // 1.25 for 31.25k->25k
    let n_out = ((post_halfband.len() as f64) / ratio).floor() as usize;
    let mut out = Vec::with_capacity(n_out);
    for n in 0..n_out {
        let pos = n as f64 * ratio;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(post_halfband.len() - 1);
        let a = post_halfband[i0];
        let b = post_halfband[i1];
        out.push(Complex32 {
            re: a.re * (1.0 - frac) + b.re * frac,
            im: a.im * (1.0 - frac) + b.im * frac,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn halfband_taps_have_expected_structure() {
        let t = halfband_taps(11, Window::Blackman);
        assert_eq!(t.len(), 11);
        // Center tap = 0.5
        assert!((t[5] - 0.5).abs() < 1e-9, "center tap should be 0.5");
        // Odd-x taps (except center which IS odd at index 5 and is the
        // exception) should be zero. Index 5 IS the center for length
        // 11 (halfLength = 5). Other odd indices: 1, 3, 7, 9.
        for i in [1usize, 3, 7, 9] {
            assert!(t[i].abs() < 1e-9, "odd tap {i} should be 0, got {}", t[i]);
        }
        // Even-x taps: non-zero
        for i in [0usize, 2, 4, 6, 8, 10] {
            assert!(t[i].abs() > 0.0,
                    "even tap {i} should be non-zero, got {}", t[i]);
        }
        // Symmetric: t[i] == t[N-1-i]
        for i in 0..11 {
            assert!((t[i] - t[10 - i]).abs() < 1e-6,
                    "tap {i} not symmetric");
        }
    }

    #[test]
    fn halfband_dc_passthrough() {
        // DC input should pass through with unity-ish gain (halfband
        // is normalized to ~0.5 + sum(taps) ≈ 1 at DC).
        let mut dec = HalfbandDecimator::new(halfband_taps(11, Window::Blackman));
        let input = vec![1.0_f32; 256];
        let out = dec.process(&input);
        assert_eq!(out.len(), 128);
        // After history fills (first ~5 samples), output should be ~1.0
        for v in &out[10..] {
            assert!((v - 1.0).abs() < 0.01,
                    "DC passthrough should be ~1.0, got {v}");
        }
    }

    #[test]
    fn cascade_4mhz_to_31250() {
        let mut casc = HalfbandCascade::new(7); // 4M -> 31.25k
        let input: Vec<f32> = (0..(128 * 100)).map(|x| (x as f32 % 7.0)).collect();
        let out = casc.process(&input);
        assert_eq!(out.len(), input.len() / 128);
    }
}

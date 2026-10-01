//! Speech synthesis from model parameters, comfort noise and tones. Ports
//! jmbe v1.0.9 `codec/MBESynthesizer.java`, `MBENoiseSequenceGenerator`,
//! `WhiteNoiseGenerator` (with `java.util.Random`), `ambe/ToneGenerator`
//! and `oscillator/Oscillator` + `Complex`.
//!
//! The harmonic sum is evaluated as jmbe does: one `Math.cos` per sample and
//! harmonic, in double where jmbe promotes to double. The 256-point DFT pair
//! of the unvoiced path uses `realfft` instead of JTransforms, the only
//! place the output differs from jmbe by more than libm rounding.

use core::f32::consts::PI;
use std::sync::Arc;

use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex;

use super::frame::AmbeTone;
use super::params::ModelParameters;
// Shared with the IMBE port (jmbe's `MBESynthesizer` constants and
// `imbe/Window.SYNTHESIS`).
use crate::jmbe::{
    clip, synthesis_window, AUDIO_SCALAR, TWO56_OVER_TWO_PI, TWO_PI, UNVOICED_SCALING_COEFFICIENT,
    WHITE_NOISE_SCALAR,
};

const SAMPLES_PER_FRAME: usize = 160;

/// `java.util.Random`: the 48-bit LCG and `nextFloat()`.
#[derive(Debug, Clone)]
pub(super) struct JavaRandom {
    seed: u64,
}

impl JavaRandom {
    const MULTIPLIER: u64 = 0x5DEECE66D;
    const MASK: u64 = (1 << 48) - 1;

    /// `new Random(seed)`.
    pub fn new(seed: u64) -> Self {
        JavaRandom {
            seed: (seed ^ Self::MULTIPLIER) & Self::MASK,
        }
    }

    fn next(&mut self, bits: u32) -> u32 {
        self.seed = (self.seed.wrapping_mul(Self::MULTIPLIER).wrapping_add(0xB)) & Self::MASK;
        (self.seed >> (48 - bits)) as u32
    }

    /// `nextFloat()`.
    pub fn next_float(&mut self) -> f32 {
        self.next(24) as f32 / (1u32 << 24) as f32
    }
}

/// `WhiteNoiseGenerator`: comfort noise for erasures and muting. jmbe seeds
/// its `Random` from the clock; the seed here is the caller's.
pub(super) struct WhiteNoiseGenerator {
    random: JavaRandom,
}

impl WhiteNoiseGenerator {
    pub fn new(seed: u64) -> Self {
        let mut random = JavaRandom::new(seed);
        // The constructor fills a buffer jmbe never reads: 1 + 256 draws.
        for _ in 0..257 {
            random.next_float();
        }
        WhiteNoiseGenerator { random }
    }

    /// `getSamples(160, 0.003f)`.
    pub fn samples(&mut self) -> [f32; SAMPLES_PER_FRAME] {
        let mut out = [0.0f32; SAMPLES_PER_FRAME];
        for s in out.iter_mut() {
            *s = (self.random.next_float() * 2.0f32 - 1.0f32) * 0.003f32;
        }
        out
    }
}

/// `MBENoiseSequenceGenerator` (Alg 117). Its first buffer is all zeros:
/// jmbe never primes it.
pub(super) struct NoiseSequence {
    sample: f32,
    buffer: [f32; 256],
}

impl NoiseSequence {
    pub fn new() -> Self {
        NoiseSequence {
            sample: 3147.0,
            buffer: [0.0; 256],
        }
    }

    fn next(&mut self) -> f32 {
        let next = self.sample;
        self.sample = ((171.0f32 * next) + 11213.0f32) % 53125.0f32;
        next
    }

    /// `nextBuffer()`: the current 256 samples; then the last 96 move to the
    /// front and 160 new ones follow.
    pub fn next_buffer(&mut self) -> [f32; 256] {
        let copy = self.buffer;
        self.buffer.copy_within(160..256, 0);
        for x in 96..256 {
            self.buffer[x] = self.next();
        }
        copy
    }
}

/// `oscillator.Oscillator` at 8 kHz with its `Complex` arithmetic.
#[derive(Debug, Clone)]
struct Oscillator {
    frequency: f64,
    step: (f32, f32),
    angle: (f32, f32),
}

impl Oscillator {
    fn new() -> Self {
        let mut o = Oscillator {
            frequency: 0.0,
            step: (1.0, 0.0),
            angle: (0.0, -1.0),
        };
        o.set_frequency(0.0);
        o
    }

    fn set_frequency(&mut self, frequency: f64) {
        self.frequency = frequency;
        let angle = (2.0f64 * core::f64::consts::PI * frequency / 8000.0f64) as f32;
        let angle = angle as f64;
        self.step = (angle.cos() as f32, angle.sin() as f32);
    }

    fn generate(&mut self, gain: f32) -> [f32; SAMPLES_PER_FRAME] {
        let mut samples = [0.0f32; SAMPLES_PER_FRAME];
        if self.frequency != 0.0 {
            for s in samples.iter_mut() {
                // rotate(): multiply, then fastNormalize().
                let (i, q) = self.angle;
                let (si, sq) = self.step;
                let i2 = (i * si) - (q * sq);
                let q2 = (q * si) + (i * sq);
                let scale = 1.9999f32 - ((i2 * i2) + (q2 * q2));
                self.angle = (i2 * scale, q2 * scale);
                *s = self.angle.1 * gain;
            }
        }
        samples
    }
}

/// `ToneGenerator`.
pub(super) struct ToneGenerator {
    oscillator1: Oscillator,
    oscillator2: Oscillator,
}

impl ToneGenerator {
    pub fn new() -> Self {
        ToneGenerator {
            oscillator1: Oscillator::new(),
            oscillator2: Oscillator::new(),
        }
    }

    /// `generate(toneParameters)` for a valid tone.
    pub fn generate(&mut self, tone: &AmbeTone) -> [f32; SAMPLES_PER_FRAME] {
        let (f1, f2) = tone.frequencies();
        let mut gain = tone.amplitude as f32 / 127.0f32;
        if f2 > 0.0 {
            gain *= 0.5f32;
            self.oscillator1.set_frequency(f1);
            self.oscillator2.set_frequency(f2);
            let mut samples = self.oscillator1.generate(gain);
            let samples2 = self.oscillator2.generate(gain);
            for (s, s2) in samples.iter_mut().zip(samples2.iter()) {
                *s += *s2;
            }
            samples
        } else {
            self.oscillator1.set_frequency(f1);
            self.oscillator1.generate(gain)
        }
    }
}

/// `MBESynthesizer`'s state: phases, the previous inverse DFT, the noise
/// sequence and the FFT plans.
pub(super) struct Synthesizer {
    noise: NoiseSequence,
    previous_phase_o: [f32; 57],
    previous_phase_v: [f32; 57],
    previous_uw: [f32; 256],
    fft_r2c: Arc<dyn RealToComplex<f32>>,
    fft_c2r: Arc<dyn ComplexToReal<f32>>,
    fft_input: Vec<f32>,
    fft_spectrum: Vec<Complex<f32>>,
    fft_output: Vec<f32>,
    fft_scratch: Vec<Complex<f32>>,
}

impl Synthesizer {
    pub fn new() -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let r2c = planner.plan_fft_forward(256);
        let c2r = planner.plan_fft_inverse(256);
        let scratch_len = r2c.get_scratch_len().max(c2r.get_scratch_len());
        Synthesizer {
            noise: NoiseSequence::new(),
            previous_phase_o: [0.0; 57],
            previous_phase_v: [0.0; 57],
            previous_uw: [0.0; 256],
            fft_r2c: r2c,
            fft_c2r: c2r,
            fft_input: vec![0.0; 256],
            fft_spectrum: vec![Complex::new(0.0, 0.0); 129],
            fft_output: vec![0.0; 256],
            fft_scratch: vec![Complex::new(0.0, 0.0); scratch_len],
        }
    }

    /// `getVoice(parameters)` with `previous` as `getPreviousFrame()`.
    /// Samples are in -0.95..0.95, or NaN where jmbe's are (see
    /// `AmbeDecoder::decode`).
    pub fn voice(
        &mut self,
        current: &ModelParameters,
        previous: &ModelParameters,
    ) -> [f32; SAMPLES_PER_FRAME] {
        // Alg 117
        let u = self.noise.next_buffer();
        let unvoiced = self.unvoiced(current, &u);
        let voiced = self.voiced(current, previous, &u);

        // Alg 142
        let mut audio = [0.0f32; SAMPLES_PER_FRAME];
        for x in 0..SAMPLES_PER_FRAME {
            audio[x] = clip((voiced[x] + unvoiced[x]) * AUDIO_SCALAR);
        }
        audio
    }

    /// `getUnvoiced()`: Algs 118-126.
    fn unvoiced(
        &mut self,
        p: &ModelParameters,
        white_noise: &[f32; 256],
    ) -> [f32; SAMPLES_PER_FRAME] {
        for x in 0..256 {
            self.fft_input[x] = white_noise[x] * synthesis_window(x as i32 - 128);
        }
        // Alg 118
        self.fft_r2c
            .process_with_scratch(
                &mut self.fft_input,
                &mut self.fft_spectrum,
                &mut self.fft_scratch,
            )
            .expect("realfft forward");

        // Algs 122 & 123: band edges.
        let l_count = p.l;
        let multiplier = TWO56_OVER_TWO_PI * p.w0;

        // Alg 120: per-bin scaling of the unvoiced bands. jmbe's packed
        // JTransforms layout puts the Nyquist real part where bin 0's
        // imaginary part would be.
        let mut bin_scalar = [0.0f32; 128];
        for l in 1..=l_count {
            if p.voicing[l] {
                continue;
            }
            let a_min = ((l as f32 - 0.5f32) * multiplier) as f64;
            let a_min = a_min.ceil() as i32;
            let b_max = (((l as f32 + 0.5f32) * multiplier) as f64).ceil() as i32;
            let mut numerator = 0.0f32;
            for n in a_min..b_max {
                if n < 128 {
                    let (re, im) = if n == 0 {
                        (self.fft_spectrum[0].re, self.fft_spectrum[128].re)
                    } else {
                        let c = self.fft_spectrum[n as usize];
                        (c.re, c.im)
                    };
                    numerator += re * re;
                    numerator += im * im;
                }
            }
            let denominator = (b_max - a_min) as f32;
            let scalar = UNVOICED_SCALING_COEFFICIENT * p.enhanced[l]
                / (((numerator / denominator) as f64).sqrt() as f32);
            for n in a_min..b_max {
                if n < 128 {
                    bin_scalar[n as usize] = scalar;
                }
            }
        }

        // Algs 119, 120 & 124: scale; the rest (bin_scalar 0) is zeroed.
        let s0 = bin_scalar[0];
        self.fft_spectrum[0].re *= s0;
        self.fft_spectrum[0].im = 0.0;
        self.fft_spectrum[128].re *= s0;
        self.fft_spectrum[128].im = 0.0;
        for bin in 1..128 {
            let s = bin_scalar[bin];
            self.fft_spectrum[bin].re *= s;
            self.fft_spectrum[bin].im *= s;
        }

        // Alg 125: inverse DFT, scaled by 1/N as `realInverse(Uw, true)`.
        self.fft_c2r
            .process_with_scratch(
                &mut self.fft_spectrum,
                &mut self.fft_output,
                &mut self.fft_scratch,
            )
            .expect("realfft inverse");
        let mut uw = [0.0f32; 256];
        for (u, &o) in uw.iter_mut().zip(self.fft_output.iter()) {
            *u = o / 256.0f32;
        }

        // Alg 126: weighted overlap-add with the previous frame.
        let mut unvoiced = [0.0f32; SAMPLES_PER_FRAME];
        for n in 0..SAMPLES_PER_FRAME {
            let previous_window = synthesis_window(n as i32);
            let current_window = synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32);
            let previous_uw = if n < 128 {
                self.previous_uw[n + 128]
            } else {
                0.0
            };
            let current_uw = if n >= 32 { uw[n - 32] } else { 0.0 };
            unvoiced[n] = ((previous_window * previous_uw) + (current_window * current_uw))
                / ((previous_window * previous_window) + (current_window * current_window));
        }
        self.previous_uw = uw;
        unvoiced
    }

    /// `getVoiced()`: Algs 127-141.
    fn voiced(
        &mut self,
        current: &ModelParameters,
        previous: &ModelParameters,
        u: &[f32; 256],
    ) -> [f32; SAMPLES_PER_FRAME] {
        let current_frequency = current.w0;
        let previous_frequency = previous.w0;
        let average_frequency = (previous_frequency + current_frequency) / 2.0f32;
        let phase_offset = average_frequency * SAMPLES_PER_FRAME as f32;

        // Alg 139
        let mut current_phase_v = [0.0f32; 57];
        for l in 1..=56 {
            self.previous_phase_v[l] %= TWO_PI;
            current_phase_v[l] = self.previous_phase_v[l] + (phase_offset * l as f32);
        }

        if !previous.has_voiced_bands() && !current.has_voiced_bands() {
            self.previous_phase_v = current_phase_v;
            return [0.0; SAMPLES_PER_FRAME];
        }

        let current_l = current.l;
        let previous_l = previous.l;
        let max_l = current_l.max(previous_l);
        let mut current_voicing = current.voicing.clone();
        current_voicing.resize(max_l + 1, false);
        let mut previous_voicing = previous.voicing.clone();
        previous_voicing.resize(max_l + 1, false);

        // Alg 140
        let unvoiced_count = current.unvoiced_band_count();
        let mut current_phase_o = [0.0f32; 57];
        let threshold = (current_l as f32 / 4.0f32).floor() as usize;
        for l in 1..=56 {
            if l <= threshold {
                current_phase_o[l] = current_phase_v[l];
            } else if l <= max_l {
                let pl = WHITE_NOISE_SCALAR * u[l] - PI;
                current_phase_o[l] =
                    current_phase_v[l] + ((unvoiced_count as f32 * pl) / current_l as f32);
            }
        }

        let current_m = &current.enhanced;
        let previous_m = &previous.enhanced;
        let mut voiced = [0.0f32; SAMPLES_PER_FRAME];
        let exceeds_threshold = ((current_frequency - previous_frequency).abs() as f64)
            >= (0.1f64 * current_frequency as f64);

        // Alg 127. jmbe loops n outer, l inner; each sample sums its
        // harmonics in increasing l either way, so l outer is the same sum.
        for l in 1..=max_l {
            let cv = current_voicing[l];
            let pv = previous_voicing[l];
            let lf = l as f32;
            if cv && pv {
                if l >= 8 || exceeds_threshold {
                    // Alg 133 (cos and the products in double).
                    let pm = previous_m[l];
                    let cm = current_m[l];
                    for n in 0..SAMPLES_PER_FRAME {
                        let previous_phase =
                            self.previous_phase_o[l] + (previous_frequency * n as f32 * lf);
                        let a = 2.0f64
                            * ((synthesis_window(n as i32) * pm) as f64
                                * (previous_phase as f64).cos());
                        voiced[n] = (voiced[n] as f64 + a) as f32;
                        let current_phase = current_phase_o[l]
                            + (current_frequency
                                * (n as i32 - SAMPLES_PER_FRAME as i32) as f32
                                * lf);
                        let b = 2.0f64
                            * ((synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32) * cm) as f64
                                * (current_phase as f64).cos());
                        voiced[n] = (voiced[n] as f64 + b) as f32;
                    }
                } else {
                    // Algs 134-138: interpolated amplitude, quadratic phase.
                    let pm = previous_m[l];
                    let cm = current_m[l];
                    let ol = current_phase_o[l] - self.previous_phase_o[l] - (phase_offset * lf);
                    let wl = (ol - (TWO_PI * ((ol + PI) / TWO_PI).floor())) / 160.0f32;
                    for n in 0..SAMPLES_PER_FRAME {
                        let amplitude = pm + ((n as f32 / SAMPLES_PER_FRAME as f32) * (cm - pm));
                        let phase = self.previous_phase_o[l]
                            + (((previous_frequency * lf) + wl) * n as f32)
                            + ((current_frequency - previous_frequency)
                                * ((l * n * n) as f32 / 320.0f32));
                        let a = 2.0f64 * (amplitude as f64 * (phase as f64).cos());
                        voiced[n] = (voiced[n] as f64 + a) as f32;
                    }
                }
            } else if pv {
                // Alg 131 (float).
                let pm = previous_m[l];
                for n in 0..SAMPLES_PER_FRAME {
                    let phase = self.previous_phase_o[l] + (previous_frequency * n as f32 * lf);
                    voiced[n] +=
                        2.0f32 * (synthesis_window(n as i32) * pm * ((phase as f64).cos() as f32));
                }
            } else if cv {
                // Alg 132 (float).
                let cm = current_m[l];
                for n in 0..SAMPLES_PER_FRAME {
                    let phase = current_phase_o[l]
                        + (current_frequency * (n as i32 - SAMPLES_PER_FRAME as i32) as f32 * lf);
                    voiced[n] += 2.0f32
                        * (synthesis_window(n as i32 - SAMPLES_PER_FRAME as i32)
                            * cm
                            * ((phase as f64).cos() as f32));
                }
            }
            // Alg 130: unvoiced in both frames contributes nothing.
        }

        self.previous_phase_v = current_phase_v;
        self.previous_phase_o = current_phase_o;
        voiced
    }
}

#[cfg(test)]
#[path = "synth_tests.rs"]
mod tests;

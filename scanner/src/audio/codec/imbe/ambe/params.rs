//! Voice model parameters of one frame. Ports jmbe v1.0.9
//! `codec/ambe/AMBEModelParameters.java` and the spectral enhancement and
//! adaptive smoothing of `codec/MBEModelParameters.java`.
//!
//! Arithmetic follows the Java expression by expression, including where
//! Java promotes to double (`Math.cos`/`exp`/`pow`/`sqrt`, `2.0 * float`,
//! float-vs-double compares), so the parameters come out the same to the bit
//! wherever the platform libm agrees with Java's.

use core::f32::consts::PI;

use super::frame::AmbeFrame;
use super::tables::{
    DIFFERENTIAL_GAIN, FUNDAMENTAL, HOCB5, HOCB6, HOCB7, HOCB8, LMPR_BLOCK_LENGTH, PRBA24, PRBA58,
    VOICING,
};
use super::FrameType;

const TWO_PI: f32 = PI * 2.0;
const PI_96: f32 = 0.96 * PI;
/// `1.0f / (2.0f * (float)Math.sqrt(2.0f))`.
fn one_over_two_sqrt_two() -> f32 {
    1.0f32 / (2.0f32 * (2.0f64.sqrt() as f32))
}

/// b0 of the initial / muted frame (`AMBEFundamentalFrequency.W124`).
const DEFAULT_B0: usize = 124;

/// `(float)(frequency * 2.0 * Math.PI)`: radians per sample.
pub(super) fn fundamental_w0(b0: usize) -> f32 {
    (FUNDAMENTAL[b0].0 * 2.0 * core::f64::consts::PI) as f32
}

#[derive(Debug, Clone)]
pub(super) struct ModelParameters {
    /// The `AMBEFundamentalFrequency` (b0 index); after a repeat, the
    /// previous frame's.
    pub b0: usize,
    pub frame_type: FrameType,
    pub w0: f32,
    pub l: usize,
    /// `[L + 1]`, index 0 unused (but counted by `unvoiced_band_count`).
    pub voicing: Vec<bool>,
    pub log2_spectral: Vec<f32>,
    pub spectral: Vec<f32>,
    pub enhanced: Vec<f32>,
    pub local_energy: f32,
    pub amplitude_threshold: i32,
    pub error_rate: f32,
    pub error_count_total: u32,
    /// Never set for AMBE (jmbe's `mErrorCount4`), always 0.
    pub error_count4: u32,
    pub repeat_count: u32,
    pub gain: f32,
}

impl ModelParameters {
    /// `MBEModelParameters(fundamental)`: field defaults, arrays unset.
    fn base(b0: usize) -> Self {
        let (_, l, frame_type) = FUNDAMENTAL[b0];
        ModelParameters {
            b0,
            frame_type,
            w0: fundamental_w0(b0),
            l,
            voicing: Vec::new(),
            log2_spectral: Vec::new(),
            spectral: Vec::new(),
            enhanced: Vec::new(),
            local_energy: 75000.0,
            amplitude_threshold: 20480,
            error_rate: 0.0,
            error_count_total: 0,
            error_count4: 0,
            repeat_count: 0,
            gain: 0.0,
        }
    }

    /// `setMBEFundamentalFrequency()`.
    fn set_fundamental(&mut self, b0: usize) {
        let (_, l, frame_type) = FUNDAMENTAL[b0];
        self.b0 = b0;
        self.frame_type = frame_type;
        self.w0 = fundamental_w0(b0);
        self.l = l;
    }

    /// `setDefaults(frameType)`.
    fn set_defaults(&mut self, frame_type: FrameType) {
        self.frame_type = frame_type;
        self.voicing = vec![false; self.l + 1];
        self.log2_spectral = vec![0.0; self.l + 1];
        self.spectral = vec![1.0; self.l + 1];
        self.enhanced = self.spectral.clone();
        self.gain = 0.0;
    }

    /// The initial frame (`new AMBEModelParameters()`): W124 with VOICE
    /// defaults.
    pub fn new_default() -> Self {
        let mut p = Self::base(DEFAULT_B0);
        p.set_defaults(FrameType::Voice);
        p
    }

    /// Parameters of a voice, silence or erasure frame
    /// (`AMBEFrame.getVoiceParameters(previous)`).
    ///
    /// `previous` is `&mut` because jmbe shares arrays with it: Alg 44 writes
    /// `previous.log2[0]`, and a repeated frame shares (and the adaptive
    /// smoothing then rewrites) the previous frame's voicing array, which
    /// the synthesizer reads as the previous frame's.
    pub fn from_frame(frame: &AmbeFrame, previous: &mut ModelParameters) -> Self {
        let b = &frame.b;
        let mut p = Self::base(frame.b0 as usize);

        // Alg 55 & 56
        p.error_count_total = frame.errors[0] + frame.errors[1];
        p.error_rate = (0.95f32 * previous.error_rate) + (0.001064f32 * p.error_count_total as f32);

        let e0 = frame.errors[0];
        if p.frame_type == FrameType::Erasure {
            p.set_defaults(FrameType::Erasure);
        } else if e0 >= 4 || (e0 >= 2 && p.error_count_total >= 6) {
            // Alg 59-64: repeat the previous frame.
            p.repeat_count = previous.repeat_count + 1;
            p.set_fundamental(previous.b0);
            p.gain = previous.gain;
            p.voicing = previous.voicing.clone();
            p.log2_spectral = previous.log2_spectral.clone();
            p.spectral = previous.spectral.clone();
            p.enhance_spectral_amplitudes(previous.local_energy, previous.amplitude_threshold);
            p.local_energy = previous.local_energy;
            // Shared array in jmbe: the smoothing above rewrote previous's too.
            previous.voicing.clone_from(&p.voicing);
        } else {
            if p.frame_type == FrameType::Voice {
                p.set_voicing_decisions(b[1] as usize);
            } else {
                p.voicing = vec![false; p.l + 1];
            }
            // Alg 26
            let (gain, adjustment) = DIFFERENTIAL_GAIN[b[2] as usize];
            p.gain = (gain + adjustment) + (0.5f32 * previous.gain);
            p.decode_prba_vector(b, previous);
        }
        p
    }

    /// `isErasureFrame()`.
    pub fn is_erasure(&self) -> bool {
        self.frame_type == FrameType::Erasure
    }

    /// `isMaxFrameRepeat()`.
    pub fn is_max_frame_repeat(&self) -> bool {
        self.repeat_count >= 4
    }

    /// `isFrameMuted()`: unused by jmbe's synthesizer, kept for metadata.
    pub fn is_frame_muted(&self) -> bool {
        (self.error_rate as f64) > 0.096 || self.repeat_count >= 4
    }

    /// `hasVoicedBands()` (over the whole array, index 0 included).
    pub fn has_voiced_bands(&self) -> bool {
        self.voicing.iter().any(|&v| v)
    }

    /// `getUnvoicedBandCount()`: counts index 0, which is never voiced.
    pub fn unvoiced_band_count(&self) -> usize {
        self.voicing.iter().filter(|&&v| !v).count()
    }

    /// `setVoicingDecisions(int b1)`.
    fn set_voicing_decisions(&mut self, b1: usize) {
        let bands = &VOICING[b1];
        let mut voicing = vec![false; self.l + 1];
        for (l, v) in voicing.iter_mut().enumerate().skip(1) {
            let index = ((l as f32 * self.w0) * 16.0f32 / TWO_PI) as usize;
            // jmbe throws for an index past 7; the table's L keeps it below 8.
            *v = bands[index.min(7)];
        }
        self.voicing = voicing;
    }

    /// `decodePRBAVector()`: Algs 27-46.
    fn decode_prba_vector(&mut self, b: &[u32; 9], previous: &mut ModelParameters) {
        let l_count = self.l;
        let mut g = [0.0f32; 9];
        let prba24 = PRBA24[b[3] as usize];
        g[2..5].copy_from_slice(&prba24);
        let prba58 = PRBA58[b[4] as usize];
        g[5..9].copy_from_slice(&prba58);

        // Alg 27 & 28: inverse DCT of G. `R += 2.0 * G * (float)cos` is
        // evaluated in double.
        let mut r = [0.0f32; 9];
        for i in 1..=8 {
            r[i] = g[1];
            for m in 2..=8 {
                let angle = (PI * (m - 1) as f32 * (i as f32 - 0.5f32)) / 8.0f32;
                let cos = (angle as f64).cos() as f32;
                r[i] = (r[i] as f64 + 2.0f64 * g[m] as f64 * cos as f64) as f32;
            }
        }

        let mut c = [[0.0f32; 18]; 5];
        // Alg 29, 31, 33, 35
        c[1][1] = 0.5f32 * (r[1] + r[2]);
        c[2][1] = 0.5f32 * (r[3] + r[4]);
        c[3][1] = 0.5f32 * (r[5] + r[6]);
        c[4][1] = 0.5f32 * (r[7] + r[8]);
        // Alg 30, 32, 34, 36
        let k = one_over_two_sqrt_two();
        c[1][2] = k * (r[1] - r[2]);
        c[2][2] = k * (r[3] - r[4]);
        c[3][2] = k * (r[5] - r[6]);
        c[4][2] = k * (r[7] - r[8]);

        let j = LMPR_BLOCK_LENGTH[l_count];

        // Alg 37: higher order coefficients.
        for i in 1..=4 {
            if j[i] > 2 {
                let coefficients = match i {
                    1 => HOCB5[b[5] as usize],
                    2 => HOCB6[b[6] as usize],
                    3 => HOCB7[b[7] as usize],
                    _ => HOCB8[b[8] as usize],
                };
                let n = (j[i] - 2).min(4);
                c[i][3..3 + n].copy_from_slice(&coefficients[..n]);
            }
        }

        // Alg 38, 39: inverse DCT of C, rearranged as T.
        let mut t = vec![0.0f32; l_count + 1];
        let mut l_pointer = 1;
        for i in 1..=4 {
            for jj in 1..=j[i] {
                let mut acc = c[i][1];
                for kk in 2..=j[i] {
                    let angle = (PI * (kk - 1) as f32 * (jj as f32 - 0.5f32)) / j[i] as f32;
                    acc += 2.0f32 * c[i][kk] * (angle as f64).cos() as f32;
                }
                t[l_pointer] = acc;
                l_pointer += 1;
            }
        }

        let previous_l = previous.l;

        // Alg 40 & 41
        let kappa = previous_l as f32 / l_count as f32;
        let mut kf = vec![0.0f32; l_count + 1];
        let mut k_floor = vec![0usize; l_count + 1];
        let mut s = vec![0.0f32; l_count + 1];

        // Alg 44 (jmbe writes the previous frame's array).
        previous.log2_spectral[0] = previous.log2_spectral[1];
        let previous_a = &previous.log2_spectral;

        for l in 1..=l_count {
            kf[l] = kappa * l as f32;
            k_floor[l] = (kf[l] as f64).floor() as usize;
            s[l] = kf[l] - k_floor[l] as f32;
        }

        let akl = |index: usize| {
            if index <= previous_l {
                previous_a[index]
            } else {
                previous_a[previous_l]
            }
        };

        // Alg 42 & 43: pre-compute the sum. jmbe takes the "+1" neighbour
        // as floor(k[l + 1]) rather than floor(k[l]) + 1.
        let mut summation43 = 0.0f32;
        let mut lambda_sum = 0.0f32;
        for l in 1..=l_count {
            let akl_previous = akl(k_floor[l]);
            let plus1 = if l < l_count { l + 1 } else { l_count };
            let akl_plus1_previous = akl(k_floor[plus1]);
            summation43 += ((1.0f32 - s[l]) * akl_previous) + (s[l] * akl_plus1_previous);
            lambda_sum += t[l];
        }
        lambda_sum /= l_count as f32;

        // Alg 42
        let log2_l = ((l_count as f64).ln() / 2.0f64.ln()) as f32;
        let gain = self.gain - (0.5f32 * log2_l) - lambda_sum;

        let mut log2_spectral = vec![0.0f32; l_count + 1];
        log2_spectral[0] = 1.0;
        let mut spectral = vec![0.0f32; l_count + 1];
        let unvoiced_coefficient = 0.2046f32 / ((self.w0 as f64).sqrt() as f32);
        summation43 *= 0.65f32 / l_count as f32;

        for l in 1..=l_count {
            // Alg 44 & 45
            let akl_previous = if k_floor[l] == 0 {
                previous_a[1]
            } else {
                akl(k_floor[l])
            };
            let plus1 = if l < l_count { l + 1 } else { l_count };
            let akl_plus1_previous = akl(k_floor[plus1]);

            // Alg 43
            log2_spectral[l] = t[l]
                + (0.65f32 * (1.0f32 - s[l]) * akl_previous)
                + (0.65f32 * s[l] * akl_plus1_previous)
                - summation43
                + gain;

            // Alg 46
            let amplitude = ((0.693f32 * log2_spectral[l]) as f64).exp() as f32;
            spectral[l] = if self.voicing[l] {
                amplitude
            } else {
                unvoiced_coefficient * amplitude
            };
        }

        self.log2_spectral = log2_spectral;
        self.spectral = spectral;
        self.enhance_spectral_amplitudes(previous.local_energy, previous.amplitude_threshold);
    }

    /// `enhanceSpectralAmplitudes()`: Algs 105-111, then 112-116.
    fn enhance_spectral_amplitudes(&mut self, previous_local_energy: f32, previous_threshold: i32) {
        let l_count = self.l;
        let w0 = self.w0;
        let spectral = &self.spectral;

        // Alg 105 & 106. RM1 accumulates a double product.
        let mut rm0 = 0.0f32;
        let mut rm1 = 0.0f32;
        for l in 1..=l_count {
            let squared = spectral[l] * spectral[l];
            rm0 += squared;
            rm1 = (rm1 as f64 + squared as f64 * ((w0 * l as f32) as f64).cos()) as f32;
        }

        let rm0_squared = rm0 * rm0;
        let rm1_squared = rm1 * rm1;
        let mut enhanced = vec![0.0f32; l_count + 1];

        if rm0 == 0.0 {
            self.enhanced = enhanced;
            return;
        }

        // Alg 107: enhancement weights.
        let mut weights = vec![0.0f32; l_count + 1];
        for l in 1..=l_count {
            let cos = ((w0 * l as f32) as f64).cos() as f32;
            let temp = (PI_96 * (rm0_squared + rm1_squared - (2.0f32 * rm0 * rm1 * cos)))
                / (w0 * rm0 * (rm0_squared - rm1_squared));
            weights[l] = ((spectral[l] as f64).sqrt() * (temp as f64).powf(0.25)) as f32;
        }

        // Alg 108
        for l in 1..=l_count {
            enhanced[l] = if 8 * l <= l_count {
                spectral[l]
            } else if weights[l] > 1.2f32 {
                spectral[l] * 1.2f32
            } else if weights[l] < 0.5f32 {
                spectral[l] * 0.5f32
            } else {
                spectral[l] * weights[l]
            };
        }

        // Alg 109 & 110
        let mut denominator = 0.0f32;
        for l in 1..=l_count {
            denominator += enhanced[l] * enhanced[l];
        }
        let y = ((rm0 / denominator) as f64).sqrt() as f32;
        for l in 1..=l_count {
            enhanced[l] *= y;
        }

        // Alg 111
        self.local_energy = (0.95f32 * previous_local_energy) + (0.05f32 * rm0);
        if self.local_energy < 10000.0f32 {
            self.local_energy = 10000.0;
        }

        self.enhanced = enhanced;
        self.apply_adaptive_smoothing(previous_threshold);
    }

    /// `applyAdaptiveSmoothing()`: Algs 112-116.
    fn apply_adaptive_smoothing(&mut self, previous_threshold: i32) {
        let l_count = self.l;
        let error_rate = self.error_rate as f64;

        // Alg 112 & 113: only in the presence of errors.
        if !(error_rate <= 0.005 && self.error_count_total <= 4) {
            let energy = (self.local_energy as f64).powf(0.375f32 as f64) as f32;
            let vm = if self.error_rate <= 0.0125f32 && self.error_count4 == 0 {
                (45.255f32 * energy) / (((277.26f32 * self.error_rate) as f64).exp() as f32)
            } else {
                1.414f32 * energy
            };
            for l in 1..=l_count {
                if self.enhanced[l] > vm {
                    self.voicing[l] = true;
                }
            }
        }

        // Alg 114
        let mut am = 0.0f32;
        for l in 1..=l_count {
            am += self.enhanced[l];
        }

        // Alg 115
        let tm = if error_rate <= 0.005 && self.error_count_total <= 6 {
            20480
        } else {
            6000 - (300 * self.error_count_total as i32) + previous_threshold
        };
        self.amplitude_threshold = tm;

        // Alg 116
        if am > tm as f32 {
            let scale = tm as f32 / am;
            for l in 1..=l_count {
                self.enhanced[l] *= scale;
            }
        }
    }
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod tests;

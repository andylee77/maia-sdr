//! AMBE+2 (AMBE 3600x2450, the DMR half-rate vocoder) decoder -- Rust port of
//! jmbe v1.0.9 by Dennis Sheirer (https://github.com/DSheirer/jmbe, GPL-3.0):
//! `codec/ambe/AMBEAudioCodec`, `AMBEFrame`, `AMBEModelParameters`,
//! `AMBESynthesizer` and the shared `MBESynthesizer` / `MBEModelParameters`.
//! (jmbe's `codec/ambeplus` package is unused by its codec and not ported.)
//!
//! Pipeline: 9 bytes (72 bits, interleaved) -> C0..C3 -> Golay(24,12) on C0
//! -> PN descramble + Golay(23,12) on C1 -> b0..b8 -> model parameters
//! (prediction from the previous frame, enhancement, smoothing) -> voiced
//! harmonics + unvoiced noise -> 160 f32 samples (20 ms @ 8 kHz). Tone
//! frames produce jmbe's tone oscillator output instead.
//!
//! The decoder keeps state across frames as jmbe does; one decoder per
//! audio stream (timeslot). The output matches jmbe's float for float
//! except for the unvoiced DFT (`realfft` vs JTransforms rounding) and
//! last-bit libm differences; see `tests.rs` for the reference comparison.

// Not yet wired into the DMR audio path.
#![allow(dead_code)]

mod frame;
mod params;
mod synth;
mod tables;

#[allow(unused_imports)] // public API, not used by the app yet
pub use frame::{AmbeFrame, AmbeTone, ToneKind};

use params::ModelParameters;
use synth::{Synthesizer, ToneGenerator, WhiteNoiseGenerator};

/// Samples per 20 ms frame at 8 kHz.
pub const SAMPLES_PER_FRAME: usize = 160;

/// `java.util.Random` seed of the comfort noise (jmbe seeds from the clock).
pub const DEFAULT_NOISE_SEED: u64 = 0x4A4D_4245; // "JMBE"

/// AMBE frame type (`codec/FrameType`), from b0 and the tone pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    Voice,
    Silence,
    /// b0 120-123 (or a damaged tone b0): comfort noise.
    Erasure,
    Tone,
}

/// What happened to the last frame, for metrics and metadata
/// (jmbe's `getAudioWithMetadata` reports only the tone).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AmbeFrameInfo {
    /// The frame as decoded (FEC, type, b-vector, tone).
    pub frame: AmbeFrame,
    /// C0 + C1 bit errors (jmbe's `getErrorCountTotal()`).
    pub error_count: u32,
    /// jmbe's running error rate (Alg 56), 0 for tone frames.
    pub error_rate: f32,
    /// Frames repeated in a row (Alg 59-64): bad C0, or an invalid tone.
    pub repeat_count: u32,
    /// The output is jmbe's comfort noise (erasure, or 4+ repeats).
    pub comfort_noise: bool,
    /// NaN samples jmbe would have output, replaced by 0.0.
    pub nan_samples: u32,
}

/// AMBE+2 decoder state (`AMBEAudioCodec` / `AMBESynthesizer`).
pub struct AmbeDecoder {
    previous: ModelParameters,
    synthesizer: Synthesizer,
    white_noise: WhiteNoiseGenerator,
    tone_generator: ToneGenerator,
    last: Option<AmbeFrameInfo>,
}

impl Default for AmbeDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl AmbeDecoder {
    pub fn new() -> Self {
        Self::with_noise_seed(DEFAULT_NOISE_SEED)
    }

    /// A decoder whose comfort noise is `new Random(seed)` (after jmbe's
    /// 257 constructor draws), for comparing with a seeded jmbe.
    pub fn with_noise_seed(seed: u64) -> Self {
        AmbeDecoder {
            previous: ModelParameters::new_default(),
            synthesizer: Synthesizer::new(),
            white_noise: WhiteNoiseGenerator::new(seed),
            tone_generator: ToneGenerator::new(),
            last: None,
        }
    }

    /// jmbe's `reset()` at a call boundary: only the previous frame's
    /// parameters return to the default (phases, noise state and tone
    /// oscillators carry on).
    pub fn reset(&mut self) {
        self.previous = ModelParameters::new_default();
    }

    /// Decodes one 72-bit frame (9 bytes, MSB first, still interleaved and
    /// FEC-encoded, as `VoiceMessage.getAMBEFrames()` gives it) into 160
    /// samples in -0.95..0.95.
    ///
    /// jmbe outputs NaN where an unvoiced band's noise spectrum is all zero
    /// (the first voice frame after construction: its noise buffer starts
    /// zeroed), and in the next frame's overlap; SDRTrunk's float to short
    /// cast makes those 0. They are returned here as 0.0 and counted in
    /// `last_frame().nan_samples`.
    pub fn decode(&mut self, frame: &[u8; 9]) -> [f32; SAMPLES_PER_FRAME] {
        let mut audio = self.decode_raw(frame);
        let mut nan = 0;
        for s in audio.iter_mut() {
            if s.is_nan() {
                *s = 0.0;
                nan += 1;
            }
        }
        if let Some(info) = self.last.as_mut() {
            info.nan_samples = nan;
        }
        audio
    }

    /// The last decoded frame's FEC, type and tone. `None` before the first.
    pub fn last_frame(&self) -> Option<&AmbeFrameInfo> {
        self.last.as_ref()
    }

    /// `AMBESynthesizer.getAudio(frame)`, NaNs included.
    fn decode_raw(&mut self, bytes: &[u8; 9]) -> [f32; SAMPLES_PER_FRAME] {
        let frame = AmbeFrame::decode(bytes);
        let mut comfort_noise = false;
        let mut error_rate = 0.0;
        let mut repeat_count = 0;
        let audio;

        if frame.frame_type == FrameType::Tone {
            let tone = frame.tone.expect("tone frame");
            if tone.id.is_some() {
                audio = self.tone_generator.generate(&tone);
            } else if !self.previous.is_max_frame_repeat() {
                repeat_count = self.previous.repeat_count;
                // jmbe's `setRepeatCount(getRepeatCount())` does not count
                // this repeat; the previous frame is synthesised as both
                // current and previous.
                let current = self.previous.clone();
                audio = self.synthesizer.voice(&current, &self.previous);
            } else {
                // Frame muting.
                self.previous = ModelParameters::new_default();
                audio = self.white_noise.samples();
                comfort_noise = true;
            }
        } else {
            let parameters = ModelParameters::from_frame(&frame, &mut self.previous);
            error_rate = parameters.error_rate;
            repeat_count = parameters.repeat_count;
            if !parameters.is_max_frame_repeat() {
                if parameters.is_erasure() {
                    audio = self.white_noise.samples();
                    comfort_noise = true;
                } else {
                    audio = self.synthesizer.voice(&parameters, &self.previous);
                }
                self.previous = parameters;
            } else {
                // Frame muting.
                self.previous = ModelParameters::new_default();
                audio = self.white_noise.samples();
                comfort_noise = true;
            }
        }

        self.last = Some(AmbeFrameInfo {
            frame,
            error_count: frame.errors[0] + frame.errors[1],
            error_rate,
            repeat_count,
            comfort_noise,
            nan_samples: 0,
        });
        audio
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

//! IMBE vocoder.
//!
//! Converts raw 144-bit IMBE frames (`ImbeFrameRaw`) to 160-sample
//! PCM audio at 8 kHz (20 ms per frame).
//!
//! Backend is **JMBE** — a pure-Rust port of the Java MBE library
//! (SDRTrunk's `jmbe` fork). Implementation lives in [`crate::jmbe`];
//! this module wraps it in [`JmbeDecoder`] which handles the i16 PCM
//! conversion the rest of the daemon expects. An earlier mbelib C-FFI
//! wrapper existed as a fallback; recoverable from git if ever needed.

/// Samples per 20 ms IMBE frame at 8 kHz (160 mono samples).
pub const SAMPLES_PER_FRAME: usize = 160;

use crate::protocol::p25::voice_frame::ImbeFrameRaw;

/// JMBE-based IMBE decoder. Pure Rust, no FFI. Better audio quality
/// than mbelib due to spectral enhancement and adaptive smoothing.
pub struct JmbeDecoder {
    inner: crate::jmbe::ImbeDecoder,
}

impl JmbeDecoder {
    pub fn new() -> Self {
        Self {
            inner: crate::jmbe::ImbeDecoder::new(),
        }
    }

    pub fn reset(&mut self) {
        self.inner = crate::jmbe::ImbeDecoder::new();
    }

    /// Decode one raw 144-bit IMBE frame to PCM.
    /// Returns 160 signed 16-bit PCM samples at 8 kHz.
    pub fn decode_frame(&mut self, frame: &ImbeFrameRaw) -> [i16; SAMPLES_PER_FRAME] {
        let floats = self.inner.decode_frame(&frame.bits);
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        for (i, &f) in floats.iter().enumerate() {
            // JMBE outputs float in roughly [-1.0, 1.0] range scaled
            // for 16-bit. Clamp and convert.
            let scaled = (f * 32767.0).round();
            pcm[i] = scaled.clamp(-32768.0, 32767.0) as i16;
        }
        pcm
    }
}
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

//! Voice codecs, ported from SDRTrunk's jmbe: IMBE (P25 Phase 1) and AMBE+2 (DMR), behind one
//! trait. A frame decodes to 20 ms of 8 kHz audio.

pub mod imbe;

pub use imbe::ambe as ambe2;

/// Samples per 20 ms frame at 8 kHz.
pub const SAMPLES_PER_FRAME: usize = 160;
/// An IMBE frame counts as an error when its FEC corrected more than this many bits.
pub const IMBE_ERROR_BITS: u32 = 4;

/// What the codec made of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameQuality {
    /// Bits the frame's FEC corrected.
    pub bit_errors: u32,
    /// Counts as a bad frame (IMBE: more than `IMBE_ERROR_BITS`; AMBE+2: any).
    pub error: bool,
}

pub trait VoiceCodec: Send {
    /// One frame (IMBE: 18 bytes; AMBE+2: 9 bytes) into `pcm`.
    fn decode(&mut self, frame: &[u8], pcm: &mut [i16; SAMPLES_PER_FRAME]) -> FrameQuality;
    /// A new call: forget the previous frame.
    fn reset(&mut self);
}

fn to_pcm(audio: &[f32; SAMPLES_PER_FRAME], pcm: &mut [i16; SAMPLES_PER_FRAME]) {
    for (o, &f) in pcm.iter_mut().zip(audio) {
        *o = (f * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
    }
}

pub struct Imbe(imbe::ImbeDecoder);

impl Default for Imbe {
    fn default() -> Self {
        Imbe(imbe::ImbeDecoder::new())
    }
}

impl VoiceCodec for Imbe {
    fn decode(&mut self, frame: &[u8], pcm: &mut [i16; SAMPLES_PER_FRAME]) -> FrameQuality {
        let Ok(bits) = <&[u8; 18]>::try_from(frame) else {
            pcm.fill(0);
            return FrameQuality { bit_errors: 0, error: true };
        };
        to_pcm(&self.0.decode_frame(bits), pcm);
        let bit_errors = self.0.last_error_count();
        FrameQuality { bit_errors, error: bit_errors > IMBE_ERROR_BITS }
    }

    fn reset(&mut self) {
        self.0 = imbe::ImbeDecoder::new();
    }
}

#[derive(Default)]
pub struct Ambe2(ambe2::AmbeDecoder);

impl VoiceCodec for Ambe2 {
    fn decode(&mut self, frame: &[u8], pcm: &mut [i16; SAMPLES_PER_FRAME]) -> FrameQuality {
        let Ok(bits) = <&[u8; 9]>::try_from(frame) else {
            pcm.fill(0);
            return FrameQuality { bit_errors: 0, error: true };
        };
        to_pcm(&self.0.decode(bits), pcm);
        let bit_errors = self.0.last_frame().map_or(0, |f| f.error_count);
        FrameQuality { bit_errors, error: bit_errors > 0 }
    }

    fn reset(&mut self) {
        self.0.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imbe_frames_decode_to_audio() {
        // A live TG 301 frame.
        let hex = "ee54972c2201c44dfa51099dbdf3dd0809a7";
        let frame: Vec<u8> = (0..36).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let mut codec = Imbe::default();
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        let mut peak = 0;
        for _ in 0..5 {
            // Captured on the air: its FEC corrects 9 bits.
            let q = codec.decode(&frame, &mut pcm);
            assert_eq!((q.bit_errors, q.error), (9, true));
            peak = peak.max(pcm.iter().map(|s| s.unsigned_abs()).max().unwrap());
        }
        assert!(peak > 100, "peak {peak}");
        assert!(codec.decode(&frame[..9], &mut pcm).error, "a frame of the wrong size");
    }

    #[test]
    fn ambe2_decodes_clay_electric_frames() {
        let frames = include_bytes!("imbe/ambe/test_frames_clay_ts2.bin");
        let mut codec = Ambe2::default();
        let mut pcm = [0i16; SAMPLES_PER_FRAME];
        let mut peak = 0;
        for f in frames.chunks_exact(9) {
            codec.decode(f, &mut pcm);
            peak = peak.max(pcm.iter().map(|s| s.unsigned_abs()).max().unwrap());
        }
        assert!(peak > 1000, "peak {peak}");
    }
}

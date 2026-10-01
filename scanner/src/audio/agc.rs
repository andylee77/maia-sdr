//! The post-vocoder AGC, for both protocols: a vocoder outputs each radio at its own mic and
//! deviation level. Voiced frames steer the gain toward a target RMS over about a second (an
//! EMA on frame RMS); silent frames get the current gain but do not move it, so pauses do not
//! pump. A soft knee at the vocoder's own clip line keeps boosted peaks inside 16 bits.

/// Samples of a 20 ms frame.
const FRAME: usize = 160;
const TARGET_RMS: f32 = 2500.0;
/// ~40 frames (800 ms) on the RMS tracker.
const RMS_ALPHA: f32 = 0.025;
/// Keeps per-frame gain changes gentle when the RMS jumps between speakers.
const SCALE_ALPHA: f32 = 0.08;
const MIN_SCALE: f32 = 0.25;
const MAX_SCALE: f32 = 8.0;
/// Linear up to the vocoder's clip line (0.95 of full scale), then tanh toward the ceiling.
const KNEE: f32 = 31000.0;
const CEIL: f32 = 32700.0;
/// Frames quieter than this are silence.
pub const SILENT_PEAK: u16 = 16;

#[derive(Debug, Clone, Copy)]
pub struct PcmAgc {
    rms_ema: f32,
    scale: f32,
}

impl Default for PcmAgc {
    fn default() -> Self {
        PcmAgc { rms_ema: TARGET_RMS, scale: 1.0 }
    }
}

impl PcmAgc {
    /// Back to unity (a new call or speaker).
    pub fn reset(&mut self) {
        *self = PcmAgc::default();
    }

    /// Level one frame in place. Returns whether it was silent.
    pub fn apply(&mut self, pcm: &mut [i16; FRAME]) -> bool {
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let silent = peak < SILENT_PEAK;
        if !silent {
            let sum_sq: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            let rms = (sum_sq / FRAME as f64).sqrt() as f32;
            if rms > 0.0 {
                self.rms_ema = (1.0 - RMS_ALPHA) * self.rms_ema + RMS_ALPHA * rms;
                let target = (TARGET_RMS / self.rms_ema.max(1.0)).clamp(MIN_SCALE, MAX_SCALE);
                self.scale = (1.0 - SCALE_ALPHA) * self.scale + SCALE_ALPHA * target;
            }
        }
        for s in pcm.iter_mut() {
            let v = f32::from(*s) * self.scale;
            let a = v.abs();
            let limited = if a <= KNEE { v } else { v.signum() * (KNEE + (CEIL - KNEE) * ((a - KNEE) / (CEIL - KNEE)).tanh()) };
            *s = limited as i16;
        }
        silent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loud_audio_is_limited_and_silence_left_alone() {
        let mut agc = PcmAgc::default();
        let mut loud = [32000i16; FRAME];
        for _ in 0..50 {
            let mut p = loud;
            agc.apply(&mut p);
            loud = p;
        }
        assert!(loud.iter().all(|&s| s <= 32700));
        let mut quiet = [3i16; FRAME];
        let mut agc = PcmAgc::default();
        assert!(agc.apply(&mut quiet));
        assert_eq!(quiet, [3i16; FRAME]);
    }

    #[test]
    fn a_quiet_talker_is_brought_up_over_a_second() {
        let mut agc = PcmAgc::default();
        let tone = |amp: f32| -> [i16; FRAME] {
            std::array::from_fn(|i| (amp * (i as f32 * 0.3).sin()) as i16)
        };
        let mut out = tone(500.0);
        for _ in 0..100 {
            out = tone(500.0);
            agc.apply(&mut out);
        }
        let rms = (out.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / FRAME as f64).sqrt();
        assert!((1500.0..3500.0).contains(&rms), "{rms}");
    }
}

//! The post-vocoder AGC, for both protocols: a vocoder outputs each radio at its own mic and
//! deviation level. A transmission starts at the gain its radio's recent ones needed (`levels`),
//! else at unity. Voiced frames steer the gain toward a target RMS: quickly over the first half
//! second (a radio's level changes from call to call, as when another dispatcher takes a
//! console), then over about a second (an EMA on frame RMS); silent frames get the current gain
//! but do not move it, so pauses do not pump. A soft knee at the vocoder's own clip line keeps
//! boosted peaks inside 16 bits. The AGC also measures each transmission's speech level, before
//! its gain, for `levels`.

/// Samples of a 20 ms frame.
const FRAME: usize = 160;
const TARGET_RMS: f32 = 2500.0;
/// ~40 frames (800 ms) on the RMS tracker once settled.
const RMS_ALPHA: f32 = 0.025;
/// Keeps per-frame gain changes gentle when the RMS jumps between speakers.
const SCALE_ALPHA: f32 = 0.08;
/// The first voiced frames of a transmission (500 ms) track faster.
const FAST_FRAMES: u32 = 25;
const FAST_RMS_ALPHA: f32 = 0.1;
const FAST_SCALE_ALPHA: f32 = 0.4;
const MIN_SCALE: f32 = 0.25;
/// +24 dB: a quiet dispatcher reaches a console's audio near -48 dBFS.
const MAX_SCALE: f32 = 16.0;
/// Frames above this RMS (-50 dBFS) are speech for the level.
const LEVEL_GATE: f32 = 103.6;
/// A level needs this many speech frames (200 ms).
const LEVEL_FRAMES: u32 = 10;
/// Linear up to the vocoder's clip line (0.95 of full scale), then tanh toward the ceiling.
const KNEE: f32 = 31000.0;
const CEIL: f32 = 32700.0;
/// Frames quieter than this are silence.
pub const SILENT_PEAK: u16 = 16;

#[derive(Debug, Clone, Copy)]
pub struct PcmAgc {
    rms_ema: f32,
    scale: f32,
    /// Voiced frames since the start.
    voiced: u32,
    /// The speech frames' energy and count since the level was last taken.
    level_sq: f64,
    level_frames: u32,
}

impl Default for PcmAgc {
    fn default() -> Self {
        PcmAgc { rms_ema: TARGET_RMS, scale: 1.0, voiced: 0, level_sq: 0.0, level_frames: 0 }
    }
}

impl PcmAgc {
    /// A new transmission: at the gain `level` (its radio's recent speech RMS) needs, else at
    /// unity.
    pub fn start(&mut self, level: Option<f32>) {
        *self = PcmAgc::default();
        if let Some(level) = level.filter(|l| *l > 0.0) {
            self.rms_ema = level;
            self.scale = (TARGET_RMS / level).clamp(MIN_SCALE, MAX_SCALE);
        }
    }

    /// The speech level (RMS before the gain) since it was last taken, and count afresh; none
    /// with too little speech.
    pub fn take_level(&mut self) -> Option<f32> {
        let level = (self.level_frames >= LEVEL_FRAMES).then(|| (self.level_sq / f64::from(self.level_frames)).sqrt() as f32);
        self.level_sq = 0.0;
        self.level_frames = 0;
        level
    }

    /// Level one frame in place. Returns whether it was silent.
    pub fn apply(&mut self, pcm: &mut [i16; FRAME]) -> bool {
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        let silent = peak < SILENT_PEAK;
        if !silent {
            let sum_sq: f64 = pcm.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
            let rms = (sum_sq / FRAME as f64).sqrt() as f32;
            if rms > 0.0 {
                let (rms_alpha, scale_alpha) =
                    if self.voiced < FAST_FRAMES { (FAST_RMS_ALPHA, FAST_SCALE_ALPHA) } else { (RMS_ALPHA, SCALE_ALPHA) };
                self.voiced += 1;
                self.rms_ema = (1.0 - rms_alpha) * self.rms_ema + rms_alpha * rms;
                let target = (TARGET_RMS / self.rms_ema.max(1.0)).clamp(MIN_SCALE, MAX_SCALE);
                self.scale = (1.0 - scale_alpha) * self.scale + scale_alpha * target;
                if rms > LEVEL_GATE {
                    self.level_sq += f64::from(rms) * f64::from(rms);
                    self.level_frames += 1;
                }
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

    fn tone(amp: f32) -> [i16; FRAME] {
        std::array::from_fn(|i| (amp * (i as f32 * 0.3).sin()) as i16)
    }

    fn rms(p: &[i16; FRAME]) -> f64 {
        (p.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / FRAME as f64).sqrt()
    }

    #[test]
    fn a_transmission_starts_at_its_radios_level_and_measures_its_own() {
        // A radio heard at RMS 250 (-42 dBFS) starts at x10: its first frame is at the target.
        let mut agc = PcmAgc::default();
        agc.start(Some(250.0));
        let mut first = tone(353.6);
        agc.apply(&mut first);
        assert!((2200.0..2800.0).contains(&rms(&first)), "{}", rms(&first));
        // The ceiling is x16 (+24 dB).
        agc.start(Some(20.0));
        assert_eq!(agc.scale, 16.0);
        // Its level: the speech frames' RMS before the gain, then counted afresh.
        agc.start(None);
        for _ in 0..20 {
            let mut p = tone(1414.2);
            agc.apply(&mut p);
        }
        let mut quiet = tone(50.0);
        agc.apply(&mut quiet);
        let level = agc.take_level().unwrap();
        assert!((990.0..1010.0).contains(&level), "{level}");
        assert_eq!(agc.take_level(), None, "too little speech since");
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

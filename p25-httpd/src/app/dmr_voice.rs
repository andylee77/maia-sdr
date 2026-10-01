//! Change 075: DMR voice: AMBE+2 frames of the followed call -> PCM -> the
//! shared audio path (live audio, recordings).
//!
//! A thread runs `jmbe::ambe::AmbeDecoder` (a fresh one per call), scales to
//! 16-bit like the IMBE path, applies the same post-vocoder AGC and soft
//! limiter as `app::vocoder_task`, and hands 20 ms `AudioChunk`s on lane One
//! to a pacer of its own. (`PcmAgc` repeats the P25 thread's inline AGC; the
//! refactor after DMR should make both use it.)

use std::sync::mpsc::{Receiver, SyncSender};

use crate::audio::AudioChunk;
use crate::hardware::traffic_lane::Lane;
use crate::jmbe::ambe::AmbeDecoder;

/// One voice burst of the followed call.
#[derive(Debug, Clone)]
pub struct DmrVoiceBatch {
    pub frames: [[u8; 9]; 3],
    pub talkgroup: u32,
    pub source: Option<u32>,
    /// The lifecycle's call (0 = not known yet).
    pub call_id: u64,
    pub captured_at_ms: u64,
}

/// Bursts queued for the vocoder (~4 s).
pub const VOICE_QUEUE: usize = 64;

pub fn voice_channel() -> (SyncSender<DmrVoiceBatch>, Receiver<DmrVoiceBatch>) {
    std::sync::mpsc::sync_channel(VOICE_QUEUE)
}

/// The P25 vocoder thread's post-vocoder AGC and soft-knee limiter
/// (`app::vocoder_task`, same constants): voiced frames steer the gain
/// toward a target RMS; peaks above the knee saturate smoothly.
pub struct PcmAgc {
    rms_ema: f32,
    scale: f32,
}

const AGC_TARGET_RMS: f32 = 2500.0;
const AGC_RMS_ALPHA: f32 = 0.025;
const AGC_SCALE_ALPHA: f32 = 0.08;
const AGC_MIN_SCALE: f32 = 0.25;
const AGC_MAX_SCALE: f32 = 8.0;
const AGC_KNEE: f32 = 31000.0;
const AGC_CEIL: f32 = 32700.0;
/// Frames quieter than this are silence: they don't steer the gain.
const SILENT_PEAK: u16 = 16;

impl Default for PcmAgc {
    fn default() -> Self {
        PcmAgc { rms_ema: AGC_TARGET_RMS, scale: 1.0 }
    }
}

impl PcmAgc {
    /// Back to unity (a new call or speaker).
    pub fn reset(&mut self) {
        *self = PcmAgc::default();
    }

    pub fn apply(&mut self, pcm: &mut [i16; 160]) {
        let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
        if peak >= SILENT_PEAK {
            let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
            let rms = (sum_sq / pcm.len() as f64).sqrt() as f32;
            if rms > 0.0 {
                self.rms_ema = (1.0 - AGC_RMS_ALPHA) * self.rms_ema + AGC_RMS_ALPHA * rms;
                let target = (AGC_TARGET_RMS / self.rms_ema.max(1.0)).clamp(AGC_MIN_SCALE, AGC_MAX_SCALE);
                self.scale = (1.0 - AGC_SCALE_ALPHA) * self.scale + AGC_SCALE_ALPHA * target;
            }
        }
        for s in pcm.iter_mut() {
            let v = (*s as f32) * self.scale;
            let abs_v = v.abs();
            let limited = if abs_v <= AGC_KNEE {
                v
            } else {
                let span = AGC_CEIL - AGC_KNEE;
                v.signum() * (AGC_KNEE + span * ((abs_v - AGC_KNEE) / span).tanh())
            };
            *s = limited as i16;
        }
    }
}

/// AMBE+2 float PCM to 16-bit, as `vocoder::decode_frame` does for IMBE.
pub fn to_i16(pcm: &[f32; 160]) -> [i16; 160] {
    let mut out = [0i16; 160];
    for (o, f) in out.iter_mut().zip(pcm.iter()) {
        *o = (f * 32767.0).round().clamp(-32768.0, 32767.0) as i16;
    }
    out
}

/// Decodes one call's bursts; a different call starts a fresh decoder.
#[derive(Default)]
pub struct DmrVoiceDecoder {
    decoder: Option<AmbeDecoder>,
    call: Option<(u64, u32, Option<u32>)>,
    agc: PcmAgc,
    pub frames: u64,
    pub frame_errors: u64,
}

impl DmrVoiceDecoder {
    /// Three 20 ms chunks of the burst.
    pub fn decode(&mut self, batch: &DmrVoiceBatch) -> Vec<AudioChunk> {
        let key = (batch.call_id, batch.talkgroup, batch.source);
        if self.call.map(|c| (c.0, c.1)) != Some((key.0, key.1)) {
            // A new call: no state carried over.
            self.decoder = Some(AmbeDecoder::new());
            self.agc.reset();
        } else if self.call.map(|c| c.2) != Some(key.2) {
            // Same call, another speaker: their level differs.
            self.agc.reset();
        }
        self.call = Some(key);
        let decoder = self.decoder.get_or_insert_with(AmbeDecoder::new);
        let mut out = Vec::with_capacity(3);
        for frame in &batch.frames {
            let pcm = decoder.decode(frame);
            self.frames += 1;
            if decoder.last_frame().is_some_and(|f| f.error_count > 0) {
                self.frame_errors += 1;
            }
            let mut pcm = to_i16(&pcm);
            self.agc.apply(&mut pcm);
            out.push(AudioChunk {
                pcm,
                talkgroup: batch.talkgroup,
                source: batch.source.unwrap_or(0),
                call_id: batch.call_id,
                captured_at_ms: batch.captured_at_ms,
                airtime: false,
                lane: Lane::One,
            });
        }
        out
    }
}

/// The DMR vocoder thread: bursts in, paced audio out.
pub fn spawn_dmr_vocoder(
    rx: Receiver<DmrVoiceBatch>,
    pacer: tokio::sync::mpsc::Sender<AudioChunk>,
    rt: std::sync::Arc<crate::app::dmr_task::DmrRuntime>,
) {
    use std::sync::atomic::Ordering;
    let spawned = std::thread::Builder::new().name("dmr-vocoder".into()).spawn(move || {
        let mut voice = DmrVoiceDecoder::default();
        while let Ok(batch) = rx.recv() {
            for chunk in voice.decode(&batch) {
                if pacer.blocking_send(chunk).is_err() {
                    return;
                }
            }
            rt.vocoder_frames.store(voice.frames, Ordering::Relaxed);
            rt.vocoder_frame_errors.store(voice.frame_errors, Ordering::Relaxed);
        }
    });
    if let Err(e) = spawned {
        tracing::error!("dmr vocoder thread not started: {e}");
    }
}

#[cfg(test)]
#[path = "dmr_voice_tests.rs"]
mod tests;

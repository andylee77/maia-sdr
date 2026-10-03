//! Live audio: per lane, a decode thread (codec, alert tones, then the AGC, which starts each
//! transmission from its radio's recent level, kept for both lanes) and a pacer that releases one
//! 20 ms chunk per 20 ms of wall clock, into one broadcast for the listeners (`/ws/audio`) and
//! the recorder.
//!
//! The vocoders run ~30 times faster than real time, so a decoded LDU would otherwise leave as a
//! burst. After a silence the pacer restarts from the first chunk's arrival rather than
//! catching up, so silence stays silence.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc};

use super::agc::PcmAgc;
use super::alert::{AlertDetector, ToneAlert};
use super::levels::LevelBook;
use super::codec::{Ambe2, Imbe, VoiceCodec, SAMPLES_PER_FRAME};
use crate::hardware::p25core::Lane;
use crate::protocol::events::VoiceFrames;
use crate::services::config::aliases::Side;

/// Voice batches queued for a lane's decoder (~4 s of P25).
const DECODE_QUEUE: usize = 24;
/// Decoded chunks queued for a lane's pacer (5 s).
const PACER_QUEUE: usize = 256;
/// Chunks the broadcast holds for a slow listener.
const BROADCAST: usize = 256;
pub const FRAME_PACE: Duration = Duration::from_millis(20);
/// No voice on a lane for this long: the transmission heard is over.
const PAUSE: Duration = Duration::from_millis(600);

/// Clear voice of a followed call, for its lane's decoder.
#[derive(Debug, Clone)]
pub struct VoiceBatch {
    pub lane: Lane,
    pub call: u64,
    pub tg: u32,
    pub source: Option<u32>,
    /// From its alias, as the follower routes the talkgroup.
    pub speaker: Side,
    pub frames: VoiceFrames,
}

/// 20 ms of decoded, levelled audio.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub lane: Lane,
    pub call: u64,
    pub tg: u32,
    pub source: Option<u32>,
    pub speaker: Side,
    pub pcm: [i16; SAMPLES_PER_FRAME],
    /// The vocoder found the frame damaged (it repeated or muted).
    pub error: bool,
    pub silent: bool,
    /// While the call's frames make an alert tone: the alert so far (`audio::alert`).
    pub alert: Option<Arc<ToneAlert>>,
}

#[derive(Debug, Default)]
pub struct AudioCounters {
    pub frames: AtomicU64,
    pub errors: AtomicU64,
    pub silent: AtomicU64,
    /// Batches dropped because a decoder was behind.
    pub dropped: AtomicU64,
}

pub struct Audio {
    tx: broadcast::Sender<AudioChunk>,
    lanes: Vec<(Lane, SyncSender<VoiceBatch>)>,
    pub counters: Arc<AudioCounters>,
    /// Open `/ws/audio` connections.
    pub listeners: AtomicUsize,
}

impl Audio {
    /// Start each lane's decoder and pacer.
    pub fn start(lanes: &[Lane]) -> Arc<Audio> {
        let (tx, _) = broadcast::channel(BROADCAST);
        let counters = Arc::new(AudioCounters::default());
        let levels = Arc::new(Mutex::new(LevelBook::default()));
        let mut inputs = Vec::new();
        for &lane in lanes {
            let (in_tx, in_rx) = sync_channel(DECODE_QUEUE);
            let (pace_tx, pace_rx) = mpsc::channel(PACER_QUEUE);
            let c = counters.clone();
            let book = levels.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("voice{}", lane.number()))
                .spawn(move || decode(in_rx, pace_tx, c, &book));
            if let Err(e) = spawned {
                tracing::error!("{lane} voice decoder not started: {e}");
                continue;
            }
            tokio::spawn(pace(pace_rx, tx.clone()));
            inputs.push((lane, in_tx));
        }
        Arc::new(Audio { tx, lanes: inputs, counters, listeners: AtomicUsize::new(0) })
    }

    /// Hand a batch to its lane's decoder; false when it was dropped.
    pub fn voice(&self, batch: VoiceBatch) -> bool {
        let Some((_, tx)) = self.lanes.iter().find(|(l, _)| *l == batch.lane) else { return false };
        match tx.try_send(batch) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                self.counters.dropped.fetch_add(1, Ordering::Relaxed);
                false
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AudioChunk> {
        self.tx.subscribe()
    }

    /// Put a chunk on the broadcast as if a pacer had released it.
    #[cfg(test)]
    pub fn inject(&self, chunk: AudioChunk) {
        let _ = self.tx.send(chunk);
    }
}

fn lock(m: &Mutex<LevelBook>) -> std::sync::MutexGuard<'_, LevelBook> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The transmission heard is over: its radio's speech level, unless it carried an alert tone (a
/// console's tones are not its dispatcher's voice).
fn note_level(levels: &Mutex<LevelBook>, agc: &mut PcmAgc, radio: Option<u32>, alerted: bool) {
    if let (Some(level), Some(radio), false) = (agc.take_level(), radio, alerted) {
        lock(levels).note(radio, level, Instant::now());
    }
}

/// Where the AGC starts a transmission of `radio`.
fn start_level(levels: &Mutex<LevelBook>, radio: Option<u32>) -> Option<f32> {
    radio.and_then(|r| lock(levels).level(r, Instant::now()))
}

/// A lane's decode thread: a codec, an alert detector and an AGC per call (the AGC starts afresh
/// on a new talker too, each time from the radio's recent level). The detector hears the
/// vocoder's own levels.
fn decode(rx: Receiver<VoiceBatch>, tx: mpsc::Sender<AudioChunk>, counters: Arc<AudioCounters>, levels: &Mutex<LevelBook>) {
    let mut imbe = Imbe::default();
    let mut ambe = Ambe2::default();
    let mut alerts = AlertDetector::default();
    let mut agc = PcmAgc::default();
    let (mut call, mut source) = (0u64, None);
    // The transmission heard carried an alert tone.
    let mut alerted = false;
    loop {
        let b = match rx.recv_timeout(PAUSE) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => {
                note_level(levels, &mut agc, source, alerted);
                alerted = false;
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if b.call != call {
            note_level(levels, &mut agc, source, alerted);
            alerted = false;
            call = b.call;
            source = b.source;
            imbe.reset();
            ambe.reset();
            alerts.reset();
            agc.start(start_level(levels, source));
        } else if b.source.is_some() && source.is_some() && b.source != source {
            note_level(levels, &mut agc, source, alerted);
            alerted = false;
            agc.start(start_level(levels, b.source));
        }
        if b.source.is_some() {
            source = b.source;
        }
        let mut emit = |codec: &mut dyn VoiceCodec, frame: &[u8]| -> bool {
            let mut pcm = [0i16; SAMPLES_PER_FRAME];
            let q = codec.decode(frame, &mut pcm);
            counters.frames.fetch_add(1, Ordering::Relaxed);
            if q.error {
                counters.errors.fetch_add(1, Ordering::Relaxed);
            }
            let alert = alerts.frame(&pcm).map(Arc::new);
            alerted |= alert.is_some();
            let silent = agc.apply(&mut pcm);
            if silent {
                counters.silent.fetch_add(1, Ordering::Relaxed);
            }
            let chunk = AudioChunk { lane: b.lane, call: b.call, tg: b.tg, source: b.source, speaker: b.speaker, pcm, error: q.error, silent, alert };
            tx.blocking_send(chunk).is_ok()
        };
        let ok = match &b.frames {
            VoiceFrames::Imbe(frames) => frames.iter().all(|f| emit(&mut imbe, &f.bits)),
            VoiceFrames::Ambe2(frames) => frames.iter().all(|f| emit(&mut ambe, f)),
        };
        if !ok {
            return;
        }
    }
}

async fn pace(mut rx: mpsc::Receiver<AudioChunk>, tx: broadcast::Sender<AudioChunk>) {
    let mut next = tokio::time::Instant::now();
    while let Some(chunk) = rx.recv().await {
        let now = tokio::time::Instant::now();
        if next < now {
            next = now;
        }
        tokio::time::sleep_until(next).await;
        let _ = tx.send(chunk);
        next += FRAME_PACE;
    }
}

/// One binary `/ws/audio` frame: 160 little-endian samples, after `[lane index, 0, 0, 0]` when
/// `tagged`.
pub fn audio_frame(chunk: &AudioChunk, tagged: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + 2 * SAMPLES_PER_FRAME);
    if tagged {
        buf.extend_from_slice(&[chunk.lane.index() as u8, 0, 0, 0]);
    }
    for s in chunk.pcm {
        buf.extend_from_slice(&s.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::p25::voice_frame::ImbeFrameRaw;

    #[tokio::test]
    async fn a_batch_comes_out_paced_with_its_call() {
        let audio = Audio::start(&[Lane::One, Lane::Two]);
        let mut rx = audio.subscribe();
        let frames = VoiceFrames::Imbe([ImbeFrameRaw { bits: [0x55; 18] }; 9]);
        assert!(audio.voice(VoiceBatch { lane: Lane::Two, call: 7, tg: 300, source: Some(1014), speaker: Side::Right, frames }));
        let start = tokio::time::Instant::now();
        let mut got = Vec::new();
        for _ in 0..9 {
            got.push(tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap());
        }
        assert!(start.elapsed() >= FRAME_PACE * 8, "paced: {:?}", start.elapsed());
        assert!(got.iter().all(|c| c.lane == Lane::Two && c.call == 7 && c.tg == 300 && c.speaker == Side::Right));
        assert_eq!(audio.counters.frames.load(Ordering::Relaxed), 9);
        let frame = audio_frame(&got[0], true);
        assert_eq!((frame.len(), frame[0]), (324, 1));
        assert_eq!(audio_frame(&got[0], false).len(), 320);
    }

    /// The AGC as the lanes run it (each transmission started from its radio's recent level,
    /// alert transmissions not counted) over the raw calls in `AGC_WAV_DIR` (named as
    /// recordings, in time order): each sender's median speech level after it, and the share of
    /// calls within 6 dB of the target.
    #[test]
    #[ignore]
    fn levels_of_raw_calls_in_a_directory() {
        use crate::audio::alert::AlertDetector;
        let Ok(dir) = std::env::var("AGC_WAV_DIR") else { return };
        let mut calls: Vec<(u64, u32, std::path::PathBuf)> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                let p = crate::services::recordings::index::parse(&name)?;
                Some((p.started_unix_ms, p.source?, e.path()))
            })
            .collect();
        calls.sort();
        let t0 = calls.first().map_or(0, |c| c.0);
        let base = Instant::now();
        let book = Mutex::new(LevelBook::default());
        let db = |x: f64| 20.0 * (x / 32768.0).max(1e-9).log10();
        let mut by: std::collections::BTreeMap<String, Vec<f64>> = Default::default();
        for (start, source, path) in &calls {
            let at = base + Duration::from_millis(start - t0);
            let bytes = std::fs::read(path).unwrap();
            let pcm: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            let mut agc = PcmAgc::default();
            agc.start(lock(&book).level(*source, at));
            let mut alerts = AlertDetector::default();
            let (mut alerted, mut frames) = (false, Vec::new());
            for f in pcm.chunks_exact(SAMPLES_PER_FRAME) {
                let raw: [i16; SAMPLES_PER_FRAME] = f.try_into().unwrap();
                alerted |= alerts.frame(&raw).is_some();
                let mut out = raw;
                agc.apply(&mut out);
                let rms = |p: &[i16; SAMPLES_PER_FRAME]| (p.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>() / 160.0).sqrt();
                let peak = raw.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
                frames.push((rms(&raw), rms(&out), peak));
            }
            if let (Some(level), false) = (agc.take_level(), alerted) {
                lock(&book).note(*source, level, at);
            }
            if alerted {
                continue;
            }
            let speech: Vec<(f64, f64)> =
                frames.iter().filter(|(r, _, pk)| *pk >= 16 && db(*r) > -55.0).map(|(r, o, _)| (*r, *o)).collect();
            if speech.len() < 10 {
                continue;
            }
            let mut raws: Vec<f64> = speech.iter().map(|s| s.0).collect();
            raws.sort_by(f64::total_cmp);
            let p95 = raws[(raws.len() - 1) * 95 / 100];
            let loud: Vec<f64> = speech.iter().filter(|s| s.0 > p95 / 18.0).map(|s| s.1 * s.1).collect();
            let level = db((loud.iter().sum::<f64>() / loud.len() as f64).sqrt());
            let key = if *source < 10_000 { source.to_string() } else { "radios".into() };
            by.entry(key).or_default().push(level);
        }
        let median = |v: &[f64]| {
            let mut v = v.to_vec();
            v.sort_by(f64::total_cmp);
            v[v.len() / 2]
        };
        let all: Vec<f64> = by.values().flatten().copied().collect();
        for (k, v) in &by {
            println!("{k:>7} {:>4} calls, median {:.1} dBFS", v.len(), median(v));
        }
        let within = all.iter().filter(|l| (**l + 22.3).abs() <= 6.0).count();
        println!("{} calls, {:.0} % within 6 dB of the target", all.len(), 100.0 * within as f64 / all.len() as f64);
    }

    #[tokio::test]
    async fn a_dmr_burst_gives_three_chunks() {
        let audio = Audio::start(&[Lane::One]);
        let mut rx = audio.subscribe();
        let frames = VoiceFrames::Ambe2([[0u8; 9]; 3]);
        assert!(audio.voice(VoiceBatch { lane: Lane::One, call: 9, tg: 87_922, source: None, speaker: Side::Both, frames }));
        for _ in 0..3 {
            let c = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
            assert_eq!((c.call, c.tg), (9, 87_922));
        }
        assert_eq!(audio.counters.frames.load(Ordering::Relaxed), 3);
    }
}

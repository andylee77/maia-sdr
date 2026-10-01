//! Live audio: per lane, a decode thread (codec, then the AGC) and a pacer that releases one
//! 20 ms chunk per 20 ms of wall clock, into one broadcast for the listeners (`/ws/audio`) and
//! the recorder.
//!
//! The vocoders run ~30 times faster than real time, so a decoded LDU would otherwise leave as a
//! burst. After a silence the pacer restarts from the first chunk's arrival rather than
//! catching up, so silence stays silence.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{broadcast, mpsc};

use super::agc::PcmAgc;
use super::codec::{Ambe2, Imbe, VoiceCodec, SAMPLES_PER_FRAME};
use crate::hardware::p25core::Lane;
use crate::protocol::events::VoiceFrames;
use crate::services::config::profiles::Side;

/// Voice batches queued for a lane's decoder (~4 s of P25).
const DECODE_QUEUE: usize = 24;
/// Decoded chunks queued for a lane's pacer (5 s).
const PACER_QUEUE: usize = 256;
/// Chunks the broadcast holds for a slow listener.
const BROADCAST: usize = 256;
pub const FRAME_PACE: Duration = Duration::from_millis(20);

/// Clear voice of a followed call, for its lane's decoder.
#[derive(Debug, Clone)]
pub struct VoiceBatch {
    pub lane: Lane,
    pub call: u64,
    pub tg: u32,
    pub source: Option<u32>,
    /// From the profile, as the follower routes the talkgroup.
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
        let mut inputs = Vec::new();
        for &lane in lanes {
            let (in_tx, in_rx) = sync_channel(DECODE_QUEUE);
            let (pace_tx, pace_rx) = mpsc::channel(PACER_QUEUE);
            let c = counters.clone();
            let spawned = std::thread::Builder::new()
                .name(format!("voice{}", lane.number()))
                .spawn(move || decode(in_rx, pace_tx, c));
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
}

/// A lane's decode thread: a codec and an AGC per call (the AGC restarts on a new talker too).
fn decode(rx: Receiver<VoiceBatch>, tx: mpsc::Sender<AudioChunk>, counters: Arc<AudioCounters>) {
    let mut imbe = Imbe::default();
    let mut ambe = Ambe2::default();
    let mut agc = PcmAgc::default();
    let (mut call, mut source) = (0u64, None);
    while let Ok(b) = rx.recv() {
        if b.call != call {
            call = b.call;
            source = b.source;
            imbe.reset();
            ambe.reset();
            agc.reset();
        } else if b.source.is_some() && source.is_some() && b.source != source {
            agc.reset();
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
            let silent = agc.apply(&mut pcm);
            if silent {
                counters.silent.fetch_add(1, Ordering::Relaxed);
            }
            let chunk = AudioChunk { lane: b.lane, call: b.call, tg: b.tg, source: b.source, speaker: b.speaker, pcm, error: q.error, silent };
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

/// One binary `/ws/audio` frame: `[lane index, 0, 0, 0]` then 160 little-endian samples.
pub fn audio_frame(chunk: &AudioChunk) -> Vec<u8> {
    let mut buf = Vec::with_capacity(4 + 2 * SAMPLES_PER_FRAME);
    buf.extend_from_slice(&[chunk.lane.index() as u8, 0, 0, 0]);
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
        let frame = audio_frame(&got[0]);
        assert_eq!((frame.len(), frame[0]), (324, 1));
    }
}

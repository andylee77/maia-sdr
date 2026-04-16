//! Call recording + playback.
//!
//! Subscribes to the `audio_tx` broadcast channel and writes per-call
//! WAV files to `/tmp/p25_recordings/`. The recorder boundaries are
//! driven by the `talkgroup` field on each `AudioChunk`:
//!
//! - First non-zero TG chunk after a gap → start new recording
//! - TG changes to a different non-zero TG → finalise + start new
//! - TG goes to 0 (idle) → finalise after a short grace window so
//!   back-to-back PTT bursts on the same TG don't fragment
//!
//! The ring buffer is capped at `MAX_RECORDINGS` entries; evicting
//! an entry also deletes its WAV file. WAV format is 8 kHz 16-bit
//! mono (matches the vocoder output directly; no resampling).
//!
//! Storage goes to tmpfs (`/tmp`) so the SD card isn't wear-cycled.
//! At 8 kHz 16-bit mono, 5 min of audio is 4.8 MB, well within the
//! Zynq-7020's 512 MB RAM budget.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::audio::AudioChunk;

/// Max number of recordings kept in the ring. Oldest evicted when
/// the ring fills. 40 entries at ~30 s each ≈ 20 minutes of recent
/// audio history in tmpfs.
pub const MAX_RECORDINGS: usize = 40;

/// Storage directory. Created if missing.
pub const STORAGE_DIR: &str = "/tmp/p25_recordings";

/// How long to wait after the last chunk before finalising the
/// current recording. A TDU naturally drives talkgroup → 0, but
/// back-to-back PTT bursts on the same TG can produce brief gaps
/// we don't want to shatter a call over.
const FINALIZE_GRACE: Duration = Duration::from_millis(1500);

/// Minimum duration before a recording is worth keeping. Guards
/// against accidental 1-frame "calls" from phantom TDU_LC bursts.
const MIN_KEEPABLE_MS: u64 = 500;

/// Metadata for a completed recording, returned by /api/recordings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingEntry {
    /// Opaque identifier — monotonic per-process counter. Used in
    /// the URL path for /api/recordings/{id}.wav.
    pub id: u64,
    /// Talkgroup the recording belongs to.
    pub talkgroup: u16,
    /// Unix epoch milliseconds at which the recording started.
    /// Reads as wall-clock time if NTP synced, else kernel boot
    /// clock (post-NTP-on-boot landing, this should be real).
    pub started_unix_ms: u64,
    /// Duration in milliseconds.
    pub duration_ms: u64,
    /// Absolute filesystem path. Not exposed in the JSON but used
    /// by the download handler.
    #[serde(skip)]
    pub path: PathBuf,
    /// WAV file size in bytes.
    pub size_bytes: u64,
}

/// Shared ring buffer of completed recordings. Newest at the back.
pub type RecordingStore = Arc<Mutex<VecDeque<RecordingEntry>>>;

pub fn new_store() -> RecordingStore {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_RECORDINGS)))
}

/// In-progress recording buffer. Not shared — lives inside the
/// recorder task.
struct ActiveCall {
    talkgroup: u16,
    started_at: Instant,
    started_unix_ms: u64,
    pcm: Vec<i16>,
    last_chunk_at: Instant,
}

impl ActiveCall {
    fn new(talkgroup: u16) -> Self {
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            talkgroup,
            started_at: Instant::now(),
            started_unix_ms,
            pcm: Vec::with_capacity(8_000 * 10), // pre-size for 10 s
            last_chunk_at: Instant::now(),
        }
    }

    fn append(&mut self, chunk: &AudioChunk) {
        self.pcm.extend_from_slice(&chunk.pcm);
        self.last_chunk_at = Instant::now();
    }

    fn duration_ms(&self) -> u64 {
        // 8 kHz, so one sample is 0.125 ms.
        (self.pcm.len() as u64) * 1000 / 8_000
    }
}

/// Write a PCM-16 mono 8 kHz WAV with proper headers (not the
/// streaming 0xFFFFFFFF placeholder used by the live /api/audio
/// endpoint — finalised recordings know their size).
fn write_wav(path: &Path, pcm: &[i16]) -> std::io::Result<u64> {
    let sample_rate: u32 = 8_000;
    let channels: u16 = 1;
    let bits_per_sample: u16 = 16;
    let byte_rate: u32 =
        sample_rate * (bits_per_sample as u32 / 8) * channels as u32;
    let block_align: u16 = channels * (bits_per_sample / 8);
    let data_bytes: u32 = (pcm.len() * 2) as u32;
    let riff_size: u32 = data_bytes + 36;

    let mut f = std::fs::File::create(path)?;
    f.write_all(b"RIFF")?;
    f.write_all(&riff_size.to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&channels.to_le_bytes())?;
    f.write_all(&sample_rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&block_align.to_le_bytes())?;
    f.write_all(&bits_per_sample.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;

    // Little-endian i16 payload.
    let mut bytes = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    f.write_all(&bytes)?;
    Ok(44 + data_bytes as u64)
}

/// Finalise an ActiveCall into a RecordingEntry (writes WAV, adds
/// to store, evicts oldest if needed). No-op if the call is too
/// short to be interesting.
async fn finalize(store: &RecordingStore, call: ActiveCall, id: u64) {
    let duration_ms = call.duration_ms();
    if duration_ms < MIN_KEEPABLE_MS {
        tracing::debug!(
            "skipping too-short recording TG={} duration={}ms",
            call.talkgroup, duration_ms
        );
        return;
    }
    let filename = format!(
        "rec_{}_{}_tg{}.wav",
        call.started_unix_ms, id, call.talkgroup
    );
    let path = Path::new(STORAGE_DIR).join(filename);
    let size = match write_wav(&path, &call.pcm) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("recorder: WAV write failed: {e}");
            return;
        }
    };
    let entry = RecordingEntry {
        id,
        talkgroup: call.talkgroup,
        started_unix_ms: call.started_unix_ms,
        duration_ms,
        path,
        size_bytes: size,
    };
    let mut ring = store.lock().await;
    ring.push_back(entry);
    while ring.len() > MAX_RECORDINGS {
        if let Some(old) = ring.pop_front() {
            let _ = std::fs::remove_file(&old.path);
        }
    }
}

/// Recorder background task. Runs for the lifetime of the process.
/// Subscribes to the audio broadcast; one task per subscription is
/// standard for tokio broadcast (slow subscribers don't block the
/// vocoder because the vocoder owns the tx side and lagging
/// receivers just get Lagged errors, which we log and ignore).
pub async fn recorder_task(
    mut audio_rx: tokio::sync::broadcast::Receiver<AudioChunk>,
    store: RecordingStore,
) {
    // Ensure storage dir exists. If this fails, keep running but
    // log; finalize() will also fail and the recording is lost.
    if let Err(e) = std::fs::create_dir_all(STORAGE_DIR) {
        tracing::warn!("recorder: cannot create {STORAGE_DIR}: {e}");
    } else {
        // On boot, clear stale recordings from a previous p25-httpd
        // process — they have ids we don't know about, which makes
        // the /api/recordings ring inconsistent. tmpfs already
        // clears on reboot; this only matters if p25-httpd restarts.
        if let Ok(entries) = std::fs::read_dir(STORAGE_DIR) {
            for e in entries.flatten() {
                if e.path().extension().and_then(|s| s.to_str())
                    == Some("wav")
                {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    let mut active: Option<ActiveCall> = None;
    let mut next_id: u64 = 1;
    let mut tick = tokio::time::interval(Duration::from_millis(500));

    loop {
        tokio::select! {
            recv = audio_rx.recv() => {
                match recv {
                    Ok(chunk) => {
                        // Chunk semantics: talkgroup == 0 is "no
                        // call active" (vocoder emits this between
                        // calls). Non-zero is active voice.
                        if chunk.talkgroup == 0 {
                            // The grace-window tick below will
                            // finalise the active call if this
                            // persists. Don't append zero-TG chunks.
                            continue;
                        }
                        match active.as_mut() {
                            None => {
                                let mut c = ActiveCall::new(chunk.talkgroup);
                                c.append(&chunk);
                                active = Some(c);
                            }
                            Some(c) if c.talkgroup == chunk.talkgroup => {
                                c.append(&chunk);
                            }
                            Some(_) => {
                                // TG switched without going idle in
                                // between. Finalise the old call
                                // and start a new one with this
                                // chunk.
                                if let Some(old) = active.take() {
                                    let id = next_id;
                                    next_id += 1;
                                    finalize(&store, old, id).await;
                                }
                                let mut c = ActiveCall::new(chunk.talkgroup);
                                c.append(&chunk);
                                active = Some(c);
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            "recorder: {n} audio chunks lagged; current \
                             call may have a gap"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Sender gone. Flush and exit.
                        if let Some(old) = active.take() {
                            let id = next_id;
                            finalize(&store, old, id).await;
                        }
                        return;
                    }
                }
            }
            _ = tick.tick() => {
                // Grace-window finaliser. If there's an active call
                // and the last chunk was more than FINALIZE_GRACE
                // ago, close it out. Handles the normal end-of-call
                // path where the vocoder stops emitting chunks
                // without ever sending a TG=0 boundary.
                if let Some(c) = active.as_ref() {
                    if c.last_chunk_at.elapsed() >= FINALIZE_GRACE {
                        if let Some(old) = active.take() {
                            let id = next_id;
                            next_id += 1;
                            finalize(&store, old, id).await;
                        }
                    }
                }
            }
        }
    }
}

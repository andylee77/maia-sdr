//! Call recording + playback.
//!
//! Subscribes to `audio_tx` + `call_boundary_tx` and writes per-call
//! WAV files to `/tmp/p25_recordings/`. Boundaries come from two
//! signals:
//!
//! 1. **Audio chunks** (talkgroup field):
//!    - First non-zero TG chunk after a gap → start new recording
//!    - TG changes to a different non-zero TG → finalise + start new
//!    - TG goes to 0 (idle) → finalise after `FINALIZE_GRACE`
//!
//! 2. **Call boundary events** (2026-04-19):
//!    - `HduStart` → finalise the in-progress recording (if any) and
//!      leave `active = None`. The next PCM chunk begins a fresh
//!      `ActiveCall`. This is what lets the recorder split a
//!      dispatcher ↔ unit conversation (same TG, multiple speakers)
//!      into per-PTT files, matching SDRTrunk.
//!    - `TdulcComplete { source }` → when `source` is `Some(id)`,
//!      stamps `active.source` so the final filename includes the
//!      speaker's radio ID (`TO_<TG>_FROM_<source>.wav`). Does NOT
//!      finalise — we wait for HDU or the grace window.
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

use crate::audio::{AudioChunk, CallBoundary, CallBoundaryKind};

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
    /// 2026-04-19: speaker radio ID (`FM:<n>` in SDRTrunk parlance,
    /// `BY:<n>` on the terminating Motorola TDULC) when the TDULC
    /// LCW parser was able to recover it; `None` otherwise (including
    /// on non-Motorola sites which don't emit the
    /// `TALK_COMPLETE` vendor LC).
    pub source: Option<u32>,
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
    /// Basename of `path` — `rec_<ms>_<id>_tg<tg>[_from<src>].wav`.
    /// Exposed in the JSON so the dashboard can render the on-disk
    /// name next to each row; debugging "why isn't the source
    /// stamped?" used to require SSHing into /tmp to check.
    pub filename: String,
}

/// Shared ring buffer of completed recordings. Newest at the back.
pub type RecordingStore = Arc<Mutex<VecDeque<RecordingEntry>>>;

pub fn new_store() -> RecordingStore {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_RECORDINGS)))
}

/// 2026-04-19 diagnostics: counters for CallBoundary events the
/// recorder actually received. Surfaced via /api/traffic so we can
/// see whether the Motorola `TdulcComplete { source }` events fired
/// by the software framer are arriving at the recorder, and whether
/// there was an ActiveCall to stamp them onto.
#[derive(Default)]
pub struct RecorderDiag {
    pub boundaries_hdu: std::sync::atomic::AtomicU64,
    pub boundaries_tdulc_with_source: std::sync::atomic::AtomicU64,
    pub boundaries_tdulc_without_source:
        std::sync::atomic::AtomicU64,
    /// TdulcComplete events where `active` was None at arrival —
    /// these source stamps were lost. If > 0 while
    /// `tdulc_parse_motorola` > 0, the parser is emitting events
    /// but they arrive after the last ActiveCall has been
    /// finalised by the grace window.
    pub source_stamps_lost_no_active: std::sync::atomic::AtomicU64,
    /// TdulcComplete events where `active.source` was successfully
    /// set. Should equal the number of recordings whose filename
    /// contains `_from<n>`.
    pub source_stamps_applied: std::sync::atomic::AtomicU64,
    /// `broadcast::Receiver::recv` lagged events — messages the
    /// recorder missed because it fell behind the broadcaster.
    pub boundary_lag_events: std::sync::atomic::AtomicU64,
}

pub type RecorderDiagArc = Arc<RecorderDiag>;

pub fn new_diag() -> RecorderDiagArc {
    Arc::new(RecorderDiag::default())
}

/// In-progress recording buffer. Not shared — lives inside the
/// recorder task.
struct ActiveCall {
    talkgroup: u16,
    /// 2026-04-19: speaker radio ID, populated from a `CallBoundary`
    /// TDULC event when the Motorola `TALK_COMPLETE` BY: field is
    /// recoverable. `None` means "unknown source" — recorder just
    /// omits the `_from<n>` suffix in that case.
    source: Option<u32>,
    #[allow(dead_code)]
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
            source: None,
            started_at: Instant::now(),
            started_unix_ms,
            pcm: Vec::with_capacity(8_000 * 10), // pre-size for 10 s
            last_chunk_at: Instant::now(),
        }
    }

    fn append(&mut self, chunk: &AudioChunk) {
        self.pcm.extend_from_slice(&chunk.pcm);
        self.last_chunk_at = Instant::now();
        // 2026-04-19: if the audio chunk carries a known source (set
        // by the vocoder from `ImbeForwarder.current_source` — which
        // the grant follower wrote from `GRP_VCH_GRANT.FM`), stamp
        // it as soon as audio starts flowing. Keeps the recorder
        // aligned with SDRTrunk's source-attribution priority
        // (control-channel grant first, traffic LC second, TDULC
        // end code third). A later LC/TDULC boundary event can
        // still update the source — whichever value is most recent
        // wins for the eventual `_fromN.wav` filename.
        if chunk.source != 0 {
            self.source = Some(chunk.source);
        }
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
async fn finalize(
    store: &RecordingStore,
    call: ActiveCall,
    id: u64,
    event_log: Option<&Arc<crate::event_log::EventLog>>,
) {
    let duration_ms = call.duration_ms();
    if duration_ms < MIN_KEEPABLE_MS {
        tracing::debug!(
            "skipping too-short recording TG={} duration={}ms",
            call.talkgroup, duration_ms
        );
        if let Some(l) = event_log {
            l.push(
                crate::event_log::LogCategory::Recorder,
                "call_discard".to_string(),
                serde_json::json!({
                    "recording_id": id,
                    "reason":       "too_short",
                    "duration_ms":  duration_ms,
                    "min_keepable_ms": MIN_KEEPABLE_MS,
                    "tg":           call.talkgroup,
                    "source":       call.source,
                }),
            );
        }
        return;
    }
    // 2026-04-19: include the speaker radio ID in the filename when
    // known, matching SDRTrunk's `TO_<TG>_FROM_<source>.mp3` layout.
    // When the TDULC LC parser couldn't recover source (or the site
    // isn't Motorola-infrastructure), omit the `_from<n>` suffix so
    // the old `rec_<ms>_<id>_tg<n>.wav` shape is still emitted.
    let filename = match call.source {
        Some(s) => format!(
            "rec_{}_{}_tg{}_from{}.wav",
            call.started_unix_ms, id, call.talkgroup, s,
        ),
        None => format!(
            "rec_{}_{}_tg{}.wav",
            call.started_unix_ms, id, call.talkgroup,
        ),
    };
    let path = Path::new(STORAGE_DIR).join(&filename);
    let size = match write_wav(&path, &call.pcm) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("recorder: WAV write failed: {e}");
            if let Some(l) = event_log {
                l.push(
                    crate::event_log::LogCategory::Recorder,
                    "call_discard".to_string(),
                    serde_json::json!({
                        "recording_id": id,
                        "reason":       "wav_write_failed",
                        "error":        format!("{}", e),
                        "tg":           call.talkgroup,
                        "source":       call.source,
                    }),
                );
            }
            return;
        }
    };
    if let Some(l) = event_log {
        l.push(
            crate::event_log::LogCategory::Recorder,
            "call_saved".to_string(),
            serde_json::json!({
                "recording_id": id,
                "tg":           call.talkgroup,
                "source":       call.source,
                "filename":     &filename,
                "duration_ms":  duration_ms,
                "size_bytes":   size,
                "pcm_samples":  call.pcm.len(),
            }),
        );
    }
    let entry = RecordingEntry {
        id,
        talkgroup: call.talkgroup,
        source: call.source,
        started_unix_ms: call.started_unix_ms,
        duration_ms,
        path,
        size_bytes: size,
        filename,
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
/// Subscribes to the audio broadcast AND the call-boundary broadcast
/// so HDU-triggered splits can happen the instant a new speaker
/// starts, independent of vocoder latency.
pub async fn recorder_task(
    mut audio_rx: tokio::sync::broadcast::Receiver<AudioChunk>,
    mut boundary_rx: tokio::sync::broadcast::Receiver<CallBoundary>,
    store: RecordingStore,
    diag: RecorderDiagArc,
    event_log: Option<Arc<crate::event_log::EventLog>>,
) {
    // Structured-event helper. Every recorder decision (open, finalise,
    // source stamp, TG-guard skip, etc.) emits one of these so the
    // per-recording timeline can be reconstructed after the fact.
    // SDRTrunk's `decoded_messages.log` equivalent.
    let log_ev = |msg: &str, fields: serde_json::Value| {
        if let Some(ref l) = event_log {
            l.push(
                crate::event_log::LogCategory::Recorder,
                msg.to_string(),
                fields,
            );
        }
    };
    use std::sync::atomic::Ordering;
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
                                log_ev("call_open", serde_json::json!({
                                    "recording_id":  next_id,
                                    "tg":            chunk.talkgroup,
                                    "chunk_source":  chunk.source,
                                    "reason":        "first_chunk",
                                }));
                                active = Some(c);
                            }
                            Some(c) if c.talkgroup == chunk.talkgroup => {
                                // Log per-chunk source change so a
                                // recording's event tail shows every
                                // source stamp that happened mid-call.
                                if chunk.source != 0
                                    && c.source != Some(chunk.source)
                                {
                                    log_ev("source_stamp_chunk", serde_json::json!({
                                        "recording_id": next_id,
                                        "tg":           c.talkgroup,
                                        "old_source":   c.source,
                                        "new_source":   chunk.source,
                                        "via":          "audio_chunk",
                                    }));
                                }
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
                                    log_ev("call_finalise", serde_json::json!({
                                        "recording_id": id,
                                        "reason":       "tg_change",
                                        "old_tg":       old.talkgroup,
                                        "new_tg":       chunk.talkgroup,
                                        "duration_ms":  old.duration_ms(),
                                        "source":       old.source,
                                    }));
                                    finalize(&store, old, id, event_log.as_ref()).await;
                                }
                                let mut c = ActiveCall::new(chunk.talkgroup);
                                c.append(&chunk);
                                log_ev("call_open", serde_json::json!({
                                    "recording_id":  next_id,
                                    "tg":            chunk.talkgroup,
                                    "chunk_source":  chunk.source,
                                    "reason":        "tg_change",
                                }));
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
                            finalize(&store, old, id, event_log.as_ref()).await;
                        }
                        return;
                    }
                }
            }
            // 2026-04-19 HDU-driven call split.
            recv = boundary_rx.recv() => {
                match recv {
                    Ok(boundary) => match boundary.kind {
                        CallBoundaryKind::HduStart => {
                            diag.boundaries_hdu.fetch_add(1, Ordering::Relaxed);
                            log_ev("boundary_recv", serde_json::json!({
                                "kind":          "hdu_start",
                                "nac":           format!("0x{:03X}", boundary.nac),
                                "tg":            boundary.talkgroup,
                                "active_rec_id": active.as_ref().map(|_| next_id),
                                "active_tg":     active.as_ref().map(|c| c.talkgroup),
                                "active_src":    active.as_ref().and_then(|c| c.source),
                            }));
                            // Fresh PTT on the traffic channel.
                            // Finalise the in-progress recording --
                            // the next PCM chunk will open a new
                            // ActiveCall with the (possibly-same) TG.
                            if let Some(old) = active.take() {
                                let id = next_id;
                                next_id += 1;
                                log_ev("call_finalise", serde_json::json!({
                                    "recording_id": id,
                                    "reason":       "hdu_start",
                                    "tg":           old.talkgroup,
                                    "source":       old.source,
                                    "duration_ms":  old.duration_ms(),
                                }));
                                finalize(&store, old, id, event_log.as_ref()).await;
                            }
                        }
                        CallBoundaryKind::SpeakerEnd { source } => {
                            log_ev("boundary_recv", serde_json::json!({
                                "kind":          "speaker_end",
                                "nac":           format!("0x{:03X}", boundary.nac),
                                "tg":            boundary.talkgroup,
                                "lcw_source":    source,
                                "active_rec_id": active.as_ref().map(|_| next_id),
                                "active_tg":     active.as_ref().map(|c| c.talkgroup),
                                "active_src":    active.as_ref().and_then(|c| c.source),
                            }));
                            // Protocol-level end-of-speaker (Motorola
                            // TALK_COMPLETE) or end-of-call (standard
                            // CALL_TERMINATION) from TDULC. Only fires
                            // from the FEC-strong TDULC path; LDU1 LC
                            // routing was reverted as too noisy.
                            //
                            // Guard: if the active call's TG doesn't
                            // match the boundary's TG, this SpeakerEnd
                            // is a late teardown-tail signal from the
                            // previous call that arrived after the new
                            // call started (busy-site race). Don't
                            // finalise in that case.
                            let tg_matches = match (
                                active.as_ref(), boundary.talkgroup,
                            ) {
                                (Some(c), Some(btg)) => c.talkgroup == btg,
                                (Some(_), None) => true,
                                (None, _) => false,
                            };
                            if !tg_matches {
                                log_ev("boundary_skip", serde_json::json!({
                                    "kind":      "speaker_end",
                                    "reason":    "tg_mismatch_guard",
                                    "event_tg":  boundary.talkgroup,
                                    "active_tg": active.as_ref().map(|c| c.talkgroup),
                                }));
                                continue;
                            }
                            if let Some(c) = active.as_mut() {
                                if source.is_some() {
                                    let old_src = c.source;
                                    c.source = source;
                                    diag.source_stamps_applied
                                        .fetch_add(1, Ordering::Relaxed);
                                    log_ev("source_stamp_boundary", serde_json::json!({
                                        "recording_id": next_id,
                                        "tg":           c.talkgroup,
                                        "old_source":   old_src,
                                        "new_source":   source,
                                        "via":          "speaker_end",
                                    }));
                                }
                            }
                            if let Some(old) = active.take() {
                                let id = next_id;
                                next_id += 1;
                                log_ev("call_finalise", serde_json::json!({
                                    "recording_id": id,
                                    "reason":       "speaker_end",
                                    "tg":           old.talkgroup,
                                    "source":       old.source,
                                    "duration_ms":  old.duration_ms(),
                                }));
                                finalize(&store, old, id, event_log.as_ref()).await;
                            }
                        }
                        CallBoundaryKind::TdulcComplete { source } => {
                            log_ev("boundary_recv", serde_json::json!({
                                "kind":          "tdulc_complete",
                                "nac":           format!("0x{:03X}", boundary.nac),
                                "tg":            boundary.talkgroup,
                                "lcw_source":    source,
                                "active_rec_id": active.as_ref().map(|_| next_id),
                                "active_tg":     active.as_ref().map(|c| c.talkgroup),
                                "active_src":    active.as_ref().and_then(|c| c.source),
                            }));
                            if source.is_some() {
                                diag.boundaries_tdulc_with_source
                                    .fetch_add(1, Ordering::Relaxed);
                            } else {
                                diag.boundaries_tdulc_without_source
                                    .fetch_add(1, Ordering::Relaxed);
                            }
                            // Mid-call source stamp only. Experimental
                            // source-change split was reverted because
                            // LDU1 LC FEC is too weak to distinguish a
                            // real speaker change from a bit-corrupt
                            // source field, which produced excess
                            // splits on a single-speaker call.
                            if let Some(c) = active.as_mut() {
                                if source.is_some() {
                                    let old_src = c.source;
                                    c.source = source;
                                    diag.source_stamps_applied
                                        .fetch_add(1, Ordering::Relaxed);
                                    log_ev("source_stamp_boundary", serde_json::json!({
                                        "recording_id": next_id,
                                        "tg":           c.talkgroup,
                                        "old_source":   old_src,
                                        "new_source":   source,
                                        "via":          "tdulc_complete",
                                    }));
                                }
                            } else if source.is_some() {
                                diag.source_stamps_lost_no_active
                                    .fetch_add(1, Ordering::Relaxed);
                                log_ev("source_stamp_lost", serde_json::json!({
                                    "source": source,
                                    "reason": "no_active_recording",
                                }));
                            }
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        diag.boundary_lag_events
                            .fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "recorder: {n} call-boundary events lagged; \
                             a PTT split may have been missed (grace \
                             window will still finalise the call)"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Boundary channel closed but audio may still
                        // flow; keep running in grace-window-only mode.
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
                            log_ev("call_finalise", serde_json::json!({
                                "recording_id":    id,
                                "reason":          "grace_window",
                                "tg":              old.talkgroup,
                                "source":          old.source,
                                "duration_ms":     old.duration_ms(),
                                "silence_ms":      FINALIZE_GRACE.as_millis() as u64,
                            }));
                            finalize(&store, old, id, event_log.as_ref()).await;
                        }
                    }
                }
            }
        }
    }
}

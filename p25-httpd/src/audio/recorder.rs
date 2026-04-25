//! Call recording + playback.
//!
//! Phase 2b (2026-04-25): the recorder subscribes to `CallTrackerEvent`
//! from `app::grant_follower`, the single authority for call identity.
//! It no longer makes its own open/close decisions from raw boundary
//! events or audio chunks — those decisions live in one place now.
//!
//! Lifecycle:
//!
//! 1. **`CallTrackerEvent::CallOpen`** → defensively finalise any
//!    leftover active recording (means we missed a `CallClose`), then
//!    open a fresh `ActiveCall` carrying the tracker's monotonic
//!    `call_id` (which becomes `RecordingEntry.id`). PCM buffer
//!    starts empty; subsequent audio chunks append.
//!
//! 2. **`AudioChunk`** → append PCM to the active call's buffer if
//!    one exists and the TG matches (or chunk TG is 0 — mid-call
//!    flicker handling). No active call ⇒ drop chunk. The recorder
//!    no longer auto-opens on first chunk; CC is the source of truth.
//!
//! 3. **`CallTrackerEvent::SourceUpdate`** → fill-in-only stamp on
//!    the active recording's `source` (matches CC-source-authoritative
//!    discipline already enforced inside `call_tracker`).
//!
//! 4. **`CallTrackerEvent::CallClose`** → finalise the matching
//!    active recording. Discards if shorter than `MIN_KEEPABLE_MS`.
//!
//! 5. **Safety-net grace timer (`FINALIZE_GRACE` = 15 s)** — runs in
//!    the background. Closes any active recording whose `last_chunk_at`
//!    is older than this. Should never fire in practice; `call_tracker`
//!    emits `CallClose(Timeout)` after 10 s of inactivity. If the
//!    safety net fires, `boundary_lag_events` likely incremented and
//!    the broadcast topology needs review.
//!
//! What got deleted in Phase 2b:
//!
//! - `CcGrantArrival` cc_grant_split (Phase 1b band-aid).
//! - `HduStart` after-gap split.
//! - `SpeakerEnd { source }` source-stamp boundary handling.
//! - `TdulcComplete { source }` fill-in handling.
//! - First-chunk-opens-recording auto-open.
//! - chunk-source-change splits.
//! - chunk-TG-change splits.
//!
//! All of those decisions now live in `app::grant_follower`. See
//! `doc/diagnostics/2026-04-25/UNIFIED_CALL_LIFECYCLE.md` for the
//! design rationale.
//!
//! WAV format: 8 kHz 16-bit mono (matches vocoder output directly,
//! no resampling). Storage in `tmpfs` (`/tmp`) so the SD card isn't
//! wear-cycled. The ring is capped at `MAX_RECORDINGS`; evicting an
//! entry also deletes its WAV.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex;

use crate::audio::AudioChunk;
use crate::app::grant_follower::{
    CallTrackerEvent, CallTrackerEventKind, CloseReason,
};

/// Max number of recordings kept in the ring. Oldest evicted when
/// the ring fills. 40 entries at ~30 s each ≈ 20 minutes of recent
/// audio history in tmpfs.
pub const MAX_RECORDINGS: usize = 40;

/// Storage directory. Created if missing.
pub const STORAGE_DIR: &str = "/tmp/p25_recordings";

/// Safety-net grace window. Phase 2b (2026-04-25): the primary close
/// trigger is `CallTrackerEvent::CallClose` from `app::grant_follower`,
/// which emits at the end of `CALL_TIMEOUT_MS = 10 s` of inactivity.
/// This grace runs longer (15 s) so it only fires if the tracker
/// broadcast lagged, the spawn wiring broke, or call_tracker missed
/// the close. If you see `reason=grace_window_safety` in the recorder
/// event log, investigate `boundary_lag_events` first — it's a
/// recorder-vs-tracker desynchronisation indicator, not a normal
/// close.
const FINALIZE_GRACE: Duration = Duration::from_millis(15_000);

/// Minimum duration before a recording is worth keeping. Guards
/// against accidental 1-frame "calls" from phantom TDU_LC bursts.
const MIN_KEEPABLE_MS: u64 = 500;

/// Recorder task main-loop tick. The `FINALIZE_GRACE` timer check
/// runs every tick but only acts when
/// `last_chunk_at.elapsed() >= FINALIZE_GRACE`. Fast cadence means
/// grace fires within ~50 ms of the deadline.
const RECORDER_TICK_MS: u64 = 50;

/// Metadata for a completed recording, returned by /api/recordings.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingEntry {
    /// Opaque identifier — monotonic per-process counter. Used in
    /// the URL path for /api/recordings/{id}.wav.
    pub id: u64,
    /// Talkgroup the recording belongs to.
    pub talkgroup: u16,
    /// Speaker radio ID (`FM:<n>` in SDRTrunk parlance, `BY:<n>` on
    /// the terminating Motorola TDULC) when the TDULC LCW parser
    /// recovered it; `None` otherwise (including non-Motorola sites
    /// which don't emit the `TALK_COMPLETE` vendor LC).
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
    /// 2026-04-24: per-recording decode stats captured between call
    /// open and close. `None` on pre-2026-04-24 entries or if the
    /// recorder wasn't handed an `Arc<ImbeForwarder>` at spawn.
    /// Dashboard shows these alongside the row so operators can
    /// see per-call drop / silent / error rates without cross-
    /// referencing cumulative counters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imbe_extracted: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imbe_dropped: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hdu_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ldu1_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ldu2_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdu_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tdu_lc_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocoder_pcm: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocoder_errors: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vocoder_silent: Option<u64>,
    /// 2026-04-24: traffic-channel frequency this call landed on,
    /// captured at recording-open from the grant follower's
    /// `current_frequency_hz`. Lets the dashboard show per-recording
    /// freq + future per-channel quality aggregation
    /// (`/api/freq_health` follow-up).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub freq_hz: Option<u64>,
    /// 2026-04-24: P25 channel string (e.g. "0-1117") matching the
    /// grant. None on pre-2026-04-24 entries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// Shared ring buffer of completed recordings. Newest at the back.
pub type RecordingStore = Arc<Mutex<VecDeque<RecordingEntry>>>;

pub fn new_store() -> RecordingStore {
    Arc::new(Mutex::new(VecDeque::with_capacity(MAX_RECORDINGS)))
}

/// Diagnostics: counters for CallBoundary events the recorder
/// received. Surfaced via /api/traffic to verify whether Motorola
/// `TdulcComplete { source }` events from the software framer are
/// arriving and whether there was an ActiveCall to stamp them onto.
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
/// recorder task. Phase 2b: `call_id` is now populated from the
/// `app::grant_follower` `CallTrackerEvent::CallOpen` event and used as
/// the recording's identifier all the way through finalise. Same id
/// joins the recording row to its `grant_decode_stats` summary
/// (downstream of the same `CallTrackerEvent` stream).
struct ActiveCall {
    /// Tracker-assigned monotonic call_id. Carries through to
    /// `RecordingEntry.id` so dashboard joins line up across modules.
    call_id: u64,
    talkgroup: u16,
    /// Speaker radio ID. Phase 2b: populated by either (a) CallOpen
    /// carrying CC `GRP_VCH_GRANT.SRC`, (b) `SourceUpdate` from
    /// LDU1 LC FM: voted consensus or TDULC MOT_TC fill-in, or
    /// (c) defensive first-known-source stamp from `chunk.source`
    /// when the chain produced audio before any of (a)/(b) landed.
    source: Option<u32>,
    #[allow(dead_code)]
    started_at: Instant,
    started_unix_ms: u64,
    pcm: Vec<i16>,
    last_chunk_at: Instant,
    /// IMBE drop snapshot at `call_open`; the delta at finalise
    /// identifies calls that took audio loss from IMBE queue full.
    imbe_drops_at_open: u64,
    /// 2026-04-24: full counter baselines at call open so each
    /// RecordingEntry carries per-call deltas. Populated from the
    /// ImbeForwarder handed into the recorder task.
    stats_at_open: Option<StatsSnapshot>,
    /// 2026-04-24: traffic-channel freq + channel string. Phase 2b:
    /// now populated from `CallTrackerEvent::CallOpen` (which got it
    /// from the CC grant), removing the recorder's dependency on
    /// `forwarder.current_frequency_hz` for per-call attribution.
    freq_hz_at_open: Option<u64>,
    channel_at_open: Option<String>,
}

#[derive(Clone, Copy)]
struct StatsSnapshot {
    imbe_extracted: u64,
    imbe_dropped: u64,
    hdu: u64,
    ldu1: u64,
    ldu2: u64,
    tdu: u64,
    tdu_lc: u64,
    pcm: u64,
    errors: u64,
    silent: u64,
}

impl StatsSnapshot {
    fn from_forwarder(
        f: &crate::app::imbe_forwarder::ImbeForwarder,
    ) -> Self {
        use std::sync::atomic::Ordering;
        Self {
            imbe_extracted: f.imbe_frames_extracted.load(Ordering::Relaxed),
            imbe_dropped:   f.imbe_frames_dropped.load(Ordering::Relaxed),
            hdu:            f.hdu_count.load(Ordering::Relaxed),
            ldu1:           f.ldu1_count.load(Ordering::Relaxed),
            ldu2:           f.ldu2_count.load(Ordering::Relaxed),
            tdu:            f.tdu_count.load(Ordering::Relaxed),
            tdu_lc:         f.tdu_lc_count.load(Ordering::Relaxed),
            pcm:            f.vocoder_pcm_produced.load(Ordering::Relaxed),
            errors:         f.vocoder_errors.load(Ordering::Relaxed),
            silent:         f.vocoder_frames_silent_observed.load(Ordering::Relaxed),
        }
    }
}

impl ActiveCall {
    fn new(call_id: u64, talkgroup: u16, imbe_drops_at_open: u64) -> Self {
        let started_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        Self {
            call_id,
            talkgroup,
            source: None,
            started_at: Instant::now(),
            started_unix_ms,
            pcm: Vec::with_capacity(8_000 * 10), // pre-size for 10 s
            last_chunk_at: Instant::now(),
            imbe_drops_at_open,
            stats_at_open: None,
            freq_hz_at_open: None,
            channel_at_open: None,
        }
    }

    fn append(&mut self, chunk: &AudioChunk) {
        self.pcm.extend_from_slice(&chunk.pcm);
        self.last_chunk_at = Instant::now();
        // First-known-source stamp only. Source CHANGE (chunk.source
        // != 0 and c.source is Some(other)) is handled by the audio
        // match arm upstream — it finalises + reopens a new recording
        // rather than mutating this recording's source mid-stream.
        // See recorder_source_ids/ANALYSIS.md Bug B.
        if chunk.source != 0 && self.source.is_none() {
            self.source = Some(chunk.source);
        }
    }

    fn duration_ms(&self) -> u64 {
        // 8 kHz, so one sample is 0.125 ms.
        (self.pcm.len() as u64) * 1000 / 8_000
    }

    /// Wall-clock duration from call_open to now. Used alongside
    /// `duration_ms` to measure audio loss: `pcm/wall` ratio shows
    /// how much of the recording window actually contained audio.
    /// <50% = heavy IMBE drop / vocoder stall / encryption skip.
    /// ~100% = clean call. >100% = vocoder tail chunks arrived
    /// after the recording closed (count-based tail window
    /// captured them).
    fn wall_duration_ms(&self) -> u64 {
        self.started_at.elapsed().as_millis() as u64
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
    event_log: Option<&Arc<crate::services::event_log::EventLog>>,
    forwarder: Option<&Arc<crate::app::imbe_forwarder::ImbeForwarder>>,
) {
    let duration_ms = call.duration_ms();
    if duration_ms < MIN_KEEPABLE_MS {
        tracing::debug!(
            "skipping too-short recording TG={} duration={}ms",
            call.talkgroup, duration_ms
        );
        if let Some(l) = event_log {
            l.push(
                crate::services::event_log::LogCategory::Recorder,
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
    // Include the speaker radio ID in the filename when known,
    // matching SDRTrunk's `TO_<TG>_FROM_<source>.mp3` layout. When
    // TDULC LC parser couldn't recover source (or the site isn't
    // Motorola), omit `_from<n>` and fall back to
    // `rec_<ms>_<id>_tg<n>.wav`.
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
                    crate::services::event_log::LogCategory::Recorder,
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
            crate::services::event_log::LogCategory::Recorder,
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
    // Per-call decode-stat deltas. `stats_at_open` is the snapshot
    // of the forwarder's cumulative counters taken when the call was
    // opened; subtracting from `now` gives the per-call numbers that
    // /api/recordings exposes for the dashboard's per-row IMBE column.
    let (imbe_extracted, imbe_dropped, hdu_count, ldu1_count, ldu2_count,
         tdu_count, tdu_lc_count, vocoder_pcm, vocoder_errors, vocoder_silent) =
        match (call.stats_at_open, forwarder) {
            (Some(base), Some(f)) => {
                let now = StatsSnapshot::from_forwarder(f);
                (
                    Some(now.imbe_extracted.saturating_sub(base.imbe_extracted)),
                    Some(now.imbe_dropped.saturating_sub(base.imbe_dropped)),
                    Some(now.hdu.saturating_sub(base.hdu)),
                    Some(now.ldu1.saturating_sub(base.ldu1)),
                    Some(now.ldu2.saturating_sub(base.ldu2)),
                    Some(now.tdu.saturating_sub(base.tdu)),
                    Some(now.tdu_lc.saturating_sub(base.tdu_lc)),
                    Some(now.pcm.saturating_sub(base.pcm)),
                    Some(now.errors.saturating_sub(base.errors)),
                    Some(now.silent.saturating_sub(base.silent)),
                )
            }
            _ => (None, None, None, None, None, None, None, None, None, None),
        };
    let entry = RecordingEntry {
        id,
        talkgroup: call.talkgroup,
        source: call.source,
        started_unix_ms: call.started_unix_ms,
        duration_ms,
        path,
        size_bytes: size,
        filename,
        imbe_extracted,
        imbe_dropped,
        hdu_count,
        ldu1_count,
        ldu2_count,
        tdu_count,
        tdu_lc_count,
        vocoder_pcm,
        vocoder_errors,
        vocoder_silent,
        freq_hz: call.freq_hz_at_open,
        channel: call.channel_at_open.clone(),
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
/// Phase 2b (2026-04-25): subscribes to the audio broadcast AND
/// `CallTrackerEvent` from `app::grant_follower`. Lifecycle decisions
/// (when does a call open, close, or change speaker) are owned
/// exclusively by `call_tracker` — this task is now a thin WAV writer
/// driven by the events `call_tracker` broadcasts.
///
/// 2026-04-25 trailing-PCM fix: the vocoder lags CC events by
/// ~200-400 ms (JMBE decode + batch buffering). When CallClose
/// arrives for the active call, we drain audio_rx into the closing
/// WAV until `forwarder.frames_consumed >= expected_submit_count` +
/// a brief grace, OR `TRAILING_PCM_TIMEOUT_MS` fires. Without this,
/// the previous speaker's tail PCM bled into the next CallOpen's
/// recording (rec=8 1020 ms instead of 540 ms observed 2026-04-25).
///
/// Source-match gate (also 2026-04-25): when both chunk.source and
/// active.source are non-zero and known, mismatched chunks are
/// dropped — they belong to a different speaker (typically the
/// previous one whose WAV has just closed).
///
/// `imbe_drops` (atomic surfaced via /api/traffic) is used here to
/// record the drop-delta during each recording's lifetime so per-call
/// drop counts surface in the log entry and JSON.
pub async fn recorder_task(
    mut audio_rx: tokio::sync::broadcast::Receiver<AudioChunk>,
    mut tracker_rx: tokio::sync::broadcast::Receiver<CallTrackerEvent>,
    store: RecordingStore,
    diag: RecorderDiagArc,
    event_log: Option<Arc<crate::services::event_log::EventLog>>,
    imbe_drops: Arc<std::sync::atomic::AtomicU64>,
    // 2026-04-24: full forwarder handle for per-call counter snapshots
    // at open + delta at finalize. Optional — pre-2026-04-24 spawn
    // paths could pass None.
    forwarder: Option<Arc<crate::app::imbe_forwarder::ImbeForwarder>>,
) {
    // Structured-event helper. Every recorder decision (open, finalise,
    // source stamp, TG-guard skip, etc.) emits one of these so the
    // per-recording timeline can be reconstructed after the fact.
    // SDRTrunk's `decoded_messages.log` equivalent.
    let log_ev = |msg: &str, fields: serde_json::Value| {
        if let Some(ref l) = event_log {
            l.push(
                crate::services::event_log::LogCategory::Recorder,
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
    let mut tick = tokio::time::interval(Duration::from_millis(RECORDER_TICK_MS));

    // Helper: finalise + push the WAV through `finalize()`, logging
    // a structured `call_finalise` event with the per-call deltas.
    // Captured by closure context: store, event_log, forwarder,
    // imbe_drops, log_ev — all set up above.
    async fn finalise_call(
        old: ActiveCall,
        reason: &str,
        extra_fields: serde_json::Value,
        store: &RecordingStore,
        event_log: Option<&Arc<crate::services::event_log::EventLog>>,
        forwarder: Option<&Arc<crate::app::imbe_forwarder::ImbeForwarder>>,
        imbe_drops: &Arc<std::sync::atomic::AtomicU64>,
    ) {
        use std::sync::atomic::Ordering;
        let id = old.call_id;
        let wall_ms = old.wall_duration_ms();
        let pcm_ms = old.duration_ms();
        let fill_pct = if wall_ms > 0 { 100 * pcm_ms / wall_ms } else { 0 };
        let drops_in_call = imbe_drops
            .load(Ordering::Relaxed)
            .saturating_sub(old.imbe_drops_at_open);
        if let Some(l) = event_log {
            let mut fields = serde_json::json!({
                "recording_id":      id,
                "reason":            reason,
                "tg":                old.talkgroup,
                "source":            old.source,
                "duration_ms":       pcm_ms,
                "wall_duration_ms":  wall_ms,
                "pcm_fill_pct":      fill_pct,
                "imbe_drops_in_call": drops_in_call,
            });
            if let (serde_json::Value::Object(ref mut a),
                    serde_json::Value::Object(b)) = (&mut fields, extra_fields) {
                for (k, v) in b { a.insert(k, v); }
            }
            l.push(
                crate::services::event_log::LogCategory::Recorder,
                "call_finalise".to_string(),
                fields,
            );
        }
        finalize(store, old, id, event_log, forwarder).await;
    }

    loop {
        tokio::select! {
            recv = audio_rx.recv() => {
                match recv {
                    Ok(chunk) => {
                        // Phase 2b: audio chunks NEVER open a recording.
                        // call_tracker is the authority — `CallOpen`
                        // creates the ActiveCall, audio chunks just fill
                        // its PCM buffer.
                        let Some(c) = active.as_mut() else {
                            // No active call → drop. Came-up-mid-call
                            // edge case where CC grant was missed: by
                            // design we lose the audio rather than open
                            // a sourceless WAV. If this shows up
                            // repeatedly in /api/log, check that the
                            // follower is emitting CcGrantArrival for
                            // the parked freq.
                            continue;
                        };
                        // Phase 2h (2026-04-25): chunk.call_id is the
                        // GrantFollower call_id active when the IMBE
                        // batch was submitted. Route by it directly —
                        // a chunk for a different call (typical: a
                        // late chunk from the previous call that the
                        // vocoder decoded after CallClose) gets
                        // dropped here so it can't bleed into the new
                        // recording. The trailing-PCM drain in the
                        // CallClose path handles the OPPOSITE
                        // direction (late chunks for the call that
                        // just closed, while no new call is yet open).
                        //
                        // call_id == 0 is "no active call known at
                        // submit time" — typically follower idle or
                        // mid-retune. We append defensively rather
                        // than drop, matching the prior behaviour
                        // where TG=0 chunks were appended.
                        if chunk.call_id != 0
                            && chunk.call_id != c.call_id
                        {
                            tracing::trace!(
                                target: "p25_recorder",
                                "audio chunk call_id={} mismatch active \
                                 call_id={} (tg={}) — dropping (likely \
                                 trailing PCM from prior call)",
                                chunk.call_id, c.call_id, c.talkgroup,
                            );
                            continue;
                        }
                        // Defensive first-known-source stamp: if
                        // GrantFollower hasn't emitted SourceUpdate
                        // yet but the chunk carries a source, fill it
                        // in. Only applies when c.source is None.
                        if chunk.source != 0 && c.source.is_none() {
                            log_ev("source_stamp_chunk", serde_json::json!({
                                "recording_id": c.call_id,
                                "tg":           c.talkgroup,
                                "old_source":   c.source,
                                "new_source":   chunk.source,
                                "via":          "audio_chunk",
                            }));
                            diag.source_stamps_applied
                                .fetch_add(1, Ordering::Relaxed);
                        }
                        c.append(&chunk);
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
                            finalise_call(
                                old, "audio_channel_closed",
                                serde_json::json!({}),
                                &store, event_log.as_ref(),
                                forwarder.as_ref(), &imbe_drops,
                            ).await;
                        }
                        return;
                    }
                }
            }
            recv = tracker_rx.recv() => {
                match recv {
                    Ok(ev) => match ev.kind {
                        CallTrackerEventKind::CallOpen {
                            tg, source, freq_hz, channel, encrypted,
                            not_followed, ..
                        } => {
                            // 2026-04-25 Phase 2b followup: skip
                            // not_followed grants entirely. The
                            // follower won't tune the chain to
                            // these (encrypted, sticky, monitor-
                            // rejected, etc.), so no audio chunks
                            // will arrive — opening an ActiveCall
                            // would just block real audio chunks
                            // for OTHER tgs (TG-mismatch drop) for
                            // the full timeout window. grant_stats
                            // still tracks them via its own
                            // CallTrackerEvent subscription.
                            if not_followed.is_some() {
                                log_ev("call_open_skipped", serde_json::json!({
                                    "event_call_id": ev.call_id,
                                    "tg":            tg,
                                    "source":        source,
                                    "freq_hz":       freq_hz,
                                    "channel":       channel,
                                    "encrypted":     encrypted,
                                    "not_followed":  not_followed,
                                    "reason":        "not_followed_grant",
                                }));
                                continue;
                            }
                            // Defensive: if a previous CallClose was
                            // missed (broadcast lag, panic in tracker),
                            // we still have an `active` here. Finalise
                            // it before opening the new one so we don't
                            // lose data.
                            if let Some(old) = active.take() {
                                tracing::warn!(
                                    target: "p25_recorder",
                                    "CallOpen(call_id={}) arrived while \
                                     call_id={} still active — finalising \
                                     stale recording defensively",
                                    ev.call_id, old.call_id,
                                );
                                finalise_call(
                                    old, "call_open_without_close",
                                    serde_json::json!({
                                        "new_call_id": ev.call_id,
                                    }),
                                    &store, event_log.as_ref(),
                                    forwarder.as_ref(), &imbe_drops,
                                ).await;
                            }
                            let mut c = ActiveCall::new(
                                ev.call_id, tg,
                                imbe_drops.load(Ordering::Relaxed),
                            );
                            c.source = source;
                            c.freq_hz_at_open = freq_hz;
                            c.channel_at_open = channel.clone();
                            c.stats_at_open = forwarder.as_ref()
                                .map(|f| StatsSnapshot::from_forwarder(f));
                            log_ev("call_open", serde_json::json!({
                                "recording_id": ev.call_id,
                                "tg":           tg,
                                "source":       source,
                                "freq_hz":      freq_hz,
                                "channel":      channel,
                                "encrypted":    encrypted,
                                "via":          "call_tracker_open",
                            }));
                            active = Some(c);
                        }
                        CallTrackerEventKind::SourceUpdate {
                            new_source, via,
                        } => {
                            // call_tracker emits SourceUpdate only as
                            // fill-in (CC SRC is authoritative; LDU1
                            // LC vote / TDULC MOT_TC fill in for None
                            // calls only). Mirror that discipline here:
                            // never override a known source.
                            if let Some(c) = active.as_mut() {
                                if c.call_id != ev.call_id {
                                    log_ev("source_update_stale", serde_json::json!({
                                        "event_call_id":  ev.call_id,
                                        "active_call_id": c.call_id,
                                        "new_source":     new_source,
                                    }));
                                } else if c.source.is_none() {
                                    let old_src = c.source;
                                    c.source = Some(new_source);
                                    diag.source_stamps_applied
                                        .fetch_add(1, Ordering::Relaxed);
                                    log_ev("source_stamp_boundary", serde_json::json!({
                                        "recording_id": c.call_id,
                                        "tg":           c.talkgroup,
                                        "old_source":   old_src,
                                        "new_source":   new_source,
                                        "via":          serde_json::to_value(via).unwrap_or(serde_json::Value::Null),
                                    }));
                                } else if c.source != Some(new_source) {
                                    // Tracker shouldn't emit this case
                                    // (it pre-filters). If we see it,
                                    // log for diagnostics but don't
                                    // override.
                                    log_ev("source_stamp_rejected", serde_json::json!({
                                        "recording_id": c.call_id,
                                        "tg":           c.talkgroup,
                                        "cc_source":    c.source,
                                        "tracker_voted": new_source,
                                        "via":          serde_json::to_value(via).unwrap_or(serde_json::Value::Null),
                                        "reason":       "active_source_already_set",
                                    }));
                                }
                            } else {
                                diag.source_stamps_lost_no_active
                                    .fetch_add(1, Ordering::Relaxed);
                                log_ev("source_stamp_lost", serde_json::json!({
                                    "event_call_id": ev.call_id,
                                    "new_source":    new_source,
                                    "via":           serde_json::to_value(via).unwrap_or(serde_json::Value::Null),
                                    "reason":        "no_active_recording",
                                }));
                            }
                        }
                        CallTrackerEventKind::ActualSpeakerObserved { .. } => {
                            // Telemetry — not used by the recorder.
                            // grant_stats surfaces this via its own
                            // CallTrackerEvent subscription.
                        }
                        CallTrackerEventKind::CallClose {
                            reason, final_source,
                            expected_submit_count, ..
                        } => {
                            let matches = active.as_ref()
                                .map(|c| c.call_id == ev.call_id)
                                .unwrap_or(false);
                            if !matches {
                                log_ev("close_event_no_active", serde_json::json!({
                                    "event_call_id": ev.call_id,
                                    "active_call_id": active.as_ref().map(|c| c.call_id),
                                    "reason": serde_json::to_value(reason).unwrap_or(serde_json::Value::Null),
                                }));
                                continue;
                            }
                            if let Some(mut old) = active.take() {
                                if old.source.is_none() {
                                    if let Some(s) = final_source {
                                        old.source = Some(s);
                                    }
                                }
                                // 2026-04-25 trailing-PCM drain.
                                // Vocoder lags CC by ~200-400 ms; if we
                                // finalise immediately, those late
                                // chunks bleed into the next CallOpen's
                                // recording. Hold the audio_rx loop
                                // open against `old`, applying the same
                                // tg+source-match gates as the steady-
                                // state path, until vocoder catches up
                                // (frames_consumed >= expected) plus a
                                // brief grace OR TRAILING_PCM_TIMEOUT.
                                const TRAILING_PCM_TIMEOUT_MS: u64 = 500;
                                const POST_CONSUME_GRACE_MS: u64 = 50;
                                let drain_deadline = Instant::now()
                                    + Duration::from_millis(TRAILING_PCM_TIMEOUT_MS);
                                let mut consumed_caught_up_at: Option<Instant> = None;
                                let starting_pcm_len = old.pcm.len();
                                loop {
                                    let now = Instant::now();
                                    if now >= drain_deadline { break; }
                                    if let Some(f) = forwarder.as_ref() {
                                        let consumed = f.frames_consumed
                                            .load(Ordering::Relaxed);
                                        if consumed >= expected_submit_count {
                                            match consumed_caught_up_at {
                                                None => consumed_caught_up_at = Some(now),
                                                Some(t) if now.duration_since(t)
                                                    >= Duration::from_millis(POST_CONSUME_GRACE_MS) => break,
                                                _ => {}
                                            }
                                        }
                                    }
                                    let select_timeout = match consumed_caught_up_at {
                                        Some(t) => Duration::from_millis(POST_CONSUME_GRACE_MS)
                                            .saturating_sub(now.duration_since(t)),
                                        None => Duration::from_millis(20),
                                    };
                                    tokio::select! {
                                        biased;
                                        chunk_result = audio_rx.recv() => match chunk_result {
                                            Ok(chunk) => {
                                                // Phase 2h drain match: chunk
                                                // belongs to OLD if its call_id
                                                // matches old.call_id, or if
                                                // call_id==0 (forwarder idle
                                                // / no active call known —
                                                // most chunks during the
                                                // drain window fall here
                                                // because GrantFollower
                                                // already cleared
                                                // current_call_id on close).
                                                if chunk.call_id == 0
                                                    || chunk.call_id == old.call_id
                                                {
                                                    old.append(&chunk);
                                                }
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                                // Late chunks may have been lost.
                                                // Continue draining what's left.
                                            }
                                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                        },
                                        _ = tokio::time::sleep(select_timeout) => {}
                                    }
                                }
                                let drained_samples = old.pcm.len()
                                    .saturating_sub(starting_pcm_len);
                                let reason_str = match reason {
                                    CloseReason::SpeakerEnd => "speaker_end",
                                    CloseReason::Timeout => "timeout",
                                    CloseReason::SpeakerChange => "speaker_change",
                                    CloseReason::TgChange => "tg_change",
                                    CloseReason::NotFollowedExpire =>
                                        "not_followed_expire",
                                };
                                finalise_call(
                                    old, reason_str,
                                    serde_json::json!({
                                        "trailing_pcm_drained_samples":
                                            drained_samples,
                                    }),
                                    &store, event_log.as_ref(),
                                    forwarder.as_ref(), &imbe_drops,
                                ).await;
                            }
                        }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        diag.boundary_lag_events
                            .fetch_add(n, Ordering::Relaxed);
                        tracing::warn!(
                            "recorder: {n} CallTrackerEvent lagged; \
                             active recording may not finalise until \
                             the safety-net grace window fires"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Tracker channel closed — keep audio side alive
                        // so any in-flight call can finalise via grace
                        // window. New calls will not open until tracker
                        // is back (which means the process is exiting
                        // anyway).
                    }
                }
            }
            _ = tick.tick() => {
                // Safety-net grace finaliser. Should never fire — see
                // FINALIZE_GRACE doc. Investigate boundary_lag_events
                // if it does.
                let should_close = active.as_ref()
                    .map(|c| c.last_chunk_at.elapsed() >= FINALIZE_GRACE)
                    .unwrap_or(false);
                if should_close {
                    if let Some(old) = active.take() {
                        finalise_call(
                            old, "grace_window_safety",
                            serde_json::json!({
                                "silence_ms": FINALIZE_GRACE.as_millis() as u64,
                            }),
                            &store, event_log.as_ref(),
                            forwarder.as_ref(), &imbe_drops,
                        ).await;
                    }
                }
            }
        }
    }
}

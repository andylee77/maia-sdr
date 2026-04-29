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

/// 2026-04-26 session-lifecycle refactor: closing-state drain.
/// When CallClose arrives, the recorder sets `active.close_at_ms`
/// and keeps the recording open in the active slot. Audio chunks
/// with `captured_at_ms <= close_at_ms` continue to land via the
/// normal capture-time-window route. The periodic tick finalises
/// the recording `CLOSING_DRAIN_MS` after `close_at_ms`. 2 s is
/// generous vs observed worst-case vocoder + queue lag (~250-400
/// ms typical, multi-second only during chain settle on retune
/// — and captured_at_ms timestamping makes that case irrelevant
/// here).
const CLOSING_DRAIN_MS: u64 = 2_000;

/// Minimum duration before a recording is worth keeping. Originally
/// 500 ms to guard against phantom 1-frame "calls" from TDU_LC
/// bursts; 2026-04-25 lowered to 0 because real short PTTs ("yes",
/// "10-4", quick acks) routinely run under 500 ms and were being
/// silently discarded with `(too short)` in the dashboard. Phantom
/// no-audio calls are now caught via the `pcm.is_empty()` gate
/// below — the duration cap was a stand-in for that and the
/// stand-in was hiding real recordings.
const MIN_KEEPABLE_MS: u64 = 0;

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
    /// 2026-04-26 routing-loss attribution. One chunk = 1 IMBE frame
    /// = 160 PCM samples = 20 ms. `chunks_match` chunks landed via
    /// exact `chunk.call_id == self.call_id`; `chunks_zero_callid`
    /// chunks were defensively appended on `chunk.call_id == 0`
    /// (forwarder hadn't yet received `mirror_active`'s update);
    /// `chunks_drain` chunks were captured during the post-CallClose
    /// trailing-PCM drain. Sum = chunks actually appended. Compare
    /// against `imbe_extracted` (global-counter delta) to spot
    /// routing loss — large delta = frames escaping this recording.
    /// `first_chunk_after_open_ms` is the time from CallOpen to the
    /// first chunk appended HERE (not the global last_imbe atomic).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks_match: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks_zero_callid: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks_drain: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_chunk_after_open_ms: Option<u64>,
    /// 2026-04-26 session-lifecycle refactor: every distinct SRC
    /// observed during the session, in insertion order. Single-
    /// speaker calls have `[primary_src]` (or empty if no SRC
    /// landed). Multi-speaker bundled grants carry the full list.
    /// Dashboard renders comma-separated when len > 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources_observed: Vec<u32>,
    /// 2026-04-26 lag instrumentation: max value of
    /// `arrived_at_ms - chunk.captured_at_ms` over the recording.
    /// Tells us worst-case end-to-end pipeline delay (LDU dispatch
    /// → vocoder PCM → broadcast → recorder). Sizes the
    /// CLOSING_DRAIN_MS empirically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chunk_lag_ms: Option<u64>,
    /// Mean lag across all chunks of this recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_chunk_lag_ms: Option<u64>,
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
    /// 2026-04-26: chunks rejected because `chunk.call_id` was
    /// non-zero and didn't match the active recording's call_id.
    /// Each chunk = 1 IMBE frame = 20 ms of audio. Should stay near
    /// 0; non-zero indicates the lifecycle layer's `mirror_active`
    /// hasn't propagated `current_call_id` to the forwarder fast
    /// enough — chunks carry stale call_ids and miss their target.
    pub chunks_dropped_call_id_mismatch:
        std::sync::atomic::AtomicU64,
    /// 2026-04-26: chunks that arrived when `active` was None —
    /// either before the first CallOpen of the session OR during
    /// the gap between recording finalise and the next CallOpen.
    /// Late frames that arrive after the trailing-PCM drain window
    /// closes hit this path. Each = 20 ms of lost audio.
    pub chunks_dropped_no_active:
        std::sync::atomic::AtomicU64,
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
    /// 2026-04-26 routing-loss instrumentation. One chunk = one IMBE
    /// frame = 160 PCM samples = 20 ms of audio. Surfaced on the
    /// recording entry + close-event log so we can see the routing
    /// breakdown per call without parsing the trace log.
    ///
    /// `chunks_match`: chunk.call_id == self.call_id (the happy path
    ///   — forwarder had `current_call_id` correctly set when it
    ///   stamped the IMBE batch).
    /// `chunks_zero`: chunk.call_id == 0 (forwarder idle / mid-retune
    ///   / mirror_active hadn't propagated yet — defensively
    ///   appended so the audio isn't lost).
    /// `chunks_drain`: appended during the post-CallClose trailing-
    ///   PCM drain window (independent count regardless of whether
    ///   the chunk's call_id matched 0 or self).
    /// `first_chunk_at`: instant the first chunk was appended;
    ///   together with `started_at` gives "time from CallOpen to
    ///   first audio bit" — the actual acquire latency for THIS
    ///   recording (independent of the global last_imbe atomic).
    chunks_match: u64,
    chunks_zero: u64,
    chunks_drain: u64,
    first_chunk_at: Option<Instant>,
    /// 2026-04-26 session-lifecycle refactor: audio chunks route by
    /// `chunk.captured_at_ms ∈ [open_at_ms, close_at_ms or u64::MAX]`,
    /// not by `chunk.call_id`. open_at_ms is set on CallOpen.
    open_at_ms: u64,
    /// 2026-04-26 session-lifecycle refactor: set when CallClose
    /// arrives with `ended_unix_ms`. While close_at_ms is Some,
    /// the recording is in "closing" state — chunks with
    /// `captured_at_ms <= close_at_ms` still append (drain
    /// captures late vocoder output for THIS call), chunks with
    /// `captured_at_ms > close_at_ms` are dropped (they belong
    /// to the next session). The recording is finalised by the
    /// periodic tick `CLOSING_DRAIN_MS` after close_at_ms.
    close_at_ms: Option<u64>,
    /// 2026-04-26 session-lifecycle refactor: every distinct SRC
    /// observed during the session, in insertion order. Populated
    /// from CallOpen.source, CcRefresh updates (bundled GRANTs in
    /// multi-speaker sessions), LDU1 LC voted SRC, and TDULC MOT
    /// BY:. Recorder JSON shows this so the dashboard can display
    /// "speakers: 1012, 3402164" for multi-speaker grants.
    sources_observed: Vec<u32>,
    /// 2026-04-26 lag instrumentation: max value of
    /// `chunk_arrived_at_ms - chunk.captured_at_ms` over the
    /// recording. Tells us worst-case pipeline delay (vocoder +
    /// queue) for sizing the drain window.
    max_chunk_lag_ms: u64,
    total_chunk_lag_ms: u64,
    lag_count: u64,
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
            chunks_match: 0,
            chunks_zero: 0,
            chunks_drain: 0,
            first_chunk_at: None,
            open_at_ms: started_unix_ms,
            close_at_ms: None,
            sources_observed: Vec::new(),
            max_chunk_lag_ms: 0,
            total_chunk_lag_ms: 0,
            lag_count: 0,
        }
    }

    fn append(&mut self, chunk: &AudioChunk) {
        if self.first_chunk_at.is_none() {
            self.first_chunk_at = Some(Instant::now());
        }
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
        if chunk.source != 0 && !self.sources_observed.contains(&chunk.source) {
            self.sources_observed.push(chunk.source);
        }
        // 2026-04-26 lag instrumentation: stamp delta from
        // capture (LDU dispatch in framer) to recorder receipt.
        // Includes vocoder time, mpsc queueing, broadcast fan-out.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let lag = now_ms.saturating_sub(chunk.captured_at_ms);
        if lag > self.max_chunk_lag_ms {
            self.max_chunk_lag_ms = lag;
        }
        self.total_chunk_lag_ms = self.total_chunk_lag_ms.saturating_add(lag);
        self.lag_count = self.lag_count.saturating_add(1);
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
    // 2026-04-25: discard only if the call produced ZERO PCM
    // samples. Duration alone is no longer the gate — real short
    // PTTs (149 ms first IMBE, ~200 ms total) are valid recordings.
    // The MIN_KEEPABLE_MS guard against phantom 1-frame calls is now
    // covered structurally: phantom calls (no IMBE → no PCM) hit
    // this empty-PCM check; real short calls keep their WAV.
    if call.pcm.is_empty() || duration_ms < MIN_KEEPABLE_MS {
        let reason = if call.pcm.is_empty() {
            "no_pcm"
        } else {
            "too_short"
        };
        tracing::debug!(
            "skipping {} recording TG={} duration={}ms pcm_len={}",
            reason, call.talkgroup, duration_ms, call.pcm.len(),
        );
        if let Some(l) = event_log {
            l.push(
                crate::services::event_log::LogCategory::Recorder,
                "call_discard".to_string(),
                serde_json::json!({
                    "recording_id": id,
                    "reason":       reason,
                    "duration_ms":  duration_ms,
                    "pcm_samples":  call.pcm.len(),
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
    // 2026-04-26: routing-loss attribution per recording.
    // chunks_match + chunks_zero + chunks_drain = chunks actually
    // appended to this WAV. Each = 20 ms = 160 PCM samples.
    // imbe_extracted (above) is the GLOBAL counter delta during this
    // call's wall-time window — includes frames that were stamped
    // with a different call_id and routed elsewhere. If
    // imbe_extracted >> chunks_total, frames are escaping this
    // recording (likely to the next via call_id mismatch, or to
    // /dev/null via the no-active path).
    let chunks_match = Some(call.chunks_match);
    let chunks_zero_callid = Some(call.chunks_zero);
    let chunks_drain = Some(call.chunks_drain);
    let first_chunk_after_open_ms = call.first_chunk_at
        .map(|t| t.saturating_duration_since(call.started_at)
            .as_millis() as u64);
    let max_chunk_lag_ms = if call.lag_count > 0 { Some(call.max_chunk_lag_ms) } else { None };
    let mean_chunk_lag_ms = if call.lag_count > 0 {
        Some(call.total_chunk_lag_ms / call.lag_count)
    } else {
        None
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
        chunks_match,
        chunks_zero_callid,
        chunks_drain,
        first_chunk_after_open_ms,
        sources_observed: call.sources_observed.clone(),
        max_chunk_lag_ms,
        mean_chunk_lag_ms,
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
            // 2026-04-26: routing-breakdown fields. `chunks_*` are in
            // IMBE-frame units (1 chunk = 20 ms). `first_chunk_after_open_ms`
            // is the actual time from CallOpen to first appended chunk
            // — independent of the global `last_imbe_at_millis` atomic.
            let first_chunk_ms = old.first_chunk_at
                .map(|t| t.saturating_duration_since(old.started_at)
                    .as_millis() as u64);
            let chunks_total =
                old.chunks_match + old.chunks_zero + old.chunks_drain;
            let mut fields = serde_json::json!({
                "recording_id":      id,
                "reason":            reason,
                "tg":                old.talkgroup,
                "source":            old.source,
                "duration_ms":       pcm_ms,
                "wall_duration_ms":  wall_ms,
                "pcm_fill_pct":      fill_pct,
                "imbe_drops_in_call": drops_in_call,
                "chunks_total":            chunks_total,
                "chunks_match":            old.chunks_match,
                "chunks_zero_callid":      old.chunks_zero,
                "chunks_drain":            old.chunks_drain,
                "first_chunk_after_open_ms": first_chunk_ms,
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
                        // 2026-04-26 session-lifecycle refactor: route
                        // by `chunk.captured_at_ms` (stamped at LDU
                        // dispatch in the framer) vs the active
                        // recording's [open_at_ms, close_at_ms or now]
                        // window. Capture-time routing handles the
                        // dominant audio-loss paths the previous
                        // call_id-routing missed:
                        //   - Late chunks (vocoder lag) crossing a
                        //     CallClose: captured_at_ms <= close_at_ms
                        //     so they STILL land in the closing
                        //     recording, regardless of arrival time.
                        //   - Chunks for the next session arriving
                        //     before its CallOpen has propagated:
                        //     captured_at_ms > close_at_ms so they
                        //     are dropped (they belong to next call).
                        let Some(c) = active.as_mut() else {
                            diag.chunks_dropped_no_active
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        // Capture-time window check.
                        if chunk.captured_at_ms < c.open_at_ms {
                            // Captured before THIS recording opened —
                            // belongs to a prior session that already
                            // finalized. Drop.
                            diag.chunks_dropped_call_id_mismatch
                                .fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        if let Some(close_ms) = c.close_at_ms {
                            if chunk.captured_at_ms > close_ms {
                                // Captured after this session's
                                // terminator/timeout fired — belongs
                                // to the NEXT session (whose CallOpen
                                // just hasn't propagated to the
                                // recorder yet). Drop here; it'll
                                // come back as no_active until
                                // CallOpen lands.
                                diag.chunks_dropped_call_id_mismatch
                                    .fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                            // Within drain window: captured before
                            // close, arrived after close. Late chunk
                            // captured for THIS recording — append.
                            c.chunks_drain += 1;
                        } else {
                            // Steady state: session open, no close
                            // signaled yet.
                            c.chunks_match += 1;
                        }
                        // First-known-source stamp. Defensive: most
                        // sources arrive via CallOpen / SourceUpdate.
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
                            // 2026-04-26 session-lifecycle refactor: open_at_ms
                            // sourced from the event timestamp (= when the
                            // primary GRP_VCH_GRANT was processed). Capture-
                            // time routing uses this + close_at_ms (set on
                            // CallClose) to bracket which chunks belong here.
                            c.open_at_ms = ev.timestamp_unix_ms;
                            if let Some(s) = source {
                                c.sources_observed.push(s);
                            }
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
                            // 2026-04-26 session-lifecycle refactor:
                            // SourceUpdate now adds to `sources_observed`
                            // for multi-speaker bundling. Primary `source`
                            // field still uses fill-in-only semantics
                            // (CC GRANT.SRC wins, LDU1 LC / TDULC fill
                            // None-only). The dashboard renders the full
                            // sources_observed list when len > 1.
                            if let Some(c) = active.as_mut() {
                                if c.call_id != ev.call_id {
                                    log_ev("source_update_stale", serde_json::json!({
                                        "event_call_id":  ev.call_id,
                                        "active_call_id": c.call_id,
                                        "new_source":     new_source,
                                    }));
                                } else {
                                    if !c.sources_observed.contains(&new_source) {
                                        c.sources_observed.push(new_source);
                                    }
                                    if c.source.is_none() {
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
                                    }
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
                            reason, final_source, ended_unix_ms, ..
                        } => {
                            // 2026-04-26 session-lifecycle refactor:
                            // CallClose just stamps `close_at_ms` on
                            // the active recording. Audio chunks
                            // continue to land via the normal capture-
                            // time-window route (the audio_rx arm
                            // checks `chunk.captured_at_ms <=
                            // close_at_ms`). The periodic tick
                            // finalises the recording CLOSING_DRAIN_MS
                            // after `close_at_ms`. No inline drain
                            // loop needed — the main select! continues
                            // to service audio + tracker + tick events
                            // through the drain window.
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
                            if let Some(c) = active.as_mut() {
                                if c.source.is_none() {
                                    if let Some(s) = final_source {
                                        c.source = Some(s);
                                    }
                                }
                                if let Some(s) = final_source {
                                    if !c.sources_observed.contains(&s) {
                                        c.sources_observed.push(s);
                                    }
                                }
                                c.close_at_ms = Some(ended_unix_ms);
                                let reason_str = match reason {
                                    CloseReason::Timeout => "timeout",
                                    CloseReason::TgChange => "tg_change",
                                    CloseReason::StreamLag => "stream_lag",
                                };
                                log_ev("call_closing", serde_json::json!({
                                    "recording_id":   c.call_id,
                                    "tg":             c.talkgroup,
                                    "reason":         reason_str,
                                    "close_at_ms":    ended_unix_ms,
                                    "drain_ms":       CLOSING_DRAIN_MS,
                                    "pcm_so_far_samples": c.pcm.len(),
                                }));
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
                // 2026-04-26 session-lifecycle refactor: tick now has
                // two finalisers:
                //  (1) Closing-drain-elapsed: CallClose set
                //      `close_at_ms`; once CLOSING_DRAIN_MS has passed,
                //      finalise. This is the normal close path under
                //      the new model.
                //  (2) Safety-net grace: 15 s with no audio AND no
                //      close_at_ms (tracker desync — broadcast lag,
                //      panic). Should rarely fire; investigate
                //      `boundary_lag_events` if it does.
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let close_decision = active.as_ref().and_then(|c| {
                    if let Some(close_at) = c.close_at_ms {
                        if now_ms.saturating_sub(close_at) >= CLOSING_DRAIN_MS {
                            return Some("closing_drain_elapsed");
                        }
                        return None;
                    }
                    if c.last_chunk_at.elapsed() >= FINALIZE_GRACE {
                        return Some("grace_window_safety");
                    }
                    None
                });
                if let Some(reason) = close_decision {
                    if let Some(old) = active.take() {
                        finalise_call(
                            old, reason,
                            serde_json::json!({
                                "drain_ms":  CLOSING_DRAIN_MS,
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

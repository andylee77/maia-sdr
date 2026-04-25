//! Per-grant decode summary ring — Phase 2 (2026-04-25) rewrite.
//!
//! Subscribes to `CallTrackerEvent` from the unified `call_tracker`
//! authority instead of independently consuming `CallBoundary` events.
//! All call-identity decisions (open / dedup / speaker-change /
//! close-on-timeout) are made by `call_tracker`; this module just
//! collects per-call decode metrics and exposes them via
//! `/api/grant_decode_stats`.
//!
//! Lifecycle:
//!
//!   - `CallOpen` → snapshot `ImbeForwarder` counters, store baseline.
//!   - `SourceUpdate` → update the in-flight summary's source field.
//!   - `CallClose` → snapshot counters again, compute deltas against
//!     baseline, push `GrantDecodeSummary` into the ring.
//!
//! The shape of `GrantDecodeSummary` is preserved for API
//! compatibility — the dashboard's `/api/grant_decode_stats` parsing
//! is unchanged. Implementation detail: drop / silent / extracted
//! deltas use the global atomic counters with per-call baselining.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::collections::VecDeque;
use std::time::Instant;

use crate::app::call_tracker::{
    CallTrackerEvent, CallTrackerEventKind, CallTrackerEventTx, CloseReason,
};
use crate::app::imbe_forwarder::ImbeForwarder;

/// Ring cap. 50 most-recent completed grants. Bigger than the pre-
/// 2026-04-24 cap of 20 because the CC-centric model also enters
/// sticky-rejected, monitor-rejected, and encrypted grants.
const RING_CAP: usize = 50;

/// Shared ring of completed grant summaries.
pub type GrantStatsRing = Arc<Mutex<VecDeque<GrantDecodeSummary>>>;

/// Per-grant decode summary. Same shape as the pre-2026-04-25
/// version; only the lifecycle source has changed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GrantDecodeSummary {
    /// Monotonic call_id from `call_tracker`. Joins this summary to
    /// the corresponding recording (same id).
    pub call_id: u64,
    pub tg: u16,
    pub nac: u16,
    pub source: Option<u32>,
    /// 2026-04-25: LDU1 LC FM: voted speaker observation. CC
    /// `source` is the call OWNER (channel grant); this is the
    /// ACTUAL KEYING RADIO observed in the on-air voice frames.
    /// They normally agree; on this site they sometimes disagree.
    /// `None` when no LDU1 LC was decoded for this call (encrypted /
    /// not-followed / lost lock).
    pub actual_speaker: Option<u32>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: u64,
    pub duration_ms: u64,
    /// Time from CallOpen to the most recent IMBE frame. `None` when
    /// the call produced no IMBE.
    pub first_imbe_ms: Option<u64>,
    /// Wall-clock unix_ms of the first audio chunk / HDU observed.
    pub first_audio_at_unix_ms: Option<u64>,
    pub hdu_count: u64,
    pub ldu1_count: u64,
    pub ldu2_count: u64,
    pub tdu_count: u64,
    pub tdu_lc_count: u64,
    pub imbe_extracted: u64,
    pub imbe_dropped: u64,
    pub vocoder_pcm_samples: u64,
    pub vocoder_errors: u64,
    pub vocoder_silent: u64,
    pub vocoder_encrypted: u64,
    pub encrypted: bool,
    pub not_followed: Option<&'static str>,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    /// 2026-04-25 Phase 2: serialised close reason from
    /// `call_tracker::CloseReason`.
    pub close_reason: CloseReason,
}

struct ActiveSummary {
    call_id: u64,
    tg: u16,
    nac: u16,
    source: Option<u32>,
    actual_speaker: Option<u32>,
    encrypted: bool,
    not_followed: Option<&'static str>,
    started_unix_ms: u64,
    started_instant: Instant,
    last_imbe_ms_at_open: u64,
    base: Counters,
    freq_hz: Option<u64>,
    channel: Option<String>,
}

struct Counters {
    hdu: u64,
    ldu1: u64,
    ldu2: u64,
    tdu: u64,
    tdu_lc: u64,
    imbe_extracted: u64,
    imbe_dropped: u64,
    pcm: u64,
    errors: u64,
    silent: u64,
    encrypted_frames: u64,
}

impl Counters {
    fn snapshot(f: &ImbeForwarder) -> Self {
        Self {
            hdu:              f.hdu_count.load(Ordering::Relaxed),
            ldu1:             f.ldu1_count.load(Ordering::Relaxed),
            ldu2:             f.ldu2_count.load(Ordering::Relaxed),
            tdu:              f.tdu_count.load(Ordering::Relaxed),
            tdu_lc:           f.tdu_lc_count.load(Ordering::Relaxed),
            imbe_extracted:   f.imbe_frames_extracted.load(Ordering::Relaxed),
            imbe_dropped:     f.imbe_frames_dropped.load(Ordering::Relaxed),
            pcm:              f.vocoder_pcm_produced.load(Ordering::Relaxed),
            errors:           f.vocoder_errors.load(Ordering::Relaxed),
            silent:           f.vocoder_frames_silent_observed.load(Ordering::Relaxed),
            encrypted_frames: f.vocoder_frames_encrypted.load(Ordering::Relaxed),
        }
    }
}

/// Spawn the grant_stats subscriber task. Subscribes to the
/// `CallTrackerEvent` broadcast.
pub fn spawn_grant_stats_task(
    tracker_tx: CallTrackerEventTx,
    forwarder: Arc<ImbeForwarder>,
    ring: GrantStatsRing,
) {
    let mut rx = tracker_tx.subscribe();
    tokio::spawn(async move {
        let mut active: Option<ActiveSummary> = None;
        while let Ok(event) = rx.recv().await {
            handle_event(event, &mut active, &forwarder, &ring).await;
        }
    });
}

async fn handle_event(
    event: CallTrackerEvent,
    active: &mut Option<ActiveSummary>,
    forwarder: &Arc<ImbeForwarder>,
    ring: &GrantStatsRing,
) {
    match event.kind {
        CallTrackerEventKind::CallOpen {
            tg, nac, source, freq_hz, channel,
            encrypted, not_followed, ..
        } => {
            // Defensive: if there's somehow an active summary still
            // here (call_tracker should have closed it first),
            // synthesise a Timeout close so we don't leak.
            if let Some(prev) = active.take() {
                tracing::warn!(
                    target: "p25_grant_stats",
                    "stale active summary for call_id={} on new \
                     CallOpen call_id={} — pushing as Timeout",
                    prev.call_id, event.call_id,
                );
                let summary = finalise_summary(
                    &prev, prev.source, prev.actual_speaker,
                    CloseReason::Timeout, forwarder,
                );
                push(ring, summary);
            }
            let base = Counters::snapshot(forwarder);
            let last_imbe_ms_at_open = forwarder
                .last_imbe_at_millis.load(Ordering::Relaxed);
            *active = Some(ActiveSummary {
                call_id: event.call_id,
                tg,
                nac,
                source,
                actual_speaker: None,
                encrypted,
                not_followed,
                started_unix_ms: event.timestamp_unix_ms,
                started_instant: Instant::now(),
                last_imbe_ms_at_open,
                base,
                freq_hz,
                channel,
            });
        }

        CallTrackerEventKind::SourceUpdate { new_source, .. } => {
            if let Some(a) = active.as_mut() {
                if a.call_id == event.call_id {
                    a.source = Some(new_source);
                }
            }
        }

        CallTrackerEventKind::ActualSpeakerObserved { speaker, .. } => {
            if let Some(a) = active.as_mut() {
                if a.call_id == event.call_id {
                    a.actual_speaker = Some(speaker);
                }
            }
        }

        CallTrackerEventKind::CallClose {
            reason, final_source, final_actual_speaker,
            expected_submit_count, ..
        } => {
            let Some(prev) = active.take() else { return; };
            if prev.call_id != event.call_id {
                tracing::warn!(
                    target: "p25_grant_stats",
                    "CallClose call_id={} doesn't match active \
                     call_id={} — discarding",
                    event.call_id, prev.call_id,
                );
                return;
            }

            // Wait for the vocoder to drain in-flight batches before
            // snapshotting close-time counters. Up to 2 s; a stuck
            // vocoder doesn't block summary emission indefinitely.
            let drain_deadline = Instant::now()
                + std::time::Duration::from_millis(2000);
            loop {
                let consumed = forwarder
                    .frames_consumed.load(Ordering::Relaxed);
                if consumed >= expected_submit_count { break; }
                if Instant::now() >= drain_deadline { break; }
                tokio::time::sleep(
                    std::time::Duration::from_millis(50)).await;
            }

            let summary = finalise_summary(
                &prev, final_source, final_actual_speaker, reason, forwarder,
            );
            push_if_interesting(ring, summary);
        }
    }
}

fn finalise_summary(
    a: &ActiveSummary,
    final_source: Option<u32>,
    final_actual_speaker: Option<u32>,
    close_reason: CloseReason,
    forwarder: &ImbeForwarder,
) -> GrantDecodeSummary {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let now_counters = Counters::snapshot(forwarder);
    let dur_ms = a.started_instant.elapsed().as_millis() as u64;

    let last_imbe_ms_now = forwarder
        .last_imbe_at_millis.load(Ordering::Relaxed);
    let first_imbe_ms = if last_imbe_ms_now > a.last_imbe_ms_at_open
        && a.started_unix_ms > 0
    {
        Some(last_imbe_ms_now.saturating_sub(a.started_unix_ms))
    } else {
        None
    };

    GrantDecodeSummary {
        call_id: a.call_id,
        tg: a.tg,
        nac: a.nac,
        // Source priority: existing recorded source (CC + LDU1 LC
        // accumulated). final_source from SpeakerEnd path is used
        // as last-resort fill.
        source: a.source.or(final_source),
        // Actual speaker is the LDU1-LC-voted RID accumulated through
        // the call. final_actual_speaker preserves what call_tracker
        // had at close time as a backup if this struct's field
        // somehow lagged.
        actual_speaker: a.actual_speaker.or(final_actual_speaker),
        started_unix_ms: a.started_unix_ms,
        ended_unix_ms: now_ms,
        duration_ms: dur_ms,
        first_imbe_ms,
        first_audio_at_unix_ms: None, // populated by call_tracker via SourceUpdate / CallClose; reserved for next pass
        hdu_count:
            now_counters.hdu.saturating_sub(a.base.hdu),
        ldu1_count:
            now_counters.ldu1.saturating_sub(a.base.ldu1),
        ldu2_count:
            now_counters.ldu2.saturating_sub(a.base.ldu2),
        tdu_count:
            now_counters.tdu.saturating_sub(a.base.tdu),
        tdu_lc_count:
            now_counters.tdu_lc.saturating_sub(a.base.tdu_lc),
        imbe_extracted:
            now_counters.imbe_extracted.saturating_sub(a.base.imbe_extracted),
        imbe_dropped:
            now_counters.imbe_dropped.saturating_sub(a.base.imbe_dropped),
        vocoder_pcm_samples:
            now_counters.pcm.saturating_sub(a.base.pcm),
        vocoder_errors:
            now_counters.errors.saturating_sub(a.base.errors),
        vocoder_silent:
            now_counters.silent.saturating_sub(a.base.silent),
        vocoder_encrypted:
            now_counters.encrypted_frames.saturating_sub(a.base.encrypted_frames),
        encrypted: a.encrypted,
        not_followed: a.not_followed,
        freq_hz: a.freq_hz,
        channel: a.channel.clone(),
        close_reason,
    }
}

/// Drop heartbeat-ghost summaries — zero IMBE, no HDU, no rejection
/// reason. Belt-and-braces guard.
fn push_if_interesting(ring: &GrantStatsRing, summary: GrantDecodeSummary) {
    let interesting = summary.imbe_extracted > 0
        || summary.hdu_count > 0
        || summary.not_followed.is_some();
    if interesting {
        push(ring, summary);
    }
}

fn push(ring: &GrantStatsRing, summary: GrantDecodeSummary) {
    if let Ok(mut r) = ring.lock() {
        if r.len() >= RING_CAP { r.pop_front(); }
        r.push_back(summary);
    }
}

pub fn new_ring() -> GrantStatsRing {
    Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

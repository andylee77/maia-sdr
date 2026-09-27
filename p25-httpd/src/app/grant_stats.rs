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

use crate::app::grant_follower::{
    CallTrackerEvent, CallTrackerEventKind, CallTrackerEventTx, CloseReason,
};
use crate::app::imbe_forwarder::ImbeForwarder;

/// Ring cap. Most-recent completed grants. 2026-04-26 raised from
/// 50 to 200 because grants are heavily dominated by encrypted
/// not-followed entries (every TG-402 ENC GRANT TSBK enters here
/// with no audio), and the dashboard joins recordings to grants by
/// `call_id`. With 50, the grant ring rolls past every recording
/// older than ~10 minutes of activity, orphaning them. 200 covers
/// a busy hour comfortably and keeps recordings paired with their
/// grant metadata. Per-entry size ~250 B → 50 KB total worst case.
/// Change 056: public for `/api/pipeline` (which hard-coded 20).
pub const RING_CAP: usize = 200;

/// Shared ring of completed grant summaries.
pub type GrantStatsRing = Arc<Mutex<VecDeque<GrantDecodeSummary>>>;

#[allow(dead_code)]
fn is_zero_u64(v: &u64) -> bool { *v == 0 }

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
    /// 2026-04-30 framer-divergence diagnostic. Per-call delta of
    /// "decoder reached the per-DUID dispatch arm" — fires AFTER
    /// the decoder's NID hunt accepts a sync candidate, BCH'd, and
    /// classified the DUID, but BEFORE body extraction. Diff vs the
    /// matching `*_count` field = body-extraction failure count.
    /// HDL register samples (sync_trace) report what HDL saw; these
    /// fields report what the PS framer saw. Divergence localises
    /// dibit-stream alignment / sync-correlator differences.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub framer_arm_hdu: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub framer_arm_ldu1: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub framer_arm_ldu2: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub framer_arm_tdu: u64,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub framer_arm_tdu_lc: u64,
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
    /// 2026-04-26 session-lifecycle refactor: every distinct SRC
    /// observed during the bundled grant — primary GRANT.SRC,
    /// LDU1 LC voted SRC, TDULC MOT BY:. Insertion order. Single-
    /// speaker calls have one entry (or zero if no SRC landed).
    /// Multi-speaker bundled grants list every speaker.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources_observed: Vec<u32>,
    /// 2026-04-26 per-call AGC tracking. Q9.7 raw u16 sampled
    /// from `traffic_lsm_agc_debug.agc_gain_dbg` at CallClose.
    /// Equals last value written by the periodic AGC poller
    /// (250 ms cadence in main.rs). Display value: `raw / 128.0`.
    /// `None` when no audio landed (chain produced nothing —
    /// either encrypted, sticky-rejected, or chain settle failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agc_gain_q97_at_close: Option<u16>,
    /// 2026-04-30 air-time from CC `GRP_VCH_GRNT_UPD` beacons.
    /// last_upd_unix_ms - started_unix_ms = the speaker's actual
    /// on-air duration, independent of whether audio was
    /// extracted. delta vs `duration_ms` (lifecycle) and
    /// `(ldu1+ldu2)*180` (audio extracted) localises which stage
    /// missed time — recording-window vs body-extraction failure
    /// vs early-close. `None` when no UPDs landed (very short
    /// call or chain missed all UPD beacons).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub air_duration_ms: Option<u64>,
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
    framer_arm_hdu: u64,
    framer_arm_ldu1: u64,
    framer_arm_ldu2: u64,
    framer_arm_tdu: u64,
    framer_arm_tdu_lc: u64,
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
            hdu:               f.hdu_count.load(Ordering::Relaxed),
            ldu1:              f.ldu1_count.load(Ordering::Relaxed),
            ldu2:              f.ldu2_count.load(Ordering::Relaxed),
            tdu:               f.tdu_count.load(Ordering::Relaxed),
            tdu_lc:            f.tdu_lc_count.load(Ordering::Relaxed),
            framer_arm_hdu:    f.framer_arm_hdu.load(Ordering::Relaxed),
            framer_arm_ldu1:   f.framer_arm_ldu1.load(Ordering::Relaxed),
            framer_arm_ldu2:   f.framer_arm_ldu2.load(Ordering::Relaxed),
            framer_arm_tdu:    f.framer_arm_tdu.load(Ordering::Relaxed),
            framer_arm_tdu_lc: f.framer_arm_tdu_lc.load(Ordering::Relaxed),
            imbe_extracted:    f.imbe_frames_extracted.load(Ordering::Relaxed),
            imbe_dropped:      f.imbe_frames_dropped.load(Ordering::Relaxed),
            pcm:               f.vocoder_pcm_produced.load(Ordering::Relaxed),
            errors:            f.vocoder_errors.load(Ordering::Relaxed),
            silent:            f.vocoder_frames_silent_observed.load(Ordering::Relaxed),
            encrypted_frames:  f.vocoder_frames_encrypted.load(Ordering::Relaxed),
        }
    }
}

/// Spawn the grant_stats subscriber task. Subscribes to the
/// `CallTrackerEvent` broadcast.
///
/// 2026-04-29: takes two rings — `clear_ring` for followed
/// (audio-bearing) calls and `enc_ring` for encrypted /
/// not_followed grants. Routing happens at CallClose by
/// inspecting `summary.encrypted` and `summary.not_followed`.
/// Recordings only pair against `clear_ring`'s call_ids, so
/// heavy ENC activity can't crowd out clear entries.
pub fn spawn_grant_stats_task(
    tracker_tx: CallTrackerEventTx,
    forwarder: Arc<ImbeForwarder>,
    clear_ring: GrantStatsRing,
    enc_ring: GrantStatsRing,
) {
    let mut rx = tracker_tx.subscribe();
    tokio::spawn(async move {
        let mut active: Option<ActiveSummary> = None;
        while let Ok(event) = rx.recv().await {
            handle_event(event, &mut active, &forwarder, &clear_ring, &enc_ring).await;
        }
    });
}

async fn handle_event(
    event: CallTrackerEvent,
    active: &mut Option<ActiveSummary>,
    forwarder: &Arc<ImbeForwarder>,
    clear_ring: &GrantStatsRing,
    enc_ring: &GrantStatsRing,
) {
    match event.kind {
        CallTrackerEventKind::CallOpen {
            tg, nac, source, freq_hz, channel,
            encrypted, not_followed, ..
        } => {
            // 2026-04-30 fix: synthetic not_followed CallOpen+CallClose
            // pairs (cross-freq encrypted/sticky/monitor-rejected
            // grants emitted for Recent-Calls visibility) must NOT
            // displace a real active call. Without this gate, the
            // pre-existing stale-active path below would force-finalise
            // the real active as Timeout — losing its real CallClose
            // counters when the eventual TgChange close arrives. Push
            // the synthetic summary inline (zero-decode, instant close)
            // and ignore the matching CallClose (active.take() will be
            // None for it). Same-freq not_followed grants close the
            // active explicitly upstream in handle_boundary, so by the
            // time we get here `active` is already None for that path.
            if not_followed.is_some() && active.is_some() {
                let summary = synthetic_not_followed_summary(
                    event.call_id, tg, nac, source, freq_hz,
                    channel, encrypted, not_followed,
                    event.timestamp_unix_ms,
                );
                route_push(clear_ring, enc_ring, summary);
                return;
            }
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
                    CloseReason::Timeout, None, forwarder,
                );
                route_push(clear_ring, enc_ring, summary);
            }
            let base = Counters::snapshot(forwarder);
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
            expected_submit_count, first_audio_at_unix_ms,
            sources_observed, last_upd_at_unix_ms, ..
        } => {
            // 2026-04-30: peek before take. The CallClose half of a
            // synthetic not_followed pair (handled inline at CallOpen)
            // arrives here; if `active` holds the real call, taking it
            // unconditionally would lose the real call's eventual
            // finalise. Only consume `active` when call_ids match.
            match active.as_ref().map(|a| a.call_id) {
                Some(id) if id == event.call_id => {} // matches — proceed
                _ => return,
            }
            let prev = active.take().unwrap();

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

            let mut summary = finalise_summary(
                &prev, final_source, final_actual_speaker, reason,
                first_audio_at_unix_ms, forwarder,
            );
            summary.sources_observed = sources_observed;
            // 2026-04-30 air-time: last_upd - started = on-air ms,
            // independent of audio extraction success. None when
            // no UPD landed (very short call or chain missed all).
            summary.air_duration_ms = if last_upd_at_unix_ms > prev.started_unix_ms {
                Some(last_upd_at_unix_ms.saturating_sub(prev.started_unix_ms))
            } else {
                None
            };
            // 2026-04-26 per-freq AGC EMA cache. Update on every
            // clear call that produced audio; the next retune to
            // this freq will seed AGC from the cache.
            if summary.imbe_extracted > 0 {
                if let (Some(freq), Some(gain)) =
                    (summary.freq_hz, summary.agc_gain_q97_at_close)
                {
                    forwarder.update_agc_cache(freq, gain);
                }
            }
            route_push(clear_ring, enc_ring, summary);
        }
    }
}

fn finalise_summary(
    a: &ActiveSummary,
    final_source: Option<u32>,
    final_actual_speaker: Option<u32>,
    close_reason: CloseReason,
    first_audio_at_unix_ms: Option<u64>,
    forwarder: &ImbeForwarder,
) -> GrantDecodeSummary {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let now_counters = Counters::snapshot(forwarder);
    let dur_ms = a.started_instant.elapsed().as_millis() as u64;

    // 2026-04-26: replace the old `last_imbe_at_millis - started`
    // computation (which was misnamed and reported `last - start`,
    // i.e. essentially the call duration) with the call_tracker's
    // `first_audio_at_unix_ms` — a real first-audio timestamp set
    // by the lifecycle owner the moment the first HDU/audio chunk
    // arrives. Now `first_imbe_ms` actually means "milliseconds
    // from CallOpen to first audio bit", which is the seeding /
    // PLL-acquire metric we wanted.
    let first_imbe_ms = first_audio_at_unix_ms
        .filter(|&t| a.started_unix_ms > 0 && t >= a.started_unix_ms)
        .map(|t| t.saturating_sub(a.started_unix_ms));

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
        first_audio_at_unix_ms,
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
        framer_arm_hdu:
            now_counters.framer_arm_hdu.saturating_sub(a.base.framer_arm_hdu),
        framer_arm_ldu1:
            now_counters.framer_arm_ldu1.saturating_sub(a.base.framer_arm_ldu1),
        framer_arm_ldu2:
            now_counters.framer_arm_ldu2.saturating_sub(a.base.framer_arm_ldu2),
        framer_arm_tdu:
            now_counters.framer_arm_tdu.saturating_sub(a.base.framer_arm_tdu),
        framer_arm_tdu_lc:
            now_counters.framer_arm_tdu_lc.saturating_sub(a.base.framer_arm_tdu_lc),
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
        sources_observed: Vec::new(),
        // 2026-04-26 per-call AGC: read the periodic poller's
        // last sample. None when no audio landed in this call
        // (chain produced nothing → no meaningful "converged"
        // value).
        agc_gain_q97_at_close: {
            let imbe_extracted_delta = now_counters
                .imbe_extracted.saturating_sub(a.base.imbe_extracted);
            if imbe_extracted_delta > 0 {
                Some(forwarder.last_traffic_agc_gain_q97
                    .load(Ordering::Relaxed))
            } else {
                None
            }
        },
        // Set by the CallClose handler from the event payload —
        // ActiveSummary doesn't carry the UPD timestamp itself.
        air_duration_ms: None,
    }
}

/// 2026-04-30: zero-decode summary for a synthetic not_followed
/// CallOpen that arrived while a real call was already active. Pushed
/// inline at CallOpen time so the synthetic emit doesn't displace the
/// real active. duration_ms=0 because the synthetic pair fires
/// back-to-back with no chain time.
fn synthetic_not_followed_summary(
    call_id: u64,
    tg: u16,
    nac: u16,
    source: Option<u32>,
    freq_hz: Option<u64>,
    channel: Option<String>,
    encrypted: bool,
    not_followed: Option<&'static str>,
    started_unix_ms: u64,
) -> GrantDecodeSummary {
    GrantDecodeSummary {
        call_id, tg, nac, source,
        actual_speaker: None,
        started_unix_ms,
        ended_unix_ms: started_unix_ms,
        duration_ms: 0,
        first_imbe_ms: None,
        first_audio_at_unix_ms: None,
        hdu_count: 0, ldu1_count: 0, ldu2_count: 0,
        tdu_count: 0, tdu_lc_count: 0,
        framer_arm_hdu: 0, framer_arm_ldu1: 0, framer_arm_ldu2: 0,
        framer_arm_tdu: 0, framer_arm_tdu_lc: 0,
        imbe_extracted: 0, imbe_dropped: 0,
        vocoder_pcm_samples: 0, vocoder_errors: 0,
        vocoder_silent: 0, vocoder_encrypted: 0,
        encrypted, not_followed,
        freq_hz, channel,
        close_reason: CloseReason::Timeout,
        sources_observed: source.map(|s| vec![s]).unwrap_or_default(),
        agc_gain_q97_at_close: None,
        air_duration_ms: None,
    }
}

/// Drop heartbeat-ghost summaries — zero IMBE, no HDU, no rejection
/// reason. Belt-and-braces guard.

fn push(ring: &GrantStatsRing, summary: GrantDecodeSummary, cap: usize) {
    if let Ok(mut r) = ring.lock() {
        if r.len() >= cap { r.pop_front(); }
        r.push_back(summary);
    }
}

/// 2026-04-29: route a finalised summary into the clear or enc
/// ring based on its encryption / not_followed status. Encrypted
/// or not_followed grants go to the enc ring (smaller cap, kept
/// separate so heavy ENC activity doesn't crowd out clear-call
/// entries that recordings need to pair against by call_id).
fn route_push(
    clear_ring: &GrantStatsRing,
    enc_ring: &GrantStatsRing,
    summary: GrantDecodeSummary,
) {
    if summary.encrypted || summary.not_followed.is_some() {
        push(enc_ring, summary, ENC_RING_CAP);
    } else {
        push(clear_ring, summary, RING_CAP);
    }
}

/// 2026-04-29 enc-side ring cap. Smaller than the clear ring
/// because the encrypted ring's only consumer is the dashboard
/// (operator visibility into ENC activity); recordings never
/// pair against it. 50 covers a few minutes of ENC chatter on
/// a busy site.
pub const ENC_RING_CAP: usize = 50;

pub fn new_ring() -> GrantStatsRing {
    Arc::new(Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

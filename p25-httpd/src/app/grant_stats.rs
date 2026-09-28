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
//!   - `CallOpen` → remember the call.
//!   - `SourceUpdate` → fill the in-flight summary's source field when
//!     the grant carried none (change 060: never overwrites it).
//!   - `CallClose` → push `GrantDecodeSummary` into the ring at once,
//!     with the call's own counters (change 057, below).
//!
//! The shape of `GrantDecodeSummary` is preserved for API
//! compatibility — the dashboard's `/api/grant_decode_stats` parsing
//! is unchanged.
//!
//! Change 057: the decode counters come from
//! `ImbeForwarder::call_counts`, attributed by call_id where each frame
//! is decoded, instead of global-counter deltas between open and close
//! (which also counted the neighbouring calls' frames). Frames of the
//! call decoded after the close (the air-time tail, still in the ring or
//! the vocoder queue) are picked up by a refresh every
//! `REFRESH_TICK_MS` for `REFRESH_WINDOW_MS` after the close; each
//! change bumps `GrantStatsRev` so `/api/ui/calls` is refetched. The
//! pre-057 2 s blocking wait for the vocoder at every close is gone
//! (it also delayed the next CallOpen, shortening that call's
//! `duration_ms`); `duration_ms` is now the lifecycle's own open time.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::app::call_counters::CallCounts;
use crate::app::grant_follower::{
    CallTrackerEvent, CallTrackerEventKind, CallTrackerEventTx, CloseReason,
};
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::hardware::traffic_lane::Lane;

/// Change 057: re-read the counters of recently closed calls this often,
/// for `REFRESH_WINDOW_MS` after the close (covers the air-time tail:
/// dibit delivery ≤ 0.2 s, vocoder queue, recorder drain 2 s).
const REFRESH_TICK_MS: u64 = 250;
const REFRESH_WINDOW_MS: u64 = 10_000;

/// Change 057: bumped whenever a summary already in a ring changes (late
/// tail frames counted after the close). Part of `/api/ui/state`
/// `calls_rev`.
pub type GrantStatsRev = Arc<AtomicU64>;

pub fn new_rev() -> GrantStatsRev {
    Arc::new(AtomicU64::new(0))
}

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
    /// Change 066: traffic chain that followed the call (1 or 2; 0 = not
    /// followed).
    #[serde(default, skip_serializing_if = "is_zero_u8")]
    pub chain: u8,
}

fn is_zero_u8(v: &u8) -> bool { *v == 0 }

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
    freq_hz: Option<u64>,
    channel: Option<String>,
    /// Change 066: traffic chain of the call.
    lane: Option<Lane>,
}

/// Change 066: the forwarder of `lane` (its AGC reading); the counter
/// book is shared, so any forwarder serves for counts.
fn forwarder_for(forwarders: &[Arc<ImbeForwarder>], lane: Option<Lane>) -> &Arc<ImbeForwarder> {
    forwarders.iter().find(|f| Some(f.lane) == lane).unwrap_or(&forwarders[0])
}

/// Change 057: copy a call's counters into its summary. Returns true
/// when anything changed.
fn apply_counts(s: &mut GrantDecodeSummary, c: &CallCounts) -> bool {
    let before = (
        s.hdu_count, s.ldu1_count, s.ldu2_count, s.tdu_count, s.tdu_lc_count,
        s.imbe_extracted, s.imbe_dropped, s.vocoder_pcm_samples,
        s.vocoder_errors, s.vocoder_silent, s.vocoder_encrypted,
        s.framer_arm_hdu + s.framer_arm_ldu1 + s.framer_arm_ldu2
            + s.framer_arm_tdu + s.framer_arm_tdu_lc,
    );
    s.hdu_count = c.hdu;
    s.ldu1_count = c.ldu1;
    s.ldu2_count = c.ldu2;
    s.tdu_count = c.tdu;
    s.tdu_lc_count = c.tdu_lc;
    s.framer_arm_hdu = c.framer_arm_hdu;
    s.framer_arm_ldu1 = c.framer_arm_ldu1;
    s.framer_arm_ldu2 = c.framer_arm_ldu2;
    s.framer_arm_tdu = c.framer_arm_tdu;
    s.framer_arm_tdu_lc = c.framer_arm_tdu_lc;
    s.imbe_extracted = c.imbe_extracted;
    s.imbe_dropped = c.imbe_dropped;
    s.vocoder_pcm_samples = c.vocoder_pcm_samples;
    s.vocoder_errors = c.vocoder_errors;
    s.vocoder_silent = c.vocoder_silent;
    s.vocoder_encrypted = c.vocoder_encrypted;
    let after = (
        s.hdu_count, s.ldu1_count, s.ldu2_count, s.tdu_count, s.tdu_lc_count,
        s.imbe_extracted, s.imbe_dropped, s.vocoder_pcm_samples,
        s.vocoder_errors, s.vocoder_silent, s.vocoder_encrypted,
        s.framer_arm_hdu + s.framer_arm_ldu1 + s.framer_arm_ldu2
            + s.framer_arm_tdu + s.framer_arm_tdu_lc,
    );
    before != after
}

/// Change 057: calls closed less than `REFRESH_WINDOW_MS` ago whose
/// summaries still pick up late-counted frames.
#[derive(Default)]
struct Pending {
    calls: VecDeque<(u64, Instant)>,
}

/// Change 057: refresh the summaries of recently closed calls from the
/// per-call counters; drop calls past the window. Bumps `rev` when a
/// summary changed.
fn refresh_pending(
    pending: &mut Pending,
    forwarder: &ImbeForwarder,
    clear_ring: &GrantStatsRing,
    enc_ring: &GrantStatsRing,
    rev: &AtomicU64,
    now: Instant,
) {
    let mut changed = false;
    for &(call_id, _) in pending.calls.iter() {
        let Some(counts) = forwarder.call_counts.get(call_id) else {
            continue;
        };
        for ring in [clear_ring, enc_ring] {
            if let Ok(mut r) = ring.lock() {
                if let Some(s) = r.iter_mut().rev().find(|s| s.call_id == call_id) {
                    changed |= apply_counts(s, &counts);
                    break;
                }
            }
        }
    }
    let window = Duration::from_millis(REFRESH_WINDOW_MS);
    pending.calls.retain(|(_, closed)| now.saturating_duration_since(*closed) < window);
    if changed {
        rev.fetch_add(1, Ordering::Relaxed);
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
    // Change 066: one per traffic chain, lane One first.
    forwarders: Vec<Arc<ImbeForwarder>>,
    clear_ring: GrantStatsRing,
    enc_ring: GrantStatsRing,
    // Change 057: bumped when a closed call's summary changes.
    rev: GrantStatsRev,
) {
    let mut rx = tracker_tx.subscribe();
    tokio::spawn(async move {
        // Change 066: the open call of each traffic chain.
        let mut active: Vec<ActiveSummary> = Vec::new();
        let mut pending = Pending::default();
        let mut tick = tokio::time::interval(Duration::from_millis(REFRESH_TICK_MS));
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Ok(event) => handle_event(
                        event, &mut active, &mut pending,
                        &forwarders, &clear_ring, &enc_ring, &rev,
                    ),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(
                            target: "p25_grant_stats",
                            "tracker events lagged by {n}; summaries may be missing",
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    if !pending.calls.is_empty() {
                        refresh_pending(
                            &mut pending, &forwarders[0], &clear_ring,
                            &enc_ring, &rev, Instant::now(),
                        );
                    }
                }
            }
        }
    });
}

/// Change 065: fill a not-followed summary's channel time from its
/// (late) CallClose. True when a summary changed.
fn note_not_followed_end(
    call_id: u64,
    ended_unix_ms: u64,
    open_ms: u64,
    last_upd_at_unix_ms: u64,
    clear_ring: &GrantStatsRing,
    enc_ring: &GrantStatsRing,
) -> bool {
    for ring in [enc_ring, clear_ring] {
        if let Ok(mut r) = ring.lock() {
            if let Some(s) = r.iter_mut().rev()
                .find(|s| s.call_id == call_id && s.not_followed.is_some())
            {
                s.ended_unix_ms = ended_unix_ms;
                s.duration_ms = open_ms;
                s.air_duration_ms = (last_upd_at_unix_ms > s.started_unix_ms)
                    .then(|| last_upd_at_unix_ms - s.started_unix_ms);
                return true;
            }
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn handle_event(
    event: CallTrackerEvent,
    active: &mut Vec<ActiveSummary>,
    pending: &mut Pending,
    forwarders: &[Arc<ImbeForwarder>],
    clear_ring: &GrantStatsRing,
    enc_ring: &GrantStatsRing,
    rev: &GrantStatsRev,
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
            // Change 065: always inline for a not-followed call — its
            // CallClose now comes seconds later (channel time, see
            // `grant_follower::NfCalls`) and updates this summary, so it
            // must never occupy `active`.
            if not_followed.is_some() {
                let summary = synthetic_not_followed_summary(
                    event.call_id, tg, nac, source, freq_hz,
                    channel, encrypted, not_followed,
                    event.timestamp_unix_ms,
                );
                route_push(clear_ring, enc_ring, summary);
                return;
            }
            // Defensive: if there's somehow an active summary still
            // open on this chain (call_tracker should have closed it
            // first), synthesise a Timeout close so we don't leak.
            // Change 066: per chain; the other chain's call stays open.
            if let Some(i) = active.iter().position(|a| a.lane == event.lane) {
                let prev = active.remove(i);
                tracing::warn!(
                    target: "p25_grant_stats",
                    "stale active summary for call_id={} on new \
                     CallOpen call_id={} — pushing as Timeout",
                    prev.call_id, event.call_id,
                );
                let open_ms = prev.started_instant.elapsed().as_millis() as u64;
                let summary = finalise_summary(
                    &prev, prev.source, prev.actual_speaker,
                    CloseReason::Timeout, None, forwarder_for(forwarders, prev.lane),
                    event.timestamp_unix_ms, open_ms,
                );
                pending.calls.push_back((prev.call_id, Instant::now()));
                route_push(clear_ring, enc_ring, summary);
            }
            active.push(ActiveSummary {
                call_id: event.call_id,
                tg,
                nac,
                source,
                actual_speaker: None,
                encrypted,
                not_followed,
                started_unix_ms: event.timestamp_unix_ms,
                started_instant: Instant::now(),
                freq_hz,
                channel,
                lane: event.lane,
            });
        }

        // Change 060: fill-only, like the lifecycle's own `source`. The
        // call's source is the unit the grant was issued to; units the
        // voice link control names later stay in `sources_observed` /
        // `actual_speaker`. On the site those were wrong each time they
        // differed from the grant (2026-09-27: grant 3400043 heard, LC
        // 1014; dispatch 1013 heard, LC 3402072), and overwriting made
        // them the call's source.
        CallTrackerEventKind::SourceUpdate { new_source, .. } => {
            if let Some(a) = active.iter_mut().find(|a| a.call_id == event.call_id) {
                if a.source.is_none() {
                    a.source = Some(new_source);
                }
            }
        }

        CallTrackerEventKind::ActualSpeakerObserved { speaker, .. } => {
            if let Some(a) = active.iter_mut().find(|a| a.call_id == event.call_id) {
                a.actual_speaker = Some(speaker);
            }
        }

        CallTrackerEventKind::CallClose {
            reason, final_source, final_actual_speaker,
            first_audio_at_unix_ms, sources_observed,
            last_upd_at_unix_ms, ended_unix_ms, open_ms, ..
        } => {
            // 2026-04-30: peek before take. The CallClose half of a
            // synthetic not_followed pair (handled inline at CallOpen)
            // arrives here; if `active` holds the real call, taking it
            // unconditionally would lose the real call's eventual
            // finalise. Only consume an open call when call_ids match.
            let Some(i) = active.iter().position(|a| a.call_id == event.call_id) else {
                // Change 065: the close of a not-followed call carries
                // its channel time.
                if note_not_followed_end(
                    event.call_id, ended_unix_ms, open_ms,
                    last_upd_at_unix_ms, clear_ring, enc_ring,
                ) {
                    rev.fetch_add(1, Ordering::Relaxed);
                }
                return;
            };
            let prev = active.remove(i);
            let forwarder = forwarder_for(forwarders, prev.lane);

            // Change 057: no wait for the vocoder here. The counters
            // are this call's own (by call_id); frames still in flight
            // are added by `refresh_pending` as they are counted.
            let mut summary = finalise_summary(
                &prev, final_source, final_actual_speaker, reason,
                first_audio_at_unix_ms, forwarder, ended_unix_ms, open_ms,
            );
            pending.calls.push_back((prev.call_id, Instant::now()));
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

#[allow(clippy::too_many_arguments)]
fn finalise_summary(
    a: &ActiveSummary,
    final_source: Option<u32>,
    final_actual_speaker: Option<u32>,
    close_reason: CloseReason,
    first_audio_at_unix_ms: Option<u64>,
    forwarder: &ImbeForwarder,
    // Change 057: the lifecycle's close time and open duration.
    ended_unix_ms: u64,
    open_ms: u64,
) -> GrantDecodeSummary {
    // Change 057: this call's counters so far (by call_id).
    let counts = forwarder.call_counts.get(a.call_id).unwrap_or_default();

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

    let mut summary = GrantDecodeSummary {
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
        ended_unix_ms,
        duration_ms: open_ms,
        first_imbe_ms,
        first_audio_at_unix_ms,
        hdu_count: 0,
        ldu1_count: 0,
        ldu2_count: 0,
        tdu_count: 0,
        tdu_lc_count: 0,
        framer_arm_hdu: 0,
        framer_arm_ldu1: 0,
        framer_arm_ldu2: 0,
        framer_arm_tdu: 0,
        framer_arm_tdu_lc: 0,
        imbe_extracted: 0,
        imbe_dropped: 0,
        vocoder_pcm_samples: 0,
        vocoder_errors: 0,
        vocoder_silent: 0,
        vocoder_encrypted: 0,
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
        agc_gain_q97_at_close: if counts.imbe_extracted > 0 {
            Some(forwarder.last_traffic_agc_gain_q97.load(Ordering::Relaxed))
        } else {
            None
        },
        // Set by the CallClose handler from the event payload —
        // ActiveSummary doesn't carry the UPD timestamp itself.
        air_duration_ms: None,
        chain: a.lane.map_or(0, |l| l.number()),
    };
    apply_counts(&mut summary, &counts);
    summary
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
        chain: 0,
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

#[cfg(test)]
#[path = "grant_stats_tests.rs"]
mod tests;

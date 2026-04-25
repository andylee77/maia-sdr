//! Unified call lifecycle authority — Phase 2 of the 2026-04-25
//! `UNIFIED_CALL_LIFECYCLE.md` design.
//!
//! Single source of truth for "what is a call, when did it open,
//! when did it close, who's talking". `grant_stats` and the recorder
//! both subscribe to the events emitted from here instead of
//! independently deciding lifecycle from raw `CallBoundary` events
//! and audio chunks.
//!
//! Why this exists: pre-Phase 2, `grant_stats` opened OpenGrants on
//! `CcGrantArrival` while the recorder opened WAVs on first
//! `AudioChunk`. The two never reconciled. Result: rec#6 (25.2 s
//! glued from four TG 302 PTTs), 84 % of vocoder PCM missing from
//! recordings, and "(no audio)" labels on calls whose audio actually
//! got appended to a prior recording. Phase 1 added explicit
//! `cc_grant_split` triggers in the recorder as a band-aid; Phase 2
//! is the proper fix where one module owns call identity.
//!
//! ## Inputs
//!
//! - `CallBoundaryRx` from `imbe_forwarder` + grant follower:
//!   `CcGrantArrival`, `CcGrantUpdate`, `HduStart`, `TdulcComplete`
//!   (LDU1 LC FM: voted consensus), `SpeakerEnd`.
//! - `AudioChunkRx` from the vocoder broadcast: refreshes ttl when
//!   audio is actually flowing on the chain, even between CC keep-
//!   alive cadences.
//! - Periodic timeout tick: closes calls whose `last_activity_ms` is
//!   older than `CALL_TIMEOUT_MS`.
//!
//! ## Outputs (`CallTrackerEvent`)
//!
//! - `CallOpen { call_id, tg, source, ... }` — recorder opens a WAV,
//!   grant_stats baselines counters.
//! - `SourceUpdate { call_id, new_source, via }` — recorder updates
//!   filename suffix, grant_stats updates summary source.
//! - `CallClose { call_id, reason, final_source, ... }` — recorder
//!   finalises, grant_stats pushes summary.
//!
//! Each `call_id` is a monotonic per-process counter. Recordings
//! and grant_decode_stats entries can join on it for per-call
//! cross-referencing.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio::{AudioChunk, CallBoundary, CallBoundaryKind};

/// Maximum gap between CC grants of the same `(tg, source)` to
/// treat the second as a retransmit (no new call). Matches the
/// 2026-04-24 grant_stats `RETRANSMIT_WINDOW_MS`.
const RETRANSMIT_WINDOW_MS: u64 = 6_000;

/// Inactivity window before an active call is closed by timeout.
/// Refreshed by audio chunks, CC updates, HduStart, and TdulcComplete.
const CALL_TIMEOUT_MS: u64 = 10_000;

/// Cadence of the periodic timeout sweep. Frequent enough that
/// timeouts feel snappy on the dashboard but cheap (no shared lock).
const TIMEOUT_TICK_MS: u64 = 500;

/// Minimum quiescence gap before an HduStart event triggers a new-
/// call split. Below this, the HDU is treated as a same-speaker PTT
/// re-key (continuous audio with momentary protocol break, common
/// during voice). Above this, the call paused long enough that a
/// new HDU is most likely a new speaker keying up.
///
/// Specifically targets TGs where every CC grant carries the SAME
/// source attribution — e.g., TG 302 on Clay County NAC 8A1 emits
/// `GRP_VCH_GRANT.SRC=00000` for every speaker, so the source-
/// change-based split can't differentiate. HDU-after-gap is the
/// fallback signal for those TGs.
const HDU_SPLIT_GAP_MIN_MS: u64 = 500;

#[derive(Debug, Clone)]
pub struct CallTrackerEvent {
    pub call_id: u64,
    pub timestamp_unix_ms: u64,
    pub kind: CallTrackerEventKind,
}

#[derive(Debug, Clone)]
pub enum CallTrackerEventKind {
    CallOpen {
        tg: u16,
        nac: u16,
        source: Option<u32>,
        freq_hz: Option<u64>,
        channel: Option<String>,
        encrypted: bool,
        not_followed: Option<&'static str>,
        opened_via: OpenReason,
        /// Snapshot of `frames_submitted` at open. Recorder + grant_
        /// stats use this as a baseline for delta computation against
        /// the value snapshotted into `CallClose.expected_submit_count`.
        baseline_frames_submitted: u64,
    },
    SourceUpdate {
        new_source: u32,
        via: SourceUpdateVia,
    },
    /// 2026-04-25: LDU1 LC FM: voted consensus observed (the actual
    /// keying radio per the on-air voice frame). Distinct from
    /// `SourceUpdate` — this NEVER overrides the call's `source`
    /// (which is CC-authoritative). It populates a separate
    /// `actual_speaker` field that downstream consumers can surface
    /// alongside the CC source. When CC SRC and LDU1 LC FM agree
    /// (the common case), both fields hold the same value.
    ActualSpeakerObserved {
        speaker: u32,
        agrees_with_cc: bool,
    },
    CallClose {
        reason: CloseReason,
        final_source: Option<u32>,
        /// LDU1-LC-voted speaker observed during the call. May
        /// differ from `final_source` when CC and on-air voice
        /// disagree (LC FEC corruption OR genuine CC-vs-actual
        /// difference on certain sites).
        final_actual_speaker: Option<u32>,
        started_unix_ms: u64,
        ended_unix_ms: u64,
        first_audio_at_unix_ms: Option<u64>,
        first_hdu_at_unix_ms: Option<u64>,
        /// Snapshot of `frames_submitted` at close. Recorder waits
        /// for `frames_consumed >= this` (with timeout) before
        /// finalising the WAV so trailing PCM gets appended.
        expected_submit_count: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenReason {
    /// CC `GRP_VCH_GRANT` arrived (or `GVCG_UPDT_EXP`). Normal path.
    CcGrant,
    /// CC announced a new SRC for an active TG. Old call closed via
    /// `CloseReason::SpeakerChange`, this event opens the successor.
    SpeakerChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceUpdateVia {
    /// Late CC `GRP_VCH_GRANT` for an active call that was opened
    /// from a CC `GRNT_UPD` (no SRC). Fills in source.
    CcRefresh,
    /// LDU1 LC FM: 3-of-4 voted consensus. Only used to fill a None.
    Ldu1LcVote,
    /// TDULC Motorola TALK_COMPLETE BY: at end-of-speaker. Stamped
    /// on the closing call without overriding existing CC source.
    TdulcMotTc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// MOT_TC, CALL_TERM, or bare TDU fired SpeakerEnd.
    SpeakerEnd,
    /// No CC refresh and no audio for `CALL_TIMEOUT_MS`.
    Timeout,
    /// CC announced a new SRC for the same TG; old call superseded.
    SpeakerChange,
    /// CC announced a different TG; old call superseded.
    TgChange,
    /// Encrypted/sticky/monitor-rejected call expired without ever
    /// getting traffic-chain decode. Implementation note: today
    /// these flow through `Timeout` since the active-call test isn't
    /// distinguishing rejected from accepted at the lifecycle layer.
    /// Reserved for future Phase 3 per-TG branching.
    NotFollowedExpire,
}

pub type CallTrackerEventTx = broadcast::Sender<CallTrackerEvent>;

/// Capacity 64 — events fire at most ~1 per call boundary, not per-
/// frame. Lag means a downstream consumer fell behind, not data
/// loss in the source-of-truth.
pub fn new_event_tx() -> CallTrackerEventTx {
    broadcast::channel(64).0
}

/// Pull-based snapshot of the currently-active call. Mirrored from
/// the spawn_call_tracker task on every state mutation so HTTP
/// handlers can read "what's active right now" without subscribing
/// to the broadcast.
///
/// Phase 2e (2026-04-25): replaces the long-lived
/// `ControlChannelDecoder.grants` HashMap as the source of truth for
/// the dashboard's Active Grants panel. The HashMap accumulated
/// zombie entries because grants only expired on a 30 s sweep; the
/// CallTracker authority closes calls within seconds of TDU /
/// timeout, so reads here track real call state.
#[derive(Debug, Clone)]
pub struct ActiveCallSnapshot {
    pub call_id: u64,
    pub tg: u16,
    pub nac: u16,
    pub source: Option<u32>,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub encrypted: bool,
    pub started_unix_ms: u64,
}

pub type ActiveCallShared =
    std::sync::Arc<std::sync::Mutex<Option<ActiveCallSnapshot>>>;

pub fn new_active_call_shared() -> ActiveCallShared {
    std::sync::Arc::new(std::sync::Mutex::new(None))
}

fn mirror_active(active: &Option<ActiveCall>, shared: &ActiveCallShared) {
    if let Ok(mut s) = shared.lock() {
        *s = active.as_ref().map(|c| ActiveCallSnapshot {
            call_id: c.call_id,
            tg: c.tg,
            nac: c.nac,
            source: c.source,
            freq_hz: c.freq_hz,
            channel: c.channel.clone(),
            encrypted: c.encrypted,
            started_unix_ms: c.started_unix_ms,
        });
    }
}

struct ActiveCall {
    call_id: u64,
    tg: u16,
    nac: u16,
    source: Option<u32>,
    /// 2026-04-25: LDU1 LC FM: voted consensus from the on-air
    /// voice frames. Tracked SEPARATELY from `source` (which is
    /// CC `GRP_VCH_GRANT.SRC`, authoritative). When the chain has
    /// not yet decoded any LDU1 LC successfully, this stays None.
    /// On disagreement with CC, this captures the radio the on-
    /// air frames identified — could be LC FEC corruption or a
    /// real CC-vs-current-speaker difference.
    actual_speaker: Option<u32>,
    freq_hz: Option<u64>,
    channel: Option<String>,
    encrypted: bool,
    not_followed: Option<&'static str>,
    started_unix_ms: u64,
    started_instant: Instant,
    last_activity_ms: u64,
    first_audio_at_unix_ms: Option<u64>,
    first_hdu_at_unix_ms: Option<u64>,
    baseline_frames_submitted: u64,
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn emit_open(
    tx: &CallTrackerEventTx,
    call_id: u64,
    tg: u16,
    nac: u16,
    source: Option<u32>,
    freq_hz: Option<u64>,
    channel: Option<String>,
    encrypted: bool,
    not_followed: Option<&'static str>,
    opened_via: OpenReason,
    baseline_frames_submitted: u64,
    started_unix_ms: u64,
) {
    let _ = tx.send(CallTrackerEvent {
        call_id,
        timestamp_unix_ms: started_unix_ms,
        kind: CallTrackerEventKind::CallOpen {
            tg, nac, source, freq_hz, channel,
            encrypted, not_followed, opened_via,
            baseline_frames_submitted,
        },
    });
}

fn emit_close(
    tx: &CallTrackerEventTx,
    call: &ActiveCall,
    reason: CloseReason,
    final_source: Option<u32>,
    expected_submit_count: u64,
) {
    let now = now_unix_ms();
    let _ = tx.send(CallTrackerEvent {
        call_id: call.call_id,
        timestamp_unix_ms: now,
        kind: CallTrackerEventKind::CallClose {
            reason,
            final_source,
            final_actual_speaker: call.actual_speaker,
            started_unix_ms: call.started_unix_ms,
            ended_unix_ms: now,
            first_audio_at_unix_ms: call.first_audio_at_unix_ms,
            first_hdu_at_unix_ms: call.first_hdu_at_unix_ms,
            expected_submit_count,
        },
    });
}

fn emit_source_update(
    tx: &CallTrackerEventTx,
    call_id: u64,
    new_source: u32,
    via: SourceUpdateVia,
) {
    let _ = tx.send(CallTrackerEvent {
        call_id,
        timestamp_unix_ms: now_unix_ms(),
        kind: CallTrackerEventKind::SourceUpdate { new_source, via },
    });
}

/// Decide whether a `CcGrantArrival` event for `(new_tg, new_source)`
/// is a CC retransmit of the active call (refresh only), an unrelated
/// grant we should ignore entirely, or a transition that should close
/// the active call and open a new one. See `UNIFIED_CALL_LIFECYCLE.md`
/// § "Speaker-change semantics" for the decision tree.
enum ArrivalDisposition {
    /// CC retransmit of the same call. Refresh ttl, don't close.
    Retransmit { source_upgrade: Option<u32> },
    /// Grant is for a different traffic freq than what we're recording.
    /// Single-traffic-chain invariant: it cannot be our call. Do
    /// nothing — neither close, nor refresh ttl, nor open. The follower
    /// decides separately whether to retune to a different priority
    /// grant; if it does, the active recording will go silent and
    /// timeout-close on its own.
    Ignore,
    /// Different TG on our SAME traffic freq (channel reuse / takeover
    /// — old call ended, new TG took the freq). Close active, open new.
    TgChange,
    /// Same TG, different non-None SRC. Close active, open new.
    SpeakerChange,
}

fn classify_cc_arrival(
    active: &ActiveCall,
    new_tg: u16,
    new_source: Option<u32>,
    new_freq_hz: Option<u64>,
    now_ms: u64,
) -> ArrivalDisposition {
    // Single-traffic-chain invariant: a CC grant for a different
    // traffic freq than what we're currently recording cannot affect
    // the active recording. The trunked CC announces grants for ALL
    // talkgroups; only those on our parked freq matter to lifecycle.
    // If either side's freq is unknown, fall through to TG-based
    // classification (defensive — older boundary-emit paths could
    // omit freq_hz; current follower always populates it).
    if let (Some(a_freq), Some(n_freq)) = (active.freq_hz, new_freq_hz) {
        if a_freq != n_freq {
            return ArrivalDisposition::Ignore;
        }
    }
    if active.tg != new_tg {
        return ArrivalDisposition::TgChange;
    }
    let stale = now_ms.saturating_sub(active.last_activity_ms)
        >= RETRANSMIT_WINDOW_MS;
    if stale {
        // Past the retransmit window even on the same TG. Conservative
        // call: treat as a new call rather than risk gluing two
        // separate PTTs into one record.
        return match (active.source, new_source) {
            (Some(_), Some(_)) if active.source != new_source => {
                ArrivalDisposition::SpeakerChange
            }
            // Same source AND past retransmit window — could be re-key
            // by same speaker or a CC retransmit from a stuck source.
            // Same-speaker re-key historically should still be ONE
            // call per the operator's "users key, talk, unkey" model.
            // Be conservative: if the source matches, refresh.
            _ => ArrivalDisposition::Retransmit { source_upgrade: None },
        };
    }
    match (active.source, new_source) {
        (None, None) => ArrivalDisposition::Retransmit {
            source_upgrade: None,
        },
        (None, Some(s)) => ArrivalDisposition::Retransmit {
            source_upgrade: Some(s),
        },
        (Some(_), None) => ArrivalDisposition::Retransmit {
            source_upgrade: None,
        },
        (Some(a), Some(b)) if a == b => ArrivalDisposition::Retransmit {
            source_upgrade: None,
        },
        (Some(_), Some(_)) => ArrivalDisposition::SpeakerChange,
    }
}

/// Spawn the call_tracker authority task. Subscribes to the
/// `CallBoundary` broadcast and the audio broadcast, dispatches
/// `CallTrackerEvent` to subscribers.
pub fn spawn_call_tracker(
    boundary_tx: crate::audio::CallBoundaryTx,
    audio_tx: broadcast::Sender<AudioChunk>,
    tracker_tx: CallTrackerEventTx,
    forwarder: Arc<ImbeForwarder>,
    active_call: ActiveCallShared,
) {
    let mut boundary_rx = boundary_tx.subscribe();
    let mut audio_rx = audio_tx.subscribe();

    tokio::spawn(async move {
        let mut active: Option<ActiveCall> = None;
        let mut next_call_id: u64 = 1;
        let mut tick = tokio::time::interval(
            Duration::from_millis(TIMEOUT_TICK_MS),
        );
        tick.tick().await; // discard first immediate tick

        loop {
            tokio::select! {
                recv = boundary_rx.recv() => {
                    let boundary = match recv {
                        Ok(b) => b,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: "p25_call_tracker",
                                "boundary lagged by {n} events; \
                                 dropping in-flight call to avoid \
                                 stale state",
                            );
                            if let Some(call) = active.take() {
                                let expected = forwarder
                                    .frames_submitted.load(Ordering::Relaxed);
                                emit_close(&tracker_tx, &call,
                                    CloseReason::Timeout,
                                    call.source, expected);
                            }
                            mirror_active(&active, &active_call);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    handle_boundary(
                        boundary, &mut active, &mut next_call_id,
                        &tracker_tx, &forwarder,
                    );
                    mirror_active(&active, &active_call);
                }
                recv = audio_rx.recv() => {
                    match recv {
                        Ok(chunk) => {
                            handle_audio(chunk, &mut active);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Audio lag is informational — call_tracker
                            // doesn't lose state from missed chunks
                            // (CC events are the authority).
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tick.tick() => {
                    let now = now_unix_ms();
                    let should_close = active.as_ref()
                        .map(|c| now.saturating_sub(c.last_activity_ms)
                                    > CALL_TIMEOUT_MS)
                        .unwrap_or(false);
                    if should_close {
                        if let Some(call) = active.take() {
                            let expected = forwarder
                                .frames_submitted.load(Ordering::Relaxed);
                            emit_close(&tracker_tx, &call,
                                CloseReason::Timeout,
                                call.source, expected);
                        }
                        mirror_active(&active, &active_call);
                    }
                }
            }
        }
        tracing::warn!(
            target: "p25_call_tracker",
            "call_tracker task exiting (channels closed)",
        );
    });
}

fn handle_boundary(
    boundary: CallBoundary,
    active: &mut Option<ActiveCall>,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    forwarder: &Arc<ImbeForwarder>,
) {
    match boundary.kind {
        CallBoundaryKind::CcGrantArrival {
            tg, source, freq_hz, channel, encrypted, not_followed,
        } => {
            let now = now_unix_ms();
            let channel_str = if channel == 0 {
                None
            } else {
                Some(format!("{}", channel))
            };

            // Decide retransmit vs new call based on currently-active
            // state.
            let (close_existing, opened_via) = match active.as_ref() {
                None => (false, OpenReason::CcGrant),
                Some(a) => match classify_cc_arrival(a, tg, source, freq_hz, now) {
                    ArrivalDisposition::Retransmit { source_upgrade } => {
                        // Refresh the active call in place. No emit.
                        let a_mut = active.as_mut().unwrap();
                        a_mut.last_activity_ms = now;
                        if let Some(s) = source_upgrade {
                            a_mut.source = Some(s);
                            emit_source_update(
                                tx, a_mut.call_id, s,
                                SourceUpdateVia::CcRefresh,
                            );
                        }
                        if not_followed.is_some()
                            && a_mut.not_followed.is_none()
                        {
                            a_mut.not_followed = not_followed;
                        }
                        if encrypted {
                            a_mut.encrypted = true;
                        }
                        return;
                    }
                    ArrivalDisposition::Ignore => {
                        // Grant is for some other (tg, freq) on the
                        // trunked system. Not our chain. Don't even
                        // refresh ttl — we are decoding something else.
                        return;
                    }
                    ArrivalDisposition::SpeakerChange => {
                        (true, OpenReason::SpeakerChange)
                    }
                    ArrivalDisposition::TgChange => {
                        (true, OpenReason::CcGrant)
                    }
                },
            };

            if close_existing {
                if let Some(prev) = active.take() {
                    let reason = match opened_via {
                        OpenReason::SpeakerChange =>
                            CloseReason::SpeakerChange,
                        OpenReason::CcGrant => CloseReason::TgChange,
                    };
                    let expected = forwarder
                        .frames_submitted.load(Ordering::Relaxed);
                    emit_close(tx, &prev, reason, prev.source, expected);
                }
            }

            let call_id = *next_call_id;
            *next_call_id += 1;
            let baseline = forwarder
                .frames_submitted.load(Ordering::Relaxed);
            *active = Some(ActiveCall {
                call_id,
                tg,
                nac: boundary.nac,
                source,
                actual_speaker: None,
                freq_hz,
                channel: channel_str.clone(),
                encrypted,
                not_followed,
                started_unix_ms: now,
                started_instant: Instant::now(),
                last_activity_ms: now,
                first_audio_at_unix_ms: None,
                first_hdu_at_unix_ms: None,
                baseline_frames_submitted: baseline,
            });
            emit_open(
                tx, call_id, tg, boundary.nac, source, freq_hz,
                channel_str, encrypted, not_followed, opened_via,
                baseline, now,
            );
        }

        CallBoundaryKind::CcGrantUpdate { tg, .. } => {
            if let Some(a) = active.as_mut() {
                if a.tg == tg {
                    a.last_activity_ms = now_unix_ms();
                }
            }
        }

        CallBoundaryKind::HduStart => {
            // 2026-04-25 Fix A — HDU-after-silence-gap split.
            //
            // For TGs where CC source attribution is constant (e.g.,
            // TG 302 SRC=00000), the CcGrantArrival classify_arrival
            // logic treats every new PTT as a retransmit. The
            // source-change path (LDU1 LC vote) is unreliable due to
            // weak LC FEC. HduStart is the third signal we have:
            // genuine PTT-key on the air. If quiescence_ms was long
            // enough (≥ HDU_SPLIT_GAP_MIN_MS), this HDU is a new
            // speaker — close active + open new with same TG/source.
            //
            // Below the threshold (e.g. 100 ms gap), the HDU is a
            // protocol artefact during a continuous PTT — keep the
            // active call open.
            //
            // 2026-04-25 Phase 2b followup: gate split on
            // `active.source.is_none()`. When CC gave us a real SRC
            // (the common case), CC-driven SpeakerChange already
            // handles speaker transitions cleanly — using HDU-after-
            // gap as a second mechanism creates false-positive
            // splits during legitimate same-speaker PTT re-keys
            // (rec=3 → rec=4 both src=3409922 observed 2026-04-25).
            // The original design intent was source-less TGs only.
            let now = now_unix_ms();
            let split = if let Some(a) = active.as_ref() {
                if a.source.is_some() {
                    false
                } else {
                    let gap_ms = now.saturating_sub(a.last_activity_ms);
                    gap_ms >= HDU_SPLIT_GAP_MIN_MS && gap_ms < CALL_TIMEOUT_MS
                }
            } else {
                false
            };
            if split {
                if let Some(prev) = active.take() {
                    let expected = forwarder
                        .frames_submitted.load(Ordering::Relaxed);
                    emit_close(tx, &prev, CloseReason::SpeakerChange,
                               prev.source, expected);
                    // Open a successor call carrying the same TG/
                    // source attribution. classify_arrival's
                    // semantics (CC drives identity) still apply
                    // when the next CC grant arrives — at which
                    // point we may legitimately see a SourceUpdate.
                    let call_id = *next_call_id;
                    *next_call_id += 1;
                    let baseline = forwarder
                        .frames_submitted.load(Ordering::Relaxed);
                    *active = Some(ActiveCall {
                        call_id,
                        tg: prev.tg,
                        nac: boundary.nac.max(prev.nac),
                        source: prev.source,
                        actual_speaker: None,
                        freq_hz: prev.freq_hz,
                        channel: prev.channel.clone(),
                        encrypted: prev.encrypted,
                        not_followed: prev.not_followed,
                        started_unix_ms: now,
                        started_instant: Instant::now(),
                        last_activity_ms: now,
                        first_audio_at_unix_ms: Some(now),
                        first_hdu_at_unix_ms: Some(now),
                        baseline_frames_submitted: baseline,
                    });
                    emit_open(
                        tx, call_id, prev.tg,
                        boundary.nac.max(prev.nac), prev.source,
                        prev.freq_hz, prev.channel, prev.encrypted,
                        prev.not_followed, OpenReason::SpeakerChange,
                        baseline, now,
                    );
                }
            } else if let Some(a) = active.as_mut() {
                if a.first_hdu_at_unix_ms.is_none() {
                    a.first_hdu_at_unix_ms = Some(now);
                }
                if a.first_audio_at_unix_ms.is_none() {
                    a.first_audio_at_unix_ms = Some(now);
                }
                a.last_activity_ms = now;
                if a.nac == 0 && boundary.nac != 0 {
                    a.nac = boundary.nac;
                }
            }
        }

        CallBoundaryKind::TdulcComplete { source } => {
            // 2026-04-25: emitted by `imbe_forwarder.on_ldu1` when
            // 3-of-4 LDU1 LC FM: vote agrees on a new plausibility-
            // passed RID. This is the ACTUAL CURRENT SPEAKER per
            // the on-air voice frames — distinct from CC `source`.
            //
            // If `a.source` is None (CC didn't tell us who, e.g.
            // TG 302 SRC=0 site convention), we DO fill it in from
            // the LC vote so the recording isn't sourceless.
            //
            // If `a.source` is already set (CC gave us SRC), we
            // populate `actual_speaker` SEPARATELY without
            // disturbing `source`. Both flow through to grant_stats
            // and the dashboard so operators can see CC owner vs
            // on-air keying RID side-by-side.
            if let Some(a) = active.as_mut() {
                a.last_activity_ms = now_unix_ms();
                if let Some(s) = source {
                    let agrees_with_cc = match a.source {
                        Some(cc) => cc == s,
                        None => false, // no CC to compare
                    };
                    if a.source.is_none() {
                        a.source = Some(s);
                        emit_source_update(
                            tx, a.call_id, s,
                            SourceUpdateVia::Ldu1LcVote,
                        );
                    }
                    // Always update actual_speaker — this is the
                    // most-recent LDU1-LC-voted observation.
                    if a.actual_speaker != Some(s) {
                        a.actual_speaker = Some(s);
                        let _ = tx.send(CallTrackerEvent {
                            call_id: a.call_id,
                            timestamp_unix_ms: now_unix_ms(),
                            kind: CallTrackerEventKind::ActualSpeakerObserved {
                                speaker: s,
                                agrees_with_cc,
                            },
                        });
                    }
                }
            }
        }

        CallBoundaryKind::SpeakerEnd { source } => {
            if let Some(call) = active.take() {
                // Stamp source from the SpeakerEnd event when the
                // active call had None (e.g., MOT_TC BY: arrives for
                // a TG 302-style SRC=0 call that LDU1 LC voting
                // hadn't filled in yet).
                let final_source = call.source.or(source);
                let expected = forwarder
                    .frames_submitted.load(Ordering::Relaxed);
                if final_source != call.source {
                    if let Some(s) = final_source {
                        emit_source_update(
                            tx, call.call_id, s,
                            SourceUpdateVia::TdulcMotTc,
                        );
                    }
                }
                emit_close(tx, &call,
                    CloseReason::SpeakerEnd, final_source, expected);
            }
        }
    }
}

fn handle_audio(chunk: AudioChunk, active: &mut Option<ActiveCall>) {
    if let Some(a) = active.as_mut() {
        let now = now_unix_ms();
        a.last_activity_ms = now;
        if a.first_audio_at_unix_ms.is_none() {
            a.first_audio_at_unix_ms = Some(now);
        }
        // Cross-check: if the chunk's TG doesn't match the active call's
        // TG, we likely missed a TG change at the CC level. Ignore the
        // chunk for now — call_tracker is CC-authoritative.
        if chunk.talkgroup != a.tg && chunk.talkgroup != 0 {
            tracing::trace!(
                target: "p25_call_tracker",
                "audio chunk tg={} mismatch active tg={} — ignoring",
                chunk.talkgroup, a.tg,
            );
        }
    }
    // No active call → drop. Audio without a CC grant doesn't open a
    // call in this design. If this becomes a problem (came-up-mid-call
    // edge case where CC grant was FEC-rejected), revisit.
}

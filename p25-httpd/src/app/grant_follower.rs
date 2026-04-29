//! Grant Follower — single owner of the active P25 grant.
//!
//! Phase 2g (2026-04-25): consolidated module owning everything
//! "grant following" — the prior `app::follower` (CC GrantEvent →
//! traffic-chain retune, sticky-lock / monitor-list / encryption
//! gates) AND the prior `app::call_tracker` (CallBoundary →
//! CallTrackerEvent broadcast, ActiveCall lifecycle). One file,
//! one mental model.
//!
//! Layout
//! ------
//!
//! 1. **Cross-platform types** (top of file, no cfg):
//!    `CallTrackerEvent` + variants, `CloseReason`, `OpenReason`,
//!    `SourceUpdateVia`, `ActiveCallSnapshot`, `ActiveCallShared`,
//!    `new_event_tx`, `new_active_call_shared`. AppState +
//!    grant_stats + recorder import these.
//!
//! 2. **Lifecycle authority** (`spawn_call_lifecycle`, portable):
//!    Subscribes to `CallBoundary` + `AudioChunk`, owns
//!    `Option<ActiveCall>`, publishes `CallTrackerEvent`. Mirrors
//!    the active call into `ActiveCallShared` for HTTP pull-side
//!    reads. Handles HduStart-after-gap split logic, LDU1 LC
//!    voted-source filling, MotorolaTalkComplete BY: stamping,
//!    timeout sweep.
//!
//! 3. **Routing + chain control** (`spawn_grant_follower`,
//!    cfg(linux)): consumes `GrantEvent` from the CC decoder,
//!    applies the gates (monitor list, encryption, sticky lock,
//!    channel reuse, traffic_lock_freq), dispatches FPGA retunes
//!    via `fpga::IpCore`, refreshes ImbeForwarder atomics for the
//!    active call. Subscribes to `CallTrackerEvent::CallClose` to
//!    release the chain (force_idle TC, pause demod, clear atomics).
//!
//! Why two spawned tasks instead of one
//! ------------------------------------
//!
//! The lifecycle authority (#2) has no FPGA dependency and
//! benefits from being host-testable on Windows. The routing path
//! (#3) is intrinsically Linux-only because it drives the FPGA
//! traffic chain. Sharing this file makes the boundary obvious
//! (one re-export surface, one set of types) without forcing the
//! lifecycle into the cfg(linux) cage. Communication between #2
//! and #3 is via the existing `CallBoundary` and `CallTrackerEvent`
//! broadcast channels — same shapes pre-Phase-2g, just relocated.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::broadcast;

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio::{AudioChunk, CallBoundary, CallBoundaryKind, TerminatorKind};

// ── 2026-04-26 session-lifecycle refactor: constants ─────────────

/// Cadence of the periodic timeout / drain-elapsed sweep. 100 ms
/// gives sub-second responsiveness on terminator-driven close
/// (terminator + drain → finalize within drain_ms + 100 ms).
const TIMEOUT_TICK_MS: u64 = 100;

/// Hard timeout — `now() - last_audio_at_ms > HARD_TIMEOUT_MS`
/// while no terminator has fired. Covers two cases:
///   (a) session opened, chain never produced audio (encrypted,
///       PLL/AGC fail, freq mis-tuned).
///   (b) audio was flowing, stopped without a terminator landing.
///
/// 2026-04-26 first set to 5 s. On-target observation revealed
/// chain settle on a fresh retune can take 7+ s before the framer
/// produces a real HDU dispatch (false-positive NID heartbeats
/// happen earlier, but those don't trigger on_hdu — and our HDU
/// boundary handler only fires on real body parse).
/// `2026-04-26-not-followed-no-preempt` flash reproduced: TG 300
/// src 3400011 PTT, retune at 15:01:29.208, real HDU at
/// 15:01:36.120 (7 s settle). 5 s timeout closed the session at
/// 34.213, before any real audio landed. Subsequent 6 s of clear
/// audio dropped to no_active. Bumped to 10 s to cover worst-
/// observed settle. HDU/audio events refresh the timer, so a
/// healthy session never closes on this — only stuck ones do.
const HARD_TIMEOUT_MS: u64 = 10_000;

// Closing-state drain lives on the recorder side: once we emit
// CallClose, the recorder keeps the WAV open for ~2 s and routes
// any straggling chunks whose `captured_at_ms` is `<=` close_at_ms
// into it. Lifecycle emits CallClose synchronously on the
// terminator parse; close_at_ms == terminator_at_ms is carried in
// the event's `ended_unix_ms` field for the recorder to compare
// against.

/// 2026-04-27 dedup window for primary GRP_VCH_GRANT arrivals.
/// P25 broadcasts each grant 2-3× within a single TSDU
/// (TSBK1/TSBK2/TSBK3 packed) and may re-broadcast during the
/// call. Without dedup each TSBK fires its own synthetic emit
/// (for not_followed grants) or its own state mutation,
/// flooding `grant_stats` Recent Calls — observed 2026-04-27:
/// ENC GRANT spam rolling the 200-slot ring past clear-call
/// entries before recordings could pair by call_id, leaving
/// most clear-call recordings under "Recordings without a
/// matching grant".  200 ms covers the TSDU triplet (members
/// arrive within ~30 ms) without affecting genuinely distinct
/// calls (seconds apart). Applied at the entry point so the
/// followed (active+Bundle) and not_followed (synthetic-emit)
/// paths share the dedup behaviour.
const GRANT_DEDUP_MS: u64 = 200;

// ── Public types (cross-platform; consumed by AppState +
//    grant_stats + recorder + dashboard API) ────────────────────

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
        baseline_frames_submitted: u64,
    },
    SourceUpdate {
        new_source: u32,
        via: SourceUpdateVia,
    },
    ActualSpeakerObserved {
        speaker: u32,
        agrees_with_cc: bool,
    },
    CallClose {
        reason: CloseReason,
        final_source: Option<u32>,
        final_actual_speaker: Option<u32>,
        started_unix_ms: u64,
        ended_unix_ms: u64,
        first_audio_at_unix_ms: Option<u64>,
        first_hdu_at_unix_ms: Option<u64>,
        expected_submit_count: u64,
        /// 2026-04-26 session-lifecycle refactor: every distinct
        /// SRC observed during the session — primary GRANT.SRC,
        /// LDU1 LC voted SRC, TDULC MOT BY: — in insertion order.
        /// Recorder mirrors this onto the WAV's metadata so the
        /// dashboard can show "speakers heard" when bundling
        /// multiple PTTs in one session.
        sources_observed: Vec<u32>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OpenReason {
    /// First primary `GRP_VCH_GRANT` for a (TG, channel) tuple
    /// with no matching open session.
    CcGrant,
    /// Pre-empt: previous session was closed because a primary
    /// GRANT arrived on the same channel with a different TG.
    TgChange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceUpdateVia {
    CcRefresh,
    Ldu1LcVote,
    TdulcMotTc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    /// 5 s without an audio chunk — covers chain-never-decoded,
    /// audio-stopped, and missed-terminator. The lifecycle's only
    /// non-preemption close path. Operator-confirmed 2026-04-26:
    /// dispatcher-final PTTs don't emit TDULC_MOT, and TDULC
    /// terminators in general can fire mid-grant on multi-PTT
    /// sessions, so terminator-driven close was retired in favour
    /// of this single hard backstop.
    Timeout,
    /// Pre-empt: primary GRANT on same channel for a different TG.
    /// The only "real" close path during normal operation.
    TgChange,
    /// Boundary stream lagged — drop active to avoid stale state.
    StreamLag,
}

pub type CallTrackerEventTx = broadcast::Sender<CallTrackerEvent>;

pub fn new_event_tx() -> CallTrackerEventTx {
    broadcast::channel(64).0
}

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

// ── Lifecycle internals (portable) ───────────────────────────────

struct ActiveCall {
    call_id: u64,
    tg: u16,
    nac: u16,
    /// "Primary" source — the most-recently observed SRC. Backwards
    /// compat with downstream consumers that take a single ID.
    /// Multi-speaker sessions also populate `sources_observed`.
    source: Option<u32>,
    actual_speaker: Option<u32>,
    freq_hz: Option<u64>,
    channel: Option<String>,
    encrypted: bool,
    not_followed: Option<&'static str>,
    started_unix_ms: u64,
    #[allow(dead_code)]
    started_instant: Instant,
    first_audio_at_unix_ms: Option<u64>,
    first_hdu_at_unix_ms: Option<u64>,
    #[allow(dead_code)]
    baseline_frames_submitted: u64,
    /// Wall time of the most recent audio chunk seen for this
    /// call. Zero until the first chunk arrives. Drives the 5 s
    /// hard timeout.
    last_audio_at_ms: u64,
    /// Every distinct SRC observed during the session — primary
    /// GRANT.SRC, LDU1 LC voted SRC, TDULC MOT BY: — in insertion
    /// order. Deduped on push.
    sources_observed: Vec<u32>,
}

impl ActiveCall {
    fn observe_source(&mut self, src: u32) -> bool {
        if src == 0 {
            return false;
        }
        if self.sources_observed.contains(&src) {
            return false;
        }
        self.sources_observed.push(src);
        true
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mirror_active(
    active: &Option<ActiveCall>,
    shared: &ActiveCallShared,
    forwarder: &Arc<ImbeForwarder>,
) {
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
    // Phase 2h (2026-04-25): broadcast the active call_id to the
    // forwarder so every IMBE batch + every emitted AudioChunk gets
    // stamped with it. Recorder routes by chunk.call_id directly.
    //
    // 2026-04-26: when there's no active call (between calls), HOLD
    // the previous call_id instead of clearing to 0. Late chunks of
    // the closing call get tagged with the closing-call id and the
    // recorder's drain captures them (or mismatch-drops cleanly into
    // /dev/null if the drain has ended). Inter-call gap chunks also
    // carry the old id, so they CAN'T be defensively appended to the
    // next recording via the call_id=0 path — eliminating the cross-
    // contamination observed 2026-04-26 (rec 24 had 86 zero-callid
    // chunks = 1.72 s of OTHER speakers' audio glued in; rec 27 had
    // 141 = 2.82 s).
    //
    // The next CallOpen WILL update current_call_id to the new id
    // (the `Some(c)` branch below), so this only affects the
    // inter-call gap.
    if let Some(c) = active.as_ref() {
        forwarder
            .current_call_id
            .store(c.call_id, Ordering::Relaxed);
    }
    // else: leave current_call_id alone — it stays at the last
    // known call_id until the next CallOpen overwrites it.
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
            sources_observed: call.sources_observed.clone(),
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

/// 2026-04-26 session-lifecycle refactor: how a primary GRANT
/// arrival relates to the open session. Operator-confirmed: TG
/// change is the only split trigger. Encryption-flag changes on
/// the same TG never happen in real P25 traffic at this site —
/// each TG has a fixed enc state.
enum ArrivalDisposition {
    /// Same TG and freq as the open session — bundle (refresh +
    /// add new SRC if any).
    Bundle,
    /// Different freq from the open session — sticky-locked
    /// elsewhere; ignore.
    Ignore,
    /// Same channel, different TG — close current, open new.
    TgChange,
}

fn classify_cc_arrival(
    active: &ActiveCall,
    new_tg: u16,
    new_freq_hz: Option<u64>,
) -> ArrivalDisposition {
    if let (Some(a_freq), Some(n_freq)) = (active.freq_hz, new_freq_hz) {
        if a_freq != n_freq {
            return ArrivalDisposition::Ignore;
        }
    }
    if active.tg != new_tg {
        return ArrivalDisposition::TgChange;
    }
    ArrivalDisposition::Bundle
}

/// Lifecycle authority task. Subscribes to the `CallBoundary`
/// broadcast (CC arrivals from the grant follower, voice-frame
/// boundaries from `imbe_forwarder`) and the audio broadcast,
/// publishes `CallTrackerEvent` to subscribers. Portable.
pub fn spawn_call_lifecycle(
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
        // 2026-04-27 not_followed dedup. P25 broadcasts each
        // primary GRP_VCH_GRANT 2-3× within a TSDU and re-
        // broadcasts during the call, so one encrypted PTT
        // produces 5-10 GRANT TSBKs. Without dedup, each one
        // fires its own synthetic CallOpen+CallClose pair,
        // flooding `grant_stats` Recent Calls and rolling the
        // ring past clear-call entries before recordings can
        // pair by call_id. Track last emit per (tg, freq,
        // encrypted); skip if a duplicate arrives within
        // NOT_FOLLOWED_DEDUP_MS.
        let mut not_followed_dedup:
            std::collections::HashMap<(u16, Option<u64>, bool), u64>
            = std::collections::HashMap::new();
        let mut tick = tokio::time::interval(
            Duration::from_millis(TIMEOUT_TICK_MS),
        );
        tick.tick().await;

        loop {
            tokio::select! {
                recv = boundary_rx.recv() => {
                    let boundary = match recv {
                        Ok(b) => b,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!(
                                target: "p25_call_lifecycle",
                                "boundary lagged by {n} events; \
                                 dropping in-flight call to avoid \
                                 stale state",
                            );
                            if let Some(call) = active.take() {
                                let expected = forwarder
                                    .frames_submitted.load(Ordering::Relaxed);
                                emit_close(&tracker_tx, &call,
                                    CloseReason::StreamLag,
                                    call.source, expected);
                            }
                            mirror_active(&active, &active_call, &forwarder);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    handle_boundary(
                        boundary, &mut active, &mut next_call_id,
                        &tracker_tx, &forwarder,
                        &mut not_followed_dedup,
                    );
                    mirror_active(&active, &active_call, &forwarder);
                }
                recv = audio_rx.recv() => {
                    match recv {
                        Ok(chunk) => {
                            handle_audio(chunk, &mut active);
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                _ = tick.tick() => {
                    // 2026-04-26 session-lifecycle refactor: tick only
                    // covers the 5 s hard-timeout backstop. Terminators
                    // fire CallClose synchronously in handle_boundary;
                    // the closing-state drain lives on the recorder side.
                    let now = now_unix_ms();
                    let close_decision: Option<CloseReason> = active
                        .as_ref()
                        .and_then(|c| {
                            let last = if c.last_audio_at_ms != 0 {
                                c.last_audio_at_ms
                            } else {
                                c.started_unix_ms
                            };
                            if now.saturating_sub(last) > HARD_TIMEOUT_MS {
                                Some(CloseReason::Timeout)
                            } else {
                                None
                            }
                        });
                    if let Some(reason) = close_decision {
                        if let Some(call) = active.take() {
                            let expected = forwarder
                                .frames_submitted.load(Ordering::Relaxed);
                            emit_close(&tracker_tx, &call,
                                reason, call.source, expected);
                        }
                        mirror_active(&active, &active_call, &forwarder);
                    }
                }
            }
        }
        tracing::warn!(
            target: "p25_call_lifecycle",
            "call lifecycle task exiting (channels closed)",
        );
    });
}

fn handle_boundary(
    boundary: CallBoundary,
    active: &mut Option<ActiveCall>,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    forwarder: &Arc<ImbeForwarder>,
    grant_dedup: &mut std::collections::HashMap<(u16, Option<u64>, bool), u64>,
) {
    match boundary.kind {
        // 2026-04-26 session-lifecycle refactor: primary
        // GRP_VCH_GRANT (`is_update=false`). Open-or-bundle-or-
        // split, no audio-driven anything.
        CallBoundaryKind::CcGrantArrival {
            tg, source, freq_hz, channel, encrypted, not_followed,
        } => {
            let now = now_unix_ms();
            // 2026-04-27 entry-point dedup. Skip GRANTs arriving
            // within GRANT_DEDUP_MS of a prior identical (tg,
            // freq, encrypted) — kills the TSDU triplet
            // re-broadcasts (TSBK1/2/3 in one TSDU, ~30 ms apart)
            // before they fan out to either the followed
            // (active+Bundle) or not_followed (synthetic-emit)
            // paths. Genuinely distinct calls (>200 ms apart)
            // pass through unchanged.
            let dedup_key = (tg, freq_hz, encrypted);
            if let Some(&last) = grant_dedup.get(&dedup_key) {
                if now.saturating_sub(last) < GRANT_DEDUP_MS {
                    return;
                }
            }
            grant_dedup.insert(dedup_key, now);
            // Bound the dedup map. Distinct (tg, freq, enc)
            // tuples on a single P25 site are ~10. Prune entries
            // older than 60 s on every insert to keep the map
            // small without a separate GC tick.
            grant_dedup.retain(|_, &mut t| now.saturating_sub(t) < 60_000);

            let channel_str = if channel == 0 {
                None
            } else {
                Some(format!("{}", channel))
            };

            // Disposition decides whether this GRANT bundles into
            // the active session, ignores (sticky-locked elsewhere),
            // or pre-empts (TG change).
            //
            // 2026-04-26 not-followed-aware preempt: if the new
            // primary GRANT is `not_followed` (encrypted / sticky /
            // monitor-rejected), the follower won't retune the chain
            // for it. The chain keeps decoding whatever was on the
            // current freq. Pre-empting the active session in that
            // case attributes the chain's continued audio to a
            // session we're not actually serving. Confirmed on-target
            // 2026-04-26: TG 300 (clear) → TG 402 (ENC) GRANT on same
            // channel produced 459 IMBE attributed to TG 402 with
            // `not_followed=encrypted` (bug). Treat not_followed
            // grants as Ignore — leave the active session alone.
            let action = match active.as_ref() {
                // No active session, grant IS followable: open as
                // the new active. Recorder will create a recording.
                None if not_followed.is_none() =>
                    OpenAction::Open(OpenReason::CcGrant),
                // No active session, grant is NOT followed
                // (encrypted/sticky/monitor-rejected). Don't keep
                // it as the active session — that would put TG-402
                // ENC in /api/grants for 10 s of timeout window
                // even though we never actually followed it. Use
                // the synthetic-emit-only pattern: emit CallOpen
                // + CallClose for grant_stats visibility, leave
                // active=None.
                None => {
                    let now = now_unix_ms();
                    let synthetic_call_id = *next_call_id;
                    *next_call_id += 1;
                    let baseline = forwarder
                        .frames_submitted.load(Ordering::Relaxed);
                    emit_open(
                        tx, synthetic_call_id, tg, boundary.nac,
                        source, freq_hz, channel_str.clone(),
                        encrypted, not_followed,
                        OpenReason::CcGrant, baseline, now,
                    );
                    let mut synth_sources = Vec::new();
                    if let Some(s) = source { synth_sources.push(s); }
                    let synth_call = ActiveCall {
                        call_id: synthetic_call_id,
                        tg, nac: boundary.nac,
                        source, actual_speaker: None,
                        freq_hz, channel: channel_str.clone(),
                        encrypted, not_followed,
                        started_unix_ms: now,
                        started_instant: Instant::now(),
                        first_audio_at_unix_ms: None,
                        first_hdu_at_unix_ms: None,
                        baseline_frames_submitted: baseline,
                        last_audio_at_ms: 0,
                        sources_observed: synth_sources,
                    };
                    emit_close(
                        tx, &synth_call, CloseReason::Timeout,
                        source, baseline,
                    );
                    return;
                }
                // Active session, new GRANT is not_followed: leave
                // the active session alone but still emit a
                // synthetic CallOpen+CallClose pair so grant_stats
                // surfaces the not-followed grant in Recent Calls
                // (operator confirmed: every CC GRANT must be
                // visible). Recorder filters not_followed CallOpens
                // so no recording is opened. Confirmed on-target
                // 2026-04-26: TG 300 → TG 402 (ENC, same freq)
                // preempt produced 459 IMBE attributed to TG 402
                // with `not_followed=encrypted` (bug). Synthetic
                // emit preserves visibility without the misattribution.
                Some(_) if not_followed.is_some() => {
                    let now = now_unix_ms();
                    let synthetic_call_id = *next_call_id;
                    *next_call_id += 1;
                    let baseline = forwarder
                        .frames_submitted.load(Ordering::Relaxed);
                    emit_open(
                        tx, synthetic_call_id, tg, boundary.nac,
                        source, freq_hz, channel_str.clone(),
                        encrypted, not_followed,
                        OpenReason::CcGrant, baseline, now,
                    );
                    let mut synth_sources = Vec::new();
                    if let Some(s) = source { synth_sources.push(s); }
                    let synth_call = ActiveCall {
                        call_id: synthetic_call_id,
                        tg, nac: boundary.nac,
                        source, actual_speaker: None,
                        freq_hz, channel: channel_str.clone(),
                        encrypted, not_followed,
                        started_unix_ms: now,
                        started_instant: Instant::now(),
                        first_audio_at_unix_ms: None,
                        first_hdu_at_unix_ms: None,
                        baseline_frames_submitted: baseline,
                        last_audio_at_ms: 0,
                        sources_observed: synth_sources,
                    };
                    emit_close(
                        tx, &synth_call, CloseReason::Timeout,
                        source, baseline,
                    );
                    return;
                }
                Some(a) => match classify_cc_arrival(a, tg, freq_hz) {
                    ArrivalDisposition::Bundle => OpenAction::Bundle,
                    ArrivalDisposition::Ignore => OpenAction::None,
                    ArrivalDisposition::TgChange => OpenAction::Preempt(
                        CloseReason::TgChange, OpenReason::TgChange,
                    ),
                },
            };

            match action {
                OpenAction::None => return,
                OpenAction::Bundle => {
                    let a_mut = active.as_mut().unwrap();
                    if let Some(s) = source {
                        if a_mut.observe_source(s) {
                            emit_source_update(
                                tx, a_mut.call_id, s,
                                SourceUpdateVia::CcRefresh,
                            );
                        }
                        if a_mut.source.is_none() {
                            a_mut.source = Some(s);
                        }
                    }
                    if not_followed.is_some()
                        && a_mut.not_followed.is_none()
                    {
                        a_mut.not_followed = not_followed;
                    }
                    return;
                }
                OpenAction::Preempt(close_reason, _) => {
                    if let Some(prev) = active.take() {
                        let expected = forwarder
                            .frames_submitted.load(Ordering::Relaxed);
                        emit_close(tx, &prev, close_reason,
                                   prev.source, expected);
                    }
                }
                OpenAction::Open(_) => {}
            }

            let opened_via = match action {
                OpenAction::Open(r) => r,
                OpenAction::Preempt(_, r) => r,
                _ => OpenReason::CcGrant,
            };

            let call_id = *next_call_id;
            *next_call_id += 1;
            let baseline = forwarder
                .frames_submitted.load(Ordering::Relaxed);
            let mut sources_observed = Vec::new();
            if let Some(s) = source {
                sources_observed.push(s);
            }
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
                first_audio_at_unix_ms: None,
                first_hdu_at_unix_ms: None,
                baseline_frames_submitted: baseline,
                last_audio_at_ms: 0,
                sources_observed,
            });
            emit_open(
                tx, call_id, tg, boundary.nac, source, freq_hz,
                channel_str, encrypted, not_followed, opened_via,
                baseline, now,
            );
        }

        // GRP_VCH_GRNT_UPD: refresh-only. Does NOT carry SRC or
        // service_options — confirmed empirically 2026-04-26 in
        // the on-target log: every UPDATE shows source=None and
        // enc_flag pulled from per-channel history. Cannot signal
        // a speaker change, TG change, or encryption flip. Treat
        // as a pure keep-alive — the existing `last_audio_at_ms`
        // timeout is the lifeline anyway, but we still nudge it
        // here so a session that has CC keep-alive traffic but no
        // audio doesn't time out as fast as one with no CC at all.
        CallBoundaryKind::CcGrantUpdate { tg, .. } => {
            if let Some(a) = active.as_mut() {
                if a.tg == tg {
                    // No-op: the spec says UPDATE doesn't gate
                    // the timeout. last_audio_at_ms is the truth.
                    // Kept as a structural arm for log-readability.
                }
            }
        }

        // HDU: informational + activity refresh. Bundling speakers
        // means we no longer split per-PTT — multiple HDUs in one
        // session is expected (e.g. dispatcher then field radio on
        // same grant). 2026-04-26: HDU also refreshes
        // `last_audio_at_ms` so the 5 s hard timeout extends past
        // chain settle. Confirmed on-target: chain settle on
        // retune can take 6-9 s, but HDU dispatch (chain detected
        // a P25 voice frame start) is strong evidence the session
        // is alive even before LDUs land.
        CallBoundaryKind::HduStart => {
            let now = now_unix_ms();
            if let Some(a) = active.as_mut() {
                if a.first_hdu_at_unix_ms.is_none() {
                    a.first_hdu_at_unix_ms = Some(now);
                }
                if a.first_audio_at_unix_ms.is_none() {
                    a.first_audio_at_unix_ms = Some(now);
                }
                a.last_audio_at_ms = now;
                if a.nac == 0 && boundary.nac != 0 {
                    a.nac = boundary.nac;
                }
            }
        }

        // LDU1 LC voted SRC. Add to sources_observed; update
        // actual_speaker (may differ from CC.SRC — dispatcher-
        // grant → field-radio-FM mismatch is a known Clay County
        // pattern, not a bug).
        CallBoundaryKind::TdulcComplete { source } => {
            if let Some(a) = active.as_mut() {
                if let Some(s) = source {
                    let agrees_with_cc = match a.source {
                        Some(cc) => cc == s,
                        None => false,
                    };
                    if a.observe_source(s) {
                        emit_source_update(
                            tx, a.call_id, s,
                            SourceUpdateVia::Ldu1LcVote,
                        );
                    }
                    if a.source.is_none() {
                        a.source = Some(s);
                    }
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

        // 2026-04-26 session-lifecycle refactor (operator clarification):
        // we model GRANTS, not speakers. A grant can carry multiple
        // PTTs back-to-back (each emitting its own TDULC_MOT
        // BY:<field-radio> terminator). Closing on TDULC_MOT would
        // cut the grant after speaker 1 and miss speakers 2..N.
        // Closing on TDULC_CALL_TERM is also unsafe — within a
        // multi-PTT grant only the LAST PTT typically carries it,
        // but BCH false-positives can produce phantom CALL_TERMs
        // mid-grant.
        //
        // So all SpeakerEnd flavours are SOURCE-STAMP-ONLY here.
        // The session closes only on TG-change-via-new-GRANT
        // (handled in the CcGrantArrival arm) or the 5 s no-audio
        // hard timeout (handled in the periodic tick).
        CallBoundaryKind::SpeakerEnd { source, kind: _ } => {
            if let Some(a) = active.as_mut() {
                if let Some(s) = source {
                    if a.observe_source(s) {
                        emit_source_update(
                            tx, a.call_id, s,
                            SourceUpdateVia::TdulcMotTc,
                        );
                    }
                    if a.source.is_none() {
                        a.source = Some(s);
                    }
                }
            }
        }
    }
}

/// 2026-04-26 session-lifecycle refactor: encodes the four ways a
/// `CcGrantArrival` can affect the active session.
enum OpenAction {
    /// No active session — open new with the given reason.
    Open(OpenReason),
    /// Active session matches (TG, enc, channel) — refresh in
    /// place + add SRC to sources_observed.
    Bundle,
    /// Active session was on a different TG or different enc on
    /// the same channel — close it (with the given reason),
    /// then open new (with the given OpenReason).
    Preempt(CloseReason, OpenReason),
    /// Active session is sticky-locked elsewhere — drop the
    /// arrival.
    None,
}

fn handle_audio(chunk: AudioChunk, active: &mut Option<ActiveCall>) {
    if let Some(a) = active.as_mut() {
        // The recorder routes by chunk.captured_at_ms vs the
        // session window. The lifecycle layer only needs to know
        // "audio is still flowing" for the hard-timeout gate.
        let now = now_unix_ms();
        a.last_audio_at_ms = now;
        if a.first_audio_at_unix_ms.is_none() {
            a.first_audio_at_unix_ms = Some(now);
        }
        if chunk.talkgroup != a.tg && chunk.talkgroup != 0 {
            tracing::trace!(
                target: "p25_call_lifecycle",
                "audio chunk tg={} mismatch active tg={} — ignoring",
                chunk.talkgroup, a.tg,
            );
        }
    }
}

// ── Routing + chain control (Linux-only) ─────────────────────────

#[cfg(target_os = "linux")]
mod routing {

use std::sync::atomic::{AtomicBool, AtomicI64};

use tokio::sync::{Mutex, RwLock};
use tokio::sync::mpsc::Receiver;

use super::{Arc, CallTrackerEventKind, CallTrackerEventTx};
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio;
use crate::hardware::fpga;
use crate::protocol::p25::{self, control_channel::ControlChannelDecoder,
    traffic_chain::TrafficChain};
use crate::services::event_log::EventLog;
use crate::services::monitor::MonitorList;

#[allow(clippy::too_many_arguments)]
pub fn spawn_grant_follower(
    follower_mgr: Arc<Mutex<TrafficChain>>,
    follower_core: Arc<Mutex<fpga::IpCore>>,
    follower_current_sample_rate_hz: Arc<std::sync::atomic::AtomicU32>,
    follower_current_rx_lo: Arc<AtomicI64>,
    follower_lo_ppm: f64,
    follower_enabled: Arc<AtomicBool>,
    follower_imbe: Arc<ImbeForwarder>,
    follower_monitor: Arc<RwLock<MonitorList>>,
    follower_event_log: Arc<EventLog>,
    follower_traffic_decoder: Arc<RwLock<ControlChannelDecoder>>,
    mut grant_event_rx: Receiver<p25::events::P25Event>,
    follower_lock_freq: Arc<AtomicBool>,
    follower_boundary_tx: audio::CallBoundaryTx,
    follower_tracker_tx: CallTrackerEventTx,
) {
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            tracing::info!(
                "traffic grant follower task started \
                 (grant events + CallTrackerEvent::CallClose)"
            );
            // Phase 2c (2026-04-25): replaced the 200 ms timeout-tick
            // poller (which called `mgr.check_timeouts()`) with a
            // CallTrackerEvent subscription. CallClose drives release;
            // CallTracker's timeout sweep is the upstream timer.
            let mut tracker_rx = follower_tracker_tx.subscribe();

            // Process a grant event; returns true if a retune was performed.
            //
            // Sticky-lock policy (from SDRTrunk PR #2010):
            // - If locked on a TG, only accept grants for that TG.
            // - If Idle, accept according to monitor list priority
            //   (or newest if monitor list is empty).
            let handle_grant_event =
                |g: &p25::events::GrantEvent,
                 mgr: &mut p25::traffic_chain::TrafficChain,
                 imbe: &ImbeForwarder| -> bool
            {
                let freq_hz = match g.frequency_hz {
                    Some(f) => f,
                    None => return false,
                };
                let retune = mgr.handle_grant(g.channel, g.talkgroup, freq_hz);

                // Only update the ImbeForwarder's active-call atomics
                // (current_talkgroup, current_source, call_encrypted)
                // when this grant is for the call we're following.
                // Otherwise a grant for TG 700 [ENC] arriving while
                // sticky-locked on unencrypted TG 300 would set
                // call_encrypted=true on the TG 300 path and IMBE
                // frames would be encryption-skipped. Gate on:
                //   - retune=true: we just switched to this TG.
                //   - mgr.current_talkgroup() == Some(g.tg): grant is
                //     a refresh for the already-active call.
                let grant_is_for_active = retune
                    || mgr.current_talkgroup() == Some(g.talkgroup);

                // Determine encryption: check the grant flag, then
                // fall back to TG history. History is updated on every
                // grant regardless of whether we follow it, so the
                // encrypted_tgs blocklist learns about TG 700 being
                // encrypted even while we stay locked on TG 300.
                let is_enc = if g.encrypted {
                    if let Ok(mut hist) = imbe.encrypted_tg_history.lock() {
                        hist.insert(g.talkgroup.0);
                    }
                    true
                } else {
                    imbe.encrypted_tg_history.lock()
                        .map(|h| h.contains(&g.talkgroup.0))
                        .unwrap_or(false)
                };

                if grant_is_for_active {
                    imbe.current_talkgroup
                        .store(g.talkgroup.0, Ordering::Relaxed);
                    // Stash the grant's FM:<source> so the recorder
                    // can stamp filenames from the CONTROL channel.
                    // Only overwrite on a genuine active-call grant.
                    if let Some(src) = g.source {
                        if src.0 != 0 {
                            imbe.current_source
                                .store(src.0, Ordering::Relaxed);
                        }
                    }
                    // 2026-04-24: tag traffic-channel freq/channel
                    // so RecordingEntry + GrantDecodeSummary can
                    // attribute calls per-channel for cross-channel
                    // quality diffs.
                    if let Some(f) = g.frequency_hz {
                        imbe.current_frequency_hz
                            .store(f, Ordering::Relaxed);
                    }
                    if let Ok(mut s) = imbe.current_channel.lock() {
                        *s = format!("{}", g.channel);
                    }
                    if retune {
                        // New call: set encryption and reset vocoder
                        imbe.call_encrypted.store(is_enc, Ordering::Relaxed);
                        imbe.vocoder_reset_pending
                            .store(true, Ordering::Relaxed);
                    } else if is_enc {
                        // Sticky-true within an active call
                        imbe.call_encrypted.store(true, Ordering::Relaxed);
                    }
                    // Note: we deliberately do NOT set call_encrypted
                    // = false on a grant refresh where g.encrypted ==
                    // false. The flag is cleared only on Idle.
                }
                retune
            };

            // Activity log is DELIBERATELY NOT deduped: every
            // `P25Event::Grant` arrival gets its own line, even
            // back-to-back grants on the same (TG, channel, freq)
            // packed into one 3-TSBK TSDU. The correct-handling
            // invariant lives in `TrafficChain::handle_grant`:
            // the `same_tg_same_freq` branch at
            // traffic_chain.rs:258 short-circuits with
            // `return false` (no retune, no second transition)
            // whenever the grant matches the current lock, and
            // Acquiring->Active auto-promotes on that same branch.

            // 2026-04-24 CC-grant-centric refactor: fire one
            // `CallBoundaryKind::CcGrantArrival` (or `CcGrantUpdate`)
            // per incoming grant event. Full grants carry the
            // `not_followed` rejection reason (None = accepted);
            // Updates are keep-alives that only refresh ttl on an
            // existing OpenGrant and ignore not_followed. grant_stats
            // owns the OpenGrant lifecycle — follower just reports
            // facts. `nac` is stubbed to 0 (CC-side NAC isn't
            // plumbed into GrantEvent; traffic heartbeat's HduStart
            // enriches OpenGrant with the real NAC once the chain
            // locks).
            let send_cc_boundary =
                |g: &p25::events::GrantEvent,
                 not_followed: Option<&'static str>| {
                let kind = if g.is_update {
                    audio::CallBoundaryKind::CcGrantUpdate {
                        tg: g.talkgroup.0,
                        freq_hz: g.frequency_hz,
                        channel: g.channel.0,
                    }
                } else {
                    audio::CallBoundaryKind::CcGrantArrival {
                        tg: g.talkgroup.0,
                        // 2026-04-25: map RadioId(0) → None. Some sites
                        // emit `GRP_VCH_GRANT TG:NNN SRC:00000` on the
                        // wire (observed for TG 302 on Clay County
                        // 8A1) — these are "CC announced the call but
                        // didn't tell us who", semantically equivalent
                        // to a GRP_VCH_GRNT_UPD that doesn't carry a
                        // source. Treat as None so downstream LDU1 LC
                        // FM: enrichment can fill in (when we follow
                        // the call) and the dashboard renders a clean
                        // "--" instead of misleading "0".
                        source: g.source.and_then(|r| {
                            if r.0 != 0 { Some(r.0) } else { None }
                        }),
                        freq_hz: g.frequency_hz,
                        channel: g.channel.0,
                        encrypted: g.encrypted,
                        not_followed,
                    }
                };
                let _ = follower_boundary_tx.send(audio::CallBoundary {
                    kind,
                    nac: 0,
                    talkgroup: Some(g.talkgroup.0),
                    expected_submit_count: 0,
                });
            };

            loop {
                tokio::select! {
                    event = grant_event_rx.recv() => {
                        let event = match event {
                            Some(e) => e,
                            None => break, // channel closed
                        };

                        if !follower_enabled.load(Ordering::Relaxed) {
                            continue;
                        }

                        match event {
                            p25::events::P25Event::Grant(g) => {
                                use crate::services::event_log::LogCategory;
                                let freq_mhz = g.frequency_hz
                                    .map(|f| f as f64 / 1e6)
                                    .unwrap_or(0.0);

                                // Eager history populate: any
                                // encrypted=true grant adds the TG to
                                // the persistent history before any
                                // gate check runs. Otherwise a site
                                // that sometimes-sets / sometimes-
                                // doesn't set service_options would
                                // let us retune to the same encrypted
                                // TG multiple times before history
                                // caught up. First encrypted=true
                                // observation for a TG permanently
                                // blocks all subsequent grants for it.
                                if g.encrypted {
                                    if let Ok(mut hist) =
                                        follower_imbe
                                            .encrypted_tg_history
                                            .lock()
                                    {
                                        hist.insert(g.talkgroup.0);
                                    }
                                }

                                // 2026-04-25: only log NEW grants, not
                                // GVCG_UPDATE keep-alives. The
                                // pre-2026-04-25 unconditional log
                                // produced ~30 entries/sec from CC
                                // refreshes for a single stuck call,
                                // which pushed real call activity out
                                // of the 1000-entry log ring within
                                // seconds (TG 600 had 68 entries vs
                                // TG 300 having zero in a 80-entry
                                // sample). is_update=true comes from
                                // GVCG_UPDATE / GVCG_UPDATE_EXP
                                // refresh TSBKs; is_update=false is
                                // only the initial GVCG / GVCG_EXP
                                // for a new call. One log entry per
                                // call instead of one per refresh.
                                if !g.is_update {
                                    follower_event_log.push(
                                        LogCategory::Grant,
                                        format!(
                                            "grant TG={} ch={} {:.4} MHz src={}{}",
                                            g.talkgroup.0,
                                            g.channel.0,
                                            freq_mhz,
                                            g.source.map(|r| r.0).unwrap_or(0),
                                            if g.encrypted { " [ENC]" } else { "" },
                                        ),
                                        serde_json::json!({
                                            "tg":        g.talkgroup.0,
                                            "channel":   g.channel.0,
                                            "frequency": g.frequency_hz,
                                            "src":       g.source.map(|r| r.0),
                                            "encrypted": g.encrypted,
                                            "emergency": g.emergency,
                                            "is_update": false,
                                        }),
                                    );
                                }

                                // 2026-04-26 GVCG_UPD fast-path. CC
                                // pumps GRP_VCH_GRNT_UPD at ~10-30/sec
                                // per active call as a keep-alive; the
                                // pre-2026-04-26 path ran every update
                                // through the full lock chain
                                // (mgr.lock().await, decoder.write().
                                // await, core.lock().await, ±2 ms FIR
                                // sleep on retune) which serialised
                                // every grant. Field observation: a
                                // single GRP_VCH_GRANT for TG 300
                                // landed 76 log events (≈ 1.4 s) after
                                // the parser had already extracted it
                                // from the CC dibits — the follower
                                // task was draining queued updates.
                                //
                                // This branch fires CcGrantUpdate
                                // without acquiring any of the heavy
                                // locks when the chain is already on
                                // the TG. Lifecycle's
                                // CallBoundaryKind::CcGrantUpdate
                                // handler refreshes last_activity_ms
                                // and returns. Initial GRANTs (and
                                // updates that disagree with the
                                // current lock) still take the full
                                // path below.
                                if g.is_update {
                                    let mgr = follower_mgr.lock().await;
                                    let chain_tg = mgr.current_talkgroup();
                                    drop(mgr);
                                    if chain_tg.map(|t| t.0 == g.talkgroup.0)
                                        .unwrap_or(false)
                                    {
                                        send_cc_boundary(&g, None);
                                        continue;
                                    }
                                }

                                // Tally every observed grant into the
                                // persistent frequency map regardless
                                // of follow decision; populates
                                // /api/grant_map for scanner-mode UI
                                // and future LO auto-center.
                                if let Some(freq) = g.frequency_hz {
                                    let mut mgr = follower_mgr.lock().await;
                                    mgr.tally_grant(
                                        g.talkgroup.0, freq, g.encrypted);
                                }

                                // Monitor list gate
                                let dominated = {
                                    let monitor = follower_monitor.read().await;
                                    if monitor.is_empty() {
                                        true // accept all
                                    } else {
                                        monitor.contains(g.talkgroup.0)
                                    }
                                };
                                if !dominated {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject: TG={} not in monitor list",
                                            g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "tg":     g.talkgroup.0,
                                            "reason": "monitor_list",
                                        }),
                                    );
                                    send_cc_boundary(&g, Some("monitor_list"));
                                    continue;
                                }

                                let mut mgr = follower_mgr.lock().await;
                                let locked_tg = mgr.current_talkgroup();
                                let locked_ch = mgr.current_channel();

                                // Channel-reuse detection: if the
                                // grant's channel matches our current
                                // lock but the TG differs, the trunking
                                // system has reassigned our voice
                                // channel to a different TG and the old
                                // call is done. Tear down so we can
                                // follow the new TG (subject to the
                                // gates below). Without this the
                                // sticky-lock reject would keep us on
                                // a dead channel for up to
                                // call_timeout_ms (2 s).
                                // 2026-04-25 Fix B: extend channel-reuse
                                // detection to also fire on same-FREQ-
                                // different-channel-id. Different TGs
                                // can be assigned to the same physical
                                // freq via different channel-id values
                                // depending on which IDEN_UPDATE band
                                // record the trunking system was using
                                // — observed on Clay County NAC 8A1
                                // where TG 302 and TG 1210 both transmit
                                // on 857.9875 MHz but the chain might
                                // be sticky-locked on TG 302's channel-
                                // id (e.g., 0-1189 from a prior call on
                                // 858.4375 MHz) when a TG 1210 grant
                                // for 0-1117 on 857.9875 arrives.
                                //
                                // Original channel-id-only check would
                                // miss this case → sticky_lock rejects
                                // the new grant → chain doesn't follow
                                // TG 1210 → no recordings, but the chain
                                // continues to decode whatever audio is
                                // on its current freq.
                                //
                                // Adding the same-freq fallback closes
                                // the gap. When the chain's actual freq
                                // (current_frequency_hz) matches the new
                                // grant's freq_hz, treat it as channel-
                                // reuse: tear down old, accept new. No
                                // retune happens because freq is already
                                // correct; just TG/source attribution
                                // updates.
                                let chan_id_match = locked_ch
                                    .map(|c| c == g.channel)
                                    .unwrap_or(false);
                                let parked_freq = follower_imbe
                                    .current_frequency_hz
                                    .load(Ordering::Relaxed);
                                let same_freq_diff_chan = parked_freq != 0
                                    && g.frequency_hz == Some(parked_freq);
                                let is_channel_reuse =
                                    (chan_id_match || same_freq_diff_chan)
                                    && locked_tg.map(|t| t.0 != g.talkgroup.0).unwrap_or(false);
                                if is_channel_reuse {
                                    let prev_tg = locked_tg.map(|t| t.0).unwrap_or(0);
                                    let reuse_reason = if chan_id_match {
                                        "channel_reuse"
                                    } else {
                                        "same_freq_reuse"
                                    };
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "channel reuse: ch={} was TG={}, now TG={} ({}) — teardown",
                                            g.channel, prev_tg, g.talkgroup.0, reuse_reason,
                                        ),
                                        serde_json::json!({
                                            "channel":     format!("{}", g.channel),
                                            "prev_tg":     prev_tg,
                                            "new_tg":      g.talkgroup.0,
                                            "parked_freq": parked_freq,
                                            "grant_freq":  g.frequency_hz,
                                            "reason":      reuse_reason,
                                        }),
                                    );
                                    mgr.force_idle();
                                    follower_imbe.current_talkgroup
                                        .store(0, Ordering::Relaxed);
                                    // Fall through — re-evaluate the
                                    // grant as if Idle. Encrypted +
                                    // sticky-lock checks below now see
                                    // locked_tg = None and proceed.
                                }
                                // Re-read after the possible force_idle.
                                let locked_tg = mgr.current_talkgroup();

                                // Encrypted check runs BEFORE the
                                // sticky-lock check. Order matters: a
                                // TG 406 [ENC] grant arriving while
                                // locked on TG 301 must reach the
                                // encrypted gate so encrypted_tg_history
                                // learns TG 406; otherwise if we later
                                // went Idle and TG 406 re-emitted with
                                // a flipped service-options byte
                                // (FEC-marginal), we'd accept it. This
                                // ordering also keeps the
                                // grants_rejected_encrypted stat
                                // accurate and names the correct reason
                                // in the log line.
                                let tg_known_enc = follower_imbe
                                    .encrypted_tg_history
                                    .lock()
                                    .map(|h| h.contains(&g.talkgroup.0))
                                    .unwrap_or(false);
                                if g.encrypted || tg_known_enc {
                                    if g.encrypted {
                                        if let Ok(mut hist) =
                                            follower_imbe
                                                .encrypted_tg_history
                                                .lock()
                                        {
                                            hist.insert(g.talkgroup.0);
                                        }
                                    }
                                    mgr.grants_rejected_encrypted += 1;

                                    // If the encrypted TG is our
                                    // current lock, tear down
                                    // synchronously — otherwise the
                                    // sticky 2 s timeout holds the
                                    // slot until the call ends
                                    // naturally.
                                    let was_locked = locked_tg
                                        .map(|t| t.0 == g.talkgroup.0)
                                        .unwrap_or(false);
                                    if was_locked {
                                        mgr.force_idle();
                                        drop(mgr);
                                        // Zero current_talkgroup so
                                        // the vocoder task's TG-change
                                        // auto-flush fires and closes
                                        // the call summary.
                                        follower_imbe.current_talkgroup
                                            .store(0, Ordering::Relaxed);
                                        // DO NOT clear call_encrypted
                                        // here. Buffered LDU dibits
                                        // from the previous channel
                                        // are still in flight in the
                                        // DMA ring + mpsc channel;
                                        // clearing the flag would let
                                        // the vocoder DECODE those
                                        // encrypted LDUs as clear and
                                        // produce garbled output.
                                        // Leaving it `true` keeps the
                                        // skip path active until the
                                        // next retune (which
                                        // unconditionally writes
                                        // call_encrypted =
                                        // new_grant.is_enc).
                                        #[cfg(target_os = "linux")]
                                        {
                                            let core = follower_core
                                                .lock().await;
                                            // Quiesce both LSM + C4FM
                                            // chains on encrypted
                                            // teardown so the traffic
                                            // LSM demod stops emitting
                                            // phantom NID events until
                                            // the next grant.
                                            core.pause_traffic_chain();
                                        }
                                        // Reset the traffic framer --
                                        // it's mid-frame on encrypted
                                        // data and would carry bogus
                                        // state into the next lock.
                                        {
                                            let mut dec = follower_traffic_decoder
                                                .write().await;
                                            dec.reset_framer_state();
                                        }
                                    }

                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject TG={} encrypted{}",
                                            g.talkgroup.0,
                                            if was_locked {
                                                " (tore down active lock)"
                                            } else { "" },
                                        ),
                                        serde_json::json!({
                                            "tg":         g.talkgroup.0,
                                            "enc_flag":   g.encrypted,
                                            "in_history": tg_known_enc,
                                            "was_locked": was_locked,
                                            "reason":     "encrypted",
                                        }),
                                    );
                                    send_cc_boundary(&g, Some("encrypted"));
                                    continue;
                                }

                                // Sticky-lock check runs AFTER the
                                // channel-reuse + encrypted gates. A
                                // grant for a different TG on a
                                // different channel is an unrelated
                                // call; stay on the current lock.
                                //
                                // 2026-04-26: log the rejected grant's
                                // freq + source + parked_freq so the
                                // operator can audit which TG was lost
                                // and whether it was on a different
                                // physical channel (real preempt cost)
                                // or the same channel (channel-reuse
                                // detection should have caught it —
                                // missing log = missing-data bug).
                                let locked_tg_final = mgr.current_talkgroup();
                                if let Some(tg) = locked_tg_final {
                                    if tg.0 != g.talkgroup.0 {
                                        let parked_freq = follower_imbe
                                            .current_frequency_hz
                                            .load(Ordering::Relaxed);
                                        let same_freq = parked_freq != 0
                                            && g.frequency_hz == Some(parked_freq);
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "reject: TG={} src={} freq={} \
                                                 (sticky-locked on TG={} \
                                                 parked_freq={}{})",
                                                g.talkgroup.0,
                                                g.source.map(|r| r.0).unwrap_or(0),
                                                g.frequency_hz.unwrap_or(0),
                                                tg.0, parked_freq,
                                                if same_freq {
                                                    " — SAME FREQ, channel-reuse miss?"
                                                } else { "" },
                                            ),
                                            serde_json::json!({
                                                "tg":          g.talkgroup.0,
                                                "src":         g.source.map(|r| r.0),
                                                "grant_freq":  g.frequency_hz,
                                                "locked_tg":   tg.0,
                                                "parked_freq": parked_freq,
                                                "same_freq":   same_freq,
                                                "reason":      "sticky_lock",
                                            }),
                                        );
                                        send_cc_boundary(&g, Some("sticky_lock"));
                                        continue;
                                    }
                                }

                                // Diagnostic lock: when on, suppress
                                // retunes so the chain stays parked
                                // on a known-active channel, but
                                // STILL process grants whose freq
                                // matches the parked freq so the
                                // call state machine fires (TG/source
                                // get stamped, IMBE flows into the
                                // recorder, audio gets decoded).
                                //
                                // 2026-04-25 fix: previously this
                                // gate skipped EVERY grant
                                // unconditionally — meaning lock=on
                                // also stopped following the very
                                // calls the operator wanted to
                                // measure on the parked freq. Now we
                                // only skip grants that would require
                                // a retune (different freq from
                                // current_frequency_hz). Same-freq
                                // grants fall through and process
                                // normally; handle_grant_event sees
                                // the matching freq and returns
                                // retune=false so no actual retune
                                // dispatch happens.
                                if follower_lock_freq.load(Ordering::Relaxed) {
                                    let parked_freq = follower_imbe
                                        .current_frequency_hz
                                        .load(Ordering::Relaxed);
                                    let grant_freq = g.frequency_hz
                                        .unwrap_or(0);
                                    let same_freq = parked_freq != 0
                                        && grant_freq != 0
                                        && grant_freq == parked_freq;
                                    if !same_freq {
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "lock: skip grant TG={} SRC={} \
                                                 ch={} freq={} (parked at {})",
                                                g.talkgroup.0,
                                                g.source.map(|s| s.0).unwrap_or(0),
                                                g.channel,
                                                grant_freq,
                                                parked_freq,
                                            ),
                                            serde_json::json!({
                                                "tg":          g.talkgroup.0,
                                                "channel":     format!("{}", g.channel),
                                                "grant_freq":  grant_freq,
                                                "parked_freq": parked_freq,
                                                "reason":      "traffic_lock",
                                            }),
                                        );
                                        send_cc_boundary(&g, Some("traffic_lock"));
                                        continue;
                                    }
                                    // Same freq as parked — fall through.
                                    // handle_grant_event will return
                                    // retune=false (TrafficChain sees
                                    // matching channel/freq), the imbe
                                    // atomics get updated, the call state
                                    // advances, and the recorder /
                                    // grant_stats see proper events.
                                }
                                // Accepted — about to evaluate handle_grant_event.
                                // Emit CcGrantArrival (or Update) with not_followed=None
                                // so grant_stats can open / refresh the OpenGrant. Kept
                                // here instead of pre-gates so we only fire for grants
                                // that actually proceed to state-machine evaluation.
                                send_cc_boundary(&g, None);
                                let pre_state = mgr.state_label();
                                let retune = handle_grant_event(
                                    &g, &mut mgr, &follower_imbe
                                );
                                let post_state = mgr.state_label();
                                drop(mgr);

                                if pre_state != post_state {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "state {} -> {} TG={}",
                                            pre_state, post_state, g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "from": pre_state,
                                            "to":   post_state,
                                            "tg":   g.talkgroup.0,
                                        }),
                                    );
                                }

                                if retune {
                                    let freq_hz = g.frequency_hz.unwrap();
                                    // PPM correction matching the control
                                    // DDC path in get_reinit(). Cancels
                                    // the Pluto crystal trim error (a
                                    // few hundred Hz depending on lo_ppm
                                    // and rx_lo) so the traffic PLL
                                    // doesn't sit at a residual steady-
                                    // state phase error on every call.
                                    //
                                    // rx_lo + sample rate read fresh
                                    // (not captured at spawn) so offset
                                    // math follows live LO or preset
                                    // changes (POST /api/preset moves
                                    // sample rate; POST /api/tune moves
                                    // rx_lo in Auto mode).
                                    let rx_lo_now = follower_current_rx_lo
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let sample_rate_now =
                                        follower_current_sample_rate_hz
                                            .load(std::sync::atomic::Ordering::Relaxed)
                                            as f64;
                                    let nco_lo_shift_hz =
                                        -follower_lo_ppm * 1e-6
                                            * rx_lo_now as f64;
                                    let offset_hz = (freq_hz as f64
                                        - rx_lo_now as f64
                                        + nco_lo_shift_hz)
                                        as i64;

                                    // Reset the traffic-side framer
                                    // BEFORE the DDC retune so dibits
                                    // from the new frequency aren't
                                    // consumed while the framer is
                                    // mid-state on stale data.
                                    // Preserves cumulative counters.
                                    {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }

                                    let core = follower_core.lock().await;
                                    // Atomic freeze-reset-thaw:
                                    // `retune_traffic_chain` disables
                                    // both LSM and C4FM chains, writes
                                    // the new DDC frequency, pulses
                                    // `traffic_lsm_reset` (clearing the
                                    // PLL accumulator and upstream
                                    // state), then re-enables. Post-
                                    // retune PLL starts from 0 and
                                    // converges in ~50-100 ms instead
                                    // of carrying stale phase from the
                                    // previous carrier. See
                                    // doc/changes/038.
                                    let agc_seed = follower_imbe
                                        .agc_seed_for_freq(freq_hz)
                                        .unwrap_or(0);
                                    match core.retune_traffic_chain(
                                        offset_hz as f64,
                                        sample_rate_now,
                                        agc_seed,
                                    ) {
                                        Ok(diag) => {
                                            tracing::info!(
                                                target: "p25_traffic",
                                                "retune: TG={} channel={:?} \
                                                 freq={} Hz offset={:+} Hz \
                                                 (LSM freeze-reset-thaw, framer reset)",
                                                g.talkgroup.0, g.channel,
                                                freq_hz, offset_hz,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune TG={} -> {:.4} MHz (offset {:+} Hz, agc_seed={})",
                                                    g.talkgroup.0,
                                                    freq_hz as f64 / 1e6,
                                                    offset_hz,
                                                    agc_seed,
                                                ),
                                                serde_json::json!({
                                                    "tg":          g.talkgroup.0,
                                                    "channel":     g.channel.0,
                                                    "frequency":   freq_hz,
                                                    "offset_hz":   offset_hz,
                                                    "framer_reset": true,
                                                    "lsm_reset":   true,
                                                    "pll_seed_written":      diag.pll_seed_written,
                                                    "traffic_pll_pre_reset": diag.traffic_pll_pre_reset,
                                                    "traffic_pll_post_reset": diag.traffic_pll_post_reset,
                                                    "seed_drift":            diag.drift(),
                                                    "seed_load_buggy":       diag.looks_buggy(),
                                                    "agc_seed_written":      diag.agc_seed_written,
                                                    "traffic_agc_pre_reset": diag.traffic_agc_pre_reset,
                                                    "traffic_agc_post_reset": diag.traffic_agc_post_reset,
                                                    "agc_drift":             diag.agc_drift(),
                                                    "agc_cache_hit":         agc_seed != 0,
                                                }),
                                            );
                                            if diag.looks_buggy() {
                                                follower_event_log.push(
                                                    LogCategory::Traffic,
                                                    format!(
                                                        "PLL seed-load mismatch on retune: \
                                                         seed={} pre={} post={} drift={}",
                                                        diag.pll_seed_written,
                                                        diag.traffic_pll_pre_reset,
                                                        diag.traffic_pll_post_reset,
                                                        diag.drift(),
                                                    ),
                                                    serde_json::json!({
                                                        "tg":   g.talkgroup.0,
                                                        "kind": "pll_seed_load_mismatch",
                                                        "pll_seed_written":      diag.pll_seed_written,
                                                        "traffic_pll_pre_reset": diag.traffic_pll_pre_reset,
                                                        "traffic_pll_post_reset": diag.traffic_pll_post_reset,
                                                        "seed_drift":            diag.drift(),
                                                    }),
                                                );
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "p25_traffic",
                                                "traffic DDC retune failed: \
                                                 TG={} freq={} Hz \
                                                 offset={:+} Hz: {}",
                                                g.talkgroup.0, freq_hz,
                                                offset_hz, e,
                                            );
                                            follower_event_log.push(
                                                LogCategory::Traffic,
                                                format!(
                                                    "retune FAILED TG={}: {}",
                                                    g.talkgroup.0, e,
                                                ),
                                                serde_json::json!({
                                                    "tg":     g.talkgroup.0,
                                                    "error":  e.to_string(),
                                                }),
                                            );
                                        }
                                    }
                                } else if pre_state == "Idle" && post_state != "Idle" {
                                    // Phase 2f NCO write-skip path: same
                                    // freq as the previous call, so the
                                    // DDC NCO + FIR can stay as-is (no
                                    // 2 ms FIR-flush sleep, no DDC
                                    // write). But the LSM chain has
                                    // been processing noise during the
                                    // inter-call gap, and the Costas
                                    // PLL random-walks on noise — by
                                    // grant time it's drifted away
                                    // from the converged value. So we
                                    // still need to seed the PLL/AGC
                                    // from the control chain and pulse
                                    // reset to load them. This is the
                                    // 2026-04-26 follow-up to the
                                    // seeding work: every grant gets a
                                    // fresh seeded start, only true
                                    // retunes pay the DDC-flush cost.
                                    // See doc/diagnostics/2026-04-25/
                                    // CHANNELIZER_REDESIGN.md.
                                    {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }
                                    let nco_skip_diag: Option<(i16, i16, i16)>;
                                    {
                                        // 2026-04-26: AGC seeding
                                        // disabled — see retune_traffic_chain
                                        // in fpga.rs for the full
                                        // rationale. Field observation
                                        // showed control-chain gain of
                                        // ~33× saturated the traffic
                                        // chain at a freq with a
                                        // stronger signal, biasing
                                        // every NID toward DUID=0xF
                                        // (TDU_LC) and producing zero
                                        // real LDU dispatches. PLL
                                        // seeding remains since the
                                        // shared crystal trim makes
                                        // the converged value portable
                                        // across freqs.
                                        let core = follower_core.lock().await;
                                        let (pll_seed, _) = core.lsm_debug();
                                        let (pre, _) = core.traffic_lsm_debug();
                                        // 2026-04-26 per-freq AGC
                                        // seed for the nco_skip path
                                        // too. Same lookup as the
                                        // full retune path — same
                                        // freq, same expected gain.
                                        // freq_hz isn't bound in this
                                        // scope (the retune branch
                                        // shadows it from g.frequency_hz);
                                        // pull it locally here.
                                        let agc_seed = g.frequency_hz
                                            .and_then(|f|
                                                follower_imbe
                                                    .agc_seed_for_freq(f))
                                            .unwrap_or(0);
                                        core.set_traffic_lsm_seeds(
                                            pll_seed, agc_seed,
                                        );
                                        core.pulse_traffic_lsm_reset();
                                        let (post, _) = core.traffic_lsm_debug();
                                        nco_skip_diag = Some((pll_seed, pre, post));
                                    }
                                    let mut fields = serde_json::json!({
                                        "tg":             g.talkgroup.0,
                                        "channel":        g.channel.0,
                                        "frequency":      g.frequency_hz,
                                        "framer_reset":   true,
                                        "lsm_reset":      true,
                                        "nco_write":      false,
                                        "lsm_enable":     "unchanged",
                                    });
                                    if let Some((seed, pre, post)) = nco_skip_diag {
                                        let drift = (post as i32) - (seed as i32);
                                        if let serde_json::Value::Object(ref mut m) = fields {
                                            m.insert("pll_seed_written".to_string(), serde_json::json!(seed));
                                            m.insert("traffic_pll_pre_reset".to_string(), serde_json::json!(pre));
                                            m.insert("traffic_pll_post_reset".to_string(), serde_json::json!(post));
                                            m.insert("seed_drift".to_string(), serde_json::json!(drift));
                                            m.insert("seed_load_buggy".to_string(), serde_json::json!(drift.abs() > 2));
                                        }
                                    }
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "nco_skip TG={} (same freq, seed+reset only)",
                                            g.talkgroup.0,
                                        ),
                                        fields,
                                    );
                                }
                            }
                        }
                    }
                    tracker_event = tracker_rx.recv() => {
                        let event = match tracker_event {
                            Ok(e) => e,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                                tracing::warn!(
                                    target: "p25_traffic",
                                    "follower tracker_rx lagged by {n} events; \
                                     skipping",
                                );
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        // Phase 2c (2026-04-25): release the chain on
                        // CallTracker CallClose. CallTracker is the
                        // upstream lifecycle authority — TDU /
                        // SpeakerEnd / TgChange / SpeakerChange /
                        // Timeout all converge here. Other event
                        // kinds (CallOpen / SourceUpdate /
                        // ActualSpeakerObserved) are no-ops at this
                        // layer.
                        let close_reason = match &event.kind {
                            CallTrackerEventKind::CallClose { reason, .. } => Some(*reason),
                            _ => None,
                        };
                        let Some(reason) = close_reason else { continue; };

                        // Diagnostic lock keeps the chain on the
                        // parked freq even at call end so the demod
                        // stays running for measurement.
                        if follower_lock_freq.load(Ordering::Relaxed) {
                            continue;
                        }

                        let mut mgr = follower_mgr.lock().await;
                        let pre_close_tg = mgr.current_talkgroup();
                        // Soft state release only — TrafficChain
                        // goes Idle so the next grant's NCO-skip
                        // detection sees Idle as the precondition.
                        // The FPGA LSM chain stays ENABLED on the
                        // last freq so the PLL keeps its lock for
                        // the next same-freq call (the dominant
                        // case on a busy site). Phantom NID events
                        // from running on noise are filtered out
                        // upstream by BCH t=4 and downstream by the
                        // grant follower (only acts on TSBK grants
                        // from the CC, not on heartbeat NIDs).
                        mgr.force_idle();
                        drop(mgr);

                        if let Some(tg) = pre_close_tg {
                            tracing::info!(
                                target: "p25_traffic",
                                "traffic Idle (CallClose {:?}) TG {} -- \
                                 chain stays parked on last freq",
                                reason, tg.0,
                            );
                            follower_event_log.push(
                                crate::services::event_log::LogCategory::Traffic,
                                format!(
                                    "state -> Idle ({:?}) TG={} (chain parked)",
                                    reason, tg.0,
                                ),
                                serde_json::json!({
                                    "tg":             tg.0,
                                    "to":             "Idle",
                                    "reason":         format!("{:?}", reason),
                                    "chain_parked":   true,
                                }),
                            );
                        }
                        follower_imbe.call_encrypted.store(
                            false, Ordering::Relaxed,
                        );
                        follower_imbe.current_talkgroup.store(
                            0, Ordering::Relaxed,
                        );
                        // Clear stashed source on Idle so a
                        // subsequent call with no FM: in its grant
                        // doesn't inherit the previous speaker's ID.
                        follower_imbe.current_source.store(
                            0, Ordering::Relaxed,
                        );
                        follower_imbe.current_frequency_hz.store(
                            0, Ordering::Relaxed,
                        );
                        if let Ok(mut s) = follower_imbe.current_channel.lock() {
                            s.clear();
                        }
                    }
                }
            }
            tracing::warn!("grant follower task exiting (channel closed)");
        });
}

} // mod routing

#[cfg(target_os = "linux")]
pub use routing::spawn_grant_follower;

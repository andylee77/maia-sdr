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
use crate::audio::{AudioChunk, CallBoundary, CallBoundaryKind};
use crate::services::ui_settings::CallPolicy;

// ── 2026-04-26 session-lifecycle refactor: constants ─────────────

/// Cadence of the periodic timeout / drain-elapsed sweep. 100 ms
/// gives sub-second responsiveness on terminator-driven close
/// (terminator + drain → finalize within drain_ms + 100 ms).
const TIMEOUT_TICK_MS: u64 = 100;

// Close triggers (change 057; the live values are the persisted
// `call` settings, `services::ui_settings::CallPolicy`):
//
//   1. End of transmission ("call_end"): `end_grace_ms` (default 2 s)
//      after the first LC-valid TDULC decoded after this call's voice
//      (`CallBoundaryKind::VoiceEnd`, same frame at which SDRTrunk ends
//      its call event), unless voice resumes first (two voice NIDs
//      within `VOICE_NID_PAIR_MS`, or a voice chunk of this call aired
//      after the marker). CC grant updates do not hold the call open
//      after the marker: on this site they keep coming through the
//      system's channel hang.
//   2. Pre-empt ("tg_change"): the next primary grant (every grant is
//      a new call), e.g. the reply granted ~0.8 s after a PTT. A repeat
//      of the on-air call's own grant is a refresh
//      (`classify_cc_arrival`).
//   3. No keep-alive ("timeout"): `hang_ms` (default 3 s) without
//      voice of this call, an HDU, or a CC grant / grant update for
//      its TG on its channel. The fallback when no terminator is
//      decoded.
//
// History of (3), the pre-057 `IDLE_TIMEOUT_MS = 10 s` (hybrid audio /
// UPD keep-alive):
//
// 2026-05-02 we tried UPD-only at 3 s — broke every call. Field evidence
// (2026-04-30 18:29:49 capture, 14 UPDs for the active TG): UPDs cluster
// at t=0 (call open burst) then go SILENT for ~3.3 s, then resume after
// the call ends. That "CC decoder stall during traffic" was the control
// ring's 3.41 s block delivery (054 finding F4): TSBKs reached the PS in
// 3.4 s bursts. Since 054 the control ring delivers within ~0.2 s (p99
// 187 ms on the bench), and SDRTrunk's logs of this site (719
// transmissions, `tools/sdrtrunk_teardown_stats.py`) show a
// GRP_VCH_GRNT_UPD for the active channel every 0.315 s (p50; p99
// 0.53 s, max 0.96 s over 4083 gaps), continuing through the system's
// channel hang. 3 s is three times the largest gap; the audio
// keep-alive stays for CC decode dropouts during voice.
// doc/changes/057 has the measurements.

/// 2026-05-03 loss-of-sync close trigger. NID events fire every
/// ~180 ms in healthy P25 voice (1 per LDU at 4800 sps, 9 IMBE/LDU).
/// After this many ms with zero NID events on the traffic chain we
/// declare loss-of-sync and close the active call with
/// `CloseReason::SyncLost`. Mirrors SDRTrunk's per-channel LoS flag
/// using our framer-native metric (NID strobe age) rather than
/// dibit-error rate.
///
/// 1500 ms = ~8 missed LDUs. Below this, brief glitches in the BCH
/// sweep are normal (a single NID can be dropped if BCH is busy).
/// Above this, sync is unambiguously lost. The no-keep-alive timeout
/// (`hang_ms`) remains as the safety net for cases where the chain is
/// still syncing but neither audio nor CC UPDs are arriving.
///
/// Initialised on `CallOpen` to `now_unix_ms()` so a freshly opened
/// call has a full window to acquire — at 0 ms post-retune even with
/// seeds, the chain may take ~50-100 ms to finish settling.
const LOS_TIMEOUT_MS: u64 = 1_500;

// Closing-state drain lives on the recorder side: once we emit
// CallClose, the recorder keeps the WAV open for ~2 s and routes
// any straggling chunks whose `captured_at_ms` is `<=` close_at_ms
// into it. Lifecycle emits CallClose synchronously on the
// terminator parse; close_at_ms == terminator_at_ms is carried in
// the event's `ended_unix_ms` field for the recorder to compare
// against.

/// Change 057: two valid voice NIDs (HDU / LDU) this close together
/// after an end-of-transmission marker mean the channel carries voice
/// again (LDUs repeat every 180 ms); a single NID may be a false decode
/// on noise.
const VOICE_NID_PAIR_MS: u64 = 400;

/// Change 057: a queued grant (see `ActiveCall::queued`) is applied at
/// the latest this long after it arrived (SDRTrunk logs: up to 8 s).
const QUEUED_GRANT_MAX_MS: u64 = 10_000;

/// Change 057: after the lifecycle closed the live call by `Timeout`,
/// a grant UPDATE for the same TG and channel re-follows it for this
/// long (see `refollow_on_update`).
pub const REFOLLOW_WINDOW_MS: u64 = 30_000;

/// Longest gap since the chain's last voice frame for which a
/// same-frequency resume may coast on the chain's state.
pub const COAST_MAX_IDLE_MS: u64 = 1_000;

/// Should a same-frequency Idle -> Active resume reset the traffic LSM
/// chain instead of coasting on its current AGC / PLL / timing state?
///
/// A parked chain stays enabled and keeps demodulating after the carrier
/// drops (1.3-1.7 s after the last LDU on this site), so its PLL walks to
/// the clamp and its AGC winds up on noise. Coasting from there lost the
/// whole first transmission (bench 2026-09-27: `pll preserved=8579`, the
/// clamp, then two calls with 0 IMBE). With 057's prompt close every
/// same-channel call after a pause takes this path, so coast only while
/// the chain carried voice within `COAST_MAX_IDLE_MS` and its PLL sits
/// well inside the clamp (`clamp_q213`: the running gateware's,
/// `CoreVersion::pll_clamp_q213`; change 059 lowered it to 0.65 rad).
pub fn resume_needs_reset(pll_q213: i16, ms_since_voice: Option<u64>, clamp_q213: i32) -> bool {
    let pll_hot = (pll_q213 as i32).abs() >= clamp_q213 / 2;
    let stale = ms_since_voice.map_or(true, |ms| ms > COAST_MAX_IDLE_MS);
    pll_hot || stale
}

/// Change 057: should a grant UPDATE (which never acquires the chain on
/// its own) re-follow a call? Only for the (TG, frequency) of the live
/// call closed by `Timeout` — no keep-alive for `hang_ms`, e.g. the
/// control and traffic signals faded together — within
/// `REFOLLOW_WINDOW_MS`, while the chain is idle. The CC still
/// announcing the call means it did not end. SDRTrunk (re)starts a
/// traffic channel on a grant update whenever none is running for it;
/// this keeps that property for calls we were following, without
/// letting updates acquire talkgroups we never followed (updates carry
/// no encryption flag: the 2026-04-30 TG 700 incident).
pub fn refollow_on_update(
    last_timeout: Option<(u16, u64, u64)>,
    tg: u16,
    freq_hz: Option<u64>,
    chain_idle: bool,
    now_ms: u64,
    window_ms: u64,
) -> bool {
    let Some((ltg, lfreq, at)) = last_timeout else {
        return false;
    };
    chain_idle
        && ltg == tg
        && freq_hz == Some(lfreq)
        && now_ms.saturating_sub(at) <= window_ms
}

/// Change 059: `refollow_on_update` window for a grant the sticky gate
/// rejected, from the reject. Voice starts ~0.5 s after the grant and a
/// clear transmission lasts 1.8 s (median; p25 1.44 s, p75 3.06 s over the
/// 241 of the Mode B corpus), so later updates mostly come from the
/// system's hang after it: a re-follow then parks the one chain on a dead
/// channel and the next real grant is rejected (bench 2026-09-27: TG 318
/// re-followed 4.9 s after its reject, TG 319's grant 0.5 s later lost 99
/// frames).
pub const REFOLLOW_STICKY_MS: u64 = 2_000;

/// Change 059: another TG's grant may pre-empt the locked call once the
/// call's end-of-transmission marker has been pending this long. Long
/// enough for the resumed-voice check (two voice NIDs within 400 ms)
/// to cancel a contradicted marker, and still before SDRTrunk frees its
/// channel (1.13-1.65 s after the last voice, p5-p90 over the 213
/// traffic recordings of the Mode B corpus; the marker arrives ~0.2-0.5 s
/// after the last voice).
pub const END_PREEMPT_AFTER_MS: u64 = 600;

/// Change 059: the active call's pending end-of-transmission marker as
/// published by the lifecycle for the follower (`ImbeForwarder::
/// active_end_marker`): `(tg << 48) | receipt unix ms`, 0 = none.
pub fn pack_end_marker(tg: u16, at_ms: u64) -> u64 {
    ((tg as u64) << 48) | (at_ms & ((1 << 48) - 1))
}

pub fn unpack_end_marker(v: u64) -> Option<(u16, u64)> {
    (v != 0).then(|| ((v >> 48) as u16, v & ((1 << 48) - 1)))
}

/// Change 059: may a grant for another TG pre-empt the call locked on
/// `locked_tg`? Only once that call's transmission has ended (its end
/// marker pending for `END_PREEMPT_AFTER_MS`): the lifecycle would keep
/// the chain `end_grace_ms` (2 s) longer for a same-TG continuation, and
/// the sticky gate rejected the other TG's grant meanwhile (Mode B
/// corpus 2026-09-27: 3 transmissions lost, granted 0.1-0.6 s after
/// SDRTrunk had freed its channel).
pub fn end_marker_frees_chain(locked_tg: u16, marker: Option<(u16, u64)>, now_ms: u64) -> bool {
    matches!(marker, Some((tg, at))
        if tg == locked_tg && now_ms.saturating_sub(at) >= END_PREEMPT_AFTER_MS)
}

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
        /// 2026-04-30 air-time tracking. Wall time of the most
        /// recent `GRP_VCH_GRNT_UPD` beacon for this call's TG.
        /// Zero if no UPDs landed (e.g. very short call, or chain
        /// missed all updates). Combined with started_unix_ms in
        /// grant_stats to derive `air_duration_ms` — the speaker's
        /// on-air duration independent of audio extraction success.
        last_upd_at_unix_ms: u64,
        /// Change 057: how long the lifecycle held the call open
        /// (monotonic clock, immune to `/api/set_time`).
        open_ms: u64,
        /// Change 057: the end-of-transmission marker that was pending
        /// at the close ("talk_complete", "channel_user", ...), if any.
        end_lc: Option<&'static str>,
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
    /// `hang_ms` (change 057: persisted setting, default 3 s; was a
    /// fixed 10 s) with no keep-alive: no CC GRP_VCH_GRNT_UPD / grant
    /// for the active TG, no audio chunk or voice NID of the call, no
    /// HDU. Also the reason on the synthetic close of a not-followed
    /// grant.
    Timeout,
    /// Change 057: `end_grace_ms` after the end-of-transmission marker
    /// (first LC-valid TDULC after the call's voice) with no voice
    /// since. Every PTT is its own call (grant = call), so ending the
    /// call at its terminator no longer fragments anything.
    CallEnd,
    /// Pre-empt: primary GRANT on same channel for a different TG.
    /// The only "real" close path during normal operation.
    TgChange,
    /// Boundary stream lagged — drop active to avoid stale state.
    StreamLag,
    /// 2026-05-03: traffic-LSM framer hasn't fired an `nid_event` for
    /// `LOS_TIMEOUT_MS` (1.5 s, ~8 LDU periods). Close fast so the
    /// next grant can retune cleanly rather than waiting for the
    /// 10 s `Timeout` backstop. Mirrors SDRTrunk's loss-of-sync flag.
    SyncLost,
}

pub type CallTrackerEventTx = broadcast::Sender<CallTrackerEvent>;

pub fn new_event_tx() -> CallTrackerEventTx {
    broadcast::channel(64).0
}

#[derive(Debug, Clone, Default)]
pub struct ActiveCallSnapshot {
    pub call_id: u64,
    pub tg: u16,
    pub nac: u16,
    pub source: Option<u32>,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub encrypted: bool,
    pub started_unix_ms: u64,
    /// Change 056: every source seen in this call, in order.
    pub sources_observed: Vec<u32>,
    /// Change 056: audio chunks (20 ms each) attributed to this call.
    pub voice_frames: u64,
    /// Change 056: first / last audio chunk of this call (lifecycle
    /// receipt time, unix ms). `None` before the first chunk.
    pub first_voice_unix_ms: Option<u64>,
    pub last_voice_unix_ms: Option<u64>,
    /// Change 056: newest keep-alive (audio, HDU or CC grant / update)
    /// the idle close measures from: the call closes `hang_ms` after
    /// this (change 057: unless an end-of-transmission marker closes it
    /// sooner, see `close_*`).
    pub last_activity_unix_ms: u64,
    /// Change 057: when the lifecycle will close the call if nothing
    /// changes (unix ms), which rule that is ("end" = end-of-transmission
    /// grace, "timeout" = no keep-alive) and the full length of that
    /// window (for the UI's countdown bar).
    pub close_at_unix_ms: u64,
    pub close_via: &'static str,
    pub close_window_ms: u64,
    /// Change 057: the pending end-of-transmission marker, if any.
    pub end_lc: Option<&'static str>,
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
    /// Monotonic open time; change 057: the CallClose `open_ms`.
    started_instant: Instant,
    first_audio_at_unix_ms: Option<u64>,
    first_hdu_at_unix_ms: Option<u64>,
    #[allow(dead_code)]
    baseline_frames_submitted: u64,
    /// Wall time of the most recent CC heartbeat for this call: the
    /// primary GRP_VCH_GRANT that opened the session, every Bundle-
    /// path GRP_VCH_GRANT/_EXP refresh, and every plain
    /// GRP_VCH_GRNT_UPD for the active TG. With `last_audio_at_ms`,
    /// drives the close trigger via `now - max(audio, upd) > IDLE_TIMEOUT_MS`.
    /// Also pairs with `started_unix_ms` to derive `air_duration_ms`
    /// at finalise (the speaker's physical airtime regardless of
    /// whether the chain extracted audio — useful for followed calls
    /// where audio dropped, and not_followed calls where we never
    /// decoded).
    last_upd_at_ms: u64,
    /// Wall time of the most recent audio chunk observed for this call.
    /// Restored 2026-05-02: was removed in the UPD-only attempt but
    /// CC decoder stalls during traffic activity (see
    /// IDLE_TIMEOUT_MS doc) leave UPD-only at the mercy of that bug.
    /// Audio keep-alive bypasses the CC stall — when chain is decoding
    /// voice, audio chunks arrive every ~20 ms regardless of CC health.
    last_audio_at_ms: u64,
    /// 2026-05-03 loss-of-sync detector. Wall time of the most recent
    /// traffic-LSM `nid_event` strobe stamped via
    /// `CallBoundaryKind::TrafficNidObserved`. Initialised to
    /// `started_unix_ms` on `CallOpen`. The periodic tick closes with
    /// `CloseReason::SyncLost` when `now - last_nid_at_ms` exceeds
    /// `LOS_TIMEOUT_MS`. Independent of the audio / UPD timeout —
    /// LoS fires fast (1.5 s), idle-timeout is the slow backstop.
    last_nid_at_ms: u64,
    /// Every distinct SRC observed during the session — primary
    /// GRANT.SRC, LDU1 LC voted SRC, TDULC MOT BY: — in insertion
    /// order. Deduped on push.
    sources_observed: Vec<u32>,
    /// Change 056: audio chunks of this call and the receipt time of
    /// the first / newest one (0 = none yet). Unlike
    /// `last_audio_at_ms` these are not refreshed by an HDU, so they
    /// tell voice from mere keep-alive for the UI.
    voice_frames: u64,
    first_voice_at_ms: u64,
    last_voice_at_ms: u64,
    /// Change 057: receipt time of the newest valid voice NID (HDU /
    /// LDU1 / LDU2) from the traffic-LSM heartbeat (real time, ahead of
    /// the PS decode). Only used to see voice resume after an end
    /// marker (two voice NIDs within `VOICE_NID_PAIR_MS`); not a
    /// keep-alive, so a false NID on noise cannot hold a call open.
    last_voice_nid_at_ms: u64,
    /// Change 057: pending end of transmission (`VoiceEnd` of this
    /// call): receipt time at the lifecycle (0 = none), air time of the
    /// terminator, and its LC kind. Cleared when voice resumes.
    end_at_ms: u64,
    end_air_ms: u64,
    end_lc: Option<&'static str>,
    /// Change 057: end markers cancelled because voice resumed.
    end_cancels: u32,
    /// Change 057: grant for the next talker on this call's channel and
    /// TG that arrived while this call was still on the air (queued /
    /// console pre-empt: 12.8 % of same-channel grants in the SDRTrunk
    /// logs, up to 8 s early). Applied when the channel hands over
    /// (`queued_due`), not at once, so the rest of this transmission
    /// stays in this call.
    queued: Option<QueuedGrant>,
}

/// Change 057: a followed grant waiting for the channel hand-over.
#[derive(Debug, Clone)]
struct QueuedGrant {
    tg: u16,
    nac: u16,
    source: Option<u32>,
    freq_hz: Option<u64>,
    channel: Option<String>,
    encrypted: bool,
    at_ms: u64,
}

impl ActiveCall {
    /// Change 056: a fresh session record; everything not given starts
    /// empty. `last_upd_at_ms` is the CC heartbeat bootstrap (the
    /// opening grant counts as one); synthetic not-followed records
    /// pass 0.
    #[allow(clippy::too_many_arguments)]
    fn open(
        call_id: u64,
        tg: u16,
        nac: u16,
        source: Option<u32>,
        freq_hz: Option<u64>,
        channel: Option<String>,
        encrypted: bool,
        not_followed: Option<&'static str>,
        now: u64,
        baseline_frames_submitted: u64,
        last_upd_at_ms: u64,
    ) -> Self {
        ActiveCall {
            call_id,
            tg,
            nac,
            source,
            actual_speaker: None,
            freq_hz,
            channel,
            encrypted,
            not_followed,
            started_unix_ms: now,
            started_instant: Instant::now(),
            first_audio_at_unix_ms: None,
            first_hdu_at_unix_ms: None,
            baseline_frames_submitted,
            last_upd_at_ms,
            last_audio_at_ms: 0,
            last_nid_at_ms: now,
            sources_observed: source.into_iter().collect(),
            voice_frames: 0,
            first_voice_at_ms: 0,
            last_voice_at_ms: 0,
            last_voice_nid_at_ms: 0,
            end_at_ms: 0,
            end_air_ms: 0,
            end_lc: None,
            end_cancels: 0,
            queued: None,
        }
    }

    /// Change 057: the call has shown voice (HDU, LDU NID or decoded
    /// audio) — so its transmission can still be on the air.
    fn voice_seen(&self) -> bool {
        self.first_hdu_at_unix_ms.is_some()
            || self.voice_frames > 0
            || self.last_voice_nid_at_ms != 0
    }

    /// Change 057: newest keep-alive for the no-activity close.
    fn last_keepalive_ms(&self) -> u64 {
        self.last_audio_at_ms
            .max(self.last_upd_at_ms)
            .max(self.started_unix_ms)
    }

    /// Change 057: voice resumed after an end-of-transmission marker
    /// (a new transmission on the same grant, or a marker the air
    /// contradicts): drop the pending end close.
    fn cancel_end(&mut self) {
        if self.end_at_ms != 0 {
            self.end_at_ms = 0;
            self.end_air_ms = 0;
            self.end_lc = None;
            self.end_cancels += 1;
        }
    }

    /// Change 057: (close time, rule, window) if nothing changes. The
    /// end-of-transmission grace wins when it is due first.
    fn close_plan(&self, hang_ms: u64, end_grace_ms: u64) -> (u64, &'static str, u64) {
        let idle_at = self.last_keepalive_ms() + hang_ms;
        if self.end_at_ms != 0 {
            let end_at = self.end_at_ms + end_grace_ms;
            if end_at <= idle_at {
                return (end_at, "end", end_grace_ms);
            }
        }
        (idle_at, "timeout", hang_ms)
    }

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

/// Change 057: the close rule due at `now` for the open call, if any.
/// The end-of-transmission grace uses `>=` (elapsed), the no-keep-alive
/// timeout `>` (as pre-057).
fn close_due(c: &ActiveCall, now: u64, hang_ms: u64, end_grace_ms: u64) -> Option<CloseReason> {
    if c.end_at_ms != 0 && now.saturating_sub(c.end_at_ms) >= end_grace_ms {
        return Some(CloseReason::CallEnd);
    }
    if now.saturating_sub(c.last_keepalive_ms()) > hang_ms {
        return Some(CloseReason::Timeout);
    }
    None
}

fn mirror_active(
    active: &Option<ActiveCall>,
    shared: &ActiveCallShared,
    forwarder: &Arc<ImbeForwarder>,
    policy: &CallPolicy,
) {
    // Change 059: the pending end marker, for the follower's sticky gate
    // (`end_marker_frees_chain`).
    let marker = active.as_ref()
        .filter(|c| c.end_at_ms != 0)
        .map_or(0, |c| pack_end_marker(c.tg, c.end_at_ms));
    forwarder.active_end_marker.store(marker, Ordering::Relaxed);
    if let Ok(mut s) = shared.lock() {
        *s = active.as_ref().map(|c| {
            let (close_at, via, window) = c.close_plan(policy.hang_ms(), policy.end_grace_ms());
            ActiveCallSnapshot {
                call_id: c.call_id,
                tg: c.tg,
                nac: c.nac,
                source: c.source,
                freq_hz: c.freq_hz,
                channel: c.channel.clone(),
                encrypted: c.encrypted,
                started_unix_ms: c.started_unix_ms,
                sources_observed: c.sources_observed.clone(),
                voice_frames: c.voice_frames,
                first_voice_unix_ms: (c.first_voice_at_ms != 0).then_some(c.first_voice_at_ms),
                last_voice_unix_ms: (c.last_voice_at_ms != 0).then_some(c.last_voice_at_ms),
                last_activity_unix_ms: c.last_keepalive_ms(),
                close_at_unix_ms: close_at,
                close_via: via,
                close_window_ms: window,
                end_lc: c.end_lc,
            }
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
    // Change 054: `set_live_call_id` also records a CallOpen epoch cut
    // when the id changes (airtime mode), so frames completing after it
    // carry the new call_id even though they are decoded later.
    if let Some(c) = active.as_ref() {
        forwarder.set_live_call_id(c.call_id);
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
            last_upd_at_unix_ms: call.last_upd_at_ms,
            open_ms: call.started_instant.elapsed().as_millis() as u64,
            end_lc: call.end_lc,
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
    /// add new SRC if any). Change 057: a repeat of the on-air call's
    /// own grant.
    Bundle,
    /// Change 057: the next talker's grant while this call is on the
    /// air: hold it until the hand-over.
    Queue,
    /// Different freq from the open session — sticky-locked
    /// elsewhere; ignore.
    Ignore,
    /// Same channel, different TG — close current, open new.
    TgChange,
}

fn classify_cc_arrival(
    active: &ActiveCall,
    new_tg: u16,
    new_source: Option<u32>,
    new_freq_hz: Option<u64>,
) -> ArrivalDisposition {
    // 2026-04-30 design pivot per operator instruction: every
    // GRP_VCH_GRANT (after the entry-point `grant_dedup` filters the
    // TSDU triplet re-broadcasts) creates a NEW call_track. Same TG,
    // same freq, same source = still a new call (different speaker
    // turn). Same TG, different freq = new call AND retune. Same TG,
    // different source = new call. Bundling and same-freq-only
    // refresh are both gone — only GRP_VCH_GRNT_UPD events take the
    // refresh path (separate `CcRefresh` boundary, handled below).
    //
    // Change 057: except a repeat of the grant of the call that is
    // still on the air. Same TG, same frequency, same source (or no
    // source: a source-less repeat or explicit update), and the call's
    // transmission has not ended (no end-of-transmission marker yet),
    // is the CC re-announcing this call — SDRTrunk's same-call check
    // (`isSameCallCheckingToOnly`) — not the next PTT. Pre-054 such
    // repeats were swallowed by `GRANT_DEDUP_MS` by accident: the
    // control ring delivered ~3.4 s of TSBKs in one burst, so every
    // repeat fell within 200 ms of processing time. Since 054 the
    // window really is 200 ms of air, and a repeat 0.3 s later would
    // split one transmission into two calls. A new PTT by the same
    // unit follows its predecessor's end marker, so it still opens a
    // new call.
    //
    // Change 057: a grant for ANOTHER source on the same TG and channel
    // while this call is still on the air (voice seen, no end marker)
    // is the next talker queued behind this one (consoles pre-empt /
    // queue: 12.8 % of same-channel grants in the SDRTrunk logs, up to
    // 8 s before the current talker unkeys). Opening the next call at
    // once would hand the rest of this transmission to it; it is held
    // and applied at the hand-over (`queued_due`, HDU / voice NIDs).
    let same_freq = active.freq_hz.is_some() && active.freq_hz == new_freq_hz;
    if active.tg != new_tg || !same_freq || active.end_at_ms != 0 {
        return ArrivalDisposition::TgChange;
    }
    let same_source = match (new_source, active.source) {
        (None, _) => true,
        (Some(n), Some(a)) => n == a,
        // Source now known for a call granted without one (SRC 0):
        // the same call, filled in by the Bundle path.
        (Some(_), None) => true,
    };
    if same_source {
        ArrivalDisposition::Bundle
    } else if active.voice_seen() {
        ArrivalDisposition::Queue
    } else {
        // Two talkers granted back to back before any voice: the later
        // grant is the one on the air.
        ArrivalDisposition::TgChange
    }
}

/// Change 057: close the open call (if `close_reason`) and open the
/// call of `g`. The successor's call_id is published before the
/// predecessor's CallClose (054: the follower releases the chain only
/// for a close of the live call).
fn open_next(
    active: &mut Option<ActiveCall>,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    forwarder: &Arc<ImbeForwarder>,
    g: QueuedGrant,
    close_reason: Option<CloseReason>,
    opened_via: OpenReason,
) {
    if let Some(reason) = close_reason {
        forwarder.set_live_call_id(*next_call_id);
        if let Some(prev) = active.take() {
            let expected = forwarder.frames_submitted.load(Ordering::Relaxed);
            emit_close(tx, &prev, reason, prev.source, expected);
        }
    }
    let now = now_unix_ms();
    let call_id = *next_call_id;
    *next_call_id += 1;
    let baseline = forwarder.frames_submitted.load(Ordering::Relaxed);
    // The primary GRP_VCH_GRANT that opened this session is itself the
    // first CC heartbeat — bootstrap the UPD timer (last argument) so
    // the close trigger doesn't fire before the first GRP_VCH_GRNT_UPD
    // lands.
    *active = Some(ActiveCall::open(
        call_id, g.tg, g.nac, g.source, g.freq_hz,
        g.channel.clone(), g.encrypted, None, now, baseline, now,
    ));
    emit_open(
        tx, call_id, g.tg, g.nac, g.source, g.freq_hz, g.channel,
        g.encrypted, None, opened_via, baseline, now,
    );
}

/// Change 057: hand the channel to the queued grant (the predecessor
/// closes as `TgChange`, like an immediate pre-empt).
fn start_queued(
    active: &mut Option<ActiveCall>,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    forwarder: &Arc<ImbeForwarder>,
) {
    let Some(q) = active.as_mut().and_then(|a| a.queued.take()) else {
        return;
    };
    open_next(active, next_call_id, tx, forwarder, q,
              Some(CloseReason::TgChange), OpenReason::TgChange);
}

/// Change 057: the periodic close sweep (every `TIMEOUT_TICK_MS`). A
/// queued grant takes over when this call's end grace ran out, its
/// no-keep-alive timeout is due, or the grant waited
/// `QUEUED_GRANT_MAX_MS`; otherwise `close_due` decides.
fn sweep(
    active: &mut Option<ActiveCall>,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    forwarder: &Arc<ImbeForwarder>,
    now: u64,
    hang_ms: u64,
    end_grace_ms: u64,
) {
    let Some(a) = active.as_ref() else {
        return;
    };
    if let Some(q) = a.queued.as_ref() {
        let due = now.saturating_sub(q.at_ms) >= QUEUED_GRANT_MAX_MS
            || close_due(a, now, hang_ms, end_grace_ms).is_some();
        if due {
            start_queued(active, next_call_id, tx, forwarder);
        }
        return;
    }
    if let Some(reason) = close_due(a, now, hang_ms, end_grace_ms) {
        if let Some(call) = active.take() {
            let expected = forwarder.frames_submitted.load(Ordering::Relaxed);
            emit_close(tx, &call, reason, call.source, expected);
        }
    }
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
    // Change 057: close timing (persisted `call` settings, live).
    policy: Arc<CallPolicy>,
    // Change 057: first call_id of this process. Recordings on the SD
    // card outlive the process and are keyed by call_id, so ids continue
    // after the highest one found there (`audio::rec_storage`).
    first_call_id: u64,
) {
    let mut boundary_rx = boundary_tx.subscribe();
    let mut audio_rx = audio_tx.subscribe();

    tokio::spawn(async move {
        let mut active: Option<ActiveCall> = None;
        let mut next_call_id: u64 = first_call_id.max(1);
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
            std::collections::HashMap<(u16, u32, Option<u64>, bool), u64>
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
                            mirror_active(&active, &active_call, &forwarder, &policy);
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    handle_boundary(
                        boundary, &mut active, &mut next_call_id,
                        &tracker_tx, &forwarder,
                        &mut not_followed_dedup,
                    );
                    mirror_active(&active, &active_call, &forwarder, &policy);
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
                    // Change 057: end-of-transmission grace, else the
                    // no-keep-alive timeout (`close_due`; timing from the
                    // persisted `call` settings). Keep-alives: CC UPD /
                    // grant for the TG, audio of this call, voice NIDs,
                    // HDU — so a decode dropout inside a live
                    // transmission (CC still announcing it, or the HDL
                    // still seeing LDU NIDs) never closes the call.
                    //
                    // 2026-05-03 LoS detector REMOVED from the close
                    // decision. Initial 1.5 s NID-age threshold killed
                    // real calls — mid-call sync gaps from BCH busy
                    // sweeps + brief noise tripped LoS while the
                    // call was still actively decoding (operator
                    // observation: call_closing reason="sync_lost"
                    // with only 360 ms of PCM accumulated, build
                    // 2026-05-03-coast-no-reset). The `Timeout`
                    // backstop catches truly dead calls. The
                    // `last_nid_at_ms` field stays in `ActiveCall` so
                    // we can re-introduce a relaxed LoS later if
                    // useful, but it's not consulted for closes.
                    sweep(
                        &mut active, &mut next_call_id, &tracker_tx,
                        &forwarder, now_unix_ms(),
                        policy.hang_ms(), policy.end_grace_ms(),
                    );
                    // Change 056: mirror every tick, not only on
                    // boundaries, so the snapshot's voice counters /
                    // timestamps (updated by the audio arm) stay within
                    // 100 ms of live. `set_live_call_id` inside is a
                    // no-op while the call_id is unchanged.
                    mirror_active(&active, &active_call, &forwarder, &policy);
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
    grant_dedup: &mut std::collections::HashMap<(u16, u32, Option<u64>, bool), u64>,
) {
    match boundary.kind {
        // 2026-04-26 session-lifecycle refactor: primary
        // GRP_VCH_GRANT (`is_update=false`). Open-or-bundle-or-
        // split, no audio-driven anything.
        CallBoundaryKind::CcGrantArrival {
            tg, source, freq_hz, channel, encrypted, not_followed,
        } => {
            let now = now_unix_ms();
            // 2026-04-27 entry-point dedup, 2026-04-30 source-aware.
            // Skip GRANTs arriving within GRANT_DEDUP_MS of a prior
            // IDENTICAL grant — kills the TSDU triplet re-broadcasts
            // (TSBK1/2/3 of one TSDU, ~30 ms apart). 2026-04-30: key
            // now includes `source` so two distinct speakers granted
            // back-to-back on the same TG/freq (~50 ms apart) are
            // both honoured as separate calls. Without `source` in
            // the key, e.g. a TG=300 SRC=1013 grant followed 49 ms
            // later by a TG=300 SRC=3402071 grant on the same freq
            // was being silently dropped — making rec 14 lose
            // speakers from `sources_observed`. SRC=0 (some
            // dispatch radios omit FM) collapses to a single class,
            // which is acceptable: those grants are typically
            // dispatcher-console rebroadcasts of an active speaker.
            let dedup_source = source.unwrap_or(0);
            let dedup_key = (tg, dedup_source, freq_hz, encrypted);
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
                    let synth_call = ActiveCall::open(
                        synthetic_call_id, tg, boundary.nac, source,
                        freq_hz, channel_str.clone(), encrypted,
                        not_followed, now, baseline, 0,
                    );
                    emit_close(
                        tx, &synth_call, CloseReason::Timeout,
                        source, baseline,
                    );
                    return;
                }
                // Active session, new GRANT is not_followed: emit a
                // synthetic CallOpen+CallClose pair so grant_stats
                // surfaces the not-followed grant in Recent Calls
                // (operator confirmed: every CC GRANT must be
                // visible). Recorder filters not_followed CallOpens
                // so no recording is opened. The 2026-04-26
                // misattribution bug — TG 300 → TG 402 (ENC, same
                // freq) preempt producing 459 IMBE attributed to
                // TG 402 — is what motivated the synthetic-emit
                // pattern.
                //
                // 2026-04-30 add: if the not_followed grant is for
                // the SAME freq as the active session, the air on
                // that freq is now hosting the encrypted/sticky/
                // monitor-rejected call. The previous clear call
                // physically ended (one voice channel per freq).
                // Close active first; then emit the synthetic
                // pair. Cross-freq not_followed grants still leave
                // active alone because they don't affect what the
                // chain is decoding.
                Some(_) if not_followed.is_some() => {
                    if let Some(a) = active.as_ref() {
                        if a.freq_hz.is_some()
                            && a.freq_hz == freq_hz
                        {
                            let prev = active.take().unwrap();
                            let expected = forwarder
                                .frames_submitted
                                .load(Ordering::Relaxed);
                            emit_close(
                                tx, &prev,
                                CloseReason::TgChange,
                                prev.source, expected,
                            );
                        }
                    }
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
                    let synth_call = ActiveCall::open(
                        synthetic_call_id, tg, boundary.nac, source,
                        freq_hz, channel_str.clone(), encrypted,
                        not_followed, now, baseline, 0,
                    );
                    emit_close(
                        tx, &synth_call, CloseReason::Timeout,
                        source, baseline,
                    );
                    return;
                }
                Some(a) => match classify_cc_arrival(a, tg, source, freq_hz) {
                    ArrivalDisposition::Bundle => OpenAction::Bundle,
                    ArrivalDisposition::Queue => OpenAction::Queue,
                    ArrivalDisposition::Ignore => OpenAction::None,
                    ArrivalDisposition::TgChange => OpenAction::Preempt(
                        CloseReason::TgChange, OpenReason::TgChange,
                    ),
                },
            };
            let grant_q = QueuedGrant {
                tg,
                nac: boundary.nac,
                source,
                freq_hz,
                channel: channel_str,
                encrypted,
                at_ms: now,
            };

            match action {
                OpenAction::None => return,
                OpenAction::Bundle => {
                    let a_mut = active.as_mut().unwrap();
                    // CcGrantArrival on the same TG is itself a CC
                    // heartbeat — refresh the UPD timer so the close
                    // trigger sees the GRP_VCH_GRNT_UPD_EXP variant
                    // (which routes through here, not CcGrantUpdate)
                    // and re-issued primary GRANTs as activity.
                    a_mut.last_upd_at_ms = now_unix_ms();
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
                // Change 057: the next talker's grant while this call is
                // on the air: held until the hand-over (`sweep`, HDU,
                // voice NIDs after the end marker). A newer queued grant
                // replaces an older one.
                OpenAction::Queue => {
                    if let Some(a) = active.as_mut() {
                        a.queued = Some(grant_q);
                    }
                    return;
                }
                // Change 054: `open_next` publishes the successor's
                // call_id BEFORE the predecessor's CallClose goes out.
                // The grant follower releases the traffic chain only for
                // a CallClose of the live call (`current_call_id`); with
                // the old order it could see CallClose(prev) while
                // `current_call_id` still named prev and zero the TG it
                // had just set for the new grant.
                OpenAction::Preempt(close_reason, opened_via) => {
                    open_next(active, next_call_id, tx, forwarder, grant_q,
                              Some(close_reason), opened_via);
                }
                OpenAction::Open(opened_via) => {
                    open_next(active, next_call_id, tx, forwarder, grant_q,
                              None, opened_via);
                }
            }
        }

        // GRP_VCH_GRNT_UPD: refresh-only. Does NOT carry SRC or
        // service_options — confirmed empirically 2026-04-26 in
        // the on-target log: every UPDATE shows source=None and
        // enc_flag pulled from per-channel history. Cannot signal
        // a speaker change, TG change, or encryption flip.
        // 2026-05-02: this beacon now drives the close trigger
        // directly — `a.last_upd_at_ms = now` keeps the call alive,
        // and a UPD_TIMEOUT_MS gap closes it.
        CallBoundaryKind::CcGrantUpdate { tg, freq_hz, channel } => {
            // 2026-04-30 dedup TSDU triplet duplicates of the same
            // UPD (TSBK1/2/3 of one TSDU, ~30 ms apart, identical
            // payload). Mirrors the primary-GRANT dedup at the top
            // of CcGrantArrival. Without this, last_upd_at_ms
            // advances 3× per UPD broadcast, which is noise vs the
            // 5 s steady-state UPD cadence — but doing it cleanly
            // keeps the event accounting symmetric with primary
            // grants. Re-uses the same map and the same window.
            // UPDs don't carry source; key the dedup with
            // source=0 + the channel-or-freq tuple.
            let dedup_key = (tg, 0u32, freq_hz.or(Some(channel as u64)), false);
            if let Some(&last) = grant_dedup.get(&dedup_key) {
                if now_unix_ms().saturating_sub(last) < GRANT_DEDUP_MS {
                    return;
                }
            }
            grant_dedup.insert(dedup_key, now_unix_ms());
            if let Some(a) = active.as_mut() {
                // Change 057: an update for this TG on ANOTHER channel
                // (patch, other site in the TSBK's second slot) says
                // nothing about this call's channel.
                let same_channel = match (a.freq_hz, freq_hz) {
                    (Some(x), Some(y)) => x == y,
                    _ => true,
                };
                if a.tg == tg && same_channel {
                    // 2026-04-30 air-time tracking. UPD beacons are
                    // the only signal that the speaker is still
                    // keyed when the chain itself extracts no audio
                    // — encrypted calls, sync-loss calls, and
                    // chains that fail body extraction all still
                    // get UPDs from the CC. Drives both the CallClose
                    // trigger (UPD_TIMEOUT_MS gap → close) and the
                    // `air_duration_ms` derivation at finalise.
                    a.last_upd_at_ms = now_unix_ms();
                }
            }
        }

        // HDU: informational only. Bundling speakers means we no
        // longer split per-PTT — multiple HDUs in one session is
        // expected (e.g. dispatcher then field radio on same grant).
        // 2026-05-02: no longer refreshes any timeout — close trigger
        // is the CC UPD heartbeat, which is independent of chain
        // decoder health.
        CallBoundaryKind::HduStart => {
            // Change 057: a new transmission on the channel while the
            // next talker's grant is queued: it is that talker (the HDU
            // NID is seen in real time, so the call_id cut lands before
            // the HDU completes and the HDU is the new call's).
            if active.as_ref().is_some_and(|a| a.queued.is_some()) {
                start_queued(active, next_call_id, tx, forwarder);
            }
            let now = now_unix_ms();
            if let Some(a) = active.as_mut() {
                if a.first_hdu_at_unix_ms.is_none() {
                    a.first_hdu_at_unix_ms = Some(now);
                }
                if a.first_audio_at_unix_ms.is_none() {
                    a.first_audio_at_unix_ms = Some(now);
                }
                // HDU = chain found a P25 voice frame start. Strong
                // evidence the call is alive — refresh the audio-side
                // keep-alive so the hybrid timeout has a fresh anchor
                // even before the first PCM chunk lands.
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
        //
        // Change 057: since 2026-04-30 every grant is its own call, so
        // the multi-PTT concern above no longer applies. The end of a
        // transmission now arrives as `VoiceEnd` (below): call-attributed,
        // LC-FEC-checked, once per transmission, and cancelled when voice
        // resumes, which also covers phantom terminators. SpeakerEnd
        // (cooldown-gated, source-checked) stays a source stamp.
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
        // 2026-05-03 loss-of-sync detector: traffic-LSM heartbeat saw an
        // `nid_event` strobe. Stamp the active call's `last_nid_at_ms`
        // so the periodic tick can decide LoS based purely on
        // framer-state age. No-op when there is no active call (the
        // heartbeat broadcasts unconditionally).
        //
        // Change 057: a pair of voice NIDs after an end marker cancels
        // the pending end close (voice resumed on the channel: a re-key
        // on the same grant, or a marker the air contradicts). The HDL
        // is read in real time, ahead of the PS decode that produced the
        // marker, so these NIDs are later on the air than the marker.
        //
        // With a queued grant (next talker), voice after the end marker
        // is that talker: hand the channel over instead.
        CallBoundaryKind::TrafficNidObserved { voice } => {
            let mut hand_over = false;
            if let Some(a) = active.as_mut() {
                let now = now_unix_ms();
                a.last_nid_at_ms = now;
                if voice {
                    let prev = a.last_voice_nid_at_ms;
                    a.last_voice_nid_at_ms = now;
                    if a.end_at_ms != 0
                        && prev >= a.end_at_ms
                        && now.saturating_sub(prev) <= VOICE_NID_PAIR_MS
                    {
                        if a.queued.is_some() {
                            hand_over = true;
                        } else {
                            a.cancel_end();
                        }
                    }
                }
            }
            if hand_over {
                start_queued(active, next_call_id, tx, forwarder);
            }
        }

        // Change 057: end of a transmission of `call_id` (first LC-valid
        // TDULC after its voice). Arms the end-of-transmission close;
        // ignored for any other call (a late terminator of a pre-empted
        // call decoded after the next call opened) and while a marker is
        // already pending.
        CallBoundaryKind::VoiceEnd { call_id, air_ms, lc } => {
            if let Some(a) = active.as_mut() {
                if a.call_id == call_id && a.end_at_ms == 0 {
                    a.end_at_ms = now_unix_ms().max(1);
                    a.end_air_ms = air_ms;
                    a.end_lc = Some(lc);
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
    /// Change 057: next talker queued behind the on-air call.
    Queue,
}

fn handle_audio(chunk: AudioChunk, active: &mut Option<ActiveCall>) {
    if let Some(a) = active.as_mut() {
        // Change 054: an air-time attributed chunk of another call (the
        // previous call's in-flight tail) is not activity of this one —
        // it must neither keep this call alive nor stamp its first-audio
        // time.
        if chunk.airtime && chunk.call_id != 0 && chunk.call_id != a.call_id {
            return;
        }
        // Change 057: voice of this call aired AFTER its end marker means
        // the transmission did not end there. Chunks aired before it (the
        // transmission's last LDUs, still in the vocoder / pacer when the
        // terminator was decoded) do not cancel. With a queued grant such
        // voice is the next talker's, handed over by the NID / HDU paths.
        if a.end_at_ms != 0 && a.queued.is_none() && chunk.captured_at_ms > a.end_air_ms {
            a.cancel_end();
        }
        // The recorder routes by chunk.captured_at_ms vs the session
        // window. We refresh `last_audio_at_ms` here so the hybrid
        // close trigger sees audio activity as a keep-alive — needed
        // because the CC UPD heartbeat is unreliable mid-call (CC
        // decoder stalls during traffic — see IDLE_TIMEOUT_MS doc).
        let now = now_unix_ms();
        a.last_audio_at_ms = now;
        if a.first_audio_at_unix_ms.is_none() {
            a.first_audio_at_unix_ms = Some(now);
        }
        // Change 056: voice accounting for `/api/ui/state`.
        a.voice_frames += 1;
        if a.first_voice_at_ms == 0 {
            a.first_voice_at_ms = now;
        }
        a.last_voice_at_ms = now;
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

use super::{now_unix_ms, resume_needs_reset, Arc, CallTrackerEventKind, CallTrackerEventTx, CloseReason};
use crate::app::dibit_airtime::EpochKind;
use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio;
use crate::hardware::fpga;
use crate::protocol::p25::{self, control_channel::ControlChannelDecoder,
    traffic_chain::TrafficChain};
use crate::services::event_log::EventLog;
use crate::services::monitor::MonitorList;

// 2026-05-03 dual-DDC pivot: the polyphase-channelizer-specific
// helpers (`CHANNELIZER_M`, `CHANNELIZER_FFT_LAG`, `bit_reverse_6`,
// `offset_to_bin_and_nco`, plus their unit tests) have been retired.
// Retunes now write a frequency offset directly to the dedicated
// `traffic_ddc` NCO via `IpCore::retune_traffic_chain`, mirroring the
// control-side DDC. See `doc/changes/` for the dual-DDC pivot.

#[allow(clippy::too_many_arguments)]
pub fn spawn_grant_follower(
    follower_mgr: Arc<Mutex<TrafficChain>>,
    follower_core: Arc<Mutex<fpga::IpCore>>,
    follower_current_sample_rate_hz: Arc<std::sync::atomic::AtomicU32>,
    follower_current_rx_lo: Arc<AtomicI64>,
    // 2026-04-30: live DDC NCO crystal-trim shift, mirroring the
    // control chain's NCO programming. Read on every retune so the
    // traffic chain's offset_hz includes the same PPM correction the
    // control chain bakes in via `tuning.rs`. Previously this slot
    // was a static `f64 lo_ppm` captured at spawn from `args.lo_ppm`,
    // which left the traffic Costas loop absorbing the full residual
    // any time auto-PPM (or a manual `PUT /api/ppm`) had moved
    // `current_lo_shift_hz` away from the boot value — diagnosed via
    // `pll_dbg ≈ -5200` (≈ -480 Hz) on traffic vs `-2658` on control
    // in `2026-04-30-sync-trace`.
    follower_current_lo_shift_hz: Arc<AtomicI64>,
    follower_enabled: Arc<AtomicBool>,
    follower_imbe: Arc<ImbeForwarder>,
    follower_monitor: Arc<RwLock<MonitorList>>,
    // Change 063: talkgroup groups -> speakers, priority pre-emption.
    follower_routing: Arc<crate::services::ui_settings::RoutingPolicy>,
    follower_event_log: Arc<EventLog>,
    follower_traffic_decoder: Arc<RwLock<ControlChannelDecoder>>,
    mut grant_event_rx: Receiver<p25::events::P25Event>,
    follower_lock_freq: Arc<AtomicBool>,
    follower_boundary_tx: audio::CallBoundaryTx,
    follower_tracker_tx: CallTrackerEventTx,
    // 2026-05-03 seeding bake: shared converged-seed snapshot
    // published by the control-chain heartbeat. Read on each
    // freq-change retune to warm-start the traffic AGC / Costas PLL
    // / Gardner timing accumulators. None during heartbeat warmup
    // (~MIN_CLEAN_SAMPLES clean NIDs) — the retune falls back to
    // cold-start until the first commit.
    follower_converged_seeds: crate::app::seed_snapshot::ConvergedSeedsShared,
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

            // 2026-05-02 same-freq chain-reset gate. Tracks the last
            // frequency we programmed into the traffic DDC so we can
            // skip the LSM reset pulse on PTT bursts that stay on the
            // same channel. See `IpCore::retune_traffic_chain` for the
            // full operational rationale.
            let mut last_traffic_freq_hz: Option<u64> = None;

            // 2026-05-02 quality gate on state preservation. Skipping
            // the LSM reset pulse on a same-freq grant inherits the
            // chain's end-of-call register state. If the prior call
            // ended cleanly (TG change, healthy IMBE rate, low silent
            // ratio), that state was a good steady-state lock and is
            // worth keeping. If the prior call ended with the chain
            // mid-fade (timeout, high silent ratio, near-zero IMBE),
            // its state is degenerate and should be cleared. Captured
            // at CallClose; consumed at the next retune.
            #[derive(Clone, Copy)]
            struct LastCallQuality {
                freq_hz: u64,
                imbe_extracted: u64,
                silent_frames: u64,
                close_reason: CloseReason,
            }
            impl LastCallQuality {
                fn was_clean(&self) -> bool {
                    // 2026-05-03 (build `quality-coast-no-los` follow-up):
                    // dropped the `close_reason == TgChange` requirement.
                    // On a trunked system calls almost always close with
                    // `Timeout` (10 s no UPD/audio after the speaker
                    // unkeys), not TgChange — TgChange only fires when a
                    // new grant for a different TG arrives on the same
                    // physical freq, which is rare. Field measurement
                    // (build 2026-05-03-quality-coast-no-los, 7 retunes
                    // observed): `prev_clean = false` on every retune,
                    // gate always resets. The IMBE + silent ratio fully
                    // characterise call quality on their own; close
                    // reason only matters as a "did the chain crash"
                    // signal which `StreamLag` already flags.
                    //
                    // Healthy: ≥1.5 s of decoded audio (≥30 IMBE @ 20 ms),
                    // < 5 % silent, did NOT close due to broadcast lag.
                    self.imbe_extracted >= 30
                        && self.silent_frames * 20 < self.imbe_extracted
                        && !matches!(self.close_reason, CloseReason::StreamLag)
                }
            }
            let mut last_call_quality: Option<LastCallQuality> = None;
            // Change 057: (TG, frequency, unix ms) of the live call the
            // lifecycle last closed by `Timeout`, for
            // `refollow_on_update`.
            let mut last_timeout_close: Option<(u16, u64, u64)> = None;
            // Change 059: (TG, frequency, unix ms) of the newest clear
            // grant the sticky gate rejected, for the same re-follow.
            let mut last_sticky_reject: Option<(u16, u64, u64)> = None;

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
                                let g = if g.is_update {
                                    let mgr = follower_mgr.lock().await;
                                    let chain_tg = mgr.current_talkgroup();
                                    drop(mgr);
                                    if chain_tg.map(|t| t.0 == g.talkgroup.0)
                                        .unwrap_or(false)
                                    {
                                        send_cc_boundary(&g, None);
                                        continue;
                                    }
                                    // Change 057: the CC still announces
                                    // the call we closed for lack of
                                    // keep-alive: follow it again, as a
                                    // (source-less) grant through every
                                    // gate below.
                                    let now_ms = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0);
                                    let refollow_timeout = super::refollow_on_update(
                                        last_timeout_close,
                                        g.talkgroup.0,
                                        g.frequency_hz,
                                        chain_tg.is_none(),
                                        now_ms,
                                        super::REFOLLOW_WINDOW_MS,
                                    );
                                    // Change 059: likewise a clear grant the
                                    // sticky gate rejected, once the chain is
                                    // idle or its call has ended (the rest of
                                    // that transmission is still on the air).
                                    let chain_free = chain_tg.map_or(true, |t| {
                                        super::end_marker_frees_chain(
                                            t.0,
                                            super::unpack_end_marker(
                                                follower_imbe.active_end_marker
                                                    .load(Ordering::Relaxed)),
                                            now_ms,
                                        )
                                    });
                                    let refollow_sticky = !refollow_timeout
                                        && super::refollow_on_update(
                                            last_sticky_reject,
                                            g.talkgroup.0,
                                            g.frequency_hz,
                                            chain_free,
                                            now_ms,
                                            super::REFOLLOW_STICKY_MS,
                                        );
                                    if refollow_timeout || refollow_sticky {
                                        let (why, reason) = if refollow_timeout {
                                            last_timeout_close = None;
                                            ("call closed by timeout", "refollow_on_update")
                                        } else {
                                            last_sticky_reject = None;
                                            ("rejected while the chain was busy", "refollow_after_sticky")
                                        };
                                        follower_event_log.push(
                                            LogCategory::Traffic,
                                            format!(
                                                "re-follow TG={} on grant update {:.4} MHz \
                                                 ({}, still announced)",
                                                g.talkgroup.0, freq_mhz, why,
                                            ),
                                            serde_json::json!({
                                                "tg":        g.talkgroup.0,
                                                "channel":   g.channel.0,
                                                "frequency": g.frequency_hz,
                                                "reason":    reason,
                                            }),
                                        );
                                        let mut regrant = g.clone();
                                        regrant.is_update = false;
                                        regrant
                                    } else {
                                        // Updates are keep-alives. They must
                                        // NEVER bring the chain out of Idle
                                        // or pull it onto a different TG —
                                        // initial GVCG / GVCG_EXP is the
                                        // only acquisition trigger. Without
                                        // this gate, a TG whose initial
                                        // grant we missed (or one whose
                                        // encryption flag we couldn't
                                        // learn — UPD opcodes carry no
                                        // service_options) would slip past
                                        // the encrypted check and force a
                                        // retune to a freq we shouldn't
                                        // touch. Field hit 2026-04-30: TG
                                        // 700 (encrypted) update pulled the
                                        // chain off-Idle.
                                        send_cc_boundary(&g, Some("update_no_lock"));
                                        continue;
                                    }
                                } else {
                                    g
                                };

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

                                // Change 063: speaker groups. A talkgroup
                                // whose group is on neither speaker, or an
                                // ungrouped one with "other talkgroups"
                                // off, is not followed.
                                if follower_routing.route(g.talkgroup.0).is_none() {
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "reject: TG={} not on a speaker",
                                            g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "tg":     g.talkgroup.0,
                                            "reason": "speaker_off",
                                        }),
                                    );
                                    send_cc_boundary(&g, Some("speaker_off"));
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
                                    // Change 054: the old TG's lock ends
                                    // here on the air-time axis.
                                    follower_imbe.mark_epoch(
                                        EpochKind::TgChange, false);
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
                                // Forensics override (2026-05-03 Track 2):
                                // if the operator armed forensics with
                                // `follow_encrypted=1`, follow encrypted
                                // grants too. Audio is still garbled but
                                // dibits + wideband are usable for HDL
                                // diff. See app/forensics.rs.
                                #[cfg(target_os = "linux")]
                                let forensics_override =
                                    crate::app::forensics::follow_encrypted_enabled();
                                #[cfg(not(target_os = "linux"))]
                                let forensics_override = false;
                                if (g.encrypted || tg_known_enc) && !forensics_override {
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
                                        // Change 054: gate closes at this
                                        // air-time cut (framer reset
                                        // there); the pause below adds
                                        // its own hardware cut.
                                        follower_imbe.mark_epoch(
                                            EpochKind::TgChange, true);
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
                                            let core = follower_core.lock().await;
                                            // M2B 2026-05-02: pause via
                                            // traffic_lsm_enable=0 so the
                                            // new mux-fed chain stops
                                            // emitting NID events on the
                                            // encrypted teardown.
                                            core.pause_traffic_chain();
                                            // Change 057: the same-freq
                                            // resume re-enables it.
                                            follower_imbe
                                                .traffic_paused_by_teardown
                                                .store(true, Ordering::Relaxed);
                                        }
                                        // Reset the traffic framer --
                                        // it's mid-frame on encrypted
                                        // data and would carry bogus
                                        // state into the next lock.
                                        // Change 054: in airtime mode the
                                        // reader resets it at the cut
                                        // recorded above instead (a reset
                                        // now would cut whatever older
                                        // dibits it is still decoding).
                                        if !follower_imbe.epochs_active() {
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
                                // Change 059: the locked call's transmission
                                // has ended (end marker pending for
                                // `END_PREEMPT_AFTER_MS`): the other TG's
                                // grant takes the chain now instead of
                                // after the lifecycle's `end_grace_ms`.
                                // Change 063: or the grant's group ranks
                                // above the locked call's (speaker groups,
                                // pre-emption on): it takes the chain now.
                                let end_marker = super::unpack_end_marker(
                                    follower_imbe.active_end_marker.load(Ordering::Relaxed));
                                let preempt = locked_tg_final
                                    .filter(|t| t.0 != g.talkgroup.0)
                                    .and_then(|t| {
                                        if super::end_marker_frees_chain(
                                            t.0, end_marker, now_unix_ms())
                                        {
                                            Some((t, "end_marker_preempt"))
                                        } else if follower_routing.preempts(g.talkgroup.0, t.0) {
                                            Some((t, "priority_preempt"))
                                        } else {
                                            None
                                        }
                                    });
                                if let Some((tg, reason)) = preempt {
                                    let ended_ms = end_marker
                                        .map_or(0, |(_, at)| now_unix_ms().saturating_sub(at));
                                    let why = if reason == "priority_preempt" {
                                        "higher-priority group".to_string()
                                    } else {
                                        format!("ended {ended_ms} ms ago (end marker)")
                                    };
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "pre-empt: TG={} {}, follow TG={}",
                                            tg.0, why, g.talkgroup.0,
                                        ),
                                        serde_json::json!({
                                            "prev_tg":  tg.0,
                                            "new_tg":   g.talkgroup.0,
                                            "ended_ms": ended_ms,
                                            "reason":   reason,
                                        }),
                                    );
                                    mgr.force_idle();
                                    follower_imbe.current_talkgroup
                                        .store(0, Ordering::Relaxed);
                                    // Change 054: the old TG's lock ends
                                    // here on the air-time axis.
                                    follower_imbe.mark_epoch(
                                        EpochKind::TgChange, false);
                                }
                                let locked_tg_final = mgr.current_talkgroup();
                                if let Some(tg) = locked_tg_final {
                                    if tg.0 != g.talkgroup.0 {
                                        // Change 059: remember a clear grant
                                        // (the encrypted gate is above) for
                                        // `refollow_on_update` once the chain
                                        // frees up.
                                        if let Some(f) = g.frequency_hz {
                                            last_sticky_reject =
                                                Some((g.talkgroup.0, f, now_unix_ms()));
                                        }
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
                                //
                                // Change 054: the boundary now goes out
                                // AFTER handle_grant_event (sync, µs),
                                // so that when a retune follows, the
                                // grant-hold cut below is recorded
                                // before the lifecycle can publish the
                                // new call_id: dibits still arriving from
                                // the old frequency are gated instead of
                                // being labelled with the new call.
                                let pre_state = mgr.state_label();
                                // Live context before the grant touches
                                // the forwarder atomics, to decide which
                                // epoch cut to record.
                                let ctx_before = follower_imbe.live_context();
                                let retune = handle_grant_event(
                                    &g, &mut mgr, &follower_imbe
                                );
                                let post_state = mgr.state_label();
                                drop(mgr);
                                let epochs = follower_imbe.epochs_active();
                                if retune && epochs {
                                    follower_imbe.mark_epoch_ctx(
                                        EpochKind::GrantHold,
                                        crate::app::dibit_airtime::SegmentContext {
                                            tg: 0,
                                            ..ctx_before
                                        },
                                        true,
                                    );
                                }
                                send_cc_boundary(&g, None);

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
                                    // 2026-04-30: read the LIVE DDC
                                    // NCO crystal-trim shift, the same
                                    // value `tuning.rs` uses when
                                    // programming the control chain's
                                    // NCO. Static `lo_ppm` was wrong
                                    // here — it never tracked
                                    // auto-PPM apply, manual
                                    // `PUT /api/ppm`, or boot-loaded
                                    // persisted shifts, leaving traffic
                                    // Costas with the full residual.
                                    let nco_lo_shift_hz =
                                        follower_current_lo_shift_hz
                                            .load(std::sync::atomic::Ordering::Relaxed)
                                            as f64;
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
                                    // Change 054: airtime mode resets
                                    // at the retune's epoch cut (the
                                    // IpCore hook records it), so the
                                    // old call's in-flight dibits are
                                    // still decoded to their end.
                                    if !epochs {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }

                                    // 2026-05-03 dual-DDC: retune writes
                                    // the traffic DDC NCO to `offset_hz`,
                                    // pulses traffic LSM reset, enables
                                    // the chain. AGC seed is implicit in
                                    // the AGC tracker — legacy
                                    // `agc_seed_for_freq` cache is
                                    // currently a no-op since the new HDL
                                    // doesn't accept a seed register
                                    // (revisit if convergence is slow).
                                    let _ = follower_imbe
                                        .agc_seed_for_freq(freq_hz)
                                        .unwrap_or(0);
                                    // 2026-05-03 quality-gated coast policy
                                    // (Option C). Original same-freq gate
                                    // required `q.freq_hz == freq_hz` to
                                    // preserve state; SDRTrunk-source review
                                    // showed cross-freq state preservation
                                    // also works (their AGC + timing
                                    // accumulators carry over across freq
                                    // changes, only PLL is zeroed).
                                    //
                                    // New rule: COAST if the previous call
                                    // was clean (regardless of freq), RESET
                                    // if it was degenerate. The chain is
                                    // re-acquired naturally through the FIR
                                    // flush + Costas re-lock when coasting;
                                    // a reset clears AGC/PLL/timing state
                                    // that drifted into a bad attractor
                                    // during a previous noisy call (the
                                    // failure mode observed under pure
                                    // coast-no-reset: chain decoded noise
                                    // during inter-call gaps, accumulated
                                    // bad AGC saturation + random PLL phase,
                                    // ~60% of subsequent calls returned 0
                                    // IMBE).
                                    let same_freq =
                                        last_traffic_freq_hz == Some(freq_hz);
                                    let prev_clean = last_call_quality
                                        .as_ref()
                                        .map(|q| q.was_clean())
                                        .unwrap_or(false);
                                    let freq_changed = !prev_clean;
                                    // 2026-05-03 seeding bake: lift the
                                    // current converged seeds (if any)
                                    // before taking the IpCore mutex.
                                    // None during warmup; once the
                                    // control-chain heartbeat has seen
                                    // MIN_CLEAN_SAMPLES clean NIDs the
                                    // tuple becomes Some and every
                                    // freq-change retune writes them
                                    // before pulsing reset.
                                    //
                                    // 2026-05-03 PLL-only gate: the
                                    // initial bake wrote all 3 seeds
                                    // but on-target measurement showed
                                    // First-IMBE stayed at 3-3.5 s on
                                    // cold-start retunes. Hypothesis:
                                    // (a) AGC seed cross-applies a
                                    // gain converged for the CC's
                                    // signal level, which differs from
                                    // each traffic channel's level →
                                    // forces a slow IIR migration that
                                    // is worse than starting from
                                    // GAIN_INIT=1.0; (b) timing seed
                                    // bypasses the FIFO-warmup delay
                                    // (cold-start init is sps_q12 +
                                    // 7*ONE_Q12 specifically to wait
                                    // for the lookahead FIFO to fill).
                                    // Zero AGC + timing → HDL falls
                                    // back to its init values.
                                    let seed_tuple: Option<(u32, i16, i32)> = {
                                        let slot = follower_converged_seeds
                                            .read().await;
                                        slot.as_ref().map(|s| (
                                            0,            // AGC: cold-start
                                            s.pll_seed,   // empirically uniform
                                            0,            // timing: FIFO warmup
                                        ))
                                    };
                                    let retune_result = {
                                        let core = follower_core.lock().await;
                                        core.retune_traffic_chain(
                                            offset_hz as f64,
                                            sample_rate_now,
                                            freq_changed,
                                            seed_tuple,
                                        )
                                    };
                                    // 2026-05-03 seeding bake: log the
                                    // seed values applied so the
                                    // diagnostic trail correlates
                                    // First-IMBE timing with seeds.
                                    if freq_changed {
                                        match seed_tuple {
                                            Some((a, p, t)) => tracing::info!(
                                                target: "p25_traffic",
                                                "retune seeded: agc=0x{:05x} \
                                                 pll={} timing={}",
                                                a, p, t,
                                            ),
                                            None => tracing::info!(
                                                target: "p25_traffic",
                                                "retune cold-start: \
                                                 ConvergedSeeds not yet \
                                                 published (heartbeat warmup)"
                                            ),
                                        }
                                    }
                                    if let Err(ref e) = retune_result {
                                        tracing::warn!(
                                            target: "p25_traffic",
                                            "retune_traffic_chain failed: {e}"
                                        );
                                    } else {
                                        last_traffic_freq_hz = Some(freq_hz);
                                        // The retune enabled the chain.
                                        follower_imbe
                                            .traffic_paused_by_teardown
                                            .store(false, Ordering::Relaxed);
                                    }
                                    // Change 054: the new call's context
                                    // starts right after the retune's
                                    // hardware cut (whose settle discard
                                    // covers the gap between the two).
                                    // A failed retune never left the old
                                    // frequency: the grant hold keeps the
                                    // gate closed there.
                                    if retune_result.is_ok() {
                                        let ctx_after = follower_imbe.live_context();
                                        let kind = if ctx_after.tg != ctx_before.tg {
                                            EpochKind::TgChange
                                        } else {
                                            EpochKind::CtxUpdate
                                        };
                                        follower_imbe.mark_epoch(kind, false);
                                    }
                                    tracing::info!(
                                        target: "p25_traffic",
                                        "retune (dual-DDC): TG={} channel={:?} \
                                         freq={} Hz offset={:+} Hz \
                                         same_freq={} prev_clean={} freq_changed={}",
                                        g.talkgroup.0, g.channel,
                                        freq_hz, offset_hz,
                                        same_freq, prev_clean, freq_changed,
                                    );
                                    // 2026-05-03 quality-gated coast log:
                                    // `freq_changed` is the actual gate
                                    // result (true → reset, false → coast).
                                    // `policy` reflects the decision; the
                                    // `prev_clean_q` / `same_freq_q` fields
                                    // expose the inputs so we can correlate
                                    // per-call First-IMBE outcomes with the
                                    // gate state.
                                    let policy = if freq_changed { "reset" } else { "coast" };
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "retune TG={} -> {:.4} MHz \
                                             (offset {:+} Hz) {}",
                                            g.talkgroup.0,
                                            freq_hz as f64 / 1e6,
                                            offset_hz,
                                            policy,
                                        ),
                                        serde_json::json!({
                                            "tg":             g.talkgroup.0,
                                            "channel":        g.channel.0,
                                            "frequency":      freq_hz,
                                            "offset_hz":      offset_hz,
                                            "framer_reset":   freq_changed,
                                            "policy":         policy,
                                            "prev_clean_q":   prev_clean,
                                            "same_freq_q":    same_freq,
                                        }),
                                    );
                                } else if pre_state == "Idle" && post_state != "Idle" {
                                    // M2B 2026-05-02: same-freq new call
                                    // (Idle -> Active on the bin we
                                    // last followed). Per design, the
                                    // chain stays parked + enabled on
                                    // the last freq through CallClose,
                                    // so the PLL/AGC carry across the
                                    // inter-call gap. Only thing that
                                    // needs reset is the PS framer
                                    // state machine (it was mid-search
                                    // when the previous call ended).
                                    //
                                    // No NCO re-write unless the chain
                                    // state went stale while parked
                                    // (`resume_needs_reset`), then the
                                    // same reset retune as a frequency
                                    // change. Subsequent dedup'd grants
                                    // for this same call are no-ops at
                                    // this layer.
                                    let last_imbe = follower_imbe
                                        .last_imbe_at_millis
                                        .load(Ordering::Relaxed);
                                    let now_ms = now_unix_ms();
                                    let ms_since_voice = (last_imbe != 0)
                                        .then(|| now_ms.saturating_sub(last_imbe));
                                    let (pll_pre, clamp) = {
                                        let core = follower_core.lock().await;
                                        let (pre, _) = core.traffic_lsm_debug();
                                        (pre, core.core_version().pll_clamp_q213())
                                    };
                                    let reset = resume_needs_reset(pll_pre, ms_since_voice, clamp);
                                    // Change 057: an encrypted teardown
                                    // paused the chain (lsm_enable = 0)
                                    // and the coast path writes no NCO,
                                    // so nothing else would re-enable
                                    // it: the channel stayed dead until
                                    // the next cross-frequency retune.
                                    // A reset retune enables the chain.
                                    let reenable = follower_imbe
                                        .traffic_paused_by_teardown
                                        .swap(false, Ordering::Relaxed);
                                    let mut reset_ok = false;
                                    if reset {
                                        let freq_hz = g.frequency_hz.unwrap_or(0);
                                        let rx_lo_now = follower_current_rx_lo
                                            .load(Ordering::Relaxed);
                                        let sample_rate_now =
                                            follower_current_sample_rate_hz
                                                .load(Ordering::Relaxed) as f64;
                                        let nco_lo_shift_hz =
                                            follower_current_lo_shift_hz
                                                .load(Ordering::Relaxed) as f64;
                                        let offset_hz = freq_hz as f64
                                            - rx_lo_now as f64 + nco_lo_shift_hz;
                                        let seed_tuple: Option<(u32, i16, i32)> = {
                                            let slot = follower_converged_seeds
                                                .read().await;
                                            slot.as_ref().map(|s| (0, s.pll_seed, 0))
                                        };
                                        if !epochs {
                                            let mut dec = follower_traffic_decoder
                                                .write().await;
                                            dec.reset_framer_state();
                                        }
                                        // Records the Retune epoch cut
                                        // with its settle discard (054
                                        // IpCore hook).
                                        let res = {
                                            let core = follower_core.lock().await;
                                            core.retune_traffic_chain(
                                                offset_hz, sample_rate_now,
                                                true, seed_tuple,
                                            )
                                        };
                                        match res {
                                            Ok(_) => reset_ok = true,
                                            Err(e) => tracing::warn!(
                                                target: "p25_traffic",
                                                "same-freq reset retune failed: {e}"
                                            ),
                                        }
                                        if epochs {
                                            follower_imbe.mark_epoch(
                                                EpochKind::TgChange, !reset_ok);
                                        }
                                    } else if epochs {
                                        // Change 054: airtime mode applies
                                        // the framer reset + new context at
                                        // this air-time cut.
                                        follower_imbe.mark_epoch(
                                            EpochKind::TgChange, true);
                                    } else {
                                        let mut dec = follower_traffic_decoder
                                            .write().await;
                                        dec.reset_framer_state();
                                    }
                                    if reenable && !reset_ok {
                                        // Records the Resume epoch cut
                                        // (054 IpCore hook).
                                        let core = follower_core.lock().await;
                                        core.set_traffic_lsm_enable(true);
                                    }
                                    let idle_s = ms_since_voice
                                        .map(|ms| format!("{:.1} s", ms as f64 / 1000.0))
                                        .unwrap_or_else(|| "never".into());
                                    follower_event_log.push(
                                        LogCategory::Traffic,
                                        format!(
                                            "same-freq resume TG={} ({}; pll {} \
                                             idle {}{})",
                                            g.talkgroup.0,
                                            if reset_ok { "reset" } else { "coast" },
                                            pll_pre, idle_s,
                                            if reenable { "; re-enabled after encrypted pause" } else { "" },
                                        ),
                                        serde_json::json!({
                                            "tg":             g.talkgroup.0,
                                            "channel":        g.channel.0,
                                            "frequency":      g.frequency_hz,
                                            "framer_reset":   true,
                                            "lsm_reset":      reset_ok,
                                            "nco_write":      reset_ok,
                                            "policy":         if reset_ok { "reset" } else { "coast" },
                                            "lsm_enable":     if reenable { "re-enabled" } else { "unchanged" },
                                            "pll_pre_resume": pll_pre,
                                            "ms_since_voice": ms_since_voice,
                                        }),
                                    );
                                } else if follower_imbe.live_context() != ctx_before {
                                    // Change 054: grant refresh for the
                                    // active call changed its source /
                                    // encryption: frames completing
                                    // after this point carry the update,
                                    // in-flight ones keep the old values.
                                    follower_imbe.mark_epoch(
                                        EpochKind::CtxUpdate, false);
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

                        // Change 054 gating fix: only a CallClose of the
                        // LIVE call releases the chain. The lifecycle
                        // also emits CallClose for (a) the synthetic
                        // open/close pair of every not-followed grant
                        // (sticky-lock / encrypted / monitor-list
                        // rejects on other channels) and (b) the
                        // predecessor of a preempting grant — after this
                        // follower already moved the chain to the new
                        // grant. Acting on those zeroed the TG of the
                        // call actually being followed, so its dibits
                        // were gated off until another primary grant
                        // re-acquired it. `current_call_id` is the live
                        // call (lifecycle publishes a successor before
                        // closing its predecessor; between calls it
                        // holds the last call's id, so a Timeout close
                        // still matches).
                        let live_call_id = follower_imbe
                            .current_call_id.load(Ordering::Relaxed);
                        if event.call_id != live_call_id {
                            tracing::debug!(
                                target: "p25_traffic",
                                "CallClose {:?} for call_id={} ignored \
                                 (live call_id={})",
                                reason, event.call_id, live_call_id,
                            );
                            continue;
                        }

                        // Snapshot the just-closed call's quality stats
                        // for the next retune's chain-reset gate.
                        // Change 057: this call's own counts by call_id
                        // (`call_counts`); pre-057 read global minus the
                        // per-HDU baseline, which mixed calls. The tail
                        // still in flight is not in yet: fine for a
                        // quality gate (≥ 30 IMBE, < 5 % silent).
                        if let Some(freq_hz) = last_traffic_freq_hz {
                            let counts = follower_imbe
                                .call_counts
                                .get(event.call_id)
                                .unwrap_or_default();
                            let call_imbe = counts.imbe_extracted;
                            let call_silent = counts.vocoder_silent;
                            last_call_quality = Some(LastCallQuality {
                                freq_hz,
                                imbe_extracted: call_imbe,
                                silent_frames: call_silent,
                                close_reason: reason,
                            });
                            tracing::info!(
                                target: "p25_traffic",
                                "call quality captured for next retune: \
                                 freq={} Hz imbe={} silent={} reason={:?} clean={}",
                                freq_hz, call_imbe, call_silent, reason,
                                last_call_quality.as_ref().unwrap().was_clean(),
                            );
                        }

                        // Diagnostic lock keeps the chain on the
                        // parked freq even at call end so the demod
                        // stays running for measurement.
                        if follower_lock_freq.load(Ordering::Relaxed) {
                            continue;
                        }

                        let mut mgr = follower_mgr.lock().await;
                        let pre_close_tg = mgr.current_talkgroup();
                        // Change 057: remember a timeout close, so a grant
                        // update that still announces this call re-follows
                        // it (`refollow_on_update`).
                        let parked_freq = follower_imbe
                            .current_frequency_hz
                            .load(Ordering::Relaxed);
                        last_timeout_close = match (reason, pre_close_tg) {
                            (CloseReason::Timeout, Some(tg)) if parked_freq != 0 => {
                                let now_ms = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_millis() as u64)
                                    .unwrap_or(0);
                                Some((tg.0, parked_freq, now_ms))
                            }
                            _ => None,
                        };
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
                        // Change 054: the gate closes at this air-time
                        // cut. Dibits produced before it (the closing
                        // call's tail, still in flight in the ring) are
                        // decoded under the closing call; the pre-054
                        // reader dropped every batch delivered after
                        // this point.
                        follower_imbe.mark_epoch(EpochKind::CallClose, false);
                    }
                }
            }
            tracing::warn!("grant follower task exiting (channel closed)");
        });
}

} // mod routing

#[cfg(target_os = "linux")]
pub use routing::spawn_grant_follower;

#[cfg(test)]
#[path = "grant_follower_tests.rs"]
mod tests;

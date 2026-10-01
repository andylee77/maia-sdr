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
use crate::hardware::traffic_lane::Lane;
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

/// Was `t` (wall ms) within `window_ms` before `now`? Change 067: a time
/// after `now` is not recent — the board clock was stepped back (set from
/// the site or a browser), and a stale entry must not block grants until
/// the clock catches up with it.
fn within(now: u64, t: u64, window_ms: u64) -> bool {
    t <= now && now - t < window_ms
}

// ── Public types (cross-platform; consumed by AppState +
//    grant_stats + recorder + dashboard API) ────────────────────

#[derive(Debug, Clone)]
pub struct CallTrackerEvent {
    pub call_id: u64,
    pub timestamp_unix_ms: u64,
    pub kind: CallTrackerEventKind,
    /// Change 066: traffic chain of a followed call; `None` for a call
    /// that was not followed.
    pub lane: Option<Lane>,
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
        /// Change 073: the site the call is on (active when its grant
        /// opened it). Summaries, recordings and the history keep it.
        site: String,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    /// Change 066: traffic chain of a followed call (`None`: not followed).
    lane: Option<Lane>,
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
            lane: None,
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
    lane: Option<Lane>,
) {
    let _ = tx.send(CallTrackerEvent {
        call_id,
        timestamp_unix_ms: started_unix_ms,
        kind: CallTrackerEventKind::CallOpen {
            tg, nac, source, freq_hz, channel,
            encrypted, not_followed, opened_via,
            baseline_frames_submitted,
            site: crate::services::lo_plan::active_site(),
        },
        lane,
    });
}

fn emit_close(
    tx: &CallTrackerEventTx,
    call: &ActiveCall,
    reason: CloseReason,
    final_source: Option<u32>,
    expected_submit_count: u64,
) {
    let open_ms = call.started_instant.elapsed().as_millis() as u64;
    emit_close_with(tx, call, reason, final_source, expected_submit_count, now_unix_ms(), open_ms);
}

/// Change 065: close with an explicit end time and open duration (a
/// not-followed call ends at its last announcement, not when the sweep
/// notices).
fn emit_close_with(
    tx: &CallTrackerEventTx,
    call: &ActiveCall,
    reason: CloseReason,
    final_source: Option<u32>,
    expected_submit_count: u64,
    ended_unix_ms: u64,
    open_ms: u64,
) {
    let _ = tx.send(CallTrackerEvent {
        call_id: call.call_id,
        timestamp_unix_ms: now_unix_ms(),
        kind: CallTrackerEventKind::CallClose {
            reason,
            final_source,
            final_actual_speaker: call.actual_speaker,
            started_unix_ms: call.started_unix_ms,
            ended_unix_ms,
            first_audio_at_unix_ms: call.first_audio_at_unix_ms,
            first_hdu_at_unix_ms: call.first_hdu_at_unix_ms,
            expected_submit_count,
            sources_observed: call.sources_observed.clone(),
            last_upd_at_unix_ms: call.last_upd_at_ms,
            open_ms,
            end_lc: call.end_lc,
        },
        lane: call.lane,
    });
}

/// Change 065: a call the follower did not take (encrypted, busy on
/// another call, monitor list or speaker groups). SDRTrunk lists such
/// calls with their channel time, grant to the last control-channel
/// update; here the record stays open while the control channel keeps
/// announcing the call (repeat grants, GRP_VCH_GRNT_UPD) and closes at
/// its last announcement once none came for `hang_ms`, or when its
/// channel is granted to another call.
struct NfCall {
    call: ActiveCall,
    last_seen_ms: u64,
}

#[derive(Default)]
struct NfCalls(Vec<NfCall>);

impl NfCalls {
    /// A not-followed grant: the same call re-announced (same TG and
    /// channel, same or unknown source, seen within `hang_ms`) extends
    /// its record; otherwise a new record opens, ending any other
    /// not-followed record on that channel.
    #[allow(clippy::too_many_arguments)]
    fn grant(
        &mut self,
        tx: &CallTrackerEventTx,
        next_call_id: &mut u64,
        tg: u16,
        nac: u16,
        source: Option<u32>,
        freq_hz: Option<u64>,
        channel: Option<String>,
        encrypted: bool,
        not_followed: Option<&'static str>,
        now: u64,
        baseline: u64,
        hang_ms: u64,
    ) {
        if let Some(c) = self.0.iter_mut().find(|c| c.call.tg == tg && c.call.freq_hz == freq_hz) {
            let same_source = match (source, c.call.source) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            };
            if same_source && now.saturating_sub(c.last_seen_ms) <= hang_ms {
                c.last_seen_ms = now;
                c.call.last_upd_at_ms = now;
                if c.call.source.is_none() {
                    c.call.source = source;
                }
                return;
            }
        }
        self.close_channel(tx, freq_hz);
        let id = *next_call_id;
        *next_call_id += 1;
        emit_open(tx, id, tg, nac, source, freq_hz, channel.clone(), encrypted,
                  not_followed, OpenReason::CcGrant, baseline, now, None);
        let call = ActiveCall::open(id, tg, nac, source, freq_hz, channel, encrypted,
                                    not_followed, now, baseline, now);
        self.0.push(NfCall { call, last_seen_ms: now });
    }

    /// A grant update for `tg` (on `freq_hz` when known).
    fn update(&mut self, tg: u16, freq_hz: Option<u64>, now: u64) {
        for c in self.0.iter_mut()
            .filter(|c| c.call.tg == tg && (freq_hz.is_none() || c.call.freq_hz == freq_hz))
        {
            c.last_seen_ms = now;
            c.call.last_upd_at_ms = now;
        }
    }

    /// Another call now holds `freq_hz`: its not-followed records end.
    fn close_channel(&mut self, tx: &CallTrackerEventTx, freq_hz: Option<u64>) {
        if freq_hz.is_some() {
            self.close_where(tx, |c| c.call.freq_hz == freq_hz);
        }
    }

    /// Records not announced for `hang_ms` end at their last announcement.
    fn sweep(&mut self, tx: &CallTrackerEventTx, now: u64, hang_ms: u64) {
        self.close_where(tx, |c| now.saturating_sub(c.last_seen_ms) > hang_ms);
    }

    fn close_where(&mut self, tx: &CallTrackerEventTx, pred: impl Fn(&NfCall) -> bool) {
        let mut i = 0;
        while i < self.0.len() {
            if pred(&self.0[i]) {
                let c = self.0.remove(i);
                let open_ms = c.last_seen_ms.saturating_sub(c.call.started_unix_ms);
                emit_close_with(tx, &c.call, CloseReason::Timeout, c.call.source,
                                c.call.baseline_frames_submitted, c.last_seen_ms, open_ms);
            } else {
                i += 1;
            }
        }
    }
}

fn emit_source_update(
    tx: &CallTrackerEventTx,
    call: &ActiveCall,
    new_source: u32,
    via: SourceUpdateVia,
) {
    let _ = tx.send(CallTrackerEvent {
        call_id: call.call_id,
        timestamp_unix_ms: now_unix_ms(),
        kind: CallTrackerEventKind::SourceUpdate { new_source, via },
        lane: call.lane,
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
    let mut call = ActiveCall::open(
        call_id, g.tg, g.nac, g.source, g.freq_hz,
        g.channel.clone(), g.encrypted, None, now, baseline, now,
    );
    call.lane = Some(forwarder.lane);
    *active = Some(call);
    emit_open(
        tx, call_id, g.tg, g.nac, g.source, g.freq_hz, g.channel,
        g.encrypted, None, opened_via, baseline, now, Some(forwarder.lane),
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

/// Change 066: the lifecycle's view of one traffic chain: its open call,
/// the forwarder that decodes it, and the snapshot the HTTP side reads.
pub(crate) struct Slot {
    active: Option<ActiveCall>,
    forwarder: Arc<ImbeForwarder>,
    shared: ActiveCallShared,
}

impl Slot {
    pub(crate) fn new(forwarder: Arc<ImbeForwarder>, shared: ActiveCallShared) -> Self {
        Slot { active: None, forwarder, shared }
    }

    fn lane(&self) -> Lane {
        self.forwarder.lane
    }

    /// Close the open call, if any, for `reason`.
    fn close(&mut self, tx: &CallTrackerEventTx, reason: CloseReason) {
        if let Some(call) = self.active.take() {
            let expected = self.forwarder.frames_submitted.load(Ordering::Relaxed);
            emit_close(tx, &call, reason, call.source, expected);
        }
    }

    /// The open call is on `freq_hz` (known).
    fn holds(&self, freq_hz: Option<u64>) -> bool {
        freq_hz.is_some() && self.active.as_ref().is_some_and(|a| a.freq_hz == freq_hz)
    }
}

/// Change 066: the slot of `lane` (the first slot for `None` or a lane
/// without a slot).
fn slot_index(slots: &[Slot], lane: Option<Lane>) -> usize {
    lane.and_then(|l| slots.iter().position(|s| s.lane() == l)).unwrap_or(0)
}

/// Change 057: the periodic close sweep (every `TIMEOUT_TICK_MS`), every
/// chain's call and the not-followed calls.
#[allow(clippy::too_many_arguments)]
fn sweep(
    slots: &mut [Slot],
    nf: &mut NfCalls,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    now: u64,
    hang_ms: u64,
    end_grace_ms: u64,
) {
    // Change 065: not-followed calls the control channel stopped
    // announcing.
    nf.sweep(tx, now, hang_ms);
    for slot in slots.iter_mut() {
        sweep_slot(&mut slot.active, next_call_id, tx, &slot.forwarder, now, hang_ms, end_grace_ms);
    }
}

/// Change 057: one chain's sweep. A queued grant takes over when this
/// call's end grace ran out, its no-keep-alive timeout is due, or the
/// grant waited `QUEUED_GRANT_MAX_MS`; otherwise `close_due` decides.
fn sweep_slot(
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
    // Change 066: per traffic chain, its forwarder and the snapshot of
    // its open call. One lifecycle serves every chain: one call-id
    // space, one not-followed list, and the cross-chain rules (a
    // channel is held by one call).
    lanes: Vec<(Arc<ImbeForwarder>, ActiveCallShared)>,
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
        let mut slots: Vec<Slot> = lanes.into_iter()
            .map(|(forwarder, shared)| Slot::new(forwarder, shared))
            .collect();
        let mirror = |slots: &[Slot], policy: &CallPolicy| {
            for s in slots {
                mirror_active(&s.active, &s.shared, &s.forwarder, policy);
            }
        };
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
        // Change 065: open not-followed calls (channel time).
        let mut nf = NfCalls::default();
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
                            // Change 071a: keep the open calls. The next
                            // grant update or voice NID refreshes them,
                            // and the silence timeout closes one that
                            // really ended; closing every call on a
                            // short stall cut live audio.
                            tracing::warn!(
                                target: "p25_call_lifecycle",
                                "boundary lagged by {n} events; open calls kept",
                            );
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };
                    handle_boundary(
                        boundary, &mut slots, &mut nf, &mut next_call_id,
                        &tracker_tx,
                        &mut not_followed_dedup, policy.hang_ms(),
                    );
                    mirror(&slots, &policy);
                }
                recv = audio_rx.recv() => {
                    match recv {
                        Ok(chunk) => {
                            handle_audio(chunk, &mut slots);
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
                        &mut slots, &mut nf, &mut next_call_id, &tracker_tx,
                        now_unix_ms(),
                        policy.hang_ms(), policy.end_grace_ms(),
                    );
                    // Change 056: mirror every tick, not only on
                    // boundaries, so the snapshot's voice counters /
                    // timestamps (updated by the audio arm) stay within
                    // 100 ms of live. `set_live_call_id` inside is a
                    // no-op while the call_id is unchanged.
                    mirror(&slots, &policy);
                }
            }
        }
        tracing::warn!(
            target: "p25_call_lifecycle",
            "call lifecycle task exiting (channels closed)",
        );
    });
}

#[allow(clippy::too_many_arguments)]
fn handle_boundary(
    boundary: CallBoundary,
    slots: &mut [Slot],
    nf: &mut NfCalls,
    next_call_id: &mut u64,
    tx: &CallTrackerEventTx,
    grant_dedup: &mut std::collections::HashMap<(u16, u32, Option<u64>, bool), u64>,
    hang_ms: u64,
) {
    // Change 066: the chain the event belongs to (voice events, followed
    // grants); grant updates and not-followed grants look at every chain.
    let si = slot_index(slots, boundary.lane);
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
                if within(now, last, GRANT_DEDUP_MS) {
                    return;
                }
            }
            grant_dedup.insert(dedup_key, now);
            // Bound the dedup map. Distinct (tg, freq, enc)
            // tuples on a single P25 site are ~10. Prune entries
            // older than 60 s on every insert to keep the map
            // small without a separate GC tick.
            grant_dedup.retain(|_, &mut t| within(now, t, 60_000));

            let channel_str = if channel == 0 {
                None
            } else {
                Some(format!("{}", channel))
            };

            // Change 065: a followed call granted a channel ends the
            // not-followed records on it (the channel is reused).
            if not_followed.is_none() {
                nf.close_channel(tx, freq_hz);
            }

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
            // A grant that is NOT followed (encrypted / sticky /
            // monitor-rejected) never becomes a chain's call: that would
            // put TG-402 ENC in /api/grants for the timeout window even
            // though we never followed it. It gets its own record for
            // grant_stats visibility (operator confirmed: every CC GRANT
            // must be visible); the recorder skips not_followed
            // CallOpens. The 2026-04-26 misattribution bug — TG 300 →
            // TG 402 (ENC, same freq) preempt producing 459 IMBE
            // attributed to TG 402 — is what motivated this.
            // Change 065: kept open while the control channel announces
            // it, for its channel time (`NfCalls`).
            if not_followed.is_some() {
                // 2026-04-30: a not_followed grant for the SAME freq as
                // a followed call means that call physically ended (one
                // voice channel per freq): close it first. Cross-freq
                // not_followed grants leave the calls alone. Change 066:
                // on whichever chain holds the frequency.
                for slot in slots.iter_mut().filter(|s| s.holds(freq_hz)) {
                    slot.close(tx, CloseReason::TgChange);
                }
                let baseline = slots[0].forwarder
                    .frames_submitted.load(Ordering::Relaxed);
                nf.grant(
                    tx, next_call_id, tg, boundary.nac, source,
                    freq_hz, channel_str.clone(), encrypted,
                    not_followed, now_unix_ms(), baseline, hang_ms,
                );
                return;
            }

            // Change 066: the channel now belongs to this chain's call;
            // a call another chain held on it ended.
            for (_, slot) in slots.iter_mut().enumerate()
                .filter(|(i, s)| *i != si && s.holds(freq_hz))
            {
                slot.close(tx, CloseReason::TgChange);
            }
            let Slot { active, forwarder, .. } = &mut slots[si];
            let action = match active.as_ref() {
                // No active session: open as the new active. Recorder
                // will create a recording.
                None => OpenAction::Open(OpenReason::CcGrant),
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
                                tx, a_mut, s,
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
                if within(now_unix_ms(), last, GRANT_DEDUP_MS) {
                    return;
                }
            }
            grant_dedup.insert(dedup_key, now_unix_ms());
            // Change 065: extends a not-followed call's channel time.
            nf.update(tg, freq_hz, now_unix_ms());
            // Change 066: the call of this TG on whichever chain.
            for a in slots.iter_mut().filter_map(|s| s.active.as_mut()) {
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
            let Slot { active, forwarder, .. } = &mut slots[si];
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
            if let Some(a) = slots[si].active.as_mut() {
                if let Some(s) = source {
                    let agrees_with_cc = match a.source {
                        Some(cc) => cc == s,
                        None => false,
                    };
                    if a.observe_source(s) {
                        emit_source_update(
                            tx, a, s,
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
                            lane: a.lane,
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
            if let Some(a) = slots[si].active.as_mut() {
                if let Some(s) = source {
                    if a.observe_source(s) {
                        emit_source_update(
                            tx, a, s,
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
            let Slot { active, forwarder, .. } = &mut slots[si];
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
            if let Some(a) = slots[si].active.as_mut() {
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

/// Change 066: audio counts for the call of the chain that decoded it.
fn handle_audio(chunk: AudioChunk, slots: &mut [Slot]) {
    let Some(slot) = slots.iter_mut().find(|s| s.lane() == chunk.lane) else {
        return;
    };
    if let Some(a) = slot.active.as_mut() {
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

// Change 066: in its own file, driving every traffic chain.
#[cfg(target_os = "linux")]
#[path = "grant_follower_routing.rs"]
mod routing;

#[cfg(target_os = "linux")]
pub use routing::{spawn_grant_follower, FollowerLane};

#[cfg(test)]
#[path = "grant_follower_tests.rs"]
mod tests;

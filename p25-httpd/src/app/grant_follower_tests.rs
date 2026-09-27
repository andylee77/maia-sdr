//! Host tests for the portable call lifecycle in `app::grant_follower`
//! (change 056: voice accounting mirrored into `ActiveCallSnapshot`;
//! change 057: end-of-transmission close, no-keep-alive timeout,
//! same-call refresh). Attached via
//! `#[cfg(test)] #[path = "grant_follower_tests.rs"] mod tests;`.
//!
//! Timings follow the bench replay: src 3436046 (81 IMBE) then the
//! reply from 1014 (72 IMBE) ~0.8 s later on 857.9875, TG 300.

use super::*;
use crate::audio::{CallBoundary, CallBoundaryKind};
use crate::hardware::core_version::{PLL_CLAMP_Q213_HOLD, PLL_CLAMP_Q213_LEGACY};

fn forwarder() -> Arc<ImbeForwarder> {
    let (tx, _rx) = tokio::sync::mpsc::channel(4);
    Arc::new(ImbeForwarder::new(tx))
}

fn grant(tg: u16, source: u32, freq_hz: u64, not_followed: Option<&'static str>) -> CallBoundary {
    CallBoundary {
        kind: CallBoundaryKind::CcGrantArrival {
            tg,
            source: Some(source),
            freq_hz: Some(freq_hz),
            channel: 1117,
            encrypted: false,
            not_followed,
        },
        nac: 0,
        talkgroup: Some(tg),
        expected_submit_count: 0,
    }
}

fn chunk(tg: u16, call_id: u64, airtime: bool) -> AudioChunk {
    AudioChunk {
        pcm: [0; 160],
        talkgroup: tg,
        source: 0,
        call_id,
        captured_at_ms: now_unix_ms(),
        airtime,
    }
}

struct Rig {
    active: Option<ActiveCall>,
    next_id: u64,
    tx: CallTrackerEventTx,
    fwd: Arc<ImbeForwarder>,
    dedup: std::collections::HashMap<(u16, u32, Option<u64>, bool), u64>,
    shared: ActiveCallShared,
    policy: CallPolicy,
}

impl Rig {
    fn new() -> Self {
        Rig {
            active: None,
            next_id: 1,
            tx: new_event_tx(),
            fwd: forwarder(),
            dedup: Default::default(),
            shared: new_active_call_shared(),
            policy: CallPolicy::default(),
        }
    }
    fn boundary(&mut self, b: CallBoundary) {
        handle_boundary(b, &mut self.active, &mut self.next_id, &self.tx, &self.fwd, &mut self.dedup);
        mirror_active(&self.active, &self.shared, &self.fwd, &self.policy);
    }
    fn audio(&mut self, c: AudioChunk) {
        handle_audio(c, &mut self.active);
        mirror_active(&self.active, &self.shared, &self.fwd, &self.policy);
    }
    /// The tick's close rule `dt_ms` after now (the policy defaults).
    fn due_in(&self, dt_ms: u64) -> Option<CloseReason> {
        let c = self.active.as_ref()?;
        close_due(c, now_unix_ms() + dt_ms, self.policy.hang_ms(), self.policy.end_grace_ms())
    }
    fn call_id(&self) -> u64 {
        self.active.as_ref().unwrap().call_id
    }
    /// The lifecycle tick `dt_ms` after now.
    fn tick(&mut self, dt_ms: u64) {
        let now = now_unix_ms() + dt_ms;
        let (h, g) = (self.policy.hang_ms(), self.policy.end_grace_ms());
        sweep(&mut self.active, &mut self.next_id, &self.tx, &self.fwd, now, h, g);
        mirror_active(&self.active, &self.shared, &self.fwd, &self.policy);
    }
    fn snap(&self) -> Option<ActiveCallSnapshot> {
        self.shared.lock().unwrap().clone()
    }
}

#[test]
fn snapshot_counts_voice_of_the_open_call() {
    let mut r = Rig::new();
    let t0 = now_unix_ms();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let s = r.snap().expect("followed grant opens a call");
    assert_eq!((s.tg, s.source, s.voice_frames), (300, Some(3436046), 0));
    assert_eq!(s.sources_observed, vec![3436046]);
    assert!(s.first_voice_unix_ms.is_none() && s.last_voice_unix_ms.is_none());
    assert!(s.last_activity_unix_ms >= t0, "the opening grant is a keep-alive");

    let id = s.call_id;
    for _ in 0..9 {
        r.audio(chunk(300, id, true));
    }
    let s = r.snap().unwrap();
    assert_eq!(s.voice_frames, 9);
    assert!(s.first_voice_unix_ms.is_some());
    assert!(s.last_voice_unix_ms >= s.first_voice_unix_ms);
    assert!(s.last_activity_unix_ms >= s.last_voice_unix_ms.unwrap());
}

#[test]
fn airtime_tail_of_another_call_is_not_voice_of_this_one() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let first = r.snap().unwrap().call_id;
    // Same TG, next speaker: a new call (grant = call).
    r.boundary(grant(300, 1014, 857_987_500, None));
    let s = r.snap().unwrap();
    assert_ne!(s.call_id, first);
    assert_eq!(s.source, Some(1014));
    // In-flight frames of the previous call decoded after the switch.
    r.audio(chunk(300, first, true));
    r.audio(chunk(300, first, true));
    assert_eq!(r.snap().unwrap().voice_frames, 0);
    r.audio(chunk(300, s.call_id, true));
    assert_eq!(r.snap().unwrap().voice_frames, 1);
}

#[test]
fn hdu_refreshes_keepalive_but_is_not_voice() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3406028, 858_437_500, None));
    r.boundary(CallBoundary {
        kind: CallBoundaryKind::HduStart,
        nac: 0x8A1,
        talkgroup: Some(300),
        expected_submit_count: 0,
    });
    let s = r.snap().unwrap();
    assert_eq!(s.voice_frames, 0);
    assert!(s.last_voice_unix_ms.is_none());
    assert_eq!(s.nac, 0x8A1);
}

#[test]
fn not_followed_grant_never_becomes_the_current_call() {
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(402, 3400015, 858_437_500, Some("encrypted")));
    assert!(r.snap().is_none());
    // Synthetic open + close pair for the call list.
    let a = rx.try_recv().unwrap();
    let b = rx.try_recv().unwrap();
    assert!(matches!(a.kind, CallTrackerEventKind::CallOpen { .. }));
    assert!(matches!(b.kind, CallTrackerEventKind::CallClose { .. }));
    assert_eq!(a.call_id, b.call_id);
}

// ── Change 057: call close ──────────────────────────────────────────

fn boundary(kind: CallBoundaryKind) -> CallBoundary {
    CallBoundary { kind, nac: 0x8A1, talkgroup: Some(300), expected_submit_count: 0 }
}

fn voice_end(call_id: u64, air_ms: u64) -> CallBoundary {
    boundary(CallBoundaryKind::VoiceEnd { call_id, air_ms, lc: "talk_complete" })
}

fn upd(freq_hz: u64) -> CallBoundary {
    boundary(CallBoundaryKind::CcGrantUpdate { tg: 300, freq_hz: Some(freq_hz), channel: 1117 })
}

fn nid(voice: bool) -> CallBoundary {
    boundary(CallBoundaryKind::TrafficNidObserved { voice })
}

/// A voice chunk of `call_id` aired at `air_ms`.
fn aired(call_id: u64, air_ms: u64) -> AudioChunk {
    AudioChunk { captured_at_ms: air_ms, ..chunk(300, call_id, true) }
}

/// Every CallClose sent so far: (call_id, reason, end_lc).
fn closes(
    rx: &mut broadcast::Receiver<CallTrackerEvent>,
) -> Vec<(u64, CloseReason, Option<&'static str>)> {
    let mut v = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if let CallTrackerEventKind::CallClose { reason, end_lc, .. } = e.kind {
            v.push((e.call_id, reason, end_lc));
        }
    }
    v
}

#[test]
fn defaults_are_the_documented_ones() {
    let p = CallPolicy::default();
    assert_eq!((p.hang_ms(), p.end_grace_ms()), (3_000, 2_000));
}

#[test]
fn end_of_transmission_then_silence_closes_within_the_grace() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let id = r.call_id();
    let t = now_unix_ms();
    for i in 0..81 {
        r.audio(aired(id, t + i * 20));
    }
    assert_eq!(r.due_in(0), None);
    // TALK COMPLETE right after the last LDU (aired at t + 1.62 s).
    r.boundary(voice_end(id, t + 1_620));
    let s = r.snap().unwrap();
    assert_eq!((s.close_via, s.close_window_ms, s.end_lc), ("end", 2_000, Some("talk_complete")));
    // The system's channel hang keeps the CC announcing the call: no
    // extension after the marker.
    r.boundary(upd(857_987_500));
    // The transmission's last chunks, still in the vocoder / pacer when
    // the terminator was decoded, do not cancel it.
    r.audio(aired(id, t + 1_600));
    assert_eq!(r.due_in(1_900), None);
    assert_eq!(r.due_in(2_000), Some(CloseReason::CallEnd));
    // ~2 s after the marker instead of ~12.8 s (056 bench).
    assert!(r.snap().unwrap().close_at_unix_ms <= now_unix_ms() + 2_000);
}

#[test]
fn a_second_marker_for_the_same_end_does_not_restart_the_grace() {
    let mut r = Rig::new();
    r.boundary(grant(300, 1014, 857_987_500, None));
    let id = r.call_id();
    r.boundary(voice_end(id, 1));
    let first = r.snap().unwrap().close_at_unix_ms;
    std::thread::sleep(Duration::from_millis(5));
    r.boundary(voice_end(id, 2));
    assert_eq!(r.snap().unwrap().close_at_unix_ms, first);
}

#[test]
fn continued_voice_keeps_the_call_open() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let id = r.call_id();
    let t = now_unix_ms();
    r.boundary(voice_end(id, t));
    // A voice frame aired after the marker: the transmission goes on
    // (phantom terminator), back to the no-keep-alive rule.
    r.audio(aired(id, t + 20));
    let s = r.snap().unwrap();
    assert_eq!((s.close_via, s.end_lc), ("timeout", None));
    assert_eq!(r.due_in(2_500), None);
    // Voice keeps coming (keep-alive): never closes.
    r.audio(aired(id, t + 40));
    assert_eq!(r.due_in(2_900), None);
    assert_eq!(r.due_in(3_050), Some(CloseReason::Timeout));
}

#[test]
fn a_pair_of_voice_nids_cancels_the_end_but_one_does_not() {
    let mut r = Rig::new();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let id = r.call_id();
    r.boundary(voice_end(id, now_unix_ms()));
    // Hang TDULC NIDs and a lone false voice NID on noise: still ending.
    r.boundary(nid(false));
    r.boundary(nid(true));
    assert_eq!(r.snap().unwrap().close_via, "end");
    // Voice on the channel again (LDUs every 180 ms): a re-key on the
    // same grant continues this call.
    r.boundary(nid(true));
    assert_eq!(r.snap().unwrap().close_via, "timeout");
    assert_eq!(r.due_in(2_000), None);
}

#[test]
fn reply_granted_during_the_grace_preempts_and_old_markers_are_ignored() {
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let a = r.call_id();
    r.boundary(voice_end(a, now_unix_ms()));
    // The reply from 1014 is granted ~0.8 s later, same channel.
    r.boundary(grant(300, 1014, 857_987_500, None));
    let b = r.call_id();
    assert_ne!(a, b);
    let s = r.snap().unwrap();
    assert_eq!((s.source, s.close_via), (Some(1014), "timeout"), "fresh call, no end pending");
    // The first call's terminator decoded late (air-time tail): it is
    // not the reply's end.
    r.boundary(voice_end(a, now_unix_ms()));
    assert_eq!(r.snap().unwrap().close_via, "timeout");
    assert_eq!(r.due_in(2_500), None);
    let c = closes(&mut rx);
    assert_eq!(c, vec![(a, CloseReason::TgChange, Some("talk_complete"))]);
    // The reply's own end closes it.
    r.boundary(voice_end(b, now_unix_ms()));
    assert_eq!(r.due_in(2_000), Some(CloseReason::CallEnd));
}

#[test]
fn repeat_of_the_same_grant_is_a_refresh_until_the_transmission_ends() {
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let id = r.call_id();
    // The CC repeats the grant 0.3 s later (past the 200 ms dedup).
    let key = (300u16, 3436046u32, Some(857_987_500u64), false);
    *r.dedup.get_mut(&key).unwrap() -= 300;
    r.boundary(grant(300, 3436046, 857_987_500, None));
    assert_eq!(r.call_id(), id, "same transmission, same call");
    // Source-less repeat / explicit update: also this call.
    r.boundary(CallBoundary {
        kind: CallBoundaryKind::CcGrantArrival {
            tg: 300,
            source: None,
            freq_hz: Some(857_987_500),
            channel: 1117,
            encrypted: false,
            not_followed: None,
        },
        nac: 0,
        talkgroup: Some(300),
        expected_submit_count: 0,
    });
    assert_eq!(r.call_id(), id);
    assert!(closes(&mut rx).is_empty());
    // After its end marker, the same unit keying again is a new call.
    r.boundary(voice_end(id, now_unix_ms()));
    *r.dedup.get_mut(&key).unwrap() -= 300;
    r.boundary(grant(300, 3436046, 857_987_500, None));
    assert_ne!(r.call_id(), id);
    // Another frequency is never the same call.
    let id2 = r.call_id();
    r.boundary(grant(300, 3436046, 858_437_500, None));
    assert_ne!(r.call_id(), id2);
}

#[test]
fn updates_keep_a_silent_call_alive_only_on_its_channel() {
    let mut r = Rig::new();
    r.boundary(grant(300, 1014, 857_987_500, None));
    assert_eq!(r.due_in(3_050), Some(CloseReason::Timeout));
    std::thread::sleep(Duration::from_millis(5));
    // Encrypted / undecodable call still announced by the CC.
    r.boundary(upd(857_987_500));
    let before = r.active.as_ref().unwrap().last_upd_at_ms;
    assert!(before > r.active.as_ref().unwrap().started_unix_ms);
    std::thread::sleep(Duration::from_millis(5));
    // Same TG on another channel: not this call's keep-alive.
    r.dedup.clear();
    r.boundary(upd(858_437_500));
    assert_eq!(r.active.as_ref().unwrap().last_upd_at_ms, before);
    // Voice NIDs alone are not keep-alives (noise can fake one).
    r.boundary(nid(true));
    r.boundary(nid(true));
    assert_eq!(r.snap().unwrap().last_activity_unix_ms, before);
}

fn hdu() -> CallBoundary {
    boundary(CallBoundaryKind::HduStart)
}

/// 3436046 on the air, 1014 granted before it unkeys (queued).
fn queued_rig() -> (Rig, broadcast::Receiver<CallTrackerEvent>, u64) {
    let mut r = Rig::new();
    let rx = r.tx.subscribe();
    r.boundary(grant(300, 3436046, 857_987_500, None));
    let a = r.call_id();
    r.boundary(hdu());
    for i in 0..9 {
        r.audio(aired(a, now_unix_ms() + i * 20));
    }
    r.boundary(grant(300, 1014, 857_987_500, None));
    (r, rx, a)
}

#[test]
fn next_talker_granted_while_this_one_talks_waits_for_the_hand_over() {
    let (mut r, mut rx, a) = queued_rig();
    // Still the first talker's call: the rest of its transmission stays
    // in it (pre-057 the grant closed it at once and its tail went to
    // the next call).
    assert_eq!(r.call_id(), a);
    assert_eq!(r.snap().unwrap().source, Some(3436046));
    assert!(closes(&mut rx).is_empty());
    r.tick(1_000);
    assert_eq!(r.call_id(), a, "a live transmission is not cut by the queue");
    // TALK COMPLETE, then the queued talker keys up (HDU in real time).
    r.boundary(voice_end(a, now_unix_ms()));
    assert_eq!(r.call_id(), a);
    r.boundary(hdu());
    let b = r.call_id();
    assert_ne!(a, b);
    assert_eq!(r.snap().unwrap().source, Some(1014));
    assert_eq!(closes(&mut rx), vec![(a, CloseReason::TgChange, Some("talk_complete"))]);
    // The new call starts clean: HDU counted, no end pending.
    let s = r.snap().unwrap();
    assert_eq!(s.close_via, "timeout");
    assert!(r.active.as_ref().unwrap().first_hdu_at_unix_ms.is_some());
}

#[test]
fn queued_talker_starting_with_an_ldu_takes_over_on_voice_nids() {
    let (mut r, mut rx, a) = queued_rig();
    r.boundary(voice_end(a, now_unix_ms()));
    // No HDU decoded: voice NIDs after the marker are the next talker,
    // not a resumed first transmission.
    r.boundary(nid(true));
    assert_eq!(r.call_id(), a);
    r.boundary(nid(true));
    assert_ne!(r.call_id(), a);
    assert_eq!(r.snap().unwrap().source, Some(1014));
    assert_eq!(closes(&mut rx).len(), 1);
}

#[test]
fn queued_grant_is_applied_at_the_end_grace_or_after_its_maximum_wait() {
    // The queued talker never keys up: the grant still becomes a call
    // when the first one ends (it closes by its own rules afterwards).
    let (mut r, mut rx, a) = queued_rig();
    r.boundary(voice_end(a, now_unix_ms()));
    r.tick(1_000);
    assert_eq!(r.call_id(), a);
    r.tick(2_000);
    assert_ne!(r.call_id(), a);
    assert_eq!(closes(&mut rx), vec![(a, CloseReason::TgChange, Some("talk_complete"))]);

    // No end marker at all (weak signal) while voice keeps the first
    // call alive: after QUEUED_GRANT_MAX_MS the grant is applied anyway.
    let (mut r, _rx, a) = queued_rig();
    r.tick(0);
    assert_eq!(r.call_id(), a);
    r.active.as_mut().unwrap().queued.as_mut().unwrap().at_ms -= QUEUED_GRANT_MAX_MS;
    r.tick(0);
    assert_ne!(r.call_id(), a);
}

#[test]
fn grants_before_any_voice_still_preempt_at_once() {
    // Two talkers granted back to back (50 ms apart) before any voice:
    // the later grant is the one on the air.
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(300, 1013, 857_987_500, None));
    let a = r.call_id();
    r.boundary(grant(300, 3402071, 857_987_500, None));
    assert_ne!(r.call_id(), a);
    assert_eq!(closes(&mut rx), vec![(a, CloseReason::TgChange, None)]);
}

#[test]
fn grant_update_refollows_only_a_call_closed_by_timeout() {
    let t = 1_000_000;
    let last = Some((300u16, 857_987_500u64, t));
    let w = REFOLLOW_WINDOW_MS;
    // Same TG and channel, chain idle, within the window: follow again.
    assert!(refollow_on_update(last, 300, Some(857_987_500), true, t + 5_000, w));
    assert!(refollow_on_update(last, 300, Some(857_987_500), true, t + w, w));
    // Anything else stays a keep-alive only (updates carry no
    // encryption flag, so they never acquire a TG on their own).
    assert!(!refollow_on_update(None, 300, Some(857_987_500), true, t, w));
    assert!(!refollow_on_update(last, 402, Some(857_987_500), true, t + 1, w));
    assert!(!refollow_on_update(last, 300, Some(858_437_500), true, t + 1, w));
    assert!(!refollow_on_update(last, 300, None, true, t + 1, w));
    assert!(!refollow_on_update(last, 300, Some(857_987_500), false, t + 1, w), "chain busy");
    assert!(!refollow_on_update(last, 300, Some(857_987_500), true, t + w + 1, w));
}

#[test]
fn close_event_carries_open_time_and_marker() {
    let mut r = Rig::new();
    let mut rx = r.tx.subscribe();
    r.boundary(grant(300, 1014, 857_987_500, None));
    let id = r.call_id();
    r.boundary(voice_end(id, now_unix_ms()));
    let call = r.active.take().unwrap();
    emit_close(&r.tx, &call, CloseReason::CallEnd, call.source, 0);
    let e = loop {
        let e = rx.try_recv().unwrap();
        if matches!(e.kind, CallTrackerEventKind::CallClose { .. }) {
            break e;
        }
    };
    match e.kind {
        CallTrackerEventKind::CallClose { reason, open_ms, end_lc, .. } => {
            assert_eq!((reason, end_lc), (CloseReason::CallEnd, Some("talk_complete")));
            assert!(open_ms < 1_000);
        }
        _ => unreachable!(),
    }
    assert_eq!(serde_json::to_value(CloseReason::CallEnd).unwrap(), "call_end");
}

// Bench 2026-09-27 (057 soak): a parked chain resumed on the same
// frequency with `pll preserved=8579` (the clamp) after the carrier had
// been gone for seconds, and lost two whole transmissions.
#[test]
fn same_freq_resume_resets_a_stale_or_runaway_chain() {
    for clamp in [PLL_CLAMP_Q213_LEGACY, PLL_CLAMP_Q213_HOLD] {
        // Voice a moment ago, PLL near centre: coast (carrier still up).
        assert!(!resume_needs_reset(300, Some(400), clamp));
        assert!(!resume_needs_reset(-2000, Some(COAST_MAX_IDLE_MS), clamp));
        // PLL at or past half the clamp: reset, however fresh.
        assert!(resume_needs_reset(clamp as i16, Some(100), clamp));
        assert!(resume_needs_reset(-(clamp / 2) as i16, Some(100), clamp));
        // Carrier gone longer than the coast window, or never any voice: reset.
        assert!(resume_needs_reset(0, Some(COAST_MAX_IDLE_MS + 1), clamp));
        assert!(resume_needs_reset(0, Some(6_000), clamp));
        assert!(resume_needs_reset(0, None, clamp));
    }
    // Change 059: 3000 is inside the π/3 clamp's coast band but past half
    // the 0.65 rad clamp.
    assert!(!resume_needs_reset(3000, Some(100), PLL_CLAMP_Q213_LEGACY));
    assert!(resume_needs_reset(3000, Some(100), PLL_CLAMP_Q213_HOLD));
}

// Mode B corpus 2026-09-27: three transmissions lost because another TG's
// grant arrived inside the locked call's 2 s end grace, after SDRTrunk had
// already freed its channel.
#[test]
fn end_marker_frees_the_chain_for_another_tg() {
    let at = 1_790_000_000_000;
    let m = unpack_end_marker(pack_end_marker(300, at));
    assert_eq!(m, Some((300, at)));
    assert_eq!(unpack_end_marker(0), None);
    // No marker, or the marker belongs to another call: keep the lock.
    assert!(!end_marker_frees_chain(300, None, at + 5_000));
    assert!(!end_marker_frees_chain(201, m, at + 5_000));
    // Too fresh for the resumed-voice check, then free.
    assert!(!end_marker_frees_chain(300, m, at + END_PREEMPT_AFTER_MS - 1));
    assert!(end_marker_frees_chain(300, m, at + END_PREEMPT_AFTER_MS));
    assert!(END_PREEMPT_AFTER_MS < 1_130, "must free before SDRTrunk's p5 teardown");
}

// A clear grant the sticky gate rejected is re-followed from its updates
// once the chain is free, for its own (TG, frequency), and only while its
// transmission is likely still on the air (bench 2026-09-27: a re-follow
// 4.9 s after the reject parked the chain on the system's hang and cost
// the next real grant).
#[test]
fn sticky_rejected_grant_refollows_from_updates() {
    let rej = Some((300u16, 858_437_500u64, 1_000u64));
    let w = REFOLLOW_STICKY_MS;
    assert!(!refollow_on_update(rej, 300, Some(858_437_500), false, 1_900, w));
    assert!(refollow_on_update(rej, 300, Some(858_437_500), true, 1_900, w));
    assert!(!refollow_on_update(rej, 301, Some(858_437_500), true, 1_900, w));
    assert!(!refollow_on_update(rej, 300, Some(857_987_500), true, 1_900, w));
    assert!(refollow_on_update(rej, 300, Some(858_437_500), true, 1_000 + w, w));
    assert!(!refollow_on_update(rej, 300, Some(858_437_500), true, 5_900, w));
    assert!(REFOLLOW_STICKY_MS < REFOLLOW_WINDOW_MS);
}

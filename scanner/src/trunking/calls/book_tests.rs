//! Host tests for the call book. Timings follow a bench replay: 3436046 (81 frames), then the
//! reply from 1014 about 0.8 s later on 857.9875 MHz, TG 300.

use super::*;

/// A test clock: `at(ms)` is `ms` after the start, on both clocks.
struct Clock {
    base: Instant,
    unix0: u64,
}

impl Clock {
    fn new() -> Self {
        Clock { base: Instant::now(), unix0: 1_790_000_000_000 }
    }

    fn at(&self, ms: u64) -> Stamp {
        Stamp { mono: self.base + Duration::from_millis(ms), unix_ms: self.unix0 + ms }
    }

    fn mono(&self, ms: u64) -> Instant {
        self.base + Duration::from_millis(ms)
    }
}

const F1: u64 = 857_987_500;
const F2: u64 = 858_437_500;

fn ch(freq: u64) -> ChannelKey {
    ChannelKey { freq_hz: Some(freq), slot: None }
}

fn grant_on(lane: Lane, tg: u32, source: u32, freq: u64) -> GrantIn {
    GrantIn {
        tg,
        source: Some(source),
        channel: ch(freq),
        channel_label: Some("0-1117".into()),
        encrypted: false,
        emergency: false,
        private: false,
        nac: 0,
        decision: Decision::Followed(lane),
    }
}

fn grant(tg: u32, source: u32, freq: u64) -> GrantIn {
    grant_on(Lane::One, tg, source, freq)
}

fn not_followed(tg: u32, source: u32, freq: u64) -> GrantIn {
    GrantIn { encrypted: true, decision: Decision::NotFollowed(NotFollowed::Encrypted), ..grant(tg, source, freq) }
}

/// A book with lanes One (and Two), and its events.
struct Rig {
    book: CallBook,
    clock: Clock,
    events: Vec<CallEvent>,
}

impl Rig {
    fn new() -> Self {
        Rig::lanes(&[Lane::One])
    }

    fn lanes(lanes: &[Lane]) -> Self {
        Rig { book: CallBook::new("clay", lanes, CallPolicy::default(), 1), clock: Clock::new(), events: Vec::new() }
    }

    fn grant(&mut self, g: GrantIn, t: u64) {
        let at = self.clock.at(t);
        self.book.grant(g, at, &mut self.events);
    }

    fn update(&mut self, tg: u32, freq: u64, t: u64) {
        self.book.grant_update(tg, ch(freq), 1117, self.clock.at(t));
    }

    fn voice(&mut self, call: Option<CallId>, aired: u64, t: u64) {
        self.book.voice(Lane::One, call, self.clock.mono(aired), self.clock.at(t));
    }

    fn voice_end(&mut self, call: CallId, air: u64, t: u64) {
        self.book.voice_end(Lane::One, call, self.clock.mono(air), "talk_complete", self.clock.at(t));
    }

    fn nid(&mut self, voice: bool, t: u64) {
        let at = self.clock.at(t);
        self.book.nid(Lane::One, voice, at, &mut self.events);
    }

    fn hdu(&mut self, t: u64) {
        let at = self.clock.at(t);
        self.book.hdu(Lane::One, 0x8A1, at, &mut self.events);
    }

    fn tick(&mut self, t: u64) {
        let at = self.clock.at(t);
        self.book.tick(at, &mut self.events);
    }

    fn call(&self) -> &Call {
        self.book.on_lane(Lane::One).expect("an open call on lane one")
    }

    fn id(&self) -> CallId {
        self.call().id
    }

    /// The close rule due at `t`, if any.
    fn due(&self, t: u64) -> Option<CloseReason> {
        self.book.on_lane(Lane::One)?.due(self.clock.mono(t), &self.book.policy)
    }

    fn via(&self) -> &'static str {
        self.call().close_plan(&self.book.policy).1
    }

    /// (call, reason, end marker) of each close since the last call.
    fn closes(&mut self) -> Vec<(CallId, CloseReason, Option<&'static str>)> {
        let out = self
            .events
            .iter()
            .filter_map(|e| match e {
                CallEvent::Closed(c) => Some((c.call, c.reason, c.end_lc)),
                _ => None,
            })
            .collect();
        self.events.clear();
        out
    }
}

#[test]
fn defaults_are_the_documented_ones() {
    let p = CallPolicy::default();
    assert_eq!((p.hang.as_millis(), p.end_grace.as_millis()), (3_000, 2_000));
}

#[test]
fn a_followed_grant_opens_a_call_and_voice_counts_for_it() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let c = r.call();
    assert_eq!((c.tg, c.source, c.voice_frames, c.sources.clone()), (300, Some(3436046), 0, vec![3436046]));
    assert!(matches!(&r.events[0], CallEvent::Opened(o) if o.via == OpenReason::CcGrant && o.lane == Some(Lane::One) && o.site == "clay"));
    let id = r.id();
    for i in 0..9 {
        r.voice(Some(id), 100 + i * 20, 200 + i * 20);
    }
    assert_eq!(r.call().voice_frames, 9);
    assert!(r.call().first_voice.is_some());
}

#[test]
fn the_previous_calls_in_flight_voice_is_not_this_calls() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let first = r.id();
    // Same TG, next speaker before any voice: a new call.
    r.grant(grant(300, 1014, F1), 50);
    let second = r.id();
    assert_ne!(second, first);
    r.voice(Some(first), 10, 100);
    r.voice(Some(first), 30, 120);
    assert_eq!(r.call().voice_frames, 0);
    r.voice(Some(second), 60, 140);
    assert_eq!(r.call().voice_frames, 1);
}

#[test]
fn an_hdu_is_a_keepalive_but_not_voice() {
    let mut r = Rig::new();
    r.grant(grant(300, 3406028, F2), 0);
    r.hdu(500);
    let c = r.call();
    assert_eq!((c.voice_frames, c.nac), (0, 0x8A1));
    assert!(c.last_voice.is_none());
    assert_eq!(r.due(3_400), None, "the HDU at 0.5 s holds the call to 3.5 s");
    assert_eq!(r.due(3_600), Some(CloseReason::Timeout));
}

#[test]
fn a_not_followed_grant_is_listed_for_its_channel_time() {
    let mut r = Rig::new();
    r.grant(not_followed(402, 3400015, F2), 0);
    assert!(r.book.on_lane(Lane::One).is_none());
    assert!(matches!(&r.events[0], CallEvent::Opened(o) if o.lane.is_none() && o.not_followed == Some(NotFollowed::Encrypted)));
    r.events.clear();
    r.update(402, F2, 1_000);
    r.grant(not_followed(402, 3400015, F2), 2_000); // re-announced: the same call
    r.update(402, F2, 4_500);
    r.tick(7_000); // 2.5 s quiet: still open
    assert!(r.events.is_empty());
    r.tick(7_600); // 3.1 s quiet: ends at its last announcement
    match &r.events[..] {
        [CallEvent::Closed(c)] => {
            assert_eq!((c.reason, c.ended_unix_ms - c.started_unix_ms, c.open_ms), (CloseReason::Timeout, 4_500, 4_500));
            assert_eq!(c.last_update_unix_ms, c.ended_unix_ms);
        }
        other => panic!("{other:?}"),
    }
    r.events.clear();
    // A new talker on the channel ends the record; so does the channel going to a followed call.
    r.grant(not_followed(402, 3400015, F2), 10_000);
    r.grant(not_followed(402, 1003, F2), 11_000);
    r.grant(grant(300, 1014, F2), 12_000);
    let kinds: Vec<&str> = r
        .events
        .iter()
        .map(|e| match e {
            CallEvent::Opened(o) if o.lane.is_none() => "nf_open",
            CallEvent::Opened(_) => "open",
            CallEvent::Closed(_) => "close",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["nf_open", "close", "nf_open", "close", "open"]);
    assert!(r.book.not_followed.is_empty());
}

#[test]
fn end_of_transmission_then_silence_closes_within_the_grace() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let id = r.id();
    for i in 0..81 {
        r.voice(Some(id), i * 20, 100 + i * 20);
    }
    assert_eq!(r.due(1_700), None);
    // TALK COMPLETE right after the last voice frame (aired at 1.62 s).
    r.voice_end(id, 1_620, 1_800);
    assert_eq!((r.via(), r.call().end_lc()), ("end", Some("talk_complete")));
    // The system's channel hang keeps the control channel announcing the call: no extension.
    r.update(300, F1, 1_900);
    // The transmission's last frames, decoded after the terminator, do not cancel it.
    r.voice(Some(id), 1_600, 1_950);
    assert_eq!(r.due(3_700), None);
    assert_eq!(r.due(3_800), Some(CloseReason::CallEnd));
}

#[test]
fn a_second_marker_does_not_restart_the_grace() {
    let mut r = Rig::new();
    r.grant(grant(300, 1014, F1), 0);
    let id = r.id();
    r.voice_end(id, 1, 100);
    r.voice_end(id, 2, 105);
    assert_eq!(r.call().end_marker(), Some(r.clock.mono(100)));
}

#[test]
fn voice_aired_after_the_marker_keeps_the_call_open() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let id = r.id();
    r.voice_end(id, 1_000, 1_000);
    // A phantom terminator: voice aired after it.
    r.voice(Some(id), 1_020, 1_020);
    assert_eq!((r.via(), r.call().end_lc()), ("timeout", None));
    r.voice(Some(id), 1_040, 1_040);
    assert_eq!(r.due(3_940), None);
    assert_eq!(r.due(4_090), Some(CloseReason::Timeout));
}

#[test]
fn a_pair_of_voice_nids_cancels_the_end_but_one_does_not() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let id = r.id();
    r.voice_end(id, 1_000, 1_000);
    // Hang TDULC NIDs and a lone false voice NID: still ending.
    r.nid(false, 1_100);
    r.nid(true, 1_200);
    assert_eq!(r.via(), "end");
    // Voice again (LDUs every 180 ms): a re-key continues this call.
    r.nid(true, 1_380);
    assert_eq!(r.via(), "timeout");
    assert_eq!(r.due(3_000), None);
}

#[test]
fn a_reply_granted_during_the_grace_preempts_and_old_markers_are_ignored() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let a = r.id();
    r.voice_end(a, 1_000, 1_000);
    r.grant(grant(300, 1014, F1), 1_800);
    let b = r.id();
    assert_ne!(a, b);
    assert_eq!((r.call().source, r.via()), (Some(1014), "timeout"));
    // The first call's terminator decoded late: not the reply's end.
    r.voice_end(a, 1_000, 1_900);
    assert_eq!(r.via(), "timeout");
    assert_eq!(r.closes(), vec![(a, CloseReason::TgChange, Some("talk_complete"))]);
    r.voice_end(b, 2_500, 2_500);
    assert_eq!(r.due(4_500), Some(CloseReason::CallEnd));
}

#[test]
fn a_repeat_of_the_grant_refreshes_until_the_transmission_ends() {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let id = r.id();
    // Within 200 ms: one announcement. 300 ms later: a refresh of the same call.
    r.grant(grant(300, 3436046, F1), 100);
    r.grant(grant(300, 3436046, F1), 300);
    assert_eq!(r.id(), id);
    // A source-less repeat (explicit update): also this call.
    r.grant(GrantIn { source: None, ..grant(300, 0, F1) }, 400);
    assert_eq!(r.id(), id);
    assert!(r.closes().is_empty());
    assert_eq!(r.call().last_update.unix_ms, r.clock.at(400).unix_ms);
    // After its end marker the same unit keying again is a new call.
    r.voice_end(id, 500, 500);
    r.grant(grant(300, 3436046, F1), 800);
    assert_ne!(r.id(), id);
    // Another frequency is never the same call.
    let id2 = r.id();
    r.grant(grant(300, 3436046, F2), 1_100);
    assert_ne!(r.id(), id2);
}

#[test]
fn updates_keep_a_silent_call_alive_only_on_its_channel() {
    let mut r = Rig::new();
    r.grant(grant(300, 1014, F1), 0);
    assert_eq!(r.due(3_050), Some(CloseReason::Timeout));
    r.update(300, F1, 1_000);
    assert_eq!(r.due(3_050), None);
    // Same TG on another channel: not this call's keep-alive.
    r.update(300, F2, 2_000);
    assert_eq!(r.due(4_050), Some(CloseReason::Timeout));
    // Voice NIDs alone are not keep-alives.
    r.nid(true, 2_500);
    r.nid(true, 2_600);
    assert_eq!(r.due(4_050), Some(CloseReason::Timeout));
}

/// 3436046 on the air; 1014 granted before it unkeys (queued).
fn queued_rig() -> (Rig, CallId) {
    let mut r = Rig::new();
    r.grant(grant(300, 3436046, F1), 0);
    let a = r.id();
    r.hdu(100);
    for i in 0..9 {
        r.voice(Some(a), 200 + i * 20, 300 + i * 20);
    }
    r.grant(grant(300, 1014, F1), 600);
    r.events.clear();
    (r, a)
}

#[test]
fn the_next_talker_granted_while_this_one_talks_waits_for_the_hand_over() {
    let (mut r, a) = queued_rig();
    assert_eq!((r.id(), r.call().source), (a, Some(3436046)));
    r.tick(1_600);
    assert_eq!(r.id(), a, "a live transmission is not cut by the queue");
    // TALK COMPLETE, then the queued talker keys up (an HDU in real time).
    r.voice_end(a, 1_700, 1_700);
    assert_eq!(r.id(), a);
    r.hdu(2_000);
    assert_ne!(r.id(), a);
    assert_eq!(r.call().source, Some(1014));
    assert_eq!(r.closes(), vec![(a, CloseReason::TgChange, Some("talk_complete"))]);
    assert_eq!(r.via(), "timeout");
    assert!(r.call().first_hdu.is_some());
}

#[test]
fn a_queued_talker_starting_with_an_ldu_takes_over_on_voice_nids() {
    let (mut r, a) = queued_rig();
    r.voice_end(a, 1_000, 1_000);
    r.nid(true, 1_100);
    assert_eq!(r.id(), a);
    r.nid(true, 1_280);
    assert_ne!(r.id(), a);
    assert_eq!(r.call().source, Some(1014));
    assert_eq!(r.closes().len(), 1);
}

#[test]
fn a_queued_grant_is_applied_at_the_end_grace_or_after_its_longest_wait() {
    let (mut r, a) = queued_rig();
    r.voice_end(a, 1_000, 1_000);
    r.tick(2_000);
    assert_eq!(r.id(), a);
    r.tick(3_000);
    assert_ne!(r.id(), a);
    assert_eq!(r.closes(), vec![(a, CloseReason::TgChange, Some("talk_complete"))]);

    // No end marker while voice keeps the first call alive: applied after 10 s anyway.
    let (mut r, a) = queued_rig();
    for t in (700..10_600).step_by(100) {
        r.voice(Some(a), t, t);
        r.tick(t);
    }
    assert_eq!(r.id(), a);
    r.tick(10_600);
    assert_ne!(r.id(), a);
}

#[test]
fn grants_before_any_voice_still_preempt_at_once() {
    let mut r = Rig::new();
    r.grant(grant(300, 1013, F1), 0);
    let a = r.id();
    r.grant(grant(300, 3402071, F1), 50);
    assert_ne!(r.id(), a);
    assert_eq!(r.closes(), vec![(a, CloseReason::TgChange, None)]);
}

#[test]
fn a_close_carries_its_open_time_marker_and_sources() {
    let mut r = Rig::new();
    r.grant(grant(300, 1014, F1), 0);
    let id = r.id();
    let mut out = Vec::new();
    r.book.link_control_source(Lane::One, 3406021, &mut out);
    assert!(matches!(out[..], [CallEvent::Source { via: SourceVia::LinkControl, .. }, CallEvent::Speaker { agrees_with_grant: false, .. }]));
    r.voice_end(id, 400, 500);
    r.tick(2_600);
    match &r.events[..] {
        [_, CallEvent::Closed(c)] => {
            assert_eq!((c.reason, c.end_lc, c.open_ms), (CloseReason::CallEnd, Some("talk_complete"), 2_600));
            assert_eq!((c.sources.clone(), c.speaker), (vec![1014, 3406021], Some(3406021)));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(serde_json::to_value(CloseReason::CallEnd).unwrap(), "call_end");
}

#[test]
fn two_lanes_carry_two_calls_at_once() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::One, 300, 1013, F1), 0);
    r.grant(grant_on(Lane::Two, 305, 3400043, 859_425_000), 10);
    let (a, b) = (r.book.on_lane(Lane::One).unwrap().id, r.book.on_lane(Lane::Two).unwrap().id);
    assert_ne!(a, b, "one call id space");
    // Voice counts for the lane that decoded it.
    r.book.voice(Lane::Two, Some(b), r.clock.mono(100), r.clock.at(100));
    assert_eq!((r.book.on_lane(Lane::One).unwrap().voice_frames, r.book.on_lane(Lane::Two).unwrap().voice_frames), (0, 1));
    // Lane one's call ends on its own; lane two's stays.
    r.voice_end(a, 200, 200);
    r.events.clear();
    r.tick(2_250);
    assert!(r.book.on_lane(Lane::One).is_none());
    assert_eq!(r.book.on_lane(Lane::Two).unwrap().id, b);
    assert_eq!(r.closes(), vec![(a, CloseReason::CallEnd, Some("talk_complete"))]);
}

#[test]
fn voice_on_one_lane_does_not_touch_the_other() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::One, 300, 1013, F1), 0);
    r.grant(grant_on(Lane::Two, 305, 3400043, 859_425_000), 10);
    let a = r.id();
    r.voice_end(a, 100, 100);
    let mut out = Vec::new();
    for t in [200, 300, 400] {
        r.book.nid(Lane::Two, true, r.clock.at(t), &mut out);
    }
    assert!(r.call().end_lc().is_some(), "lane two's NIDs are not lane one's voice");
    r.book.hdu(Lane::Two, 0, r.clock.at(500), &mut out);
    assert!(r.call().first_hdu.is_none());
    assert!(r.book.on_lane(Lane::Two).unwrap().first_hdu.is_some());
}

#[test]
fn a_channel_moving_to_the_other_lane_ends_its_call_there() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::One, 300, 1013, F1), 0);
    let a = r.id();
    r.events.clear();
    r.grant(grant_on(Lane::Two, 305, 3400043, F1), 100);
    assert!(r.book.on_lane(Lane::One).is_none());
    assert_eq!(r.book.on_lane(Lane::Two).unwrap().tg, 305);
    assert_eq!(r.closes(), vec![(a, CloseReason::TgChange, None)]);
}

#[test]
fn a_not_followed_grant_ends_only_the_call_on_its_channel() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::One, 300, 1013, F1), 0);
    r.grant(grant_on(Lane::Two, 305, 3400043, 859_425_000), 10);
    let b = r.book.on_lane(Lane::Two).unwrap().id;
    r.events.clear();
    r.grant(not_followed(402, 3400015, 859_425_000), 100);
    assert!(r.book.on_lane(Lane::One).is_some());
    assert!(r.book.on_lane(Lane::Two).is_none());
    let closed: Vec<_> = r.events.iter().filter_map(|e| match e {
        CallEvent::Closed(c) => Some((c.call, c.lane)),
        _ => None,
    }).collect();
    assert_eq!(closed, vec![(b, Some(Lane::Two))]);
}

#[test]
fn a_grant_update_refreshes_the_call_on_whichever_lane() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::Two, 300, 1013, F1), 0);
    r.update(300, F1, 1_000);
    assert_eq!(r.book.on_lane(Lane::Two).unwrap().last_update.unix_ms, r.clock.at(1_000).unix_ms);
}

#[test]
fn channels_differ_by_timeslot() {
    // DMR: TS1 and TS2 of one carrier are two channels.
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    let ts = |s| ChannelKey { freq_hz: Some(451_087_500), slot: Some(s) };
    r.grant(GrantIn { channel: ts(1), ..grant_on(Lane::One, 87921, 81921, 0) }, 0);
    r.grant(GrantIn { channel: ts(2), ..grant_on(Lane::Two, 87924, 81922, 0) }, 10);
    assert!(r.book.on_lane(Lane::One).is_some() && r.book.on_lane(Lane::Two).is_some());
    r.grant(GrantIn { channel: ts(2), ..not_followed(87925, 81923, 0) }, 20);
    assert!(r.book.on_lane(Lane::One).is_some(), "TS2's grant leaves TS1's call alone");
    assert!(r.book.on_lane(Lane::Two).is_none());
}

#[test]
fn a_site_switch_closes_every_call() {
    let mut r = Rig::lanes(&[Lane::One, Lane::Two]);
    r.grant(grant_on(Lane::One, 300, 1013, F1), 0);
    r.grant(not_followed(402, 3400015, F2), 10);
    r.events.clear();
    let at = r.clock.at(500);
    r.book.site_switch("cec_gcs", at, &mut r.events);
    let reasons: Vec<_> = r.closes().into_iter().map(|c| c.1).collect();
    assert_eq!(reasons, [CloseReason::SiteSwitch, CloseReason::SiteSwitch]);
    r.grant(grant(87921, 81921, 451_087_500), 600);
    assert!(matches!(&r.events[0], CallEvent::Opened(o) if o.site == "cec_gcs"));
}

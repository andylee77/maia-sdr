//! Host tests for the follower. The routing is the bench's: Primary (TG 300) on the left, TAC
//! (301-310) and Hospital (600-601) on the right, TAC above Hospital, other talkgroups right.

use super::*;
use crate::protocol::events::LogicalChannel;
use crate::services::config::profiles::{Group, Profile, Speakers};

fn profile(other: Side, right: &[&str]) -> Profile {
    Profile {
        groups: vec![
            Group { name: "Primary".into(), talkgroups: vec![300] },
            Group { name: "TAC".into(), talkgroups: (301..=310).collect() },
            Group { name: "Hospital".into(), talkgroups: vec![600, 601] },
        ],
        speakers: Speakers { left: vec!["Primary".into()], right: right.iter().map(|s| s.to_string()).collect(), other, preempt: true },
        ..Profile::default()
    }
}

fn bench() -> Routing {
    Routing::new(&profile(Side::Right, &["TAC", "Hospital"]))
}

fn idle(lane: Lane) -> LaneView {
    LaneView { lane, locked_tg: None, tuned_hz: None, end_marker: None }
}

fn busy(lane: Lane, tg: u32) -> LaneView {
    LaneView { locked_tg: Some(tg), ..idle(lane) }
}

const NEVER: fn(u32, Option<(u32, Instant)>) -> bool = |_, _| false;

fn choose(tg: u32, lanes: &[LaneView], r: &Routing) -> LaneChoice {
    let side = r.route(tg).map_or(Side::Off, |x| x.side);
    choose_lane(tg, side, Some(859_425_000), lanes, r, NEVER)
}

#[test]
fn one_lane_takes_stays_preempts_or_rejects() {
    let r = bench();
    let one = |v: LaneView| [v];
    assert_eq!(choose(305, &one(idle(Lane::One)), &r), LaneChoice::Take(Lane::One));
    assert_eq!(choose(300, &one(busy(Lane::One, 300)), &r), LaneChoice::Stay(Lane::One));
    // TAC above Hospital pre-empts; the other way round is rejected.
    assert_eq!(choose(305, &one(busy(Lane::One, 600)), &r), LaneChoice::Preempt(Lane::One, Preemption::Priority));
    assert_eq!(choose(600, &one(busy(Lane::One, 305)), &r), LaneChoice::Reject);
    assert_eq!(choose(300, &one(busy(Lane::One, 305)), &r), LaneChoice::Preempt(Lane::One, Preemption::Priority));
    // The locked call's end marker: any talkgroup takes the lane.
    let ended = |_: u32, m: Option<(u32, Instant)>| m.is_some();
    let v = LaneView { end_marker: Some((305, Instant::now())), ..busy(Lane::One, 305) };
    assert_eq!(choose_lane(600, Side::Right, None, &[v], &r, ended), LaneChoice::Preempt(Lane::One, Preemption::EndMarker));
}

#[test]
fn sides_map_to_their_lanes_and_do_not_borrow() {
    let r = bench();
    let both_idle = [idle(Lane::One), idle(Lane::Two)];
    assert_eq!(choose(300, &both_idle, &r), LaneChoice::Take(Lane::One));
    assert_eq!(choose(305, &both_idle, &r), LaneChoice::Take(Lane::Two));
    assert_eq!(choose(999, &both_idle, &r), LaneChoice::Take(Lane::Two));
    assert_eq!(choose(305, &[busy(Lane::One, 300), idle(Lane::Two)], &r), LaneChoice::Take(Lane::Two));
    // Hospital while TAC holds the right lane: rejected, lane one stays free.
    assert_eq!(choose(600, &[idle(Lane::One), busy(Lane::Two, 305)], &r), LaneChoice::Reject);
    assert_eq!(choose(306, &[busy(Lane::One, 300), busy(Lane::Two, 305)], &r), LaneChoice::Reject);
    // Priority within a lane; an ungrouped talkgroup never pre-empts a grouped call.
    assert_eq!(choose(305, &[busy(Lane::One, 300), busy(Lane::Two, 600)], &r), LaneChoice::Preempt(Lane::Two, Preemption::Priority));
    assert_eq!(choose(999, &[idle(Lane::One), busy(Lane::Two, 600)], &r), LaneChoice::Reject);
    // A talkgroup stays on its lane though its side changed.
    assert_eq!(choose(999, &[busy(Lane::One, 999), idle(Lane::Two)], &r), LaneChoice::Stay(Lane::One));
}

#[test]
fn other_talkgroups_on_both_use_either_lane() {
    let r = Routing::new(&profile(Side::Both, &["TAC", "Hospital"]));
    let f = Some(859_425_000);
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), idle(Lane::Two)], &r, NEVER), LaneChoice::Take(Lane::One));
    let parked = LaneView { tuned_hz: f, ..idle(Lane::Two) };
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), parked], &r, NEVER), LaneChoice::Take(Lane::Two));
    assert_eq!(choose_lane(999, Side::Both, f, &[busy(Lane::One, 300), idle(Lane::Two)], &r, NEVER), LaneChoice::Take(Lane::Two));
    let r = Routing::new(&profile(Side::Both, &[]));
    assert!(!r.side_has_groups(Side::Right) && r.side_has_groups(Side::Left));
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), idle(Lane::Two)], &r, NEVER), LaneChoice::Take(Lane::Two));
    assert!(candidates(Side::Off, None, &[idle(Lane::One), idle(Lane::Two)], &r).is_empty());
}

#[test]
fn routing_ignores_monitors_and_ranks() {
    let mut p = profile(Side::Right, &["TAC"]);
    p.ignore = vec![305];
    p.monitor = vec![300, 305, 999];
    let r = Routing::new(&p);
    assert!(r.ignored(305) && r.route(305).is_none());
    assert!(r.monitored(999) && !r.monitored(301));
    assert_eq!(r.route(600), None, "Hospital is on no speaker");
    assert_eq!(r.route(999).map(|x| x.rank), Some(routing::OTHER_RANK));
    assert!(r.preempts(300, 301) && !r.preempts(301, 300) && !r.preempts(300, 300));
    assert!(r.preempts(301, 600), "a call no longer followed yields");
}

#[test]
fn the_speaker_of_a_talkgroup_is_its_groups() {
    let f = Follower::new(&[Lane::One, Lane::Two], Routing::new(&profile(Side::Both, &["TAC"])), HashSet::new());
    assert_eq!((f.speaker(300), f.speaker(305), f.speaker(999)), (Side::Left, Side::Right, Side::Both));
}

// ── The follower ─────────────────────────────────────────────────────

const F1: u64 = 857_987_500;
const F2: u64 = 858_437_500;

fn grant(tg: u32, source: u32, freq: u64) -> Grant {
    Grant {
        tg,
        source: Some(source),
        private: false,
        channel: LogicalChannel { id: ChannelId::P25 { iden: 0, number: 1117 }, slot: None, freq_hz: Some(freq), tdma: false },
        encrypted: false,
        emergency: false,
        update: false,
    }
}

fn upd(tg: u32, freq: u64) -> Grant {
    Grant { update: true, source: None, ..grant(tg, 0, freq) }
}

struct Rig {
    f: Follower,
    t0: Instant,
    markers: Vec<(Lane, (u32, Instant))>,
}

impl Rig {
    fn new(lanes: &[Lane]) -> Self {
        Rig { f: Follower::new(lanes, bench(), HashSet::new()), t0: Instant::now(), markers: Vec::new() }
    }

    fn at(&self, ms: u64) -> Instant {
        self.t0 + Duration::from_millis(ms)
    }

    fn grant(&mut self, g: Grant, ms: u64) -> Outcome {
        let markers = self.markers.clone();
        let at = self.at(ms);
        self.f.grant(&g, 0x8A1, at, &move |l| markers.iter().find(|m| m.0 == l).map(|m| m.1), &|f| f < 860_000_000)
    }
}

fn decision(o: &Outcome) -> Option<Decision> {
    match &o.record {
        Some(Record::Grant(g)) => Some(g.decision),
        _ => None,
    }
}

#[test]
fn the_gates_name_why_a_grant_is_not_followed() {
    let mut p = profile(Side::Right, &["TAC"]);
    p.ignore = vec![402];
    p.monitor = vec![300, 301, 600, 999];
    let mut f = Follower::new(&[Lane::One], Routing::new(&p), HashSet::from([700]));
    let t = Instant::now();
    let mut run = |g: Grant| decision(&f.grant(&g, 0, t, &|_| None, &|hz| hz < 860_000_000));
    use NotFollowed as N;
    assert_eq!(run(grant(402, 1, F1)), Some(Decision::NotFollowed(N::Ignored)));
    let mut tdma = grant(300, 1, F1);
    tdma.channel.tdma = true;
    assert_eq!(run(tdma), Some(Decision::NotFollowed(N::Phase2)));
    assert_eq!(run(grant(302, 1, F1)), Some(Decision::NotFollowed(N::MonitorList)));
    assert_eq!(run(grant(600, 1, F1)), Some(Decision::NotFollowed(N::SpeakerOff)));
    let mut unknown = grant(300, 1, F1);
    unknown.channel.freq_hz = None;
    assert_eq!(run(unknown), Some(Decision::NotFollowed(N::UnknownLcn)));
    assert_eq!(run(grant(300, 1, 861_000_000)), Some(Decision::NotFollowed(N::OutOfBand)));
    assert_eq!(run(Grant { encrypted: true, ..grant(301, 1, F1) }), Some(Decision::NotFollowed(N::Encrypted)));
    assert_eq!(run(grant(301, 1, F1)), Some(Decision::NotFollowed(N::Encrypted)), "learned from its first encrypted grant");
    assert_eq!(run(grant(300, 1, F1)), Some(Decision::Followed(Lane::One)));
}

#[test]
fn a_hold_follows_only_its_talkgroup_whatever_the_profile_says() {
    let mut p = profile(Side::Right, &["TAC"]);
    p.ignore = vec![402];
    p.monitor = vec![300];
    let mut f = Follower::new(&[Lane::One, Lane::Two], Routing::new(&p), HashSet::new());
    let t = Instant::now();
    let run = |f: &mut Follower, g: Grant| f.grant(&g, 0, t, &|_| None, &|hz| hz < 860_000_000);
    assert_eq!(decision(&run(&mut f, grant(300, 1, F1))), Some(Decision::Followed(Lane::One)));
    // Holding 402 (ignored, off the monitor list, on no speaker) lets lane one go.
    assert_eq!(f.set_hold(Some(402)), vec![Command::Release { lane: Lane::One }]);
    assert_eq!(f.locked(), vec![(Lane::One, None), (Lane::Two, None)]);
    assert_eq!(decision(&run(&mut f, grant(300, 1, F1))), Some(Decision::NotFollowed(NotFollowed::Held)));
    assert!(matches!(decision(&run(&mut f, grant(402, 2, F2))), Some(Decision::Followed(_))));
    assert_eq!(f.set_hold(Some(402)), Vec::new(), "a lane on the held talkgroup is kept");
    assert_eq!(f.set_hold(None), Vec::new());
    // Encrypted stays refused: there is nothing to hear.
    let mut f2 = Follower::new(&[Lane::One], Routing::new(&p), HashSet::from([700]));
    f2.set_hold(Some(700));
    assert_eq!(decision(&run(&mut f2, grant(700, 3, F1))), Some(Decision::NotFollowed(NotFollowed::Encrypted)));
    // Released: the profile again.
    assert_eq!(decision(&run(&mut f, grant(402, 2, F2))), Some(Decision::NotFollowed(NotFollowed::Ignored)));
}

#[test]
fn a_followed_grant_tunes_its_lane_once() {
    let mut r = Rig::new(&[Lane::One]);
    let o = r.grant(grant(300, 1014, F1), 0);
    assert_eq!(decision(&o), Some(Decision::Followed(Lane::One)));
    let ch = ChannelKey { freq_hz: Some(F1), slot: None };
    assert_eq!(o.commands, vec![Command::Follow { lane: Lane::One, channel: ch }]);
    // A repeat, or the next talker: the lane stays, no retune.
    assert!(r.grant(grant(300, 3436046, F1), 500).commands.is_empty());
    // The talkgroup moving channel: the lane follows it.
    let o = r.grant(grant(300, 3436046, F2), 1_000);
    assert_eq!(o.commands, vec![Command::Follow { lane: Lane::One, channel: ChannelKey { freq_hz: Some(F2), slot: None } }]);
    // An update for the followed talkgroup is a keep-alive.
    assert!(matches!(r.grant(upd(300, F2), 1_100).record, Some(Record::Update { tg: 300, .. })));
}

#[test]
fn a_busy_lane_refuses_and_an_ended_call_yields() {
    let mut r = Rig::new(&[Lane::One]);
    r.grant(grant(305, 1, F1), 0);
    // Hospital is below TAC: busy.
    assert_eq!(decision(&r.grant(grant(600, 2, F2), 100)), Some(Decision::NotFollowed(NotFollowed::Busy)));
    // TAC's transmission ended 600 ms ago: Hospital takes the lane.
    r.markers = vec![(Lane::One, (305, r.at(500)))];
    assert_eq!(decision(&r.grant(grant(600, 2, F2), 1_000)), Some(Decision::NotFollowed(NotFollowed::Busy)), "only 500 ms");
    let o = r.grant(grant(600, 2, F2), 1_200);
    assert_eq!(decision(&o), Some(Decision::Followed(Lane::One)));
    assert_eq!(o.commands[0], Command::Release { lane: Lane::One });
}

#[test]
fn channel_reuse_releases_and_encryption_pauses() {
    let mut r = Rig::new(&[Lane::One, Lane::Two]);
    r.grant(grant(300, 1, F1), 0);
    r.grant(grant(305, 2, F2), 10);
    // The system gives 305's channel to TG 999 (right side, so lane two again).
    let o = r.grant(grant(999, 3, F2), 100);
    assert_eq!(o.commands, vec![Command::Release { lane: Lane::Two }, Command::Follow { lane: Lane::Two, channel: ChannelKey { freq_hz: Some(F2), slot: None } }]);
    // TG 300 turns out encrypted: its lane is paused.
    let o = r.grant(Grant { encrypted: true, ..grant(300, 1, F1) }, 200);
    assert_eq!(o.commands, vec![Command::Pause { lane: Lane::One }]);
    assert_eq!(r.f.locked(), vec![(Lane::One, None), (Lane::Two, Some(999))]);
}

#[test]
fn an_update_follows_again_a_call_closed_by_timeout() {
    let mut r = Rig::new(&[Lane::One]);
    r.grant(grant(300, 1, F1), 0);
    r.f.opened(Lane::One, 7);
    let closed = |call, reason| Closed {
        call,
        lane: Some(Lane::One),
        reason,
        source: None,
        speaker: None,
        started_unix_ms: 0,
        ended_unix_ms: 0,
        first_voice_unix_ms: None,
        first_hdu_unix_ms: None,
        sources: vec![],
        last_update_unix_ms: 0,
        open_ms: 0,
        end_lc: None,
        voice_frames: 0,
    };
    // A close of another call does not release the lane.
    assert_eq!(r.f.closed(&closed(6, CloseReason::Timeout), r.at(3_000)), None);
    assert_eq!(r.f.closed(&closed(7, CloseReason::Timeout), r.at(3_000)), Some(Command::Release { lane: Lane::One }));
    // Still announced 5 s later: followed again, source-less.
    let o = r.grant(upd(300, F1), 8_000);
    match o.record {
        Some(Record::Grant(g)) => assert_eq!((g.decision, g.source), (Decision::Followed(Lane::One), None)),
        other => panic!("{other:?}"),
    }
    // Once only; and never for an update of a talkgroup that was not followed.
    r.f.closed(&closed(8, CloseReason::CallEnd), r.at(9_000));
    assert!(matches!(r.grant(upd(301, F1), 9_500).record, Some(Record::Update { .. })));
}

#[test]
fn an_update_follows_again_a_grant_refused_as_busy_for_two_seconds() {
    let mut r = Rig::new(&[Lane::One]);
    r.grant(grant(305, 1, F1), 0);
    assert_eq!(decision(&r.grant(grant(306, 2, F2), 100)), Some(Decision::NotFollowed(NotFollowed::Busy)));
    // The lane is still busy: the update stays a keep-alive.
    assert!(matches!(r.grant(upd(306, F2), 500).record, Some(Record::Update { .. })));
    // 305 ended (marker 600 ms old) within 2 s of the refusal: 306 is followed from its update.
    r.markers = vec![(Lane::One, (305, r.at(800)))];
    let o = r.grant(upd(306, F2), 1_500);
    assert_eq!(decision(&o), Some(Decision::Followed(Lane::One)));
    // Later than 2 s: no.
    let mut r = Rig::new(&[Lane::One]);
    r.grant(grant(305, 1, F1), 0);
    r.grant(grant(306, 2, F2), 100);
    r.markers = vec![(Lane::One, (305, r.at(800)))];
    assert!(matches!(r.grant(upd(306, F2), 2_200).record, Some(Record::Update { .. })));
}

#[test]
fn a_lane_locked_with_no_call_is_released_on_the_second_look() {
    let mut r = Rig::new(&[Lane::One]);
    r.grant(grant(300, 1, F1), 0);
    assert!(r.f.stuck_check(&|_| true).is_empty());
    assert!(r.f.stuck_check(&|_| false).is_empty());
    assert_eq!(r.f.stuck_check(&|_| false), vec![Command::Release { lane: Lane::One }]);
    assert_eq!(r.f.locked(), vec![(Lane::One, None)]);
}

#[test]
fn end_markers_free_a_lane_after_600_ms_for_its_own_talkgroup_only() {
    let at = Instant::now();
    let m = Some((300, at));
    assert!(!end_marker_frees(300, None, at + Duration::from_secs(5)));
    assert!(!end_marker_frees(201, m, at + Duration::from_secs(5)));
    assert!(!end_marker_frees(300, m, at + END_PREEMPT_AFTER - Duration::from_millis(1)));
    assert!(end_marker_frees(300, m, at + END_PREEMPT_AFTER));
    // 87921 and 22385 share their low 16 bits (DMR talkgroups are 24-bit).
    assert!(!end_marker_frees(22_385, Some((87_921, at)), at + Duration::from_secs(5)));
}

//! Host tests for `app::lane_policy` (change 066). The routing is the
//! bench's: Primary (TG 300) on the left, TAC (301-310) and Hospital
//! (600-601) on the right, TAC above Hospital, other talkgroups right.

use super::*;
use crate::services::ui_settings::{Speakers, TgGroup};

fn groups() -> Vec<TgGroup> {
    vec![
        TgGroup { name: "Primary".into(), tgs: vec![300] },
        TgGroup { name: "TAC".into(), tgs: (301..=310).collect() },
        TgGroup { name: "Hospital".into(), tgs: vec![600, 601] },
    ]
}

fn routing(other: Side, right: &[&str]) -> Routing {
    let sp = Speakers {
        left: vec!["Primary".into()],
        right: right.iter().map(|s| s.to_string()).collect(),
        other,
        preempt: true,
    };
    Routing::new(&groups(), &sp)
}

fn bench() -> Routing {
    routing(Side::Right, &["TAC", "Hospital"])
}

fn idle(lane: Lane) -> LaneView {
    LaneView { lane, locked_tg: None, parked_freq: None, end_marker: None }
}

fn busy(lane: Lane, tg: u32) -> LaneView {
    LaneView { locked_tg: Some(tg), ..idle(lane) }
}

const NEVER: fn(u32, Option<(u32, u64)>) -> bool = |_, _| false;

fn choose(tg: u32, lanes: &[LaneView], r: &Routing) -> LaneChoice {
    let side = r.route(tg).map(|x| x.side).unwrap_or(Side::Off);
    choose_lane(tg, side, Some(859_425_000), lanes, r, NEVER)
}

#[test]
fn one_chain_behaves_as_before() {
    let r = bench();
    let one = |v: LaneView| [v];
    assert_eq!(choose(305, &one(idle(Lane::One)), &r), LaneChoice::Take(Lane::One));
    assert_eq!(choose(300, &one(idle(Lane::One)), &r), LaneChoice::Take(Lane::One));
    assert_eq!(choose(300, &one(busy(Lane::One, 300)), &r), LaneChoice::Stay(Lane::One));
    // TAC above Hospital: pre-empts; the other way round: rejected.
    assert_eq!(choose(305, &one(busy(Lane::One, 600)), &r),
               LaneChoice::Preempt(Lane::One, "priority_preempt"));
    assert_eq!(choose(600, &one(busy(Lane::One, 305)), &r), LaneChoice::Reject);
    // Primary is above everything, left or right.
    assert_eq!(choose(300, &one(busy(Lane::One, 305)), &r),
               LaneChoice::Preempt(Lane::One, "priority_preempt"));
    // End marker of the locked call: any talkgroup takes the chain.
    let ended = |_: u32, m: Option<(u32, u64)>| m.is_some();
    let v = LaneView { end_marker: Some((305, 1)), ..busy(Lane::One, 305) };
    assert_eq!(choose_lane(600, Side::Right, None, &[v], &r, ended),
               LaneChoice::Preempt(Lane::One, "end_marker_preempt"));
}

#[test]
fn sides_map_to_their_chains() {
    let r = bench();
    let both_idle = [idle(Lane::One), idle(Lane::Two)];
    assert_eq!(choose(300, &both_idle, &r), LaneChoice::Take(Lane::One));
    assert_eq!(choose(305, &both_idle, &r), LaneChoice::Take(Lane::Two));
    assert_eq!(choose(600, &both_idle, &r), LaneChoice::Take(Lane::Two));
    // Other talkgroups are on the right here.
    assert_eq!(choose(999, &both_idle, &r), LaneChoice::Take(Lane::Two));
}

#[test]
fn primary_and_tac_play_at_once() {
    let r = bench();
    assert_eq!(choose(305, &[busy(Lane::One, 300), idle(Lane::Two)], &r),
               LaneChoice::Take(Lane::Two));
    assert_eq!(choose(300, &[idle(Lane::One), busy(Lane::Two, 305)], &r),
               LaneChoice::Take(Lane::One));
}

#[test]
fn a_busy_side_does_not_borrow_the_other_chain() {
    let r = bench();
    // Hospital while TAC holds the right chain: rejected, chain 1 idle.
    assert_eq!(choose(600, &[idle(Lane::One), busy(Lane::Two, 305)], &r), LaneChoice::Reject);
    // TAC while Primary holds chain 1 and TAC's own chain is busy with
    // another TAC talkgroup: rejected too (equal rank).
    assert_eq!(choose(306, &[busy(Lane::One, 300), busy(Lane::Two, 305)], &r), LaneChoice::Reject);
}

#[test]
fn priority_applies_within_a_chain() {
    let r = bench();
    // TAC takes the right chain from Hospital; chain 1 is not involved.
    assert_eq!(choose(305, &[busy(Lane::One, 300), busy(Lane::Two, 600)], &r),
               LaneChoice::Preempt(Lane::Two, "priority_preempt"));
    // An ungrouped talkgroup never pre-empts a grouped call.
    assert_eq!(choose(999, &[idle(Lane::One), busy(Lane::Two, 600)], &r), LaneChoice::Reject);
}

#[test]
fn a_talkgroup_stays_on_its_chain() {
    let r = bench();
    // TG 999 is on chain 1 (settings changed mid-call, or it was "both"):
    // its next grant stays there although its side is now right.
    assert_eq!(choose(999, &[busy(Lane::One, 999), idle(Lane::Two)], &r),
               LaneChoice::Stay(Lane::One));
}

#[test]
fn other_talkgroups_on_both_use_either_chain() {
    let r = routing(Side::Both, &["TAC", "Hospital"]);
    let f = Some(859_425_000);
    // Both idle, neither tuned there, both sides have groups: chain 1.
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), idle(Lane::Two)], &r, NEVER),
               LaneChoice::Take(Lane::One));
    // Chain 2 already tuned to the grant's frequency: it resumes there.
    let parked = LaneView { parked_freq: f, ..idle(Lane::Two) };
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), parked], &r, NEVER),
               LaneChoice::Take(Lane::Two));
    // One busy: the idle one.
    assert_eq!(choose_lane(999, Side::Both, f, &[busy(Lane::One, 300), idle(Lane::Two)], &r, NEVER),
               LaneChoice::Take(Lane::Two));
    // Right side without groups: prefer chain 2, keeping the left free.
    let r = routing(Side::Both, &[]);
    assert!(!r.side_has_groups(Side::Right) && r.side_has_groups(Side::Left));
    assert_eq!(choose_lane(999, Side::Both, f, &[idle(Lane::One), idle(Lane::Two)], &r, NEVER),
               LaneChoice::Take(Lane::Two));
}

#[test]
fn off_side_has_no_chain() {
    let r = bench();
    assert!(candidates(Side::Off, None, &[idle(Lane::One), idle(Lane::Two)], &r).is_empty());
    assert_eq!(choose_lane(999, Side::Off, None, &[idle(Lane::One), idle(Lane::Two)], &r, NEVER),
               LaneChoice::Reject);
    assert_eq!(LaneChoice::Reject.lane(), None);
    assert_eq!(LaneChoice::Take(Lane::Two).lane(), Some(Lane::Two));
}

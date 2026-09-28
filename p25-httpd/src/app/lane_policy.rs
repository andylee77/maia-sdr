//! Change 066: which traffic chain follows a grant (pure, host-tested).
//!
//! With one chain everything goes to it, exactly as before 066: take it
//! when idle, stay with a talkgroup it already follows, pre-empt at the
//! locked call's end marker or for a higher-priority group, else reject.
//!
//! With two chains, chain 1 serves the left speaker's groups and chain 2
//! the right's. A talkgroup on "both" (the "other talkgroups" setting)
//! may use either chain. The same take / stay / pre-empt / reject rules
//! then apply within the grant's candidate chains. A busy side does not
//! borrow the other side's chain: that chain stays free for its own
//! side's next grant, and one speaker never carries two calls at once.

use crate::hardware::traffic_lane::Lane;
use crate::services::ui_settings::{Routing, Side};

/// What the follower knows about one chain when a grant arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneView {
    pub lane: Lane,
    /// Talkgroup the chain follows (`None`: idle).
    pub locked_tg: Option<u16>,
    /// Frequency the chain is tuned to, idle or not (`None`: never tuned
    /// or released).
    pub parked_freq: Option<u64>,
    /// The locked call's pending end-of-transmission marker (tg, unix ms).
    pub end_marker: Option<(u16, u64)>,
}

/// Where a grant goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneChoice {
    /// The chain already follows this talkgroup (a repeat grant, the next
    /// talker, or the talkgroup moving channel): it stays there. Calls
    /// never move between chains.
    Stay(Lane),
    /// An idle chain takes it.
    Take(Lane),
    /// The chain's call yields to it (`"end_marker_preempt"` or
    /// `"priority_preempt"`).
    Preempt(Lane, &'static str),
    /// Every candidate chain is busy with a call that does not yield.
    Reject,
}

impl LaneChoice {
    pub fn lane(self) -> Option<Lane> {
        match self {
            LaneChoice::Stay(l) | LaneChoice::Take(l) | LaneChoice::Preempt(l, _) => Some(l),
            LaneChoice::Reject => None,
        }
    }
}

/// The chains a grant on `side` may use, most preferred first.
pub fn candidates(side: Side, grant_freq: Option<u64>, lanes: &[LaneView], routing: &Routing) -> Vec<Lane> {
    if lanes.len() <= 1 {
        return lanes.iter().map(|v| v.lane).collect();
    }
    let has = |l: Lane| lanes.iter().any(|v| v.lane == l);
    match side {
        Side::Left => [Lane::One].into_iter().filter(|&l| has(l)).collect(),
        Side::Right => [Lane::Two].into_iter().filter(|&l| has(l)).collect(),
        Side::Off => Vec::new(),
        Side::Both => {
            // Prefer the chain already tuned to the grant's frequency (it
            // resumes without a retune), then the side that carries no
            // groups (the other side stays free for them), then chain 1.
            let side_of = |l: Lane| if l == Lane::One { Side::Left } else { Side::Right };
            let mut v: Vec<&LaneView> = lanes.iter().collect();
            v.sort_by_key(|x| {
                let parked = grant_freq.is_some() && x.parked_freq == grant_freq;
                (!parked, routing.side_has_groups(side_of(x.lane)), x.lane)
            });
            v.into_iter().map(|x| x.lane).collect()
        }
    }
}

/// Choose the chain for a clear, followed grant of `tg` on `side`.
/// `end_frees(locked_tg, marker)` says whether a locked call's pending
/// end marker lets another talkgroup take its chain
/// (`grant_follower::end_marker_frees_chain` at the current time).
pub fn choose_lane(
    tg: u16,
    side: Side,
    grant_freq: Option<u64>,
    lanes: &[LaneView],
    routing: &Routing,
    end_frees: impl Fn(u16, Option<(u16, u64)>) -> bool,
) -> LaneChoice {
    if let Some(v) = lanes.iter().find(|v| v.locked_tg == Some(tg)) {
        return LaneChoice::Stay(v.lane);
    }
    let cands = candidates(side, grant_freq, lanes, routing);
    let view = |l: Lane| lanes.iter().find(|v| v.lane == l).copied();
    let views: Vec<LaneView> = cands.iter().filter_map(|&l| view(l)).collect();
    if let Some(v) = views.iter().find(|v| v.locked_tg.is_none()) {
        return LaneChoice::Take(v.lane);
    }
    if let Some(v) = views.iter().find(|v| v.locked_tg.is_some_and(|t| end_frees(t, v.end_marker))) {
        return LaneChoice::Preempt(v.lane, "end_marker_preempt");
    }
    // The candidate whose call ranks lowest yields first (the more
    // preferred candidate on a tie). A call no longer followed ranks last.
    let rank = |t: u16| routing.route(t).map_or(u32::MAX, |r| r.rank as u32);
    let mut best: Option<(u32, Lane)> = None;
    for v in &views {
        let Some(t) = v.locked_tg else { continue };
        if routing.preempts(tg, t) && best.map_or(true, |(r, _)| rank(t) > r) {
            best = Some((rank(t), v.lane));
        }
    }
    best.map_or(LaneChoice::Reject, |(_, l)| LaneChoice::Preempt(l, "priority_preempt"))
}

#[cfg(test)]
#[path = "lane_policy_tests.rs"]
mod tests;

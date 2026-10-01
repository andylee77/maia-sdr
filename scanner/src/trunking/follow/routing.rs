//! A profile as the follower reads it: each talkgroup's speaker side and priority, the ignore
//! list and pre-emption.

use std::collections::{HashMap, HashSet};

use crate::services::config::profiles::{Profile, Side};

/// Priority rank of the talkgroups in no group (the lowest).
pub const OTHER_RANK: u16 = u16::MAX;

/// How the follower treats one talkgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub side: Side,
    /// Position of its group in the group list (0 = the highest priority).
    pub rank: u16,
}

#[derive(Debug, Clone, Default)]
pub struct Routing {
    by_tg: HashMap<u32, Route>,
    other: Side,
    preempt: bool,
    ignored: HashSet<u32>,
    /// Followed talkgroups; empty = every one.
    monitor: HashSet<u32>,
}

impl Routing {
    pub fn new(profile: &Profile) -> Self {
        let sp = &profile.speakers;
        let mut by_tg = HashMap::new();
        for (i, g) in profile.groups.iter().enumerate() {
            let side = if sp.left.contains(&g.name) {
                Side::Left
            } else if sp.right.contains(&g.name) {
                Side::Right
            } else {
                Side::Off
            };
            for &tg in &g.talkgroups {
                // A talkgroup in several groups takes the first (highest).
                by_tg.entry(tg).or_insert(Route { side, rank: i as u16 });
            }
        }
        Routing {
            by_tg,
            other: sp.other,
            preempt: sp.preempt,
            ignored: profile.ignore.iter().copied().collect(),
            monitor: profile.monitor.iter().copied().collect(),
        }
    }

    /// On the ignore list (it wins over the monitor list and the groups).
    pub fn ignored(&self, tg: u32) -> bool {
        self.ignored.contains(&tg)
    }

    pub fn monitored(&self, tg: u32) -> bool {
        self.monitor.is_empty() || self.monitor.contains(&tg)
    }

    /// `None`: not followed (its group is on neither speaker, or it is in no group and "other
    /// talkgroups" is off, or it is ignored).
    pub fn route(&self, tg: u32) -> Option<Route> {
        if self.ignored(tg) {
            return None;
        }
        let r = self.by_tg.get(&tg).copied().unwrap_or(Route { side: self.other, rank: OTHER_RANK });
        (r.side != Side::Off).then_some(r)
    }

    /// Should a grant for `new_tg` take a lane from the call of `active_tg`? Only a strictly
    /// higher-priority group, with pre-emption on. A call no longer followed (settings changed
    /// mid-call) yields to any followed grant.
    pub fn preempts(&self, new_tg: u32, active_tg: u32) -> bool {
        if !self.preempt || new_tg == active_tg {
            return false;
        }
        match (self.route(new_tg), self.route(active_tg)) {
            (Some(n), Some(a)) => n.rank < a.rank,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// A group with talkgroups plays on `side`.
    pub fn side_has_groups(&self, side: Side) -> bool {
        self.by_tg.values().any(|r| r.side == side)
    }
}

//! A system's aliases as the follower reads them: each talkgroup's speaker and priority, the
//! talkgroups never followed, and pre-emption.

use crate::services::config::aliases::{Alias, AliasIndex, Listening, Side};

/// Priority rank of the talkgroups with no priority (the lowest).
pub const OTHER_RANK: u16 = u16::MAX;

/// How the follower treats one talkgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub side: Side,
    /// Its monitor priority (1 = the highest); `OTHER_RANK` with none.
    pub rank: u16,
}

#[derive(Debug, Clone, Default)]
pub struct Routing {
    aliases: AliasIndex,
    listening: Listening,
}

impl Routing {
    pub fn new(aliases: &[Alias], listening: Listening) -> Self {
        Routing { aliases: AliasIndex::new(aliases), listening }
    }

    /// Its alias says never follow it.
    pub fn ignored(&self, tg: u32) -> bool {
        self.aliases.talkgroup(tg).is_some_and(|a| a.do_not_monitor)
    }

    /// Not ignored, and it has a priority or talkgroups with none are followed.
    pub fn monitored(&self, tg: u32) -> bool {
        !self.ignored(tg) && (self.listening.follow_unmonitored || self.aliases.talkgroup(tg).is_some_and(|a| a.priority.is_some()))
    }

    /// `None`: not followed (never, or it has no priority and only those with one are).
    pub fn route(&self, tg: u32) -> Option<Route> {
        if !self.monitored(tg) {
            return None;
        }
        Some(match self.aliases.talkgroup(tg) {
            Some(a) => Route { side: a.speaker, rank: a.priority.map_or(OTHER_RANK, u16::from) },
            None => Route { side: self.listening.unmonitored_speaker, rank: OTHER_RANK },
        })
    }

    /// Its alias says record its calls.
    pub fn record(&self, tg: u32) -> bool {
        self.aliases.talkgroup(tg).is_some_and(|a| a.record)
    }

    /// Should a grant for `new_tg` take a lane from the call of `active_tg`? Only a strictly
    /// higher priority, with pre-emption on. A call no longer followed (settings changed
    /// mid-call) yields to any followed grant.
    pub fn preempts(&self, new_tg: u32, active_tg: u32) -> bool {
        if !self.listening.preempt || new_tg == active_tg {
            return false;
        }
        match (self.route(new_tg), self.route(active_tg)) {
            (Some(n), Some(a)) => n.rank < a.rank,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Talkgroups with a priority play on `side`.
    pub fn side_has_groups(&self, side: Side) -> bool {
        self.aliases.aliases().iter().any(|a| a.priority.is_some() && !a.do_not_monitor && a.speaker == side)
    }
}

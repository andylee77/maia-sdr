//! Phase 7B: Talkgroup monitor list.
//!
//! When non-empty, the grant follower only follows TGs in this list.
//! When empty, it falls back to "newest grant" (Phase 7A.1 behavior).
//! Priority order: first TG in the list wins when multiple monitored
//! TGs have concurrent grants.

use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct MonitorList {
    /// Ordered list of monitored TGs (first = highest priority).
    priority: Vec<u16>,
    /// Fast membership lookup.
    set: HashSet<u16>,
}

impl MonitorList {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn contains(&self, tg: u16) -> bool {
        self.set.contains(&tg)
    }

    /// Replace the entire list. Deduplicates, preserving first occurrence.
    pub fn set(&mut self, tgs: Vec<u16>) {
        self.set.clear();
        self.priority.clear();
        for tg in tgs {
            if self.set.insert(tg) {
                self.priority.push(tg);
            }
        }
    }

    pub fn add(&mut self, tg: u16) {
        if self.set.insert(tg) {
            self.priority.push(tg);
        }
    }

    pub fn remove(&mut self, tg: u16) {
        if self.set.remove(&tg) {
            self.priority.retain(|&t| t != tg);
        }
    }

    /// Return the priority-ordered list.
    pub fn list(&self) -> &[u16] {
        &self.priority
    }

    /// Given a TG, return its priority index (lower = higher priority).
    /// Returns `usize::MAX` if not in the list (used for comparison).
    pub fn priority_of(&self, tg: u16) -> usize {
        self.priority.iter().position(|&t| t == tg).unwrap_or(usize::MAX)
    }
}

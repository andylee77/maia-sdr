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
    priority: Vec<u32>,
    /// Fast membership lookup.
    set: HashSet<u32>,
}

impl MonitorList {
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn contains(&self, tg: u32) -> bool {
        self.set.contains(&tg)
    }

    /// Replace the entire list. Deduplicates, preserving first occurrence.
    pub fn set(&mut self, tgs: Vec<u32>) {
        self.set.clear();
        self.priority.clear();
        for tg in tgs {
            if self.set.insert(tg) {
                self.priority.push(tg);
            }
        }
    }

    pub fn add(&mut self, tg: u32) {
        if self.set.insert(tg) {
            self.priority.push(tg);
        }
    }

    pub fn remove(&mut self, tg: u32) {
        if self.set.remove(&tg) {
            self.priority.retain(|&t| t != tg);
        }
    }

    /// Return the priority-ordered list.
    pub fn list(&self) -> &[u32] {
        &self.priority
    }

}

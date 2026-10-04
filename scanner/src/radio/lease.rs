//! Who may move the radio. Normally the live site's follower and planner do; a site switch or a
//! scan takes the radio for itself, and grants decoded meanwhile are dropped (they belong to
//! whatever the radio is passing through); ATSC mode holds it for as long as it lasts. Dropping
//! the guard hands the radio back.

use std::sync::Arc;

use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lease {
    Normal,
    Switching,
    Scan,
    Atsc,
}

impl Lease {
    pub fn as_str(self) -> &'static str {
        match self {
            Lease::Normal => "normal",
            Lease::Switching => "switching",
            Lease::Scan => "scan",
            Lease::Atsc => "atsc",
        }
    }
}

#[derive(Clone)]
pub struct RadioLease {
    state: Arc<watch::Sender<Lease>>,
}

impl Default for RadioLease {
    fn default() -> Self {
        RadioLease { state: Arc::new(watch::channel(Lease::Normal).0) }
    }
}

impl RadioLease {
    pub fn current(&self) -> Lease {
        *self.state.borrow()
    }

    pub fn is_normal(&self) -> bool {
        self.current() == Lease::Normal
    }

    #[cfg(test)]
    pub fn watch(&self) -> watch::Receiver<Lease> {
        self.state.subscribe()
    }

    /// Take the radio for `purpose`; `None` while someone else has it.
    pub fn take(&self, purpose: Lease) -> Option<LeaseGuard> {
        debug_assert!(purpose != Lease::Normal);
        let mut taken = false;
        self.state.send_if_modified(|s| {
            taken = *s == Lease::Normal;
            if taken {
                *s = purpose;
            }
            taken
        });
        taken.then(|| LeaseGuard { lease: self.clone() })
    }
}

/// The radio is taken while this lives.
pub struct LeaseGuard {
    lease: RadioLease,
}

impl Drop for LeaseGuard {
    fn drop(&mut self) {
        self.lease.state.send_replace(Lease::Normal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_holder_at_a_time() {
        let lease = RadioLease::default();
        let rx = lease.watch();
        let switching = lease.take(Lease::Switching).expect("free");
        assert_eq!(lease.current(), Lease::Switching);
        assert!(lease.take(Lease::Scan).is_none(), "a scan waits for the switch");
        assert_eq!(*rx.borrow(), Lease::Switching);
        drop(switching);
        assert!(lease.is_normal());
        assert!(lease.take(Lease::Scan).is_some());
        assert!(lease.is_normal(), "a guard dropped at once hands the radio back");
    }
}

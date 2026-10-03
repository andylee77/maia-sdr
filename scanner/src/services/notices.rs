//! What happened, the moment it happens: a call opened or closed, a recording saved, a part of
//! the configuration changed. `/ws/live` turns them into the state a page shows; `/ws/events`
//! passes them on as they are.

use serde::Serialize;
use tokio::sync::broadcast;

/// Notices a slow listener may miss before it lags.
const BACKLOG: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    CallOpened { call: u64, tg: u32, followed: bool },
    CallClosed { call: u64, tg: u32 },
    RecordingSaved { call: u64 },
    /// A followed call's alert tones are known (after it closed).
    Alert { call: u64, tg: u32 },
    /// A part of the configuration changed: `radio`, `systems`, `hold` or
    /// `recordings`.
    Changed { what: &'static str },
}

#[derive(Clone)]
pub struct Notices(broadcast::Sender<Notice>);

impl Default for Notices {
    fn default() -> Self {
        Notices(broadcast::channel(BACKLOG).0)
    }
}

impl Notices {
    pub fn send(&self, n: Notice) {
        let _ = self.0.send(n);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notice> {
        self.0.subscribe()
    }
}

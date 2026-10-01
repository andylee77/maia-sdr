//! What the UI should refresh on, the moment it happens: a call opened or closed, a recording
//! saved. Pages then read the details from the API (`/ws/events` carries these).

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

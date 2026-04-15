//! Phase 7F.1 (2026-04-14): structured in-process event log.
//!
//! A bounded ring buffer of JSON-typed events that tees the important
//! pipeline decisions (control-channel grants, traffic-follower
//! retunes, decoder state, IMBE extraction, vocoder output) out to
//! both stdout (via `tracing::info!`) and a `/api/log` endpoint the
//! dashboard reads.
//!
//! This exists because the on-dashboard counters (grants_seen,
//! retunes, imbe_frames_extracted, pcm_produced) can all read "good"
//! while the audible output is garbage. The counters tell you *how
//! many things happened* -- the event log tells you *which things
//! happened in what order*, which is what you need to debug e.g.
//! "the framer latched onto the wrong sync on retune" or "the vocoder
//! is being handed frames from a stale TG".
//!
//! Entries are produced synchronously from any task via
//! `EventLog::push`. Consumers read via `/api/log?since=N` which
//! returns entries with monotonic seq > N so the dashboard can
//! incrementally tail without re-fetching the whole ring.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Log category -- used by the dashboard's filter chips. Add new
/// variants sparingly; keep the set small so the UI filter stays
/// manageable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LogCategory {
    /// Raw TSBK grant / grant-update received from the control channel
    /// decoder (before the follower has decided what to do with it).
    Grant,
    /// TrafficManager state transitions + retune decisions from the
    /// follower task (accepted, rejected-sticky, rejected-encrypted,
    /// retune executed, call Idle transition).
    Traffic,
    /// IMBE frame extraction events from the traffic LSM decoder's
    /// voice handler: HDU / LDU1 / LDU2 / TDU batches.
    Imbe,
    /// Vocoder task: per-call summary (start, end, total frames in /
    /// PCM samples out), plus decode errors.
    Vocoder,
    /// Anything that doesn't fit the above: framer resets, monitor
    /// list changes, follower enable/disable.
    System,
}

impl LogCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogCategory::Grant => "grant",
            LogCategory::Traffic => "traffic",
            LogCategory::Imbe => "imbe",
            LogCategory::Vocoder => "vocoder",
            LogCategory::System => "system",
        }
    }
}

/// One entry in the log ring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Monotonic sequence number (process-lifetime). The dashboard
    /// uses this as the incremental-tail cursor.
    pub seq: u64,
    /// Wall-clock Unix epoch milliseconds. The dashboard formats this
    /// as HH:MM:SS.mmm in the user's local timezone.
    pub timestamp_ms: u64,
    /// Category string (not enum) so the JSON surface is stable.
    pub category: &'static str,
    /// Short human-readable message shown as the line title.
    pub message: String,
    /// Structured fields -- arbitrary JSON shape per event type, not
    /// validated. The dashboard renders this as a greyed-out
    /// key=value trailer after the message.
    pub fields: Value,
}

/// Bounded ring buffer. Append-only; oldest entries drop when full.
pub struct EventLog {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
    next_seq: AtomicU64,
}

impl EventLog {
    pub fn new(capacity: usize) -> Self {
        EventLog {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            next_seq: AtomicU64::new(1),
        }
    }

    /// Push a new entry. Infallible -- lock poisoning is ignored so
    /// logging can never panic a task. Also emits a `tracing::info!`
    /// line targeted at `p25_event_log` so the systemd journal on the
    /// board has the same events even without the dashboard.
    pub fn push(
        &self,
        category: LogCategory,
        message: impl Into<String>,
        fields: Value,
    ) {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let msg = message.into();

        // Mirror to stdout for `journalctl -u p25-httpd -f`.
        tracing::info!(
            target: "p25_event_log",
            category = category.as_str(),
            %fields,
            "{}",
            msg,
        );

        let entry = LogEntry {
            seq,
            timestamp_ms: ts,
            category: category.as_str(),
            message: msg,
            fields,
        };

        if let Ok(mut guard) = self.entries.lock() {
            if guard.len() >= self.capacity {
                guard.pop_front();
            }
            guard.push_back(entry);
        }
    }

    /// Return entries with `seq > since`, capped at `limit`. The
    /// dashboard passes `since = last_seen_seq` for incremental tail
    /// and `since = 0` for the initial load.
    pub fn recent_since(&self, since: u64, limit: usize) -> Vec<LogEntry> {
        let guard = match self.entries.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        guard
            .iter()
            .filter(|e| e.seq > since)
            .take(limit)
            .cloned()
            .collect()
    }

    /// Current length of the ring (diagnostic surface for
    /// /api/log?stats=1).
    pub fn len(&self) -> usize {
        self.entries.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Monotonic sequence of the last pushed entry, or 0 if empty.
    pub fn last_seq(&self) -> u64 {
        self.next_seq.load(Ordering::Relaxed).saturating_sub(1)
    }
}

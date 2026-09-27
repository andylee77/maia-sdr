//! Change 057: per-call decode counters, attributed at the source by
//! call_id.
//!
//! Before 057 every per-call number (`/api/grant_decode_stats`,
//! `/api/recordings`, `/api/ui/calls`) was a delta of the forwarder's
//! global counters between two instants (call open, and the close or
//! the recorder's finalise). Anything counted in between — the next
//! call's first frames, the previous call's in-flight tail — landed in
//! the wrong call (056 finding R1: 72-frame PTTs reported 144–153).
//!
//! Since 054 every decoded frame already knows its call: in airtime mode
//! the traffic reader feeds each dibit piece under the call context of
//! its air time (`ImbeForwarder::begin_segment`), and each `ImbeBatch`
//! carries that call_id to the vocoder. This book is keyed by that
//! call_id:
//!
//!   - the forwarder's voice handlers add HDU / LDU / TDU / TDULC /
//!     framer-arm / IMBE extracted / IMBE dropped for `eff_call_id()`;
//!   - the vocoder thread adds PCM / silent / error / encrypted-skipped
//!     frames for `ImbeBatch::call_id`.
//!
//! Readers (grant_stats, the recorder, the follower's call-quality gate)
//! look a call up by id whenever they like: a frame of the closing call
//! decoded after the close still lands in that call, and nothing of the
//! next call does. The global counters are unchanged (tools and the
//! bench read them).
//!
//! In `legacy` / `poll` delivery modes the forwarder has no segment
//! context and attributes to the live call_id, which is the pre-054
//! quality of attribution (in-flight frames can land in the next call).

use std::collections::VecDeque;
use std::sync::Mutex;

/// Decode counters of one call. Field names match `GrantDecodeSummary`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct CallCounts {
    pub hdu: u64,
    pub ldu1: u64,
    pub ldu2: u64,
    pub tdu: u64,
    pub tdu_lc: u64,
    pub framer_arm_hdu: u64,
    pub framer_arm_ldu1: u64,
    pub framer_arm_ldu2: u64,
    pub framer_arm_tdu: u64,
    pub framer_arm_tdu_lc: u64,
    pub imbe_extracted: u64,
    pub imbe_dropped: u64,
    pub vocoder_pcm_samples: u64,
    pub vocoder_errors: u64,
    pub vocoder_silent: u64,
    pub vocoder_encrypted: u64,
    /// End-of-transmission markers sent to the lifecycle for this call.
    pub end_markers: u64,
    /// `ldu1 + ldu2` when the last end marker was sent. A new marker is
    /// sent only after more voice (a TDULC repeated through the
    /// system's channel hang is one end, not many).
    pub ldus_at_last_end: u64,
}

impl CallCounts {
    pub fn ldus(&self) -> u64 {
        self.ldu1 + self.ldu2
    }

    /// Record an end-of-transmission marker if this call carried voice
    /// since the previous one. Returns true when the marker should be
    /// sent.
    pub fn note_end_marker(&mut self) -> bool {
        let ldus = self.ldus();
        if ldus == 0 || ldus == self.ldus_at_last_end {
            return false;
        }
        self.ldus_at_last_end = ldus;
        self.end_markers += 1;
        true
    }
}

/// Bounded map call_id → counters, newest call last. Calls are added on
/// their first counted event; the oldest entries are dropped beyond the
/// capacity (far more than the grant-stats and recording rings need
/// while a call is being finalised).
#[derive(Debug)]
pub struct CallCounterBook {
    inner: Mutex<VecDeque<(u64, CallCounts)>>,
    cap: usize,
}

impl Default for CallCounterBook {
    fn default() -> Self {
        Self::new(Self::DEFAULT_CAP)
    }
}

impl CallCounterBook {
    /// Covers the grant-stats clear ring (200) plus headroom.
    pub const DEFAULT_CAP: usize = 512;

    pub fn new(cap: usize) -> Self {
        CallCounterBook {
            inner: Mutex::new(VecDeque::with_capacity(cap.min(1024))),
            cap: cap.max(1),
        }
    }

    /// Apply `f` to the counters of `call_id`, creating them on first
    /// use. Call id 0 (no call known) is not recorded: returns `None`.
    pub fn update<R>(&self, call_id: u64, f: impl FnOnce(&mut CallCounts) -> R) -> Option<R> {
        if call_id == 0 {
            return None;
        }
        let mut q = self.inner.lock().ok()?;
        // Recent calls sit at the back; search from there.
        if let Some((_, c)) = q.iter_mut().rev().find(|(id, _)| *id == call_id) {
            return Some(f(c));
        }
        let mut c = CallCounts::default();
        let r = f(&mut c);
        q.push_back((call_id, c));
        while q.len() > self.cap {
            q.pop_front();
        }
        Some(r)
    }

    pub fn get(&self, call_id: u64) -> Option<CallCounts> {
        let q = self.inner.lock().ok()?;
        q.iter().rev().find(|(id, _)| *id == call_id).map(|(_, c)| *c)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|q| q.len()).unwrap_or(0)
    }
}

#[cfg(test)]
#[path = "call_counters_tests.rs"]
mod tests;

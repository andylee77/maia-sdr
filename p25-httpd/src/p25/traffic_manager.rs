//! Traffic Channel Manager
//!
//! When a voice channel grant is detected on the control channel:
//! 1. Map logical channel number to RF frequency (via IDEN_UP table)
//! 2. Compute DDC NCO offset for the traffic channel frequency
//! 3. Command the traffic DDC to retune (write NCO frequency register)
//! 4. Start traffic DMA and monitor dibit stream for voice frames
//! 5. Manage call lifecycle (grant -> active -> teardown)
//!
//! Grant following latency budget (from DEVPLAN):
//!   TSBK received: ~20ms
//!   PS processes grant: ~1ms
//!   DDC retune (register write): ~1µs
//!   FIR flush + sync acquisition: ~40ms
//!   Total: ~60ms (P25 allows ~200ms)
//!
//! Phase 7A.1 (2026-04-11): wired into main.rs as a singleton driven by
//! a 50 ms polling task that snapshots the canonical `lsm_decoder.grants`
//! HashMap and forwards the newest entry. Polling rather than typed
//! events because the existing broadcast channel is `Sender<String>` --
//! see doc/changes/033 for the rationale and the upgrade path to typed
//! events in Phase 7B.

use std::time::Instant;

use super::types::*;

/// Traffic channel state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrafficState {
    /// No active call, waiting for grant
    Idle,
    /// DDC retuned, acquiring sync on traffic channel
    Acquiring {
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
        started: Instant,
    },
    /// Locked on traffic channel, receiving voice frames
    Active {
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
        started: Instant,
    },
}

/// Traffic channel manager
pub struct TrafficManager {
    /// Current state
    pub state: TrafficState,
    /// RX LO frequency (center of AD9361 capture band)
    rx_lo_hz: u64,
    /// ADC sample rate
    sample_rate_hz: u64,
    /// NCO word for the traffic DDC (28-bit, computed from frequency offset)
    pub nco_word: u32,
    /// Last NCO offset (Hz, signed) -- diagnostic surface for /api/traffic.
    pub last_offset_hz: i64,
    /// Timeout for sync acquisition (ms)
    acquire_timeout_ms: u64,
    /// Timeout for call inactivity before returning to idle (ms)
    call_timeout_ms: u64,
    /// Last dibit activity timestamp
    last_activity: Instant,
    /// Total handle_grant() calls (Phase 7A.1: counts grant snapshots
    /// the polling task forwarded; some are duplicates that don't
    /// trigger a retune).
    pub grants_seen: u64,
    /// Total retunes triggered (handle_grant calls that returned true).
    pub retunes: u64,
    /// Wall-clock instant of the most recent retune.
    pub last_retune_at: Option<Instant>,
}

impl TrafficManager {
    pub fn new(rx_lo_hz: u64, sample_rate_hz: u64) -> Self {
        TrafficManager {
            state: TrafficState::Idle,
            rx_lo_hz,
            sample_rate_hz,
            nco_word: 0,
            last_offset_hz: 0,
            acquire_timeout_ms: 200,
            // Phase 7A.1 sticky-lock: 2000 ms matches SDRTrunk
            // upstream PR #2010 / commit 1b3ce431's
            // STALE_EVENT_THRESHOLD_MS = 2000 in
            // P25TrafficChannelEventTracker.java. Years of P25
            // monitoring on the SDRTrunk codebase have settled on
            // 2 s as the right "this call is really over" gap; any
            // longer and we hold a singleton DDC slot through real
            // call ends, any shorter and we drop calls during PTT
            // releases between speakers in the same conversation.
            // Phase 7C will replace this with a TDU-based release
            // (with a 2 s post-TDU hold window, also from PR #2010)
            // once we have LDU/TDU sync detection.
            call_timeout_ms: 2000,
            last_activity: Instant::now(),
            grants_seen: 0,
            retunes: 0,
            last_retune_at: None,
        }
    }

    /// Single-character state label for /api/traffic JSON.
    pub fn state_label(&self) -> &'static str {
        match self.state {
            TrafficState::Idle => "Idle",
            TrafficState::Acquiring { .. } => "Acquiring",
            TrafficState::Active { .. } => "Active",
        }
    }

    /// Channel currently being followed (if any).
    pub fn current_channel(&self) -> Option<Channel> {
        match &self.state {
            TrafficState::Acquiring { channel, .. }
            | TrafficState::Active { channel, .. } => Some(*channel),
            TrafficState::Idle => None,
        }
    }

    /// Handle a voice grant from the control channel.
    /// Returns true if the traffic DDC should be retuned.
    ///
    /// **Sticky-lock policy (Phase 7A.1, derived from SDRTrunk
    /// upstream PR #2010 / commit 1b3ce431):**
    ///
    /// Call identity is determined by **talkgroup ID only** (the
    /// "TO" identifier in P25 parlance), NOT by channel ID. This
    /// matches `isSameCallCheckingToOnly()` in
    /// `P25TrafficChannelEventTracker.java` from upstream
    /// `1b3ce431`. The reason: P25 networks routinely reassign an
    /// active call from one channel to another mid-conversation
    /// (network rebalancing, channel-add via
    /// `GroupVoiceChannelGrantUpdate`). Channel-based matching
    /// would treat a reassignment as a different call and break
    /// audio continuity.
    ///
    /// Behaviour:
    ///
    /// - Same TG, same frequency -> refresh activity, no retune.
    /// - Same TG, different frequency -> retune to new frequency
    ///   (TG was reassigned by the network), keep call alive.
    /// - Different TG (any frequency) -> caller is responsible for
    ///   filtering this out via the polling task's sticky-lock
    ///   policy (only call handle_grant on a different TG when
    ///   state is Idle). If the caller violates that contract,
    ///   handle_grant will accept the new TG and start a new
    ///   call -- the manager itself does not enforce stickiness.
    ///
    /// The Idle->next-grant transition is gated entirely by the
    /// `call_timeout_ms` inactivity timer (2000 ms, matching
    /// SDRTrunk's STALE_EVENT_THRESHOLD_MS). Once Idle, any new
    /// grant is accepted via the same code path.
    pub fn handle_grant(
        &mut self,
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
    ) -> bool {
        self.grants_seen += 1;

        // Same call (same TG)? Refresh activity. If the network
        // moved the TG to a new frequency, fall through to the
        // retune path so we follow it.
        //
        // Phase 7A.1 bug-fix (same commit, post-on-target observation):
        // also auto-promote Acquiring -> Active here. The original
        // design had `sync_acquired()` as the only way to promote out
        // of Acquiring, but Phase 7A.1 has no sync detector (Phase 7C
        // will add LDU sync extraction). Without auto-promotion the
        // state stayed in Acquiring forever, and `check_timeouts`'s
        // Acquiring branch uses the 200 ms `acquire_timeout_ms`
        // against `started`, not `last_activity` -- so the call
        // unconditionally timed out 200 ms after the retune and was
        // immediately re-acquired by the next poll, producing a
        // ~4 retunes/sec thrashing cycle even with sticky lock
        // working correctly. Promoting on the very next matching
        // poll (50 ms after the retune) puts us in the Active
        // branch's 2 s `call_timeout_ms` window, which is the right
        // semantics for the Phase 7A.1 "no real sync detection yet"
        // state.
        let same_tg_same_freq = match &self.state {
            TrafficState::Active {
                talkgroup: t,
                frequency_hz: f,
                ..
            } if t.0 == talkgroup.0 => {
                self.last_activity = Instant::now();
                *f == frequency_hz
            }
            TrafficState::Acquiring {
                channel: c,
                talkgroup: t,
                frequency_hz: f,
                started: s,
            } if t.0 == talkgroup.0 => {
                let same_freq = *f == frequency_hz;
                if same_freq {
                    // Auto-promote: we have at least one same-TG
                    // poll matching, treat the chain as locked.
                    let promoted = TrafficState::Active {
                        channel: *c,
                        talkgroup: *t,
                        frequency_hz: *f,
                        started: *s,
                    };
                    self.state = promoted;
                }
                self.last_activity = Instant::now();
                same_freq
            }
            _ => false,
        };

        // Same TG, same frequency -> nothing to do.
        if same_tg_same_freq {
            return false;
        }

        // Either a fresh call (different TG, or no current call)
        // OR same TG that has been reassigned to a new frequency.
        // Both paths require a retune.
        let offset_hz = frequency_hz as i64 - self.rx_lo_hz as i64;
        let nco_frac = offset_hz as f64 / self.sample_rate_hz as f64;
        // Convert to 28-bit unsigned (two's complement wrapping)
        self.nco_word = (nco_frac * (1u64 << 28) as f64) as i32 as u32 & 0x0FFF_FFFF;
        self.last_offset_hz = offset_hz;

        self.state = TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started: Instant::now(),
        };
        let now = Instant::now();
        self.last_activity = now;
        self.retunes += 1;
        self.last_retune_at = Some(now);

        true // DDC retune needed
    }

    /// Called when we detect frame sync on the traffic channel
    pub fn sync_acquired(&mut self) {
        if let TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started,
        } = self.state.clone()
        {
            self.state = TrafficState::Active {
                channel,
                talkgroup,
                frequency_hz,
                started,
            };
            self.last_activity = Instant::now();
        }
    }

    /// Called on each traffic channel dibit to refresh activity timer
    pub fn note_activity(&mut self) {
        self.last_activity = Instant::now();
    }

    /// Check for timeouts and return to idle if needed.
    /// Returns true if state changed to Idle.
    pub fn check_timeouts(&mut self) -> bool {
        let now = Instant::now();
        let elapsed_ms = now.duration_since(self.last_activity).as_millis() as u64;

        match &self.state {
            TrafficState::Acquiring { started, .. } => {
                let acquire_elapsed =
                    now.duration_since(*started).as_millis() as u64;
                if acquire_elapsed > self.acquire_timeout_ms {
                    self.state = TrafficState::Idle;
                    return true;
                }
            }
            TrafficState::Active { .. } => {
                if elapsed_ms > self.call_timeout_ms {
                    self.state = TrafficState::Idle;
                    return true;
                }
            }
            TrafficState::Idle => {}
        }

        false
    }

    /// Check if we're currently following a call
    pub fn is_active(&self) -> bool {
        !matches!(self.state, TrafficState::Idle)
    }

    /// Get the current talkgroup being followed (if any)
    pub fn current_talkgroup(&self) -> Option<Talkgroup> {
        match &self.state {
            TrafficState::Acquiring { talkgroup, .. }
            | TrafficState::Active { talkgroup, .. } => Some(*talkgroup),
            TrafficState::Idle => None,
        }
    }

    /// Get the current traffic channel frequency (if any)
    pub fn current_frequency(&self) -> Option<u64> {
        match &self.state {
            TrafficState::Acquiring { frequency_hz, .. }
            | TrafficState::Active { frequency_hz, .. } => Some(*frequency_hz),
            TrafficState::Idle => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_nco_calculation() {
        // RX LO at 858.1 MHz, sample rate 8 MSPS
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        // Retune to 860.9625 MHz (control channel, +2.8625 MHz offset)
        let retune = mgr.handle_grant(
            Channel(0x0639),
            Talkgroup(300),
            860_962_500,
        );
        assert!(retune);

        // NCO word: 2862500 / 8000000 * 2^28 = 0x05_B3D4A0... let me compute
        // 2862500 / 8000000 = 0.35781250
        // 0.35781250 * 2^28 = 0.35781250 * 268435456 = 96050176 = 0x05B9_0000
        // The exact value depends on floating point, but should be in this range
        assert!(mgr.nco_word > 0x05B0_0000 && mgr.nco_word < 0x05C0_0000,
                "NCO word {} should be near 0x05B9_0000", mgr.nco_word);
    }

    #[test]
    fn test_negative_offset() {
        // RX LO at 858.1 MHz, target 855.2375 MHz (-2.8625 MHz)
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        let retune = mgr.handle_grant(
            Channel(0x0001),
            Talkgroup(100),
            855_237_500,
        );
        assert!(retune);
        // Negative offset should produce a large unsigned NCO word (two's complement)
        assert!(mgr.nco_word > 0x0A00_0000, "Negative offset should wrap: {:#010X}", mgr.nco_word);
    }

    #[test]
    fn test_grant_lifecycle() {
        let mut mgr = TrafficManager::new(858_100_000, 8_000_000);

        // Start idle
        assert!(!mgr.is_active());
        assert_eq!(mgr.current_talkgroup(), None);

        // Handle grant -> acquiring
        mgr.handle_grant(Channel(0x045D), Talkgroup(300), 857_987_500);
        assert!(mgr.is_active());
        assert_eq!(mgr.current_talkgroup(), Some(Talkgroup(300)));

        // Sync acquired -> active
        mgr.sync_acquired();
        assert!(matches!(mgr.state, TrafficState::Active { .. }));

        // Same channel grant just refreshes
        let retune = mgr.handle_grant(Channel(0x045D), Talkgroup(300), 857_987_500);
        assert!(!retune); // no retune needed
    }
}

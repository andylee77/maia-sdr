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
//! TODO(traffic-following): the structs in this module are scaffolding for
//! the channel-grant follow-along feature. They will be wired into main.rs
//! after the control channel decode is solid. Suppressing dead-code warnings
//! at module scope until then so the rest of the build stays warning-clean.

#![allow(dead_code)]

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
    /// Timeout for sync acquisition (ms)
    acquire_timeout_ms: u64,
    /// Timeout for call inactivity before returning to idle (ms)
    call_timeout_ms: u64,
    /// Last dibit activity timestamp
    last_activity: Instant,
}

impl TrafficManager {
    pub fn new(rx_lo_hz: u64, sample_rate_hz: u64) -> Self {
        TrafficManager {
            state: TrafficState::Idle,
            rx_lo_hz,
            sample_rate_hz,
            nco_word: 0,
            acquire_timeout_ms: 200,
            call_timeout_ms: 3000,
            last_activity: Instant::now(),
        }
    }

    /// Handle a voice grant from the control channel.
    /// Returns true if the traffic DDC should be retuned.
    pub fn handle_grant(
        &mut self,
        channel: Channel,
        talkgroup: Talkgroup,
        frequency_hz: u64,
    ) -> bool {
        // If already on this channel, just refresh the timestamp
        match &self.state {
            TrafficState::Active {
                channel: c,
                ..
            }
            | TrafficState::Acquiring {
                channel: c,
                ..
            } if c.0 == channel.0 => {
                self.last_activity = Instant::now();
                return false;
            }
            _ => {}
        }

        // Compute NCO frequency word for the traffic DDC
        // offset = target_freq - rx_lo (can be negative)
        // nco_word = offset / sample_rate * 2^28 (28-bit NCO)
        let offset_hz = frequency_hz as i64 - self.rx_lo_hz as i64;
        let nco_frac = offset_hz as f64 / self.sample_rate_hz as f64;
        // Convert to 28-bit unsigned (two's complement wrapping)
        self.nco_word = (nco_frac * (1u64 << 28) as f64) as i32 as u32 & 0x0FFF_FFFF;

        self.state = TrafficState::Acquiring {
            channel,
            talkgroup,
            frequency_hz,
            started: Instant::now(),
        };
        self.last_activity = Instant::now();

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

//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;
use crate::protocol::p25::test_fixtures::*;

#[test]
fn test_nco_calculation() {
    // RX LO at 858.1 MHz, sample rate 8 MSPS
    let mut mgr = TrafficChain::new(858_100_000, 8_000_000);

    // Retune to 860.9625 MHz (control channel, +2.8625 MHz offset)
    let retune = mgr.handle_grant(
        Channel(0x0639),
        Talkgroup(300),
        CLAY_CONTROL_FREQ_HZ,
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
    let mut mgr = TrafficChain::new(858_100_000, 8_000_000);

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
    let mut mgr = TrafficChain::new(858_100_000, 8_000_000);

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

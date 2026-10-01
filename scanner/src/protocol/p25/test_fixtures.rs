//! Shared test-only constants.
//!
//! The P25 test suite repeatedly reaches for the same handful of bare
//! hex literals — Clay County's NAC, WACN, system ID, and control
//! frequency — because that is the primary site we validate against.
//! Pulling them into named constants here removes the hand-typed
//! `0x8A1` / `0xBEE00` / `0x8A0` / `860_962_500` sprinkled across test
//! functions and makes it obvious when a test is talking about a real
//! observed site versus a synthetic value.
//!
//! These values are all copied verbatim from the tests that already
//! used them. See `reference_p25_sites.md` in the session memory for
//! the live-site inventory.
//!
//! Module is `#[cfg(test)]` only; it is not part of any production code
//! path.
#![cfg(test)]

// -------- Clay County, FL (Harris VIDA simulcast, LSM) --------

/// Clay County Network Access Code (12 bits). Matches the site the P25
/// decoder is primarily tuned against in CI.
pub const CLAY_NAC: u16 = 0x8A1;

/// Clay County System ID (12 bits), carried in NET_STS_BCST
/// (`TsbkMessage::NetworkStatus.system_id`).
pub const CLAY_SYSTEM_ID: u16 = 0x8A0;

/// Clay County control-channel centre frequency in Hz (860.9625 MHz).
/// Computed by `FrequencyBand::channel_frequency` for
/// `Channel(0x0639)` against the IDEN_UPDATE band-0 parameters below.
pub const CLAY_CONTROL_FREQ_HZ: u64 = 860_962_500;

// -------- Duval County, FL (Motorola simulcast) --------

/// Duval County Network Access Code (12 bits).
#[allow(dead_code)]
pub const DUVAL_NAC: u16 = 0x3BA;

/// Duval County control-channel centre frequency in Hz (855.4875 MHz).
#[allow(dead_code)]
pub const DUVAL_CONTROL_FREQ_HZ: u64 = 855_487_500;

// -------- Shared (both Florida sites share this WACN) --------

/// Florida statewide WACN (20 bits). Carried by NET_STS_BCST on both
/// Clay and Duval sites.
pub const FLORIDA_WACN: u32 = 0xBEE00;

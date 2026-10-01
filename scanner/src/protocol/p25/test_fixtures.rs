//! Identities of the real sites the P25 tests use.
#![cfg(test)]

/// Clay County, FL (Harris, LSM simulcast): NAC.
pub const CLAY_NAC: u16 = 0x8A1;

/// Clay County: system ID (NET_STS_BCST).
pub const CLAY_SYSTEM_ID: u16 = 0x8A0;

/// Clay County: the control channel, `Channel(0x0639)` on band 0 (base 851.00625 MHz, 6.25 kHz).
pub const CLAY_CONTROL_FREQ_HZ: u64 = 860_962_500;

/// The Florida statewide WACN (Clay and Duval).
pub const FLORIDA_WACN: u32 = 0xBEE00;

//! What aircraft send of themselves, on two links.
//!
//! At 1090 MHz: Mode S extended squitters (ADS-B) and the short all-call replies. `mode_s`
//! finds the messages in the window's samples and takes those whose parity is right;
//! `squitter` reads what a message says; `cpr` makes a whole position of an even and an odd
//! half.
//!
//! At 978 MHz: UAT, which small aircraft use in the United States (`uat`).
//!
//! Written from the formats' facts (for Mode S: pulse-position keying at 1 Mbit/s, the
//! preamble's four pulses, the 24-bit parity, a squitter's fields), not from another decoder's
//! code. `tools/adsb_look.py` and `tools/uat_look.py` read a capture the same way on the PC,
//! sample for sample, and are what the readers here are held against.

pub mod cpr;
pub mod mode_s;
pub mod squitter;
pub mod uat;

/// Where Mode S is sent, and the window's rate both readers take: four samples a microsecond,
/// two to a Mode S pulse.
pub const FREQ_HZ: u64 = 1_090_000_000;
pub const RATE_HZ: u32 = 4_000_000;

//! What aircraft send of themselves at 1090 MHz: Mode S extended squitters (ADS-B) and the short
//! all-call replies. `mode_s` finds the messages in the window's samples and takes those whose
//! parity is right; `squitter` reads what a message says; `cpr` makes a whole position of an
//! even and an odd half.
//!
//! Written from the formats' facts (pulse-position keying at 1 Mbit/s, the preamble's four
//! pulses, the 24-bit parity, a squitter's fields), not from another decoder's code.
//! `tools/adsb_look.py` reads a capture the same way on the PC, sample for sample, and is what
//! `mode_s` is held against.

pub mod cpr;
pub mod mode_s;
pub mod squitter;

/// Where the messages are sent, and the window's rate `mode_s` reads: four samples a
/// microsecond, two to a pulse.
pub const FREQ_HZ: u64 = 1_090_000_000;
pub const RATE_HZ: u32 = 4_000_000;

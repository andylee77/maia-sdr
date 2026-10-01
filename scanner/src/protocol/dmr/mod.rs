//! DMR (ETSI TS 102 361) receive, Tier III. A port of SDRTrunk's
//! `module/decode/dmr`; names follow the Java so the two can be read side by
//! side.
//!
//! Bits are `u8` values 0 or 1 in transmission order (first bit sent first)
//! throughout this module.

/// 4FSK demodulation: 50 kSPS IQ to dibits, sync-driven timing.
pub mod demod;
/// Error correction and checksums: CACH Hamming(7,4), Golay(20,8) slot type,
/// BPTC(196,96) / (68,36) / (128,77), CRC-CCITT / CRC-8 / RS(12,9) / checksum 5.
pub mod fec;
mod filters;
/// Messages from bursts: CSBKs, link control, voice; SDRTrunk's text for each.
pub mod message;
/// Bursts and timeslots from the dibit stream.
pub mod framer;
/// Sync patterns and the soft sync detector.
pub mod sync;

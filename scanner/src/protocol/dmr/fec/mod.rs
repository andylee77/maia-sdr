//! DMR error correction and checksums (SDRTrunk `edac` and `module/decode/dmr/bptc`).
//!
//! One file per SDRTrunk class family, names following the Java. Bits are `u8`
//! 0/1 in transmission order. Decoders return `Option<(bits, corrected)>`,
//! `corrected` being SDRTrunk's `CorrectedBinaryMessage.getCorrectedBitCount()`
//! and `None` its "uncorrectable" (-1 / -2).
//!
//! Burst positions used here (288-bit burst = 24-bit CACH + 264-bit burst):
//! CACH 0..24, BPTC info 24..122 and 190..288, slot type 122..132 and 180..190,
//! EMB 132..140 and 172..180.

pub mod bptc;
pub mod bptc_16_2;
pub mod bptc_196_96;
pub mod cach;
pub mod crc;
pub mod emb;
pub mod rs_12_9;
pub mod slot_type;

#[cfg(test)]
mod test_util;

pub use crate::protocol::fec::{get_int, set_int};

/// Number of positions where `a` and `b` differ (the corrected-bit count).
fn bit_distance(a: &[u8], b: &[u8]) -> u32 {
    a.iter().zip(b).filter(|(x, y)| x != y).count() as u32
}

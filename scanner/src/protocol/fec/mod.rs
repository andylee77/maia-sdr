//! Error correction shared by P25 and DMR, ported from SDRTrunk's `edac`: Golay(24,12) with its
//! Golay(18,6) shortening, and the Hamming codes. Bits are `u8` 0/1 in transmission order.

pub mod golay24;
pub mod hamming;

/// Reads `bits` (MSB first) as an unsigned integer, like `BinaryMessage.getInt`.
pub fn get_int(bits: &[u8]) -> u32 {
    bits.iter().fold(0, |acc, &b| (acc << 1) | u32::from(b & 1))
}

/// Writes `value` into `bits` (MSB first), like `BinaryMessage.setInt`.
pub fn set_int(value: u32, bits: &mut [u8]) {
    let n = bits.len();
    for (i, bit) in bits.iter_mut().enumerate() {
        *bit = ((value >> (n - 1 - i)) & 1) as u8;
    }
}

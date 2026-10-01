//! CRC-CCITT, CRC-8 and the 5-bit checksum: ports of SDRTrunk `edac/CRCDMR.java`
//! and `edac/Checksum_5_DMR.java`.
//!
//! CRC-CCITT (TS 102 361-1 B.3.7): x^16 + x^12 + x^5 + 1, zero preset, MSB
//! first, over the first 80 bits; the transmitted CRC is the ones complement of
//! the remainder XOR the data type's mask. Verified on air: the Clay Electric
//! ALOHA `99001101F6400B000000DEDA` has CRC 0xDEDA = !0x8480 ^ 0xA5A5.
//!
//! SDRTrunk leaves out the complement and accepts residual 0 or 0xFFFF, so each
//! of its masks also accepts the complemented mask (its PI header mask 0x9696
//! is ETSI's 0x6969), and its single-bit correction never matches a complemented
//! residual. This port checks the ETSI form only, with ETSI masks.

use super::get_int;

/// ETSI CRC masks for 80+16-bit blocks (Table B.21), applied after the complement.
pub const PI_HEADER_CRC_MASK: u16 = 0x6969;
pub const CSBK_CRC_MASK: u16 = 0xA5A5;
pub const MBC_HEADER_CRC_MASK: u16 = 0xAAAA;
/// SDRTrunk `CSBKMessage.MBC_LAST_BLOCK_CRC_MASK` (0x0000; SDRTrunk also accepts 0xFFFF). Not yet seen on air.
pub const MBC_LAST_BLOCK_CRC_MASK: u16 = 0x0000;
pub const DATA_HEADER_CRC_MASK: u16 = 0xCCCC;
pub const USB_DATA_CRC_MASK: u16 = 0x3333;

/// Remainder of each single data bit (0..80) of an 80-bit message, then the
/// 16 CRC bits MSB first (`CRCUtil.generate(80, 16, 0x11021, 0xFFFF, true)`;
/// SDRTrunk lists the CRC bits LSB first, so it flips the mirror bit there).
pub const CCITT_80_CHECKSUMS: [u16; 96] = [
    0xE434, 0x721A, 0x390D, 0x9496, 0x4A4B, 0xAD35, 0xDE8A, 0x6F45, 0xBFB2, 0x5FD9, 0xA7FC, 0x53FE,
    0x29FF, 0x9CEF, 0xC667, 0xEB23, 0xFD81, 0xF6D0, 0x7B68, 0x3DB4, 0x1EDA, 0x0F6D, 0x8FA6, 0x47D3,
    0xABF9, 0xDDEC, 0x6EF6, 0x377B, 0x93AD, 0xC1C6, 0x60E3, 0xB861, 0xD420, 0x6A10, 0x3508, 0x1A84,
    0x0D42, 0x06A1, 0x8B40, 0x45A0, 0x22D0, 0x1168, 0x08B4, 0x045A, 0x022D, 0x8906, 0x4483, 0xAA51,
    0xDD38, 0x6E9C, 0x374E, 0x1BA7, 0x85C3, 0xCAF1, 0xED68, 0x76B4, 0x3B5A, 0x1DAD, 0x86C6, 0x4363,
    0xA9A1, 0xDCC0, 0x6E60, 0x3730, 0x1B98, 0x0DCC, 0x06E6, 0x0373, 0x89A9, 0xCCC4, 0x6662, 0x3331,
    0x9188, 0x48C4, 0x2462, 0x1231, 0x8108, 0x4084, 0x2042, 0x1021, 0x8000, 0x4000, 0x2000, 0x1000,
    0x0800, 0x0400, 0x0200, 0x0100, 0x0080, 0x0040, 0x0020, 0x0010, 0x0008, 0x0004, 0x0002, 0x0001,
];

/// CRC-CCITT remainder (zero preset, no complement) of `bits`, bit by bit: the reference the
/// tests build and check frames with.
#[cfg(test)]
pub fn crc_ccitt(bits: &[u8]) -> u16 {
    let mut reg: u16 = 0;
    for &b in bits {
        let feedback = ((reg >> 15) as u8 & 1) ^ b;
        reg <<= 1;
        if feedback == 1 {
            reg ^= 0x1021;
        }
    }
    reg
}

/// Residual of an 80+16-bit block against `mask`: 0 = CRC good; with mask 0 it is
/// the mask in use. Ports `CRCDMR.calculateResidual()` (ETSI complement added).
pub fn calculate_residual(message: &[u8], mask: u16) -> u16 {
    let mut calculated = !mask;
    for i in 0..80 {
        if message[i] == 1 {
            calculated ^= CCITT_80_CHECKSUMS[i];
        }
    }
    calculated ^ get_int(&message[80..96]) as u16
}

/// Checks an 80+16-bit block and corrects one bit error: `Some(0)` good,
/// `Some(1)` corrected, `None` failed. Ports `CRCDMR.correctCCITT80(message, 0, 80, mask)`.
pub fn correct_ccitt80(message: &mut [u8], mask: u16) -> Option<u32> {
    let residual = calculate_residual(message, mask);
    if residual == 0 {
        return Some(0);
    }
    let position = CCITT_80_CHECKSUMS.iter().position(|&c| c == residual)?;
    message[position] ^= 1;
    Some(1)
}

/// CRC-8 (x^8 + x^2 + x + 1, zero preset) of `bits`: over the 28 data bits it is
/// the CRC, over all 36 short LC bits it is 0 when valid. Ports `CRCDMR.crc8(bits, len)`.
pub fn crc8(bits: &[u8]) -> u8 {
    let mut reg: u8 = 0;
    for &b in bits {
        let feedback = (reg >> 7) ^ b;
        reg <<= 1;
        if feedback & 1 == 1 {
            reg ^= 0x07;
        }
    }
    reg
}

/// 5-bit checksum residual of a 77-bit embedded LC (72 LC bits + checksum),
/// 0 = valid. Ports `Checksum_5_DMR.isValid()` (B.3.11: sum of the 9 bytes mod 31).
pub fn checksum_5(message: &[u8]) -> u32 {
    let accumulator: u32 = (0..9).map(|i| get_int(&message[i * 8..i * 8 + 8])).sum();
    (accumulator % 31) ^ get_int(&message[72..77])
}

#[cfg(test)]
#[path = "crc_tests.rs"]
mod tests;

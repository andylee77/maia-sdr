//! Hamming codes for the DMR BPTCs: ports of SDRTrunk `edac/IHamming.java`,
//! `Hamming13.java`, `Hamming15.java`, `Hamming16.java` and `Hamming17.java`.
//!
//! Each word is `k` data bits followed by the parity bits, MSB first. The
//! `CHECKSUMS` tables hold each position's syndrome contribution (TS 102 361-1
//! B.3.x generator matrices); a single error's syndrome is its position's entry.

use super::get_int;

/// Result of a Hamming error lookup (`IHamming` -1 / index / 1000).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorIndex {
    NoErrors,
    /// Absolute index (offset + position) of the single bit in error.
    At(usize),
    MultipleErrors,
}

/// Row code used by `BPTCBase` (`IHamming`).
pub trait IHamming {
    /// Error index of the word starting at `offset`. Ports `IHamming.getErrorIndex()`.
    fn get_error_index(&self, bits: &[u8], offset: usize) -> ErrorIndex;
}

/// XOR of `checksums[i]` for each set bit among the first `count` bits at `offset`.
fn calculate(bits: &[u8], offset: usize, count: usize, checksums: &[u32]) -> u32 {
    let mut calculated = 0;
    for i in 0..count {
        if bits[offset + i] == 1 {
            calculated ^= checksums[i];
        }
    }
    calculated
}

/// Hamming(13,9,3), used on BPTC(196,96) columns.
pub struct Hamming13;

impl Hamming13 {
    /// TS 102 361-1 Table B.14.
    pub const CHECKSUMS: [u32; 13] = [
        0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3, 0x8, 0x4, 0x2, 0x1,
    ];
    /// Syndrome to position; -1 = not a single-bit syndrome (9 and 13).
    const ERROR_INDEX: [i8; 16] = [-1, 12, 11, 8, 10, 4, 7, 2, 9, -1, 3, 5, 6, -1, 1, 0];

    /// Parity (4 bits) of the 9 data bits at `indices[0..9]`. Ports `calculateChecksum(message, indices)`.
    pub fn calculate_checksum(bits: &[u8], indices: &[usize; 13]) -> u32 {
        let mut calculated = 0;
        for x in 0..9 {
            if bits[indices[x]] == 1 {
                calculated ^= Self::CHECKSUMS[x];
            }
        }
        calculated
    }

    /// Syndrome of the word at `indices`. Ports `Hamming13.getSyndrome(message, indices)`.
    pub fn get_syndrome(bits: &[u8], indices: &[usize; 13]) -> u32 {
        let mut checksum = 0;
        for &i in &indices[9..13] {
            checksum = (checksum << 1) | u32::from(bits[i]);
        }
        Self::calculate_checksum(bits, indices) ^ checksum
    }

    /// Message index of the bit in error. Ports `Hamming13.getErrorIndex(message, indices)`.
    pub fn get_error_index(bits: &[u8], indices: &[usize; 13]) -> ErrorIndex {
        match Self::get_syndrome(bits, indices) {
            0 => ErrorIndex::NoErrors,
            s => match Self::ERROR_INDEX[s as usize] {
                -1 => ErrorIndex::MultipleErrors,
                p => ErrorIndex::At(indices[p as usize]),
            },
        }
    }
}

/// Hamming(15,11,3), used on BPTC(196,96) rows.
pub struct Hamming15;

impl Hamming15 {
    /// TS 102 361-1 Table B.15.
    pub const CHECKSUMS: [u32; 15] = [
        0x9, 0xD, 0xF, 0xE, 0x7, 0xA, 0x5, 0xB, 0xC, 0x6, 0x3, 0x8, 0x4, 0x2, 0x1,
    ];
    /// Syndrome to position (a perfect code: every syndrome is a single error).
    const ERROR_INDEX: [u8; 16] = [0, 14, 13, 10, 12, 6, 9, 4, 11, 0, 5, 7, 8, 1, 3, 2];

    /// Parity (4 bits) of the 11 data bits at `offset`. Ports `Hamming15.calculateChecksum()`.
    pub fn calculate_checksum(bits: &[u8], offset: usize) -> u32 {
        calculate(bits, offset, 11, &Self::CHECKSUMS)
    }

    /// Syndrome of the word at `offset`. Ports `Hamming15.getSyndrome()`.
    pub fn get_syndrome(bits: &[u8], offset: usize) -> u32 {
        Self::calculate_checksum(bits, offset) ^ get_int(&bits[offset + 11..offset + 15])
    }

    /// Message index of the bit in error. Ports `Hamming15.getErrorIndex()`.
    pub fn get_error_index(bits: &[u8], offset: usize) -> ErrorIndex {
        match Self::get_syndrome(bits, offset) {
            0 => ErrorIndex::NoErrors,
            s => ErrorIndex::At(offset + usize::from(Self::ERROR_INDEX[s as usize])),
        }
    }
}

/// Hamming(16,11,4), used on BPTC(128,77) rows (embedded LC).
pub struct Hamming16;

impl Hamming16 {
    pub const CHECKSUMS: [u32; 16] = [
        0x13, 0x1A, 0x1F, 0x1C, 0x0E, 0x15, 0x0B, 0x16, 0x19, 0x0D, 0x07, 0x10, 0x08, 0x04, 0x02,
        0x01,
    ];

    /// Parity (5 bits) of the 11 data bits at `offset`. Ports `Hamming16.calculateChecksum()`.
    pub fn calculate_checksum(bits: &[u8], offset: usize) -> u32 {
        calculate(bits, offset, 11, &Self::CHECKSUMS)
    }

    /// Syndrome of the word at `offset`. Ports `Hamming16.getSyndrome()`.
    pub fn get_syndrome(bits: &[u8], offset: usize) -> u32 {
        Self::calculate_checksum(bits, offset) ^ get_int(&bits[offset + 11..offset + 16])
    }
}

impl IHamming for Hamming16 {
    /// Ports `Hamming16.getErrorIndex()` without its "syndrome 1 and odd word
    /// weight = multiple errors" test: every codeword has even weight, so that
    /// test only ever rejected a genuine single error in bit 15.
    fn get_error_index(&self, bits: &[u8], offset: usize) -> ErrorIndex {
        let syndrome = Self::get_syndrome(bits, offset);
        if syndrome == 0 {
            return ErrorIndex::NoErrors;
        }
        match Self::CHECKSUMS.iter().position(|&c| c == syndrome) {
            Some(p) => ErrorIndex::At(offset + p),
            None => ErrorIndex::MultipleErrors,
        }
    }
}

/// Hamming(17,12,3), used on BPTC(68,36) rows (short LC).
pub struct Hamming17;

impl Hamming17 {
    pub const CHECKSUMS: [u32; 17] = [
        0x1B, 0x1F, 0x1D, 0x1C, 0x0E, 0x07, 0x11, 0x1A, 0x0D, 0x14, 0x0A, 0x05, 0x10, 0x08, 0x04,
        0x02, 0x01,
    ];

    /// Parity (5 bits) of the 12 data bits at `offset`. Ports `Hamming17.calculateChecksum()`.
    pub fn calculate_checksum(bits: &[u8], offset: usize) -> u32 {
        calculate(bits, offset, 12, &Self::CHECKSUMS)
    }

    /// Syndrome of the word at `offset`. Ports `Hamming17.getSyndrome()`.
    pub fn get_syndrome(bits: &[u8], offset: usize) -> u32 {
        Self::calculate_checksum(bits, offset) ^ get_int(&bits[offset + 12..offset + 17])
    }
}

impl IHamming for Hamming17 {
    /// Ports `Hamming17.getErrorIndex()` without its "syndrome 1 and odd word
    /// weight = multiple errors" test. Code weights are mixed, so the test is a
    /// coin flip; it blocked two bit-16 errors in one column, and the step 1
    /// mis-corrections it sometimes avoided are handled by BPTCBase's retry.
    fn get_error_index(&self, bits: &[u8], offset: usize) -> ErrorIndex {
        let syndrome = Self::get_syndrome(bits, offset);
        if syndrome == 0 {
            return ErrorIndex::NoErrors;
        }
        match Self::CHECKSUMS.iter().position(|&c| c == syndrome) {
            Some(p) => ErrorIndex::At(offset + p),
            None => ErrorIndex::MultipleErrors,
        }
    }
}

#[cfg(test)]
#[path = "hamming_tests.rs"]
mod tests;

//! Extended Golay(24,12,8): port of SDRTrunk `edac/Golay24.java`.
//!
//! Word layout: 12 data bits, 11 Golay(23,12) checksum bits, 1 even parity bit
//! over all 24. DMR's slot type Golay(20,8) is this code with 4 leading zero
//! data bits (see `slot_type`).
//!
//! SDRTrunk decodes by error trapping and returns 0/1/2 (2 = failed) rather
//! than a bit count. This port decodes the perfect Golay(23,12) part with a
//! syndrome table (all patterns of up to 3 errors), then uses the parity bit to
//! fix bit 23 or to detect a 4th error, and reports the exact corrected count.

use std::sync::OnceLock;

use super::get_int;

/// Syndrome contribution of each of the 23 positions (`CRCUtil.generate(12, 11, 0xC75, 0x0, true)`).
pub const CHECKSUMS: [u32; 23] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3, 0x6E3, 0x54B, 0x49F, 0x475, 0x400,
    0x200, 0x100, 0x080, 0x040, 0x020, 0x010, 0x008, 0x004, 0x002, 0x001,
];

/// Checksum (11 bits) of the 12 data bits at `start`. Ports `Golay24.calculateChecksum()`.
pub fn calculate_checksum(bits: &[u8], start: usize) -> u32 {
    let mut calculated = 0;
    for i in 0..12 {
        if bits[start + i] == 1 {
            calculated ^= CHECKSUMS[i];
        }
    }
    calculated
}

/// Golay(23,12) syndrome of the word at `start`. Ports `Golay24.getSyndrome()`.
pub fn get_syndrome(bits: &[u8], start: usize) -> u32 {
    calculate_checksum(bits, start) ^ get_int(&bits[start + 12..start + 23])
}

/// Syndrome to error pattern (bit `i` = position `i`) for every pattern of weight <= 3.
fn syndrome_table() -> &'static [u32; 2048] {
    static TABLE: OnceLock<[u32; 2048]> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [u32::MAX; 2048];
        table[0] = 0;
        for a in 0..23 {
            let sa = CHECKSUMS[a];
            table[sa as usize] = 1 << a;
            for b in a + 1..23 {
                let sb = sa ^ CHECKSUMS[b];
                table[sb as usize] = (1 << a) | (1 << b);
                for c in b + 1..23 {
                    table[(sb ^ CHECKSUMS[c]) as usize] = (1 << a) | (1 << b) | (1 << c);
                }
            }
        }
        table
    })
}

/// Corrects the 24-bit word at `start` in place; returns the corrected bit
/// count, or `None` (word unchanged) for 4+ detected errors. Ports `Golay24.checkAndCorrect()`.
pub fn check_and_correct(bits: &mut [u8], start: usize) -> Option<u32> {
    let word = &mut bits[start..start + 24];
    let pattern = syndrome_table()[get_syndrome(word, 0) as usize];
    let mut corrected = pattern.count_ones();
    let flip = |word: &mut [u8]| {
        for (i, bit) in word.iter_mut().take(23).enumerate() {
            *bit ^= ((pattern >> i) & 1) as u8;
        }
    };

    flip(word);
    let parity = word.iter().fold(0, |acc, &b| acc ^ b);
    if parity != 0 {
        if corrected == 3 {
            flip(word);
            return None;
        }
        word[23] ^= 1;
        corrected += 1;
    }
    Some(corrected)
}

#[cfg(test)]
#[path = "golay24_tests.rs"]
mod tests;

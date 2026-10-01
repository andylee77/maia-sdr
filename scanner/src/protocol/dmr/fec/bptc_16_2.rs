//! BPTC(16,2) for the single-burst embedded signalling of voice burst F: port
//! of SDRTrunk `module/decode/dmr/bptc/BPTC_16_2.java` (TS 102 361-1 B.2.2).
//!
//! The 32 bits are two 16-bit rows: a Hamming(16,11,4) word and, for the
//! non-reverse-channel single burst, an identical copy (even column parity).

use super::hamming::{ErrorIndex, Hamming16, IHamming};

/// Transmitted bit `x` goes to deinterleaved bit `DEINTERLEAVE[x]`.
pub const DEINTERLEAVE: [usize; 32] = [
    0, 24, 1, 25, 2, 26, 3, 27, 4, 28, 5, 29, 6, 30, 7, 31, 8, 16, 9, 17, 10, 18, 11, 19, 12, 20,
    13, 21, 14, 22, 15, 23,
];

/// Deinterleaves the 32 transmitted bits. Ports `BPTC_16_2.deinterleave()`.
pub fn deinterleave(original: &[u8]) -> [u8; 32] {
    let mut deinterleaved = [0u8; 32];
    for x in 0..32 {
        deinterleaved[DEINTERLEAVE[x]] = original[x];
    }
    deinterleaved
}

/// Deinterleaves, corrects one bit of the Hamming row and checks that the
/// second row repeats it; `None` otherwise. Ports `BPTC_16_2.decodeShortBurst()`.
pub fn decode_short_burst(fragment: &[u8]) -> Option<[u8; 32]> {
    let mut deinterleaved = deinterleave(fragment);
    match Hamming16.get_error_index(&deinterleaved, 0) {
        ErrorIndex::NoErrors => {}
        ErrorIndex::At(index) => deinterleaved[index] ^= 1,
        ErrorIndex::MultipleErrors => return None,
    }
    if (0..16).any(|x| deinterleaved[x] != deinterleaved[x + 16]) {
        return None;
    }
    Some(deinterleaved)
}

#[cfg(test)]
#[path = "bptc_16_2_tests.rs"]
mod tests;

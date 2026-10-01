//! Voice burst EMB, Quadratic Residue (16,7,6): port of SDRTrunk
//! `module/decode/dmr/message/voice/EMB.java` (TS 102 361-1 B.3.2).
//!
//! 16 bits: colour code (4), PI (1), LCSS (2), 9 parity. As in SDRTrunk, the
//! word is corrected to the first codeword within 3 bits, but only 0 or 1
//! corrected bits count as valid (d = 6 would allow 2).

use super::get_int;

/// Burst positions of the 16 EMB bits (8 either side of the embedded signalling).
pub const EMB_INDEXES: [usize; 16] = [
    132, 133, 134, 135, 136, 137, 138, 139, 172, 173, 174, 175, 176, 177, 178, 179,
];

/// (15,7) cyclic code generator g(x) = x^8 + x^5 + x^4 + x^3 + 1; bit 0 of
/// each word is even parity over the other 15.
const GENERATOR: u32 = 0x139;

/// All 128 codewords, indexed by the 7 information bits. SDRTrunk's literal
/// table matches for indexes 0..63 but has row 64 as 0x802F instead of 0x804F,
/// so entries 64..127 (colour codes 8..15) are off by 0x0060 and its code has
/// a weight-4 word (index 76). Generated here from g(x): d = 6.
pub const VALID_WORDS: [u16; 128] = build_valid_words();

const fn build_valid_words() -> [u16; 128] {
    let mut words = [0u16; 128];
    let mut data = 0;
    while data < 128 {
        let mut remainder = (data as u32) << 8;
        let mut bit = 14;
        while bit >= 8 {
            if (remainder >> bit) & 1 == 1 {
                remainder ^= GENERATOR << (bit - 8);
            }
            bit -= 1;
        }
        let word = ((data as u32) << 8) | remainder;
        words[data] = ((word << 1) | (word.count_ones() & 1)) as u16;
        data += 1;
    }
    words
}

/// Decoded EMB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Emb {
    /// Codeword received exactly or with 1 corrected bit (SDRTrunk `isValid()`).
    pub valid: bool,
    pub color_code: u8,
    /// PI bit: privacy (encryption) in use.
    pub encrypted: bool,
    /// Link control start/stop, 2 bits (SDRTrunk `LCSS` ordinal).
    pub lcss: u8,
    /// Bits corrected, 0..=3.
    pub corrected: u32,
}

/// Decodes the 16 EMB bits (transmission order). Ports `EMB.checkCRC()` and its accessors.
pub fn decode(bits: &[u8]) -> Emb {
    let (word, corrected, valid) = check_crc(get_int(&bits[..16]) as u16);
    Emb {
        valid,
        color_code: (word >> 12) as u8,
        encrypted: (word >> 11) & 1 == 1,
        lcss: ((word >> 9) & 3) as u8,
        corrected,
    }
}

/// Decodes the EMB of a 288-bit voice burst (`VoiceEMBMessage.getEMB()`).
pub fn decode_burst(burst: &[u8]) -> Emb {
    let mut bits = [0u8; 16];
    for (x, bit) in bits.iter_mut().enumerate() {
        *bit = burst[EMB_INDEXES[x]];
    }
    decode(&bits)
}

/// (word, corrected bits, valid): the first codeword within 3 bits, or the word
/// unchanged and invalid. Ports `EMB.checkCRC()`.
pub fn check_crc(word: u16) -> (u16, u32, bool) {
    if VALID_WORDS[usize::from(word >> 9)] == word {
        return (word, 0, true);
    }
    for &valid_word in VALID_WORDS.iter() {
        let bit_errors = (word ^ valid_word).count_ones();
        if bit_errors <= 3 {
            return (valid_word, bit_errors, bit_errors <= 1);
        }
    }
    (word, 0, false)
}

#[cfg(test)]
#[path = "emb_tests.rs"]
mod tests;

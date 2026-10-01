//! Unit tests for `emb.rs`.

use super::*;
use crate::protocol::dmr::fec::set_int;

fn word_bits(word: u16) -> [u8; 16] {
    let mut bits = [0u8; 16];
    set_int(u32::from(word), &mut bits);
    bits
}

/// SDRTrunk `EMB.VALID_WORDS` as published.
const SDRTRUNK_VALID_WORDS: [u16; 128] = [
    0x0000, 0x0273, 0x04E5, 0x0696, 0x09C9, 0x0BBA, 0x0D2C, 0x0F5F, 0x11E2, 0x1391, 0x1507, 0x1774,
    0x182B, 0x1A58, 0x1CCE, 0x1EBD, 0x21B7, 0x23C4, 0x2552, 0x2721, 0x287E, 0x2A0D, 0x2C9B, 0x2EE8,
    0x3055, 0x3226, 0x34B0, 0x36C3, 0x399C, 0x3BEF, 0x3D79, 0x3F0A, 0x411E, 0x436D, 0x45FB, 0x4788,
    0x48D7, 0x4AA4, 0x4C32, 0x4E41, 0x50FC, 0x528F, 0x5419, 0x566A, 0x5935, 0x5B46, 0x5DD0, 0x5FA3,
    0x60A9, 0x62DA, 0x644C, 0x663F, 0x6960, 0x6B13, 0x6D85, 0x6FF6, 0x714B, 0x7338, 0x75AE, 0x77DD,
    0x7882, 0x7AF1, 0x7C67, 0x7E14, 0x802F, 0x825C, 0x84CA, 0x86B9, 0x89E6, 0x8B95, 0x8D03, 0x8F70,
    0x91CD, 0x93BE, 0x9528, 0x975B, 0x9804, 0x9A77, 0x9CE1, 0x9E92, 0xA198, 0xA3EB, 0xA57D, 0xA70E,
    0xA851, 0xAA22, 0xACB4, 0xAEC7, 0xB07A, 0xB209, 0xB49F, 0xB6EC, 0xB9B3, 0xBBC0, 0xBD56, 0xBF25,
    0xC131, 0xC342, 0xC5D4, 0xC7A7, 0xC8F8, 0xCA8B, 0xCC1D, 0xCE6E, 0xD0D3, 0xD2A0, 0xD436, 0xD645,
    0xD91A, 0xDB69, 0xDDFF, 0xDF8C, 0xE086, 0xE2F5, 0xE463, 0xE610, 0xE94F, 0xEB3C, 0xEDAA, 0xEFD9,
    0xF164, 0xF317, 0xF581, 0xF7F2, 0xF8AD, 0xFADE, 0xFC48, 0xFE3B,
];

#[test]
fn table_matches_sdrtrunk_except_its_upper_half() {
    for i in 0..128 {
        let fix = if i >= 64 { 0x0060 } else { 0 };
        assert_eq!(VALID_WORDS[i], SDRTRUNK_VALID_WORDS[i] ^ fix, "index {i}");
    }
    // SDRTrunk's index 76 has weight 4: a distance-4 code.
    assert_eq!(SDRTRUNK_VALID_WORDS[76].count_ones(), 4);
}

#[test]
fn table_is_a_systematic_linear_code_with_distance_6() {
    for (data, &w) in VALID_WORDS.iter().enumerate() {
        assert_eq!(usize::from(w >> 9), data);
    }
    for &a in VALID_WORDS.iter() {
        for &b in VALID_WORDS.iter() {
            let sum = a ^ b;
            assert_eq!(VALID_WORDS[usize::from(sum >> 9)], sum, "closed under XOR");
            if a != b {
                assert!(sum.count_ones() >= 6);
            }
        }
    }
}

#[test]
fn fields_round_trip_from_burst() {
    for cc in 0..16u16 {
        for pi in 0..2u16 {
            for lcss in 0..4u16 {
                let w = VALID_WORDS[usize::from((cc << 3) | (pi << 2) | lcss)];
                let mut burst = [0u8; 288];
                for (x, &b) in word_bits(w).iter().enumerate() {
                    burst[EMB_INDEXES[x]] = b;
                }
                let e = decode_burst(&burst);
                assert_eq!(
                    e,
                    Emb {
                        valid: true,
                        color_code: cc as u8,
                        encrypted: pi == 1,
                        lcss: lcss as u8,
                        corrected: 0
                    }
                );
            }
        }
    }
}

#[test]
fn one_and_two_bit_errors_corrected_one_bit_valid() {
    for &w in VALID_WORDS.iter() {
        let expect = decode(&word_bits(w));
        for a in 0..16 {
            for b in a..16 {
                let mut rx = w ^ (0x8000 >> a);
                if b != a {
                    rx ^= 0x8000 >> b;
                }
                let n = if a == b { 1 } else { 2 };
                let e = decode(&word_bits(rx));
                assert_eq!(
                    (e.color_code, e.encrypted, e.lcss),
                    (expect.color_code, expect.encrypted, expect.lcss)
                );
                assert_eq!(e.corrected, n);
                assert_eq!(e.valid, n <= 1);
            }
        }
    }
}

#[test]
fn three_bit_errors_never_valid() {
    let w = VALID_WORDS[0x2B];
    for a in 0..16 {
        for b in a + 1..16 {
            for c in b + 1..16 {
                let rx = w ^ (0x8000 >> a) ^ (0x8000 >> b) ^ (0x8000 >> c);
                assert!(!decode(&word_bits(rx)).valid);
            }
        }
    }
}

//! Unit tests for `golay24.rs`.

use super::*;
use crate::protocol::fec::set_int;

/// Encodes 12 data bits into a 24-bit extended Golay word.
fn encode(data: u32) -> [u8; 24] {
    let mut w = [0u8; 24];
    set_int(data, &mut w[..12]);
    let c = calculate_checksum(&w, 0);
    set_int(c, &mut w[12..23]);
    w[23] = w[..23].iter().fold(0, |acc, &b| acc ^ b);
    w
}

#[test]
fn syndrome_table_is_complete() {
    // Golay(23,12) is perfect: the 2048 patterns of weight <= 3 fill every syndrome.
    let table = syndrome_table();
    assert!(table.iter().all(|&p| p != u32::MAX && p.count_ones() <= 3));
}

#[test]
fn checksum_table_matches_generator_0xc75() {
    // Data bit i alone: remainder of x^(22-i) mod g(x) = x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1.
    for i in 0..12 {
        let mut reg: u32 = 1 << (22 - i);
        for bit in (11..23).rev() {
            if reg & (1 << bit) != 0 {
                reg ^= 0xC75 << (bit - 11);
            }
        }
        assert_eq!(reg, CHECKSUMS[i], "position {i}");
    }
}

#[test]
fn up_to_three_errors_corrected() {
    for data in (0..4096u32).step_by(97) {
        let cw = encode(data);
        let mut w = cw;
        assert_eq!(check_and_correct(&mut w, 0), Some(0));
        for a in 0..24 {
            for b in a..24 {
                for c in b..24 {
                    let mut rx = cw;
                    rx[a] ^= 1;
                    let mut n = 1;
                    if b != a {
                        rx[b] ^= 1;
                        n += 1;
                    }
                    if c != b {
                        rx[c] ^= 1;
                        n += 1;
                    }
                    assert_eq!(
                        check_and_correct(&mut rx, 0),
                        Some(n),
                        "{data:#x} {a} {b} {c}"
                    );
                    assert_eq!(rx, cw);
                }
            }
        }
    }
}

#[test]
fn four_errors_detected_and_word_unchanged() {
    for data in [0x000u32, 0x5A5, 0xFFF, 0x123] {
        let cw = encode(data);
        for a in 0..24 {
            for b in a + 1..24 {
                for c in b + 1..24 {
                    for d in c + 1..24 {
                        let mut rx = cw;
                        for p in [a, b, c, d] {
                            rx[p] ^= 1;
                        }
                        let before = rx;
                        assert_eq!(check_and_correct(&mut rx, 0), None);
                        assert_eq!(rx, before);
                    }
                }
            }
        }
    }
}

#[test]
fn works_at_an_offset() {
    let mut m = vec![1u8; 5];
    m.extend_from_slice(&encode(0xABC));
    m.extend_from_slice(&[1, 1, 1]);
    m[5 + 3] ^= 1;
    m[5 + 20] ^= 1;
    assert_eq!(check_and_correct(&mut m, 5), Some(2));
    assert_eq!(&m[5..29], &encode(0xABC));
    assert_eq!(&m[..5], &[1, 1, 1, 1, 1]);
}

/// The P25 voice parsers' earlier decoder: search the weight 1, 2 and 3 patterns in
/// order for the syndrome; the parity bit is ignored.
fn search23(word: &mut [u8; 24]) {
    let syndrome = get_syndrome(word, 0);
    if syndrome == 0 {
        return;
    }
    for a in 0..23 {
        if CHECKSUMS[a] == syndrome {
            word[a] ^= 1;
            return;
        }
    }
    for a in 0..23 {
        for b in a + 1..23 {
            if CHECKSUMS[a] ^ CHECKSUMS[b] == syndrome {
                word[a] ^= 1;
                word[b] ^= 1;
                return;
            }
        }
    }
    for a in 0..23 {
        for b in a + 1..23 {
            for c in b + 1..23 {
                if CHECKSUMS[a] ^ CHECKSUMS[b] ^ CHECKSUMS[c] == syndrome {
                    word[a] ^= 1;
                    word[b] ^= 1;
                    word[c] ^= 1;
                    return;
                }
            }
        }
    }
}

#[test]
fn correct23_matches_the_pattern_search_on_every_syndrome() {
    // Every 23-bit word is a codeword plus one pattern; one word per syndrome, with
    // both parity values and a few data values, covers every case.
    let mut state = 0x2545_F491u32;
    for _ in 0..20_000 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let mut w = [0u8; 24];
        set_int(state & 0xFF_FFFF, &mut w);
        let mut reference = w;
        search23(&mut reference);
        let mut ours = w;
        let n = correct23(&mut ours, 0);
        assert_eq!(ours, reference, "{:06x}", state & 0xFF_FFFF);
        assert_eq!(n, w.iter().zip(&ours).filter(|(a, b)| a != b).count() as u32);
    }
}

#[test]
fn correct18_decodes_the_shortened_code() {
    for data in 0..64u32 {
        let mut w = [0u8; 24];
        set_int(data, &mut w[6..12]);
        let c = calculate_checksum(&w, 0);
        set_int(c, &mut w[12..23]);
        let clean: [u8; 18] = w[6..].try_into().unwrap();
        for (a, b) in [(0, 5), (3, 16), (7, 7)] {
            let mut rx = clean;
            rx[a] ^= 1;
            rx[b] ^= 1;
            let expected = if a == b { 0 } else { 2 };
            assert_eq!(correct18(&mut rx, 0), expected);
            assert_eq!(rx, clean);
        }
    }
}

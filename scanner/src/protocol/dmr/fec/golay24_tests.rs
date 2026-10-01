//! Unit tests for `golay24.rs`.

use super::*;
use crate::protocol::dmr::fec::set_int;

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

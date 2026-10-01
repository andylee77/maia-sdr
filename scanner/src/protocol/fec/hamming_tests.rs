//! Unit tests for `hamming.rs`.

use super::*;
use crate::protocol::dmr::fec::set_int;

/// Codeword of `n` bits: `k` data bits from `data`, parity from `parity_fn`.
fn word(n: usize, k: usize, data: u32, parity_fn: fn(&[u8], usize) -> u32) -> Vec<u8> {
    let mut w = vec![0u8; n];
    set_int(data, &mut w[..k]);
    let p = parity_fn(&w, 0);
    set_int(p, &mut w[k..]);
    w
}

#[test]
fn error_index_tables_match_checksums() {
    for (p, &c) in Hamming13::CHECKSUMS.iter().enumerate() {
        assert_eq!(Hamming13::ERROR_INDEX[c as usize], p as i8);
    }
    for (p, &c) in Hamming15::CHECKSUMS.iter().enumerate() {
        assert_eq!(Hamming15::ERROR_INDEX[c as usize], p as u8);
    }
}

#[test]
fn hamming15_all_codewords_and_single_errors() {
    for data in 0..(1u32 << 11) {
        let mut w = vec![0u8; 3];
        w.extend(word(15, 11, data, Hamming15::calculate_checksum));
        assert_eq!(Hamming15::get_error_index(&w, 3), ErrorIndex::NoErrors);
        for p in 0..15 {
            w[3 + p] ^= 1;
            assert_eq!(Hamming15::get_error_index(&w, 3), ErrorIndex::At(3 + p));
            w[3 + p] ^= 1;
        }
    }
}

#[test]
fn hamming13_indices_single_and_double_errors() {
    // Word scattered through a larger message, like a BPTC(196,96) column.
    let indices: [usize; 13] = core::array::from_fn(|r| 5 + 15 * r);
    for data in 0..(1u32 << 9) {
        let mut m = vec![0u8; 200];
        for x in 0..9 {
            m[indices[x]] = ((data >> (8 - x)) & 1) as u8;
        }
        let p = Hamming13::calculate_checksum(&m, &indices);
        for x in 0..4 {
            m[indices[9 + x]] = ((p >> (3 - x)) & 1) as u8;
        }
        assert_eq!(
            Hamming13::get_error_index(&m, &indices),
            ErrorIndex::NoErrors
        );
        for a in 0..13 {
            m[indices[a]] ^= 1;
            assert_eq!(
                Hamming13::get_error_index(&m, &indices),
                ErrorIndex::At(indices[a])
            );
            for b in a + 1..13 {
                m[indices[b]] ^= 1;
                // d = 3: a double error is never "no errors".
                assert_ne!(
                    Hamming13::get_error_index(&m, &indices),
                    ErrorIndex::NoErrors
                );
                m[indices[b]] ^= 1;
            }
            m[indices[a]] ^= 1;
        }
    }
}

#[test]
fn hamming16_secded() {
    let h = Hamming16;
    for data in 0..(1u32 << 11) {
        let mut w = word(16, 11, data, Hamming16::calculate_checksum);
        assert_eq!(
            w.iter().filter(|&&b| b == 1).count() % 2,
            0,
            "even-weight code"
        );
        assert_eq!(h.get_error_index(&w, 0), ErrorIndex::NoErrors);
        for a in 0..16 {
            w[a] ^= 1;
            assert_eq!(
                h.get_error_index(&w, 0),
                ErrorIndex::At(a),
                "data {data:#x} bit {a}"
            );
            for b in a + 1..16 {
                w[b] ^= 1;
                assert_eq!(h.get_error_index(&w, 0), ErrorIndex::MultipleErrors);
                w[b] ^= 1;
            }
            w[a] ^= 1;
        }
    }
}

#[test]
fn hamming17_single_errors() {
    let h = Hamming17;
    for data in 0..(1u32 << 12) {
        let mut w = vec![1u8; 2];
        w.extend(word(17, 12, data, Hamming17::calculate_checksum));
        assert_eq!(h.get_error_index(&w, 2), ErrorIndex::NoErrors);
        for a in 0..17 {
            w[2 + a] ^= 1;
            assert_eq!(h.get_error_index(&w, 2), ErrorIndex::At(2 + a));
            w[2 + a] ^= 1;
        }
    }
}

/// The P25 voice parsers' earlier Hamming(10,6,3) decoder, on bools.
fn hamming10_bools(cw: &mut [bool; 10]) -> Option<u32> {
    let mut syndrome = 0u8;
    for i in 0..6 {
        if cw[i] {
            syndrome ^= Hamming10::CHECKSUMS[i] as u8;
        }
    }
    for i in 0..4 {
        if cw[6 + i] {
            syndrome ^= 1 << (3 - i);
        }
    }
    let flip = match syndrome {
        0 => return Some(0),
        1 => 9,
        2 => 8,
        3 => 4,
        4 => 7,
        7 => 3,
        8 => 6,
        11 => 2,
        12 => 5,
        13 => 1,
        14 => 0,
        _ => return None,
    };
    cw[flip] ^= true;
    Some(1)
}

#[test]
fn hamming10_matches_the_earlier_decoder_on_every_word() {
    for v in 0..1024u32 {
        let mut bits = [0u8; 10];
        crate::protocol::fec::set_int(v, &mut bits);
        let mut bools = bits.map(|b| b == 1);
        let expected = hamming10_bools(&mut bools);
        assert_eq!(Hamming10::check_and_correct(&mut bits, 0), expected, "{v:010b}");
        assert_eq!(bits.map(|b| b == 1), bools, "{v:010b}");
    }
}

#[test]
fn hamming10_corrects_every_single_error() {
    for data in 0..64u32 {
        let mut w = [0u8; 10];
        crate::protocol::fec::set_int(data, &mut w[..6]);
        let parity = calculate(&w, 0, 6, &Hamming10::CHECKSUMS);
        crate::protocol::fec::set_int(parity, &mut w[6..]);
        assert_eq!(Hamming10::get_syndrome(&w, 0), 0);
        for e in 0..10 {
            let mut rx = w;
            rx[e] ^= 1;
            assert_eq!(Hamming10::check_and_correct(&mut rx, 0), Some(1), "{data} {e}");
            assert_eq!(rx, w);
        }
    }
}

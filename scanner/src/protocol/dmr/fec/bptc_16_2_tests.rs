//! Unit tests for `bptc_16_2.rs`.

use super::*;
use crate::protocol::dmr::fec::set_int;

/// Transmitted 32 bits for 11 data bits (Hamming row repeated).
fn encode(data: u32) -> [u8; 32] {
    let mut d = [0u8; 32];
    set_int(data, &mut d[..11]);
    let p = Hamming16::calculate_checksum(&d, 0);
    set_int(p, &mut d[11..16]);
    let (first, second) = d.split_at_mut(16);
    second.copy_from_slice(first);
    let mut tx = [0u8; 32];
    for x in 0..32 {
        tx[x] = d[DEINTERLEAVE[x]];
    }
    tx
}

#[test]
fn deinterleave_is_a_permutation() {
    let mut seen = [false; 32];
    for &i in DEINTERLEAVE.iter() {
        seen[i] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn round_trip_and_null_burst() {
    for data in [0u32, 1, 0x2AA, 0x7FF] {
        let tx = encode(data);
        let d = decode_short_burst(&tx).expect("valid");
        assert_eq!(crate::protocol::dmr::fec::get_int(&d[..11]), data);
    }
}

#[test]
fn errors_in_the_hamming_row_are_corrected_others_rejected() {
    let tx = encode(0x155);
    let clean = decode_short_burst(&tx).unwrap();
    for x in 0..32 {
        let mut rx = tx;
        rx[x] ^= 1;
        let got = decode_short_burst(&rx);
        if DEINTERLEAVE[x] < 16 {
            // The Hamming fix restores row 1, which then matches row 2.
            assert_eq!(got, Some(clean), "bit {x}");
        } else {
            assert_eq!(got, None, "bit {x}");
        }
    }
}

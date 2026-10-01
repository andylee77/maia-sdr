//! Unit tests for `cach.rs` (attached with `#[path]`, so `super::*` sees private items).

use super::*;
use crate::protocol::dmr::fec::set_int;

/// Builds a transmitted 24-bit CACH from AT, TC, LCSS and a 17-bit payload.
fn encode(at: u8, tc: u8, lcss: u8, payload: &[u8; 17]) -> [u8; 24] {
    let mut m = [0u8; 24];
    m[0] = at;
    m[1] = tc;
    set_int(u32::from(lcss), &mut m[2..4]);
    let mut parity = 0;
    for (x, p) in CHECKSUMS.iter().enumerate() {
        if m[x] == 1 {
            parity ^= p;
        }
    }
    set_int(parity, &mut m[4..7]);
    m[7..24].copy_from_slice(payload);

    let mut tx = [0u8; 24];
    for x in 0..24 {
        tx[INTERLEAVE_MATRIX[x]] = m[x];
    }
    tx
}

fn payload_pattern(seed: u32) -> [u8; 17] {
    let mut p = [0u8; 17];
    set_int(seed.wrapping_mul(0x9E37) & 0x1FFFF, &mut p);
    p
}

#[test]
fn interleave_matrix_is_a_permutation() {
    let mut seen = [false; 24];
    for &i in INTERLEAVE_MATRIX.iter() {
        assert!(!seen[i]);
        seen[i] = true;
    }
}

#[test]
fn round_trip_all_tact_values() {
    for at in 0..2u8 {
        for tc in 0..2u8 {
            for lcss in 1..4u8 {
                let payload = payload_pattern(u32::from(at * 8 + tc * 4 + lcss));
                let tx = encode(at, tc, lcss, &payload);
                assert_eq!(get_crc_checksum(&deinterleave(&tx)), 0);
                let c = decode(&tx);
                assert!(c.valid);
                assert_eq!(c.busy, at == 1);
                assert_eq!(c.timeslot, tc + 1);
                assert_eq!(c.lcss, lcss);
                assert_eq!(c.payload, payload);
            }
        }
    }
}

#[test]
fn decode_reads_only_first_24_bits_of_a_burst() {
    let payload = payload_pattern(7);
    let mut burst = [1u8; 288];
    burst[..24].copy_from_slice(&encode(1, 0, 2, &payload));
    let c = decode(&burst);
    assert!(c.valid);
    assert!(c.busy);
    assert_eq!(c.timeslot, 1);
    assert_eq!(c.lcss, 2);
    assert_eq!(c.payload, payload);
}

#[test]
fn single_tact_bit_error_corrected() {
    for lcss in 1..4u8 {
        for at in 0..2u8 {
            for tc in 0..2u8 {
                let tx = encode(at, tc, lcss, &payload_pattern(3));
                // Decoded TACT bit x is transmitted bit INTERLEAVE_MATRIX[x].
                for x in 0..7 {
                    let mut rx = tx;
                    rx[INTERLEAVE_MATRIX[x]] ^= 1;
                    let c = decode(&rx);
                    // A flip into an LCSS=00 codeword's sphere is "uncorrectable"
                    // by SDRTrunk's table; every other single error is fixed.
                    if c.valid {
                        assert_eq!((c.busy, c.timeslot, c.lcss), (at == 1, tc + 1, lcss));
                    } else {
                        assert!(x == 2 || x == 3, "lcss {lcss} at {at} tc {tc} bit {x}");
                    }
                }
            }
        }
    }
}

#[test]
fn lcss_zero_codewords_are_invalid() {
    // SDRTrunk treats LCSS 00 (single fragment) as an invalid CACH.
    let c = decode(&encode(0, 1, 0, &payload_pattern(1)));
    assert!(!c.valid);
}

#[test]
fn table_matches_generator() {
    // Rebuild SDRTrunk's createHamming7_4BitErrorMap() and compare.
    let mut table = [-2i8; 128];
    for x in 0..16u32 {
        let mut word = x << 3;
        for (i, p) in CHECKSUMS.iter().enumerate() {
            if (x >> (3 - i)) & 1 == 1 {
                word ^= p;
            }
        }
        if word & 0x18 == 0 {
            continue;
        }
        table[word as usize] = -1;
        for bit in 0..7 {
            let e = word ^ (1 << (6 - bit));
            if e & 0x18 != 0 {
                table[e as usize] = bit as i8;
            }
        }
    }
    assert_eq!(table, BIT_ERROR_INDEXES);
}

#[test]
fn payload_errors_are_not_touched() {
    // Payload bits are protected by BPTC(68,36) later, not here.
    let payload = payload_pattern(11);
    let mut rx = encode(0, 1, 3, &payload);
    rx[INTERLEAVE_MATRIX[10]] ^= 1;
    let c = decode(&rx);
    assert!(c.valid);
    let mut expect = payload;
    expect[3] ^= 1;
    assert_eq!(c.payload, expect);
}

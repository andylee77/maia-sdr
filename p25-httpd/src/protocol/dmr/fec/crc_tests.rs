//! Unit tests for `crc.rs`.

use super::*;
use crate::protocol::dmr::fec::test_util::{hex_to_bits, XorShift};
use crate::protocol::dmr::fec::{get_int, set_int};

/// CSBKs logged by SDRTrunk on Clay Electric (454.36875 MHz, 2026-03-16), all PASSED.
const ON_AIR_CSBKS: [&str; 4] = [
    "99001101F6400B000000DEDA", // ALOHA
    "990011C1F6400B000000A8CF", // ALOHA
    "AE000000FFFED4FFFECAFF8F", // P_CLEAR
    "9C58300C014227FFFEF3AA10",
];

/// Appends the ETSI CRC (complemented, masked) to 80 data bits.
fn with_crc(data: &[u8], mask: u16) -> Vec<u8> {
    let mut m = data[..80].to_vec();
    let mut crc = [0u8; 16];
    set_int(u32::from(!crc_ccitt(data) ^ mask), &mut crc);
    m.extend_from_slice(&crc);
    m
}

#[test]
fn table_is_single_bit_remainders() {
    for i in 0..80 {
        let mut m = [0u8; 80];
        m[i] = 1;
        assert_eq!(crc_ccitt(&m), CCITT_80_CHECKSUMS[i], "bit {i}");
    }
    for k in 0..16 {
        assert_eq!(CCITT_80_CHECKSUMS[80 + k], 0x8000 >> k);
    }
}

#[test]
fn on_air_csbk_crcs_pass_with_mask_a5a5() {
    for hex in ON_AIR_CSBKS {
        let bits = hex_to_bits(hex);
        assert_eq!(calculate_residual(&bits, CSBK_CRC_MASK), 0, "{hex}");
        // With mask 0 the residual is the mask in use.
        assert_eq!(calculate_residual(&bits, 0), CSBK_CRC_MASK, "{hex}");
        let mut m = bits.clone();
        assert_eq!(correct_ccitt80(&mut m, CSBK_CRC_MASK), Some(0));
        assert_eq!(correct_ccitt80(&mut m, DATA_HEADER_CRC_MASK), None);
    }
    // Spelled out for the first one: CRC = !remainder ^ mask.
    let bits = hex_to_bits(ON_AIR_CSBKS[0]);
    assert_eq!(crc_ccitt(&bits[..80]), 0x8480);
    assert_eq!(!0x8480u16 ^ 0xA5A5, 0xDEDA);
}

#[test]
fn single_bit_errors_corrected_everywhere() {
    let original = hex_to_bits(ON_AIR_CSBKS[2]);
    for p in 0..96 {
        let mut m = original.clone();
        m[p] ^= 1;
        assert_eq!(correct_ccitt80(&mut m, CSBK_CRC_MASK), Some(1), "bit {p}");
        assert_eq!(m, original);
    }
}

#[test]
fn double_bit_errors_detected_not_miscorrected() {
    let mut rng = XorShift::new(0xC5B1);
    let mut data = [0u8; 80];
    rng.fill_bits(&mut data);
    let original = with_crc(&data, MBC_HEADER_CRC_MASK);
    for a in 0..96 {
        for b in a + 1..96 {
            let mut m = original.clone();
            m[a] ^= 1;
            m[b] ^= 1;
            assert_eq!(
                correct_ccitt80(&mut m, MBC_HEADER_CRC_MASK),
                None,
                "{a} {b}"
            );
        }
    }
}

#[test]
fn every_mask_round_trips() {
    let mut rng = XorShift::new(7);
    for mask in [
        PI_HEADER_CRC_MASK,
        CSBK_CRC_MASK,
        MBC_HEADER_CRC_MASK,
        MBC_LAST_BLOCK_CRC_MASK,
        DATA_HEADER_CRC_MASK,
        USB_DATA_CRC_MASK,
    ] {
        let mut data = [0u8; 80];
        rng.fill_bits(&mut data);
        let m = with_crc(&data, mask);
        assert_eq!(calculate_residual(&m, mask), 0);
        assert_eq!(calculate_residual(&m, 0), mask);
    }
}

#[test]
fn on_air_short_lc_crc8() {
    // Tier III SLCs logged by SDRTrunk (36 bits: 28 data + CRC-8), all PASSED.
    for hex in [
        "2400BE39A",
        "2400BE48F",
        "2400A6306",
        "34009681E",
        "340097B67",
    ] {
        let bits = hex_to_bits(hex);
        assert_eq!(crc8(&bits), 0, "{hex}");
        assert_eq!(get_int(&bits[28..]) as u8, crc8(&bits[..28]));
        for p in 0..36 {
            let mut m = bits.clone();
            m[p] ^= 1;
            assert_ne!(crc8(&m), 0);
        }
    }
}

#[test]
fn checksum_5_round_trip() {
    let mut rng = XorShift::new(55);
    for _ in 0..200 {
        let mut m = [0u8; 77];
        rng.fill_bits(&mut m[..72]);
        let sum: u32 = (0..9).map(|i| get_int(&m[i * 8..i * 8 + 8])).sum();
        set_int(sum % 31, &mut m[72..]);
        assert_eq!(checksum_5(&m), 0);
        m[3] ^= 1;
        assert_ne!(checksum_5(&m), 0);
    }
}

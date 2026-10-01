//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;
use crate::protocol::p25::test_fixtures::*;

/// The `raw_duid` field is the pre-BCH 4-bit DUID straight off the
/// wire (u64 bits 51..48). `control_channel.rs::process_dibit`
/// computes it inline alongside the BCH decode for the diagnostic
/// histogram; tests below reproduce that shape.
fn raw_duid_of(nid_bits: u64) -> u8 {
    ((nid_bits >> 48) & 0xF) as u8
}

#[test]
fn test_nid_decode_clean_clay_county() {
    // Clay County NAC 0x8A1 / DUID 0x7 (TSDU). Encode via the
    // validated lsm::nid_fec encoder so we get a real BCH codeword
    // with the right 48 parity bits, then verify decode_nid round-
    // trips it cleanly.
    let nid_bits = crate::protocol::p25::fec::bch::encode_nid(CLAY_NAC, 0x7);
    let decoded = crate::protocol::p25::fec::bch::decode_nid(nid_bits).unwrap();
    assert_eq!(decoded.nac, CLAY_NAC);
    assert_eq!(decoded.duid, 0x7);
    // For a clean codeword raw matches BCH-corrected.
    assert_eq!(raw_duid_of(nid_bits), 0x7);
}

#[test]
fn test_nid_decode_corrects_11_bit_errors() {
    // The BCH(63,16,11) code has minimum distance 23 and corrects up
    // to t = 11 bit errors. Flip 11 bits in a clean Clay County
    // codeword and verify the decoder still recovers the original
    // NAC/DUID, matching the existing
    // lsm::nid_fec::error_correction_sweep_up_to_t11 test.
    let clean = crate::protocol::p25::fec::bch::encode_nid(CLAY_NAC, 0x7);
    // 11 fixed bit positions from the parity field (avoid bit 63
    // which is the SDRTrunk-test convention).
    let positions = [0u32, 5, 9, 14, 20, 27, 33, 40, 46, 51, 58];
    let mut corrupted = clean;
    for &p in &positions {
        corrupted ^= 1u64 << (63 - p);
    }
    let decoded = crate::protocol::p25::fec::bch::decode_nid(corrupted).unwrap();
    assert_eq!(decoded.nac, CLAY_NAC);
    assert_eq!(decoded.duid, 0x7);
}

#[test]
fn test_nid_decode_rejects_uncorrectable() {
    // 20 bit errors is well outside the t=11 unique-decoding sphere.
    // The decoder may either return None (uncorrectable) or land on
    // a different valid codeword that happens to be closer; in
    // EITHER case, test_nid_decode must NOT silently return the
    // original (NAC, DUID) pair, because that would be undetectably
    // wrong. The strict assertion is "either None, or a different
    // (NAC, DUID)".
    let clean = crate::protocol::p25::fec::bch::encode_nid(CLAY_NAC, 0x7);
    // Flip the first 20 bits.
    let mut corrupted = clean;
    for p in 0u32..20 {
        corrupted ^= 1u64 << (63 - p);
    }
    match crate::protocol::p25::fec::bch::decode_nid(corrupted) {
        None => {} // ok -- uncorrectable
        Some(d) => {
            assert_ne!(
                (d.nac, d.duid),
                (CLAY_NAC, 0x7),
                "20-bit-error word silently decoded as the original NAC/DUID -- BCH bypass?"
            );
        }
    }
}

#[test]
fn test_nid_decode_records_raw_duid_under_corruption() {
    // Encode a real codeword for NAC=0xE28 / DUID=0x7, then flip a
    // few bits in the DUID nibble (positions 12..15 of the on-wire
    // bit stream = bits 51..48 of the u64). The BCH FEC should
    // correct the DUID back to 0x7, but the raw_duid that we
    // expose for the diagnostic histogram should still be the
    // PRE-correction value (so the histogram measures slicer noise,
    // not BCH-corrected output).
    let clean = crate::protocol::p25::fec::bch::encode_nid(0xE28, 0x7);
    // Flip the DUID LSB (on-wire bit 15 = u64 bit 48). 1 bit error
    // is well within the t=11 correction sphere.
    let corrupted = clean ^ (1u64 << 48);
    let decoded = crate::protocol::p25::fec::bch::decode_nid(corrupted).unwrap();
    assert_eq!(decoded.nac, 0xE28);
    assert_eq!(decoded.duid, 0x7); // BCH-corrected
    assert_eq!(raw_duid_of(corrupted), 0x6); // the un-FEC'd LSB-flipped DUID
}

#[test]
fn test_trellis_decode_clean_roundtrip() {
    // Build a 48-symbol input pattern and round-trip it through
    // encode + decode. The new Viterbi (P25 1/2 rate) should
    // recover the original 12 bytes exactly with zero corrected
    // errors.
    let mut inputs = [0u8; 48];
    for i in 0..48 {
        inputs[i] = ((i * 7 + 1) % 4) as u8;
    }
    let dibits = trellis_encode_block(&inputs);
    let bytes = TrellisDecoder::decode(&dibits).expect("decode should succeed");

    // Re-pack expected bytes from inputs (48 × 2 bits = 96 bits =
    // 12 bytes), MSB first within each byte.
    let mut expected = [0u8; 12];
    let mut bit_buf = [0u8; 96];
    for n in 0..48 {
        let two = inputs[n] & 0x03;
        bit_buf[n * 2] = (two >> 1) & 0x01;
        bit_buf[n * 2 + 1] = two & 0x01;
    }
    for byte_idx in 0..12 {
        let mut b = 0u8;
        for k in 0..8 {
            b |= bit_buf[byte_idx * 8 + k] << (7 - k);
        }
        expected[byte_idx] = b;
    }

    assert_eq!(
        bytes, expected,
        "trellis decode of clean encoded input must round-trip exactly"
    );
}

#[test]
fn test_trellis_decode_corrects_single_dibit_error() {
    // Same setup as round-trip, but flip 1 bit in the encoded
    // stream. The Viterbi should still recover the original.
    let mut inputs = [0u8; 48];
    for i in 0..48 {
        inputs[i] = ((i * 11 + 2) % 4) as u8;
    }
    let mut dibits = trellis_encode_block(&inputs);
    // Flip the LSB of dibit 30 (somewhere in the middle).
    dibits[30] ^= 0x01;

    let bytes = TrellisDecoder::decode(&dibits).expect("decode should succeed");

    let mut expected = [0u8; 12];
    let mut bit_buf = [0u8; 96];
    for n in 0..48 {
        let two = inputs[n] & 0x03;
        bit_buf[n * 2] = (two >> 1) & 0x01;
        bit_buf[n * 2 + 1] = two & 0x01;
    }
    for byte_idx in 0..12 {
        let mut b = 0u8;
        for k in 0..8 {
            b |= bit_buf[byte_idx * 8 + k] << (7 - k);
        }
        expected[byte_idx] = b;
    }
    assert_eq!(bytes, expected, "Viterbi should correct a single bit error");
}

#[test]
fn test_tsdu_deinterleave_removes_status_and_nulls() {
    // Build a 123-dibit on-air TSBK1 body:
    //   - 98 trellis data dibits (marker 0x01) interleaved with
    //   - 4 status dibits (marker 0xFE) at body positions 13,49,85,121
    //   - followed by 21 trailing null dibits (marker 0xFD)
    // Net: 98 surviving 0x01 dibits after deinterleave.
    let mut tsdu = vec![0u8; 123];
    // Default fill: data marker.
    for d in tsdu.iter_mut() {
        *d = 0x01;
    }
    // Status markers at the 4 body positions.
    for p in [14usize, 50, 86, 122] {
        tsdu[p] = 0xFE;
    }
    // Null padding: the LAST 21 non-status positions get 0xFD.
    let mut non_status_positions: Vec<usize> = (0..123)
        .filter(|i| !matches!(*i, 14 | 50 | 86 | 122))
        .collect();
    let null_positions = non_status_positions.split_off(non_status_positions.len() - 21);
    for p in &null_positions {
        tsdu[*p] = 0xFD;
    }

    let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 1);
    assert_eq!(
        data.len(),
        98,
        "deinterleaver must return exactly 98 trellis data dibits \
         (123 raw - 4 status - 21 null)"
    );
    for (i, &d) in data.iter().enumerate() {
        assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
        assert_ne!(d, 0xFD, "null marker survived at output[{}]", i);
        assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
    }
}

/// **Phase 6F.3 multi-block TSBK regression guard.** Build a
/// 231-dibit on-air TSBK1+TSBK2 body with the 7 expected status
/// drops at {13,49,85,121,157,193,229} and 28 trailing null padding
/// dibits, and verify the deinterleaver returns exactly 196 trellis
/// data dibits (= two contiguous 98-dibit blocks) with all status
/// and null markers stripped.
#[test]
fn test_tsdu_deinterleave_two_blocks() {
    let mut tsdu = vec![0u8; 231];
    for d in tsdu.iter_mut() {
        *d = 0x01;
    }
    for &p in &[14usize, 50, 86, 122, 158, 194, 230] {
        tsdu[p] = 0xFE;
    }
    // Mark the LAST 28 non-status positions as null.
    let mut non_status_positions: Vec<usize> = (0..231)
        .filter(|i| !matches!(*i, 14 | 50 | 86 | 122 | 158 | 194 | 230))
        .collect();
    let null_positions =
        non_status_positions.split_off(non_status_positions.len() - 28);
    for p in &null_positions {
        tsdu[*p] = 0xFD;
    }

    let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 2);
    assert_eq!(
        data.len(),
        196,
        "two-block deinterleaver must return 196 trellis dibits \
         (231 raw - 7 status - 28 null)"
    );
    for (i, &d) in data.iter().enumerate() {
        assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
        assert_ne!(d, 0xFD, "null marker survived at output[{}]", i);
        assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
    }
}

/// **Phase 6F.3 multi-block TSBK3 regression guard.** TSBK3 has 9
/// status drops at {13,49,85,121,157,193,229,265,301} and ZERO
/// trailing null padding. Body length 303 raw → 294 trellis dibits
/// (= 3 contiguous 98-dibit blocks).
#[test]
fn test_tsdu_deinterleave_three_blocks() {
    let mut tsdu = vec![0u8; 303];
    for d in tsdu.iter_mut() {
        *d = 0x01;
    }
    for &p in &[14usize, 50, 86, 122, 158, 194, 230, 266, 302] {
        tsdu[p] = 0xFE;
    }

    let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 3);
    assert_eq!(
        data.len(),
        294,
        "three-block deinterleaver must return 294 trellis dibits \
         (303 raw - 9 status - 0 null)"
    );
    for (i, &d) in data.iter().enumerate() {
        assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
        assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
    }
}

#[test]
fn test_body_dibits_for_blocks_table() {
    assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(1), Some(123));
    assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(2), Some(231));
    assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(3), Some(303));
    assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(0), None);
    assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(4), None);
}

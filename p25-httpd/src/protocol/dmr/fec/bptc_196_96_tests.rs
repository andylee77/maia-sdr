//! Unit tests for `bptc_196_96.rs`.

use super::*;
use crate::protocol::dmr::fec::crc::{calculate_residual, CSBK_CRC_MASK};
use crate::protocol::dmr::fec::set_int;
use crate::protocol::dmr::fec::test_util::{hex_to_bits, str_to_bits, XorShift};

/// Deinterleaved block for 96 info bits (reserved bits zero).
fn block(info: &[u8]) -> [u8; BPTC_LENGTH] {
    let mut m = [0u8; BPTC_LENGTH];
    let mut pointer = 0;
    let mut index = MESSAGE_START_INDEX;
    while index < MAX_ORIGINAL_INDEX {
        if index % COLUMN_COUNT < MESSAGE_COLUMN_COUNT {
            m[index] = info[pointer];
            pointer += 1;
            index += 1;
        } else {
            index += CHECKSUM_COLUMN_COUNT;
        }
    }
    for row in 0..9 {
        let offset = row * COLUMN_COUNT + 1;
        let p = Hamming15::calculate_checksum(&m, offset);
        set_int(p, &mut m[offset + 11..offset + 15]);
    }
    for indexes in COLUMN_INDEXES.iter() {
        let p = Hamming13::calculate_checksum(&m, indexes);
        for x in 0..4 {
            m[indexes[9 + x]] = ((p >> (3 - x)) & 1) as u8;
        }
    }
    m
}

fn interleave(m: &[u8; BPTC_LENGTH]) -> [u8; BPTC_LENGTH] {
    let mut tx = [0u8; BPTC_LENGTH];
    for x in 0..BPTC_LENGTH {
        tx[BPTC_DEINTERLEAVE[x]] = m[x];
    }
    tx
}

fn encode(info: &[u8]) -> [u8; BPTC_LENGTH] {
    interleave(&block(info))
}

fn random_info(rng: &mut XorShift) -> [u8; 96] {
    let mut info = [0u8; 96];
    rng.fill_bits(&mut info);
    info
}

/// (corrected, failed, wrong) over `trials` random messages with `k` random bit errors.
fn random_errors(rng: &mut XorShift, k: usize, trials: usize) -> (usize, usize, usize) {
    let (mut ok, mut failed, mut wrong) = (0, 0, 0);
    for _ in 0..trials {
        let info = random_info(rng);
        let mut rx = encode(&info);
        for p in rng.positions(BPTC_LENGTH, k) {
            rx[p] ^= 1;
        }
        match decode(&rx) {
            Some((bits, _)) if bits == info => ok += 1,
            Some(_) => wrong += 1,
            None => failed += 1,
        }
    }
    (ok, failed, wrong)
}

#[test]
fn tables_match_sdrtrunk() {
    assert_eq!(
        &BPTC_DEINTERLEAVE[..16],
        &[0, 181, 166, 151, 136, 121, 106, 91, 76, 61, 46, 31, 16, 1, 182, 167]
    );
    assert_eq!(
        &BPTC_DEINTERLEAVE[190..],
        &[105, 90, 75, 60, 45, 30, 15][1..]
    );
    assert_eq!(
        COLUMN_INDEXES[0],
        [1, 16, 31, 46, 61, 76, 91, 106, 121, 136, 151, 166, 181]
    );
    assert_eq!(
        COLUMN_INDEXES[14],
        [15, 30, 45, 60, 75, 90, 105, 120, 135, 150, 165, 180, 195]
    );
    let mut seen = [false; BPTC_LENGTH];
    for &i in BPTC_DEINTERLEAVE.iter() {
        seen[i] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn round_trip() {
    let mut rng = XorShift::new(196);
    for _ in 0..50 {
        let info = random_info(&mut rng);
        assert_eq!(decode(&encode(&info)), Some((info, 0)));
    }
}

#[test]
fn every_single_bit_error_corrected() {
    let mut rng = XorShift::new(1);
    for _ in 0..10 {
        let info = random_info(&mut rng);
        let tx = encode(&info);
        for p in 0..BPTC_LENGTH {
            let mut rx = tx;
            rx[p] ^= 1;
            // Transmitted bit 0 is R(3), outside every check: ignored, not corrected.
            let expect = if p == BPTC_DEINTERLEAVE[0] { 0 } else { 1 };
            assert_eq!(decode(&rx), Some((info, expect)), "bit {p}");
        }
    }
}

#[test]
fn every_double_bit_error_corrected() {
    let mut rng = XorShift::new(2);
    let info = random_info(&mut rng);
    let tx = encode(&info);
    for a in 1..BPTC_LENGTH {
        for b in a + 1..BPTC_LENGTH {
            let mut rx = tx;
            rx[a] ^= 1;
            rx[b] ^= 1;
            assert_eq!(decode(&rx), Some((info, 2)), "bits {a} {b}");
        }
    }
}

#[test]
fn multi_bit_errors_beyond_the_code_distance() {
    // d = 9 allows 4, but the turbo search is not maximum-likelihood: ~0.1%
    // of 3..4-bit patterns fail and ~1 in 10^4 decodes wrong (the CRC's job).
    let mut rng = XorShift::new(3);
    for k in 3..=4 {
        let (ok, failed, wrong) = random_errors(&mut rng, k, 5000);
        assert!(
            ok >= 4975 && wrong <= 2,
            "k {k}: ok {ok} failed {failed} wrong {wrong}"
        );
    }
    // It goes well past d/2.
    let (ok, _, wrong) = random_errors(&mut rng, 8, 2000);
    assert!(ok > 1850 && wrong < 25, "k 8: ok {ok} wrong {wrong}");
}

#[test]
fn random_blocks_rejected_within_the_search_budget() {
    let mut rng = XorShift::new(4);
    for _ in 0..1000 {
        let mut rx = [0u8; BPTC_LENGTH];
        rng.fill_bits(&mut rx);
        let (result, used) = decode_with_budget(&rx, MAX_SEARCH_STEPS);
        assert_eq!(result, None);
        assert!(used <= 2 * MAX_SEARCH_STEPS, "two passes");
    }
}

#[test]
fn search_budget_does_not_cost_correctable_blocks() {
    let mut rng = XorShift::new(5);
    for _ in 0..1000 {
        let info = random_info(&mut rng);
        let mut rx = encode(&info);
        for p in rng.positions(BPTC_LENGTH, 10) {
            rx[p] ^= 1;
        }
        let unlimited = decode_with_budget(&rx, usize::MAX).0;
        if let Some((bits, _)) = unlimited {
            if bits == info {
                assert_eq!(decode(&rx), unlimited);
            }
        }
    }
}

#[test]
fn sdrtrunk_reference_block_is_an_on_air_csbk() {
    // BPTC_196_96.main(): corrected reference (deinterleaved) and the received
    // copy with 16 bit errors.
    let reference = str_to_bits("0000101111100000000100001100100001010000001000000000000000000000000000000000000000000000000000000000000000000000001111001010000100001000000001110000100001011111110101011011011111101011111011100000");
    let raw = str_to_bits("0000101111101000000100001010100001010000001000000011100000000000100000000000001000000000001000000000000010000000001111001010000100001000000001100000100001000111110101011111011111101001111011100010");
    let mut block_ref = [0u8; BPTC_LENGTH];
    block_ref.copy_from_slice(&reference);
    let (info, corrected) = decode(&interleave(&block_ref)).expect("valid block");
    assert_eq!(corrected, 0);
    assert_eq!(info.to_vec(), hex_to_bits("BE10C5000000000000003A10"));
    assert_eq!(calculate_residual(&info, CSBK_CRC_MASK), 0);

    let mut block_raw = [0u8; BPTC_LENGTH];
    block_raw.copy_from_slice(&raw);
    if let Some((bits, _)) = decode(&interleave(&block_raw)) {
        assert_eq!(bits, info);
    }
}

#[test]
fn full_burst_aloha_through_slot_type_bptc_and_crc() {
    use crate::protocol::dmr::fec::crc::correct_ccitt80;
    use crate::protocol::dmr::fec::golay24;
    use crate::protocol::dmr::fec::slot_type::{DataType, SlotType, MESSAGE_INDEXES};

    // Clay Electric ALOHA, colour code 0, data type CSBK (3).
    let info = hex_to_bits("99001101F6400B000000DEDA");
    let payload = encode(&info);
    let mut slot = [0u8; 24];
    set_int(0x03, &mut slot[4..12]);
    let c = golay24::calculate_checksum(&slot, 0);
    set_int(c, &mut slot[12..23]);
    slot[23] = slot[..23].iter().fold(0, |acc, &b| acc ^ b);

    let mut burst = [0u8; 288];
    burst[24..122].copy_from_slice(&payload[..98]);
    burst[190..288].copy_from_slice(&payload[98..]);
    for (x, &i) in MESSAGE_INDEXES.iter().enumerate() {
        burst[i] = slot[4 + x];
    }
    // A few channel errors: one in the slot type, four in the BPTC payload.
    for p in [30, 77, 125, 200, 260] {
        burst[p] ^= 1;
    }

    let st = SlotType::get_slot_type(&burst).expect("slot type");
    assert_eq!(
        (st.color_code, st.data_type, st.corrected),
        (0, DataType::Csbk, 1)
    );
    let (mut bits, corrected) = decode(&payload_from_burst(&burst)).expect("bptc");
    assert_eq!(corrected, 4);
    assert_eq!(correct_ccitt80(&mut bits, CSBK_CRC_MASK), Some(0));
    assert_eq!(bits.to_vec(), info);
}

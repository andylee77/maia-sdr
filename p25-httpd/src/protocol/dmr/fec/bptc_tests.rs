//! Unit tests for `bptc.rs`.

use super::*;
use crate::protocol::dmr::fec::crc::{checksum_5, crc8};
use crate::protocol::dmr::fec::set_int;
use crate::protocol::dmr::fec::test_util::{str_to_bits, XorShift};

/// Deinterleaved BPTC(68,36) block for 36 info bits.
fn block_68_36(info: &[u8]) -> [u8; 68] {
    let mut d = [0u8; 68];
    for row in 0..3 {
        d[row * 17..row * 17 + 12].copy_from_slice(&info[row * 12..row * 12 + 12]);
        let p = Hamming17::calculate_checksum(&d, row * 17);
        set_int(p, &mut d[row * 17 + 12..row * 17 + 17]);
    }
    for column in 0..17 {
        d[51 + column] = d[column] ^ d[17 + column] ^ d[34 + column];
    }
    d
}

fn interleave_68_36(d: &[u8; 68]) -> [u8; 68] {
    let mut tx = [0u8; 68];
    for index in 0..67 {
        tx[index] = d[index * 17 % 67];
    }
    tx[67] = d[67];
    tx
}

/// Deinterleaved BPTC(128,77) block for 77 info bits (72 LC + 5 checksum).
fn block_128_77(info: &[u8]) -> [u8; 128] {
    let mut d = [0u8; 128];
    let mut pointer = 0;
    for row in 0..7 {
        let width = if row < 2 { 11 } else { 10 };
        d[row * 16..row * 16 + width].copy_from_slice(&info[pointer..pointer + width]);
        pointer += width;
        if row >= 2 {
            d[row * 16 + 10] = info[70 + row];
        }
        let p = Hamming16::calculate_checksum(&d, row * 16);
        set_int(p, &mut d[row * 16 + 11..row * 16 + 16]);
    }
    for column in 0..16 {
        d[112 + column] = (0..7).fold(0, |acc, row| acc ^ d[row * 16 + column]);
    }
    d
}

fn interleave_128_77(d: &[u8; 128]) -> [u8; 128] {
    let mut tx = [0u8; 128];
    for i in 0..127 {
        tx[(i * 8) % 127] = d[i];
    }
    tx[127] = d[127];
    tx
}

/// (corrected, failed, wrong) counts over every k-bit error pattern from `positions`.
fn sweep<const N: usize, const K: usize>(
    tx: &[u8; N],
    info: &[u8; K],
    patterns: &[Vec<usize>],
    decode: fn(&[u8]) -> Option<([u8; K], u32)>,
) -> (usize, usize, usize) {
    let (mut ok, mut failed, mut wrong) = (0, 0, 0);
    for pattern in patterns {
        let mut rx = *tx;
        for &p in pattern {
            rx[p] ^= 1;
        }
        match decode(&rx) {
            Some((bits, n)) if bits == *info => {
                assert_eq!(n as usize, pattern.len());
                ok += 1;
            }
            Some(_) => wrong += 1,
            None => failed += 1,
        }
    }
    (ok, failed, wrong)
}

fn all_pairs(n: usize) -> Vec<Vec<usize>> {
    let mut v = Vec::new();
    for a in 0..n {
        for b in a + 1..n {
            v.push(vec![a, b]);
        }
    }
    v
}

fn singles(n: usize) -> Vec<Vec<usize>> {
    (0..n).map(|p| vec![p]).collect()
}

#[test]
fn deinterleavers_are_permutations() {
    let mut seen = [false; 68];
    for index in 0..67 {
        seen[index * 17 % 67] = true;
    }
    assert!(seen[..67].iter().all(|&s| s));
    let mut seen = [false; 127];
    for i in 0..127 {
        seen[(i * 8) % 127] = true;
    }
    assert!(seen.iter().all(|&s| s));
}

#[test]
fn bptc_68_36_round_trip_and_all_single_errors() {
    let mut rng = XorShift::new(68);
    for _ in 0..20 {
        let mut info = [0u8; 36];
        rng.fill_bits(&mut info);
        let tx = interleave_68_36(&block_68_36(&info));
        assert_eq!(deinterleave_68_36(&tx), block_68_36(&info));
        assert_eq!(decode_68_36(&tx), Some((info, 0)));
        assert_eq!(sweep(&tx, &info, &singles(68), decode_68_36), (68, 0, 0));
    }
}

#[test]
fn bptc_68_36_all_double_errors_corrected() {
    // d = 6. SDRTrunk alone fixes ~88%: the retry without step 1 gets the rest.
    let mut rng = XorShift::new(6836);
    for _ in 0..5 {
        let mut info = [0u8; 36];
        rng.fill_bits(&mut info);
        let tx = interleave_68_36(&block_68_36(&info));
        assert_eq!(
            sweep(&tx, &info, &all_pairs(68), decode_68_36),
            (2278, 0, 0)
        );
    }
}

#[test]
fn bptc_68_36_triple_errors_mostly_corrected_rarely_wrong() {
    let mut rng = XorShift::new(36);
    let mut info = [0u8; 36];
    rng.fill_bits(&mut info);
    let tx = interleave_68_36(&block_68_36(&info));
    let triples: Vec<Vec<usize>> = (0..3000).map(|_| rng.positions(68, 3)).collect();
    let (ok, _failed, wrong) = sweep(&tx, &info, &triples, decode_68_36);
    // Beyond d/2 a wrong block is possible; the CRC-8 catches those.
    assert!(ok > 1800 && wrong < 30, "ok {ok} wrong {wrong}");
}

#[test]
fn bptc_68_36_carries_a_valid_short_lc() {
    // On-air SLC "2400BE39A" through BPTC(68,36) and back, CRC-8 intact.
    let mut info = [0u8; 36];
    set_int(0x2400BE3, &mut info[..28]);
    set_int(0x9A, &mut info[28..]);
    let mut rx = interleave_68_36(&block_68_36(&info));
    rx[5] ^= 1;
    rx[40] ^= 1;
    let (bits, corrected) = decode_68_36(&rx).expect("correctable");
    assert_eq!((bits, corrected), (info, 2));
    assert_eq!(crc8(&bits), 0);
}

#[test]
fn bptc_128_77_round_trip_and_all_single_errors() {
    let mut rng = XorShift::new(128);
    for _ in 0..20 {
        let mut info = [0u8; 77];
        rng.fill_bits(&mut info);
        let tx = interleave_128_77(&block_128_77(&info));
        assert_eq!(deinterleave_128_77(&tx), block_128_77(&info));
        assert_eq!(decode_128_77(&tx), Some((info, 0)));
        assert_eq!(sweep(&tx, &info, &singles(128), decode_128_77), (128, 0, 0));
    }
}

#[test]
fn bptc_128_77_double_and_triple_errors() {
    let mut rng = XorShift::new(12877);
    let mut info = [0u8; 77];
    rng.fill_bits(&mut info);
    let tx = interleave_128_77(&block_128_77(&info));
    assert_eq!(
        sweep(&tx, &info, &all_pairs(128), decode_128_77),
        (8128, 0, 0)
    );

    // d = 8: three errors are still closer to the sent block than to any other.
    let triples: Vec<Vec<usize>> = (0..3000).map(|_| rng.positions(128, 3)).collect();
    assert_eq!(sweep(&tx, &info, &triples, decode_128_77), (3000, 0, 0));
}

#[test]
fn bptc_128_77_sdrtrunk_reference_block() {
    // BPTC_128_77.main(): reference block and a received copy with 6 bit errors.
    let reference = str_to_bits(
        "00000100000101010000011000111001100000000001001101000010001101100000000110101000111100000000101000011000100010110010100100100000",
    );
    let raw = str_to_bits(
        "00000100000101010000001000111000100000000001001101000010001001000000000010101000111100000000001000011000100010110010100100100000",
    );
    let mut block = [0u8; 128];
    block.copy_from_slice(&reference);
    let (info, corrected) = decode_128_77(&interleave_128_77(&block)).expect("valid block");
    assert_eq!(corrected, 0);
    assert_eq!(checksum_5(&info), 0, "real embedded LC checksum");

    let mut raw_block = [0u8; 128];
    raw_block.copy_from_slice(&raw);
    if let Some((bits, _)) = decode_128_77(&interleave_128_77(&raw_block)) {
        assert_eq!(bits, info);
    }
}

#[test]
fn random_blocks_are_rejected() {
    // Without the MAX_CORRECTED caps ~30% (68,36) and ~9% (128,77) would pass.
    let mut rng = XorShift::new(99);
    for _ in 0..3000 {
        let mut rx = [0u8; 128];
        rng.fill_bits(&mut rx);
        assert_eq!(decode_68_36(&rx[..68]), None);
        assert_eq!(decode_128_77(&rx), None);
    }
}

#[test]
fn five_errors_in_one_row_exceed_the_68_36_cap() {
    // Step 3 can rebuild a row with 5 errors from the others' parity, but 5
    // flips is past MAX_CORRECTED_68_36.
    let info = [1u8; 36];
    let block = block_68_36(&info);
    let mut bad = block;
    for column in [0, 3, 6, 9, 12] {
        bad[17 + column] ^= 1;
    }
    let mut corrected = bad;
    assert!(BPTCBase::new(Hamming17, 17, 4, false).correct(&mut corrected));
    assert_eq!(corrected, block);
    assert_eq!(decode_68_36(&interleave_68_36(&bad)), None);
}

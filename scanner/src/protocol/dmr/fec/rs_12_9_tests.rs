//! Unit tests for `rs_12_9.rs`.

use super::*;
use crate::protocol::dmr::fec::test_util::XorShift;

/// 96-bit full LC: 9 data bytes, parity XOR mask.
fn encode(data: &[u8; 9], mask: u8) -> Vec<u8> {
    let parity = calculate_checksum(data);
    let mut bits = vec![0u8; 96];
    for (i, &b) in data
        .iter()
        .chain(parity.iter().map(|p| p ^ mask).collect::<Vec<_>>().iter())
        .enumerate()
    {
        set_int(u32::from(b), &mut bits[i * 8..i * 8 + 8]);
    }
    bits
}

fn random_data(rng: &mut XorShift) -> [u8; 9] {
    core::array::from_fn(|_| rng.next_u64() as u8)
}

#[test]
fn tables_match_sdrtrunk() {
    // Spot checks against RS_12_9_DMR.EXPONENTS_TABLE / LOG_TABLE.
    assert_eq!(
        &EXPONENTS_TABLE[..12],
        &[0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1D, 0x3A, 0x74, 0xE8]
    );
    assert_eq!(
        &EXPONENTS_TABLE[248..],
        &[0x1B, 0x36, 0x6C, 0xD8, 0xAD, 0x47, 0x8E, 0x01]
    );
    assert_eq!(&LOG_TABLE[..10], &[0, 0, 1, 25, 2, 50, 26, 198, 3, 223]);
    assert_eq!(&LOG_TABLE[250..], &[244, 234, 168, 80, 88, 175]);
}

#[test]
fn generator_has_roots_a1_a2_a3() {
    for j in 1..=3 {
        let x = EXPONENTS_TABLE[j];
        let mut value = 0u8;
        for &coefficient in GENERATOR_POLYNOMIAL.iter().rev() {
            value = galois_multiplication(value, x) ^ coefficient;
        }
        assert_eq!(value, 0, "a^{j}");
    }
}

#[test]
fn clean_codewords_pass_with_their_mask_only() {
    let mut rng = XorShift::new(129);
    for _ in 0..100 {
        let data = random_data(&mut rng);
        let mut m = encode(&data, VOICE_LINK_CONTROL_CRC_MASK);
        assert_eq!(correct(&mut m, VOICE_LINK_CONTROL_CRC_MASK), Ok(0));
        // With mask 0 the residual is the mask in use, in all three bytes.
        assert_eq!(correct(&mut m.clone(), 0), Err(0x969696));
        assert_eq!(
            correct(&mut m.clone(), TERMINATOR_LINK_CONTROL_CRC_MASK),
            Err(0x0F0F0F)
        );
        let mut t = encode(&data, TERMINATOR_LINK_CONTROL_CRC_MASK);
        assert_eq!(correct(&mut t, TERMINATOR_LINK_CONTROL_CRC_MASK), Ok(0));
    }
}

#[test]
fn every_single_byte_error_corrected() {
    let mut rng = XorShift::new(3);
    let data = random_data(&mut rng);
    let original = encode(&data, TERMINATOR_LINK_CONTROL_CRC_MASK);
    for index in 0..12 {
        for error in 1..=255u8 {
            let mut m = original.clone();
            let byte = &mut m[index * 8..index * 8 + 8];
            set_int(get_int(byte) ^ u32::from(error), byte);
            assert_eq!(
                correct(&mut m, TERMINATOR_LINK_CONTROL_CRC_MASK),
                Ok(error.count_ones()),
                "byte {index} error {error:#x}"
            );
            assert_eq!(m, original);
        }
    }
}

#[test]
fn two_byte_errors_always_detected() {
    let mut rng = XorShift::new(12_9);
    let data = random_data(&mut rng);
    let original = encode(&data, VOICE_LINK_CONTROL_CRC_MASK);
    for a in 0..12 {
        for b in a + 1..12 {
            for _ in 0..40 {
                let mut m = original.clone();
                for index in [a, b] {
                    let error = (rng.next_u64() % 255 + 1) as u32;
                    let byte = &mut m[index * 8..index * 8 + 8];
                    set_int(get_int(byte) ^ error, byte);
                }
                let before = m.clone();
                assert!(
                    correct(&mut m, VOICE_LINK_CONTROL_CRC_MASK).is_err(),
                    "bytes {a} {b}"
                );
                assert_eq!(m, before, "failed decode leaves the message alone");
            }
        }
    }
}

#[test]
fn single_bit_errors_in_parity_fixed_without_unmasking() {
    // SDRTrunk writes the masked parity byte back; this port flips only the bad bit.
    let data = [0x00, 0x00, 0x20, 0x01, 0x57, 0x74, 0x01, 0x40, 0x0F];
    let original = encode(&data, VOICE_LINK_CONTROL_CRC_MASK);
    for p in 72..96 {
        let mut m = original.clone();
        m[p] ^= 1;
        assert_eq!(correct(&mut m, VOICE_LINK_CONTROL_CRC_MASK), Ok(1));
        assert_eq!(m, original);
    }
}

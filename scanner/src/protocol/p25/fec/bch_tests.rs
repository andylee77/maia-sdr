//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;
use crate::protocol::p25::test_fixtures::*;

/// Golden vector from `BCH_63_16_23_P25_Test.java` line 35-40:
///   NAC=1 (0x001), DUID=0 (HDU) -> 0x00103185B7E9E224
/// Bit-exact with the SDRTrunk encoder. If this fails the generator
/// matrix or bit ordering is wrong.
#[test]
fn encoder_matches_sdrtrunk_vector() {
    let cw = encode_nid(1, 0);
    assert_eq!(cw, 0x0010_3185_B7E9_E224);
}

/// The Python reference's `_self_test` Test 2 — round-trip a handful of
/// (NAC, DUID) pairs through encoder + decoder, expect zero errors.
#[test]
fn encode_decode_roundtrip_clean() {
    let pairs: &[(u16, u8)] = &[
        (0x000, 0),
        (0x001, 0),
        (CLAY_NAC, 7), // Clay County NAC
        (0xFFF, 0xF),
        (0x534, 2),
        (0x123, 5),
    ];
    for &(nac, duid) in pairs {
        let cw = encode_nid(nac, duid);
        let decoded = decode_nid(cw).expect("clean codeword must decode");
        assert_eq!(decoded.nac, nac);
        assert_eq!(decoded.duid, duid);
        assert_eq!(decoded.n_errors, 0);
    }
}

/// Error correction sweep up to t=11 — Python `_self_test` Test 3.
/// For NAC=0x8A1 / DUID=7, flip n random bits in positions 0..62 and
/// verify the decoder corrects them. Uses a deterministic xorshift PRNG
/// so the test is reproducible without bringing in `rand`.
#[test]
fn error_correction_sweep_up_to_t11() {
    let base = encode_nid(CLAY_NAC, 7);
    let mut rng = XorShift64::new(0xDEAD_BEEF_CAFE_BABE);
    const TRIALS_PER_LEVEL: usize = 50;
    for n_errors in 1..=11u32 {
        for _ in 0..TRIALS_PER_LEVEL {
            let mut positions: [u8; 11] = [0xFF; 11];
            let mut filled = 0;
            while filled < n_errors as usize {
                // SDRTrunk's test methodology excludes parity bit 63
                let p = (rng.next_u32() % 63) as u8;
                if !positions[..filled].contains(&p) {
                    positions[filled] = p;
                    filled += 1;
                }
            }
            let mut corrupted = base;
            for &p in &positions[..filled] {
                corrupted ^= 1u64 << (CODE_BITS - 1 - p as u32);
            }
            let decoded = decode_nid(corrupted)
                .expect("within sphere -> must decode");
            assert_eq!(decoded.nac, CLAY_NAC);
            assert_eq!(decoded.duid, 7);
            assert_eq!(decoded.n_errors as u32, n_errors);
        }
    }
}

/// 12+ bit errors are outside the unique-decoding sphere; the decoder
/// must NOT silently return the wrong NAC. It may either flag
/// uncorrectable, or land on a different valid codeword (also fine).
#[test]
fn errors_beyond_sphere_dont_silently_corrupt() {
    let base = encode_nid(CLAY_NAC, 7);
    let mut rng = XorShift64::new(0xC0FFEE);
    let mut wrong_or_uncorrectable = 0;
    const TRIALS: usize = 100;
    for _ in 0..TRIALS {
        let mut positions: [u8; 12] = [0xFF; 12];
        let mut filled = 0;
        while filled < 12 {
            let p = (rng.next_u32() % 63) as u8;
            if !positions[..filled].contains(&p) {
                positions[filled] = p;
                filled += 1;
            }
        }
        let mut corrupted = base;
        for &p in &positions[..filled] {
            corrupted ^= 1u64 << (CODE_BITS - 1 - p as u32);
        }
        match decode_nid(corrupted) {
            None => wrong_or_uncorrectable += 1,
            Some(d) if (d.nac, d.duid) != (CLAY_NAC, 7) => {
                wrong_or_uncorrectable += 1
            }
            Some(_) => {}
        }
    }
    // Most 12-error patterns should fall outside the sphere. Don't
    // pin a tight number — this is informational about beyond-t behavior.
    assert!(
        wrong_or_uncorrectable > TRIALS / 4,
        "12-error words should mostly leave the sphere; \
         only {wrong_or_uncorrectable}/{TRIALS} did"
    );
}

/// Every codeword, by its data word.
fn codebook() -> Vec<u64> {
    (0..1u32 << 16).map(|d| encode_nid((d >> 4) as u16, (d & 0xF) as u8)).collect()
}

/// Every codeword's BCH word (bits 63..1) has α^1 ..= α^22 for roots: the field, the bit order
/// and the generator agree.
#[test]
fn every_codeword_has_zero_syndromes() {
    for (d, c) in codebook().into_iter().enumerate() {
        assert!(syndromes(c >> 1).iter().all(|&s| s == 0), "data word {d:04x}");
    }
}

/// Every non-zero codeword has 23 or more bits set (the code is linear), so two codewords differ
/// in 23 or more and a codeword within 11 bits of a word is the only one.
#[test]
fn the_minimum_distance_is_23() {
    let lightest = codebook()[1..].iter().map(|c| c.count_ones()).min().unwrap();
    assert!(lightest >= 2 * T_MAX_ERRORS + 1, "minimum distance {lightest}");
}

/// The decoder answers as the closest codeword of all 65536 would (the first of equals), for
/// words with every number of errors over all 64 bits, the most around the 11-error edge, and
/// for noise.
#[test]
fn the_decoder_is_the_closest_codeword() {
    let book = codebook();
    let closest = |r: u64| {
        let (idx, d) = book.iter().enumerate().map(|(i, c)| (i, (c ^ r).count_ones())).min_by_key(|&(i, d)| (d, i)).unwrap();
        (d <= T_MAX_ERRORS).then(|| ((idx >> 4) as u16, (idx & 0xF) as u8, d as u8))
    };
    let mut rng = XorShift64::new(0x5EED_0F_B0C4);
    for trial in 0..600 {
        let errors = [0, 1, 2, 3, 5, 8, 9, 10, 10, 11, 11, 11, 12, 12, 13, 16][trial % 16];
        let received = if trial % 20 == 19 {
            rng.next_u64()
        } else {
            let mut w = book[(rng.next_u32() & 0xFFFF) as usize];
            let mut flipped = 0u64;
            while flipped.count_ones() < errors {
                flipped |= 1u64 << (rng.next_u32() % 64);
            }
            w ^= flipped;
            w
        };
        let got = decode_nid(received).map(|d| (d.nac, d.duid, d.n_errors));
        assert_eq!(got, closest(received), "word {received:016x}");
    }
}

/// Tiny xorshift64 PRNG for reproducible tests without `rand`.
struct XorShift64(u64);
impl XorShift64 {
    fn new(seed: u64) -> Self {
        XorShift64(if seed == 0 { 1 } else { seed })
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn next_u32(&mut self) -> u32 {
        self.next_u64() as u32
    }
}

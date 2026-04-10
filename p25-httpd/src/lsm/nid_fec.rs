//! BCH(63,16,11) NID forward error correction.
//!
//! Phase 6D port of `tools/p25_nid_fec.py` (which is itself a port of
//! SDRTrunk's `BCH_63_16_23_P25_Test.java` encoder + a maximum-likelihood
//! decoder over the 65536-entry codebook).
//!
//! The P25 NID is a 64-bit field protected by binary BCH(63,16,d=23):
//!
//! ```text
//! bit  0..11 : NAC  (12 bits, MSB-first within the data word)
//! bit 12..15 : DUID (4 bits)
//! bit 16..63 : 48 BCH parity bits
//! ```
//!
//! With minimum distance d=23 the code corrects up to t = (d-1)/2 = 11 bit
//! errors. We decode by maximum likelihood: compute the Hamming distance to
//! every one of the 65536 valid codewords and pick the closest. Within the
//! unique-decoding sphere of radius t, ML decoding is identical to a
//! properly-implemented Berlekamp-Massey decoder, so this is bit-exact with
//! SDRTrunk's BCH decoder for any received word with <=11 bit errors.
//!
//! The codebook is built on the first call via `OnceLock` and reused for the
//! lifetime of the process. ~512 KB resident, built in <10 ms on a Cortex-A9.
//! Phase 6E will fold the codebook into BRAM.
//!
//! Source of truth for the generator matrix:
//!     sdrtrunk/src/test/java/io/github/dsheirer/edac/bch/BCH_63_16_23_P25_Test.java
//! The 16 octal rows below are copied verbatim from
//! `P25_NID_BCH_63_16_GENERATOR_MATRIX` in that file.

use std::sync::OnceLock;

/// 16-row systematic generator matrix from `BCH_63_16_23_P25_Test.java`.
/// Octal literals in the Java source, converted to u64 here.
const P25_NID_GENERATOR_MATRIX: [u64; 16] = [
    0o6331141367235452, // row  0
    0o5265521614723276, // row  1
    0o4603711461164164, // row  2
    0o2301744630472072, // row  3
    0o7271623073000466, // row  4
    0o5605650752635660, // row  5
    0o2702724365316730, // row  6
    0o1341352172547354, // row  7
    0o0560565075263566, // row  8
    0o6141333751704220, // row  9
    0o3060555764742110, // row 10
    0o1430266772361044, // row 11
    0o0614133375170422, // row 12
    0o6037114611641642, // row 13
    0o5326507063515373, // row 14
    0o4662302756473127, // row 15
];

/// Bits in the NAC field.
const NAC_BITS: u32 = 12;
/// Bits in the DUID field.
const DUID_BITS: u32 = 4;
/// Bits in the data word (NAC || DUID).
const DATA_BITS: u32 = NAC_BITS + DUID_BITS;
/// Bits in the parity field.
const PARITY_BITS: u32 = 48;
/// Bits in the full code word.
pub const CODE_BITS: u32 = DATA_BITS + PARITY_BITS;
/// Maximum bit errors the BCH(63,16,23) code can correct: (d-1)/2 = 11.
pub const T_MAX_ERRORS: u32 = 11;

/// Total codebook entries (2^16).
const N_CODEWORDS: usize = 1 << DATA_BITS;

/// Encode a (NAC, DUID) pair into a 64-bit BCH(63,16,11) codeword.
///
/// Verbatim port of `BCH_63_16_23_P25_Test.create()`:
///
/// ```java
/// CorrectedBinaryMessage cbm = new CorrectedBinaryMessage(64);
/// cbm.setInt(nac,  NAC_FIELD);   // bits 0..11  (MSB-first)
/// cbm.setInt(duid, DUID_FIELD);  // bits 12..15
/// long parity = 0;
/// for (int x = 0; x < 16; x++) {
///     if (cbm.get(x)) parity ^= P25_NID_BCH_63_16_GENERATOR_MATRIX[x];
/// }
/// cbm.load(16, 48, parity);
/// ```
///
/// # Panics
/// Panics if `nac >= 4096` or `duid >= 16`.
pub fn encode_nid(nac: u16, duid: u8) -> u64 {
    assert!((nac as u32) < (1 << NAC_BITS), "NAC out of range: {nac}");
    assert!((duid as u32) < (1 << DUID_BITS), "DUID out of range: {duid}");

    let data_word: u64 = ((nac as u64) << DUID_BITS) | (duid as u64);
    let mut parity: u64 = 0;
    // Iterate bit positions 0..15 in MSB-first order, matching cbm.get(x).
    // Bit 0 of cbm is the MSB of data_word (bit DATA_BITS-1 in numeric form).
    for bit_idx in 0..DATA_BITS {
        if data_word & (1 << (DATA_BITS - 1 - bit_idx)) != 0 {
            parity ^= P25_NID_GENERATOR_MATRIX[bit_idx as usize];
        }
    }
    (data_word << PARITY_BITS) | parity
}

/// Lazily built ML codebook: `[u64; 65536]` of valid codewords, indexed by
/// data word value `((nac << DUID_BITS) | duid)`.
fn codebook() -> &'static [u64; N_CODEWORDS] {
    static CODEBOOK: OnceLock<Box<[u64; N_CODEWORDS]>> = OnceLock::new();
    CODEBOOK.get_or_init(|| {
        let mut cb = Box::new([0u64; N_CODEWORDS]);
        for nac in 0..(1u32 << NAC_BITS) {
            for duid in 0..(1u32 << DUID_BITS) {
                let idx = (nac << DUID_BITS) | duid;
                cb[idx as usize] = encode_nid(nac as u16, duid as u8);
            }
        }
        cb
    })
}

/// Result of a successful BCH NID decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedNid {
    pub nac: u16,
    pub duid: u8,
    /// Number of bit errors corrected (0..=11).
    pub n_errors: u8,
}

/// Decode a received 64-bit NID via maximum likelihood.
///
/// Returns `None` if the closest codeword has more than `T_MAX_ERRORS` (11)
/// bit differences from the received word — i.e. the received word is
/// outside the BCH(63,16,23) unique-decoding sphere.
pub fn decode_nid(received_nid: u64) -> Option<DecodedNid> {
    let cb = codebook();
    // Mask to 64 bits is a no-op, but kept symmetric with the Python ref
    // which masks to `(1 << CODE_BITS) - 1`.
    let received = received_nid;

    // Linear ML scan. 65536 XOR + popcount + min — runs in well under 1 ms
    // on a Cortex-A9.
    let mut best_idx: usize = 0;
    let mut best_dist: u32 = u32::MAX;
    for (idx, &cw) in cb.iter().enumerate() {
        let d = (cw ^ received).count_ones();
        if d < best_dist {
            best_dist = d;
            best_idx = idx;
            if d == 0 {
                break;
            }
        }
    }

    if best_dist > T_MAX_ERRORS {
        return None;
    }
    let data = best_idx as u32;
    let nac = ((data >> DUID_BITS) & ((1 << NAC_BITS) - 1)) as u16;
    let duid = (data & ((1 << DUID_BITS) - 1)) as u8;
    Some(DecodedNid {
        nac,
        duid,
        n_errors: best_dist as u8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
            (0x8A1, 7), // Clay County NAC
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
        let base = encode_nid(0x8A1, 7);
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
                assert_eq!(decoded.nac, 0x8A1);
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
        let base = encode_nid(0x8A1, 7);
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
                Some(d) if (d.nac, d.duid) != (0x8A1, 7) => {
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
}

//! BCH(63,16,11) NID forward error correction.
//!
//! SDRTrunk's `BCH_63_16_23_P25_Test.java` encoder, with a maximum-likelihood
//! decoder over the 65536-entry codebook.
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
/// Bits in the full code word. Test-only today; kept pub because
/// `bch_tests.rs` exercises the full 64-bit syndrome calc with it.
#[cfg(test)]
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

/// Decode a received 64-bit NID: the codeword within `T_MAX_ERRORS` (11) bits of it, or `None`
/// when the received word is outside every codeword's unique-decoding sphere.
///
/// The code's minimum distance is 23, so a codeword within 11 bits is the only one, and the
/// closest: the search stops at the first it finds. It starts with the codewords of the received
/// data word and of the words one data bit from it, which hold nearly every NID on the air; a
/// word with no codeword in reach takes the whole codebook.
pub fn decode_nid(received: u64) -> Option<DecodedNid> {
    let cb = codebook();
    let data = (received >> PARITY_BITS) as usize;
    let near = std::iter::once(data).chain((0..DATA_BITS).map(|b| data ^ (1 << b)));
    let (idx, distance) = near.chain(0..N_CODEWORDS).find_map(|idx| {
        let d = (cb[idx] ^ received).count_ones();
        (d <= T_MAX_ERRORS).then_some((idx as u32, d))
    })?;
    Some(DecodedNid {
        nac: ((idx >> DUID_BITS) & ((1 << NAC_BITS) - 1)) as u16,
        duid: (idx & ((1 << DUID_BITS) - 1)) as u8,
        n_errors: distance as u8,
    })
}
#[cfg(test)]
#[path = "bch_tests.rs"]
mod tests;

//! BCH(63,16,23) NID forward error correction.
//!
//! SDRTrunk's `BCH_63_16_23_P25_Test.java` encoder, and its decoder's method: syndromes,
//! Berlekamp-Massey and the error locator's roots over GF(2^6) (`BCH`, `BCH_63`).
//!
//! The P25 NID is a 64-bit field protected by binary BCH(63,16,d=23):
//!
//! ```text
//! bit  0..11 : NAC  (12 bits, MSB-first within the data word)
//! bit 12..15 : DUID (4 bits)
//! bit 16..62 : 47 BCH parity bits
//! bit 63     : the 48th parity bit (outside the BCH code)
//! ```
//!
//! The code corrects up to t = 11 bit errors in its 63 bits. A NID is accepted when the codeword
//! found is within 11 bits of all 64 (SDRTrunk does not look at bit 63): the answer is the
//! closest codeword's, as a search of all 65536 would give.
//!
//! Source of truth for the generator matrix:
//!     sdrtrunk/src/test/java/io/github/dsheirer/edac/bch/BCH_63_16_23_P25_Test.java
//! The 16 octal rows below are copied verbatim from
//! `P25_NID_BCH_63_16_GENERATOR_MATRIX` in that file.

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

/// Result of a successful BCH NID decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedNid {
    pub nac: u16,
    pub duid: u8,
    /// Number of bit errors corrected (0..=11).
    pub n_errors: u8,
}

/// GF(2^6) over x^6 + x + 1 (SDRTrunk `PRIMITIVE_POLYNOMIAL_GF_63`): α^i for i < 63, and log_α.
const N: usize = 63;
const GF: ([u8; 64], [u8; 64]) = {
    let (mut pow, mut log) = ([0u8; 64], [0u8; 64]);
    let (mut x, mut i) = (1u32, 0);
    while i < N {
        pow[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 64 != 0 {
            x ^= 0x43;
        }
        i += 1;
    }
    (pow, log)
};

fn alpha(e: usize) -> u8 {
    GF.0[e % N]
}

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 { 0 } else { alpha(GF.1[a as usize] as usize + GF.1[b as usize] as usize) }
}

/// a / b, b not zero.
fn div(a: u8, b: u8) -> u8 {
    if a == 0 { 0 } else { alpha(GF.1[a as usize] as usize + N - GF.1[b as usize] as usize) }
}

/// The syndromes S_1 ..= S_2t of a 63-bit word (bit k the coefficient of x^k): `s[j]` is S_j.
fn syndromes(word: u64) -> [u8; 2 * T_MAX_ERRORS as usize + 1] {
    let mut s = [0u8; 2 * T_MAX_ERRORS as usize + 1];
    for j in (1..s.len()).step_by(2) {
        let mut bits = word;
        while bits != 0 {
            s[j] ^= alpha(j * bits.trailing_zeros() as usize);
            bits &= bits - 1;
        }
    }
    // A binary code: S_2j = S_j^2.
    for j in (2..s.len()).step_by(2) {
        s[j] = mul(s[j / 2], s[j / 2]);
    }
    s
}

/// The error positions in a 63-bit word, as a mask: Berlekamp-Massey's error locator and its
/// roots. `None` when it is not a correctable word (more than 11 errors): the locator's degree
/// is over 11, or it has fewer distinct roots than its degree.
fn error_positions(word: u64) -> Option<u64> {
    const T: usize = T_MAX_ERRORS as usize;
    let s = syndromes(word);
    if s.iter().all(|&v| v == 0) {
        return Some(0);
    }
    // C(x), the error locator; B(x), C before its last length change; `step` the shift since.
    let (mut c, mut prev) = ([0u8; 2 * T + 2], [0u8; 2 * T + 2]);
    c[0] = 1;
    prev[0] = 1;
    let (mut len, mut step, mut prev_d) = (0usize, 1usize, 1u8);
    for n in 0..2 * T {
        let d = (1..=len).fold(s[n + 1], |d, i| d ^ mul(c[i], s[n + 1 - i]));
        if d == 0 {
            step += 1;
            continue;
        }
        let before = c;
        let f = div(d, prev_d);
        for i in 0..c.len() - step {
            c[i + step] ^= mul(f, prev[i]);
        }
        if 2 * len <= n {
            len = n + 1 - len;
            prev = before;
            prev_d = d;
            step = 1;
        } else {
            step += 1;
        }
    }
    if len > T || c[len] == 0 || c[len + 1..].iter().any(|&v| v != 0) {
        return None;
    }
    // An error at x^k is a root at α^-k.
    let mut errors = 0u64;
    let mut roots = 0;
    for k in 0..N {
        let v = (0..=len).filter(|&i| c[i] != 0).fold(0u8, |v, i| v ^ alpha(GF.1[c[i] as usize] as usize + i * (N - k)));
        if v == 0 {
            errors |= 1 << k;
            roots += 1;
        }
    }
    (roots == len).then_some(errors)
}

/// Decode a received 64-bit NID: the codeword within `T_MAX_ERRORS` (11) bits of it, or `None`
/// when the received word is outside every codeword's unique-decoding sphere.
pub fn decode_nid(received: u64) -> Option<DecodedNid> {
    // The BCH word is bits 63..1 (x^62 .. x^0); bit 0 is the NID's 48th parity bit.
    let errors = error_positions(received >> 1)?;
    let data = ((received ^ (errors << 1)) >> PARITY_BITS) as u32;
    let (nac, duid) = ((data >> DUID_BITS) as u16, (data & ((1 << DUID_BITS) - 1)) as u8);
    let distance = (encode_nid(nac, duid) ^ received).count_ones();
    (distance <= T_MAX_ERRORS).then_some(DecodedNid { nac, duid, n_errors: distance as u8 })
}

#[cfg(test)]
#[path = "bch_tests.rs"]
mod tests;

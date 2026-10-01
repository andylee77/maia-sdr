//! Reed-Solomon RS(12,9,4) over GF(2^8) for full LC: port of SDRTrunk `edac/RS_12_9_DMR.java`.
//!
//! 96 bits = 9 LC bytes + 3 parity bytes, each parity byte XORed with the
//! data type's mask byte. Corrects one byte (up to 8 bit errors in it).
//!
//! SDRTrunk's Berlekamp-Massey corrects the first error-locator root it finds
//! without re-checking the syndrome, so some 2-byte errors come back "corrected"
//! wrong. With only 3 syndromes the code can fix exactly one byte, so this port
//! solves that case directly (S2^2 = S1*S3) and reports everything else as
//! uncorrectable; d = 4 guarantees every 2-byte error is caught.
//!
//! Validity is residual 0. Do not copy `LCMessageFactory.java:134`
//! (`valid = residual != 0`), which inverts it.

use super::{get_int, set_int};

/// Parity mask byte for the voice LC header (ETSI 0x969696).
pub const VOICE_LINK_CONTROL_CRC_MASK: u8 = 0x96;
/// Parity mask byte for the terminator with LC (ETSI 0x999999).
pub const TERMINATOR_LINK_CONTROL_CRC_MASK: u8 = 0x99;

/// Primitive polynomial x^8 + x^4 + x^3 + x^2 + 1 (TS 102 361-1 B.14).
const PRIMITIVE_POLYNOMIAL: u32 = 0x11D;
/// g(x) = (x + a)(x + a^2)(x + a^3), lowest power first (B.10).
pub const GENERATOR_POLYNOMIAL: [u8; 4] = [0x40, 0x38, 0x0E, 0x01];
const CODEWORD_SIZE: usize = 12;
const CHECKSUM_SIZE: usize = 3;

/// (EXPONENTS_TABLE, LOG_TABLE), TS 102 361-1 Tables B.19 and B.20.
const TABLES: ([u8; 256], [u8; 256]) = build_tables();
const EXPONENTS_TABLE: [u8; 256] = TABLES.0;
const LOG_TABLE: [u8; 256] = TABLES.1;

const fn build_tables() -> ([u8; 256], [u8; 256]) {
    let mut exponents = [0u8; 256];
    let mut log = [0u8; 256];
    let mut x: u32 = 1;
    let mut i = 0;
    while i < 255 {
        exponents[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= PRIMITIVE_POLYNOMIAL;
        }
        i += 1;
    }
    exponents[255] = 1;
    (exponents, log)
}

fn galois_multiplication(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    EXPONENTS_TABLE
        [(usize::from(LOG_TABLE[usize::from(a)]) + usize::from(LOG_TABLE[usize::from(b)])) % 255]
}

fn galois_inverse(element: u8) -> u8 {
    EXPONENTS_TABLE[255 - usize::from(LOG_TABLE[usize::from(element)])]
}

/// Syndromes S1..S3 = c(a^1..a^3), byte 0 the highest power. Ports `calculateSyndrome()`.
fn calculate_syndrome(codeword: &[u8; CODEWORD_SIZE]) -> [u8; CHECKSUM_SIZE] {
    let mut syndrome = [0u8; CHECKSUM_SIZE];
    for (j, s) in syndrome.iter_mut().enumerate() {
        for &symbol in codeword {
            *s = galois_multiplication(EXPONENTS_TABLE[j + 1], *s) ^ symbol;
        }
    }
    syndrome
}

/// Parity bytes for the 9 data bytes (unmasked). Ports `calculateChecksum()`.
pub fn calculate_checksum(codeword: &[u8]) -> [u8; CHECKSUM_SIZE] {
    let mut checksum = [0u8; CHECKSUM_SIZE];
    for &symbol in &codeword[..9] {
        let feedback = symbol ^ checksum[0];
        checksum[0] = checksum[1] ^ galois_multiplication(GENERATOR_POLYNOMIAL[2], feedback);
        checksum[1] = checksum[2] ^ galois_multiplication(GENERATOR_POLYNOMIAL[1], feedback);
        checksum[2] = galois_multiplication(GENERATOR_POLYNOMIAL[0], feedback);
    }
    checksum
}

/// (byte index, error value) when the syndromes fit exactly one bad byte.
fn locate_single_error(syndrome: &[u8; CHECKSUM_SIZE]) -> Option<(usize, u8)> {
    let [s1, s2, s3] = *syndrome;
    if s1 == 0 || s2 == 0 || galois_multiplication(s2, s2) != galois_multiplication(s1, s3) {
        return None;
    }
    let locator = galois_multiplication(s2, galois_inverse(s1)); // a^power
    let power = usize::from(LOG_TABLE[usize::from(locator)]);
    if power >= CODEWORD_SIZE {
        return None;
    }
    Some((
        CODEWORD_SIZE - 1 - power,
        galois_multiplication(s1, galois_inverse(locator)),
    ))
}

/// Checks and corrects a 96-bit full LC in place: `Ok(corrected bits)`, or
/// `Err(residual)` where the residual (3 parity bytes, 0xAABBCC) is the extra
/// mask that would make the parity match. Ports `RS_12_9_DMR.correct()`.
pub fn correct(message: &mut [u8], mask: u8) -> Result<u32, u32> {
    let mut codeword = [0u8; CODEWORD_SIZE];
    for (index, symbol) in codeword.iter_mut().enumerate() {
        *symbol = get_int(&message[index * 8..index * 8 + 8]) as u8;
        if index >= 9 {
            *symbol ^= mask;
        }
    }

    let syndrome = calculate_syndrome(&codeword);
    if syndrome == [0; CHECKSUM_SIZE] {
        return Ok(0);
    }

    if let Some((index, error)) = locate_single_error(&syndrome) {
        let byte = &mut message[index * 8..index * 8 + 8];
        set_int(get_int(byte) ^ u32::from(error), byte);
        return Ok(error.count_ones());
    }

    let checksum = calculate_checksum(&codeword);
    let residual = (0..CHECKSUM_SIZE).fold(0u32, |acc, i| {
        (acc << 8) | u32::from(checksum[i] ^ codeword[9 + i])
    });
    Err(residual)
}

#[cfg(test)]
#[path = "rs_12_9_tests.rs"]
mod tests;

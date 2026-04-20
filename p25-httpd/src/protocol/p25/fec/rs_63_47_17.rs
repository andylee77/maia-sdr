//! Reed-Solomon(63,47,17) over GF(2^6).
//!
//! Protects the 120-bit HDU header body: 20 CW hexbits + 16 RS parity
//! hexbits, carrying Message Indicator (72 bits), Algorithm ID
//! (8 bits), Key ID (16 bits), and talkgroup (16 bits) after Golay18
//! per-codeword correction. `t = 8` correctable hexbit errors.
//!
//! Once the HDU RS is decoded we can mirror SDRTrunk's
//! `HDU TALKGROUP:<tg> ENCRYPTION:<alg> KEY:<id> MI:<hex>` lines and,
//! when our grant store missed the GRP_VCH_GRANT, recover the
//! encryption flag from the voice path directly.
//!
//! Thin shim over `rs_p25::decode`. Unlike RS(24,12,13) / RS(24,16,9),
//! this variant uses the full 63-symbol code (no shortening) — the
//! caller still populates all 63 input positions but only
//! `input[0..36]` carry real data (20 CW + 16 parity) and
//! `input[36..63]` are zero-filled per SDRTrunk's HDUMessage layout.

pub use super::rs_p25::NN;

/// Information-symbol count `kk` passed to the shared Berlekamp-Massey
/// core. Unlike the shortened variants, this one is the full 63-symbol
/// code.
pub const KK: usize = 47;

/// Decode a (full-length, nominally 36-symbol data-bearing) RS(63,47,17)
/// codeword. Caller populates `input` in SDRTrunk's HDU RS-input
/// order and zero-fills unused positions.
pub fn decode(input: &[u32; NN]) -> Result<[u32; NN], [u32; NN]> {
    super::rs_p25::decode(input, KK)
}

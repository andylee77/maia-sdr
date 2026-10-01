//! Reed-Solomon(24,16,9) — shortened from RS(63,55,9) over GF(2^6).
//!
//! Protects the 96-bit LDU2 Encryption Sync Signature (ESS):
//! 16 ESS hexbits + 8 RS parity hexbits. `t = 4` correctable
//! hexbit errors.
//!
//! The ESS carries the per-LDU continuously-refreshed encryption
//! metadata — Message Indicator (MI), Algorithm ID, Key ID — needed
//! for AES-256 voice decryption and to mirror SDRTrunk's
//! `LDU2 VOICE LSD:... ENCRYPTION:AES-256 KEY:... MSG INDICATOR:...`
//! log lines.
//!
//! Thin shim over `rs_p25::decode`; the field + Berlekamp iteration
//! are shared with RS(24,12,13) and RS(63,47,17).

pub use super::rs_p25::NN;

/// Information-symbol count `kk` passed to the shared Berlekamp-Massey
/// core for this shortened variant. 63 - kk = 8 data symbols.
pub const KK: usize = 55;

/// Decode a shortened RS(24,16,9) codeword. Caller packs the 24
/// hexbits into `input[0..24]` in SDRTrunk's RS-input order (RS_HEX_7
/// first, ..., RS_HEX_0, then CW_HEX_15, ..., CW_HEX_0) and leaves
/// `input[24..63]` zero-filled.
pub fn decode(input: &[u32; NN]) -> Result<[u32; NN], [u32; NN]> {
    super::rs_p25::decode(input, KK)
}

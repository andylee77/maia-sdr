//! Reed-Solomon(24,12,13) — shortened from RS(63,51,13) over GF(2^6).
//!
//! Protects the 72-bit Link Control Word in TDULC and LDU1 frames
//! (12 LC hexbits + 12 RS parity hexbits). `t = 6` correctable
//! hexbit errors.
//!
//! The heavy lifting is in `rs_p25::decode`, which is shared with
//! RS(24,16,9) and RS(63,47,17). This module is a thin shim so the
//! existing `TDULCMessage.createLinkControlWord` / LDU1 call sites
//! don't need to carry the `kk` parameter.

pub use super::rs_p25::NN;

/// Decode a shortened RS(24,12,13) codeword. Caller packs the 24
/// hexbits into `input[0..24]` in SDRTrunk's RS-input order (RS_HEX_11
/// first, ..., RS_HEX_0, then LC_HEX_11, ..., LC_HEX_0) and leaves
/// `input[24..63]` zero-filled.
pub fn decode(input: &[u32; NN]) -> Result<[u32; NN], [u32; NN]> {
    super::rs_p25::decode(input, 51)
}

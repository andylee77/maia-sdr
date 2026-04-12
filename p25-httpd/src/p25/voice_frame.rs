//! P25 Phase 1 voice frame extraction (Phase 7C)
//!
//! Extracts the 9 raw 144-bit IMBE voice frames from an LDU1 or LDU2
//! body dibit stream.
//!
//! # Architecture
//!
//! Sits BETWEEN the existing `ControlChannelDecoder` framer and the
//! Phase 7D vocoder. The decoder framer feeds it the 807-dibit body
//! of an LDU1/LDU2 (raw, including in-body status dibits at the
//! universal `is_body_status_dibit` positions). This module:
//!
//!   1. **Strips body status dibits** (those at body raw positions
//!      `{13, 49, 85, 121, ..., 13 + 36*k}`). Status dibits carry
//!      network-status information for the channel and are NOT part
//!      of the voice payload -- they must be removed before applying
//!      the SDRTrunk-documented IMBE bit positions.
//!   2. **Packs the resulting 784 data dibits** into a `BitVec` of
//!      1568 bits, MSB-first big-endian within each dibit. Dibit
//!      `0bAB` becomes bits `[A, B]` in the bit string. This matches
//!      SDRTrunk's `BinaryMessage` ordering.
//!   3. **Extracts 9 raw 144-bit IMBE frames** at the fixed bit
//!      positions `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]`
//!      from `LDUMessage.java:32-40`.
//!   4. Each frame becomes an `ImbeFrameRaw { bits: [u8; 18] }` --
//!      144 bits packed into 18 bytes, MSB-first within each byte.
//!      This is exactly the format both JMBE (`P25P1AudioModule.java:134-149`)
//!      and mbelib expect.
//!
//! # What this module does NOT do
//!
//! - **No FEC**: the 144 raw bits per IMBE frame include the
//!   Golay(23,12,7) + Hamming(15,11,3) + derand internals that
//!   the vocoder library handles internally. SDRTrunk hands the
//!   raw 144 bits straight to JMBE; we mirror that for mbelib in
//!   Phase 7D.
//! - **No LC / ESS / LSD parsing**: those bits live at other
//!   positions in the LDU body and would feed Phase 7C.2 / 7B work
//!   (encryption flag from LDU2 ESS for late-entry, end-of-call LC
//!   from LDU1, etc). The encryption flag for active grants comes
//!   from the control-channel `GroupVoiceChannelGrant` service
//!   options byte instead -- see
//!   `reference_p25_encryption_flag_from_control_channel.md` memory.
//! - **No vocoder**: that's Phase 7D.
//!
//! # Reference
//!
//! - SDRTrunk `LDUMessage.java:32-40`, `LDU1Message.java`,
//!   `LDU2Message.java` (all upstream-verified, no fork modifications)
//! - `reference_p25_ldu_bit_layout.md` memory for the full bit
//!   layout including LC / ESS / LSD positions

use super::types::{is_body_status_dibit, DataUnit};

/// Raw 144-bit IMBE voice frame, ready for direct vocoder input.
///
/// Bits are packed MSB-first within each byte: byte 0 bit 7 is the
/// first IMBE bit on-air, byte 0 bit 0 is the 8th, byte 1 bit 7 is
/// the 9th, etc. 144 bits / 8 = 18 bytes exactly. Both mbelib and
/// JMBE accept this format directly.
#[derive(Debug, Clone, Copy)]
pub struct ImbeFrameRaw {
    pub bits: [u8; 18],
}

impl ImbeFrameRaw {
    pub const BITS: usize = 144;
    pub const BYTES: usize = 18;

    /// All-zero placeholder. Used as a default when extraction fails
    /// (so the caller still gets 9 entries per LDU and can dispatch
    /// per-frame validity flags separately).
    pub const ZERO: Self = ImbeFrameRaw { bits: [0u8; 18] };
}

/// Bit positions of the 9 IMBE frames within an LDU body of 1568
/// data bits (after status dibits have been stripped).
///
/// From SDRTrunk `LDUMessage.java:32-40`:
///
/// ```text
/// IMBE_FRAME_1 = 0,     IMBE_FRAME_2 = 144,   IMBE_FRAME_3 = 328,
/// IMBE_FRAME_4 = 512,   IMBE_FRAME_5 = 696,   IMBE_FRAME_6 = 880,
/// IMBE_FRAME_7 = 1064,  IMBE_FRAME_8 = 1248,  IMBE_FRAME_9 = 1424
/// ```
///
/// Each frame is exactly 144 bits (`IMBE_FRAME_BITS`). The
/// non-uniform spacing comes from LC/ESS/LSD chunks interleaved
/// between consecutive frames -- see
/// `reference_p25_ldu_bit_layout.md` for the full layout.
pub const IMBE_FRAME_BIT_POSITIONS: [usize; 9] = [
    0, 144, 328, 512, 696, 880, 1064, 1248, 1424,
];

/// LDU1 / LDU2 body data length in bits, after status dibit strip.
/// Equals `DataUnit::Ldu1.data_dibits() * 2` = 784 * 2 = 1568.
pub const LDU_DATA_BITS: usize = 1568;

/// LDU1 / LDU2 body raw length in dibits, including body status
/// dibits. Equals `DataUnit::Ldu1.length_dibits()` = 807.
pub const LDU_RAW_DIBITS: usize = 807;

/// Strips body status dibits from a raw body dibit slice.
///
/// Returns a new `Vec<u8>` containing only the data dibits, in the
/// same order. The output length is `body_raw.len() - n_status_dibits`
/// where `n_status_dibits = floor((body_raw.len() - 14) / 36) + 1`
/// (clamped to zero if body_raw.len() < 14).
///
/// This is the universal body status pattern -- it works for any
/// P25 Phase 1 data unit (HDU, TDU, LDU1, LDU2, TSDU, TDU_LC).
pub fn strip_body_status_dibits(body_raw: &[u8]) -> Vec<u8> {
    body_raw
        .iter()
        .enumerate()
        .filter_map(|(pos, &d)| {
            if is_body_status_dibit(pos) {
                None
            } else {
                Some(d)
            }
        })
        .collect()
}

/// Packs a sequence of dibits into a bit string, MSB-first within
/// each dibit. Dibit `0bAB` (with A as the MSB) becomes bits
/// `[A, B]` in the output. The output is a `Vec<bool>` indexed by
/// bit position (bit 0 = first bit on-air = first dibit's MSB).
///
/// We use a `Vec<bool>` rather than a `BitVec` to keep the
/// dependency tree minimal -- the LDU pack/unpack happens once per
/// LDU (~140 ms) so the per-bit overhead is negligible vs. the
/// 144 IMBE bits per frame this enables.
pub fn dibits_to_bits(dibits: &[u8]) -> Vec<bool> {
    let mut bits = Vec::with_capacity(dibits.len() * 2);
    for &d in dibits {
        // MSB first: bit 1 of dibit, then bit 0
        bits.push((d & 0b10) != 0);
        bits.push((d & 0b01) != 0);
    }
    bits
}

/// Extracts the 9 raw 144-bit IMBE frames from an LDU body raw dibit
/// slice (807 dibits, including body status dibits).
///
/// Returns `None` if the input length doesn't match `LDU_RAW_DIBITS`.
/// On success, returns 9 `ImbeFrameRaw` entries in transmission
/// order (frame 0 = first IMBE in the LDU = oldest 20 ms of audio,
/// frame 8 = last = newest).
///
/// **Each LDU represents 9 * 20 ms = 180 ms of audio.** At the P25
/// Phase 1 voice channel rate of one LDU per ~140 ms (LDU1 + LDU2
/// alternating), the IMBE frame stream is continuous: 50
/// frames/second per active call.
pub fn extract_imbe_frames(body_raw: &[u8]) -> Option<[ImbeFrameRaw; 9]> {
    if body_raw.len() != LDU_RAW_DIBITS {
        return None;
    }
    // Strip status dibits -> 784 data dibits = 1568 data bits.
    let data_dibits = strip_body_status_dibits(body_raw);
    debug_assert_eq!(data_dibits.len(), DataUnit::Ldu1.data_dibits());
    let bits = dibits_to_bits(&data_dibits);
    debug_assert_eq!(bits.len(), LDU_DATA_BITS);

    let mut frames = [ImbeFrameRaw::ZERO; 9];
    for (i, &start) in IMBE_FRAME_BIT_POSITIONS.iter().enumerate() {
        for byte_idx in 0..ImbeFrameRaw::BYTES {
            let mut byte: u8 = 0;
            for bit_in_byte in 0..8 {
                let abs_bit = start + byte_idx * 8 + bit_in_byte;
                if bits[abs_bit] {
                    // MSB-first within each byte
                    byte |= 1 << (7 - bit_in_byte);
                }
            }
            frames[i].bits[byte_idx] = byte;
        }
    }
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 807 dibits with status dibits stripped -> 784 data dibits.
    /// Verifies the strip helper against the SDRTrunk-documented
    /// status positions.
    #[test]
    fn strip_807_dibits_yields_784() {
        let raw: Vec<u8> = (0..807).map(|i| (i & 0x03) as u8).collect();
        let stripped = strip_body_status_dibits(&raw);
        assert_eq!(stripped.len(), 784);
        // Verify the first few removed positions are exactly the
        // body status positions {13, 49, 85, ...}.
        let removed: Vec<usize> = (0..807)
            .filter(|p| is_body_status_dibit(*p))
            .collect();
        assert_eq!(removed[0], 13);
        assert_eq!(removed[1], 49);
        assert_eq!(removed[2], 85);
        assert_eq!(removed[3], 121);
        assert_eq!(removed.len(), 23);
    }

    /// dibits_to_bits packs dibit MSB first: 0b10 -> [true, false],
    /// 0b01 -> [false, true], 0b11 -> [true, true], 0b00 -> [false, false].
    #[test]
    fn dibits_to_bits_msb_first() {
        let dibits = vec![0b10, 0b01, 0b11, 0b00];
        let bits = dibits_to_bits(&dibits);
        assert_eq!(
            bits,
            vec![true, false, false, true, true, true, false, false]
        );
    }

    /// End-to-end: extract IMBE frames from a body where every dibit
    /// is uniformly 0b11 (all-ones). After strip + bit-pack we expect
    /// 1568 ones, so every IMBE frame should be 18 bytes of 0xFF.
    #[test]
    fn extract_all_ones_body_yields_all_ones_frames() {
        let raw = vec![0b11_u8; LDU_RAW_DIBITS];
        let frames = extract_imbe_frames(&raw).expect("len matches");
        for (i, frame) in frames.iter().enumerate() {
            for (j, &b) in frame.bits.iter().enumerate() {
                assert_eq!(b, 0xFF, "frame {} byte {}", i, j);
            }
        }
    }

    /// Place a known marker bit at IMBE frame 0 bit 0 (the first
    /// MSB of the LDU body data) and verify extract_imbe_frames
    /// finds it at frame 0 byte 0 bit 7.
    #[test]
    fn extract_first_bit_position() {
        // Build a body of all-zeros, then set body data dibit 0
        // to 0b10 (bit 0 = '1', bit 1 = '0'). After strip and pack,
        // bit 0 of the bit-string should be true and bit 1 false.
        // Frame 0 starts at bit 0, so byte 0 bit 7 is the marker
        // and byte 0 bit 6 is zero.
        let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
        raw[0] = 0b10; // body dibit 0
        let frames = extract_imbe_frames(&raw).expect("len matches");
        assert_eq!(
            frames[0].bits[0], 0x80,
            "frame 0 byte 0 should have bit 7 set (= IMBE bit 0)"
        );
        for &b in &frames[0].bits[1..] {
            assert_eq!(b, 0x00);
        }
    }

    /// Frame 1 starts at LDU data bit 144. Set the dibit at body
    /// data position 72 (= bit 144) to 0b11, leave everything else
    /// zero, and verify frame 1 byte 0 == 0xC0.
    ///
    /// Note: body data position 72 is NOT body raw position 72,
    /// because status dibits at body raw positions {13, 49} have
    /// already been removed by then. body data 72 = body raw 74
    /// (skipping over status at 13 and 49 which are <72 raw, but
    /// remember body data position 72 = body raw position
    /// 72 + (status before 72)). Status dibits with raw pos < 74:
    /// {13, 49} = 2 dibits. So body raw 74 -> body data 72.
    #[test]
    fn extract_frame_1_marker_bit() {
        let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
        // body data dibit 72 -> body raw dibit (72 + 2) = 74
        // (we skip raw positions 13 and 49 which are status dibits).
        raw[74] = 0b11;
        let frames = extract_imbe_frames(&raw).expect("len matches");
        // All frame 0 bytes should be zero.
        for &b in &frames[0].bits {
            assert_eq!(b, 0x00);
        }
        // Frame 1 byte 0 should have bits 7 and 6 set = 0xC0.
        assert_eq!(frames[1].bits[0], 0xC0);
        for &b in &frames[1].bits[1..] {
            assert_eq!(b, 0x00);
        }
    }
}

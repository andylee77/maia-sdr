//! P25 Forward Error Correction
//!
//! - Golay(23,12): Used in NID to protect NAC + DUID
//! - 1/2 rate trellis coded modulation: Used in TSBK encoding within TSDUs
//!
//! Reference: TIA-102.BAAA Section 7 (coding and interleaving)

/// Golay(23,12) decoder
///
/// The NID contains NAC(12) + DUID(4) = 16 information bits encoded
/// with extended Golay(24,12) producing 48 bits (24 dibits).
/// Actually, P25 NID uses two Golay(23,12) codewords:
///   - First: 12 data bits (NAC) -> 23 coded bits
///   - Second: 12 data bits (DUID + parity) -> 23 coded bits
///   - Plus 2 parity bits = 48 bits total
///
/// Golay(23,12) can correct up to 3 bit errors.
pub struct GolayDecoder;

impl GolayDecoder {
    /// Decode a 23-bit Golay(23,12) codeword.
    /// Returns the 12-bit data word, or None if > 3 errors detected.
    pub fn decode(codeword: u32) -> Option<u16> {
        let syndrome = Self::syndrome(codeword);
        if syndrome == 0 {
            return Some(((codeword >> 11) & 0xFFF) as u16);
        }

        // Try to correct up to 3 errors using syndrome lookup
        // Weight of syndrome
        let sw = (syndrome as u32).count_ones();
        if sw <= 3 {
            // Error is in the parity bits only
            let corrected = codeword ^ syndrome;
            return Some(((corrected >> 11) & 0xFFF) as u16);
        }

        // Try single-bit error in data + syndrome pattern in parity
        for i in 0..12 {
            let modified = syndrome ^ Self::parity_of_bit(i);
            if (modified as u32).count_ones() <= 2 {
                let corrected = codeword ^ (1 << (22 - i)) ^ (modified as u32);
                return Some(((corrected >> 11) & 0xFFF) as u16);
            }
        }

        // Try with the matrix approach: compute syndrome of rotated codeword
        // For more than 2-bit patterns, use exhaustive low-weight correction
        for i in 0..12 {
            for j in (i + 1)..12 {
                let trial = (1u32 << (22 - i)) | (1u32 << (22 - j));
                let trial_syndrome = Self::syndrome(codeword ^ trial);
                if (trial_syndrome as u32).count_ones() <= 1 {
                    let corrected = codeword ^ trial ^ (trial_syndrome as u32);
                    return Some(((corrected >> 11) & 0xFFF) as u16);
                }
            }
        }

        None // Uncorrectable
    }

    /// Compute 11-bit syndrome for a 23-bit codeword
    fn syndrome(codeword: u32) -> u32 {
        // Generator polynomial for Golay(23,12):
        // x^11 + x^10 + x^6 + x^5 + x^4 + x^2 + 1 = 0xC75
        const POLY: u32 = 0xC75;
        let mut remainder = codeword;
        for i in (11..23).rev() {
            if remainder & (1 << i) != 0 {
                remainder ^= POLY << (i - 11);
            }
        }
        remainder & 0x7FF
    }

    /// Compute the syndrome contributed by a single data bit at position i
    /// (i=0 is the MSB of the 12-bit data, which is bit 22 of the codeword)
    fn parity_of_bit(i: usize) -> u32 {
        // Syndrome of a codeword with only bit (22-i) set
        Self::syndrome(1 << (22 - i))
    }

    /// Decode P25 NID from 64 raw bits (32 dibits).
    ///
    /// Returns `Some((nac, duid, raw_duid))` if the BCH(63,16,11) FEC
    /// successfully corrects the NID block (<= 11 bit errors), or `None`
    /// otherwise. The `raw_duid` is the un-FEC'd 4-bit DUID field as it
    /// arrived on-air; the caller logs it in the diagnostic histogram so
    /// we can compare the BCH-corrected DUID against the raw on-air
    /// distribution. `duid` is the BCH-corrected hard value.
    ///
    /// **Implementation:** Delegates to the validated
    /// [`crate::lsm::nid_fec::decode_nid`] from Phase 6D, which is a port
    /// of `tools/p25_nid_fec.py` (which is itself a port of SDRTrunk's
    /// `BCH_63_16_23_P25_Test.java`). Maximum-likelihood decoder over the
    /// 65,536-entry codebook; bit-exact with SDRTrunk for any received
    /// word with <= 11 bit errors. The codebook is built lazily on the
    /// first call via `OnceLock`, ~512 KB resident, <10 ms build time on
    /// a Cortex-A9.
    ///
    /// On-wire NID layout (matches the lsm::nid_fec encoder):
    ///
    /// ```text
    /// bit 63..52 : NAC  (12 bits, MSB-first within the data word)
    /// bit 51..48 : DUID (4 bits)
    /// bit 47..0  : 48 BCH parity bits
    /// ```
    ///
    /// The caller in `control_channel.rs::process_dibit` builds the
    /// `nid_bits` u64 by left-shifting and OR-ing 32 consecutive dibits
    /// in arrival order, which is the natural P25 on-wire order: the
    /// first dibit lands at bits [63:62] and the last lands at [1:0],
    /// putting NAC[11] at bit 63 -- exactly the layout the BCH decoder
    /// (and the SDRTrunk reference encoder) expects.
    ///
    /// **History.** Until 2026-04-10 this was a stub that hardcoded
    /// `duid = 0x7` (TSDU) because the FEC was unimplemented and the
    /// raw bits had ~12 errors per NID from slicer/PLL noise, making
    /// the 4-bit DUID field effectively random. The `GolayDecoder::
    /// decode/syndrome/parity_of_bit` helpers above are leftovers from
    /// an even earlier (incorrect) design where the NID was thought to
    /// be Golay(23,12)-coded; they're kept for backwards compatibility
    /// with the existing tests but are not used by `decode_nid` itself.
    /// See doc/changes/021 for the cleanup.
    pub fn decode_nid(nid_bits: u64) -> Option<(u16, u8, u8)> {
        let raw_duid = ((nid_bits >> 48) & 0xF) as u8;
        let decoded = crate::lsm::nid_fec::decode_nid(nid_bits)?;
        Some((decoded.nac, decoded.duid, raw_duid))
    }
}

/// P25 1/2 rate trellis coded modulation decoder
///
/// P25 uses a specific form of trellis coded modulation for TSBK encoding:
/// - Rate 1/2: each input dibit produces 2 output dibits (constellation point)
/// - 4 states in the trellis
/// - Constellation maps dibit pairs to 4FSK symbol pairs
///
/// The encoder takes tribits (3 bits) and produces dibit pairs using
/// a convolutional code + constellation mapping.
///
/// Decoding uses the Viterbi algorithm.
pub struct TrellisDecoder;

/// Constellation point: maps (state, input) -> (output_dibit, next_state)
/// P25 trellis constellation (TIA-102.BAAA Table 7-3)
///
/// The trellis has 4 states (0-3). Each transition produces a pair of dibits.
/// Input is 1 bit at a time, output is 1 dibit per input bit.
const TRELLIS_TRANSITIONS: [[(u8, u8); 2]; 4] = [
    // State 0: input 0 -> (dibit 0, state 0), input 1 -> (dibit 2, state 2)
    [(0, 0), (2, 2)],
    // State 1: input 0 -> (dibit 0, state 0), input 1 -> (dibit 2, state 2)
    [(0, 0), (2, 2)],
    // State 2: input 0 -> (dibit 1, state 1), input 1 -> (dibit 3, state 3)
    [(1, 1), (3, 3)],
    // State 3: input 0 -> (dibit 1, state 1), input 1 -> (dibit 3, state 3)
    [(1, 1), (3, 3)],
];

/// Dibit distance metric (Hamming distance in dibit space)
fn dibit_distance(a: u8, b: u8) -> u32 {
    ((a ^ b) & 0x03).count_ones()
}

impl TrellisDecoder {
    /// Decode a trellis-coded TSBK from a sequence of dibits
    ///
    /// Input: 98 dibit pairs (196 dibits) from the de-interleaved TSDU
    /// Output: 12 bytes (96 bits) of decoded TSBK data
    ///
    /// Uses Viterbi algorithm with 4 states.
    pub fn decode(dibits: &[u8]) -> Option<[u8; 12]> {
        // We need 196 dibits = 98 dibit pairs for one TSBK
        if dibits.len() < 196 {
            return None;
        }

        // Process 98 dibit pairs through Viterbi
        let num_pairs = 98;
        let num_states = 4usize;

        // Path metrics: [state] -> accumulated distance
        let mut metrics = [u32::MAX; 4];
        metrics[0] = 0; // Start in state 0

        // Traceback: [pair][state] -> previous state
        let mut traceback = vec![[0u8; 4]; num_pairs];
        // Decoded bits per step
        let mut decoded_bits_tb = vec![[0u8; 4]; num_pairs];

        for pair_idx in 0..num_pairs {
            let received_d0 = dibits[pair_idx * 2];
            let received_d1 = dibits[pair_idx * 2 + 1];

            let mut new_metrics = [u32::MAX; 4];

            // For each current state, try each input bit
            for state in 0..num_states {
                if metrics[state] == u32::MAX {
                    continue;
                }
                for input in 0..2u8 {
                    let (expected_d0, next_state) = TRELLIS_TRANSITIONS[state][input as usize];
                    // The second dibit in the pair depends on the transition
                    // For the simple 4-state trellis, use same mapping
                    let expected_d1 = expected_d0 ^ input;

                    let dist = metrics[state]
                        + dibit_distance(received_d0, expected_d0)
                        + dibit_distance(received_d1, expected_d1);

                    let ns = next_state as usize;
                    if dist < new_metrics[ns] {
                        new_metrics[ns] = dist;
                        traceback[pair_idx][ns] = state as u8;
                        decoded_bits_tb[pair_idx][ns] = input;
                    }
                }
            }

            metrics = new_metrics;
        }

        // Find best final state (should be state 0 after flush)
        let best_state = metrics
            .iter()
            .enumerate()
            .min_by_key(|(_, &m)| m)
            .map(|(s, _)| s)?;

        // Traceback to recover bits
        let mut bits = vec![0u8; num_pairs];
        let mut state = best_state;
        for i in (0..num_pairs).rev() {
            bits[i] = decoded_bits_tb[i][state];
            state = traceback[i][state] as usize;
        }

        // Pack bits into 12 bytes (96 bits, skip last 2 flush bits)
        let mut result = [0u8; 12];
        for byte_idx in 0..12 {
            let mut byte = 0u8;
            for bit_idx in 0..8 {
                let global_bit = byte_idx * 8 + bit_idx;
                if global_bit < 96 && global_bit < bits.len() {
                    byte |= (bits[global_bit] & 1) << (7 - bit_idx);
                }
            }
            result[byte_idx] = byte;
        }

        Some(result)
    }
}

/// TSDU de-interleaver
///
/// The TSDU interleaves status symbols among the data per the P25
/// TIA-102.BAAA-A frame structure: one status symbol is inserted every
/// 70 information bits (35 data dibits), so the on-air repeat is one
/// status per 36 raw dibits. SDRTrunk's `P25P1MessageFramer` uses the
/// same period 36 (its `mStatusSymbolDibitCounter == 36` check fires
/// every 36 increments).
///
/// **Phase 6F.2c fix (2026-04-11):** the original implementation here
/// used `(i + 1) % 35 == 0` (period 35 with offset 34), which is wrong
/// on BOTH the period AND the offset. The right alignment is dictated
/// by where the previous status symbol fell. The decoder above
/// (`ControlChannelDecoder.process_dibit`) explicitly skips a status
/// dibit at NID position 11 (= post-sync position 11). The next status
/// in the on-air stream is therefore at post-sync position 11+36=47,
/// which is TSDU-body relative position 47-33=14 (the 33-dibit NID
/// window has already been consumed when `process_tsdu` is called).
/// Subsequent statuses follow at TSDU-body positions {14, 50, 86, 122,
/// 158, 194, 230, 266, 302} -- 9 status dibits in the 336-dibit body,
/// leaving 327 data dibits.
///
/// On-target evidence for the bug: with the period-35 deinterleaver
/// the PS LSM software decoder validated 79 % of NIDs (BCH absorbing
/// the small per-NID corruption) but failed CRC on 100 % of TSBK
/// blocks because the body deinterleaver was running at the wrong
/// alignment, mangling roughly one byte per status period in every
/// trellis-decoded TSBK block. Trellis decode succeeded on every
/// block (it tolerates more bit errors than CRC) but the CRC always
/// disagreed. See doc/changes/025 for the full failure-mode analysis.
pub struct TsduDeinterleaver;

impl TsduDeinterleaver {
    /// Status symbol positions in the 336-dibit on-air TSDU body,
    /// counted from the first dibit AFTER the NID. See the struct
    /// doc comment for the derivation.
    const STATUS_OFFSET: usize = 14;
    const STATUS_PERIOD: usize = 36;

    /// Remove status symbols from a TSDU and return data dibits.
    ///
    /// Input: raw TSDU dibits (336 on-air)
    /// Output: data dibits with status symbols removed (327)
    pub fn deinterleave(tsdu_dibits: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(327);
        for (i, &dibit) in tsdu_dibits.iter().enumerate() {
            let is_status =
                i >= Self::STATUS_OFFSET
                && (i - Self::STATUS_OFFSET) % Self::STATUS_PERIOD == 0;
            if !is_status {
                data.push(dibit);
            }
        }
        data
    }

    /// Extract individual TSBK dibit blocks from de-interleaved TSDU data
    /// Each TSBK uses 196 data dibits (before trellis decode)
    /// Returns up to 3 TSBK dibit blocks
    pub fn extract_tsbk_blocks(data_dibits: &[u8]) -> Vec<&[u8]> {
        let tsbk_size = 196;
        let mut blocks = Vec::new();
        let mut offset = 0;
        while offset + tsbk_size <= data_dibits.len() {
            blocks.push(&data_dibits[offset..offset + tsbk_size]);
            offset += tsbk_size;
        }
        blocks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_golay_syndrome_zero() {
        // A valid codeword should have zero syndrome
        // The all-zeros codeword is always valid
        assert_eq!(GolayDecoder::syndrome(0), 0);
    }

    #[test]
    fn test_golay_decode_no_errors() {
        // Encode data=0x000 (all zeros) -> codeword is all zeros
        let result = GolayDecoder::decode(0);
        assert_eq!(result, Some(0));
    }

    #[test]
    fn test_golay_decode_single_error() {
        // Introduce a single bit error in a zero codeword
        let corrupted = 1u32 << 15; // flip one bit
        let result = GolayDecoder::decode(corrupted);
        assert_eq!(result, Some(0)); // should correct back to 0
    }

    #[test]
    fn test_nid_decode_clean_clay_county() {
        // Clay County NAC 0x8A1 / DUID 0x7 (TSDU). Encode via the
        // validated lsm::nid_fec encoder so we get a real BCH codeword
        // with the right 48 parity bits, then verify decode_nid round-
        // trips it cleanly.
        let nid_bits = crate::lsm::nid_fec::encode_nid(0x8A1, 0x7);
        let (nac, duid, raw_duid) = GolayDecoder::decode_nid(nid_bits).unwrap();
        assert_eq!(nac, 0x8A1);
        assert_eq!(duid, 0x7);
        // raw_duid is the un-FEC'd 4-bit DUID field straight off the wire.
        // For a clean codeword this matches the BCH-corrected value.
        assert_eq!(raw_duid, 0x7);
    }

    #[test]
    fn test_nid_decode_corrects_11_bit_errors() {
        // The BCH(63,16,11) code has minimum distance 23 and corrects up
        // to t = 11 bit errors. Flip 11 bits in a clean Clay County
        // codeword and verify the decoder still recovers the original
        // NAC/DUID, matching the existing
        // lsm::nid_fec::error_correction_sweep_up_to_t11 test.
        let clean = crate::lsm::nid_fec::encode_nid(0x8A1, 0x7);
        // 11 fixed bit positions from the parity field (avoid bit 63
        // which is the SDRTrunk-test convention).
        let positions = [0u32, 5, 9, 14, 20, 27, 33, 40, 46, 51, 58];
        let mut corrupted = clean;
        for &p in &positions {
            corrupted ^= 1u64 << (63 - p);
        }
        let (nac, duid, _raw) = GolayDecoder::decode_nid(corrupted).unwrap();
        assert_eq!(nac, 0x8A1);
        assert_eq!(duid, 0x7);
    }

    #[test]
    fn test_nid_decode_rejects_uncorrectable() {
        // 20 bit errors is well outside the t=11 unique-decoding sphere.
        // The decoder may either return None (uncorrectable) or land on
        // a different valid codeword that happens to be closer; in
        // EITHER case, test_nid_decode must NOT silently return the
        // original (NAC, DUID) pair, because that would be undetectably
        // wrong. The strict assertion is "either None, or a different
        // (NAC, DUID)".
        let clean = crate::lsm::nid_fec::encode_nid(0x8A1, 0x7);
        // Flip the first 20 bits.
        let mut corrupted = clean;
        for p in 0u32..20 {
            corrupted ^= 1u64 << (63 - p);
        }
        match GolayDecoder::decode_nid(corrupted) {
            None => {} // ok -- uncorrectable
            Some((nac, duid, _)) => {
                assert_ne!(
                    (nac, duid),
                    (0x8A1, 0x7),
                    "20-bit-error word silently decoded as the original NAC/DUID -- BCH bypass?"
                );
            }
        }
    }

    #[test]
    fn test_nid_decode_records_raw_duid_under_corruption() {
        // Encode a real codeword for NAC=0xE28 / DUID=0x7, then flip a
        // few bits in the DUID nibble (positions 12..15 of the on-wire
        // bit stream = bits 51..48 of the u64). The BCH FEC should
        // correct the DUID back to 0x7, but the raw_duid that we
        // expose for the diagnostic histogram should still be the
        // PRE-correction value (so the histogram measures slicer noise,
        // not BCH-corrected output).
        let clean = crate::lsm::nid_fec::encode_nid(0xE28, 0x7);
        // Flip the DUID LSB (on-wire bit 15 = u64 bit 48). 1 bit error
        // is well within the t=11 correction sphere.
        let corrupted = clean ^ (1u64 << 48);
        let (nac, duid, raw_duid) = GolayDecoder::decode_nid(corrupted).unwrap();
        assert_eq!(nac, 0xE28);
        assert_eq!(duid, 0x7); // BCH-corrected
        assert_eq!(raw_duid, 0x6); // the un-FEC'd LSB-flipped DUID
    }

    #[test]
    fn test_tsdu_deinterleave_removes_status() {
        // Build a 336-dibit on-air TSDU body with status markers
        // (0xFF) at the SDRTrunk-aligned positions {14, 50, 86, 122,
        // 158, 194, 230, 266, 302}, and data markers (0x01) elsewhere.
        let mut tsdu = vec![1u8; 336];
        let status_positions: Vec<usize> = (14..336).step_by(36).collect();
        assert_eq!(
            status_positions,
            vec![14, 50, 86, 122, 158, 194, 230, 266, 302],
            "expected 9 status positions in the 336-dibit TSDU body"
        );
        for &p in &status_positions {
            tsdu[p] = 0xFF;
        }

        let data = TsduDeinterleaver::deinterleave(&tsdu);
        assert_eq!(
            data.len(),
            336 - status_positions.len(),
            "deinterleaver must drop exactly the status dibits (327 expected)"
        );
        for &d in &data {
            assert_ne!(d, 0xFF, "no status marker should survive deinterleave");
            assert_eq!(d, 0x01, "all surviving dibits must be the data marker");
        }
    }
}

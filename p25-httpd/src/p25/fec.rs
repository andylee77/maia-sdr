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

/// P25 1/2 rate Trellis Coded Modulation (TCM) constellation table.
///
/// **TIA-102 BAAA Table 7-2** transition matrix, port of SDRTrunk's
/// `P25_1_2_Node.TRANSITION_MATRIX`. Indexed as
/// `TRANSITION_MATRIX[prev_input][curr_input] = transmitted_4bit_value`.
/// Both prev_input and curr_input are 2-bit values (0..3), giving a
/// 4-state trellis with 4 inputs per state.
///
/// The encoder works as: for each pair of bits to transmit, look up the
/// 4-bit constellation point using the previous bit pair as state and the
/// current bit pair as the input. Transmit 4 bits per 2-bit input symbol
/// = rate 1/2.
const TRANSITION_MATRIX: [[u8; 4]; 4] = [
    [2, 12, 1, 15],
    [14, 0, 13, 3],
    [9, 7, 10, 4],
    [5, 11, 6, 8],
];

/// Hamming distance lookup for 4-bit values (popcount of `a ^ b`).
const HAMMING_4BIT: [u8; 16] = [
    0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4,
];

impl TrellisDecoder {
    /// Decode a P25 1/2 rate trellis-coded message.
    ///
    /// **Input format:** 196 bits = 49 four-bit constellation symbols,
    /// already de-interleaved and stripped of status dibits and null
    /// padding. Each bit comes from `data_dibits` packed MSB-first per
    /// dibit (`bit1` then `bit2`), exactly as SDRTrunk's
    /// `P25P1MessageAssembler` packs them via `mMessage.add(getBit1(),
    /// getBit2())` -- which means dibit value 0 (binary 00) → bits 00,
    /// dibit 1 (01) → 01, dibit 2 (10) → 10, dibit 3 (11) → 11. So
    /// reading the input as packed bits is the same as reading the
    /// input dibits as 2-bit values in arrival order. The 49 nibbles
    /// are then formed by grouping every 4 bits = every 2 dibits.
    ///
    /// **Output format:** 12 bytes (96 bits) of decoded TSBK data,
    /// recovered as the encoder's data-input sequence: 48 two-bit
    /// symbols spread across 49 transmitted nibbles. The encoder
    /// starts in state 0 (implicit, not transmitted), runs 48 data
    /// transitions producing nibbles[0..48], then flushes with one
    /// final input=0 transition producing nibble[48]. We discard the
    /// flush input and keep the 48 data inputs = 96 bits = 12 bytes.
    ///
    /// **Algorithm:** Viterbi over the 4-state TIA-102 BAAA Table 7-2
    /// trellis. Port of SDRTrunk `ViterbiDecoder` + `P25_1_2_Node`.
    /// Returns `Some(bytes)` even on uncorrectable errors -- the caller
    /// is expected to verify with the TSBK CRC.
    pub fn decode(dibits: &[u8]) -> Option<[u8; 12]> {
        // We need 98 dibits = 196 bits = 49 nibbles (start + 47 data + flush)
        if dibits.len() < 98 {
            return None;
        }

        // Pack the first 98 dibits into 49 four-bit nibbles. Each dibit
        // is 2 bits (MSB first within the pair: bit1 in [1] and bit2 in
        // [0] of the 2-bit value the slicer produces). Group two dibits
        // into one nibble: nibble = (dibit_a << 2) | dibit_b.
        let mut nibbles = [0u8; 49];
        for n in 0..49 {
            let a = dibits[n * 2] & 0x03;
            let b = dibits[n * 2 + 1] & 0x03;
            nibbles[n] = (a << 2) | b;
        }

        // Viterbi over 4 states. The encoder starts in state 0 (input
        // value 0) and the receiver knows this, so we initialise the
        // path metric for state 0 to 0 and all others to "infinity".
        const NUM_STATES: usize = 4;
        const NUM_STEPS: usize = 49;
        let mut metrics = [u32::MAX; NUM_STATES];
        metrics[0] = 0;

        // Traceback: at each step `t`, for each surviving state `s`,
        // record which previous state we came from. This lets us walk
        // backwards from the best final state to recover the input
        // sequence.
        let mut traceback = [[0u8; NUM_STATES]; NUM_STEPS];

        for (t, &recv) in nibbles.iter().enumerate() {
            let mut new_metrics = [u32::MAX; NUM_STATES];
            for prev in 0..NUM_STATES {
                if metrics[prev] == u32::MAX {
                    continue;
                }
                for curr in 0..NUM_STATES {
                    let expected = TRANSITION_MATRIX[prev][curr];
                    let err = HAMMING_4BIT[(expected ^ recv) as usize] as u32;
                    let cand = metrics[prev] + err;
                    if cand < new_metrics[curr] {
                        new_metrics[curr] = cand;
                        traceback[t][curr] = prev as u8;
                    }
                }
            }
            metrics = new_metrics;
        }

        // The encoder flushes with input value 0 at the very end, so
        // the final state is guaranteed to be 0. Even if the lowest-
        // metric state is something else, we choose 0 to be consistent
        // with the encoder and let the CRC catch any disagreement.
        let mut state = 0usize;

        // Walk backwards through the traceback to recover the input
        // value at each step. The input value at step t IS the state
        // we transitioned INTO -- because the encoder maps `curr` to
        // the next state directly (each row of TRANSITION_MATRIX is
        // indexed by the previous-step input which is the current
        // state, and the column is the current input which becomes
        // the next state).
        let mut inputs = [0u8; NUM_STEPS];
        for t in (0..NUM_STEPS).rev() {
            inputs[t] = state as u8;
            state = traceback[t][state] as usize;
        }

        // The encoder runs 48 data transitions (inputs[0..48]) followed
        // by 1 flush transition (inputs[48] = 0). We keep the 48 data
        // inputs and drop the flush, giving 48 × 2 = 96 bits = 12
        // bytes. Lay out MSB-first per byte to match SDRTrunk's
        // `BinaryMessage` ordering: the first decoded bit (high bit of
        // inputs[0]) lands in byte[0] bit 7.
        let mut bits = [0u8; 96];
        for n in 0..48 {
            let two_bits = inputs[n] & 0x03;
            bits[n * 2] = (two_bits >> 1) & 0x01;
            bits[n * 2 + 1] = two_bits & 0x01;
        }

        let mut result = [0u8; 12];
        for byte_idx in 0..12 {
            let mut byte = 0u8;
            for bit_in_byte in 0..8 {
                byte |= bits[byte_idx * 8 + bit_in_byte] << (7 - bit_in_byte);
            }
            result[byte_idx] = byte;
        }

        Some(result)
    }
}

/// TSDU de-interleaver
///
/// The TSDU body has 3 status dibits embedded in it at on-air positions
/// {14, 50, 86} (counted from the first dibit AFTER the NID), followed
/// by 21 trailing null padding dibits.
///
/// **Phase 6F.2g (2026-04-11) note:** off-by-one correction from 6F.2f
/// (which had 4 status positions including 122). Trace SDRTrunk's
/// framer carefully: `mStatusSymbolDibitCounter` starts at 21 after
/// `nidDetected()` and increments BEFORE the `== 36` check. The first
/// body dibit takes the counter to 22; the 15th body dibit (raw index
/// 14) takes it to 36 → status drop. Status drops repeat at body
/// indices {14, 50, 86}. After body raw index 121 the message
/// assembler has accumulated 119 non-status dibits and is complete --
/// we never read body index 122 (which WOULD be a 4th status). So:
/// 14 (non-status) + 1 (status) + 35 + 1 + 35 + 1 + 35 = **122** raw
/// on-air body dibits, **3** status drops, 119 non-status dibits, 21
/// trailing nulls → 98 trellis data dibits.
///
/// **Phase 6F.2f (2026-04-11):** total rewrite from the 6F.2c version
/// which was based on a wrong understanding of the TSBK frame (336
/// dibits with 9 status drops -- both numbers way off).
///
/// On-target evidence after 6F.2e (sync threshold 4) and 6F.2f (real
/// trellis + length 123): every TSBK block trellis-decoded successfully
/// but 100 % of CRCs failed -- the residual misalignment from
/// over-counting status dibits by 1 corrupted enough bytes per block
/// to make CRC always fail.
pub struct TsduDeinterleaver;

impl TsduDeinterleaver {
    /// On-air positions of the 3 status dibits inside the 122-dibit
    /// TSDU body (post-NID). Derived from SDRTrunk's framer
    /// `mStatusSymbolDibitCounter` reset-to-21 + period-36 logic; see
    /// the struct doc comment above for the full trace.
    const STATUS_POSITIONS: [usize; 3] = [14, 50, 86];

    /// Number of trailing null padding dibits in the TSBK1 body
    /// (`nullBits = 42 = 21 dibits` per SDRTrunk's data unit table).
    const NULL_DIBITS: usize = 21;

    /// Number of trellis-coded data dibits in one TSBK block:
    /// 196 trellis-encoded bits = 49 four-bit symbols = 98 dibits.
    const TRELLIS_DATA_DIBITS: usize = 98;

    /// Remove status symbols and trailing null padding from a TSDU body.
    ///
    /// **Input:** the 123 raw on-air dibits of one TSBK1 body, taken
    /// from the dibit stream immediately after the 33-dibit NID
    /// window has been consumed.
    ///
    /// **Output:** 98 trellis-coded data dibits, ready to feed to
    /// `TrellisDecoder::decode`.
    pub fn deinterleave(tsdu_dibits: &[u8]) -> Vec<u8> {
        // Step 1: drop the 4 status dibits.
        let mut after_status: Vec<u8> = Vec::with_capacity(
            tsdu_dibits.len().saturating_sub(Self::STATUS_POSITIONS.len()),
        );
        for (i, &dibit) in tsdu_dibits.iter().enumerate() {
            if !Self::STATUS_POSITIONS.contains(&i) {
                after_status.push(dibit);
            }
        }

        // Step 2: drop the 21 trailing null padding dibits, leaving
        // exactly 98 trellis data dibits if the input was 123 dibits.
        let trellis_len = after_status
            .len()
            .saturating_sub(Self::NULL_DIBITS);
        after_status.truncate(trellis_len);

        // Defensive: clamp to 98. Anything longer is from a caller
        // that fed a multi-block TSBK; we only handle TSBK1 right now
        // (see length_dibits in p25/types.rs).
        if after_status.len() > Self::TRELLIS_DATA_DIBITS {
            after_status.truncate(Self::TRELLIS_DATA_DIBITS);
        }
        after_status
    }

    /// Extract trellis-block-sized slices from de-interleaved TSDU data.
    ///
    /// Phase 6F.2f reduced this to a single-block extractor: with the
    /// new TSBK1-only `length_dibits` (123 raw → 98 trellis dibits),
    /// `data_dibits` is exactly one trellis block. Multi-block TSBK
    /// support is a follow-up.
    pub fn extract_tsbk_blocks(data_dibits: &[u8]) -> Vec<&[u8]> {
        let tsbk_size = Self::TRELLIS_DATA_DIBITS;
        let mut blocks = Vec::new();
        if data_dibits.len() >= tsbk_size {
            blocks.push(&data_dibits[..tsbk_size]);
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

    /// Encode 48 two-bit input symbols into 49 four-bit transmitted
    /// symbols using SDRTrunk's TIA-102 BAAA Table 7-2 transition
    /// matrix. Mirrors `P25_1_2_Node.getOutputValue()`. The encoder
    /// starts in implicit state 0, runs 48 data transitions, then
    /// flushes with one input=0 transition for a total of 49 emitted
    /// nibbles = 196 bits = 98 dibits.
    fn trellis_encode(inputs: &[u8; 48]) -> [u8; 98] {
        let mut nibbles = [0u8; 49];
        let mut prev = 0u8;
        for n in 0..48 {
            let curr = inputs[n] & 0x03;
            nibbles[n] = TRANSITION_MATRIX[prev as usize][curr as usize];
            prev = curr;
        }
        // Flush transition: input = 0.
        nibbles[48] = TRANSITION_MATRIX[prev as usize][0];

        // Pack 49 nibbles into 98 dibits (high nibble bits = first
        // dibit, low nibble bits = second dibit).
        let mut dibits = [0u8; 98];
        for n in 0..49 {
            let nib = nibbles[n] & 0x0F;
            dibits[n * 2] = (nib >> 2) & 0x03;
            dibits[n * 2 + 1] = nib & 0x03;
        }
        dibits
    }

    #[test]
    fn test_trellis_decode_clean_roundtrip() {
        // Build a 48-symbol input pattern and round-trip it through
        // encode + decode. The new Viterbi (P25 1/2 rate) should
        // recover the original 12 bytes exactly with zero corrected
        // errors.
        let mut inputs = [0u8; 48];
        for i in 0..48 {
            inputs[i] = ((i * 7 + 1) % 4) as u8;
        }
        let dibits = trellis_encode(&inputs);
        let bytes = TrellisDecoder::decode(&dibits).expect("decode should succeed");

        // Re-pack expected bytes from inputs (48 × 2 bits = 96 bits =
        // 12 bytes), MSB first within each byte.
        let mut expected = [0u8; 12];
        let mut bit_buf = [0u8; 96];
        for n in 0..48 {
            let two = inputs[n] & 0x03;
            bit_buf[n * 2] = (two >> 1) & 0x01;
            bit_buf[n * 2 + 1] = two & 0x01;
        }
        for byte_idx in 0..12 {
            let mut b = 0u8;
            for k in 0..8 {
                b |= bit_buf[byte_idx * 8 + k] << (7 - k);
            }
            expected[byte_idx] = b;
        }

        assert_eq!(
            bytes, expected,
            "trellis decode of clean encoded input must round-trip exactly"
        );
    }

    #[test]
    fn test_trellis_decode_corrects_single_dibit_error() {
        // Same setup as round-trip, but flip 1 bit in the encoded
        // stream. The Viterbi should still recover the original.
        let mut inputs = [0u8; 48];
        for i in 0..48 {
            inputs[i] = ((i * 11 + 2) % 4) as u8;
        }
        let mut dibits = trellis_encode(&inputs);
        // Flip the LSB of dibit 30 (somewhere in the middle).
        dibits[30] ^= 0x01;

        let bytes = TrellisDecoder::decode(&dibits).expect("decode should succeed");

        let mut expected = [0u8; 12];
        let mut bit_buf = [0u8; 96];
        for n in 0..48 {
            let two = inputs[n] & 0x03;
            bit_buf[n * 2] = (two >> 1) & 0x01;
            bit_buf[n * 2 + 1] = two & 0x01;
        }
        for byte_idx in 0..12 {
            let mut b = 0u8;
            for k in 0..8 {
                b |= bit_buf[byte_idx * 8 + k] << (7 - k);
            }
            expected[byte_idx] = b;
        }
        assert_eq!(bytes, expected, "Viterbi should correct a single bit error");
    }

    #[test]
    fn test_tsdu_deinterleave_removes_status_and_nulls() {
        // Build a 122-dibit on-air TSBK1 body:
        //   - 98 trellis data dibits (marker 0x01) interleaved with
        //   - 3 status dibits (marker 0xFE) at body positions 14, 50, 86
        //   - followed by 21 trailing null dibits (marker 0xFD)
        // Net: 98 surviving 0x01 dibits after deinterleave.
        let mut tsdu = vec![0u8; 122];
        // Default fill: data marker.
        for d in tsdu.iter_mut() {
            *d = 0x01;
        }
        // Status markers at the 3 body positions.
        for p in [14usize, 50, 86] {
            tsdu[p] = 0xFE;
        }
        // Null padding: the LAST 21 non-status positions get 0xFD.
        let mut non_status_positions: Vec<usize> = (0..122)
            .filter(|i| !matches!(*i, 14 | 50 | 86))
            .collect();
        let null_positions = non_status_positions.split_off(non_status_positions.len() - 21);
        for p in &null_positions {
            tsdu[*p] = 0xFD;
        }

        let data = TsduDeinterleaver::deinterleave(&tsdu);
        assert_eq!(
            data.len(),
            98,
            "deinterleaver must return exactly 98 trellis data dibits \
             (122 raw - 3 status - 21 null)"
        );
        for (i, &d) in data.iter().enumerate() {
            assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
            assert_ne!(d, 0xFD, "null marker survived at output[{}]", i);
            assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
        }
    }
}

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

/// **TIA-102 BAAA Table 7-7 / SDRTrunk `P25P1Interleave.DATA_DEINTERLEAVE`**
///
/// 196-element bit permutation applied to the trellis-coded message
/// BEFORE feeding it to the Viterbi decoder. The encoder applies the
/// inverse permutation; the receiver runs `out[DEINTERLEAVE[i]] = in[i]`
/// to undo it.
///
/// This table was missing from our pipeline through 6F.2i. Without it
/// the trellis input bits are scrambled relative to the encoder's
/// output and the Viterbi finds garbage paths with high error metrics.
/// Adding this step is the difference between "metric=24, garbage
/// bytes" and "metric=0, valid TSBK bytes" -- this fix lands every
/// known control-channel TSBK on the test target. See
/// doc/changes/025 follow-up notes for the full diagnosis.
pub const DATA_DEINTERLEAVE: [usize; 196] = [
    0, 1, 2, 3, 16, 17, 18, 19, 32, 33, 34, 35, 48, 49, 50, 51,
    64, 65, 66, 67, 80, 81, 82, 83, 96, 97, 98, 99, 112, 113, 114, 115,
    128, 129, 130, 131, 144, 145, 146, 147, 160, 161, 162, 163, 176, 177, 178, 179,
    192, 193, 194, 195, 4, 5, 6, 7, 20, 21, 22, 23, 36, 37, 38, 39,
    52, 53, 54, 55, 68, 69, 70, 71, 84, 85, 86, 87, 100, 101, 102, 103,
    116, 117, 118, 119, 132, 133, 134, 135, 148, 149, 150, 151, 164, 165, 166, 167,
    180, 181, 182, 183, 8, 9, 10, 11, 24, 25, 26, 27, 40, 41, 42, 43,
    56, 57, 58, 59, 72, 73, 74, 75, 88, 89, 90, 91, 104, 105, 106, 107,
    120, 121, 122, 123, 136, 137, 138, 139, 152, 153, 154, 155, 168, 169, 170, 171,
    184, 185, 186, 187, 12, 13, 14, 15, 28, 29, 30, 31, 44, 45, 46, 47,
    60, 61, 62, 63, 76, 77, 78, 79, 92, 93, 94, 95, 108, 109, 110, 111,
    124, 125, 126, 127, 140, 141, 142, 143, 156, 157, 158, 159, 172, 173, 174, 175,
    188, 189, 190, 191,
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

        // **Phase 6F.2j (2026-04-11):** apply the P25 1/2 trellis bit
        // deinterleave (DATA_DEINTERLEAVE) BEFORE forming the trellis
        // nibbles. The encoder permutes the 196 trellis-coded bits
        // before transmission per TIA-102 BAAA Table 7-7; the receiver
        // must un-permute. Without this step the Viterbi sees scrambled
        // input and finds no valid trellis path. With it, clean
        // signals decode to metric=0 and the TSBK CRC validates.

        // Step 1: convert the 98 input dibits into 196 raw bits
        // (interleaved order).
        let mut interleaved_bits = [0u8; 196];
        for n in 0..98 {
            let d = dibits[n] & 0x03;
            interleaved_bits[n * 2] = (d >> 1) & 1;
            interleaved_bits[n * 2 + 1] = d & 1;
        }

        // Step 2: apply the deinterleave permutation -- bit i in the
        // input lands at position DATA_DEINTERLEAVE[i] in the output.
        let mut de_bits = [0u8; 196];
        for i in 0..196 {
            de_bits[DATA_DEINTERLEAVE[i]] = interleaved_bits[i];
        }

        // Step 3: re-pack the deinterleaved bits into 49 four-bit
        // trellis nibbles, MSB-first per nibble (= 2 dibits per nibble
        // with the first dibit in the high 2 bits).
        let mut nibbles = [0u8; 49];
        for n in 0..49 {
            let mut nib = 0u8;
            for k in 0..4 {
                nib = (nib << 1) | de_bits[n * 4 + k];
            }
            nibbles[n] = nib;
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
/// The TSDU body has its status dibits embedded at the period-36 schedule
/// driven by SDRTrunk's `mStatusSymbolDibitCounter` (reset to 21 by
/// `nidDetected()`, hits 36 → drop → reset to 0). The first status drop
/// in the body lands at raw position **13**, and subsequent drops follow
/// at +36 each (49, 85, 121, 157, 193, 229, 265, 301...). After stripping
/// status dibits and trailing null padding, the data is exactly
/// `num_blocks * 98` trellis-coded dibits.
///
/// **Phase 6F.2j (2026-04-11):** TSBK1 single-block path was confirmed
/// against SDRTrunk and decodes the Clay County control channel
/// end-to-end (see doc/changes/026). **Phase 6F.3 (2026-04-11):** added
/// multi-block TSBK2/TSBK3 support per SDRTrunk's
/// `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_{1,2,3}` table:
///
/// | Blocks | Body raw dibits | Body status dibits          | Trail nulls |
/// |--------|----------------:|-----------------------------|------------:|
/// |   1    |             123 | 4 — {13,49,85,121}          |         21  |
/// |   2    |             231 | 7 — {…,157,193,229}         |         28  |
/// |   3    |             303 | 9 — {…,265,301}             |          0  |
///
/// SDRTrunk constants: TSBK1 messageLength=196 bits + nullBits=42, TSBK2
/// messageLength=392 + nullBits=56, TSBK3 messageLength=588 + nullBits=0,
/// statusDibits = 5/8/10 INCLUDING the in-NID status dibit. Each block
/// is 196 trellis-coded bits = 98 dibits, transmitted contiguously after
/// status removal.
pub struct TsduDeinterleaver;

impl TsduDeinterleaver {
    /// Number of trellis-coded data dibits in one TSBK block:
    /// 196 trellis-encoded bits = 49 four-bit symbols = 98 dibits.
    pub const TRELLIS_DATA_DIBITS: usize = 98;

    /// Maximum number of TSBK blocks per TSDU per the SDRTrunk
    /// `P25P1DataUnitID` table (TSBK1, TSBK2, TSBK3).
    pub const MAX_BLOCKS: usize = 3;

    /// Raw on-air body dibit length for each multi-block TSBK extent.
    /// Indexed by `num_blocks - 1`.
    const BODY_DIBITS_PER_BLOCKS: [usize; 3] = [123, 231, 303];

    /// Trailing null-padding dibit count for each multi-block extent.
    /// Per SDRTrunk's `P25P1DataUnitID` table: TSBK1=42 null bits =
    /// 21 dibits, TSBK2=56 null bits = 28 dibits, TSBK3=0.
    const NULL_DIBITS_PER_BLOCKS: [usize; 3] = [21, 28, 0];

    /// On-air status-dibit positions inside the body (post-NID), in
    /// order. Period 36 starting at raw position 13. We pre-compute all
    /// 9 positions and slice to the relevant prefix per block count.
    const STATUS_POSITIONS_ALL: [usize; 9] =
        [13, 49, 85, 121, 157, 193, 229, 265, 301];

    /// Number of body status dibits for each multi-block extent.
    const STATUS_COUNT_PER_BLOCKS: [usize; 3] = [4, 7, 9];

    /// Raw on-air body dibits required to fully assemble `num_blocks`
    /// TSBK blocks (1..=3). Returns `None` for invalid block counts.
    pub fn body_dibits_for_blocks(num_blocks: usize) -> Option<usize> {
        if num_blocks == 0 || num_blocks > Self::MAX_BLOCKS {
            None
        } else {
            Some(Self::BODY_DIBITS_PER_BLOCKS[num_blocks - 1])
        }
    }

    /// Strip status dibits and trailing nulls from a multi-block TSDU
    /// body. Returns `num_blocks * 98` trellis-coded dibits ready to
    /// hand to `TrellisDecoder::decode` block by block.
    ///
    /// **Input:** the `body_dibits_for_blocks(num_blocks)` raw on-air
    /// dibits of the TSDU body (everything after the 33-dibit NID
    /// window).
    ///
    /// **Output:** `num_blocks * 98` deinterleaved trellis dibits,
    /// laid out contiguously: block 0 in `[0..98]`, block 1 in
    /// `[98..196]`, block 2 in `[196..294]`.
    pub fn deinterleave_multi(tsdu_dibits: &[u8], num_blocks: usize) -> Vec<u8> {
        if num_blocks == 0 || num_blocks > Self::MAX_BLOCKS {
            return Vec::new();
        }
        let status_count = Self::STATUS_COUNT_PER_BLOCKS[num_blocks - 1];
        let null_dibits = Self::NULL_DIBITS_PER_BLOCKS[num_blocks - 1];
        let status_positions = &Self::STATUS_POSITIONS_ALL[..status_count];

        // Step 1: drop the status dibits at the known on-air positions.
        let mut after_status: Vec<u8> =
            Vec::with_capacity(tsdu_dibits.len().saturating_sub(status_count));
        for (i, &dibit) in tsdu_dibits.iter().enumerate() {
            if !status_positions.contains(&i) {
                after_status.push(dibit);
            }
        }

        // Step 2: drop the trailing null padding dibits.
        let trellis_len = after_status.len().saturating_sub(null_dibits);
        after_status.truncate(trellis_len);

        // Defensive: clamp to num_blocks * 98 in case the input was
        // longer than expected.
        let max = num_blocks * Self::TRELLIS_DATA_DIBITS;
        if after_status.len() > max {
            after_status.truncate(max);
        }
        after_status
    }
}

/// Encode 48 two-bit input symbols into 49 four-bit transmitted
/// symbols using SDRTrunk's TIA-102 BAAA Table 7-2 transition matrix
/// (`P25_1_2_Node.getOutputValue()`), then apply the encoder-side bit
/// interleave so the output dibits are in on-air order. Inverse of
/// `TrellisDecoder::decode`. Test-only helper used by both `fec` tests
/// and the multi-block TSBK e2e tests in `control_channel`.
#[cfg(test)]
pub(crate) fn trellis_encode_block(inputs: &[u8; 48]) -> [u8; 98] {
    let mut nibbles = [0u8; 49];
    let mut prev = 0u8;
    for n in 0..48 {
        let curr = inputs[n] & 0x03;
        nibbles[n] = TRANSITION_MATRIX[prev as usize][curr as usize];
        prev = curr;
    }
    // Flush transition: input = 0.
    nibbles[48] = TRANSITION_MATRIX[prev as usize][0];

    // Unpack the 49 nibbles into 196 deinterleaved bits.
    let mut de_bits = [0u8; 196];
    for n in 0..49 {
        let nib = nibbles[n] & 0x0F;
        for k in 0..4 {
            de_bits[n * 4 + k] = (nib >> (3 - k)) & 1;
        }
    }

    // Apply the encoder's inverse-deinterleave: bit at position
    // DATA_DEINTERLEAVE[i] in deinterleaved order goes to position i
    // in the on-air order.
    let mut interleaved = [0u8; 196];
    for i in 0..196 {
        interleaved[i] = de_bits[DATA_DEINTERLEAVE[i]];
    }

    // Pack the 196 interleaved bits back into 98 dibits.
    let mut dibits = [0u8; 98];
    for n in 0..98 {
        dibits[n] = (interleaved[n * 2] << 1) | interleaved[n * 2 + 1];
    }
    dibits
}

/// Encode 12 TSBK bytes (96 bits, MSB-first per byte) into 98 on-air
/// dibits via the P25 1/2 trellis. Convenience wrapper around
/// `trellis_encode_block` that handles the bit-to-symbol packing.
#[cfg(test)]
pub(crate) fn trellis_encode_bytes(bytes: &[u8; 12]) -> [u8; 98] {
    let mut inputs = [0u8; 48];
    for n in 0..48 {
        let bit_hi = (bytes[(n * 2) / 8] >> (7 - (n * 2) % 8)) & 1;
        let bit_lo = (bytes[(n * 2 + 1) / 8] >> (7 - (n * 2 + 1) % 8)) & 1;
        inputs[n] = (bit_hi << 1) | bit_lo;
    }
    trellis_encode_block(&inputs)
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
    fn test_trellis_decode_clean_roundtrip() {
        // Build a 48-symbol input pattern and round-trip it through
        // encode + decode. The new Viterbi (P25 1/2 rate) should
        // recover the original 12 bytes exactly with zero corrected
        // errors.
        let mut inputs = [0u8; 48];
        for i in 0..48 {
            inputs[i] = ((i * 7 + 1) % 4) as u8;
        }
        let dibits = trellis_encode_block(&inputs);
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
        let mut dibits = trellis_encode_block(&inputs);
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
        // Build a 123-dibit on-air TSBK1 body:
        //   - 98 trellis data dibits (marker 0x01) interleaved with
        //   - 4 status dibits (marker 0xFE) at body positions 13,49,85,121
        //   - followed by 21 trailing null dibits (marker 0xFD)
        // Net: 98 surviving 0x01 dibits after deinterleave.
        let mut tsdu = vec![0u8; 123];
        // Default fill: data marker.
        for d in tsdu.iter_mut() {
            *d = 0x01;
        }
        // Status markers at the 4 body positions.
        for p in [13usize, 49, 85, 121] {
            tsdu[p] = 0xFE;
        }
        // Null padding: the LAST 21 non-status positions get 0xFD.
        let mut non_status_positions: Vec<usize> = (0..123)
            .filter(|i| !matches!(*i, 13 | 49 | 85 | 121))
            .collect();
        let null_positions = non_status_positions.split_off(non_status_positions.len() - 21);
        for p in &null_positions {
            tsdu[*p] = 0xFD;
        }

        let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 1);
        assert_eq!(
            data.len(),
            98,
            "deinterleaver must return exactly 98 trellis data dibits \
             (123 raw - 4 status - 21 null)"
        );
        for (i, &d) in data.iter().enumerate() {
            assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
            assert_ne!(d, 0xFD, "null marker survived at output[{}]", i);
            assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
        }
    }

    /// **Phase 6F.3 multi-block TSBK regression guard.** Build a
    /// 231-dibit on-air TSBK1+TSBK2 body with the 7 expected status
    /// drops at {13,49,85,121,157,193,229} and 28 trailing null padding
    /// dibits, and verify the deinterleaver returns exactly 196 trellis
    /// data dibits (= two contiguous 98-dibit blocks) with all status
    /// and null markers stripped.
    #[test]
    fn test_tsdu_deinterleave_two_blocks() {
        let mut tsdu = vec![0u8; 231];
        for d in tsdu.iter_mut() {
            *d = 0x01;
        }
        for &p in &[13usize, 49, 85, 121, 157, 193, 229] {
            tsdu[p] = 0xFE;
        }
        // Mark the LAST 28 non-status positions as null.
        let mut non_status_positions: Vec<usize> = (0..231)
            .filter(|i| !matches!(*i, 13 | 49 | 85 | 121 | 157 | 193 | 229))
            .collect();
        let null_positions =
            non_status_positions.split_off(non_status_positions.len() - 28);
        for p in &null_positions {
            tsdu[*p] = 0xFD;
        }

        let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 2);
        assert_eq!(
            data.len(),
            196,
            "two-block deinterleaver must return 196 trellis dibits \
             (231 raw - 7 status - 28 null)"
        );
        for (i, &d) in data.iter().enumerate() {
            assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
            assert_ne!(d, 0xFD, "null marker survived at output[{}]", i);
            assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
        }
    }

    /// **Phase 6F.3 multi-block TSBK3 regression guard.** TSBK3 has 9
    /// status drops at {13,49,85,121,157,193,229,265,301} and ZERO
    /// trailing null padding. Body length 303 raw → 294 trellis dibits
    /// (= 3 contiguous 98-dibit blocks).
    #[test]
    fn test_tsdu_deinterleave_three_blocks() {
        let mut tsdu = vec![0u8; 303];
        for d in tsdu.iter_mut() {
            *d = 0x01;
        }
        for &p in &[13usize, 49, 85, 121, 157, 193, 229, 265, 301] {
            tsdu[p] = 0xFE;
        }

        let data = TsduDeinterleaver::deinterleave_multi(&tsdu, 3);
        assert_eq!(
            data.len(),
            294,
            "three-block deinterleaver must return 294 trellis dibits \
             (303 raw - 9 status - 0 null)"
        );
        for (i, &d) in data.iter().enumerate() {
            assert_ne!(d, 0xFE, "status marker survived at output[{}]", i);
            assert_eq!(d, 0x01, "non-data dibit at output[{}]: 0x{:02X}", i, d);
        }
    }

    #[test]
    fn test_body_dibits_for_blocks_table() {
        assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(1), Some(123));
        assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(2), Some(231));
        assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(3), Some(303));
        assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(0), None);
        assert_eq!(TsduDeinterleaver::body_dibits_for_blocks(4), None);
    }
}

//! P25 forward error correction:
//!
//! - BCH(63,16,23) protects the NID (NAC and DUID): `bch`;
//! - the 1/2-rate trellis code carries TSBKs and packet data blocks, with the TSDU
//!   deinterleaver;
//! - RS(24,12,13), RS(24,16,9) and RS(63,47,17) over GF(2^6) protect link control and the
//!   header, with the shared Berlekamp-Massey core in `rs_p25`.
//!
//! Reference: TIA-102.BAAA section 7 (coding and interleaving).

pub mod bch;
pub mod rs_24_12_13;
pub mod rs_24_16_9;
pub mod rs_63_47_17;
pub mod rs_p25;

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
/// to undo it. Without it the Viterbi sees scrambled input and finds no
/// valid path.
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

        // The encoder permutes the 196 trellis-coded bits before
        // transmission (TIA-102.BAAA table 7-7), so they are
        // un-permuted (DATA_DEINTERLEAVE) before forming the trellis
        // nibbles; otherwise the Viterbi finds no valid path.

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
/// The TSDU body has its status dibits on the frame's period-36
/// schedule: body raw positions 14, 50, 86, 122, 158, 194, 230, 266,
/// 302 (see `types::is_body_status_dibit`).
/// The last raw dibit of each extent is a status dibit. After stripping
/// status dibits and trailing null padding, the data is exactly
/// `num_blocks * 98` trellis-coded dibits.
///
/// One, two or three blocks, as in SDRTrunk's
/// `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_{1,2,3}`:
///
/// | Blocks | Body raw dibits | Body status dibits          | Trail nulls |
/// |--------|----------------:|-----------------------------|------------:|
/// |   1    |             123 | 4 — {14,50,86,122}          |         21  |
/// |   2    |             231 | 7 — {…,158,194,230}         |         28  |
/// |   3    |             303 | 9 — {…,266,302}             |          0  |
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
        [14, 50, 86, 122, 158, 194, 230, 266, 302];

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
#[path = "tests.rs"]
mod tests;

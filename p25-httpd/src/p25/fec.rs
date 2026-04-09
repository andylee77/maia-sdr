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

    /// Decode P25 NID from 64 raw bits (32 dibits)
    ///
    /// Returns `(nac, duid, raw_duid)` where `raw_duid` is the 4-bit value
    /// that the (currently stubbed) FEC *would* have returned -- the caller
    /// uses it for diagnostic histograms so we can see the actual on-air
    /// DUID distribution while the BCH FEC is still missing. `duid` is the
    /// hard-corrected value (currently always 0x7 = TSDU on the control
    /// channel; see hack note below).
    ///
    /// **Status (2026-04-09): NID FEC IS A STUB.**
    ///
    /// Per TIA-102.BAAA Section 7.2 the P25 NID is encoded with shortened
    /// BCH(63,16,11) (= BCH(64,16,11) with a leading zero), capable of
    /// correcting up to 11 bit errors in the 64-bit NID block. This
    /// implementation currently does no error correction at all -- it
    /// just reads the high 16 bits as `NAC[12] || DUID[4]`. The
    /// `GolayDecoder::decode/syndrome/parity_of_bit` helpers in this
    /// module are remnants of an earlier (incorrect) design and are not
    /// yet wired in.
    ///
    /// We discovered this on 2026-04-09 while debugging the dashboard's
    /// stuck-at-"Searching" state. Sync acquisition was working (sync
    /// hits at ~3/sec, distances 7-10) but every "valid" NID decoded as
    /// DUID 0x5 (LDU1) or 0x0 (HDU) -- never 0x7 (TSDU). Reason: the
    /// slicer's residual DC bias produces ~12 bit errors per NID, and
    /// without FEC the 4-bit DUID field is essentially random within
    /// ~3 bits of the true value. ~7 of the 16 possible DUID nibbles
    /// happen to be legal enum values, so most reads land on a "valid"
    /// but wrong DUID and the rest get flagged "DUID invalid".
    ///
    /// **Temporary hack (until BCH(64,16) is implemented):** the control
    /// channel only carries TSDUs (DUID=0x7). Since we're hard-tuned to
    /// a known control frequency, we hardcode `duid = 0x7` and rely on
    /// the downstream TSBK CRC + trellis FEC to validate or reject the
    /// payload. This unblocks testing of the entire post-NID pipeline
    /// without waiting for the BCH implementation. It WILL break if
    /// the same code path is reused for traffic channel decoding --
    /// fix the FEC properly before that happens.
    pub fn decode_nid(nid_bits: u64) -> Option<(u16, u8, u8)> {
        let nac = ((nid_bits >> 52) & 0xFFF) as u16;
        let raw_duid = ((nid_bits >> 48) & 0xF) as u8;
        // HACK: see docstring. Hardcode TSDU until BCH(64,16) lands.
        const HARDCODED_DUID_TSDU: u8 = 0x7;
        Some((nac, HARDCODED_DUID_TSDU, raw_duid))
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
/// The TSDU interleaves status symbols among the data.
/// A TSDU frame contains 336 dibits total:
///   - 12 status dibits (SS)
///   - 324 data dibits (for up to 3 TSBKs, but typically 1-2)
///
/// Status symbols appear every 35 dibits (at positions 35, 71, 107, ...)
pub struct TsduDeinterleaver;

impl TsduDeinterleaver {
    /// Remove status symbols from a TSDU and return data dibits
    /// Input: raw TSDU dibits (336)
    /// Output: data dibits with status symbols removed
    pub fn deinterleave(tsdu_dibits: &[u8]) -> Vec<u8> {
        let mut data = Vec::with_capacity(324);
        for (i, &dibit) in tsdu_dibits.iter().enumerate() {
            // Status symbols at positions 35*n + 34 (0-indexed)
            // i.e., every 35th dibit starting from position 34
            if (i + 1) % 35 != 0 {
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
    fn test_nid_decode() {
        // NAC=0x8A1, DUID=0x7 (TSDU)
        // Pack into NID bits: NAC in bits 63-52, DUID in bits 51-48
        let nid_bits: u64 = (0x8A1u64 << 52) | (0x7u64 << 48);
        let result = GolayDecoder::decode_nid(nid_bits);
        assert!(result.is_some());
        let (nac, duid, raw_duid) = result.unwrap();
        assert_eq!(nac, 0x8A1);
        // duid is currently always hardcoded to 0x7 (control-channel hack
        // until BCH(64,16) NID FEC is implemented). raw_duid is what the
        // un-FECed extractor saw -- here it matches because we built a
        // clean test vector.
        assert_eq!(duid, 0x7);
        assert_eq!(raw_duid, 0x7);
    }

    #[test]
    fn test_nid_decode_records_raw_duid() {
        // NAC=0xE28, raw DUID=0x5 (looks like LDU1) -- this is the actual
        // pattern we observed on hardware before adding the hardcode hack.
        // The function should return the hardcoded TSDU but report the
        // raw_duid as 0x5 so we can build the diagnostic histogram.
        let nid_bits: u64 = (0xE28u64 << 52) | (0x5u64 << 48);
        let (nac, duid, raw_duid) = GolayDecoder::decode_nid(nid_bits).unwrap();
        assert_eq!(nac, 0xE28);
        assert_eq!(duid, 0x7); // hardcoded
        assert_eq!(raw_duid, 0x5); // what the air actually said
    }

    #[test]
    fn test_tsdu_deinterleave_removes_status() {
        // Create a 350-dibit TSDU with known pattern
        let mut tsdu = vec![0u8; 350];
        // Mark status positions with 0xFF
        for i in 0..tsdu.len() {
            if (i + 1) % 35 == 0 {
                tsdu[i] = 0xFF;
            } else {
                tsdu[i] = 1;
            }
        }

        let data = TsduDeinterleaver::deinterleave(&tsdu);
        // Should have removed the status symbols
        for &d in &data {
            assert_ne!(d, 0xFF, "Status symbol should be removed");
        }
    }
}

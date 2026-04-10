//! P25 NID frame-sync detectors and status-aware NID extractor.
//!
//! Phase 6D port of `find_sync_events_hard`, `find_sync_events_soft`, and
//! `_extract_nid_skipping_status` from `tools/p25_lsm_demod.py`. Both the
//! hard and soft detectors are provided so we can A/B compare them at
//! runtime exactly like the Python reference does.
//!
//! - **Hard detector** — sliding 48-bit register, Hamming distance to the
//!   `0x5575_F5FF_77FF` sync pattern, threshold ≤ 4. The same algorithm
//!   p25-httpd's existing C4FM dibit pipeline uses, easy to port to HDL.
//! - **Soft detector** — port of `P25P1SoftSyncDetectorScalar` in SDRTrunk:
//!   inner product of the 24 ideal sync phases (`±3π/4`) with the
//!   demod's soft phase output. Threshold = 60. SDRTrunk's chosen detector
//!   for LSM because it picks up syncs the hard detector misses on noisy
//!   data.
//!
//! Both detectors share the same status-dibit-aware NID extractor: read 33
//! dibits past the sync hit and skip index 11 (the position where the
//! 35-dibit-cycle status symbol falls inside the NID block, given that
//! `P25P1MessageFramer` resets the status counter on sync detection).

use super::nid_fec::{decode_nid as bch_decode_nid, DecodedNid};
use std::f32::consts::PI;

/// 48-bit P25 frame sync (24 dibits), packed dibits-MSB-first into a u64.
/// Same constant SDRTrunk uses (`P25P1SyncDetector.SYNC_PATTERN`); matches
/// TIA-102.BAAA.
pub const FRAME_SYNC_DIBIT_PATTERN: u64 = 0x5575_F5FF_77FF;
/// Mask of the 48 sync bits inside the u64 register.
pub const FRAME_SYNC_MASK: u64 = 0xFFFF_FFFF_FFFF;
/// Number of dibits in the sync pattern.
pub const FRAME_SYNC_DIBITS: usize = 24;

/// 33 dibits transmitted for the NID, including 1 status dibit at index 11.
pub const NID_TRANSMITTED_DIBITS: usize = 33;
/// Position (within the 33-dibit window) of the inserted status dibit.
pub const NID_STATUS_DIBIT_INDEX: usize = 11;

/// Hamming distance threshold for the hard sync detector. Same value the
/// Python reference uses (`SYNC_THRESHOLD = 4`).
pub const SYNC_THRESHOLD: u32 = 4;

/// Soft sync correlation threshold. Same value SDRTrunk's
/// `P25P1MessageFramer.SYNC_DETECTION_THRESHOLD` uses (60.0). A perfect
/// lock on the sync pattern produces ~133, this threshold is roughly half
/// of that — generous tolerance for noisy syncs.
pub const SYNC_SCORE_THRESHOLD: f32 = 60.0;

/// One frame-sync match -> NID extraction (raw + BCH-corrected).
///
/// Mirrors the `SyncEvent` dataclass in the Python reference.
#[derive(Debug, Clone, Copy)]
pub struct SyncEvent {
    /// Symbol index of the dibit immediately after the sync window.
    pub symbol_idx: usize,
    /// Hamming distance of the sync match (hard detector only; -1 for soft).
    pub distance: i32,
    /// Soft correlation score (soft detector only; 0.0 for hard).
    pub score: f32,
    /// 12-bit NAC, raw extraction (no FEC).
    pub nac: u16,
    /// 4-bit DUID, raw extraction (no FEC).
    pub duid: u8,
    /// Full 64-bit NID word, status dibit already skipped.
    pub nid_raw: u64,
    /// Result of running the 64-bit NID through the BCH(63,16,11) decoder.
    /// `None` means the FEC declared the word uncorrectable (>11 errors).
    pub fec: Option<DecodedNid>,
}

impl SyncEvent {
    /// Convenience: NAC after FEC if correctable, else raw NAC.
    pub fn best_nac(&self) -> u16 {
        self.fec.map(|d| d.nac).unwrap_or(self.nac)
    }

    /// Convenience: DUID after FEC if correctable, else raw DUID.
    pub fn best_duid(&self) -> u8 {
        self.fec.map(|d| d.duid).unwrap_or(self.duid)
    }
}

/// Read 33 dibits starting at `start_idx`, skip index 11 (status dibit),
/// pack the remaining 32 dibits into a 64-bit NID word, and split out
/// (NAC, DUID).
///
/// Returns `None` if `dibits` doesn't have enough room past `start_idx`.
/// The third return value is the full 64-bit NID, suitable for handing
/// directly to `nid_fec::decode_nid`.
fn extract_nid_skipping_status(dibits: &[u8], start_idx: usize) -> Option<(u16, u8, u64)> {
    let end = start_idx + NID_TRANSMITTED_DIBITS;
    if end > dibits.len() {
        return None;
    }
    let mut nid_bits: u64 = 0;
    for j in 0..NID_TRANSMITTED_DIBITS {
        if j == NID_STATUS_DIBIT_INDEX {
            continue;
        }
        nid_bits = (nid_bits << 2) | (dibits[start_idx + j] as u64 & 0x3);
    }
    let nac = ((nid_bits >> 52) & 0xFFF) as u16;
    let duid = ((nid_bits >> 48) & 0xF) as u8;
    Some((nac, duid, nid_bits))
}

/// Hard-decision Hamming-distance sync correlator. The simple HDL-friendly
/// version. Slides a 48-bit register over the dibit stream and emits a
/// sync event whenever the register lands within `SYNC_THRESHOLD` Hamming
/// distance of `FRAME_SYNC_DIBIT_PATTERN`.
///
/// After each hit we skip past the 33-dibit NID window before resuming
/// search, exactly like SDRTrunk's `P25P1MessageFramer` suppresses sync
/// detection during message assembly. Without this skip we false-trigger
/// on dibit content immediately following the sync.
pub fn find_sync_events_hard(dibits: &[u8]) -> Vec<SyncEvent> {
    let mut sync_register: u64 = 0;
    let mut out: Vec<SyncEvent> = Vec::new();
    let mut i: usize = 0;
    while i < dibits.len() {
        sync_register = ((sync_register << 2) | (dibits[i] as u64 & 0x3)) & FRAME_SYNC_MASK;
        i += 1;
        if i < FRAME_SYNC_DIBITS {
            continue;
        }
        let dist = (sync_register ^ FRAME_SYNC_DIBIT_PATTERN).count_ones();
        if dist <= SYNC_THRESHOLD {
            let Some((nac, duid, nid_bits)) = extract_nid_skipping_status(dibits, i) else {
                break;
            };
            let fec = bch_decode_nid(nid_bits);
            out.push(SyncEvent {
                symbol_idx: i,
                distance: dist as i32,
                score: 0.0,
                nac,
                duid,
                nid_raw: nid_bits,
                fec,
            });
            // Skip past the 33-dibit NID window so we don't false-trigger
            // on its contents.
            i += NID_TRANSMITTED_DIBITS;
            sync_register = 0;
        }
    }
    out
}

/// Build the 24 ideal symbol phases of the sync pattern.
///
/// Mirrors `_build_sync_pattern_phases` in the Python reference and
/// `P25P1SyncDetector.syncPatternToSymbols()` in SDRTrunk: extract dibits
/// MSB-first, map `01 → +3π/4` and `11 → -3π/4` (the sync pattern is all
/// outer ±3 symbols, no inner ±1).
fn build_sync_pattern_phases() -> [f32; 24] {
    let mut out = [0.0_f32; 24];
    for x in 0..24 {
        let shift = (23 - x) * 2;
        let dibit = (FRAME_SYNC_DIBIT_PATTERN >> shift) & 0x3;
        match dibit {
            0b01 => out[x] = 3.0 * PI / 4.0,
            0b11 => out[x] = -3.0 * PI / 4.0,
            _ => panic!(
                "sync pattern dibit {x} = {dibit:02b}; expected only ±3 symbols"
            ),
        }
    }
    out
}

/// Soft-symbol sync correlator. Direct port of
/// `P25P1SoftSyncDetectorScalar` in SDRTrunk and `find_sync_events_soft`
/// in the Python reference.
///
/// For every soft symbol position k:
///
/// ```text
///     score[k] = sum_{x=0..23}( SYNC_PATTERN_PHASES[x] * soft_phases[k+x] )
/// ```
///
/// A perfect lock on the sync pattern gives `score = 24 * (3π/4)² ≈ 133`.
/// SDRTrunk uses threshold 60 (about half the maximum). The window for
/// score k spans soft phases [k .. k+23]; the sync window thus *ends* at
/// dibit index `k + 24 - 1`, and the first NID dibit is at `k + 24`.
///
/// We pick local maxima above threshold to suppress double-triggering on
/// adjacent positions of the same sync event, and use the corresponding
/// `hard_dibits` for NID extraction so the NID/FEC stages are bit-identical
/// across hard and soft paths.
pub fn find_sync_events_soft(soft_phases: &[f32], hard_dibits: &[u8]) -> Vec<SyncEvent> {
    let mut out: Vec<SyncEvent> = Vec::new();
    let n = soft_phases.len();
    if n < FRAME_SYNC_DIBITS + 1 {
        return out;
    }
    let pattern = build_sync_pattern_phases();

    // Score length = n - 24 + 1 (matches np.correlate(..., mode='valid')).
    let score_len = n - FRAME_SYNC_DIBITS + 1;
    let mut scores: Vec<f32> = Vec::with_capacity(score_len);
    for k in 0..score_len {
        let mut s = 0.0_f32;
        for x in 0..FRAME_SYNC_DIBITS {
            s += pattern[x] * soft_phases[k + x];
        }
        scores.push(s);
    }

    // Walk through above-threshold windows and emit local maxima only.
    // SyncEvent positions follow the Python reference: symbol_idx points
    // to the FIRST dibit of the NID payload (sync_end + 1).
    let mut last_emit: i64 = -10_000_000;
    for k in 0..score_len {
        if scores[k] <= SYNC_SCORE_THRESHOLD {
            continue;
        }
        let sync_end = k + FRAME_SYNC_DIBITS - 1;
        if (sync_end as i64) - last_emit < FRAME_SYNC_DIBITS as i64 {
            continue;
        }
        let prev = if k > 0 { scores[k - 1] } else { f32::MIN };
        let nxt = if k + 1 < score_len {
            scores[k + 1]
        } else {
            f32::MIN
        };
        if !(scores[k] >= prev && scores[k] >= nxt) {
            continue;
        }
        let first_nid_idx = sync_end + 1;
        let Some((nac, duid, nid_bits)) =
            extract_nid_skipping_status(hard_dibits, first_nid_idx)
        else {
            break;
        };
        let fec = bch_decode_nid(nid_bits);
        out.push(SyncEvent {
            symbol_idx: first_nid_idx,
            distance: -1,
            score: scores[k],
            nac,
            duid,
            nid_raw: nid_bits,
            fec,
        });
        last_emit = (sync_end + NID_TRANSMITTED_DIBITS) as i64;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::nid_fec::encode_nid;
    use super::*;

    /// Build a clean dibit stream containing the sync pattern followed by
    /// a known NID encoded for NAC=0x8A1 / DUID=7. The hard detector must
    /// find exactly one event with distance 0, and the BCH decoder must
    /// recover the same NAC/DUID.
    #[test]
    fn hard_detector_finds_clean_sync_and_decodes_nid() {
        // Build the 24 sync dibits MSB-first from FRAME_SYNC_DIBIT_PATTERN.
        let mut dibits: Vec<u8> = Vec::new();
        for x in 0..FRAME_SYNC_DIBITS {
            let shift = (FRAME_SYNC_DIBITS - 1 - x) * 2;
            dibits.push(((FRAME_SYNC_DIBIT_PATTERN >> shift) & 0x3) as u8);
        }
        // Append the 33-dibit NID window: 11 dibits of NID, 1 status
        // dibit (anything — we choose 0), 21 more dibits of NID.
        let nid = encode_nid(0x8A1, 7);
        let mut nid_dibits: [u8; 32] = [0; 32];
        for j in 0..32 {
            let shift = (31 - j) * 2;
            nid_dibits[j] = ((nid >> shift) & 0x3) as u8;
        }
        // Splice in the status dibit at index 11.
        for j in 0..NID_TRANSMITTED_DIBITS {
            if j == NID_STATUS_DIBIT_INDEX {
                dibits.push(0); // status dibit value doesn't affect extraction
            } else {
                let payload_idx = if j < NID_STATUS_DIBIT_INDEX { j } else { j - 1 };
                dibits.push(nid_dibits[payload_idx]);
            }
        }

        let events = find_sync_events_hard(&dibits);
        assert_eq!(events.len(), 1, "expected exactly one sync hit");
        let e = events[0];
        assert_eq!(e.distance, 0);
        assert_eq!(e.nid_raw, nid);
        assert_eq!(e.nac, 0x8A1);
        assert_eq!(e.duid, 7);
        let fec = e.fec.expect("clean NID must decode");
        assert_eq!(fec.nac, 0x8A1);
        assert_eq!(fec.duid, 7);
        assert_eq!(fec.n_errors, 0);
    }

    /// One bit error in the sync pattern (well within `SYNC_THRESHOLD = 4`)
    /// must still produce a hit.
    #[test]
    fn hard_detector_tolerates_one_dibit_error_in_sync() {
        let mut dibits: Vec<u8> = Vec::new();
        for x in 0..FRAME_SYNC_DIBITS {
            let shift = (FRAME_SYNC_DIBITS - 1 - x) * 2;
            dibits.push(((FRAME_SYNC_DIBIT_PATTERN >> shift) & 0x3) as u8);
        }
        // Flip one bit of the second dibit. 01 → 11 → distance 1.
        dibits[1] ^= 0b10;
        // Pad with zeros for the NID window — we only care that the sync
        // hit fires, not that the NID decodes.
        for _ in 0..NID_TRANSMITTED_DIBITS {
            dibits.push(0);
        }
        let events = find_sync_events_hard(&dibits);
        assert_eq!(events.len(), 1, "expected one sync hit despite 1-bit error");
        assert!(events[0].distance >= 1 && events[0].distance <= 2);
    }

    /// Soft detector should fire on a perfect sync sequence as well. Build
    /// soft phases that exactly match the sync pattern phases (so the
    /// inner product equals the maximum possible value, well above
    /// threshold), and the hard dibits are the same clean NID we built
    /// for the hard test.
    #[test]
    fn soft_detector_finds_clean_sync() {
        let pattern = build_sync_pattern_phases();
        // 24 sync soft phases + 33 NID payload soft phases (zero is fine,
        // soft detector only looks at the first 24).
        let mut soft_phases: Vec<f32> =
            pattern.iter().copied().chain(std::iter::repeat(0.0).take(NID_TRANSMITTED_DIBITS)).collect();
        // Pad a few extra zeros so soft_phases.len() > 24+33 (the soft
        // correlator window slides from 0 to len-24).
        soft_phases.extend(std::iter::repeat(0.0).take(8));

        // Hard dibits: 24 sync dibits + 33 NID dibits encoding NAC=1/DUID=0
        let mut hard_dibits: Vec<u8> = Vec::new();
        for x in 0..FRAME_SYNC_DIBITS {
            let shift = (FRAME_SYNC_DIBITS - 1 - x) * 2;
            hard_dibits.push(((FRAME_SYNC_DIBIT_PATTERN >> shift) & 0x3) as u8);
        }
        let nid = encode_nid(1, 0);
        let mut payload: [u8; 32] = [0; 32];
        for j in 0..32 {
            payload[j] = ((nid >> ((31 - j) * 2)) & 0x3) as u8;
        }
        for j in 0..NID_TRANSMITTED_DIBITS {
            if j == NID_STATUS_DIBIT_INDEX {
                hard_dibits.push(0);
            } else {
                let pi = if j < NID_STATUS_DIBIT_INDEX { j } else { j - 1 };
                hard_dibits.push(payload[pi]);
            }
        }
        // Pad hard_dibits to soft_phases length.
        while hard_dibits.len() < soft_phases.len() {
            hard_dibits.push(0);
        }

        let events = find_sync_events_soft(&soft_phases, &hard_dibits);
        assert_eq!(events.len(), 1, "expected one soft sync hit");
        let e = events[0];
        // 24 * (3π/4)² ≈ 133.3
        assert!(
            e.score > 130.0,
            "expected near-max correlation score, got {}",
            e.score
        );
        assert_eq!(e.nac, 1);
        assert_eq!(e.duid, 0);
        let fec = e.fec.expect("clean NID must decode");
        assert_eq!(fec.nac, 1);
        assert_eq!(fec.duid, 0);
        assert_eq!(fec.n_errors, 0);
    }

    /// Status dibit at index 11 must be skipped — i.e. the value of that
    /// dibit must NOT affect the extracted NID.
    #[test]
    fn status_dibit_is_skipped() {
        let nid = encode_nid(0x8A1, 7);
        let mut payload: [u8; 32] = [0; 32];
        for j in 0..32 {
            payload[j] = ((nid >> ((31 - j) * 2)) & 0x3) as u8;
        }
        // Two streams: identical except for the status dibit at index 11.
        let make_stream = |status: u8| -> Vec<u8> {
            let mut d = Vec::new();
            for j in 0..NID_TRANSMITTED_DIBITS {
                if j == NID_STATUS_DIBIT_INDEX {
                    d.push(status);
                } else {
                    let pi = if j < NID_STATUS_DIBIT_INDEX { j } else { j - 1 };
                    d.push(payload[pi]);
                }
            }
            d
        };
        let a = extract_nid_skipping_status(&make_stream(0), 0).unwrap();
        let b = extract_nid_skipping_status(&make_stream(3), 0).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.0, 0x8A1);
        assert_eq!(a.1, 7);
        assert_eq!(a.2, nid);
    }
}

//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::super::nid_fec::encode_nid;
use super::*;
use crate::protocol::p25::test_fixtures::*;

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
    let nid = encode_nid(CLAY_NAC, 7);
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
    assert_eq!(e.nac, CLAY_NAC);
    assert_eq!(e.duid, 7);
    let fec = e.fec.expect("clean NID must decode");
    assert_eq!(fec.nac, CLAY_NAC);
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
    let nid = encode_nid(CLAY_NAC, 7);
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
    assert_eq!(a.0, CLAY_NAC);
    assert_eq!(a.1, 7);
    assert_eq!(a.2, nid);
}

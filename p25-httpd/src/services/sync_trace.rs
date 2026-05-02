//! Per-LDU PLL / AGC / sync register sampler ring.
//!
//! Populated by the traffic LSM heartbeat task on every `nid_event`
//! during an active call (`current_call_id != 0`). Same lock
//! acquisition as the existing `traffic_lsm_status` + `traffic_lsm_nid`
//! reads, extended to also capture `traffic_lsm_debug` (PLL + symbol
//! timing) and `traffic_lsm_agc_debug` (gain + input magnitude).
//!
//! Single shared bounded ring rather than per-recording storage so the
//! heartbeat doesn't need a back-channel to the recorder. The HTTP
//! handler at `/api/recordings/{id}/sync_trace` filters by `call_id`.
//! Closed-call samples roll off naturally as the next call writes.
//!
//! Captured even when `nid_valid` is false — those samples are the
//! diagnostic gold for "framer locking onto jittery symbols mid-call"
//! per the post-pacer next-step plan.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Per-NID snapshot of the traffic chain's lock-quality registers.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SyncTraceSample {
    /// `GrantFollower` call_id (= `RecordingEntry.id`) at sample time.
    pub call_id: u64,
    /// Wall-clock unix milliseconds. Real if NTP synced, else kernel
    /// boot clock.
    pub unix_ms: u64,
    /// `0x0` HDU / `0x3` TDU / `0x5` LDU1 / `0xA` LDU2 / `0xF` TDU_LC.
    pub duid: u8,
    /// Network Access Code latched on this NID.
    pub nac: u16,
    /// BCH(63,16) decode passed (errors ≤ t).
    pub nid_valid: bool,
    /// BCH busy flag during the NID decode pipeline.
    pub bch_busy: bool,
    /// BCH correctable error count for this NID.
    pub n_errors: u8,
    /// Sync correlator Hamming distance.
    pub sync_distance: u8,
    /// Live PLL phase increment (signed Q2.13). Drift over the call
    /// surfaces as a slow walk away from zero.
    pub pll_dbg: i16,
    /// Symbol-timing recovery sample point (signed Q4.10). Stable
    /// near the eye centre; drift indicates Gardner slip.
    pub sample_point_dbg: i16,
    /// AGC gain (unsigned Q9.7 truncation of Q9.11). At steady state
    /// `gain × mag / 2^11 ≈ TARGET_RAW (32768 = 1.0 in Q1.15)`.
    pub agc_gain: u16,
    /// L2 magnitude of the AGC's most recent input sample (Q1.15).
    pub agc_mag: u16,
}

pub type SyncTraceRing = Arc<Mutex<VecDeque<SyncTraceSample>>>;

/// 16 384 samples ≈ 5 min at 50 NID/s. Most calls are well under 30 s
/// so a single recording's window fits comfortably; multi-call lookback
/// is bounded by ring rotation rather than memory.
pub const SYNC_TRACE_CAP: usize = 16_384;

pub fn new_ring() -> SyncTraceRing {
    Arc::new(Mutex::new(VecDeque::with_capacity(SYNC_TRACE_CAP)))
}

/// Push a sample, evicting the oldest when the cap is reached. Lock is
/// uncontested in practice (only the heartbeat writes; HTTP handlers
/// read), so a `std::sync::Mutex` is fine — no async needed.
pub fn push(ring: &SyncTraceRing, sample: SyncTraceSample) {
    if let Ok(mut r) = ring.lock() {
        if r.len() == SYNC_TRACE_CAP {
            r.pop_front();
        }
        r.push_back(sample);
    }
}

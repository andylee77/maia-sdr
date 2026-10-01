//! Alternate CRC masks ("RAS"): port of SDRTrunk `DMRCrcMaskManager`.
//!
//! Some systems mask CSBK / LC checksums with a value other than the
//! standard one. A failed check whose residual (the mask in use) has been
//! seen before for the same opcode, recently, is accepted.

use std::collections::BTreeMap;

/// Tracked masks are dropped after this long without a sighting...
const CACHE_EJECTION_POLICY_TIME_MS: u64 = 2 * 60 * 1000;
/// ...or after this many checks of other masks.
const CACHE_EJECTION_POLICY_COUNT: u32 = 20;

/// Ports `DMRCrcMaskManager`.
#[derive(Debug, Default)]
pub struct DmrCrcMaskManager {
    csbk: BTreeMap<i32, OpcodeMaskTracker>,
    crc5: BTreeMap<i32, OpcodeMaskTracker>,
    rs12_9: BTreeMap<i32, OpcodeMaskTracker>,
}

impl DmrCrcMaskManager {
    /// Full LC (RS(12,9)) residual seen before for this opcode. Ports `isValidRS12_9()`.
    pub fn is_valid_rs12_9(&mut self, opcode: i32, residual: u32, timestamp_ms: u64) -> bool {
        is_valid(opcode, residual, timestamp_ms, &mut self.rs12_9)
    }

    /// Embedded LC (checksum 5) residual seen before. Ports `isValidCRC5()`.
    pub fn is_valid_crc5(&mut self, opcode: i32, residual: u32, timestamp_ms: u64) -> bool {
        is_valid(opcode, residual, timestamp_ms, &mut self.crc5)
    }

    /// CSBK (CRC-CCITT) residual seen before. Ports `isValidCSBK()`.
    pub fn is_valid_csbk(&mut self, opcode: i32, residual: u32, timestamp_ms: u64) -> bool {
        is_valid(opcode, residual, timestamp_ms, &mut self.csbk)
    }
}

fn is_valid(
    opcode: i32,
    residual: u32,
    timestamp_ms: u64,
    map: &mut BTreeMap<i32, OpcodeMaskTracker>,
) -> bool {
    if opcode < 0 {
        return false;
    }
    match map.get_mut(&opcode) {
        Some(tracker) => tracker.is_valid(residual, timestamp_ms),
        None => {
            map.insert(opcode, OpcodeMaskTracker::new(residual, timestamp_ms));
            false
        }
    }
}

/// Masks seen for one opcode. Ports `DMRCrcMaskManager.OpcodeMaskTracker`.
#[derive(Debug)]
struct OpcodeMaskTracker {
    trackers: BTreeMap<u32, MaskTracker>,
}

impl OpcodeMaskTracker {
    fn new(mask: u32, timestamp_ms: u64) -> Self {
        let mut trackers = BTreeMap::new();
        trackers.insert(mask, MaskTracker::new(timestamp_ms));
        OpcodeMaskTracker { trackers }
    }

    /// Counts a sighting of `mask`: valid from the second one. Ages every mask.
    fn is_valid(&mut self, mask: u32, timestamp_ms: u64) -> bool {
        let valid = match self.trackers.get_mut(&mask) {
            Some(tracker) => {
                tracker.increment(timestamp_ms);
                tracker.observations >= 2
            }
            None => {
                self.trackers.insert(mask, MaskTracker::new(timestamp_ms));
                false
            }
        };
        self.trackers
            .retain(|_, tracker| !tracker.is_stale(timestamp_ms));
        valid
    }
}

/// Ports `DMRCrcMaskManager.MaskTracker`.
#[derive(Debug)]
struct MaskTracker {
    observations: u32,
    staleness: u32,
    last_updated_ms: u64,
}

impl MaskTracker {
    fn new(timestamp_ms: u64) -> Self {
        MaskTracker {
            observations: 1,
            staleness: 0,
            last_updated_ms: timestamp_ms,
        }
    }

    /// Each check ages the tracker (SDRTrunk counts this in `isStale()`).
    fn is_stale(&mut self, timestamp_ms: u64) -> bool {
        self.staleness += 1;
        self.staleness > CACHE_EJECTION_POLICY_COUNT
            || timestamp_ms.abs_diff(self.last_updated_ms) > CACHE_EJECTION_POLICY_TIME_MS
    }

    fn increment(&mut self, timestamp_ms: u64) {
        self.staleness = 0;
        self.observations = self.observations.saturating_add(1);
        self.last_updated_ms = timestamp_ms;
    }
}

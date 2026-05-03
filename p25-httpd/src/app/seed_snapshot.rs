//! Converged-seed snapshot for warm-starting the traffic LSM chain.
//!
//! 2026-05-03 seeding bake follow-on: the HDL now exposes write-only
//! seed registers for the AGC / Costas PLL / Gardner timing
//! accumulators on both LSM chains (bank 8 @ 0x100, see
//! `doc/changes/050_seeding_bake.md`). The control LSM chain stays
//! locked on real signal continuously, so its converged debug taps
//! (`pll_dbg`, `sample_point_dbg`, `agc_gain_dbg`) are the cleanest
//! source of warm-start values for the traffic chain on every retune.
//!
//! Capture rule: only sample during clean LDU flow on the control
//! chain — `nid_event && nid_valid && sync_distance == 0 &&
//! !bch_busy`. Anything else is between-PTT noise that corrupts the
//! median (memory: 2026-05-03 session, "AGC: needs PTT-time
//! sampling").
//!
//! Commit rule: after `MIN_CLEAN_SAMPLES` clean snapshots have
//! accumulated, compute the median across them and atomically replace
//! the published `ConvergedSeeds`. Subsequent clean samples roll a
//! fixed-size window; once full, each new clean sample evicts the
//! oldest and re-medians. Median (not mean) so a single bad reading
//! that snuck through the gate doesn't pull the whole window.

use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Minimum clean snapshots before publishing the first ConvergedSeeds.
/// At ~6 NID/s on a busy site, 6 clean samples = ~1 s of clean signal.
pub const MIN_CLEAN_SAMPLES: usize = 6;

/// Rolling window depth. ~5 s of clean signal at the same NID rate.
/// Larger windows smooth more but lag operator gain changes; this
/// trades responsiveness for noise rejection.
pub const WINDOW_CAPACITY: usize = 32;

/// Q-format-tagged seed values ready to write into the HDL bank-8
/// registers. Field widths match the HDL accumulators, NOT the debug
/// taps (e.g. AGC accumulator is Q9.11 / 20-bit even though
/// `agc_gain_dbg` is Q9.7 / 16-bit; the snapshot path shifts the
/// debug value left by 4 to recover Q9.11).
#[derive(Debug, Clone, Copy)]
pub struct ConvergedSeeds {
    /// Q9.11 unsigned 20-bit. From `agc_gain_dbg << 4`.
    pub agc_seed: u32,
    /// Q2.13 signed 16-bit. From `pll_dbg`.
    pub pll_seed: i16,
    /// Q5.12 signed 18-bit. From `sample_point_dbg`.
    pub timing_seed: i32,
    /// Wall-clock instant the median was last recomputed. None means
    /// the snapshot has not yet committed (still warming up).
    pub last_updated_at: Instant,
    /// Number of clean samples folded into the current window.
    pub samples_in_window: usize,
    /// Cumulative clean-sample count since boot. Useful for
    /// `/api/system` to surface whether the heartbeat is gathering
    /// data even when the seeds aren't yet committed.
    pub total_clean_samples: u64,
}

/// Shared handle published by the heartbeat task and consumed by
/// `retune_traffic_chain`. `None` until the heartbeat has seen
/// `MIN_CLEAN_SAMPLES` clean snapshots.
pub type ConvergedSeedsShared = Arc<RwLock<Option<ConvergedSeeds>>>;

pub fn new_converged_seeds_shared() -> ConvergedSeedsShared {
    Arc::new(RwLock::new(None))
}

/// One captured tick. The capture site asserts the gate (clean LDU
/// flow on the control chain); no validation here.
#[derive(Debug, Clone, Copy)]
pub struct CleanSample {
    /// Q9.11 (already shifted from Q9.7 dbg).
    pub agc_q9_11: u32,
    /// Q2.13.
    pub pll_q2_13: i16,
    /// Q5.12.
    pub timing_q5_12: i32,
}

/// Heartbeat-side ring buffer of clean samples. Owned by the
/// heartbeat task; not shared. Folds new samples into the published
/// `ConvergedSeedsShared` whenever the median changes meaningfully.
pub struct CleanSampleWindow {
    samples: Vec<CleanSample>,
    next_idx: usize,
    total_clean: u64,
}

impl CleanSampleWindow {
    pub fn new() -> Self {
        Self {
            samples: Vec::with_capacity(WINDOW_CAPACITY),
            next_idx: 0,
            total_clean: 0,
        }
    }

    /// Adds a clean sample. Returns the new committed `ConvergedSeeds`
    /// if the window now has at least `MIN_CLEAN_SAMPLES` samples;
    /// `None` while still warming up.
    pub fn observe(&mut self, sample: CleanSample) -> Option<ConvergedSeeds> {
        self.total_clean = self.total_clean.saturating_add(1);
        if self.samples.len() < WINDOW_CAPACITY {
            self.samples.push(sample);
        } else {
            self.samples[self.next_idx] = sample;
            self.next_idx = (self.next_idx + 1) % WINDOW_CAPACITY;
        }
        if self.samples.len() < MIN_CLEAN_SAMPLES {
            return None;
        }
        Some(self.commit())
    }

    fn commit(&self) -> ConvergedSeeds {
        let mut agcs: Vec<u32> = self.samples.iter().map(|s| s.agc_q9_11).collect();
        let mut plls: Vec<i16> = self.samples.iter().map(|s| s.pll_q2_13).collect();
        let mut tims: Vec<i32> = self.samples.iter().map(|s| s.timing_q5_12).collect();
        agcs.sort_unstable();
        plls.sort_unstable();
        tims.sort_unstable();
        let mid = agcs.len() / 2;
        ConvergedSeeds {
            agc_seed: agcs[mid],
            pll_seed: plls[mid],
            timing_seed: tims[mid],
            last_updated_at: Instant::now(),
            samples_in_window: self.samples.len(),
            total_clean_samples: self.total_clean,
        }
    }

    pub fn total_clean(&self) -> u64 {
        self.total_clean
    }
}

impl Default for CleanSampleWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_warms_up_then_commits_median() {
        let mut w = CleanSampleWindow::new();
        // Below minimum: still None.
        for i in 0..(MIN_CLEAN_SAMPLES - 1) {
            let s = CleanSample {
                agc_q9_11: 1000 + i as u32,
                pll_q2_13: -13,
                timing_q5_12: 30000,
            };
            assert!(w.observe(s).is_none(), "warmup should return None");
        }
        // The crossing sample commits.
        let last = CleanSample {
            agc_q9_11: 1000 + (MIN_CLEAN_SAMPLES - 1) as u32,
            pll_q2_13: -13,
            timing_q5_12: 30000,
        };
        let seeds = w.observe(last).expect("commits at MIN_CLEAN_SAMPLES");
        assert_eq!(seeds.pll_seed, -13);
        assert_eq!(seeds.samples_in_window, MIN_CLEAN_SAMPLES);
        assert_eq!(seeds.total_clean_samples, MIN_CLEAN_SAMPLES as u64);
    }

    #[test]
    fn outlier_rejection_via_median() {
        // Five clean samples at PLL = -13, then one outlier at +1000.
        // Median should still pick a value near -13, not be pulled.
        let mut w = CleanSampleWindow::new();
        for _ in 0..5 {
            w.observe(CleanSample {
                agc_q9_11: 2048,
                pll_q2_13: -13,
                timing_q5_12: 30000,
            });
        }
        let seeds = w
            .observe(CleanSample {
                agc_q9_11: 2048,
                pll_q2_13: 1000, // outlier
                timing_q5_12: 30000,
            })
            .unwrap();
        // 6 samples sorted: [-13, -13, -13, -13, -13, 1000]; mid=3 -> -13.
        assert_eq!(seeds.pll_seed, -13);
    }

    #[test]
    fn rolling_window_evicts_old_samples() {
        let mut w = CleanSampleWindow::new();
        // Fill window with PLL = -13.
        for _ in 0..WINDOW_CAPACITY {
            w.observe(CleanSample {
                agc_q9_11: 2048,
                pll_q2_13: -13,
                timing_q5_12: 30000,
            });
        }
        // Now feed WINDOW_CAPACITY samples at PLL = +50.
        let mut last = None;
        for _ in 0..WINDOW_CAPACITY {
            last = w.observe(CleanSample {
                agc_q9_11: 2048,
                pll_q2_13: 50,
                timing_q5_12: 30000,
            });
        }
        // Window is now fully replaced; median should be +50.
        let seeds = last.unwrap();
        assert_eq!(seeds.pll_seed, 50);
        assert_eq!(seeds.samples_in_window, WINDOW_CAPACITY);
        assert_eq!(
            seeds.total_clean_samples as usize,
            2 * WINDOW_CAPACITY,
        );
    }
}

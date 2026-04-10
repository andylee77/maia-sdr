//! P25 LSM (Linear Simulcast Modulation) demodulator.
//!
//! Phase 6D of the Fishball P25 dev plan: a Rust port of the validated
//! Python reference in `tools/p25_lsm_demod.py` and `tools/p25_nid_fec.py`,
//! consuming raw post-DDC IQ from the Phase 6C `iq_dma` ring instead of the
//! existing pre-decoded `dibit_dma` path. The dibit pipeline keeps running
//! in parallel; LSM runs as an independent task that produces its own NID
//! sync events.
//!
//! Pipeline (file-by-file mirror of the Python reference):
//!
//! ```text
//!   iq_dma ring   →  ring::IqRingReader   (i16[] → Complex32[])
//!                       ↓
//!                    filters::decimate_by_2   62.5 → 31.25 kSPS
//!                       ↓
//!                    filters::apply_real_fir_complex(LPF_TAPS_31250)
//!                       ↓
//!                    filters::apply_real_fir_complex(RRC_TAPS_31250)
//!                       ↓
//!                    demod::demod_lsm   (AGC + PLL + Gardner + slicer)
//!                       ↓
//!                    sync::find_sync_events_*  (hard or soft)
//!                       ↓
//!                    nid_fec::decode_nid   (BCH(63,16,11) ML decode)
//!                       ↓
//!                    Vec<SyncEvent>
//! ```
//!
//! Module layout:
//!
//! - `nid_fec`  — BCH(63,16,11) NID FEC (port of `p25_nid_fec.py`)
//! - `filters`  — half-band decimator + LPF + RRC (frozen taps)
//! - `demod`    — LSM demod loop (port of `demod_lsm()`)
//! - `sync`     — hard + soft sync detectors + status-aware NID extractor
//! - `ring`     — `/dev/p25-iq` ring reader (Linux only)
//!
//! See `doc/changes/014_phase6d_lsm_rust_port.md` for the porting log.

pub mod demod;
pub mod filters;
pub mod nid_fec;
pub mod sync;

#[cfg(target_os = "linux")]
pub mod ring;

// Phase 6E.0: golden vector emitter for the HDL port. Test-only,
// writes JSON fixtures to maia-hdl/test/golden_vectors/. See
// `golden_dump.rs` for the file format.
#[cfg(test)]
mod golden_dump;

/// Minimal complex number type for the LSM pipeline.
///
/// Defined locally instead of pulling in `num-complex` as a new dependency
/// because the LSM pipeline only needs add/sub/mul, magnitude, and direct
/// field access for the demod loop. This stays a thin POD wrapper so the
/// compiler can vectorise the inner loops.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
#[repr(C)]
pub struct Complex32 {
    pub re: f32,
    pub im: f32,
}

impl Complex32 {
    #[inline]
    pub const fn new(re: f32, im: f32) -> Self {
        Complex32 { re, im }
    }

    /// Magnitude `sqrt(re*re + im*im)`.
    #[inline]
    pub fn norm(&self) -> f32 {
        (self.re * self.re + self.im * self.im).sqrt()
    }
}

use std::collections::HashMap;
use std::time::Instant;

use demod::{DemodResult, DemodState, P25_SYMBOL_RATE};
use filters::{
    StreamingDecimator2, StreamingFir, LPF_TAPS_31250, POST_DECIMATION_RATE_HZ,
    RRC_TAPS_31250,
};
use sync::{find_sync_events_hard, find_sync_events_soft, SyncEvent};

/// End-to-end streaming LSM demod pipeline.
///
/// Owns the persistent state for each stage so successive calls to
/// `process_iq()` produce a continuous, transient-free output as raw IQ
/// arrives from the iq_dma ring. Construct once at startup, then call
/// `process_iq` from the IRQ-driven reader task.
pub struct LsmPipeline {
    decimator: StreamingDecimator2,
    lpf: StreamingFir,
    rrc: StreamingFir,
    demod_state: DemodState,
}

/// Output of one `LsmPipeline::process_iq` call.
pub struct LsmBatch {
    pub demod: DemodResult,
    pub hard_events: Vec<SyncEvent>,
    pub soft_events: Vec<SyncEvent>,
}

impl LsmPipeline {
    /// Build a fresh pipeline. The demod loop is initialised with the
    /// SDRTrunk default `prev_sym = 0.7+0.7j` initialiser; PLL/timing/AGC
    /// are zero/unity.
    pub fn new() -> Self {
        let sps = POST_DECIMATION_RATE_HZ / P25_SYMBOL_RATE;
        LsmPipeline {
            decimator: StreamingDecimator2::new(),
            lpf: StreamingFir::new(&LPF_TAPS_31250),
            rrc: StreamingFir::new(&RRC_TAPS_31250),
            demod_state: DemodState::new(sps),
        }
    }

    /// Run one chunk of post-DDC IQ (62.5 kSPS) through the full pipeline.
    /// Returns the per-symbol demod output and any sync events found in
    /// this chunk by both the hard and soft detectors.
    pub fn process_iq(&mut self, iq_62k5: &[Complex32]) -> LsmBatch {
        let dec = self.decimator.process(iq_62k5);
        let lpf = self.lpf.process(&dec);
        let rrc = self.rrc.process(&lpf);
        let demod = demod::demod_lsm_with_state(
            &rrc,
            POST_DECIMATION_RATE_HZ,
            &mut self.demod_state,
        );
        let hard_events = find_sync_events_hard(&demod.hard_dibits);
        let soft_events = find_sync_events_soft(&demod.soft_phases, &demod.hard_dibits);
        LsmBatch {
            demod,
            hard_events,
            soft_events,
        }
    }

    /// Reset all streaming state. Call after a long IRQ stall or any time
    /// the input continuity is broken (e.g. iq_dma overflow latched).
    pub fn reset(&mut self) {
        let sps = POST_DECIMATION_RATE_HZ / P25_SYMBOL_RATE;
        self.decimator = StreamingDecimator2::new();
        self.lpf = StreamingFir::new(&LPF_TAPS_31250);
        self.rrc = StreamingFir::new(&RRC_TAPS_31250);
        self.demod_state = DemodState::new(sps);
    }
}

impl Default for LsmPipeline {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Runtime stats for dashboard / API ───────────────────────────────────

/// Cumulative runtime stats for the LSM pipeline.
///
/// Populated by the LSM IRQ task on every wake and shared with the HTTP
/// handler via `Arc<tokio::sync::Mutex<LsmStats>>`, so `GET /api/lsm` can
/// expose the demod output structurally instead of forcing the operator
/// to tail `p25-httpd.log`. This is the implementation of the "wire LSM
/// events into the dashboard" follow-up called out in doc 014.
#[derive(Debug, Clone, Default)]
pub struct LsmStats {
    /// Instant the task first recorded a batch. Used to compute uptime +
    /// steady-state rates. `None` until the first `record_batch` call.
    pub started_at: Option<Instant>,
    /// Most recent instant the task completed a wake. Dashboard uses the
    /// age of this to show "ALIVE" vs "STALLED".
    pub last_wake_at: Option<Instant>,

    /// Total LSM task wakes (== iq_dma interrupts serviced).
    pub wakeups: u64,
    /// Cumulative IQ samples drained from the iq_dma ring at 62.5 kSPS.
    pub iq_samples: u64,
    /// Cumulative post-RRC demodulated dibits at 4800 sym/s.
    pub dibits: u64,
    /// Cumulative hard-detector sync hits across all batches.
    pub hard_events: u64,
    /// Cumulative soft-detector sync hits across all batches.
    pub soft_events: u64,
    /// Number of times the iq_dma overflow latch fired and we reset the
    /// pipeline streaming state. **Known false positive in the Phase 6C
    /// gateware — fires once per sub-buffer regardless of actual drain
    /// rate; tracked here so the operator can confirm the bug is still
    /// present after an HDL fix.** See doc 014 follow-ups.
    pub overflow_resets: u64,

    /// NAC histogram across ALL sync events (hard + soft combined). Top
    /// entry by count is the winning on-air site ID.
    pub nac_hist: HashMap<u16, u64>,

    /// Most recent sync event, for "last decoded NID" display.
    pub last_sync: Option<LastSync>,
}

/// Snapshot of the most recent sync event that the LSM pipeline emitted.
#[derive(Debug, Clone, Copy)]
pub struct LastSync {
    pub at: Instant,
    pub nac: u16,
    pub duid: u8,
    /// True if the BCH(63,16,11) decoder corrected the NID; false means
    /// the raw NAC/DUID are from the uncorrected NID extraction.
    pub fec_corrected: bool,
    /// Hamming distance of the hard-sync match, or -1 if this event came
    /// from the soft detector.
    pub distance: i32,
    /// Soft correlation score, or 0.0 if this event came from the hard
    /// detector.
    pub score: f32,
}

impl LsmStats {
    /// Fold one `LsmBatch` into the cumulative stats. Called by the LSM
    /// tokio task after every `process_iq` call.
    ///
    /// `iq_samples_in` is the number of IQ samples the batch consumed
    /// (the task knows this, `LsmBatch` does not carry it through).
    pub fn record_batch(&mut self, iq_samples_in: usize, batch: &LsmBatch) {
        let now = Instant::now();
        if self.started_at.is_none() {
            self.started_at = Some(now);
        }
        self.last_wake_at = Some(now);
        self.wakeups += 1;
        self.iq_samples += iq_samples_in as u64;
        self.dibits += batch.demod.n_symbols() as u64;
        self.hard_events += batch.hard_events.len() as u64;
        self.soft_events += batch.soft_events.len() as u64;

        for e in batch.hard_events.iter().chain(batch.soft_events.iter()) {
            *self.nac_hist.entry(e.best_nac()).or_insert(0) += 1;
            self.last_sync = Some(LastSync {
                at: now,
                nac: e.best_nac(),
                duid: e.best_duid(),
                fec_corrected: e.fec.is_some(),
                distance: e.distance,
                score: e.score,
            });
        }
    }

    /// Record one iq_dma overflow latch event (Rust side's only reaction
    /// is `LsmPipeline::reset()` and bumping this counter).
    pub fn record_overflow(&mut self) {
        self.overflow_resets += 1;
    }

    /// Return the top `n` NACs sorted by count descending, as
    /// `(nac, count)` tuples. Ties are broken by NAC value ascending.
    pub fn top_nacs(&self, n: usize) -> Vec<(u16, u64)> {
        let mut v: Vec<(u16, u64)> = self
            .nac_hist
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        // Sort by count desc, then NAC asc for stable ordering.
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

#[cfg(test)]
mod stats_tests {
    use super::*;
    use crate::lsm::demod::DemodResult;
    use crate::lsm::sync::SyncEvent;

    fn mk_event(nac: u16, duid: u8) -> SyncEvent {
        SyncEvent {
            symbol_idx: 0,
            distance: 0,
            score: 0.0,
            nac,
            duid,
            nid_raw: 0,
            fec: None,
        }
    }

    fn mk_batch(events: Vec<SyncEvent>) -> LsmBatch {
        LsmBatch {
            demod: DemodResult {
                soft_symbols: Vec::new(),
                soft_phases: Vec::new(),
                hard_dibits: Vec::new(),
                pll_trace: Vec::new(),
                timing_trace: Vec::new(),
                samples_per_symbol: 6.51,
            },
            hard_events: events,
            soft_events: Vec::new(),
        }
    }

    #[test]
    fn record_batch_accumulates_counters_and_nac_hist() {
        let mut stats = LsmStats::default();
        let b = mk_batch(vec![mk_event(0x8A1, 7), mk_event(0x8A1, 7), mk_event(0x12E, 7)]);
        stats.record_batch(8192, &b);
        assert_eq!(stats.wakeups, 1);
        assert_eq!(stats.iq_samples, 8192);
        assert_eq!(stats.hard_events, 3);
        assert_eq!(stats.nac_hist.get(&0x8A1), Some(&2));
        assert_eq!(stats.nac_hist.get(&0x12E), Some(&1));
        assert!(stats.last_sync.is_some());
        let ls = stats.last_sync.unwrap();
        assert_eq!(ls.nac, 0x12E);
        assert_eq!(ls.duid, 7);
    }

    #[test]
    fn top_nacs_sorts_by_count_desc() {
        let mut stats = LsmStats::default();
        stats.record_batch(
            1,
            &mk_batch(vec![
                mk_event(0x8A1, 7),
                mk_event(0x8A1, 7),
                mk_event(0x8A1, 7),
                mk_event(0x12E, 7),
                mk_event(0x12E, 7),
                mk_event(0xABB, 7),
            ]),
        );
        let top = stats.top_nacs(3);
        assert_eq!(top, vec![(0x8A1, 3), (0x12E, 2), (0xABB, 1)]);
    }

    #[test]
    fn overflow_counter_increments_independently() {
        let mut stats = LsmStats::default();
        stats.record_overflow();
        stats.record_overflow();
        stats.record_overflow();
        assert_eq!(stats.overflow_resets, 3);
        assert_eq!(stats.wakeups, 0); // overflow should not touch wake count
    }
}

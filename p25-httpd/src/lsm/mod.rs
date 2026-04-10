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

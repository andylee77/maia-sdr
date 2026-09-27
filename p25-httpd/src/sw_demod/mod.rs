//! Live software P25 demodulator (Stage 2B).
//!
//! Streaming counterpart to the offline `software_decode_tests.rs` harness.
//! Drains the wideband_iq_dma ring (8 MSPS pre-DDC IQ from `rxiq_cdc`),
//! runs:
//!
//! ```text
//!   MultistageDdc         (NCO + multistage FIR decimation, 8 MSPS → 25 kSPS)
//!     → LsmPipeline        (lsm/ — decim/2 → LPF → RRC → Costas+AGC+Gardner+slicer+diff)
//!     → ControlChannelDecoder (framer)
//!     → ImbeForwarder      (existing — feeds vocoder + audio_tx)
//! ```
//!
//! See `app/sw_demod_task.rs` for the tokio task wiring + lifetime.

pub mod ddc;
pub mod halfband_ddc;
pub mod multistage_ddc;

pub use ddc::StreamingSoftwareDdc;
pub use halfband_ddc::halfband_ddc_to_25k;
pub use multistage_ddc::MultistageDdc;

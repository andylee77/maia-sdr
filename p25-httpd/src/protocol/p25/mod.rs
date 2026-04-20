//! P25 Phase 1 protocol decoder
//!
//! Decodes the P25 control channel from a dibit stream produced by the FPGA.

pub mod control_channel;
pub mod events;
pub mod fec;
pub mod traffic_manager;
pub mod tsbk;
pub mod types;
pub mod voice_frame;
pub mod wire;

// Offline SDRTrunk `.bits` cross-check for the TDULC LCW parser +
// framer integration. Hidden behind `#[cfg(test)]` so it only shows
// up in test builds; hidden again behind `P25_SDRTRUNK_DIR` env var
// at runtime so it's a no-op when SDRTrunk captures aren't present.
#[cfg(test)]
mod sdrtrunk_bits_test;

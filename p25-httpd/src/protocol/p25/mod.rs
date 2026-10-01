//! P25 Phase 1 protocol decoder
//!
//! Decodes the P25 control channel from a dibit stream produced by the FPGA.

/// Change 071b: software C4FM demodulation (SDRTrunk port).
pub mod c4fm;
mod c4fm_filters;
mod c4fm_interp_taps;
pub mod control_channel;
pub mod events;
pub mod fec;
pub mod traffic_chain;
pub mod tsbk;
/// Change 074: packet data units (PDUs).
pub mod pdu;
pub mod types;
pub mod voice_frame;
pub mod wire;

// Offline SDRTrunk `.bits` cross-check for the TDULC LCW parser +
// framer integration. Hidden behind `#[cfg(test)]` so it only shows
// up in test builds; hidden again behind `P25_SDRTRUNK_DIR` env var
// at runtime so it's a no-op when SDRTrunk captures aren't present.
#[cfg(test)]
mod sdrtrunk_bits_test;

// Shared test-only constants (Clay County NAC/WACN/freq, Duval County
// NAC/freq, etc.). Module is `#[cfg(test)]` so it does not leak into
// production builds.
#[cfg(test)]
pub(crate) mod test_fixtures;

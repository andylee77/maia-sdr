//! Linux-only hardware glue: UIO, FPGA IP-core, IIO (AD9361), DMA
//! ring buffers. The entire module is a no-op on non-Linux host
//! builds so developer-side `cargo check` still works.
//!
//! The `ddc_presets` submodule is pure data (coefficient tables +
//! the `DdcPreset` struct) with no hardware dependency, so it is
//! compiled on every platform — AppState carries a preset handle
//! even on host builds where `fpga.rs` is stubbed out.

/// Change 059: P25 core version register decoding (host-tested).
pub mod core_version;
/// Change 066: DDC FIR coefficient RAM images (host-tested).
pub mod ddc_fir_ram;
pub mod ddc_presets;
/// Post-DDC sample rates (single source for every rate label).
pub mod ddc_rate;
/// Change 054: portable dibit ring position math + production clock
/// (host-tested; used by the Linux readers and `fpga::IpCore`).
pub mod dibit_ring;
/// Change 066: traffic chains ("lanes") and how many a core offers.
pub mod traffic_lane;

#[cfg(target_os = "linux")]
pub mod fpga;
#[cfg(target_os = "linux")]
pub mod iio;
#[cfg(target_os = "linux")]
pub mod rxbuffer;
#[cfg(target_os = "linux")]
pub mod uio;

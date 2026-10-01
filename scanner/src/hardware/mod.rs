//! Drivers: the AD9361 receiver (IIO), the P25 core in the FPGA (registers, DMA rings,
//! interrupt) and the DDC presets. Nothing here decides anything; `radio` drives it.

pub mod ad9361;
pub mod core_version;
#[cfg(target_os = "linux")]
pub mod mmio;
pub mod p25core;
pub mod presets;

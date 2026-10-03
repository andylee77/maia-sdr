//! Drivers: the AD9361 receiver (IIO), the radio core in the FPGA (registers, DMA rings,
//! interrupt) and the DDC presets. Nothing here decides anything; `radio` drives it.

pub mod ad9361;
#[cfg(target_os = "linux")]
pub mod mmio;
pub mod presets;
pub mod radiocore;

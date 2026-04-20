//! Linux-only hardware glue: UIO, FPGA IP-core, IIO (AD9361), DMA
//! ring buffers. The entire module is a no-op on non-Linux host
//! builds so developer-side `cargo check` still works.

#[cfg(target_os = "linux")]
pub mod fpga;
#[cfg(target_os = "linux")]
pub mod iio;
#[cfg(target_os = "linux")]
pub mod rxbuffer;
#[cfg(target_os = "linux")]
pub mod uio;

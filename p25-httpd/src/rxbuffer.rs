//! DMA receive buffer driver.
//!
//! Provides memory mapping and cache invalidation for DMA ring buffers
//! managed by the maia-sdr kernel module. The P25 FPGA writes dibit data
//! into these buffers via AXI HP ports; the CPU reads them after cache
//! invalidation.
//!
//! Adapted from maia-httpd/src/rxbuffer.rs.

use anyhow::Result;
use std::os::unix::io::{AsRawFd, RawFd};
use tokio::fs;

/// Receive DMA ring buffer.
///
/// Corresponds to a rxbuffer device (e.g., `/dev/p25-dibit`).
#[derive(Debug)]
pub struct RxBuffer {
    _file: fs::File,
    fd: RawFd,
    buffer: *mut libc::c_void,
    buffer_size: usize,
    num_buffers: usize,
}

unsafe impl Send for RxBuffer {}

impl RxBuffer {
    /// Opens an rxbuffer device by name (e.g., `"p25-dibit"`).
    ///
    /// Reads buffer geometry from `/sys/class/maia-sdr/<name>/device/`.
    pub async fn new(name: &str) -> Result<RxBuffer> {
        let file = fs::File::open(format!("/dev/{name}")).await?;
        let fd = file.as_raw_fd();
        let buffer_size = usize::from_str_radix(
            fs::read_to_string(format!(
                "/sys/class/maia-sdr/{name}/device/buffer_size"
            ))
            .await?
            .trim_end()
            .trim_start_matches("0x"),
            16,
        )?;
        let num_buffers = fs::read_to_string(format!(
            "/sys/class/maia-sdr/{name}/device/num_buffers"
        ))
        .await?
        .trim_end()
        .parse::<usize>()?;
        let buffer = unsafe {
            match libc::mmap(
                std::ptr::null_mut::<libc::c_void>(),
                buffer_size * num_buffers,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            ) {
                libc::MAP_FAILED => anyhow::bail!("mmap rxbuffer failed"),
                x => x,
            }
        };
        Ok(RxBuffer {
            _file: file,
            fd,
            buffer,
            buffer_size,
            num_buffers,
        })
    }

    /// Size in bytes of each buffer in the ring.
    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    /// Number of buffers in the ring.
    pub fn num_buffers(&self) -> usize {
        self.num_buffers
    }

    /// Returns a slice for one buffer in the ring.
    ///
    /// # Panics
    ///
    /// Panics if `num_buffer >= num_buffers()`.
    pub fn buffer_as_slice(&self, num_buffer: usize) -> &[u8] {
        assert!(num_buffer < self.num_buffers);
        unsafe {
            std::slice::from_raw_parts(
                self.buffer.add(num_buffer * self.buffer_size) as *const u8,
                self.buffer_size,
            )
        }
    }

    /// Invalidates the CPU cache for one buffer.
    ///
    /// Must be called before reading a buffer that the FPGA may have written
    /// to since the last invalidation (non-coherent AXI HP writes).
    pub fn cache_invalidate(&self, num_buffer: usize) -> Result<()> {
        assert!(num_buffer < self.num_buffers);
        unsafe { ioctl::maia_kmod_cacheinv(self.fd, num_buffer as _) }?;
        Ok(())
    }
}

mod ioctl {
    use nix::ioctl_write_int;

    const MAIA_SDR_IOC_MAGIC: u8 = b'M';
    const MAIA_SDR_CACHEINV: u8 = 0;

    ioctl_write_int!(maia_kmod_cacheinv, MAIA_SDR_IOC_MAGIC, MAIA_SDR_CACHEINV);
}

impl Drop for RxBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.buffer, self.buffer_size * self.num_buffers);
        }
    }
}

//! Memory-mapped I/O on Linux: UIO devices (the P25 core's registers and its interrupt) and
//! maia-kmod's rxbuffer devices (the DMA rings, mapped cacheable, invalidated per sub-buffer).

use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A UIO device: its register maps and its interrupt.
#[derive(Debug)]
pub struct Uio {
    num: usize,
    file: fs::File,
}

/// One mmap of a UIO map, unmapped on drop.
#[derive(Debug)]
pub struct Mapping {
    base: *mut libc::c_void,
    effective: *mut libc::c_void,
    size: usize,
}

// The mapping is device memory shared by every holder; access goes through volatile register
// cells.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Uio {
    pub async fn open(name: &str) -> Result<Uio> {
        let mut entries = fs::read_dir("/sys/class/uio").await?;
        while let Some(entry) = entries.next_entry().await? {
            let Some(num) = entry.file_name().to_str().and_then(|n| n.strip_prefix("uio")?.parse().ok()) else {
                continue;
            };
            if fs::read_to_string(entry.path().join("name")).await?.trim_end() == name {
                let file = fs::OpenOptions::new().read(true).write(true).open(format!("/dev/uio{num}")).await?;
                return Ok(Uio { num, file });
            }
        }
        bail!("UIO device {name:?} not found")
    }

    pub async fn map(&self, index: usize) -> Result<Mapping> {
        let size = self.map_attr(index, "size").await?;
        let offset = self.map_attr(index, "offset").await?;
        // The UIO convention: map N is at page offset N.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                self.file.as_raw_fd(),
                (index * page_size::get()) as libc::off_t,
            )
        };
        if base == libc::MAP_FAILED {
            bail!("mmap of UIO {} map {index} failed", self.num);
        }
        let effective = unsafe { base.add(offset) };
        Ok(Mapping { base, effective, size })
    }

    async fn map_attr(&self, index: usize, attr: &str) -> Result<usize> {
        let path = format!("/sys/class/uio/uio{}/maps/map{index}/{attr}", self.num);
        let text = fs::read_to_string(&path).await.with_context(|| path.clone())?;
        let hex = text.trim().strip_prefix("0x").with_context(|| format!("{path}: {text:?}"))?;
        Ok(usize::from_str_radix(hex, 16)?)
    }

    pub async fn irq_enable(&mut self) -> Result<()> {
        self.file.write_all(&1u32.to_ne_bytes()).await?;
        Ok(())
    }

    /// Wait for the next interrupt; returns the interrupt count.
    pub async fn irq_wait(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        self.file.read_exact(&mut bytes).await?;
        Ok(u32::from_ne_bytes(bytes))
    }
}

impl Mapping {
    pub fn addr(&self) -> *mut libc::c_void {
        self.effective
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.size);
        }
    }
}

/// A DMA ring of `num_buffers` sub-buffers of `buffer_size` bytes (`/dev/<name>`).
#[derive(Debug)]
pub struct RxBuffer {
    _file: fs::File,
    fd: RawFd,
    buffer: *mut libc::c_void,
    buffer_size: usize,
    num_buffers: usize,
}

// Read-only mapping of DMA memory; reads are preceded by a cache invalidate of the sub-buffer.
unsafe impl Send for RxBuffer {}
unsafe impl Sync for RxBuffer {}

impl RxBuffer {
    pub async fn open(name: &str) -> Result<RxBuffer> {
        let file = fs::File::open(format!("/dev/{name}")).await.with_context(|| format!("/dev/{name}"))?;
        let fd = file.as_raw_fd();
        let dev = Path::new("/sys/class/maia-sdr").join(name).join("device");
        let size_text = fs::read_to_string(dev.join("buffer_size")).await?;
        let buffer_size = usize::from_str_radix(size_text.trim().trim_start_matches("0x"), 16)?;
        let num_buffers: usize = fs::read_to_string(dev.join("num_buffers")).await?.trim().parse()?;
        let buffer = unsafe {
            libc::mmap(std::ptr::null_mut(), buffer_size * num_buffers, libc::PROT_READ, libc::MAP_SHARED, fd, 0)
        };
        if buffer == libc::MAP_FAILED {
            bail!("mmap of /dev/{name} failed");
        }
        Ok(RxBuffer { _file: file, fd, buffer, buffer_size, num_buffers })
    }

    pub fn num_buffers(&self) -> usize {
        self.num_buffers
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn buffer(&self, index: usize) -> &[u8] {
        assert!(index < self.num_buffers);
        unsafe { std::slice::from_raw_parts(self.buffer.add(index * self.buffer_size) as *const u8, self.buffer_size) }
    }

    /// Must precede every read of newly written bytes of `index` (the ring is mapped cacheable).
    pub fn cache_invalidate(&self, index: usize) -> Result<()> {
        assert!(index < self.num_buffers);
        unsafe { ioctl::cache_invalidate(self.fd, index as _) }?;
        Ok(())
    }
}

impl Drop for RxBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.buffer, self.buffer_size * self.num_buffers);
        }
    }
}

mod ioctl {
    // maia-kmod: _IOW('M', 0, int).
    nix::ioctl_write_int!(cache_invalidate, b'M', 0);
}

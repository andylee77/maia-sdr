//! UIO device access.
//!
//! This module is used to work with UIO devices.
//! Adapted from maia-httpd/src/uio.rs.

use anyhow::Result;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// UIO device.
#[derive(Debug)]
pub struct Uio {
    num: usize,
    file: fs::File,
}

/// UIO device mapping.
///
/// Corresponds to a memory-mapped IO region of a UIO device.
/// Dropping this struct unmaps the region.
#[derive(Debug, Clone)]
pub struct Mapping {
    base: *mut libc::c_void,
    effective: *mut libc::c_void,
    map_size: usize,
}

// Safety: the mapped region is shared with hardware but not with other Rust
// threads. Access through the PAC RegisterBlock uses volatile reads/writes.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Uio {
    /// Opens a UIO device using its number (`/dev/uio<num>`).
    pub async fn from_num(num: usize) -> Result<Uio> {
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(format!("/dev/uio{num}"))
            .await?;
        Ok(Uio { num, file })
    }

    /// Opens a UIO device by searching `/sys/class/uio` for a matching name.
    pub async fn from_name(name: &str) -> Result<Uio> {
        match Self::find_by_name(name).await? {
            Some(num) => Self::from_num(num).await,
            None => anyhow::bail!("UIO device '{name}' not found"),
        }
    }

    async fn find_by_name(name: &str) -> Result<Option<usize>> {
        let mut entries = fs::read_dir(Path::new("/sys/class/uio")).await?;
        while let Some(entry) = entries.next_entry().await? {
            let file_name = entry.file_name();
            let uio = file_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("file name is not valid UTF8"))?;
            if let Some(num) = uio
                .strip_prefix("uio")
                .and_then(|a| a.parse::<usize>().ok())
            {
                let mut path = entry.path();
                path.push("name");
                let this_name = fs::read_to_string(path).await?;
                if this_name.trim_end() == name {
                    return Ok(Some(num));
                }
            }
        }
        Ok(None)
    }

    /// Maps a memory mapping of a UIO device.
    ///
    /// The `mapping` number corresponds to `/sys/class/uio/uio*/maps/map<N>`.
    /// Devices with a single mapping use `0`.
    pub async fn map_mapping(&self, mapping: usize) -> Result<Mapping> {
        let offset = mapping * page_size::get();
        let fd = self.file.as_raw_fd();
        let map_size = self.map_size(mapping).await?;

        let base = unsafe {
            match libc::mmap(
                std::ptr::null_mut::<libc::c_void>(),
                map_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                offset as libc::off_t,
            ) {
                libc::MAP_FAILED => anyhow::bail!("mmap UIO failed"),
                x => x,
            }
        };
        let effective_offset = isize::try_from(self.map_offset(mapping).await?)?;
        let effective = unsafe { base.offset(effective_offset) };
        Ok(Mapping {
            base,
            effective,
            map_size,
        })
    }

    async fn read_mapping_hex(&self, mapping: usize, fname: &str) -> Result<usize> {
        let n = fs::read_to_string(format!(
            "/sys/class/uio/uio{}/maps/map{}/{}",
            self.num, mapping, fname
        ))
        .await?;
        Ok(usize::from_str_radix(
            n.strip_prefix("0x")
                .ok_or_else(|| anyhow::anyhow!("prefix 0x not present"))?
                .trim_end(),
            16,
        )?)
    }

    /// Returns the size of a UIO mapping.
    pub async fn map_size(&self, mapping: usize) -> Result<usize> {
        self.read_mapping_hex(mapping, "size").await
    }

    /// Returns the offset of a UIO mapping.
    pub async fn map_offset(&self, mapping: usize) -> Result<usize> {
        self.read_mapping_hex(mapping, "offset").await
    }

    /// Returns the physical address of a UIO mapping.
    pub async fn map_addr(&self, mapping: usize) -> Result<usize> {
        self.read_mapping_hex(mapping, "addr").await
    }

    /// Enables interrupts by writing `1` to the UIO device file.
    pub async fn irq_enable(&mut self) -> Result<()> {
        let bytes = 1u32.to_ne_bytes();
        self.file.write_all(&bytes).await?;
        Ok(())
    }

    /// Disables interrupts by writing `0` to the UIO device file.
    pub async fn irq_disable(&mut self) -> Result<()> {
        let bytes = 0u32.to_ne_bytes();
        self.file.write_all(&bytes).await?;
        Ok(())
    }

    /// Waits for an interrupt by reading from the UIO device file.
    pub async fn irq_wait(&mut self) -> Result<u32> {
        let mut bytes = [0; 4];
        self.file.read_exact(&mut bytes).await?;
        Ok(u32::from_ne_bytes(bytes))
    }
}

impl Mapping {
    /// Returns a pointer to the effective virtual address of the mapping.
    pub fn addr(&self) -> *mut libc::c_void {
        self.effective
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base, self.map_size);
        }
    }
}

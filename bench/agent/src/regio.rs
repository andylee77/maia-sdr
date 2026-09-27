//! Physical memory / register mapping.
//!
//! `PhysMap` maps a physical window through `/dev/mem` with `O_SYNC`
//! (uncached, strongly ordered on ARM for non-RAM pages). All register
//! accesses go through `RegIo`, which the unit tests replace with `MockIo`.

use crate::err::{AResult, AgentError, Code};

pub trait RegIo {
    fn read32(&mut self, off: u32) -> u32;
    fn write32(&mut self, off: u32, v: u32);
}

/// A mapped physical window.
pub struct PhysMap {
    #[allow(dead_code)]
    map_base: *mut u8,
    #[allow(dead_code)]
    map_len: usize,
    ptr: *mut u8,
    len: usize,
    pub phys: u64,
    pub writable: bool,
}

// SAFETY: the mapping is only used from the thread that owns the PhysMap.
unsafe impl Send for PhysMap {}

impl PhysMap {
    #[cfg(target_os = "linux")]
    pub fn open(phys: u64, len: usize, writable: bool) -> AResult<PhysMap> {
        use std::ffi::CString;
        let page = 4096u64;
        let map_off = phys & !(page - 1);
        let delta = (phys - map_off) as usize;
        let map_len = ((delta + len + page as usize - 1) / page as usize) * page as usize;
        let path = CString::new("/dev/mem").unwrap();
        let flags = if writable { libc::O_RDWR } else { libc::O_RDONLY } | libc::O_SYNC;
        // SAFETY: plain syscalls with valid arguments.
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = std::io::Error::last_os_error();
            return Err(AgentError::new(
                Code::Precondition,
                format!("open /dev/mem: {e} (run as root)"),
            ));
        }
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                map_len,
                prot,
                libc::MAP_SHARED,
                fd,
                map_off as libc::off_t,
            )
        };
        unsafe { libc::close(fd) };
        if p == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            return Err(AgentError::new(
                Code::Precondition,
                format!("mmap /dev/mem at 0x{phys:08X} (+0x{len:X}): {e}"),
            ));
        }
        let base = p as *mut u8;
        Ok(PhysMap {
            map_base: base,
            map_len,
            ptr: unsafe { base.add(delta) },
            len,
            phys,
            writable,
        })
    }

    /// Maps map0 of a UIO device (`/dev/uioN`), as p25-httpd does. The
    /// mapping's physical address must equal `expect_phys`.
    #[cfg(target_os = "linux")]
    pub fn open_uio(num: usize, expect_phys: u64, len: usize, writable: bool) -> AResult<PhysMap> {
        use std::ffi::CString;
        let rd = |f: &str| -> Option<u64> {
            let s = std::fs::read_to_string(format!("/sys/class/uio/uio{num}/maps/map0/{f}")).ok()?;
            crate::util::parse_u64(s.trim())
        };
        let addr = rd("addr").unwrap_or(0);
        let size = rd("size").unwrap_or(0) as usize;
        let moff = rd("offset").unwrap_or(0) as usize;
        if addr != expect_phys || size < len {
            return Err(AgentError::new(
                Code::WrongImage,
                format!("uio{num} map0 is 0x{addr:08X}+0x{size:X}, expected 0x{expect_phys:08X}+0x{len:X}"),
            ));
        }
        let path = CString::new(format!("/dev/uio{num}")).unwrap();
        let flags = if writable { libc::O_RDWR } else { libc::O_RDONLY };
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(AgentError::new(
                Code::Precondition,
                format!("open /dev/uio{num}: {}", std::io::Error::last_os_error()),
            ));
        }
        let prot = if writable {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };
        let p = unsafe { libc::mmap(std::ptr::null_mut(), size, prot, libc::MAP_SHARED, fd, 0) };
        unsafe { libc::close(fd) };
        if p == libc::MAP_FAILED {
            return Err(AgentError::new(
                Code::Precondition,
                format!("mmap /dev/uio{num}: {}", std::io::Error::last_os_error()),
            ));
        }
        let base = p as *mut u8;
        Ok(PhysMap {
            map_base: base,
            map_len: size,
            ptr: unsafe { base.add(moff) },
            len,
            phys: expect_phys,
            writable,
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open_uio(num: usize, _expect_phys: u64, _len: usize, _writable: bool) -> AResult<PhysMap> {
        Err(AgentError::new(
            Code::Unsupported,
            format!("/dev/uio{num} access is only available on the board (Linux)"),
        ))
    }

    #[cfg(not(target_os = "linux"))]
    pub fn open(phys: u64, _len: usize, _writable: bool) -> AResult<PhysMap> {
        Err(AgentError::new(
            Code::Unsupported,
            format!("/dev/mem access (0x{phys:08X}) is only available on the board (Linux)"),
        ))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[inline]
    pub fn rd32(&self, off: usize) -> u32 {
        assert!(off % 4 == 0 && off + 4 <= self.len, "PhysMap read out of range");
        // SAFETY: bounds checked above; the mapping is valid for `len` bytes.
        unsafe { std::ptr::read_volatile(self.ptr.add(off) as *const u32) }
    }

    #[inline]
    pub fn wr32(&self, off: usize, v: u32) {
        assert!(self.writable, "PhysMap is read-only");
        assert!(off % 4 == 0 && off + 4 <= self.len, "PhysMap write out of range");
        // SAFETY: bounds checked above.
        unsafe { std::ptr::write_volatile(self.ptr.add(off) as *mut u32, v) }
    }

    /// Bulk copy out of the window (memcpy; fine for uncached normal and
    /// strongly-ordered memory, all accesses are aligned).
    pub fn copy_out(&self, off: usize, dst: &mut [u8]) {
        assert!(off + dst.len() <= self.len, "PhysMap copy out of range");
        // SAFETY: bounds checked.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.add(off), dst.as_mut_ptr(), dst.len()) }
    }
}

impl Drop for PhysMap {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::munmap(self.map_base as *mut libc::c_void, self.map_len);
        }
    }
}

impl RegIo for PhysMap {
    fn read32(&mut self, off: u32) -> u32 {
        self.rd32(off as usize)
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.wr32(off as usize, v)
    }
}

impl<T: RegIo + ?Sized> RegIo for &mut T {
    fn read32(&mut self, off: u32) -> u32 {
        (**self).read32(off)
    }
    fn write32(&mut self, off: u32, v: u32) {
        (**self).write32(off, v)
    }
}

impl<T: RegIo + ?Sized> RegIo for Box<T> {
    fn read32(&mut self, off: u32) -> u32 {
        (**self).read32(off)
    }
    fn write32(&mut self, off: u32, v: u32) {
        (**self).write32(off, v)
    }
}

/// In-memory register file for tests. `on_write` can emulate hardware
/// behaviour (e.g. SNAP_REQ -> SNAP_ACK).
#[cfg(test)]
pub struct MockIo {
    pub regs: std::collections::HashMap<u32, u32>,
    pub reads: Vec<u32>,
    pub writes: Vec<(u32, u32)>,
    #[allow(clippy::type_complexity)]
    pub on_write: Option<Box<dyn FnMut(&mut std::collections::HashMap<u32, u32>, u32, u32)>>,
}

#[cfg(test)]
impl MockIo {
    pub fn new() -> Self {
        MockIo {
            regs: Default::default(),
            reads: Vec::new(),
            writes: Vec::new(),
            on_write: None,
        }
    }
}

#[cfg(test)]
impl RegIo for MockIo {
    fn read32(&mut self, off: u32) -> u32 {
        self.reads.push(off);
        *self.regs.get(&off).unwrap_or(&0)
    }
    fn write32(&mut self, off: u32, v: u32) {
        self.writes.push((off, v));
        if let Some(cb) = self.on_write.as_mut() {
            cb(&mut self.regs, off, v);
        } else {
            self.regs.insert(off, v);
        }
    }
}

//! Ring-buffer access: geometry discovery, cached (maia-kmod rxbuffer mmap
//! + cache-invalidate ioctl) and uncached (/dev/mem O_SYNC) mappings, the
//! legacy last_buffer reader logic and the ring v2 consumer protocol
//! (design doc section 7). The pure bookkeeping is host-testable.

use crate::err::{AResult, AgentError, Code};
use crate::regio::PhysMap;
use crate::sys;
use crate::util::read_trim;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingKind {
    P25Wideband,
    HwvalLegacy,
    HwvalV2,
}

#[derive(Debug, Clone)]
pub struct RingDef {
    pub kind: RingKind,
    pub name: &'static str,
    pub dev: String,
    pub phys: u64,
    pub subbuf_bytes: usize,
    pub num_subbufs: usize,
}

impl RingDef {
    pub fn bytes(&self) -> usize {
        self.subbuf_bytes * self.num_subbufs
    }
}

pub fn parse_ring(name: &str) -> AResult<RingKind> {
    Ok(match name {
        "p25-wideband" | "p25_wideband" | "p25-wideband-iq" | "wideband" => RingKind::P25Wideband,
        "hwval-legacy" | "legacy" => RingKind::HwvalLegacy,
        "hwval-v2" | "hwval-ringv2" | "ringv2" | "v2" => RingKind::HwvalV2,
        _ => {
            return Err(AgentError::new(
                Code::Usage,
                format!("unknown ring '{name}' (p25-wideband | hwval-legacy | hwval-v2)"),
            ))
        }
    })
}

/// Physical base of an rxbuffer device from its DT memory-region phandle.
pub fn rxbuffer_phys(dev: &str) -> Option<u64> {
    let ph = std::fs::read(format!("/sys/class/maia-sdr/{dev}/device/of_node/memory-region")).ok()?;
    if ph.len() < 4 {
        return None;
    }
    let want = u32::from_be_bytes([ph[0], ph[1], ph[2], ph[3]]);
    let root = std::path::Path::new("/proc/device-tree/reserved-memory");
    for e in std::fs::read_dir(root).ok()?.flatten() {
        if let Ok(p) = std::fs::read(e.path().join("phandle")) {
            if p.len() >= 4 && u32::from_be_bytes([p[0], p[1], p[2], p[3]]) == want {
                let name = e.file_name().to_string_lossy().to_string();
                return sys::reserved_memory().into_iter().find(|r| r.node == name).map(|r| r.base);
            }
        }
    }
    None
}

fn sysfs_geometry(dev: &str) -> Option<(usize, usize)> {
    let bs = read_trim(format!("/sys/class/maia-sdr/{dev}/device/buffer_size"))?;
    let bs = usize::from_str_radix(bs.trim_start_matches("0x"), 16).ok()?;
    let n = read_trim(format!("/sys/class/maia-sdr/{dev}/device/num_buffers"))?
        .parse()
        .ok()?;
    Some((bs, n))
}

/// Ring definition with defaults from the design doc, refined from sysfs/DT.
pub fn ring_def(kind: RingKind, dev: Option<String>, phys: Option<u64>) -> RingDef {
    let (name, ddev, dphys) = match kind {
        RingKind::P25Wideband => ("p25-wideband", "p25-wideband-iq", 0x2200_0000u64),
        RingKind::HwvalLegacy => ("hwval-legacy", "hwval-legacy", 0x2200_0000),
        RingKind::HwvalV2 => ("hwval-v2", "hwval-ringv2", 0x2000_0000),
    };
    let dev = dev.unwrap_or_else(|| ddev.to_string());
    let (subbuf_bytes, num_subbufs) = sysfs_geometry(&dev).unwrap_or((1 << 20, 16));
    let phys = phys.or_else(|| rxbuffer_phys(&dev)).unwrap_or(dphys);
    RingDef {
        kind,
        name,
        dev,
        phys,
        subbuf_bytes,
        num_subbufs,
    }
}

// ── mappings ──────────────────────────────────────────────────────────

/// maia-kmod rxbuffer: read-only mmap + per-sub-buffer cache invalidate.
pub struct RxBuffer {
    #[allow(dead_code)]
    fd: i32,
    ptr: *const u8,
    pub subbuf_bytes: usize,
    pub num: usize,
}

#[cfg(target_os = "linux")]
impl RxBuffer {
    pub fn open(dev: &str, subbuf_bytes: usize, num: usize) -> AResult<RxBuffer> {
        use std::ffi::CString;
        let path = format!("/dev/{dev}");
        let c = CString::new(path.clone()).unwrap();
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(AgentError::new(
                Code::NoDevice,
                format!("open {path}: {}", std::io::Error::last_os_error()),
            ));
        }
        let len = subbuf_bytes * num;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0)
        };
        if p == libc::MAP_FAILED {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            let hint = if e.raw_os_error() == Some(libc::EINVAL) {
                " (maia-kmod allows one mapping per device: is the scanner running? use `maint enter` or --mapping uncached)"
            } else {
                ""
            };
            return Err(AgentError::new(Code::Precondition, format!("mmap {path}: {e}{hint}")));
        }
        Ok(RxBuffer {
            fd,
            ptr: p as *const u8,
            subbuf_bytes,
            num,
        })
    }

    /// MAIA_SDR_IOC_CACHEINV = _IOW('M', 0, int): L1 then L2 invalidate of
    /// one sub-buffer (the kmod's order, see F8).
    pub fn invalidate(&self, idx: usize) -> AResult<()> {
        const IOC: u32 = (1 << 30) | (4 << 16) | ((b'M' as u32) << 8);
        let r = unsafe { libc::ioctl(self.fd, IOC as _, idx as libc::c_int) };
        if r != 0 {
            return Err(AgentError::new(
                Code::Error,
                format!("cache invalidate ioctl (buffer {idx}): {}", std::io::Error::last_os_error()),
            ));
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl RxBuffer {
    pub fn open(dev: &str, _subbuf_bytes: usize, _num: usize) -> AResult<RxBuffer> {
        Err(AgentError::new(
            Code::Unsupported,
            format!("rxbuffer /dev/{dev} is only available on the board"),
        ))
    }
    pub fn invalidate(&self, _idx: usize) -> AResult<()> {
        Ok(())
    }
}

impl Drop for RxBuffer {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.subbuf_bytes * self.num);
            libc::close(self.fd);
        }
    }
}

pub enum Mapping {
    Cached(RxBuffer),
    Uncached(PhysMap),
}

impl Mapping {
    pub fn open(def: &RingDef, mapping: &str) -> AResult<Mapping> {
        match mapping {
            "cached" => Ok(Mapping::Cached(RxBuffer::open(&def.dev, def.subbuf_bytes, def.num_subbufs)?)),
            "uncached" => {
                let region_ok = sys::reserved_memory()
                    .iter()
                    .any(|r| def.phys >= r.base && def.phys + def.bytes() as u64 <= r.base + r.size);
                if !region_ok && !sys::reserved_memory().is_empty() {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!(
                            "0x{:08X}+0x{:X} is not inside a reserved-memory region; refusing /dev/mem mapping",
                            def.phys,
                            def.bytes()
                        ),
                    ));
                }
                Ok(Mapping::Uncached(PhysMap::open(def.phys, def.bytes(), false)?))
            }
            _ => Err(AgentError::new(Code::Usage, "--mapping must be cached or uncached")),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Mapping::Cached(_) => "cached",
            Mapping::Uncached(_) => "uncached",
        }
    }

    /// Invalidates (cached) and copies one whole sub-buffer.
    pub fn copy_subbuf(&self, idx: usize, subbuf_bytes: usize, dst: &mut [u8]) -> AResult<()> {
        let off = idx * subbuf_bytes;
        match self {
            Mapping::Cached(rb) => {
                rb.invalidate(idx)?;
                // SAFETY: idx < num (caller), mapping covers num*subbuf bytes.
                unsafe { std::ptr::copy_nonoverlapping(rb.ptr.add(off), dst.as_mut_ptr(), subbuf_bytes) };
            }
            Mapping::Uncached(pm) => pm.copy_out(off, &mut dst[..subbuf_bytes]),
        }
        Ok(())
    }

    /// Copies an arbitrary byte range (ring v2); for the cached mapping the
    /// covering sub-buffers are invalidated first.
    pub fn copy_range(&self, off: usize, dst: &mut [u8]) -> AResult<()> {
        match self {
            Mapping::Cached(rb) => {
                let first = off / rb.subbuf_bytes;
                let last = (off + dst.len()).saturating_sub(1) / rb.subbuf_bytes;
                for i in first..=last.min(rb.num - 1) {
                    rb.invalidate(i)?;
                }
                if off + dst.len() > rb.subbuf_bytes * rb.num {
                    return Err(AgentError::new(Code::Error, "ring range outside the rxbuffer mapping"));
                }
                unsafe { std::ptr::copy_nonoverlapping(rb.ptr.add(off), dst.as_mut_ptr(), dst.len()) };
            }
            Mapping::Uncached(pm) => pm.copy_out(off, dst),
        }
        Ok(())
    }
}

// ── legacy reader bookkeeping ─────────────────────────────────────────

/// Tracks `last_buffer` between polls and yields the sub-buffers that
/// became complete, oldest first, with the backlog behind each.
#[derive(Debug, Clone)]
pub struct LegacyTracker {
    pub n: u32,
    pub prev: Option<u32>,
    pub polls: u64,
    pub wakes: u64,
    pub delta_hist: Vec<u64>,
}

impl LegacyTracker {
    pub fn new(n: u32) -> Self {
        LegacyTracker {
            n,
            prev: None,
            polls: 0,
            wakes: 0,
            delta_hist: vec![0; n as usize],
        }
    }

    /// Returns [(index, backlog_after_this_one)].
    pub fn advance(&mut self, lb: u32) -> Vec<(u32, u32)> {
        self.polls += 1;
        let lb = lb % self.n;
        let prev = match self.prev {
            None => {
                self.prev = Some(lb);
                return Vec::new();
            }
            Some(p) => p,
        };
        if lb == prev {
            return Vec::new();
        }
        self.wakes += 1;
        let delta = (lb + self.n - prev) % self.n;
        self.delta_hist[delta as usize] += 1;
        self.prev = Some(lb);
        (1..=delta).map(|j| ((prev + j) % self.n, delta - j)).collect()
    }
}

// ── ring v2 consumer protocol ─────────────────────────────────────────

pub const V2_BURST_BYTES: u64 = 128;

/// Reader state for ring v2 (all counts in bursts).
///
/// In overwrite mode up to `max_out` bursts beyond COMMITTED_BURSTS may
/// already be in flight into ring slots, so the safe unread window is
/// `N - 1 - max_out` bursts (not N - 1).
#[derive(Debug, Clone)]
pub struct V2Consumer {
    pub n: u64,
    pub max_out: u64,
    pub r: u64,
    w_ext: u64,
    last32: Option<u32>,
    pub lost_total: u64,
    pub discarded_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V2Plan {
    pub w: u64,
    pub lost: u64,
    pub from: u64,
    pub to: u64,
}

impl V2Consumer {
    pub fn new(n: u64, max_out: u64) -> Self {
        V2Consumer {
            n,
            max_out: max_out.min(n.saturating_sub(2)),
            r: 0,
            w_ext: 0,
            last32: None,
            lost_total: 0,
            discarded_total: 0,
        }
    }

    /// Safe unread window in bursts.
    pub fn window(&self) -> u64 {
        self.n.saturating_sub(1 + self.max_out).max(1)
    }

    /// Extends the 32-bit COMMITTED_BURSTS to a monotonic 64-bit count.
    pub fn extend(&mut self, w32: u32) -> u64 {
        match self.last32 {
            None => {
                self.w_ext = w32 as u64;
            }
            Some(l) => {
                self.w_ext += w32.wrapping_sub(l) as u64;
            }
        }
        self.last32 = Some(w32);
        self.w_ext
    }

    /// Starts consuming at the current write position.
    pub fn start_at(&mut self, w32: u32) {
        let w = self.extend(w32);
        self.r = w;
    }

    /// Lap rule: if more than the safe window is pending, skip the oldest.
    pub fn plan(&mut self, w: u64) -> V2Plan {
        let avail = w.saturating_sub(self.r);
        let win = self.window();
        let mut lost = 0;
        if avail > win {
            lost = avail - win;
            self.r = w - win;
            self.lost_total += lost;
        }
        V2Plan { w, lost, from: self.r, to: w }
    }

    /// Torn-copy check after the copy: a copied burst b is unsafe if the
    /// producer may have reached its slot, i.e. `w2 + max_out >= b + N`.
    /// Returns how many bursts at the start of [from, to) to discard.
    pub fn overwritten(&mut self, w2: u64, from: u64, to: u64) -> u64 {
        let reach = w2 + self.max_out;
        if reach < from + self.n {
            return 0;
        }
        let x = (reach + 1 - self.n - from).min(to - from);
        self.discarded_total += x;
        x
    }

    pub fn commit(&mut self, to: u64) {
        self.r = to;
    }

    /// Byte ranges (ring offsets) covering bursts [from, to).
    pub fn segments(&self, from: u64, to: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        let mut b = from;
        while b < to {
            let slot = b % self.n;
            let run = (self.n - slot).min(to - b);
            out.push((slot * V2_BURST_BYTES, run * V2_BURST_BYTES));
            b += run;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_tracker() {
        let mut t = LegacyTracker::new(16);
        assert!(t.advance(5).is_empty());
        assert!(t.advance(5).is_empty());
        assert_eq!(t.advance(7), vec![(6, 1), (7, 0)]);
        // 7 -> 2 is 11 new sub-buffers: 8..15, 0, 1, 2 (aliasing mod 16 is
        // exactly the lap blindness the checker measures).
        let v = t.advance(2);
        let idx: Vec<u32> = v.iter().map(|x| x.0).collect();
        assert_eq!(idx, vec![8, 9, 10, 11, 12, 13, 14, 15, 0, 1, 2]);
        assert_eq!(v[0].1, 10);
        assert_eq!(v[10].1, 0);
        assert_eq!(t.wakes, 2);
    }

    #[test]
    fn legacy_tracker_wrap() {
        let mut t = LegacyTracker::new(16);
        t.advance(14);
        let v = t.advance(1);
        assert_eq!(v, vec![(15, 2), (0, 1), (1, 0)]);
        assert_eq!(t.delta_hist[3], 1);
    }

    #[test]
    fn v2_protocol() {
        // N = 16, up to 4 bursts in flight -> safe window 11.
        let mut c = V2Consumer::new(16, 4);
        assert_eq!(c.window(), 11);
        c.start_at(100);
        assert_eq!(c.r, 100);
        let w = c.extend(103);
        let p = c.plan(w);
        assert_eq!(p, V2Plan { w: 103, lost: 0, from: 100, to: 103 });
        assert_eq!(c.overwritten(104, p.from, p.to), 0);
        c.commit(p.to);
        // Reader stalled: 30 bursts committed, only 11 are safe.
        let w = c.extend(133);
        let p = c.plan(w);
        assert_eq!(p.lost, 19);
        assert_eq!(p.from, 122);
        // Producer advanced 2 more while copying: reach = 135 + 4 = 139;
        // bursts b with b + 16 <= 139 (b <= 123) are unsafe -> 2 discarded.
        assert_eq!(c.overwritten(135, p.from, p.to), 2);
        c.commit(p.to);
        // 32-bit wrap of COMMITTED_BURSTS.
        let mut c = V2Consumer::new(8, 0);
        c.start_at(u32::MAX - 1);
        let w = c.extend(2);
        assert_eq!(w, u32::MAX as u64 - 1 + 4);
        assert_eq!(c.segments(6, 11), vec![(6 * 128, 2 * 128), (0, 3 * 128)]);
    }
}

//! System helpers: /proc and /sys parsing (portable code that simply finds
//! nothing off-target) plus thin Linux syscall wrappers with stubs.

use crate::err::{AResult, AgentError, Code};
use crate::util::read_trim;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

// ── UIO / IIO discovery ───────────────────────────────────────────────

/// `/sys/class/uio/uioN` whose `name` equals `name`.
pub fn find_uio(name: &str) -> Option<(usize, PathBuf)> {
    let rd = std::fs::read_dir("/sys/class/uio").ok()?;
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().to_string();
        if let Some(n) = fname.strip_prefix("uio").and_then(|x| x.parse::<usize>().ok()) {
            if read_trim(e.path().join("name")).as_deref() == Some(name) {
                return Some((n, e.path()));
            }
        }
    }
    None
}

pub fn uio_names() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/class/uio") {
        for e in rd.flatten() {
            if let Some(n) = read_trim(e.path().join("name")) {
                out.push(n);
            }
        }
    }
    out.sort();
    out
}

#[derive(Debug, Clone)]
pub struct IioDev {
    pub id: String,
    pub path: PathBuf,
    pub name: String,
}

pub fn iio_devices() -> Vec<IioDev> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/bus/iio/devices") {
        for e in rd.flatten() {
            let id = e.file_name().to_string_lossy().to_string();
            if !id.starts_with("iio:device") {
                continue;
            }
            let name = read_trim(e.path().join("name")).unwrap_or_default();
            out.push(IioDev {
                id,
                path: e.path(),
                name,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

pub fn iio_present(name: &str) -> bool {
    iio_devices().iter().any(|d| d.name == name)
}

// ── processes ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    pub comm: String,
    pub cmdline: String,
}

pub fn processes() -> Vec<Proc> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let pid = match e.file_name().to_string_lossy().parse::<u32>() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let comm = read_trim(e.path().join("comm")).unwrap_or_default();
            let cmdline = std::fs::read(e.path().join("cmdline"))
                .map(|b| {
                    String::from_utf8_lossy(&b)
                        .split('\0')
                        .filter(|s| !s.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            out.push(Proc { pid, comm, cmdline });
        }
    }
    out
}

/// Services of interest on the Fishball images.
pub const SERVICES: &[&str] = &[
    "p25-httpd",
    "maia-httpd",
    "iiod",
    "fishball_ctrl",
    "fishball_discovery",
    "dropbear",
    "sshd",
];

/// Finds processes whose comm (truncated to 15 chars by the kernel) or
/// argv[0] basename matches `name`.
pub fn find_procs(name: &str) -> Vec<Proc> {
    let comm_name: String = name.chars().take(15).collect();
    processes()
        .into_iter()
        .filter(|p| {
            if p.pid == std::process::id() {
                return false;
            }
            let argv0 = p.cmdline.split(' ').next().unwrap_or("");
            let base = argv0.rsplit('/').next().unwrap_or("");
            p.comm == comm_name || base == name
        })
        .collect()
}

pub fn services_json() -> Value {
    let mut m = serde_json::Map::new();
    for s in SERVICES {
        let ps = find_procs(s);
        m.insert(
            s.to_string(),
            json!({"running": !ps.is_empty(), "pids": ps.iter().map(|p| p.pid).collect::<Vec<_>>()}),
        );
    }
    Value::Object(m)
}

pub fn services_running() -> Vec<String> {
    SERVICES
        .iter()
        .filter(|s| !find_procs(s).is_empty())
        .map(|s| s.to_string())
        .collect()
}

// ── device tree reserved memory, /proc/iomem ─────────────────────────

#[derive(Debug, Clone)]
pub struct ResMem {
    pub node: String,
    pub base: u64,
    pub size: u64,
    pub no_map: bool,
    pub label: Option<String>,
    pub compatible: Option<String>,
}

fn be_cells(b: &[u8]) -> Vec<u32> {
    b.chunks_exact(4)
        .map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn dt_string(p: &Path) -> Option<String> {
    std::fs::read(p).ok().map(|b| {
        String::from_utf8_lossy(&b)
            .split('\0')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(",")
    })
}

pub fn reserved_memory() -> Vec<ResMem> {
    reserved_memory_at(Path::new("/proc/device-tree/reserved-memory"))
}

pub fn reserved_memory_at(root: &Path) -> Vec<ResMem> {
    let addr_cells = std::fs::read(root.join("#address-cells"))
        .ok()
        .map(|b| be_cells(&b).first().copied().unwrap_or(1))
        .unwrap_or(1) as usize;
    let size_cells = std::fs::read(root.join("#size-cells"))
        .ok()
        .map(|b| be_cells(&b).first().copied().unwrap_or(1))
        .unwrap_or(1) as usize;
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(root) {
        for e in rd.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let reg = match std::fs::read(p.join("reg")) {
                Ok(r) => be_cells(&r),
                Err(_) => continue,
            };
            let step = addr_cells + size_cells;
            if step == 0 || reg.len() < step {
                continue;
            }
            let mut base = 0u64;
            for c in &reg[..addr_cells] {
                base = (base << 32) | *c as u64;
            }
            let mut size = 0u64;
            for c in &reg[addr_cells..step] {
                size = (size << 32) | *c as u64;
            }
            out.push(ResMem {
                node: e.file_name().to_string_lossy().to_string(),
                base,
                size,
                no_map: p.join("no-map").exists(),
                label: dt_string(&p.join("label")),
                compatible: dt_string(&p.join("compatible")),
            });
        }
    }
    out.sort_by_key(|r| r.base);
    out
}

/// Finds a reserved region by node name, node-name prefix (before `@`) or label.
pub fn find_reserved(name: &str) -> Option<ResMem> {
    let n = name.to_ascii_lowercase().replace('_', "-");
    reserved_memory().into_iter().find(|r| {
        let node = r.node.to_ascii_lowercase().replace('_', "-");
        let stem = node.split('@').next().unwrap_or("").to_string();
        let label = r
            .label
            .clone()
            .unwrap_or_default()
            .to_ascii_lowercase()
            .replace('_', "-");
        node == n || stem == n || label == n || stem == format!("{n}-dma") || stem.trim_end_matches("-dma") == n
    })
}

/// Top-level `/proc/iomem` ranges with their names.
pub fn iomem() -> Vec<(u64, u64, String)> {
    parse_iomem(&std::fs::read_to_string("/proc/iomem").unwrap_or_default())
}

pub fn parse_iomem(text: &str) -> Vec<(u64, u64, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.starts_with(' ') {
            continue;
        }
        let (range, name) = match line.split_once(" : ") {
            Some(x) => x,
            None => continue,
        };
        if let Some((a, b)) = range.trim().split_once('-') {
            if let (Ok(a), Ok(b)) = (u64::from_str_radix(a, 16), u64::from_str_radix(b, 16)) {
                out.push((a, b, name.trim().to_string()));
            }
        }
    }
    out
}

/// True if [base, base+size) overlaps any "System RAM" range.
pub fn overlaps_system_ram(iomem: &[(u64, u64, String)], base: u64, size: u64) -> bool {
    let end = base + size.saturating_sub(1);
    iomem
        .iter()
        .filter(|(_, _, n)| n == "System RAM")
        .any(|(a, b, _)| base <= *b && end >= *a)
}

// ── /proc parsing ─────────────────────────────────────────────────────

pub fn meminfo() -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    if let Ok(t) = std::fs::read_to_string("/proc/meminfo") {
        for line in t.lines() {
            if let Some((k, v)) = line.split_once(':') {
                if let Some(n) = v.split_whitespace().next().and_then(|x| x.parse().ok()) {
                    m.insert(k.trim().to_string(), n);
                }
            }
        }
    }
    m
}

pub fn modules() -> Vec<Value> {
    let mut out = Vec::new();
    if let Ok(t) = std::fs::read_to_string("/proc/modules") {
        for line in t.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 {
                let name = f[0];
                let srcversion = read_trim(format!("/sys/module/{name}/srcversion"));
                let version = read_trim(format!("/sys/module/{name}/version"));
                out.push(json!({"name": name, "size": f[1].parse::<u64>().unwrap_or(0),
                                "used_by": f.get(3).unwrap_or(&"-"), "srcversion": srcversion,
                                "version": version}));
            }
        }
    }
    out
}

pub fn loadavg() -> Option<[f64; 3]> {
    let t = read_trim("/proc/loadavg")?;
    let v: Vec<f64> = t.split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
    if v.len() == 3 {
        Some([v[0], v[1], v[2]])
    } else {
        None
    }
}

pub fn uptime_s() -> Option<f64> {
    read_trim("/proc/uptime")?.split_whitespace().next()?.parse().ok()
}

pub fn boot_id() -> Option<String> {
    read_trim("/proc/sys/kernel/random/boot_id")
}

/// `/proc/interrupts` -> {"<irq>:<name>": total count across CPUs}.
pub fn parse_interrupts(text: &str) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    let mut lines = text.lines();
    let ncpu = lines
        .next()
        .map(|h| h.split_whitespace().filter(|x| x.starts_with("CPU")).count())
        .unwrap_or(1);
    for line in lines {
        let (irq, rest) = match line.split_once(':') {
            Some(x) => x,
            None => continue,
        };
        let parts: Vec<&str> = rest.split_whitespace().collect();
        let mut total = 0u64;
        let mut i = 0;
        while i < parts.len() && i < ncpu {
            match parts[i].parse::<u64>() {
                Ok(n) => total += n,
                Err(_) => break,
            }
            i += 1;
        }
        let name = parts.get(i..).map(|p| p.join(" ")).unwrap_or_default();
        let desc = name.split_whitespace().last().unwrap_or("").to_string();
        let key = if desc.is_empty() {
            irq.trim().to_string()
        } else {
            format!("{}:{}", irq.trim(), desc)
        };
        m.insert(key, total);
    }
    m
}

/// `/proc/stat` per-CPU (irq, softirq, busy, total) jiffies.
pub fn parse_stat_cpus(text: &str) -> BTreeMap<String, [u64; 4]> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        if !line.starts_with("cpu") {
            continue;
        }
        let f: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        if f.len() < 7 {
            continue;
        }
        let name = line.split_whitespace().next().unwrap().to_string();
        let total: u64 = f.iter().sum();
        let idle = f[3] + f.get(4).copied().unwrap_or(0);
        m.insert(name, [f[5], f[6], total - idle, total]);
    }
    m
}

/// `/proc/softirqs` -> {"TYPE": total}.
pub fn parse_softirqs(text: &str) -> BTreeMap<String, u64> {
    let mut m = BTreeMap::new();
    for line in text.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            let total: u64 = v.split_whitespace().filter_map(|x| x.parse::<u64>().ok()).sum();
            m.insert(k.trim().to_string(), total);
        }
    }
    m
}

pub fn cmdline() -> Option<String> {
    read_trim("/proc/cmdline")
}

// ── syscall wrappers ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct FsStat {
    pub total: u64,
    pub free: u64,
    pub avail: u64,
}

#[cfg(target_os = "linux")]
pub fn statvfs(p: &Path) -> Option<FsStat> {
    use std::ffi::CString;
    let c = CString::new(p.to_string_lossy().as_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    let f = s.f_frsize as u64;
    Some(FsStat {
        total: s.f_blocks as u64 * f,
        free: s.f_bfree as u64 * f,
        avail: s.f_bavail as u64 * f,
    })
}

#[cfg(not(target_os = "linux"))]
pub fn statvfs(_p: &Path) -> Option<FsStat> {
    None
}

/// Is `p` a mount point (per /proc/mounts)?
pub fn mount_of(p: &str) -> Option<(String, String)> {
    let t = std::fs::read_to_string("/proc/mounts").ok()?;
    for line in t.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() >= 3 && f[1] == p {
            return Some((f[0].to_string(), f[2].to_string()));
        }
    }
    None
}

#[cfg(target_os = "linux")]
pub fn set_affinity(cpu: usize) -> AResult<()> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set) != 0 {
            return Err(AgentError::new(
                Code::Error,
                format!("sched_setaffinity(cpu {cpu}): {}", std::io::Error::last_os_error()),
            ));
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_affinity(_cpu: usize) -> AResult<()> {
    Err(AgentError::new(Code::Unsupported, "CPU affinity needs Linux"))
}

#[cfg(target_os = "linux")]
pub fn mlock(ptr: *const u8, len: usize) -> bool {
    unsafe { libc::mlock(ptr as *const libc::c_void, len) == 0 }
}

#[cfg(not(target_os = "linux"))]
pub fn mlock(_ptr: *const u8, _len: usize) -> bool {
    false
}

#[cfg(target_os = "linux")]
pub fn munlock(ptr: *const u8, len: usize) {
    unsafe {
        libc::munlock(ptr as *const libc::c_void, len);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn munlock(_ptr: *const u8, _len: usize) {}

pub fn ncpus() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

#[cfg(target_os = "linux")]
pub fn fsync_dir(p: &Path) {
    if let Ok(f) = std::fs::File::open(p) {
        let _ = f.sync_all();
    }
}

#[cfg(not(target_os = "linux"))]
pub fn fsync_dir(_p: &Path) {}

#[cfg(target_os = "linux")]
pub fn fadvise_dontneed(f: &std::fs::File) {
    use std::os::unix::io::AsRawFd;
    unsafe {
        libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn fadvise_dontneed(_f: &std::fs::File) {}

#[cfg(target_os = "linux")]
pub fn sync_all() {
    unsafe { libc::sync() };
}

#[cfg(not(target_os = "linux"))]
pub fn sync_all() {}

/// Mounts debugfs at /sys/kernel/debug if it is not mounted yet.
#[cfg(target_os = "linux")]
pub fn ensure_debugfs() -> AResult<bool> {
    if mount_of("/sys/kernel/debug").is_some() {
        return Ok(false);
    }
    use std::ffi::CString;
    let src = CString::new("debugfs").unwrap();
    let dst = CString::new("/sys/kernel/debug").unwrap();
    let r = unsafe {
        libc::mount(
            src.as_ptr(),
            dst.as_ptr(),
            src.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if r != 0 {
        return Err(AgentError::new(
            Code::Precondition,
            format!("debugfs is not mounted and mounting failed: {}", std::io::Error::last_os_error()),
        ));
    }
    Ok(true)
}

#[cfg(not(target_os = "linux"))]
pub fn ensure_debugfs() -> AResult<bool> {
    Err(AgentError::new(Code::Unsupported, "debugfs needs Linux"))
}

pub fn kernel_release() -> Option<String> {
    read_trim("/proc/sys/kernel/osrelease")
}

pub fn kernel_version() -> Option<String> {
    read_trim("/proc/sys/kernel/version")
}

pub fn hostname() -> Option<String> {
    read_trim("/proc/sys/kernel/hostname")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iomem_parse_and_overlap() {
        let t = "00000000-1fffffff : System RAM\n  00008000-00afffff : Kernel code\n22000000-22ffffff : reserved\n23000000-3fffffff : System RAM\n7c460000-7c460fff : 7c460000.p25-core\n";
        let r = parse_iomem(t);
        assert_eq!(r.len(), 4);
        assert!(overlaps_system_ram(&r, 0x1FFF_F000, 0x2000));
        assert!(!overlaps_system_ram(&r, 0x2200_0000, 0x0100_0000));
        assert!(overlaps_system_ram(&r, 0x22FF_0000, 0x20000));
    }

    #[test]
    fn interrupts_parse() {
        let t = "           CPU0       CPU1       \n 16:       1000          5     GIC-0  29 Edge      twd\n 28:          7          3     GIC-0  61 Level     p25-core\nIPI0:          0          0  CPU wakeup interrupts\n";
        let m = parse_interrupts(t);
        assert_eq!(m.get("16:twd"), Some(&1005));
        assert_eq!(m.get("28:p25-core"), Some(&10));
        assert_eq!(m.get("IPI0:interrupts"), Some(&0));
    }

    #[test]
    fn stat_and_softirqs() {
        let s = "cpu  10 0 20 100 5 3 4 0 0 0\ncpu0 5 0 10 50 2 1 2 0 0 0\nintr 123\n";
        let m = parse_stat_cpus(s);
        assert_eq!(m["cpu0"], [1, 2, 18, 70]);
        let q = "                    CPU0       CPU1\n          HI:          1          2\n      NET_RX:         10         20\n";
        let sq = parse_softirqs(q);
        assert_eq!(sq["NET_RX"], 30);
    }

    #[test]
    fn reserved_memory_from_fake_dt() {
        let dir = std::env::temp_dir().join(format!("fbench_dt_{}", std::process::id()));
        let node = dir.join("p25-wideband-iq-dma@22000000");
        std::fs::create_dir_all(&node).unwrap();
        std::fs::write(dir.join("#address-cells"), 1u32.to_be_bytes()).unwrap();
        std::fs::write(dir.join("#size-cells"), 1u32.to_be_bytes()).unwrap();
        let mut reg = Vec::new();
        reg.extend_from_slice(&0x2200_0000u32.to_be_bytes());
        reg.extend_from_slice(&0x0100_0000u32.to_be_bytes());
        std::fs::write(node.join("reg"), reg).unwrap();
        std::fs::write(node.join("no-map"), b"").unwrap();
        std::fs::write(node.join("label"), b"p25_wideband_iq_dma\0").unwrap();
        let r = reserved_memory_at(&dir);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].base, 0x2200_0000);
        assert_eq!(r[0].size, 0x0100_0000);
        assert!(r[0].no_map);
        assert_eq!(r[0].label.as_deref(), Some("p25_wideband_iq_dma"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

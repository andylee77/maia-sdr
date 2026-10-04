//! `profile`: where a process's CPU goes. The kernel samples every CPU on its clock
//! (`perf_event_open`, one sample a millisecond by default); each sample of the process counts
//! for its thread and for the function it was in, found in the ELF symbol table of the
//! executable or library mapped there (a stripped executable gives only its libraries and the
//! kernel; `--symbols` names an unstripped build of the same code, checked against the running
//! one). Inlined code counts for the function it was inlined into. Other processes and idle
//! time are reported beside it.

use super::Ctx;
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use serde_json::{json, Value};
use std::collections::HashMap;

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let target = args.req("pid")?;
    let seconds = parse_f64(&args.opt_or("seconds", "30")?, "seconds")?;
    let hz = parse_f64(&args.opt_or("hz", "1000")?, "hz")?;
    let top = parse_f64(&args.opt_or("top", "40")?, "top")? as usize;
    let symbols = args.opt("symbols")?;
    args.finish()?;
    if !(seconds > 0.0 && (1.0..=10_000.0).contains(&hz)) {
        return Err(AgentError::new(Code::Usage, "--seconds must be positive and --hz 1-10000"));
    }
    imp::run(ctx, &target, seconds, hz, top, symbols.as_deref())
}

/// The bytes of an ELF32 file's executable PT_LOAD segment (its code), to tell whether two
/// builds hold the same code.
fn code(elf: &[u8]) -> Option<&[u8]> {
    let (phoff, phentsize, phnum) = (u32_at(elf, 0x1C)? as usize, u16_at(elf, 0x2A)? as usize, u16_at(elf, 0x2C)? as usize);
    (0..phnum).map(|i| phoff + i * phentsize).find_map(|p| {
        // PT_LOAD with PF_X.
        (u32_at(elf, p)? == 1 && u32_at(elf, p + 24)? & 1 == 1)
            .then(|| elf.get(u32_at(elf, p + 4)? as usize..(u32_at(elf, p + 4)? + u32_at(elf, p + 16)?) as usize))?
    })
}

fn parse_f64(s: &str, what: &str) -> AResult<f64> {
    s.parse().map_err(|_| AgentError::new(Code::Usage, format!("--{what}: not a number: {s}")))
}

/// One sample, before attribution.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Sample {
    pid: u32,
    tid: u32,
    ip: u32,
    kernel: bool,
    /// The link register in user space (0 when not read): in a leaf function, its caller.
    lr: u32,
}

/// An executable mapping of the process (`/proc/PID/maps`).
#[derive(Debug, Clone, PartialEq)]
struct Mapping {
    start: u32,
    end: u32,
    offset: u32,
    path: String,
}

fn parse_maps(text: &str) -> Vec<Mapping> {
    text.lines()
        .filter_map(|line| {
            let mut f = line.split_whitespace();
            let range = f.next()?;
            let perms = f.next()?;
            let offset = u32::from_str_radix(f.next()?, 16).ok()?;
            let (_dev, _inode) = (f.next()?, f.next()?);
            let path = f.collect::<Vec<_>>().join(" ");
            let (a, b) = range.split_once('-')?;
            let (start, end) = (u32::from_str_radix(a, 16).ok()?, u32::from_str_radix(b, 16).ok()?);
            perms.contains('x').then_some(Mapping { start, end, offset, path })
        })
        .collect()
}

/// A function symbol: [start, end) in the file's virtual addresses.
struct Sym {
    start: u32,
    end: u32,
    name: String,
}

/// The functions of an ELF32 file and how its file offsets map to its virtual addresses.
struct Symbols {
    syms: Vec<Sym>,
    /// PT_LOAD segments: (file offset, virtual address, file size).
    loads: Vec<(u32, u32, u32)>,
    /// "symtab", "dynsym" or "none".
    table: &'static str,
}

fn u16_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as u32)
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

impl Symbols {
    fn parse(elf: &[u8]) -> Result<Symbols, String> {
        if elf.get(..6) != Some(&[0x7f, b'E', b'L', b'F', 1, 1][..]) {
            return Err("not a little-endian ELF32 file".into());
        }
        let bad = || "truncated ELF headers".to_string();
        let (phoff, shoff) = (u32_at(elf, 0x1C).ok_or_else(bad)? as usize, u32_at(elf, 0x20).ok_or_else(bad)? as usize);
        let (phentsize, phnum) = (u16_at(elf, 0x2A).ok_or_else(bad)? as usize, u16_at(elf, 0x2C).ok_or_else(bad)? as usize);
        let (shentsize, shnum) = (u16_at(elf, 0x2E).ok_or_else(bad)? as usize, u16_at(elf, 0x30).ok_or_else(bad)? as usize);
        let mut loads = Vec::new();
        for i in 0..phnum {
            let p = phoff + i * phentsize;
            if u32_at(elf, p) == Some(1) {
                loads.push((u32_at(elf, p + 4).ok_or_else(bad)?, u32_at(elf, p + 8).ok_or_else(bad)?, u32_at(elf, p + 16).ok_or_else(bad)?));
            }
        }
        let section = |i: usize| {
            let s = shoff + i * shentsize;
            Some((u32_at(elf, s + 4)?, u32_at(elf, s + 16)? as usize, u32_at(elf, s + 20)? as usize, u32_at(elf, s + 24)? as usize))
        };
        let sections: Vec<_> = (0..shnum).filter_map(section).collect();
        // SHT_SYMTAB, else SHT_DYNSYM.
        let found = [(2, "symtab"), (11, "dynsym")]
            .iter()
            .find_map(|&(kind, table)| sections.iter().find(|s| s.0 == kind).map(|s| (*s, table)));
        let Some(((_, off, size, link), table)) = found else {
            return Ok(Symbols { syms: Vec::new(), loads, table: "none" });
        };
        let (_, str_off, str_size, _) = *sections.get(link).ok_or("symbol table without strings")?;
        let strings = elf.get(str_off..str_off + str_size).ok_or("truncated string table")?;
        let mut syms: Vec<Sym> = elf
            .get(off..off + size)
            .ok_or("truncated symbol table")?
            .chunks_exact(16)
            .filter(|e| e[12] & 0xf == 2 && u16_at(e, 14) != Some(0))
            .filter_map(|e| {
                let name = strings.get(u32_at(e, 0)? as usize..)?;
                let name = String::from_utf8_lossy(&name[..name.iter().position(|&c| c == 0)?]);
                // Bit 0 marks a Thumb function.
                let start = u32_at(e, 4)? & !1;
                Some(Sym { start, end: start + u32_at(e, 8)?, name: demangle(&name) })
            })
            .collect();
        syms.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
        syms.dedup_by_key(|s| s.start);
        // A symbol without a size runs to the next one.
        for i in 0..syms.len() {
            if syms[i].end <= syms[i].start {
                syms[i].end = syms.get(i + 1).map_or(syms[i].start + 1, |n| n.start);
            }
        }
        Ok(Symbols { syms, loads, table })
    }

    fn vaddr(&self, file_offset: u32) -> Option<u32> {
        self.loads
            .iter()
            .find(|&&(off, _, size)| (off..off + size).contains(&file_offset))
            .map(|&(off, vaddr, _)| vaddr + (file_offset - off))
    }

    fn lookup(&self, vaddr: u32) -> Option<&str> {
        let i = self.syms.partition_point(|s| s.start <= vaddr).checked_sub(1)?;
        let s = &self.syms[i];
        (vaddr < s.end).then_some(s.name.as_str())
    }
}

/// A Rust legacy-mangled name (`_ZN...E`) as a path, without its hash; any other name as is.
fn demangle(name: &str) -> String {
    let name = name.split(".llvm.").next().unwrap_or(name);
    let Some(mut rest) = name.strip_prefix("_ZN") else {
        return name.to_string();
    };
    let mut parts = Vec::new();
    while !rest.starts_with('E') {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let Ok(n) = rest[..digits].parse::<usize>() else {
            return name.to_string();
        };
        let Some(part) = rest.get(digits..digits + n) else {
            return name.to_string();
        };
        parts.push(part);
        rest = &rest[digits + n..];
    }
    if parts.last().is_some_and(|p| p.len() == 17 && p.starts_with('h') && p[1..].bytes().all(|c| c.is_ascii_hexdigit())) {
        parts.pop();
    }
    const ESCAPES: [(&str, &str); 15] = [
        ("$LT$", "<"),
        ("$GT$", ">"),
        ("$RF$", "&"),
        ("$BP$", "*"),
        ("$C$", ","),
        ("$SP$", "@"),
        ("$u20$", " "),
        ("$u22$", "\""),
        ("$u27$", "'"),
        ("$u2b$", "+"),
        ("$u3b$", ";"),
        ("$u5b$", "["),
        ("$u5d$", "]"),
        ("$u7b$", "{"),
        ("$u7d$", "}"),
    ];
    let unescape = |p: &str| {
        let mut s = p.strip_prefix("_$").map_or(p.to_string(), |r| format!("${r}"));
        for (from, to) in ESCAPES {
            s = s.replace(from, to);
        }
        s.replace("..", "::")
    };
    parts.iter().map(|p| unescape(p)).collect::<Vec<_>>().join("::")
}

/// Names samples by thread and function: the process's executable mappings, each file's
/// symbols loaded once.
struct Attributor<'a> {
    maps: &'a [Mapping],
    files: HashMap<String, Option<Symbols>>,
    read: &'a dyn Fn(&str) -> Option<Vec<u8>>,
}

impl<'a> Attributor<'a> {
    fn new(maps: &'a [Mapping], read: &'a dyn Fn(&str) -> Option<Vec<u8>>) -> Self {
        Attributor { maps, files: HashMap::new(), read }
    }

    fn function(&mut self, ip: u32, kernel: bool) -> String {
        if kernel {
            return "[kernel]".into();
        }
        let Some(m) = self.maps.iter().find(|m| (m.start..m.end).contains(&ip)) else {
            return "[unknown]".into();
        };
        let file = m.path.trim_end_matches(" (deleted)").to_string();
        let base = file.rsplit('/').next().unwrap_or(&file).to_string();
        let read = self.read;
        let syms = self
            .files
            .entry(file.clone())
            .or_insert_with(|| if file.starts_with('[') { None } else { read(&file).and_then(|b| Symbols::parse(&b).ok()) });
        let found = syms.as_ref().and_then(|s| s.vaddr(ip - m.start + m.offset).and_then(|v| s.lookup(v)));
        match found {
            Some(f) => format!("{f} [{base}]"),
            None => format!("[{base}]"),
        }
    }

    /// The symbol table each file's symbols came from.
    fn tables(&self) -> Value {
        self.files
            .iter()
            .map(|(f, s)| (f.clone(), json!(s.as_ref().map_or("unread", |s| s.table))))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
}

/// The report: the process by thread and by function, the other processes, idle.
fn report(
    pid: u32,
    samples: &HashMap<Sample, u32>,
    attr: &mut Attributor<'_>,
    thread_name: &dyn Fn(u32) -> String,
    comm: &dyn Fn(u32) -> String,
    sample_s: f64,
    seconds: f64,
    top: usize,
) -> Value {
    let pct = |n: u32| (n as f64 * sample_s / seconds * 1000.0).round() / 10.0;
    let mut threads: HashMap<String, (u32, HashMap<String, u32>)> = HashMap::new();
    let mut functions: HashMap<String, (u32, HashMap<String, u32>, HashMap<String, u32>)> = HashMap::new();
    let mut others: HashMap<u32, u32> = HashMap::new();
    let (mut total, mut idle, mut process) = (0u32, 0u32, 0u32);
    for (s, &n) in samples {
        total += n;
        if s.pid == 0 {
            idle += n;
            continue;
        }
        if s.pid != pid {
            *others.entry(s.pid).or_default() += n;
            continue;
        }
        process += n;
        let (thread, function) = (thread_name(s.tid), attr.function(s.ip, s.kernel));
        let t = threads.entry(thread.clone()).or_default();
        t.0 += n;
        *t.1.entry(function.clone()).or_default() += n;
        let f = functions.entry(function).or_default();
        f.0 += n;
        *f.1.entry(thread).or_default() += n;
        if !s.kernel && s.lr != 0 {
            // The return address less a few bytes: inside the call (ARM or Thumb).
            *f.2.entry(attr.function((s.lr & !1).wrapping_sub(2), false)).or_default() += n;
        }
    }
    let ranked = |m: &HashMap<String, u32>, k: usize| {
        let mut v: Vec<_> = m.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        v.into_iter().take(k).map(|(name, &n)| json!({"name": name, "pct_core": pct(n)})).collect::<Vec<_>>()
    };
    let mut threads: Vec<_> = threads.into_iter().collect();
    threads.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    let mut functions: Vec<_> = functions.into_iter().collect();
    functions.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.0.cmp(&b.0)));
    let mut others: Vec<_> = others.into_iter().collect();
    others.sort_by(|a, b| b.1.cmp(&a.1));
    json!({
        "pid": pid,
        "samples": total,
        "pct_core": pct(process),
        "idle_pct_core": pct(idle),
        "threads": threads.iter().map(|(name, (n, f))| json!({
            "name": name, "pct_core": pct(*n), "functions": ranked(f, 12),
        })).collect::<Vec<_>>(),
        "functions": functions.iter().take(top).map(|(name, (n, t, c))| json!({
            "name": name, "pct_core": pct(*n), "threads": ranked(t, 4), "callers": ranked(c, 4),
        })).collect::<Vec<_>>(),
        "others": others.iter().take(12).map(|&(p, n)| json!({
            "pid": p, "comm": comm(p), "pct_core": pct(n),
        })).collect::<Vec<_>>(),
        "symbols": attr.tables(),
    })
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use crate::safety;
    use std::sync::atomic::{fence, Ordering};
    use std::time::{Duration, Instant};

    /// `struct perf_event_attr` to `clockid` (its fourth version, 96 bytes).
    #[repr(C)]
    struct Attr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
        config2: u64,
        branch_sample_type: u64,
        sample_regs_user: u64,
        sample_stack_user: u32,
        clockid: i32,
    }

    const PERF_TYPE_SOFTWARE: u32 = 1;
    const PERF_COUNT_SW_CPU_CLOCK: u64 = 0;
    const PERF_SAMPLE_IP: u64 = 1;
    const PERF_SAMPLE_TID: u64 = 2;
    const PERF_SAMPLE_REGS_USER: u64 = 1 << 12;
    /// `PERF_REG_ARM_LR`.
    const REG_LR: u64 = 1 << 14;
    const PERF_FLAG_FD_CLOEXEC: libc::c_ulong = 8;
    const PERF_RECORD_LOST: u32 = 2;
    const PERF_RECORD_SAMPLE: u32 = 9;
    const PERF_RECORD_MISC_KERNEL: u16 = 1;
    /// `data_head` and `data_tail` in the ring's first page.
    const DATA_HEAD: usize = 1024;
    const DATA_TAIL: usize = 1032;
    const DATA_PAGES: usize = 64;

    /// One CPU's sampling event and its ring.
    struct Ring {
        fd: libc::c_int,
        base: *mut u8,
        page: usize,
    }

    impl Ring {
        fn open(cpu: i32, period_ns: u64) -> AResult<Ring> {
            let attr = Attr {
                kind: PERF_TYPE_SOFTWARE,
                size: std::mem::size_of::<Attr>() as u32,
                config: PERF_COUNT_SW_CPU_CLOCK,
                sample_period: period_ns,
                sample_type: PERF_SAMPLE_IP | PERF_SAMPLE_TID | PERF_SAMPLE_REGS_USER,
                read_format: 0,
                flags: 0,
                wakeup_events: 0,
                bp_type: 0,
                config1: 0,
                config2: 0,
                branch_sample_type: 0,
                sample_regs_user: REG_LR,
                sample_stack_user: 0,
                clockid: 0,
            };
            let fd = unsafe {
                libc::syscall(libc::SYS_perf_event_open, &attr as *const Attr, -1 as libc::pid_t, cpu, -1 as libc::c_int, PERF_FLAG_FD_CLOEXEC)
            } as libc::c_int;
            if fd < 0 {
                return Err(AgentError::new(
                    Code::Error,
                    format!("perf_event_open(cpu {cpu}): {}", std::io::Error::last_os_error()),
                ));
            }
            let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
            let base = unsafe {
                libc::mmap(std::ptr::null_mut(), (1 + DATA_PAGES) * page, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0)
            };
            if base == libc::MAP_FAILED {
                let e = std::io::Error::last_os_error();
                unsafe { libc::close(fd) };
                return Err(AgentError::new(Code::Error, format!("mmap of the cpu {cpu} sample ring: {e}")));
            }
            Ok(Ring { fd, base: base as *mut u8, page })
        }

        /// Every record since the last drain: (type, misc, body).
        fn drain(&mut self, mut each: impl FnMut(u32, u16, &[u8])) {
            let size = DATA_PAGES * self.page;
            // SAFETY: the first page is the kernel's control page, the next DATA_PAGES its ring;
            // the kernel writes up to data_head and reads data_tail.
            unsafe {
                let head = std::ptr::read_volatile(self.base.add(DATA_HEAD) as *const u64);
                fence(Ordering::Acquire);
                let mut pos = std::ptr::read_volatile(self.base.add(DATA_TAIL) as *const u64);
                let data = self.base.add(self.page);
                let byte = |at: u64| *data.add((at % size as u64) as usize);
                let mut record = Vec::new();
                while pos + 8 <= head {
                    let header: Vec<u8> = (0..8).map(|i| byte(pos + i)).collect();
                    let kind = u32::from_le_bytes(header[0..4].try_into().unwrap());
                    let misc = u16::from_le_bytes(header[4..6].try_into().unwrap());
                    let len = u16::from_le_bytes(header[6..8].try_into().unwrap()) as u64;
                    if len < 8 || pos + len > head {
                        break;
                    }
                    record.clear();
                    record.extend((8..len).map(|i| byte(pos + i)));
                    each(kind, misc, &record);
                    pos += len;
                }
                fence(Ordering::Release);
                std::ptr::write_volatile(self.base.add(DATA_TAIL) as *mut u64, head);
            }
        }
    }

    impl Drop for Ring {
        fn drop(&mut self) {
            unsafe {
                libc::munmap(self.base as *mut libc::c_void, (1 + DATA_PAGES) * self.page);
                libc::close(self.fd);
            }
        }
    }

    fn find_pid(target: &str) -> AResult<u32> {
        if let Ok(pid) = target.parse::<u32>() {
            return Ok(pid);
        }
        let matches: Vec<u32> = std::fs::read_dir("/proc")
            .map_err(|e| AgentError::new(Code::Error, format!("/proc: {e}")))?
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.parse::<u32>().ok())
            .filter(|p| crate::util::read_trim(format!("/proc/{p}/comm")).as_deref() == Some(target))
            .collect();
        match matches[..] {
            [pid] => Ok(pid),
            [] => Err(AgentError::new(Code::NotFound, format!("no process named {target}"))),
            _ => Err(AgentError::new(Code::Usage, format!("several processes named {target}: {matches:?}; give --pid N"))),
        }
    }

    pub fn run(ctx: &Ctx, target: &str, seconds: f64, hz: f64, top: usize, symbols: Option<&str>) -> AResult<Value> {
        let pid = find_pid(target)?;
        // The executable through /proc, so a replaced or deleted file still reads as it ran.
        let running = std::fs::read(format!("/proc/{pid}/exe"))
            .map_err(|e| AgentError::new(Code::Error, format!("/proc/{pid}/exe: {e}")))?;
        let exe_symbols = match symbols {
            None => running,
            Some(path) => {
                let file = std::fs::read(path).map_err(|e| AgentError::new(Code::NotFound, format!("{path}: {e}")))?;
                if code(&file).is_none() || code(&file) != code(&running) {
                    return Err(AgentError::new(
                        Code::Precondition,
                        format!("{path} does not hold the running executable's code (another build?)"),
                    ));
                }
                file
            }
        };
        let period_ns = (1e9 / hz).round() as u64;
        let cpus = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) }.max(1) as i32;
        let mut rings = (0..cpus).map(|c| Ring::open(c, period_ns)).collect::<AResult<Vec<_>>>()?;
        let mut samples: HashMap<Sample, u32> = HashMap::new();
        let mut lost = 0u64;
        let start = Instant::now();
        let mut collect = |rings: &mut [Ring]| {
            for r in rings.iter_mut() {
                r.drain(|kind, misc, body| match kind {
                    PERF_RECORD_SAMPLE if body.len() >= 24 => {
                        let ip = u64::from_le_bytes(body[0..8].try_into().unwrap()) as u32;
                        let pid = u32::from_le_bytes(body[8..12].try_into().unwrap());
                        let tid = u32::from_le_bytes(body[12..16].try_into().unwrap());
                        let kernel = misc & 7 == PERF_RECORD_MISC_KERNEL;
                        // The user registers' ABI (0: none read), then LR.
                        let abi = u64::from_le_bytes(body[16..24].try_into().unwrap());
                        let lr = match body.get(24..32) {
                            Some(r) if abi != 0 => u64::from_le_bytes(r.try_into().unwrap()) as u32,
                            _ => 0,
                        };
                        *samples.entry(Sample { pid, tid, ip, kernel, lr }).or_default() += 1;
                    }
                    PERF_RECORD_LOST if body.len() >= 16 => lost += u64::from_le_bytes(body[8..16].try_into().unwrap()),
                    _ => {}
                });
            }
        };
        while start.elapsed().as_secs_f64() < seconds && !safety::stop_requested() {
            std::thread::sleep(Duration::from_millis(100));
            collect(&mut rings);
        }
        collect(&mut rings);
        let elapsed = start.elapsed().as_secs_f64();
        drop(rings);
        if lost > 0 {
            ctx.warn(format!("{lost} samples lost (the ring overflowed)"));
        }

        let maps = parse_maps(&std::fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default());
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).map(|p| p.display().to_string()).unwrap_or_default();
        let read = |path: &str| {
            if path.trim_end_matches(" (deleted)") == exe.trim_end_matches(" (deleted)") {
                Some(exe_symbols.clone())
            } else {
                std::fs::read(path).ok()
            }
        };
        let mut attr = Attributor::new(&maps, &read);
        let thread_name = |tid: u32| {
            crate::util::read_trim(format!("/proc/{pid}/task/{tid}/comm")).unwrap_or_else(|| format!("tid {tid} (exited)"))
        };
        let comm = |p: u32| crate::util::read_trim(format!("/proc/{p}/comm")).unwrap_or_else(|| "(exited)".into());
        let mut out = report(pid, &samples, &mut attr, &thread_name, &comm, period_ns as f64 * 1e-9, elapsed, top);
        let o = out.as_object_mut().unwrap();
        o.insert("exe".into(), json!(exe));
        o.insert("seconds".into(), json!(crate::util::round3(elapsed)));
        o.insert("hz".into(), json!(1e9 / period_ns as f64));
        o.insert("cpus".into(), json!(cpus));
        o.insert("lost".into(), json!(lost));
        Ok(out)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::*;

    pub fn run(_ctx: &Ctx, _target: &str, _seconds: f64, _hz: f64, _top: usize, _symbols: Option<&str>) -> AResult<Value> {
        Err(AgentError::new(Code::Unsupported, "profile needs Linux perf events"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF32: one PT_LOAD (file offset 0x1000 at vaddr 0x11000), a symbol table with
    /// the given functions (name, value, size) and its strings.
    fn elf(funcs: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut symtab = vec![0u8; 16];
        for &(name, value, size) in funcs {
            let at = strtab.len() as u32;
            strtab.extend_from_slice(name.as_bytes());
            strtab.push(0);
            let mut e = Vec::new();
            e.extend_from_slice(&at.to_le_bytes());
            e.extend_from_slice(&value.to_le_bytes());
            e.extend_from_slice(&size.to_le_bytes());
            e.extend_from_slice(&[2, 0, 1, 0]);
            symtab.extend_from_slice(&e);
        }
        let (phoff, symoff) = (52usize, 84usize);
        let stroff = symoff + symtab.len();
        let shoff = stroff + strtab.len();
        let mut b = vec![0u8; shoff + 3 * 40];
        b[..6].copy_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1]);
        b[0x1C..0x20].copy_from_slice(&(phoff as u32).to_le_bytes());
        b[0x20..0x24].copy_from_slice(&(shoff as u32).to_le_bytes());
        b[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes());
        b[0x2C..0x2E].copy_from_slice(&1u16.to_le_bytes());
        b[0x2E..0x30].copy_from_slice(&40u16.to_le_bytes());
        b[0x30..0x32].copy_from_slice(&3u16.to_le_bytes());
        for (at, v) in [(0, 1u32), (4, 0x1000), (8, 0x11000), (16, 0x10000)] {
            b[phoff + at..phoff + at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[symoff..stroff].copy_from_slice(&symtab);
        b[stroff..shoff].copy_from_slice(&strtab);
        let section = |b: &mut Vec<u8>, i: usize, kind: u32, off: usize, size: usize, link: u32| {
            let s = shoff + i * 40;
            for (at, v) in [(4, kind), (16, off as u32), (20, size as u32), (24, link)] {
                b[s + at..s + at + 4].copy_from_slice(&v.to_le_bytes());
            }
        };
        section(&mut b, 1, 2, symoff, symtab.len(), 2);
        section(&mut b, 2, 3, stroff, strtab.len(), 0);
        b
    }

    #[test]
    fn rust_names_read_as_paths() {
        assert_eq!(
            demangle("_ZN7scanner3dsp4fsk43Fir7process17h0123456789abcdefE"),
            "scanner::dsp::fsk4::Fir::process"
        );
        assert_eq!(
            demangle("_ZN57_$LT$scanner..radio..Lane$u20$as$u20$core..fmt..Debug$GT$3fmt17h0123456789abcdefE.llvm.42"),
            "<scanner::radio::Lane as core::fmt::Debug>::fmt"
        );
        assert_eq!(demangle("atan2f"), "atan2f");
    }

    #[test]
    fn addresses_resolve_through_the_mapping_and_the_load_segment() {
        let bytes = elf(&[("a", 0x11000, 0x100), ("_ZN1x1b17h0123456789abcdefE", 0x11101, 0), ("c", 0x11200, 0x10)]);
        let s = Symbols::parse(&bytes).unwrap();
        assert_eq!(s.table, "symtab");
        // The Thumb bit is dropped; a size of zero runs to the next symbol.
        assert_eq!(s.lookup(0x110ff), Some("a"));
        assert_eq!(s.lookup(0x11150), Some("x::b"));
        assert_eq!(s.lookup(0x11210), None);
        let maps = parse_maps(
            "00400000-00410000 r-xp 00001000 b3:02 77 /usr/bin/scanner\n\
             00410000-00420000 rw-p 00011000 b3:02 77 /usr/bin/scanner\n\
             b6e00000-b6e80000 r-xp 00000000 b3:02 12 /lib/libm.so.6\n",
        );
        assert_eq!(maps.len(), 2, "only executable mappings");
        let read = |p: &str| (p == "/usr/bin/scanner").then(|| bytes.clone());
        let mut a = Attributor::new(&maps, &read);
        assert_eq!(a.function(0x00400080, false), "a [scanner]");
        assert_eq!(a.function(0x00400150, false), "x::b [scanner]");
        assert_eq!(a.function(0xb6e00010, false), "[libm.so.6]");
        assert_eq!(a.function(0xc0008000, true), "[kernel]");
        assert_eq!(a.function(0x1000, false), "[unknown]");
    }

    #[test]
    fn the_code_is_the_executable_segment() {
        // An ELF32 header, one PT_LOAD (offset 84, 4 bytes, flags R+X) and its bytes.
        let mut b = vec![0u8; 88];
        b[..6].copy_from_slice(&[0x7f, b'E', b'L', b'F', 1, 1]);
        b[0x1C..0x20].copy_from_slice(&52u32.to_le_bytes());
        b[0x2A..0x2C].copy_from_slice(&32u16.to_le_bytes());
        b[0x2C..0x2E].copy_from_slice(&1u16.to_le_bytes());
        for (at, v) in [(0, 1u32), (4, 84), (16, 4), (24, 5)] {
            b[52 + at..56 + at].copy_from_slice(&v.to_le_bytes());
        }
        b[84..].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(code(&b), Some(&[1, 2, 3, 4][..]));
        b[52 + 24] = 4;
        assert_eq!(code(&b), None, "a segment without PF_X is not code");
    }

    #[test]
    fn the_report_splits_the_process_from_the_rest() {
        let bytes = elf(&[("work", 0x11000, 0x80), ("caller", 0x11080, 0x80)]);
        let maps = parse_maps("00400000-00410000 r-xp 00001000 b3:02 77 /usr/bin/scanner\n");
        let read = |_: &str| Some(bytes.clone());
        let mut a = Attributor::new(&maps, &read);
        let mut samples = HashMap::new();
        samples.insert(Sample { pid: 7, tid: 8, ip: 0x00400010, kernel: false, lr: 0x00400084 }, 300);
        samples.insert(Sample { pid: 7, tid: 9, ip: 0xc0000000, kernel: true, lr: 0 }, 100);
        samples.insert(Sample { pid: 5, tid: 5, ip: 0, kernel: false, lr: 0 }, 50);
        samples.insert(Sample { pid: 0, tid: 0, ip: 0, kernel: true, lr: 0 }, 1550);
        let name = |t: u32| if t == 8 { "worker".to_string() } else { "io".to_string() };
        let comm = |_: u32| "other".to_string();
        // 1 ms samples over 1 s: 1000 samples are one core.
        let r = report(7, &samples, &mut a, &name, &comm, 1e-3, 1.0, 10);
        assert_eq!(r["pct_core"], json!(40.0));
        assert_eq!(r["idle_pct_core"], json!(155.0));
        assert_eq!(r["threads"][0]["name"], json!("worker"));
        assert_eq!(
            r["functions"][0],
            json!({"name": "work [scanner]", "pct_core": 30.0, "threads": [{"name": "worker", "pct_core": 30.0}], "callers": [{"name": "caller [scanner]", "pct_core": 30.0}]})
        );
        assert_eq!(r["others"][0], json!({"pid": 5, "comm": "other", "pct_core": 5.0}));
    }
}

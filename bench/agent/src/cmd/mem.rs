//! `mem test | canary | bw` (design doc section 8, layer 2).

use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::memtest::{self, PtrRegion, Region};
use crate::regio::PhysMap;
use crate::safety;
use crate::sys;
use crate::util::{self, round1, round3};
use serde_json::{json, Value};
use std::time::Instant;

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    match sub(args, 1, &["test", "canary", "bw"])? {
        "test" => test(ctx, args),
        "canary" => canary(ctx, args),
        _ => bw(ctx, args),
    }
}

/// Anonymous, page-aligned, optionally mlock'd buffer.
struct Anon {
    ptr: *mut u8,
    len: usize,
    locked: bool,
}

impl Anon {
    #[cfg(target_os = "linux")]
    fn new(len: usize, lock: bool) -> AResult<Anon> {
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(AgentError::new(
                Code::Precondition,
                format!("mmap {len} bytes: {}", std::io::Error::last_os_error()),
            ));
        }
        let locked = lock && sys::mlock(p as *const u8, len);
        Ok(Anon { ptr: p as *mut u8, len, locked })
    }

    #[cfg(not(target_os = "linux"))]
    fn new(len: usize, _lock: bool) -> AResult<Anon> {
        let v: &'static mut [u64] = Box::leak(vec![0u64; len.div_ceil(8)].into_boxed_slice());
        Ok(Anon { ptr: v.as_mut_ptr() as *mut u8, len, locked: false })
    }
}

impl Drop for Anon {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        unsafe {
            if self.locked {
                sys::munlock(self.ptr, self.len);
            }
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

/// Validates a /dev/mem test window: it must lie inside a no-map
/// reserved-memory region and never overlap System RAM.
fn check_phys_window(phys: u64, size: u64) -> AResult<Value> {
    if phys % 4096 != 0 || size == 0 || size % 4 != 0 {
        return Err(AgentError::new(Code::Usage, "--phys must be 4 KiB aligned and --size a non-zero multiple of 4"));
    }
    let iomem = sys::iomem();
    if sys::overlaps_system_ram(&iomem, phys, size) {
        return Err(AgentError::new(
            Code::Safety,
            format!("0x{phys:08X}+0x{size:X} overlaps System RAM: refusing to overwrite kernel memory"),
        ));
    }
    let region = sys::reserved_memory()
        .into_iter()
        .find(|r| r.no_map && phys >= r.base && phys + size <= r.base + r.size)
        .ok_or_else(|| {
            AgentError::new(
                Code::Safety,
                format!("0x{phys:08X}+0x{size:X} is not inside a no-map reserved-memory region"),
            )
        })?;
    Ok(json!({"node": region.node, "base": format!("0x{:08X}", region.base), "size": region.size}))
}

fn test(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let anon_mb = args.u64_opt("anon-mb")?;
    let phys = args.u64_opt("phys")?;
    let size = args.size_opt("size")?;
    let patterns = args
        .list("patterns")?
        .unwrap_or_else(|| memtest::PATTERNS.iter().map(|s| s.to_string()).collect());
    let passes = args.u64_or("passes", 1)?.max(1);
    let cpu = args.u64_opt("cpu")?;
    let no_lock = args.flag("no-mlock");
    args.finish()?;
    for p in &patterns {
        if !memtest::PATTERNS.contains(&p.as_str()) {
            return Err(AgentError::new(Code::Usage, format!("unknown pattern '{p}' ({})", memtest::PATTERNS.join(","))));
        }
    }
    if let Some(c) = cpu {
        sys::set_affinity(c as usize)?;
    }
    let mut warnings = Vec::new();
    let t0 = Instant::now();
    let (mut region, mode, region_info, _keep_anon, _keep_phys);
    match (anon_mb, phys) {
        (Some(mb), None) => {
            let len = (mb as usize) << 20;
            let avail = sys::meminfo().get("MemAvailable").copied().map(|k| k * 1024);
            if let Some(a) = avail {
                if len as u64 + (32 << 20) > a {
                    return Err(AgentError::new(Code::Precondition, format!("{mb} MiB exceeds MemAvailable ({} MiB) minus 32 MiB", a >> 20)));
                }
            }
            let a = Anon::new(len, !no_lock)?;
            if !a.locked && !no_lock {
                warnings.push("mlock failed: pages may be swapped/migrated (continuing)".to_string());
            }
            region = PtrRegion { ptr: a.ptr as *mut u32, words: len / 4, base_addr: a.ptr as u64 };
            mode = "anon";
            region_info = json!({"bytes": len, "mlocked": a.locked, "vaddr": format!("0x{:08X}", a.ptr as u64)});
            _keep_anon = Some(a);
            _keep_phys = None;
        }
        (None, Some(p)) => {
            let size = size.ok_or_else(|| AgentError::new(Code::Usage, "--phys needs --size"))?;
            let info = check_phys_window(p, size)?;
            let pm = PhysMap::open(p, size as usize, true)?;
            region = PtrRegion { ptr: pm.as_ptr() as *mut u32, words: size as usize / 4, base_addr: p };
            mode = "phys";
            region_info = json!({"phys": format!("0x{p:08X}"), "bytes": size, "reserved_region": info,
                                 "mapping": "uncached (/dev/mem O_SYNC)"});
            _keep_anon = None;
            _keep_phys = Some(pm);
        }
        _ => return Err(AgentError::new(Code::Usage, "give exactly one of --anon-mb N or --phys ADDR --size S")),
    }
    let stop = || safety::stop_requested();
    let mut results = Vec::new();
    let mut per_pattern: Vec<(String, u64)> = Vec::new();
    let mut errors = 0u64;
    let mut bytes = 0u64;
    for pass in 0..passes {
        for p in &patterns {
            safety::check_stop()?;
            let r = memtest::run_pattern(&mut region, p, pass as u32, &stop).unwrap();
            errors += r.errors;
            bytes += r.bytes_read + r.bytes_written;
            match per_pattern.iter_mut().find(|x| x.0 == *p) {
                Some(x) => x.1 += r.errors,
                None => per_pattern.push((p.clone(), r.errors)),
            }
            let mut v = r.to_json();
            v["pass_index"] = json!(pass);
            results.push(v);
        }
    }
    let _ = ctx;
    Ok(json!({
        "mode": mode,
        "region": region_info,
        "cpu": cpu,
        "passes": passes,
        "errors": errors,
        "pass": errors == 0,
        "bytes_tested": region.words() as u64 * 4,
        "bytes_moved": bytes,
        "seconds": round3(t0.elapsed().as_secs_f64()),
        "patterns": per_pattern.iter().map(|(n, e)| json!({"name": n, "errors": e})).collect::<Vec<_>>(),
        "results": results,
        "warnings": warnings,
    }))
}

fn canary(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 2, &["fill", "verify"])?;
    let region_name = args.req("region")?;
    let size_opt = args.size_opt("size")?;
    let seed_opt = args.u64_opt("seed")?;
    args.finish()?;
    let r = sys::find_reserved(&region_name).ok_or_else(|| {
        AgentError::new(
            Code::NotFound,
            format!(
                "reserved-memory region '{region_name}' not found (known: {})",
                sys::reserved_memory().iter().map(|r| r.node.clone()).collect::<Vec<_>>().join(", ")
            ),
        )
    })?;
    let size = size_opt.unwrap_or(r.size).min(r.size) & !3;
    check_phys_window(r.base, size)?;
    let state_p = format!("/tmp/fbench_canary_{}.json", r.node.replace(['@', '/'], "_"));
    let _ = ctx;
    match op {
        "fill" => {
            let seed = seed_opt.unwrap_or_else(|| util::unix_now().0 ^ 0x5EED_CA11_u64);
            let pm = PhysMap::open(r.base, size as usize, true)?;
            let t0 = Instant::now();
            for i in 0..(size as usize / 4) {
                pm.wr32(i * 4, memtest::canary_word(seed, r.base + 4 * i as u64));
            }
            let st = json!({"region": r.node, "base": r.base, "size": size, "seed": seed,
                            "filled": util::iso_now(), "boot_id": sys::boot_id()});
            util::write_file(&state_p, st.to_string().as_bytes())?;
            Ok(json!({"region": r.node, "base": format!("0x{:08X}", r.base), "bytes": size, "seed": seed,
                      "state_file": state_p, "seconds": round3(t0.elapsed().as_secs_f64())}))
        }
        _ => {
            let st: Value = std::fs::read_to_string(&state_p)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .ok_or_else(|| AgentError::new(Code::Precondition, format!("no canary state ({state_p}); run `mem canary fill` first")))?;
            let seed = seed_opt.or_else(|| st["seed"].as_u64()).unwrap_or(0);
            let size = st["size"].as_u64().unwrap_or(size).min(size);
            let pm = PhysMap::open(r.base, size as usize, false)?;
            let t0 = Instant::now();
            let mut corrupt = 0u64;
            let mut first = Vec::new();
            let mut pages: Vec<(u64, u64)> = Vec::new();
            for i in 0..(size as usize / 4) {
                let a = r.base + 4 * i as u64;
                let got = pm.rd32(i * 4);
                let want = memtest::canary_word(seed, a);
                if got != want {
                    corrupt += 1;
                    if first.len() < 16 {
                        first.push(json!({"addr": format!("0x{a:08X}"), "expected": format!("0x{want:08X}"), "actual": format!("0x{got:08X}")}));
                    }
                    let page = a & !0xFFF;
                    match pages.last_mut() {
                        Some(last) if last.1 + 0x1000 >= page => last.1 = page,
                        _ => pages.push((page, page)),
                    }
                }
            }
            Ok(json!({
                "region": r.node, "base": format!("0x{:08X}", r.base), "bytes": size,
                "seed": seed, "filled": st["filled"],
                "same_boot": st["boot_id"].as_str() == sys::boot_id().as_deref(),
                "corrupt_words": corrupt,
                "intact": corrupt == 0,
                "first_corrupt_addr": first.first().map(|f| f["addr"].clone()),
                "first_corrupt": first,
                "corrupt_page_ranges": pages.iter().take(64).map(|(a, b)| json!([format!("0x{a:08X}"), format!("0x{:08X}", b + 0xFFF)])).collect::<Vec<_>>(),
                "seconds": round3(t0.elapsed().as_secs_f64()),
            }))
        }
    }
}

fn mbs(bytes: usize, secs: f64) -> f64 {
    if secs > 0.0 {
        round1(bytes as f64 / 1e6 / secs)
    } else {
        0.0
    }
}

fn best_of<F: FnMut()>(reps: u32, mut f: F) -> f64 {
    let mut best = f64::MAX;
    for _ in 0..reps {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed().as_secs_f64());
    }
    best
}

fn bw(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    let size = args.size_opt("size")?.unwrap_or(16 << 20) as usize & !63;
    let reps = args.u64_or("reps", 3)?.max(1) as u32;
    let region = args.opt("region")?;
    let phys = args.u64_opt("phys")?;
    let write_uncached = args.flag("write");
    let cpu = args.u64_opt("cpu")?;
    args.finish()?;
    if let Some(c) = cpu {
        sys::set_affinity(c as usize)?;
    }
    let mut results = serde_json::Map::new();
    // Cached anonymous memory.
    let src = Anon::new(size, false)?;
    let dst = Anon::new(size, false)?;
    let s = unsafe { std::slice::from_raw_parts_mut(src.ptr, size) };
    let d = unsafe { std::slice::from_raw_parts_mut(dst.ptr, size) };
    for (i, b) in s.iter_mut().enumerate() {
        *b = i as u8;
    }
    d.fill(1);
    let t = best_of(reps, || d.copy_from_slice(s));
    results.insert("cached_memcpy".into(), json!(mbs(size, t)));
    let t = best_of(reps, || s.fill(0x5A));
    results.insert("cached_write".into(), json!(mbs(size, t)));
    let mut acc = 0u64;
    let t = best_of(reps, || {
        let w = unsafe { std::slice::from_raw_parts(src.ptr as *const u64, size / 8) };
        for x in w {
            acc = acc.wrapping_add(*x);
        }
        acc = std::hint::black_box(acc);
    });
    results.insert("cached_read".into(), json!(mbs(size, t)));

    // Uncached /dev/mem window (reserved region), if requested.
    let mut uncached = Value::Null;
    let target = match (&region, phys) {
        (Some(name), _) => sys::find_reserved(name).map(|r| (r.base, r.size.min(size as u64))),
        (None, Some(p)) => Some((p, size as u64)),
        _ => None,
    };
    if let Some((base, len)) = target {
        let info = check_phys_window(base, len)?;
        let len = len as usize & !63;
        let pm = PhysMap::open(base, len, write_uncached)?;
        let mut tmp = vec![0u8; len];
        let t = best_of(reps, || pm.copy_out(0, &mut tmp));
        results.insert("uncached_memcpy_read".into(), json!(mbs(len, t)));
        let t = best_of(reps, || {
            let mut a = 0u32;
            for i in (0..len).step_by(4) {
                a = a.wrapping_add(pm.rd32(i));
            }
            std::hint::black_box(a);
        });
        results.insert("uncached_read32".into(), json!(mbs(len, t)));
        if write_uncached {
            let t = best_of(reps, || {
                for i in (0..len).step_by(4) {
                    pm.wr32(i, i as u32);
                }
            });
            results.insert("uncached_write32".into(), json!(mbs(len, t)));
        }
        uncached = json!({"base": format!("0x{base:08X}"), "bytes": len, "region": info});
    }
    Ok(json!({
        "size": size,
        "reps": reps,
        "cpu": cpu,
        "results": Value::Object(results),
        "uncached_region": uncached,
        "unit": "MB/s (1e6 bytes/s, best of reps)",
    }))
}

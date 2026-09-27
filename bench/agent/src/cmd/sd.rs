//! `sd bench --mb N --bs K [--fsync] [--fsync-each] [--dir D] [--keep]`

use super::{sub, Ctx, BENCH_ROOT};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code, Context};
use crate::hist;
use crate::safety;
use crate::sys;
use crate::util::{self, round1, round3};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::time::Instant;

pub fn run(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    sub(args, 1, &["bench"])?;
    let mb = args.u64_or("mb", 64)?;
    let bs_kb = args.u64_or("bs", 1024)?.max(1);
    let fsync = args.flag("fsync");
    let fsync_each = args.flag("fsync-each");
    let keep = args.flag("keep");
    let dir = args.opt("dir")?.unwrap_or_else(|| format!("{BENCH_ROOT}/runs/sdbench_tmp"));
    args.finish()?;
    let dir = util::ensure_dir(&dir)?;
    let bs = (bs_kb * 1024) as usize;
    let total = (mb as usize) << 20;
    if total == 0 {
        return Err(AgentError::new(Code::Usage, "--mb must be > 0"));
    }
    let free = sys::statvfs(&dir).map(|s| s.avail);
    if let Some(f) = free {
        if total as u64 + (16 << 20) > f {
            return Err(AgentError::new(Code::Precondition, format!("not enough free space in {} ({} MiB free)", dir.display(), f >> 20)));
        }
    }
    let path = util::check_write_path(dir.join(format!("fbench_sdbench_{}.bin", std::process::id())))?;
    let mut block = vec![0u8; bs];
    let mut lat_us = Vec::with_capacity(total / bs + 1);
    let mut f = std::fs::File::create(&path).ctx(format!("create {}", path.display()))?;
    let t0 = Instant::now();
    let mut written = 0usize;
    let mut k = 0u32;
    while written < total {
        safety::check_stop()?;
        let n = bs.min(total - written);
        // Pattern: block index in every word (verifiable on read-back).
        for (i, c) in block[..n].chunks_exact_mut(4).enumerate() {
            c.copy_from_slice(&(k.wrapping_mul(0x9E37_79B9) ^ i as u32).to_le_bytes());
        }
        let t = Instant::now();
        f.write_all(&block[..n])?;
        if fsync_each {
            f.sync_data()?;
        }
        lat_us.push(t.elapsed().as_micros() as u64);
        written += n;
        k += 1;
    }
    let t_write = t0.elapsed().as_secs_f64();
    let tf = Instant::now();
    if fsync || fsync_each {
        f.sync_all()?;
    }
    let fsync_ms = tf.elapsed().as_secs_f64() * 1000.0;
    let t_write_total = t0.elapsed().as_secs_f64();
    sys::fadvise_dontneed(&f);
    drop(f);

    // Read back (page cache dropped for this file with fadvise).
    let mut f = std::fs::File::open(&path)?;
    sys::fadvise_dontneed(&f);
    let tr = Instant::now();
    let mut read = 0usize;
    let mut mismatches = 0u64;
    let mut k = 0u32;
    loop {
        let mut got = 0;
        while got < bs {
            let r = f.read(&mut block[got..])?;
            if r == 0 {
                break;
            }
            got += r;
        }
        if got == 0 {
            break;
        }
        for (i, c) in block[..got - got % 4].chunks_exact(4).enumerate() {
            if u32::from_le_bytes([c[0], c[1], c[2], c[3]]) != (k.wrapping_mul(0x9E37_79B9) ^ i as u32) {
                mismatches += 1;
            }
        }
        read += got;
        k += 1;
        if got < bs {
            break;
        }
    }
    let t_read = tr.elapsed().as_secs_f64();
    if !keep {
        let _ = std::fs::remove_file(&path);
    }
    let lat = hist::summary(&lat_us, "us");
    let write_mbs = round1(written as f64 / 1e6 / t_write_total.max(1e-9));
    Ok(json!({
        "path": path.display().to_string(),
        "kept": keep,
        "bytes": written,
        "block_bytes": bs,
        "fsync": fsync,
        "fsync_each": fsync_each,
        "write_s": round3(t_write),
        "write_total_s": round3(t_write_total),
        "write_mbs": write_mbs,
        "write_mbs_no_fsync": round1(written as f64 / 1e6 / t_write.max(1e-9)),
        "fsync_ms": if fsync || fsync_each { Some(round3(fsync_ms)) } else { None },
        "read_mbs": round1(read as f64 / 1e6 / t_read.max(1e-9)),
        "read_bytes": read,
        "verify_mismatches": mismatches,
        "write_lat_us": {"p50": lat["p50"], "p99": lat["p99"], "max": lat["max"], "mean": lat["mean"]},
        "write_lat_hist": lat["hist_log2"],
        "slow": write_mbs < 8.0,
        "fs_free_mb": free.map(|f| f >> 20),
    }))
}

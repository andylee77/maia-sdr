//! `hwval init|id|census|ingest|ringv2|legacy|mt|evt|guard|contention`
//! (Tier 1, design doc section 6; register names from share/hwval_regs.json).
//!
//! Every hwval operation first runs `implicit_init` (unless --no-init):
//! once per boot the core's async FIFOs need a CORE_RESET pulse after the
//! sampling/sync clocks run, and the DDR address guard must be programmed
//! from the device tree and locked.

use super::ring::{legacy_safe_stop, v2_accounting, wait_v2_idle};
use super::{open_core, sub, Ctx};
use crate::access::RegAccess;
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::iio;
use crate::regio::PhysMap;
use crate::safety::{self, Cleanup};
use crate::sys;
use crate::util::{self, hex32, round3};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const INIT_FILE: &str = "/tmp/fbench_hwval_init.json";
pub const HWVAL_ID: u32 = 0x6877_7631;
pub const SYNC_HZ: f64 = 62.5e6;
pub const MEM_HZ: f64 = 125e6;
pub const AXIL_HZ: f64 = 100e6;

type Acc<'m> = RegAccess<'m, PhysMap>;

fn open<'m>(ctx: &'m Ctx) -> AResult<Acc<'m>> {
    let (core, pm) = open_core(ctx, "hwval", true)?;
    if !core.has("SNAP_REQ") {
        return Err(AgentError::new(
            Code::NotFound,
            "the hwval register map is incomplete (built-in ID-only map): deploy share/hwval_regs.json",
        ));
    }
    Ok(RegAccess::new(core, pm))
}

fn r64(a: &mut Acc, base: &str) -> AResult<u64> {
    let lo = a.read(&format!("{base}_LO"))? as u64;
    let hi = a.read(&format!("{base}_HI"))? as u64;
    Ok((hi << 32) | lo)
}

fn all_mask(a: &Acc) -> u32 {
    a.core.snapshot_domains.values().fold(0, |m, b| m | b)
}

// ── init ──────────────────────────────────────────────────────────────

/// Guard window from the DT: spans hwval-ringv2 + hwval-memtest (min..max).
pub fn guard_from_dt() -> Option<(u64, u64, Value)> {
    let names = ["hwval-ringv2", "hwval-memtest", "hwval-legacy"];
    let found: Vec<sys::ResMem> = names.iter().filter_map(|n| sys::find_reserved(n)).collect();
    let span: Vec<&sys::ResMem> = found
        .iter()
        .filter(|r| {
            let s = r.node.to_ascii_lowercase();
            s.contains("ringv2") || s.contains("memtest")
        })
        .collect();
    if span.is_empty() {
        return None;
    }
    let lo = span.iter().map(|r| r.base).min()?;
    let hi = span.iter().map(|r| r.base + r.size).max()?;
    let regions: Vec<Value> = found
        .iter()
        .map(|r| json!({"node": r.node, "base": format!("0x{:08X}", r.base), "size": format!("0x{:X}", r.size)}))
        .collect();
    Some((lo, hi, json!(regions)))
}

fn do_init(ctx: &Ctx, census: bool) -> AResult<Value> {
    let mut a = open(ctx)?;
    let id = a.read("ID")?;
    if id != HWVAL_ID {
        return Err(AgentError::new(Code::WrongImage, format!("hwval ID 0x{id:08X} != 0x{HWVAL_ID:08X}")));
    }
    let mask = all_mask(&a);
    let pre = a.snapshot(mask)?;
    let census_v = if census { Some(run_census(&mut a, 100)?) } else { None };
    a.write("CORE_RESET", 1)?;
    std::thread::sleep(Duration::from_millis(2));
    a.write("CORE_RESET", 0)?;
    std::thread::sleep(Duration::from_millis(2));
    a.refresh();
    let mut guard = json!({"programmed": false});
    match guard_from_dt() {
        Some((lo, hi, regions)) => {
            a.write("GUARD_LO", lo as u32)?;
            a.write("GUARD_HI", hi.min(u32::MAX as u64) as u32)?;
            a.write("GUARD_LOCK", 1)?;
            guard = json!({"programmed": true, "lo": hex32(a.read("GUARD_LO")?), "hi": hex32(a.read("GUARD_HI")?),
                           "locked": a.read("GUARD_LOCK")? & 1, "regions": regions});
        }
        None => ctx.warn("hwval-ringv2 / hwval-memtest reserved-memory regions not found: address guard left at reset values and unlocked"),
    }
    let post = a.snapshot(mask)?;
    let alive = mask & post.ack;
    let st = json!({
        "boot_id": sys::boot_id(),
        "done_at": util::iso_now(),
        "alive_mask": alive,
        "all_mask": mask,
        "guard": guard,
    });
    util::write_file(INIT_FILE, st.to_string().as_bytes())?;
    if !post.dead.is_empty() {
        ctx.warn(format!(
            "hwval init: dead clock domain(s) {:?} (the sampling domain needs the AD9361 streaming); init will be repeated once they run",
            post.dead
        ));
    }
    Ok(json!({"id": hex32(id), "pre_reset_snapshot": pre.to_json(), "post_reset_snapshot": post.to_json(),
              "core_reset_pulsed": true, "guard": guard, "census": census_v, "state_file": INIT_FILE}))
}

/// Initialises the core once per boot (and again when previously dead
/// domains come alive). Returns the init report when it ran.
pub fn implicit_init(ctx: &Ctx, no_init: bool) -> AResult<Option<Value>> {
    if no_init || sys::find_uio("hwval-core").is_none() {
        return Ok(None);
    }
    let st: Option<Value> = std::fs::read_to_string(INIT_FILE).ok().and_then(|t| serde_json::from_str(&t).ok());
    if let Some(s) = &st {
        if s["boot_id"].as_str() == sys::boot_id().as_deref() {
            let alive = s["alive_mask"].as_u64().unwrap_or(0) as u32;
            let all = s["all_mask"].as_u64().unwrap_or(7) as u32;
            if alive == all {
                return Ok(None);
            }
            let mut a = open(ctx)?;
            let now = a.snapshot(all)?;
            if (now.ack & all) & !alive == 0 {
                return Ok(None);
            }
        }
    }
    let r = do_init(ctx, false)?;
    ctx.log("hwval implicit init done");
    Ok(Some(r))
}

// ── census ────────────────────────────────────────────────────────────

const CENSUS: &[(&str, &str, Option<f64>)] = &[
    ("CENSUS_SYNC", "sync", Some(62.5e6)),
    ("CENSUS_MEM", "mem", Some(125e6)),
    ("CENSUS_CLK3X", "clk3x", Some(187.5e6)),
    ("CENSUS_SAMPLING", "sampling", None),
    ("CENSUS_LCLK", "lclk", None),
    ("CENSUS_FCLK1", "fclk1", Some(200e6)),
    ("CENSUS_Y1", "y1", Some(50e6)),
    ("CENSUS_CLKOUT", "clkout", None),
];

fn run_census(a: &mut Acc, gate_ms: u64) -> AResult<Value> {
    let gate = (gate_ms as f64 * AXIL_HZ / 1000.0) as u32;
    a.write("CENSUS_GATE", gate)?;
    a.write("CENSUS_CTRL", 1)?;
    let end = Instant::now() + Duration::from_millis(gate_ms * 2 + 200);
    let mut done = false;
    while Instant::now() < end {
        if a.read("CENSUS_STATUS")? & 2 == 2 {
            done = true;
            break;
        }
        safety::sleep_ms(2)?;
    }
    if !done {
        return Err(AgentError::new(Code::Error, "census did not complete"));
    }
    let actual = a.read("CENSUS_GATE_ACTUAL")?.max(1) as f64;
    let fs = iio::Ad9361::open().ok().and_then(|p| p.sample_rate()).map(|x| x as f64);
    let mut clocks = serde_json::Map::new();
    for (reg, name, nominal) in CENSUS {
        if !a.core.has(reg) {
            continue;
        }
        let c = a.read(reg)? as f64;
        let f = c / actual * AXIL_HZ;
        let mut v = json!({"count": c as u64, "hz": f.round(), "alive": c > 0.0});
        if let Some(n) = nominal {
            v["nominal_hz"] = json!(n);
            v["ppm_vs_fclk0"] = json!(round3((f / n - 1.0) * 1e6));
        }
        if *name == "sampling" {
            if let Some(fs) = fs {
                v["ratio_to_fs"] = json!(round3(f / fs));
            }
        }
        clocks.insert(name.to_string(), v);
    }
    Ok(json!({"gate_cycles": gate, "gate_actual": actual as u64, "gate_s": round3(actual / AXIL_HZ),
              "resolution_ppm": round3(1e6 / actual), "clocks": clocks, "ad9361_fs_hz": fs,
              "note": "frequencies are relative to FCLK0 (PS 33.333 MHz crystal x IO PLL); y1 vs fclk0 gives the oscillator ppm difference"}))
}

// ── ingest ────────────────────────────────────────────────────────────

fn ingest(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let seconds = args.f64_or("seconds", 1.0)?;
    let prbs = args.opt_or("prbs", "off")?;
    let honor = args.flag("honor-valid");
    let clear = args.flag("clear");
    let bist = args.flag("bist");
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let (pe, pm) = match prbs.as_str() {
        "off" => (0, 0),
        "pn0fn" | "ad9361" => (1, 0),
        "pn9" | "pn11" | "pn9pn11" => (1, 1),
        _ => return Err(AgentError::new(Code::Usage, "--prbs off|pn0fn|pn9")),
    };
    let mut warnings = Vec::new();
    let _m = if bist { safety::require_maintenance(auto, ignore, &mut warnings)? } else { None };
    for w in warnings {
        ctx.warn(w);
    }
    let mut cleanup = Cleanup::new();
    if bist {
        iio::Ad9361::open()?.bist_prbs(2)?;
        cleanup.push("bist_prbs=0", || iio::Ad9361::open()?.bist_prbs(0).map(|_| json!(0)));
    }
    let mut a = open(ctx)?;
    let ctrl = 1 | (pe << 1) | (pm << 2) | ((honor as u32) << 3);
    let old = a.read("INGEST_CTRL")?;
    if old != ctrl {
        a.write("INGEST_CTRL", ctrl)?;
    }
    if clear || old != ctrl {
        a.write("INGEST_CMD", 1)?;
    }
    safety::sleep_ms((seconds * 1000.0) as u64)?;
    let mask = a.domain_bit("sampling").unwrap_or(4);
    let snap = a.snapshot(mask)?;
    let s12 = |v: u32| util::sext12(v & 0xFFF);
    let samples = r64(&mut a, "SAMPLES")?;
    let win = r64(&mut a, "WIN_SAMPLES")?;
    let i_sum = r64(&mut a, "I_SUM")? as i64;
    let q_sum = r64(&mut a, "Q_SUM")? as i64;
    let i_sumsq = r64(&mut a, "I_SUMSQ")?;
    let q_sumsq = r64(&mut a, "Q_SUMSQ")?;
    let sext48 = |v: i64| (v << 16) >> 16;
    let (i_sum, q_sum) = (sext48(i_sum), sext48(q_sum));
    let n = win.max(1) as f64;
    let i_or = a.read("I_OR_MASK")?;
    let i_and = a.read("I_AND_MASK")?;
    let q_or = a.read("Q_OR_MASK")?;
    let q_and = a.read("Q_AND_MASK")?;
    let checked = r64(&mut a, "PRBS_CHECKED")?;
    let errors = a.read("PRBS_ERRORS")?;
    let oos = a.read("PRBS_OOS_EVENTS")?;
    let out = json!({
        "ctrl": hex32(ctrl),
        "snapshot": snap.to_json(),
        "stale": !snap.dead.is_empty(),
        "samples": samples,
        "valid_gap_cycles": a.read("VALID_GAP_CYCLES")?,
        "valid_gap_runs": a.read("VALID_GAP_RUNS")?,
        "cdc_wrerr": a.read("CDC_WRERR")?,
        "cdc_full_cycles": a.try_read("CDC_FULL_CYCLES"),
        "window": {
            "samples": win,
            "i_min": s12(a.read("I_MIN")?), "i_max": s12(a.read("I_MAX")?),
            "q_min": s12(a.read("Q_MIN")?), "q_max": s12(a.read("Q_MAX")?),
            "i_mean": round3(i_sum as f64 / n), "q_mean": round3(q_sum as f64 / n),
            "i_rms": round3((i_sumsq as f64 / n).sqrt()), "q_rms": round3((q_sumsq as f64 / n).sqrt()),
            "clip_count": a.read("CLIP_COUNT")?,
            "i_stuck0": hex32(!i_or & 0xFFF), "i_stuck1": hex32(i_and & 0xFFF),
            "q_stuck0": hex32(!q_or & 0xFFF), "q_stuck1": hex32(q_and & 0xFFF),
        },
        "prbs": {
            "mode": prbs, "checked": checked, "errors": errors, "oos_events": oos,
            "in_sync": a.read("PRBS_STATUS")? & 1,
            "ber": if checked > 0 { Some(errors as f64 / checked as f64) } else { None },
            "dropped_samples_estimate": if oos > 0 && errors == 16 * oos { Some(oos) } else { None },
            "note": "checked counts in-sync samples only; one dropped sample in sync = 16 errors + 1 OOS event",
        },
        "counters_note": "samples/valid_gap/prbs counters run continuously; window statistics freeze at each snapshot",
    });
    let _ = cleanup.run();
    Ok(out)
}

// ── ring v2 / legacy configuration ────────────────────────────────────

fn ringv2_status(a: &mut Acc) -> AResult<Value> {
    a.refresh();
    let mut m = serde_json::Map::new();
    let regs: Vec<String> = a.core.regs.iter().filter(|r| r.name.starts_with("RINGV2_") && r.access.readable()).map(|r| r.name.clone()).collect();
    for n in regs {
        m.insert(n.clone(), json!(a.read(&n).ok()));
    }
    let v = Value::Object(m);
    let acc = v2_accounting(&v);
    Ok(json!({"regs": v, "accounting": acc, "snapshot": a.snapshot_json()}))
}

fn ringv2(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 2, &["status", "setup", "stop"])?;
    let src = args.opt("src")?;
    let rate_inc = args.u64_opt("rate-inc")?;
    let rate_mbs = args.f64_opt("rate-mbs")?;
    let size = args.u64_opt("size-bursts")?;
    let base = args.u64_opt("base")?;
    let subbuf = args.u64_opt("subbuf-bursts")?;
    let protect = args.flag("protect");
    let header = args.flag("header");
    let tag = args.u64_opt("tag")?;
    let max_out = args.u64_opt("max-outstanding")?;
    let flush_to = args.u64_opt("flush-timeout")?;
    let irq_every = args.u64_opt("irq-every")?;
    let clear = args.flag("clear");
    let enable = args.flag("enable");
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    match op {
        "status" => ringv2_status(&mut a),
        "stop" => {
            let c = a.read("RINGV2_CTRL")?;
            a.write("RINGV2_CTRL", c & !1)?;
            let idle = wait_v2_idle(&mut a)?;
            let mut s = ringv2_status(&mut a)?;
            s["drained_idle"] = json!(idle);
            Ok(s)
        }
        _ => {
            let c = a.read("RINGV2_CTRL")?;
            if c & 1 == 1 {
                a.write("RINGV2_CTRL", c & !1)?;
                if !wait_v2_idle(&mut a)? {
                    return Err(AgentError::new(Code::Error, "ring v2 did not drain to idle"));
                }
            }
            if let Some(b) = base {
                if b % 4096 != 0 {
                    return Err(AgentError::new(Code::Usage, "--base must be 4 KiB aligned"));
                }
                a.write("RINGV2_BASE", b as u32)?;
            }
            if let Some(s) = size {
                if s < 2 {
                    return Err(AgentError::new(Code::Usage, "--size-bursts must be >= 2"));
                }
                a.write("RINGV2_SIZE_BURSTS", s as u32)?;
            }
            if let Some(s) = subbuf {
                a.write("RINGV2_SUBBUF_BURSTS", s as u32)?;
            }
            if let Some(m) = max_out {
                a.write("RINGV2_MAX_OUTSTANDING", m.min(8) as u32)?;
            }
            if let Some(f) = flush_to {
                a.write("RINGV2_FLUSH_TIMEOUT", f as u32)?;
            }
            if let Some(i) = irq_every {
                a.write("RINGV2_IRQ_EVERY", i as u32)?;
            }
            let inc = match (rate_inc, rate_mbs) {
                (Some(i), _) => Some(i as u32),
                (None, Some(mbs)) => Some(((mbs * 1e6 / 8.0) / SYNC_HZ * 4_294_967_296.0).round().min(u32::MAX as f64) as u32),
                _ => None,
            };
            if let Some(i) = inc {
                a.write("RINGV2_RATE_INC", i)?;
            }
            let srcv: u32 = match src.as_deref() {
                None => (c >> 3) & 7,
                Some("off") => 0,
                Some("ramp64") => 1,
                Some("tagged") => 2,
                Some("prbs31") => 3,
                Some("live") | Some("rxiq") => 4,
                Some(o) => return Err(AgentError::new(Code::Usage, format!("--src off|ramp64|tagged|prbs31|live, got {o}"))),
            };
            if clear {
                a.write("RINGV2_CMD", 4)?;
            }
            a.refresh();
            let gb_before = a.read("RINGV2_GUARD_BLOCKED").unwrap_or(0);
            let mut ctrl = (c & 0x0300) | (srcv << 3) | ((protect as u32) << 1) | ((header as u32) << 2);
            ctrl |= (tag.unwrap_or(((c >> 12) & 0xF) as u64) as u32 & 0xF) << 12;
            if protect {
                let committed = a.read("RINGV2_COMMITTED_BURSTS")?;
                a.write("RINGV2_CONSUMER_BURSTS", committed)?;
            }
            a.write("RINGV2_CTRL", ctrl | enable as u32)?;
            std::thread::sleep(Duration::from_millis(2));
            a.refresh();
            let gb_after = a.read("RINGV2_GUARD_BLOCKED").unwrap_or(0);
            let mut s = ringv2_status(&mut a)?;
            s["ctrl"] = json!(hex32(ctrl | enable as u32));
            s["refused"] = json!(enable && gb_after != gb_before);
            if enable && gb_after != gb_before {
                s["refused_reason"] = json!("enable refused (GUARD_BLOCKED incremented): size < 2, base not 4 KiB aligned or window outside the guard; write enable 0 then 1 to retry");
            }
            Ok(s)
        }
    }
}

fn legacy(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 2, &["status", "setup", "stop"])?;
    let src = args.opt("src")?;
    let rate_inc = args.u64_opt("rate-inc")?;
    let rate_msps = args.f64_opt("rate-msps")?;
    let clear = args.flag("clear");
    let enable = args.flag("enable");
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    let status = |a: &mut Acc| -> AResult<Value> {
        a.refresh();
        let mut m = serde_json::Map::new();
        let regs: Vec<String> = a.core.regs.iter().filter(|r| r.name.starts_with("LEGACY_") && r.access.readable()).map(|r| r.name.clone()).collect();
        for n in regs {
            m.insert(n.clone(), json!(a.read(&n).ok()));
        }
        let wi = m.get("LEGACY_WORDS_IN").and_then(|v| v.as_u64());
        let wa = m.get("LEGACY_WORDS_ACCEPTED").and_then(|v| v.as_u64());
        let lat = m.get("LEGACY_LAT_MAX").and_then(|v| v.as_u64());
        Ok(json!({"regs": m, "loss_words": wi.zip(wa).map(|(i, x)| i.wrapping_sub(x)),
                  "loss_note": "true loss = WORDS_IN - WORDS_ACCEPTED (+-1 held word); PACKER_OVF overstates it",
                  "lat_max_us": lat.map(|c| round3(c as f64 / 62.5)), "lat_cliff_us": 16.6,
                  "snapshot": a.snapshot_json()}))
    };
    match op {
        "status" => status(&mut a),
        "stop" => {
            let r = legacy_safe_stop(&mut a, true)?;
            let mut s = status(&mut a)?;
            s["stop"] = r;
            Ok(s)
        }
        _ => {
            let c = a.read("LEGACY_CTRL")?;
            if c & 1 == 1 {
                legacy_safe_stop(&mut a, false)?;
            }
            let inc = match (rate_inc, rate_msps) {
                (Some(i), _) => Some(i as u32),
                (None, Some(m)) => Some((m * 1e6 / SYNC_HZ * 4_294_967_296.0).round().min(u32::MAX as f64) as u32),
                _ => None,
            };
            if let Some(i) = inc {
                a.write("LEGACY_RATE_INC", i)?;
            }
            let srcv = match src.as_deref() {
                None => (c >> 1) & 3,
                Some("off") => 0,
                Some("ramp") | Some("iqramp") => 1,
                Some("live") | Some("rxiq") => 2,
                Some(o) => return Err(AgentError::new(Code::Usage, format!("--src off|ramp|live, got {o}"))),
            };
            if clear {
                a.write("LEGACY_CMD", 1)?;
            }
            a.write("LEGACY_CTRL", (srcv << 1) | enable as u32)?;
            let mut s = status(&mut a)?;
            s["ctrl"] = json!(hex32((srcv << 1) | enable as u32));
            Ok(s)
        }
    }
}

// ── memory testers ────────────────────────────────────────────────────

pub const MT_MODES: &[&str] = &["write-only", "read-verify", "write-verify", "read-only", "byte-lane"];
pub const MT_PATTERNS: &[&str] = &["address", "walking1", "walking0", "checkerboard", "prbs", "zeros", "ones", "toggle"];

fn lookup(list: &[&str], s: &str, what: &str) -> AResult<u32> {
    if let Some(i) = list.iter().position(|x| *x == s) {
        return Ok(i as u32);
    }
    util::parse_u64(s)
        .map(|v| v as u32)
        .filter(|v| (*v as usize) < list.len())
        .ok_or_else(|| AgentError::new(Code::Usage, format!("bad {what} '{s}' ({})", list.join("|"))))
}

/// Host-side PRBS reference for decoding MTx_FIRST_ERR (pattern 4).
pub fn mt_prbs_word(seed: u32, pass: u32, addr: u32) -> u64 {
    let k = seed ^ pass.wrapping_mul(0x9E37_79B9);
    let mut x = addr ^ k;
    for _ in 0..3 {
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
    }
    ((x as u64) << 32) | (x.rotate_left(16) ^ 0xA5A5_A5A5) as u64
}

fn mt_hist(a: &mut Acc, p: &str, read: bool) -> AResult<Vec<u32>> {
    let mask = a.domain_bit("mem").unwrap_or(2);
    let mut out = Vec::with_capacity(16);
    for bin in 0..16u32 {
        a.write(&format!("{p}HIST_SEL"), ((read as u32) << 4) | bin)?;
        a.snapshot(mask)?;
        out.push(a.read(&format!("{p}HIST_VAL"))?);
    }
    Ok(out)
}

fn mt_report(a: &mut Acc, p: &str, gb_before: Option<u32>) -> AResult<Value> {
    a.refresh();
    let status = a.read(&format!("{p}STATUS"))?;
    let bwr = r64(a, &format!("{p}BYTES_WR"))?;
    let brd = r64(a, &format!("{p}BYTES_RD"))?;
    let cyc = r64(a, &format!("{p}CYCLES"))?;
    let gb = a.read(&format!("{p}GUARD_BLOCKED"))?;
    let secs = cyc as f64 / MEM_HZ;
    let wl = a.read(&format!("{p}WLAT_MAX"))?;
    let rl = a.read(&format!("{p}RLAT_MAX"))?;
    let v = json!({
        "status": {"busy": status & 1, "done": (status >> 1) & 1, "error": (status >> 2) & 1},
        "pass_count": a.read(&format!("{p}PASS_COUNT"))?,
        "bytes_wr": bwr, "bytes_rd": brd, "cycles": cyc, "seconds": round3(secs),
        "wr_mbs": if secs > 0.0 { round3(bwr as f64 / 1e6 / secs) } else { 0.0 },
        "rd_mbs": if secs > 0.0 { round3(brd as f64 / 1e6 / secs) } else { 0.0 },
        "err_count": a.read(&format!("{p}ERR_COUNT"))?,
        "first_err": {"addr": hex32(a.read(&format!("{p}FIRST_ERR_ADDR"))?),
                      "expected": format!("0x{:016X}", r64(a, &format!("{p}FIRST_ERR_EXP"))?),
                      "actual": format!("0x{:016X}", r64(a, &format!("{p}FIRST_ERR_ACT"))?)},
        "err_lanes": format!("0x{:016X}", r64(a, &format!("{p}ERR_LANES"))?),
        "bresp_err": a.read(&format!("{p}BRESP_ERR"))?,
        "rresp_err": a.read(&format!("{p}RRESP_ERR"))?,
        "wlat_max_cycles": wl, "wlat_max_ns": wl * 8,
        "rlat_max_cycles": rl, "rlat_max_ns": rl * 8,
        "guard_blocked": gb,
        "refused": gb_before.map(|b| gb != b).unwrap_or(false),
        "snapshot": a.snapshot_json(),
    });
    Ok(v)
}

fn mt(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 2, &["run", "status", "abort"])?;
    let n = args.u64_or("mt", 0)?;
    let mode = args.opt_or("mode", "write-verify")?;
    let pattern = args.opt_or("pattern", "prbs")?;
    let burst = args.u64_or("burst-len", 16)?;
    let outst = args.u64_or("outstanding", 8)?;
    let passes = args.u64_or("passes", 1)?;
    let base = args.u64_opt("base")?;
    let size = args.size_opt("size")?;
    let seed = args.u64_or("seed", 1)?;
    let idle = args.u64_or("idle", 0)?;
    let stop_on_err = args.flag("stop-on-error");
    let timeout = args.f64_or("timeout-s", 60.0)?;
    let seconds = args.f64_opt("seconds")?;
    let hist = !args.flag("no-hist");
    let no_init = args.flag("no-init");
    args.finish()?;
    if n > 1 {
        return Err(AgentError::new(Code::Usage, "--mt 0|1"));
    }
    implicit_init(ctx, no_init)?;
    let p = format!("MT{n}_");
    let mut a = open(ctx)?;
    match op {
        "status" => mt_report(&mut a, &p, None),
        "abort" => {
            a.write(&format!("{p}CMD"), 2)?;
            mt_report(&mut a, &p, None)
        }
        _ => {
            if ![1, 2, 4, 8, 16].contains(&burst) {
                return Err(AgentError::new(Code::Usage, "--burst-len 1|2|4|8|16"));
            }
            let m = lookup(MT_MODES, &mode, "mode")?;
            let pt = lookup(MT_PATTERNS, &pattern, "pattern")?;
            // Default window: the DT hwval-memtest region split in halves.
            let (dbase, dsize) = match sys::find_reserved("hwval-memtest") {
                Some(r) => (r.base + n * (r.size / 2), r.size / 2),
                None => (a.read(&format!("{p}BASE"))? as u64, a.read(&format!("{p}SIZE"))? as u64),
            };
            let base = base.unwrap_or(dbase);
            let size = size.unwrap_or(dsize);
            if a.read(&format!("{p}STATUS"))? & 1 == 1 {
                return Err(AgentError::new(Code::Precondition, format!("mt{n} is busy (hwval mt abort --mt {n})")));
            }
            let ctrl = m | (pt << 3) | ((burst as u32) << 7) | (((outst.clamp(1, 8)) as u32) << 12) | ((stop_on_err as u32) << 16);
            a.write(&format!("{p}CTRL"), ctrl)?;
            a.write(&format!("{p}BASE"), base as u32)?;
            a.write(&format!("{p}SIZE"), size as u32)?;
            a.write(&format!("{p}PASSES"), passes as u32)?;
            a.write(&format!("{p}IDLE_CYCLES"), idle as u32)?;
            a.write(&format!("{p}SEED"), seed as u32)?;
            a.refresh();
            let gb_before = a.read(&format!("{p}GUARD_BLOCKED"))?;
            // start | clear in one write (start alone keeps the counters).
            a.write(&format!("{p}CMD"), 0b101)?;
            let t0 = Instant::now();
            let limit = Duration::from_secs_f64(seconds.unwrap_or(timeout));
            let mut timed_out = false;
            loop {
                safety::check_stop()?;
                let st = a.read(&format!("{p}STATUS"))?;
                if st & 2 == 2 && st & 1 == 0 {
                    break;
                }
                if t0.elapsed() >= limit {
                    a.write(&format!("{p}CMD"), 2)?;
                    timed_out = seconds.is_none();
                    std::thread::sleep(Duration::from_millis(5));
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            let mut r = mt_report(&mut a, &p, Some(gb_before))?;
            if hist {
                r["wlat_hist"] = json!(mt_hist(&mut a, &p, false)?);
                r["rlat_hist"] = json!(mt_hist(&mut a, &p, true)?);
                r["hist_bins"] = json!("bin k = [2^k, 2^(k+1)) clk2x cycles (8 ns); bin 15 = >= 2^15");
            }
            r["config"] = json!({"mt": n, "mode": MT_MODES[m as usize], "pattern": MT_PATTERNS[pt as usize],
                                 "burst_len": burst, "outstanding": outst, "passes": passes, "base": hex32(base as u32),
                                 "size": size, "seed": seed, "idle_cycles": idle,
                                 "offered_load": round3(burst as f64 / (burst + idle) as f64), "ctrl": hex32(ctrl)});
            r["timed_out"] = json!(timed_out);
            r["wall_s"] = json!(round3(t0.elapsed().as_secs_f64()));
            if r["refused"] == json!(true) {
                r["refused_reason"] = json!("start refused: bad burst_len/mode/pattern, window outside the guard, size 0 or misaligned");
            }
            if pt == 4 && r["err_count"].as_u64().unwrap_or(0) > 0 {
                r["prbs_reference"] = json!("k = seed ^ (pass*0x9E3779B9); x = 3 x xorshift32(13,17,5) of (addr ^ k); data = {x, rotl(x,16) ^ 0xA5A5A5A5}");
            }
            r["pass"] = json!(r["err_count"].as_u64() == Some(0) && r["bresp_err"].as_u64() == Some(0)
                && r["rresp_err"].as_u64() == Some(0) && r["refused"] == json!(false) && !timed_out);
            Ok(r)
        }
    }
}

// ── CTRL_OUT event recorder ───────────────────────────────────────────

/// Decodes a 36-bit record: bit35 heartbeat, [34:27] value, [26:0] ts.
pub fn evt_decode(lo: u32, hi: u32) -> (bool, u32, u32) {
    let heartbeat = (hi >> 3) & 1 == 1;
    let value = ((hi & 0x7) << 5) | (lo >> 27);
    (heartbeat, value, lo & 0x07FF_FFFF)
}

/// Unwraps 27-bit timestamps (heartbeats every 2^26 cycles keep the gaps
/// below one wrap).
pub fn evt_unwrap(ts: &[u32]) -> Vec<u64> {
    let mut out = Vec::with_capacity(ts.len());
    let mut wraps = 0u64;
    let mut prev: Option<u32> = None;
    for &t in ts {
        if let Some(p) = prev {
            if t < p {
                wraps += 1;
            }
        }
        prev = Some(t);
        out.push((wraps << 27) | t as u64);
    }
    out
}

fn evt(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 2, &["status", "enable", "disable", "drain"])?;
    let mask = args.u64_or("mask", 0xFF)?;
    let max = args.u64_or("max", 100_000)?;
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    match op {
        "enable" => {
            a.write("EVT_CTRL", 1 | ((mask as u32 & 0xFF) << 8))?;
        }
        "disable" => {
            let c = a.read("EVT_CTRL")?;
            a.write("EVT_CTRL", c & !1)?;
        }
        "drain" => {
            let mut recs = Vec::new();
            let mut ts = Vec::new();
            while (recs.len() as u64) < max {
                let level = a.read("EVT_LEVEL")?;
                if level == 0 {
                    break;
                }
                let lo = a.read("EVT_DATA_LO")?;
                let hi = a.read("EVT_DATA_HI")?;
                a.write("EVT_POP", 1)?;
                let (hb, val, t) = evt_decode(lo, hi);
                ts.push(t);
                recs.push((hb, val));
            }
            let un = evt_unwrap(&ts);
            let t0 = un.first().copied().unwrap_or(0);
            let events: Vec<Value> = recs
                .iter()
                .zip(un.iter())
                .map(|((hb, v), t)| json!({"t_s": round3((t - t0) as f64 / SYNC_HZ), "cycles": t, "heartbeat": hb,
                                            "value": format!("0x{v:02X}")}))
                .collect();
            a.refresh();
            let transitions = recs.iter().filter(|r| !r.0).count();
            return Ok(json!({"count": events.len(), "transitions": transitions, "events": events,
                             "overflows": a.read("EVT_OVERFLOWS").ok(), "current": a.read("EVT_CURRENT").ok().map(|v| format!("0x{v:02X}")),
                             "remaining": a.read("EVT_LEVEL").ok()}));
        }
        _ => {}
    }
    a.refresh();
    Ok(json!({"ctrl": a.read("EVT_CTRL").ok().map(hex32), "level": a.read("EVT_LEVEL").ok(),
              "overflows": a.read("EVT_OVERFLOWS").ok(), "current": a.read("EVT_CURRENT").ok().map(|v| format!("0x{v:02X}"))}))
}

// ── id / guard / contention ───────────────────────────────────────────

fn id(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let no_init = args.flag("no-init");
    args.finish()?;
    let init = implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    let idv = a.read("ID")?;
    let ver = a.read("VERSION")?;
    let feat = a.read("FEATURES")?;
    let feat_names: Vec<String> = a
        .core
        .get("FEATURES")
        .map(|r| r.fields.iter().filter(|f| (feat >> f.lsb) & 1 == 1).map(|f| f.name.clone()).collect())
        .unwrap_or_default();
    let scratch_ok = {
        let old = a.read("SCRATCH")?;
        a.write("SCRATCH", 0xA5A5_5A5A)?;
        let ok = a.read("SCRATCH")? == 0xA5A5_5A5A;
        a.write("SCRATCH", old)?;
        ok
    };
    let mask = all_mask(&a);
    let snap = a.snapshot(mask)?;
    let ts = r64(&mut a, "TS")?;
    let dna = match (a.try_read("DNA_LO"), a.try_read("DNA_HI"), a.try_read("DNA_STATUS")) {
        (Some(lo), Some(hi), Some(st)) if st & 1 == 1 => Some(format!("0x{:015X}", ((hi as u64 & 0x1FF_FFFF) << 32) | lo as u64)),
        _ => None,
    };
    Ok(json!({
        "id": hex32(idv), "id_ok": idv == HWVAL_ID,
        "version": format!("{}.{}.{}", (ver >> 16) & 0xFF, (ver >> 8) & 0xFF, ver & 0xFF),
        "features": hex32(feat), "feature_names": feat_names,
        "scratch_ok": scratch_ok,
        "fpga_dna": dna,
        "snapshot": snap.to_json(),
        "all_clocks_alive": snap.dead.is_empty(),
        "ts_cycles": ts, "ts_s": round3(ts as f64 / SYNC_HZ),
        "snap_seq": a.read("SNAP_SEQ")?,
        "irq": {"pending": a.read("IRQ_PENDING")?, "enable": a.read("IRQ_ENABLE")?, "count": a.read("IRQ_COUNT")?},
        "guard": {"lo": hex32(a.read("GUARD_LO")?), "hi": hex32(a.read("GUARD_HI")?), "locked": a.read("GUARD_LOCK")? & 1},
        "core_reset": a.read("CORE_RESET")?,
        "init": init,
        "map_source": a.core.source,
    }))
}

fn guard(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let lo = args.u64_opt("lo")?;
    let hi = args.u64_opt("hi")?;
    let lock = args.flag("lock");
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    let locked = a.read("GUARD_LOCK")? & 1 == 1;
    let dt = guard_from_dt();
    if !locked {
        let (dlo, dhi) = dt.as_ref().map(|(l, h, _)| (Some(*l), Some(*h))).unwrap_or((None, None));
        if let Some(l) = lo.or(dlo) {
            a.write("GUARD_LO", l as u32)?;
        }
        if let Some(h) = hi.or(dhi) {
            a.write("GUARD_HI", h.min(u32::MAX as u64) as u32)?;
        }
        if lock {
            a.write("GUARD_LOCK", 1)?;
        }
    }
    Ok(json!({"was_locked": locked, "lo": hex32(a.read("GUARD_LO")?), "hi": hex32(a.read("GUARD_HI")?),
              "locked": a.read("GUARD_LOCK")? & 1, "dt": dt.map(|d| d.2)}))
}

fn census_cmd(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let gate_ms = args.u64_or("gate-ms", 1000)?.clamp(1, 40_000);
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    run_census(&mut a, gate_ms)
}

/// Ring v2 health under PL memory-tester aggressor load (the PS/SD/IIO
/// aggressor axes are orchestrated by the host).
fn contention(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let seconds = args.f64_or("seconds", 5.0)?;
    let idles = args.u64_list("idle")?.unwrap_or_else(|| vec![0, 16, 64, 256]);
    let mode = args.opt_or("mt-mode", "read-only")?;
    let burst = args.u64_or("burst-len", 16)?;
    let no_init = args.flag("no-init");
    args.finish()?;
    implicit_init(ctx, no_init)?;
    let mut a = open(ctx)?;
    if a.read("RINGV2_CTRL")? & 1 == 0 {
        return Err(AgentError::new(Code::Precondition, "ring v2 is not enabled (hwval ringv2 setup --src ramp64 --enable)"));
    }
    let m = lookup(MT_MODES, &mode, "mode")?;
    let mut cells = Vec::new();
    for idle in idles {
        a.refresh();
        let df0 = a.read("RINGV2_DROP_FULL")?;
        let c0 = a.read("RINGV2_COMMITTED_BURSTS")?;
        for n in 0..2 {
            let p = format!("MT{n}_");
            let (b, s) = match sys::find_reserved("hwval-memtest") {
                Some(r) => (r.base + n * (r.size / 2), r.size / 2),
                None => (a.read(&format!("{p}BASE"))? as u64, a.read(&format!("{p}SIZE"))? as u64),
            };
            a.write(&format!("{p}CTRL"), m | (4 << 3) | ((burst as u32) << 7) | (8 << 12))?;
            a.write(&format!("{p}BASE"), b as u32)?;
            a.write(&format!("{p}SIZE"), s as u32)?;
            a.write(&format!("{p}PASSES"), 0)?;
            a.write(&format!("{p}IDLE_CYCLES"), idle as u32)?;
            a.write(&format!("{p}CMD"), 0b101)?;
        }
        let r = safety::sleep_ms((seconds * 1000.0) as u64);
        for n in 0..2 {
            a.write(&format!("MT{n}_CMD"), 2)?;
        }
        r?;
        std::thread::sleep(Duration::from_millis(5));
        let mt0 = mt_report(&mut a, "MT0_", None)?;
        let mt1 = mt_report(&mut a, "MT1_", None)?;
        a.refresh();
        let df1 = a.read("RINGV2_DROP_FULL")?;
        let c1 = a.read("RINGV2_COMMITTED_BURSTS")?;
        cells.push(json!({
            "idle_cycles": idle,
            "offered_load": round3(burst as f64 / (burst + idle) as f64),
            "mt0_mbs": mt0["rd_mbs"].as_f64().unwrap_or(0.0) + mt0["wr_mbs"].as_f64().unwrap_or(0.0),
            "mt1_mbs": mt1["rd_mbs"].as_f64().unwrap_or(0.0) + mt1["wr_mbs"].as_f64().unwrap_or(0.0),
            "ringv2": {"drop_full_delta": df1.wrapping_sub(df0), "committed_bursts_delta": c1.wrapping_sub(c0),
                       "fifo_hwm": a.read("RINGV2_FIFO_HWM").ok(), "lat_max_cycles": a.read("RINGV2_LAT_MAX").ok(),
                       "max_outstanding_seen": a.read("RINGV2_MAX_OUTSTANDING_SEEN").ok()},
        }));
    }
    Ok(json!({"seconds_per_cell": seconds, "mt_mode": mode, "burst_len": burst, "cells": cells,
              "note": "ring v2 runs in overwrite mode without a PS reader here; use `ring check --ring hwval-v2` concurrently for data-level loss"}))
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(
        args,
        1,
        &["init", "id", "census", "ingest", "ringv2", "legacy", "mt", "evt", "guard", "contention"],
    )?;
    match op {
        "init" => {
            let census = args.flag("census");
            let _ = args.flag("force");
            args.finish()?;
            if sys::find_uio("hwval-core").is_none() {
                return Err(AgentError::new(Code::WrongImage, format!("hwval core not present (UIO devices: {:?})", sys::uio_names())));
            }
            do_init(ctx, census)
        }
        "id" => id(ctx, args),
        "census" => census_cmd(ctx, args),
        "ingest" => ingest(ctx, args),
        "ringv2" => ringv2(ctx, args),
        "legacy" => legacy(ctx, args),
        "mt" => mt(ctx, args),
        "evt" => evt(ctx, args),
        "guard" => guard(ctx, args),
        _ => contention(ctx, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evt_decoding_and_unwrap() {
        // value 0xA5 = 1010_0101: hi[2:0] = 0b101 (bits 7:5), lo[31:27] = 0b00101.
        let lo = (0b00101u32 << 27) | 12345;
        let hi = 0b101 | (1 << 3);
        assert_eq!(evt_decode(lo, hi), (true, 0xA5, 12345));
        let u = evt_unwrap(&[100, 1 << 26, (1 << 27) - 1, 5, 1 << 26]);
        assert_eq!(u, vec![100, 1 << 26, (1 << 27) - 1, (1 << 27) + 5, (1 << 27) + (1 << 26)]);
    }

    #[test]
    fn mt_lookup() {
        assert_eq!(lookup(MT_MODES, "read-only", "mode").unwrap(), 3);
        assert_eq!(lookup(MT_PATTERNS, "4", "pattern").unwrap(), 4);
        assert!(lookup(MT_PATTERNS, "9", "pattern").is_err());
        // Reference PRBS is deterministic and seed dependent.
        assert_eq!(mt_prbs_word(1, 0, 0x2400_0000), mt_prbs_word(1, 0, 0x2400_0000));
        assert_ne!(mt_prbs_word(1, 0, 0x2400_0000), mt_prbs_word(2, 0, 0x2400_0000));
    }
}

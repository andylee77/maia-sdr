//! `ring capture | check | synth` (design doc sections 5.3 and 7).

use super::{build_info, hw_serial, open_core, sub, Ctx};
use crate::access::RegAccess;
use crate::checker::{self, CheckCfg, ChunkMeta, PatternOpts, StreamCheck, CLASS_NAMES};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code, Context};
use crate::iio;
use crate::regio::PhysMap;
use crate::regmap::Core;
use crate::rings::{self, LegacyTracker, Mapping, RingDef, RingKind, V2Consumer, V2_BURST_BYTES};
use crate::safety::{self, Cleanup};
use crate::sigmf;
use crate::synth;
use crate::sys;
use crate::util::{self, mono_ns, round3};
use serde_json::{json, Value};
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    match sub(args, 1, &["capture", "check", "synth"])? {
        "synth" => synth_cmd(ctx, args),
        "check" if args.has("file") => check_file(ctx, args),
        "check" => stream(ctx, args, false),
        _ => stream(ctx, args, true),
    }
}

// ── options ───────────────────────────────────────────────────────────

fn check_cfg(args: &Args) -> AResult<CheckCfg> {
    Ok(CheckCfg {
        bit_err_max_bits: args.u64_or("bit-err-max", 3)? as u32,
        max_lap_k: args.u64_or("max-laps", 256)?,
        epoch_window_units: args.u64_opt("epoch-window")?,
        max_records: args.u64_or("max-records", 32)? as usize,
        skip_ringv2_meta: false,
    })
}

fn pattern_opts(args: &Args) -> AResult<PatternOpts> {
    let iq_swap = match args.opt("iq-swap")?.as_deref() {
        None | Some("auto") => None,
        Some("yes") | Some("1") | Some("true") => Some(true),
        Some("no") | Some("0") | Some("false") => Some(false),
        Some(o) => return Err(AgentError::new(Code::Usage, format!("--iq-swap auto|yes|no, got {o}"))),
    };
    Ok(PatternOpts {
        iq_swap,
        tone_tol: args.u64_or("tone-tol", 0)? as u32,
        tone_period: args.u64_or("tone-period", 32)? as usize,
        ring_bytes_override: None,
    })
}

fn make(pattern: &str, sub_b: usize, num: usize, cfg: CheckCfg, po: &PatternOpts) -> AResult<Box<dyn StreamCheck>> {
    checker::make_checker(pattern, sub_b, num, cfg, po).ok_or_else(|| {
        AgentError::new(
            Code::Usage,
            format!("unknown pattern '{pattern}' ({})", checker::PATTERNS.join("|")),
        )
    })
}

// ── synth ─────────────────────────────────────────────────────────────

fn synth_cmd(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    let pattern = args.req("pattern")?;
    let out = args.req("out")?;
    let subbuf = args.size_opt("subbuf-bytes")?.unwrap_or(64 * 1024) as usize;
    let ring_n = args.u64_or("ring-subbufs", 16)? as usize;
    let subbufs = args.u64_or("subbufs", 24)?;
    let inject = args.opt_or("inject", "default")?;
    let tone_k = args.u64_or("tone-k", 3)? as u32;
    let tone_amp = args.f64_or("tone-amp", 1500.0)?;
    args.finish()?;
    let ub = checker::pattern_unit_bytes(&pattern)
        .ok_or_else(|| AgentError::new(Code::Usage, format!("unknown pattern '{pattern}'")))?;
    if subbuf == 0 || subbuf % 32 != 0 {
        return Err(AgentError::new(Code::Usage, "--subbuf-bytes must be a multiple of 32"));
    }
    let ups = (subbuf / ub) as u64;
    let injections = match inject.as_str() {
        "default" => synth::default_injections(&pattern, subbufs, ups),
        "none" => Vec::new(),
        spec => synth::parse_injections(spec).map_err(|e| AgentError::new(Code::Usage, e))?,
    };
    let spec = synth::SynthSpec {
        pattern: pattern.clone(),
        subbuf_bytes: subbuf,
        ring_subbufs: ring_n,
        subbufs,
        injections: injections.clone(),
        tone_k,
        tone_amp,
    };
    let g = synth::generate(&spec).map_err(|e| AgentError::new(Code::Usage, e))?;
    let base = sigmf::base_path(&out);
    let data_p = util::check_write_path(format!("{base}.sigmf-data"))?;
    if let Some(d) = data_p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&data_p, &g.data)?;
    let side_p = util::check_write_path(format!("{base}.subbuf.jsonl"))?;
    let mut side = String::new();
    for r in &g.sidecar {
        side.push_str(&r.to_string());
        side.push('\n');
    }
    std::fs::write(&side_p, side)?;
    let meta = sigmf::meta(&sigmf::CaptureInfo {
        datatype: if ub == 4 { "ci16_le" } else { "ru32_le" },
        sample_rate: Some(8e6),
        frequency: None,
        datetime: util::iso_now(),
        hw: "synthetic".into(),
        description: format!("synthetic {pattern} capture with injected anomalies"),
        agent_version: env!("CARGO_PKG_VERSION").into(),
        extra: json!({"ring": "synthetic", "pattern": pattern, "subbuf_bytes": subbuf,
                      "num_subbufs": ring_n, "unit_bytes": ub,
                      "injections": injections.iter().map(|i| format!("{i:?}")).collect::<Vec<_>>()}),
    });
    let meta_p = util::write_file(format!("{base}.sigmf-meta"), serde_json::to_string_pretty(&meta)?.as_bytes())?;
    let mut expected = serde_json::Map::new();
    for n in CLASS_NAMES {
        expected.insert(n.into(), json!(injections.iter().filter(|i| i.class() == n).count()));
    }
    Ok(json!({
        "path": data_p.display().to_string(),
        "meta_path": meta_p.display().to_string(),
        "sidecar_path": side_p.display().to_string(),
        "bytes": g.data.len(),
        "pattern": pattern,
        "subbuf_bytes": subbuf,
        "num_subbufs": ring_n,
        "subbufs": subbufs,
        "injections": injections.iter().map(|i| json!({"class": i.class(), "spec": format!("{i:?}")})).collect::<Vec<_>>(),
        "expected_counts": Value::Object(expected),
    }))
}

// ── check --file ──────────────────────────────────────────────────────

fn read_sidecar(p: &Path) -> Vec<Value> {
    let f = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect()
}

fn check_file(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let file = args.req("file")?;
    let pattern_opt = args.opt("pattern")?;
    let sub_opt = args.size_opt("subbuf-bytes")?;
    let num_opt = args.u64_opt("num-subbufs")?;
    let sidecar_opt = args.opt("sidecar")?;
    let anomalies_out = args.opt("anomalies-out")?;
    let cfg = check_cfg(args)?;
    let po = pattern_opts(args)?;
    let v2meta = args.flag("ringv2-meta");
    args.finish()?;

    let base = sigmf::base_path(&file);
    let data_p = if Path::new(&file).is_file() && !file.ends_with(".sigmf-meta") {
        PathBuf::from(&file)
    } else {
        PathBuf::from(format!("{base}.sigmf-data"))
    };
    let meta: Value = std::fs::read_to_string(format!("{base}.sigmf-meta"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let g = &meta["global"];
    let pattern = pattern_opt
        .or_else(|| g["fbench:pattern"].as_str().map(|s| s.to_string()))
        .ok_or_else(|| AgentError::new(Code::Usage, "--pattern is required (not recorded in the SigMF meta)"))?;
    let sub_b = sub_opt
        .or_else(|| g["fbench:subbuf_bytes"].as_u64())
        .unwrap_or(1 << 20) as usize;
    let num = num_opt.or_else(|| g["fbench:num_subbufs"].as_u64()).unwrap_or(16) as usize;
    let side_p = sidecar_opt.map(PathBuf::from).unwrap_or_else(|| PathBuf::from(format!("{base}.subbuf.jsonl")));
    let sidecar = read_sidecar(&side_p);
    let mut cfg = cfg;
    cfg.skip_ringv2_meta = v2meta || g["fbench:ring"].as_str() == Some("hwval-v2");
    let mut po = po;
    po.ring_bytes_override = g["fbench:ring_bytes"].as_u64();
    let mut chk = make(&pattern, sub_b, num, cfg, &po)?;
    let anom_p = match anomalies_out {
        Some(p) => util::check_write_path(p)?,
        None => util::check_write_path(format!("{base}.anomalies.jsonl"))
            .or_else(|_| util::check_write_path(ctx.run_dir()?.join("anomalies.jsonl")))?,
    };
    chk.recorder().open_sink(anom_p.clone()).ctx(format!("create {}", anom_p.display()))?;

    let mut f = std::fs::File::open(&data_p).ctx(format!("open {}", data_p.display()))?;
    let mut buf = vec![0u8; sub_b];
    let mut seq = 0u64;
    let mut offset = 0u64;
    let mut check_ns = 0u64;
    loop {
        let mut filled = 0;
        while filled < sub_b {
            let n = f.read(&mut buf[filled..])?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        if filled == 0 {
            break;
        }
        let sc = sidecar.get(seq as usize);
        let meta = ChunkMeta {
            seq,
            subbuf: sc.and_then(|s| s["index"].as_u64()).map(|x| x as u32),
            byte_offset: offset,
            wake_ts_ns: sc.and_then(|s| s["wake_ts_ns"].as_u64()),
            boundary: sc.map(|s| s["boundary"].as_bool().unwrap_or(true)).unwrap_or(true),
            epoch: sc.and_then(|s| s["epoch"].as_bool()).unwrap_or(false),
            declared_gap_units: sc.and_then(|s| s["declared_gap_units"].as_u64()).unwrap_or(0),
        };
        let ub = chk.unit_bytes();
        let t = Instant::now();
        chk.process(&buf[..filled - filled % ub], &meta);
        check_ns += t.elapsed().as_nanos() as u64;
        offset += filled as u64;
        seq += 1;
        if filled < sub_b {
            break;
        }
        safety::check_stop()?;
    }
    let mut s = chk.summary();
    let rec = chk.recorder();
    s["anomalies"] = json!(rec.records);
    s["anomaly_file"] = json!(anom_p.display().to_string());
    s["file"] = json!(data_p.display().to_string());
    s["bytes_checked"] = json!(offset);
    s["subbuffers"] = json!(seq);
    s["subbuf_bytes"] = json!(sub_b);
    s["num_buffers"] = json!(num);
    s["sidecar_records"] = json!(sidecar.len());
    s["check_seconds"] = json!(round3(check_ns as f64 * 1e-9));
    s["check_mbs"] = json!(if check_ns > 0 { util::round1(offset as f64 * 1e3 / check_ns as f64) } else { 0.0 });
    s["pass"] = json!(rec.total == 0);
    Ok(s)
}

// ── live rings ────────────────────────────────────────────────────────

struct StreamOpts {
    kind: RingKind,
    mapping: String,
    pattern: Option<String>,
    seconds: f64,
    bytes: Option<u64>,
    stall_ms: Vec<u64>,
    stall_every_s: f64,
    poll_us: u64,
    enable: bool,
    reenable: bool,
    leave_enabled: bool,
    release_reset: bool,
    bist: Option<String>,
    auto_maint: bool,
    ignore_maint: bool,
    dev: Option<String>,
    phys: Option<u64>,
    out: Option<String>,
    anomalies_out: Option<String>,
    timeout_s: Option<f64>,
    no_init: bool,
}

fn parse_stream_opts(args: &Args, capture: bool) -> AResult<StreamOpts> {
    let kind = rings::parse_ring(&args.req("ring")?)?;
    Ok(StreamOpts {
        kind,
        mapping: args.opt_or("mapping", "cached")?,
        pattern: if capture { args.opt("pattern")? } else { Some(args.req("pattern")?) },
        seconds: args.f64_or("seconds", 10.0)?,
        bytes: if capture { Some(args.size_opt("bytes")?.ok_or_else(|| AgentError::new(Code::Usage, "--bytes is required"))?) } else { None },
        stall_ms: args.u64_list("stall-ms")?.unwrap_or_default(),
        stall_every_s: args.f64_or("stall-every-s", 1.5)?,
        poll_us: args.u64_or("poll-us", 500)?.max(50),
        enable: args.flag("enable"),
        reenable: args.flag("reenable"),
        leave_enabled: args.flag("leave-enabled"),
        release_reset: args.flag("release-reset"),
        bist: args.opt("bist")?,
        auto_maint: args.flag("auto-maint"),
        ignore_maint: args.flag("ignore-maint"),
        dev: args.opt("dev")?,
        phys: args.u64_opt("phys")?,
        out: if capture { Some(args.req("out")?) } else { None },
        anomalies_out: args.opt("anomalies-out")?,
        timeout_s: args.f64_opt("timeout-s")?,
        no_init: args.flag("no-init"),
    })
}

/// Receives the chunks read from a ring.
trait Consumer {
    fn chunk(&mut self, data: &[u8], meta: &ChunkMeta, side: Value) -> AResult<bool>;
    fn counts(&mut self) -> Option<[u64; 7]> {
        None
    }
    fn lost(&self) -> u128 {
        0
    }
}

struct CheckConsumer {
    chk: Box<dyn StreamCheck>,
    sidecar: Option<std::io::BufWriter<std::fs::File>>,
}

impl Consumer for CheckConsumer {
    fn chunk(&mut self, data: &[u8], meta: &ChunkMeta, side: Value) -> AResult<bool> {
        self.chk.process(data, meta);
        if let Some(s) = self.sidecar.as_mut() {
            let _ = writeln!(s, "{side}");
        }
        Ok(true)
    }
    fn counts(&mut self) -> Option<[u64; 7]> {
        Some(self.chk.recorder().counts)
    }
    fn lost(&self) -> u128 {
        self.chk.lost_units()
    }
}

struct CaptureConsumer {
    buf: Vec<u8>,
    filled: usize,
    sidecar: Vec<Value>,
}

impl Consumer for CaptureConsumer {
    fn chunk(&mut self, data: &[u8], _meta: &ChunkMeta, side: Value) -> AResult<bool> {
        let room = self.buf.len() - self.filled;
        let n = data.len().min(room);
        self.buf[self.filled..self.filled + n].copy_from_slice(&data[..n]);
        self.filled += n;
        let mut side = side;
        side["bytes"] = json!(n);
        self.sidecar.push(side);
        Ok(self.filled < self.buf.len())
    }
}

#[derive(Default)]
struct ReaderReport {
    t_sub_ms: f64,
    num_subbufs: u32,
    polls: u64,
    wakes: u64,
    chunks: u64,
    bytes: u64,
    busy_ns: u64,
    backlog_max: u64,
    overflow_flags: u64,
    lost_bursts: u64,
    discarded_bursts: u64,
    stalls: Vec<Value>,
    delta_hist: Vec<u64>,
}

struct StallPlan {
    list: Vec<u64>,
    every: Duration,
    next_at: Option<Instant>,
    idx: usize,
    pending: Option<(usize, Option<[u64; 7]>, u128)>,
}

impl StallPlan {
    fn new(list: Vec<u64>, every_s: f64, start: Instant) -> StallPlan {
        let every = Duration::from_secs_f64(every_s.max(0.05));
        StallPlan {
            next_at: if list.is_empty() { None } else { Some(start + every) },
            list,
            every,
            idx: 0,
            pending: None,
        }
    }

    /// Closes the attribution window of the previous stall.
    fn close(&mut self, rep: &mut ReaderReport, cons: &mut dyn Consumer) {
        if let Some((i, before, lost_before)) = self.pending.take() {
            if let (Some(b), Some(a)) = (before, cons.counts()) {
                let mut m = serde_json::Map::new();
                for (k, n) in CLASS_NAMES.iter().enumerate() {
                    m.insert(n.to_string(), json!(a[k] - b[k]));
                }
                rep.stalls[i]["anomalies_after"] = Value::Object(m);
                rep.stalls[i]["lost_units_after"] = json!((cons.lost() - lost_before) as u64);
            }
        }
    }

    /// Runs a stall if one is due. Returns true if it stalled.
    fn maybe_stall(&mut self, rep: &mut ReaderReport, cons: &mut dyn Consumer, t0: Instant, last_lb: Option<u32>) -> AResult<bool> {
        let due = match self.next_at {
            Some(t) => Instant::now() >= t,
            None => false,
        };
        if !due {
            return Ok(false);
        }
        self.close(rep, cons);
        let ms = self.list[self.idx];
        let ts = t0.elapsed().as_secs_f64();
        safety::sleep_ms(ms)?;
        let produced = if rep.t_sub_ms > 0.0 { ms as f64 / rep.t_sub_ms } else { 0.0 };
        let n = rep.num_subbufs.max(1) as f64;
        rep.stalls.push(json!({
            "stall_ms": ms, "t_start": round3(ts), "last_buffer_before": last_lb,
            "produced_subbufs_est": round3(produced),
            "lap_expected": produced > n - 1.0,
            "laps_est": (produced / n).floor(),
        }));
        let i = rep.stalls.len() - 1;
        self.pending = Some((i, cons.counts(), cons.lost()));
        self.idx += 1;
        self.next_at = if self.idx < self.list.len() { Some(Instant::now() + self.every) } else { None };
        Ok(true)
    }
}

/// Ring control handle (register side of the ring).
enum Ctl<'m> {
    P25(RegAccess<'m, PhysMap>),
    Legacy(RegAccess<'m, PhysMap>),
    V2 {
        acc: RegAccess<'m, PhysMap>,
        n: u64,
        base: u64,
        max_out: u64,
        protect: bool,
    },
}

impl Ctl<'_> {
    /// (last_buffer, overflow flag) for the legacy-style rings.
    fn poll_legacy(&mut self) -> AResult<(u32, bool)> {
        match self {
            Ctl::P25(a) => {
                let v = a.read_se("wideband_iq_dma_status")?;
                Ok(((v >> 1) & 0xF, v & 1 == 1))
            }
            Ctl::Legacy(a) => {
                a.refresh();
                Ok((a.read("LEGACY_LAST_BUFFER")?, false))
            }
            Ctl::V2 { .. } => Err(AgentError::new(Code::Error, "not a legacy ring")),
        }
    }

    fn committed(&mut self) -> AResult<u32> {
        match self {
            Ctl::V2 { acc, .. } => acc.read("RINGV2_COMMITTED_BURSTS"),
            _ => Err(AgentError::new(Code::Error, "not ring v2")),
        }
    }
}

fn bytes_per_second(ctx: &Ctx, ctl: &mut Ctl, fs: f64) -> f64 {
    let sync_hz = 62.5e6;
    match ctl {
        Ctl::P25(_) => 4.0 * fs,
        Ctl::Legacy(a) => {
            let ctrl = a.read("LEGACY_CTRL").unwrap_or(0);
            if (ctrl >> 1) & 3 == 1 {
                let inc = a.read("LEGACY_RATE_INC").unwrap_or(0) as f64;
                4.0 * inc / 4_294_967_296.0 * sync_hz
            } else {
                4.0 * fs
            }
        }
        Ctl::V2 { acc, .. } => {
            let ctrl = acc.read("RINGV2_CTRL").unwrap_or(0);
            if (ctrl >> 3) & 7 == 4 {
                4.0 * fs
            } else {
                let inc = acc.read("RINGV2_RATE_INC").unwrap_or(0) as f64;
                let _ = ctx;
                8.0 * inc / 4_294_967_296.0 * sync_hz
            }
        }
    }
}

fn setup_bist(bist: &str, fs: f64, cleanup: &mut Cleanup) -> AResult<Value> {
    let phy = iio::Ad9361::open()?;
    if bist == "prbs" {
        phy.bist_prbs(2)?;
        cleanup.push("bist_prbs=0", || iio::Ad9361::open()?.bist_prbs(0).map(|_| json!(0)));
        return Ok(json!({"bist_prbs": 2}));
    }
    if let Some(rest) = bist.strip_prefix("tone") {
        let freq = rest.trim_start_matches(':').parse::<f64>().unwrap_or(fs / 32.0);
        let spec = format!("2 {} 0 0", freq.round() as u64);
        phy.bist_tone(&spec)?;
        cleanup.push("bist_tone=off", || iio::Ad9361::open()?.bist_tone("0 0 0 0").map(|_| json!("0 0 0 0")));
        return Ok(json!({"bist_tone": spec}));
    }
    Err(AgentError::new(Code::Usage, "--bist prbs | tone[:freq_hz]"))
}

fn open_ctl<'m>(ctx: &'m Ctx, o: &StreamOpts, cleanup: &mut Cleanup<'m>, notes: &mut Vec<Value>) -> AResult<(Ctl<'m>, bool)> {
    match o.kind {
        RingKind::P25Wideband => {
            if !safety::daemon_pids().is_empty() {
                let name = safety::daemon_name();
                if o.enable || o.reenable || o.release_reset {
                    if !(o.ignore_maint || o.auto_maint) {
                        return Err(AgentError::new(
                            Code::Precondition,
                            format!("{name} is running and owns the wideband ring: `maint enter` first (or --auto-maint)"),
                        ));
                    }
                }
                notes.push(json!(format!("{name} is running: each poll of the read-to-clear wideband status also clears its overflow latch")));
            }
            let (core, pm) = open_core(ctx, "p25", true)?;
            let mut a = RegAccess::new(core, pm);
            if a.gate_asserted()? {
                if !o.release_reset {
                    return Err(AgentError::new(
                        Code::Safety,
                        "p25 control.sdr_reset = 1 (sync domain held in reset; the scanner has not started): pass --release-reset to clear it",
                    ));
                }
                a.write("control", 0)?;
                a.refresh();
                notes.push(json!("cleared control.sdr_reset"));
            }
            let prev = a.read("wideband_iq_dma_control")? & 1;
            let mut epoch = false;
            if prev == 0 || o.reenable {
                if !(o.enable || o.reenable) {
                    return Err(AgentError::new(
                        Code::Precondition,
                        "wideband IQ ring is disabled: pass --enable (or --reenable to measure the enable splice)",
                    ));
                }
                if prev == 1 {
                    a.write("wideband_iq_dma_control", 0)?;
                    safety::sleep_ms(2)?;
                }
                a.write("wideband_iq_dma_control", 1)?;
                epoch = true;
                notes.push(json!("enabled wideband_iq DMA (epoch)"));
                if !o.leave_enabled {
                    cleanup.push("restore wideband_iq_enable", move || {
                        let (core, pm) = open_core(ctx, "p25", true)?;
                        let mut a = RegAccess::new(core, pm);
                        a.write("wideband_iq_dma_control", prev)?;
                        Ok(json!(prev))
                    });
                }
            }
            Ok((Ctl::P25(a), epoch))
        }
        RingKind::HwvalLegacy | RingKind::HwvalV2 => {
            super::hwval::implicit_init(ctx, o.no_init)?;
            let (core, pm) = open_core(ctx, "hwval", true)?;
            let mut a = RegAccess::new(core, pm);
            if o.kind == RingKind::HwvalLegacy {
                let ctrl = a.read("LEGACY_CTRL")?;
                let mut epoch = false;
                if ctrl & 1 == 0 || o.reenable {
                    if !(o.enable || o.reenable) {
                        return Err(AgentError::new(
                            Code::Precondition,
                            "legacy ring dma_enable = 0: configure it with `hwval legacy setup` or pass --enable",
                        ));
                    }
                    if ctrl & 1 == 1 {
                        legacy_safe_stop(&mut a, false)?;
                    }
                    a.write("LEGACY_CTRL", (ctrl & !1) | 1 | if (ctrl >> 1) & 3 == 0 { 2 } else { 0 })?;
                    epoch = true;
                    if !o.leave_enabled {
                        cleanup.push("legacy safe stop", move || {
                            let (core, pm) = open_core(ctx, "hwval", true)?;
                            let mut a = RegAccess::new(core, pm);
                            legacy_safe_stop(&mut a, (ctrl >> 1) & 3 == 0)
                        });
                    }
                }
                Ok((Ctl::Legacy(a), epoch))
            } else {
                let ctrl = a.read("RINGV2_CTRL")?;
                let n = a.read("RINGV2_SIZE_BURSTS")? as u64;
                let base = a.read("RINGV2_BASE")? as u64;
                let mo = a.read("RINGV2_MAX_OUTSTANDING")? as u64;
                let max_out = if mo == 0 { 8 } else { mo.min(8) };
                let mut epoch = false;
                if ctrl & 1 == 0 || o.reenable {
                    if !(o.enable || o.reenable) {
                        return Err(AgentError::new(
                            Code::Precondition,
                            "ring v2 is disabled: configure it with `hwval ringv2 setup` or pass --enable",
                        ));
                    }
                    if ctrl & 1 == 1 {
                        a.write("RINGV2_CTRL", ctrl & !1)?;
                        wait_v2_idle(&mut a)?;
                    }
                    a.write("RINGV2_CTRL", ctrl | 1)?;
                    epoch = true;
                    if !o.leave_enabled {
                        cleanup.push("ring v2 disable", move || {
                            let (core, pm) = open_core(ctx, "hwval", true)?;
                            let mut a = RegAccess::new(core, pm);
                            let c = a.read("RINGV2_CTRL")?;
                            a.write("RINGV2_CTRL", c & !1)?;
                            let idle = wait_v2_idle(&mut a)?;
                            Ok(json!({"idle": idle}))
                        });
                    }
                }
                Ok((
                    Ctl::V2 {
                        acc: a,
                        n,
                        base,
                        max_out,
                        protect: ctrl & 2 == 2,
                    },
                    epoch,
                ))
            }
        }
    }
}

/// Safe stop order for the legacy ring: clear dma_enable, keep the source
/// running until LEGACY_AW == LEGACY_B, then (optionally) source off.
pub fn legacy_safe_stop(a: &mut RegAccess<PhysMap>, src_off: bool) -> AResult<Value> {
    let ctrl = a.read("LEGACY_CTRL")?;
    a.write("LEGACY_CTRL", ctrl & !1)?;
    let t0 = Instant::now();
    let mut balanced = false;
    while t0.elapsed() < Duration::from_millis(200) {
        a.refresh();
        let aw = a.read("LEGACY_AW")?;
        let b = a.read("LEGACY_B")?;
        if aw == b {
            balanced = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if src_off {
        a.write("LEGACY_CTRL", ctrl & !1 & !(3 << 1))?;
    }
    Ok(json!({"aw_eq_b": balanced, "src_off": src_off}))
}

pub fn wait_v2_idle(a: &mut RegAccess<PhysMap>) -> AResult<bool> {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(200) {
        if a.read("RINGV2_STATUS")? & 1 == 1 {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(false)
}

fn legacy_loop(
    ctl: &mut Ctl,
    map: &Mapping,
    def: &RingDef,
    o: &StreamOpts,
    deadline: Instant,
    epoch: bool,
    cons: &mut dyn Consumer,
    rep: &mut ReaderReport,
) -> AResult<()> {
    let n = def.num_subbufs as u32;
    let mut tr = LegacyTracker::new(n);
    let (lb0, _) = ctl.poll_legacy()?;
    tr.advance(lb0);
    let mut buf = vec![0u8; def.subbuf_bytes];
    let t0 = Instant::now();
    let mut stall = StallPlan::new(o.stall_ms.clone(), o.stall_every_s, t0);
    let mut first = epoch;
    let mut seq = 0u64;
    'outer: while Instant::now() < deadline {
        safety::check_stop()?;
        if stall.maybe_stall(rep, cons, t0, tr.prev)? {
            let (lb, ovf) = ctl.poll_legacy()?;
            let before = tr.prev;
            let ready = tr.advance(lb);
            let last = rep.stalls.len() - 1;
            rep.stalls[last]["last_buffer_after"] = json!(lb);
            rep.stalls[last]["seen_new_subbufs"] = json!(ready.len());
            rep.stalls[last]["overflow_flag"] = json!(ovf);
            let _ = before;
            if ovf {
                rep.overflow_flags += 1;
            }
            if !process_ready(&ready, map, def, cons, rep, &mut buf, &mut seq, &mut first, lb)? {
                break 'outer;
            }
            continue;
        }
        let (lb, ovf) = ctl.poll_legacy()?;
        rep.polls += 1;
        if ovf {
            rep.overflow_flags += 1;
        }
        let ready = tr.advance(lb);
        if ready.is_empty() {
            safety::sleep_us(o.poll_us);
            continue;
        }
        rep.wakes += 1;
        if !process_ready(&ready, map, def, cons, rep, &mut buf, &mut seq, &mut first, lb)? {
            break;
        }
    }
    stall.close(rep, cons);
    rep.delta_hist = tr.delta_hist.clone();
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn process_ready(
    ready: &[(u32, u32)],
    map: &Mapping,
    def: &RingDef,
    cons: &mut dyn Consumer,
    rep: &mut ReaderReport,
    buf: &mut [u8],
    seq: &mut u64,
    first: &mut bool,
    lb: u32,
) -> AResult<bool> {
    let wake = mono_ns();
    for (idx, backlog) in ready {
        let t = Instant::now();
        map.copy_subbuf(*idx as usize, def.subbuf_bytes, buf)?;
        let meta = ChunkMeta {
            seq: *seq,
            subbuf: Some(*idx),
            byte_offset: *seq * def.subbuf_bytes as u64,
            wake_ts_ns: Some(wake),
            boundary: true,
            epoch: *first,
            declared_gap_units: 0,
        };
        let side = json!({"index": idx, "seq": *seq, "wake_ts_ns": wake, "last_buffer": lb,
                          "backlog": backlog, "epoch": *first, "offset": meta.byte_offset});
        *first = false;
        rep.backlog_max = rep.backlog_max.max(*backlog as u64);
        let more = cons.chunk(buf, &meta, side)?;
        rep.busy_ns += t.elapsed().as_nanos() as u64;
        rep.chunks += 1;
        rep.bytes += def.subbuf_bytes as u64;
        *seq += 1;
        if !more {
            return Ok(false);
        }
    }
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn v2_loop(
    ctl: &mut Ctl,
    map: &Mapping,
    def: &RingDef,
    o: &StreamOpts,
    deadline: Instant,
    epoch: bool,
    cons: &mut dyn Consumer,
    rep: &mut ReaderReport,
) -> AResult<()> {
    let (n, base, max_out, protect) = match ctl {
        Ctl::V2 { n, base, max_out, protect, .. } => (*n, *base, *max_out, *protect),
        _ => unreachable!(),
    };
    if base < def.phys || base + n * V2_BURST_BYTES > def.phys + def.bytes() as u64 {
        return Err(AgentError::new(
            Code::Safety,
            format!("RINGV2_BASE 0x{base:08X} + {n} bursts is outside the mapped window 0x{:08X}+0x{:X}", def.phys, def.bytes()),
        ));
    }
    let base_off = (base - def.phys) as usize;
    let mut c = V2Consumer::new(n, max_out);
    c.start_at(ctl.committed()?);
    let t0 = Instant::now();
    let mut stall = StallPlan::new(o.stall_ms.clone(), o.stall_every_s, t0);
    let mut buf: Vec<u8> = Vec::new();
    let mut first = epoch;
    let mut seq = 0u64;
    let mut offset = 0u64;
    while Instant::now() < deadline {
        safety::check_stop()?;
        stall.maybe_stall(rep, cons, t0, None)?;
        let w = c.extend(ctl.committed()?);
        rep.polls += 1;
        if w == c.r {
            safety::sleep_us(o.poll_us);
            continue;
        }
        rep.wakes += 1;
        let wake = mono_ns();
        let t = Instant::now();
        let plan = c.plan(w);
        let total = ((plan.to - plan.from) * V2_BURST_BYTES) as usize;
        buf.resize(total, 0);
        let mut pos = 0usize;
        for (off, len) in c.segments(plan.from, plan.to) {
            map.copy_range(base_off + off as usize, &mut buf[pos..pos + len as usize])?;
            pos += len as usize;
        }
        let w2 = c.extend(ctl.committed()?);
        let disc = c.overwritten(w2, plan.from, plan.to);
        c.commit(plan.to);
        if protect {
            if let Ctl::V2 { acc, .. } = ctl {
                acc.write("RINGV2_CONSUMER_BURSTS", plan.to as u32)?;
            }
        }
        rep.lost_bursts += plan.lost;
        rep.discarded_bursts += disc;
        rep.backlog_max = rep.backlog_max.max(w - plan.from + plan.lost);
        let data = &buf[(disc * V2_BURST_BYTES) as usize..];
        let gap = plan.lost + disc;
        let meta = ChunkMeta {
            seq,
            subbuf: None,
            byte_offset: offset,
            wake_ts_ns: Some(wake),
            boundary: gap > 0,
            epoch: first,
            declared_gap_units: gap * (V2_BURST_BYTES / 8),
        };
        let side = json!({"index": seq, "seq": seq, "wake_ts_ns": wake, "committed": w, "from": plan.from,
                          "to": plan.to, "lost_bursts": plan.lost, "discarded_bursts": disc,
                          "backlog": w - plan.from, "epoch": first, "offset": offset,
                          "declared_gap_units": meta.declared_gap_units, "boundary": meta.boundary});
        first = false;
        let more = cons.chunk(data, &meta, side)?;
        offset += data.len() as u64;
        rep.bytes += data.len() as u64;
        rep.chunks += 1;
        rep.busy_ns += t.elapsed().as_nanos() as u64;
        seq += 1;
        if !more {
            break;
        }
    }
    stall.close(rep, cons);
    Ok(())
}

fn hw_counters(ctl: &mut Ctl) -> Value {
    match ctl {
        Ctl::P25(a) => {
            json!({"wideband_iq_next_address": a.read("wideband_iq_next_address").ok().map(util::hex32)})
        }
        Ctl::Legacy(a) => {
            a.refresh();
            let names = [
                "LEGACY_WORDS_IN", "LEGACY_WORDS_ACCEPTED", "LEGACY_PACKER_OVF", "LEGACY_PACKER_OVF_DISABLED",
                "LEGACY_AW", "LEGACY_B", "LEGACY_BRESP_ERR", "LEGACY_SUBBUF_DONE", "LEGACY_STALL_CYCLES",
                "LEGACY_MAX_STALL", "LEGACY_MAX_OUTSTANDING", "LEGACY_LAT_MAX", "LEGACY_LAST_BUFFER",
            ];
            let mut m = serde_json::Map::new();
            for nm in names {
                m.insert(nm.into(), json!(a.read(nm).ok()));
            }
            let wi = a.try_read("LEGACY_WORDS_IN");
            let wa = a.try_read("LEGACY_WORDS_ACCEPTED");
            let lat = a.try_read("LEGACY_LAT_MAX");
            let mo = a.try_read("LEGACY_MAX_OUTSTANDING");
            m.insert(
                "loss_words".into(),
                json!(match (wi, wa) {
                    (Some(i), Some(ac)) => Some(i.wrapping_sub(ac)),
                    _ => None,
                }),
            );
            m.insert("loss_note".into(), json!("true loss = WORDS_IN - WORDS_ACCEPTED (+-1 held word); PACKER_OVF overstates loss"));
            m.insert("lat_max_us".into(), json!(lat.map(|c| round3(c as f64 / 62.5))));
            m.insert("lat_cliff_us".into(), json!(16.6));
            m.insert("b_cap_hit".into(), json!(mo.map(|x| x >= 5)));
            m.insert("snapshot".into(), a.snapshot_json());
            Value::Object(m)
        }
        Ctl::V2 { acc, .. } => {
            acc.refresh();
            let names = [
                "RINGV2_STATUS", "RINGV2_COMMITTED_BURSTS", "RINGV2_ISSUED_BURSTS", "RINGV2_WORDS_IN_LO",
                "RINGV2_WORDS_IN_HI", "RINGV2_DROP_FULL", "RINGV2_DROP_PROTECT", "RINGV2_PAD_WORDS", "RINGV2_FLUSHES",
                "RINGV2_BRESP_ERR", "RINGV2_FIFO_HWM", "RINGV2_MAX_OUTSTANDING_SEEN", "RINGV2_LAT_MAX", "RINGV2_EPOCH",
                "RINGV2_HEADERS", "RINGV2_GEN_LO", "RINGV2_GEN_HI", "RINGV2_GUARD_BLOCKED",
            ];
            let mut m = serde_json::Map::new();
            for nm in names {
                m.insert(nm.into(), json!(acc.read(nm).ok()));
            }
            m.insert("snapshot".into(), acc.snapshot_json());
            Value::Object(m)
        }
    }
}

/// Re-reads the ring v2 counters after the cleanup (disable -> drain to
/// idle) and evaluates the word accounting on them.
fn v2_after_cleanup(ctx: &Ctx, counters: &mut Value) -> AResult<()> {
    let (core, pm) = open_core(ctx, "hwval", false)?;
    let mut ctl = Ctl::V2 {
        acc: RegAccess::new(core, pm),
        n: 0,
        base: 0,
        max_out: 0,
        protect: false,
    };
    let after = hw_counters(&mut ctl);
    counters["accounting"] = v2_accounting(&after);
    counters["after_cleanup"] = after;
    Ok(())
}

/// After a drain-to-idle: WORDS_IN == data words written + DROP_FULL + DROP_PROTECT.
pub fn v2_accounting(c: &Value) -> Value {
    let g = |k: &str| c[k].as_u64();
    match (
        g("RINGV2_WORDS_IN_LO"),
        g("RINGV2_WORDS_IN_HI"),
        g("RINGV2_COMMITTED_BURSTS"),
        g("RINGV2_PAD_WORDS"),
        g("RINGV2_HEADERS"),
        g("RINGV2_DROP_FULL"),
        g("RINGV2_DROP_PROTECT"),
        g("RINGV2_STATUS"),
    ) {
        (Some(lo), Some(hi), Some(cb), Some(pad), Some(hdr), Some(df), Some(dp), Some(st)) => {
            let words_in = (hi << 32) | lo;
            let data_written = (cb * 16).saturating_sub(pad + hdr);
            let idle = st & 1 == 1;
            json!({"idle": idle, "words_in": words_in, "data_words_written": data_written,
                   "drop_full": df, "drop_protect": dp,
                   "ok": if idle { Some(words_in == data_written + df + dp) } else { None },
                   "note": "checked only when the producer is idle (committed counter is 32-bit)"})
        }
        _ => Value::Null,
    }
}

fn stream(ctx: &Ctx, args: &Args, capture: bool) -> AResult<Value> {
    let o = parse_stream_opts(args, capture)?;
    let cfg = check_cfg(args)?;
    let po = pattern_opts(args)?;
    args.finish()?;
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    let def = rings::ring_def(o.kind, o.dev.clone(), o.phys);
    let phy = iio::Ad9361::open().ok();
    let fs = phy.as_ref().and_then(|p| p.sample_rate()).unwrap_or(8_000_000) as f64;

    // Maintenance: BIST replaces the RX stream; enabling/toggling the
    // production ring or releasing sdr_reset changes the scanner's state.
    let needs_maint = o.bist.is_some()
        || (o.kind == RingKind::P25Wideband && (o.enable || o.reenable || o.release_reset));
    let _maint = if needs_maint {
        safety::require_maintenance(o.auto_maint, o.ignore_maint, &mut warnings)?
    } else {
        None
    };
    let mut cleanup = Cleanup::new();
    let bist = match &o.bist {
        Some(b) => Some(setup_bist(b, fs, &mut cleanup)?),
        None => None,
    };
    let (mut ctl, epoch) = open_ctl(ctx, &o, &mut cleanup, &mut notes)?;
    let map = Mapping::open(&def, &o.mapping)?;
    let bps = bytes_per_second(ctx, &mut ctl, fs).max(1.0);
    let t_sub_ms = def.subbuf_bytes as f64 / bps * 1000.0;
    let mut rep = ReaderReport {
        t_sub_ms,
        num_subbufs: def.num_subbufs as u32,
        ..Default::default()
    };
    let is_v2 = matches!(ctl, Ctl::V2 { .. });

    if capture {
        let want = o.bytes.unwrap() as usize;
        let want = if is_v2 { want - want % 128 } else { want - want % def.subbuf_bytes };
        if want == 0 {
            return Err(AgentError::new(Code::Usage, "--bytes is smaller than one sub-buffer"));
        }
        let avail = sys::meminfo().get("MemAvailable").copied().unwrap_or(u64::MAX / 2048) * 1024;
        if (want as u64) + (64 << 20) > avail {
            return Err(AgentError::new(
                Code::Precondition,
                format!("--bytes {want} does not fit in RAM (MemAvailable {} MiB, 64 MiB reserve)", avail >> 20),
            ));
        }
        let base = sigmf::base_path(o.out.as_deref().unwrap());
        let data_p = util::check_write_path(format!("{base}.sigmf-data"))?;
        let mut cons = CaptureConsumer {
            buf: vec![0u8; want],
            filled: 0,
            sidecar: Vec::new(),
        };
        let locked = sys::mlock(cons.buf.as_ptr(), cons.buf.len());
        if !locked {
            warnings.push("mlock of the capture buffer failed (continuing)".into());
        }
        let timeout = o.timeout_s.unwrap_or(want as f64 / bps * 3.0 + 5.0);
        let deadline = Instant::now() + Duration::from_secs_f64(timeout);
        let t0 = Instant::now();
        if is_v2 {
            v2_loop(&mut ctl, &map, &def, &o, deadline, epoch, &mut cons, &mut rep)?;
        } else {
            legacy_loop(&mut ctl, &map, &def, &o, deadline, epoch, &mut cons, &mut rep)?;
        }
        let wall = t0.elapsed().as_secs_f64();
        let mut counters = hw_counters(&mut ctl);
        let cleanup_res = cleanup.run();
        if is_v2 {
            v2_after_cleanup(ctx, &mut counters)?;
        }
        sys::munlock(cons.buf.as_ptr(), cons.buf.len());
        // Flush to storage after the window.
        if let Some(d) = data_p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tw = Instant::now();
        {
            let mut f = std::fs::File::create(&data_p)?;
            f.write_all(&cons.buf[..cons.filled])?;
            f.sync_all()?;
        }
        let write_s = tw.elapsed().as_secs_f64();
        let side_p = util::check_write_path(format!("{base}.subbuf.jsonl"))?;
        let mut side = String::new();
        for r in &cons.sidecar {
            side.push_str(&r.to_string());
            side.push('\n');
        }
        std::fs::write(&side_p, side)?;
        let ub = if is_v2 { 8 } else { 4 };
        let meta = sigmf::meta(&sigmf::CaptureInfo {
            datatype: if is_v2 { "ru32_le" } else { "ci16_le" },
            sample_rate: if is_v2 { None } else { Some(bps / 4.0) },
            frequency: phy.as_ref().and_then(|p| p.rx_lo()).map(|f| f as f64),
            datetime: util::iso_now(),
            hw: format!("Fishball Z7020 serial {}", hw_serial().unwrap_or_else(|| "unknown".into())),
            description: format!("{} ring capture ({} mapping)", def.name, map.name()),
            agent_version: env!("CARGO_PKG_VERSION").into(),
            extra: json!({"ring": def.name, "dev": def.dev, "phys": format!("0x{:08X}", def.phys),
                          "mapping": map.name(), "subbuf_bytes": if is_v2 { 128 } else { def.subbuf_bytes },
                          "num_subbufs": match &ctl { Ctl::V2 { n, .. } => *n as usize, _ => def.num_subbufs },
                          "ring_bytes": match &ctl { Ctl::V2 { n, .. } => Some(*n * 128), _ => None },
                          "unit_bytes": ub, "pattern": o.pattern, "serial": hw_serial(),
                          "build": build_info(), "bist": bist, "epoch_at_start": epoch}),
        });
        let meta_p = util::write_file(format!("{base}.sigmf-meta"), serde_json::to_string_pretty(&meta)?.as_bytes())?;
        return Ok(json!({
            "path": data_p.display().to_string(),
            "meta_path": meta_p.display().to_string(),
            "sidecar_path": side_p.display().to_string(),
            "bytes": cons.filled,
            "requested_bytes": want,
            "complete": cons.filled == want,
            "sample_rate_hz": if is_v2 { None } else { Some(bps / 4.0) },
            "format": if is_v2 { "ru32_le (64-bit words)" } else { "ci16_le" },
            "ring": def.name, "mapping": map.name(), "mlocked": locked,
            "capture_s": round3(wall), "write_s": round3(write_s),
            "subbuf_period_ms": round3(t_sub_ms),
            "reader": reader_json(&rep, wall),
            "hw": counters,
            "notes": notes, "cleanup": cleanup_res,
            "warnings": warnings,
        }));
    }

    // check mode
    let pattern = o.pattern.clone().unwrap();
    let mut cfg = cfg;
    cfg.skip_ringv2_meta = is_v2;
    let mut po = po;
    let (sub_b, num) = match &ctl {
        Ctl::V2 { n, .. } => {
            po.ring_bytes_override = Some(*n * V2_BURST_BYTES);
            (V2_BURST_BYTES as usize * 64, (*n / 64).max(1) as usize)
        }
        _ => (def.subbuf_bytes, def.num_subbufs),
    };
    let chk = make(&pattern, sub_b, num, cfg, &po)?;
    let dir = ctx.run_dir()?;
    let stamp = util::stamp_now();
    let anom_p = match &o.anomalies_out {
        Some(p) => util::check_write_path(p)?,
        None => dir.join(format!("ring_check_{}_{stamp}.anomalies.jsonl", def.name)),
    };
    let side_p = dir.join(format!("ring_check_{}_{stamp}.subbuf.jsonl", def.name));
    let mut cons = CheckConsumer {
        chk,
        sidecar: std::fs::File::create(&side_p).ok().map(std::io::BufWriter::new),
    };
    cons.chk.recorder().open_sink(anom_p.clone())?;
    let deadline = Instant::now() + Duration::from_secs_f64(o.seconds.max(0.1));
    let t0 = Instant::now();
    if is_v2 {
        v2_loop(&mut ctl, &map, &def, &o, deadline, epoch, &mut cons, &mut rep)?;
    } else {
        legacy_loop(&mut ctl, &map, &def, &o, deadline, epoch, &mut cons, &mut rep)?;
    }
    let wall = t0.elapsed().as_secs_f64();
    let counters = hw_counters(&mut ctl);
    let cleanup_res = cleanup.run();
    let mut counters = counters;
    if is_v2 {
        v2_after_cleanup(ctx, &mut counters)?;
    }
    if let Some(s) = cons.sidecar.as_mut() {
        let _ = s.flush();
    }
    let mut s = cons.chk.summary();
    let rec = cons.chk.recorder();
    let total = rec.total;
    s["anomalies"] = json!(rec.records);
    s["anomaly_file"] = json!(anom_p.display().to_string());
    s["sidecar_path"] = json!(side_p.display().to_string());
    s["ring"] = json!(def.name);
    s["mapping"] = json!(map.name());
    s["seconds"] = json!(round3(wall));
    s["bytes_checked"] = json!(rep.bytes);
    s["subbuffers"] = json!(rep.chunks);
    s["subbuf_period_ms"] = json!(round3(t_sub_ms));
    s["num_buffers"] = json!(def.num_subbufs);
    s["ring_depth_ms"] = json!(round3(t_sub_ms * def.num_subbufs as f64));
    s["lap_threshold_ms"] = json!(round3(t_sub_ms * (def.num_subbufs as f64 - 1.0)));
    s["reader"] = reader_json(&rep, wall);
    s["stalls"] = json!(rep.stalls);
    s["hw"] = counters;
    s["bist"] = json!(bist);
    s["epoch_at_start"] = json!(epoch);
    s["notes"] = json!(notes);
    s["cleanup"] = json!(cleanup_res);
    s["pass"] = json!(total == 0 && rep.lost_bursts == 0 && rep.discarded_bursts == 0);
    if !warnings.is_empty() {
        s["warnings"] = json!(warnings);
    }
    Ok(s)
}

fn reader_json(rep: &ReaderReport, wall: f64) -> Value {
    json!({
        "polls": rep.polls,
        "wakes": rep.wakes,
        "chunks": rep.chunks,
        "bytes": rep.bytes,
        "busy_frac": if wall > 0.0 { round3(rep.busy_ns as f64 * 1e-9 / wall) } else { 0.0 },
        "backlog_max": rep.backlog_max,
        "hw_overflow_flags": rep.overflow_flags,
        "last_buffer_delta_hist": rep.delta_hist,
        "v2_lost_bursts": rep.lost_bursts,
        "v2_discarded_bursts": rep.discarded_bursts,
    })
}

#[allow(dead_code)]
fn _unused(_: &Core) {}

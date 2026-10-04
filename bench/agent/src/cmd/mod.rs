//! Subcommand dispatch and shared context.

pub mod ad9361;
pub mod adi_util;
pub mod audit;
pub mod boot;
pub mod eyescan;
pub mod hwval;
pub mod iio_cmd;
pub mod info;
pub mod maint;
pub mod mem;
pub mod net;
pub mod prbs;
pub mod profile;
pub mod reg;
pub mod replay;
pub mod ring;
pub mod sd;
pub mod telemetry;
pub mod tx;
pub mod txlink;
pub mod version;

use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::regio::{PhysMap, RegIo};
use crate::regmap::{Core, MapSet};
use crate::sys;
use crate::util;
use serde_json::{json, Value};
use std::cell::{OnceCell, RefCell};
use std::path::PathBuf;

pub const DEFAULT_SHARE: &str = "/mnt/sd/bench/share";
pub const BENCH_ROOT: &str = "/mnt/sd/bench";

pub struct Ctx {
    pub share_dir: PathBuf,
    pub run_id: String,
    pub run_dir_opt: Option<PathBuf>,
    pub verbose: u8,
    maps: OnceCell<MapSet>,
    warnings: RefCell<Vec<String>>,
}

impl Ctx {
    /// Consumes the global options.
    pub fn from_args(args: &Args) -> AResult<Ctx> {
        let share_dir = args
            .opt("share")?
            .or_else(|| std::env::var("FBENCH_SHARE").ok())
            .unwrap_or_else(|| DEFAULT_SHARE.to_string());
        let verbose = match args.opt_maybe("verbose") {
            None => 0,
            Some(None) => 1,
            Some(Some(v)) => v.parse().unwrap_or(1),
        };
        let _ = args.flag("json");
        let _ = args.flag("pretty");
        Ok(Ctx {
            share_dir: PathBuf::from(share_dir),
            run_id: args.opt("run-id")?.unwrap_or_else(|| format!("adhoc_{}", util::stamp_now())),
            run_dir_opt: args.opt("run-dir")?.map(PathBuf::from),
            verbose,
            maps: OnceCell::new(),
            warnings: RefCell::new(Vec::new()),
        })
    }

    pub fn maps(&self) -> &MapSet {
        self.maps.get_or_init(|| {
            let m = MapSet::load(&self.share_dir);
            for w in &m.warnings {
                self.warn(format!("regmap: {w}"));
            }
            m
        })
    }

    pub fn warn(&self, w: impl Into<String>) {
        let w = w.into();
        if self.verbose > 0 {
            eprintln!("fbench-agent: warning: {w}");
        }
        self.warnings.borrow_mut().push(w);
    }

    pub fn take_warnings(&self) -> Vec<String> {
        std::mem::take(&mut *self.warnings.borrow_mut())
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        if self.verbose > 0 {
            eprintln!("fbench-agent: {}", msg.as_ref());
        }
    }

    /// Run directory for bulk artifacts (created on demand):
    /// --run-dir, else /mnt/sd/bench/runs/<run_id> when the SD layout
    /// exists, else /tmp/fbench_runs/<run_id>.
    pub fn run_dir(&self) -> AResult<PathBuf> {
        let p = match &self.run_dir_opt {
            Some(p) => p.clone(),
            None => {
                if std::path::Path::new(BENCH_ROOT).is_dir() {
                    PathBuf::from(BENCH_ROOT).join("runs").join(&self.run_id)
                } else {
                    PathBuf::from("/tmp/fbench_runs").join(&self.run_id)
                }
            }
        };
        util::ensure_dir(p)
    }
}

pub fn command_name(args: &Args) -> String {
    let n = match args.word(0) {
        Some("reg") | Some("replay") | Some("ring") | Some("mem") | Some("sd") | Some("net") | Some("prbs")
        | Some("tx") | Some("maint") | Some("boot") | Some("hwval") => 2,
        Some("iio") | Some("ad9361") => 3,
        _ => 1,
    };
    args.words.iter().take(n).cloned().collect::<Vec<_>>().join(" ")
}

pub const COMMANDS: &[(&str, &str)] = &[
    ("version", "agent version and build info"),
    ("info", "identity: model, serial, image, bitstream, boot medium, SD, modules, services"),
    ("audit", "PS configuration audit (PLLs, DDR, DDRIOB, PL310, AFI, reserved memory, kmod)"),
    ("reg read|write|dump|list --core C [--reg R] [--value V]", "allow-listed register access"),
    ("telemetry --seconds N --interval-ms M [--jsonl FILE|-]", "XADC, AD9361 temp, CLK_FREQ, load, IRQs"),
    ("profile --pid PID|NAME [--seconds N] [--hz H] [--top K] [--symbols UNSTRIPPED_EXE]", "where a process's CPU goes: by thread and by function (perf sampling)"),
    ("iio attr get|set --dev D [--chan C [--out]] --attr A [--value V]", "IIO sysfs access"),
    ("iio debug get|set --dev D --attr A [--value V]", "IIO debugfs access"),
    ("ad9361 spi read|write --addr A [--value V]", "AD9361 SPI register via direct_reg_access"),
    ("eyescan --mode idelay|ad9361|2d|ad9361-driver --rate HZ --dwell-ms D [--lanes ..]", "RX interface eye scans"),
    ("prbs soak --seconds N --poll-ms P [--rate HZ]", "AD9361 BIST PRBS error-interval soak"),
    ("txlink --mode ad9361-loopback|fpga-loopback [--sweep]", "TX LVDS link test with DAC PN"),
    ("ring capture --ring R --bytes B --mapping cached|uncached --out FILE", "RAM-first raw ring capture (SigMF)"),
    ("ring check --ring R --pattern P --seconds N [--stall-ms ..] | --file F", "ring checker / anomaly classifier"),
    ("ring synth --pattern P --out FILE [--inject SPEC]", "synthetic capture with injected anomalies"),
    ("mem test --anon-mb N | --phys A --size S [--patterns ..] [--passes P] [--cpu C]", "PS memory test"),
    ("mem canary fill|verify --region NAME", "carve-out foreign-writer canary"),
    ("mem bw --size S [--region NAME|--phys A]", "memcpy/read/write bandwidth"),
    ("sd bench --mb N --bs K [--fsync] [--dir D]", "SD throughput and write latency"),
    ("net serve|send --port P --mb N [--host H]", "TCP throughput"),
    ("hwval init|id|census|ingest|ringv2|legacy|mt|evt|guard|contention ...", "Tier 1 hwval core operations"),
    ("replay stream --playlist P [--ring-mb M] [--status F] [--report F] [--on-underrun wait|zero]", "SD/RAM IQ files -> RAM ring -> int16 on stdout (for iio_writedev)"),
    ("replay check --playlist P | verify --file F [--sha256 H]", "validate a replay playlist / hash a staged file"),
    ("tx off", "max TX attenuation, DAC zero, DDS scale 0, loopback/BIST off"),
    ("maint enter|exit|status", "maintenance mode (stop/start the radio daemon: scanner or p25-httpd)"),
    ("boot status|install|select [NAME|--image NAME] [--reboot]", "dual boot-image swap helper"),
];

pub fn dispatch(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let w0 = args.word(0).unwrap_or("help");
    match w0 {
        "version" => version::run(ctx, args),
        "info" => info::run(ctx, args),
        "audit" => audit::run(ctx, args),
        "reg" => reg::run(ctx, args),
        "replay" => replay::run(ctx, args),
        "telemetry" => telemetry::run(ctx, args),
        "profile" => profile::run(ctx, args),
        "iio" => iio_cmd::run(ctx, args),
        "ad9361" => ad9361::run(ctx, args),
        "eyescan" => eyescan::run(ctx, args),
        "prbs" => prbs::run(ctx, args),
        "txlink" => txlink::run(ctx, args),
        "ring" => ring::run(ctx, args),
        "mem" => mem::run(ctx, args),
        "sd" => sd::run(ctx, args),
        "net" => net::run(ctx, args),
        "hwval" => hwval::run(ctx, args),
        "tx" => tx::run(ctx, args),
        "maint" => maint::run(ctx, args),
        "boot" => boot::run(ctx, args),
        "help" | "--help" => {
            args.finish()?;
            Ok(json!({
                "agent": "fbench-agent",
                "version": env!("CARGO_PKG_VERSION"),
                "commands": COMMANDS.iter().map(|(c, d)| json!({"usage": c, "desc": d})).collect::<Vec<_>>(),
                "global_options": ["--share DIR", "--run-id ID", "--run-dir DIR", "-v", "--pretty", "--json"],
            }))
        }
        other => Err(AgentError::new(
            Code::UnknownCommand,
            format!("unknown command '{other}' (try `help`)"),
        )),
    }
}

pub fn sub<'a>(args: &'a Args, idx: usize, allowed: &[&str]) -> AResult<&'a str> {
    let w = args.word(idx).ok_or_else(|| {
        AgentError::new(
            Code::Usage,
            format!("missing subcommand ({})", allowed.join("|")),
        )
    })?;
    if !allowed.contains(&w) {
        return Err(AgentError::new(
            Code::UnknownCommand,
            format!("unknown subcommand '{w}' ({})", allowed.join("|")),
        ));
    }
    Ok(w)
}

/// Verifies that the core is present on this image before any access.
pub fn check_presence(core: &Core) -> AResult<()> {
    if let Some(u) = &core.requires_uio {
        if sys::find_uio(u).is_none() {
            return Err(AgentError::new(
                Code::WrongImage,
                format!(
                    "core '{}' is not present: UIO '{u}' not found (UIO devices: {:?})",
                    core.name,
                    sys::uio_names()
                ),
            ));
        }
    }
    if let Some(i) = &core.requires_iio {
        if !sys::iio_present(i) {
            return Err(AgentError::new(
                Code::NoDevice,
                format!("core '{}' is not present: IIO device '{i}' not found", core.name),
            ));
        }
    }
    Ok(())
}

/// Presence check + /dev/mem mapping + product-ID check for PL cores with
/// a UIO requirement (p25 vs hwval share one address).
pub fn open_core<'m>(ctx: &'m Ctx, name: &str, writable: bool) -> AResult<(&'m Core, PhysMap)> {
    let core = ctx.maps().core(name)?;
    check_presence(core)?;
    let w = writable && !core.readonly;
    // PL cores with a UIO device are mapped through it (like p25-httpd);
    // /dev/mem is the fallback and the path for PS / ADI registers.
    let uio = core.requires_uio.as_deref().and_then(sys::find_uio);
    let mut pm = match uio {
        Some((num, _)) => match PhysMap::open_uio(num, core.base, core.size as usize, w) {
            Ok(pm) => pm,
            Err(e) if e.code == Code::WrongImage => return Err(e),
            Err(_) => PhysMap::open(core.base, core.size as usize, w)?,
        },
        None => PhysMap::open(core.base, core.size as usize, w)?,
    };
    if core.requires_uio.is_some() {
        if let (Some(idr), Some(idv)) = (&core.id_reg, core.id_value) {
            let r = core.get(idr)?;
            let v = pm.read32(r.offset);
            if v as u64 != idv {
                return Err(AgentError::new(
                    Code::WrongImage,
                    format!("core '{}' ID mismatch: {} = 0x{v:08X}, expected 0x{idv:08X}", core.name, r.name),
                ));
            }
        }
    }
    Ok((core, pm))
}

/// Agent build info (shared by version/info/sigmf).
pub fn build_info() -> Value {
    let ts: u64 = env!("FBENCH_BUILD_UNIX").parse().unwrap_or(0);
    json!({
        "version": env!("CARGO_PKG_VERSION"),
        "git": env!("FBENCH_GIT"),
        "built": util::iso8601_utc(ts, 0),
        "target": env!("FBENCH_TARGET"),
        "profile": env!("FBENCH_PROFILE"),
        "rustc": env!("FBENCH_RUSTC"),
        "p25_map": env!("FBENCH_P25_MAP_SOURCE"),
    })
}

/// Hardware serial (IIO context hw_serial from /etc/libiio.ini, else /etc/serial).
pub fn hw_serial() -> Option<String> {
    ini_value("/etc/libiio.ini", "hw_serial").or_else(|| util::read_trim("/etc/serial"))
}

pub fn ini_value(path: &str, key: &str) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?;
    for line in t.lines() {
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim().to_string();
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// Parses a JSONL output option: None, Some("-") for stdout, or a path.
pub fn jsonl_target(args: &Args) -> AResult<Option<String>> {
    Ok(match args.opt_maybe("jsonl") {
        None => None,
        Some(None) => Some("-".to_string()),
        Some(Some(p)) => Some(p),
    })
}

/// JSONL writer to stdout or a checked file.
pub struct Jsonl {
    out: Option<Box<dyn std::io::Write>>,
    pub path: Option<String>,
    pub lines: u64,
}

impl Jsonl {
    pub fn open(target: Option<&str>) -> AResult<Jsonl> {
        match target {
            None => Ok(Jsonl { out: None, path: None, lines: 0 }),
            Some("-") => Ok(Jsonl {
                out: Some(Box::new(std::io::stdout())),
                path: Some("-".into()),
                lines: 0,
            }),
            Some(p) => {
                let p = util::check_write_path(p)?;
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d)?;
                }
                let f = std::fs::File::create(&p)?;
                Ok(Jsonl {
                    out: Some(Box::new(std::io::BufWriter::new(f))),
                    path: Some(p.display().to_string()),
                    lines: 0,
                })
            }
        }
    }

    pub fn active(&self) -> bool {
        self.out.is_some()
    }

    pub fn to_stdout(&self) -> bool {
        self.path.as_deref() == Some("-")
    }

    pub fn write(&mut self, v: &Value) {
        if let Some(o) = self.out.as_mut() {
            let _ = writeln!(o, "{v}");
            if self.path.as_deref() == Some("-") {
                let _ = o.flush();
            }
            self.lines += 1;
        }
    }

    pub fn finish(&mut self) {
        if let Some(o) = self.out.as_mut() {
            let _ = o.flush();
        }
    }
}

use std::io::Write as _;

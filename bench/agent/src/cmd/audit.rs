//! `audit`: PS configuration audit (design doc sys.audit, F8/F12).

use super::Ctx;
use crate::access::RegAccess;
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::iio;
use crate::regio::PhysMap;
use crate::sha256;
use crate::sys;
use crate::util::{hex32, round3};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const PS_CLK_HZ: f64 = 33_333_333.0;
/// MT41K256M16TW-107 (Micron D9SHD) minimum CAS latency time.
pub const TAA_MIN_NS: f64 = 13.125;

#[derive(Default)]
pub struct Checks {
    pub list: Vec<Value>,
}

impl Checks {
    pub fn add(&mut self, name: &str, addr: Option<u64>, value: Option<u64>, expected: Value, ok: bool, severity: &str, detail: impl Into<String>) {
        self.list.push(json!({
            "name": name,
            "addr": addr.map(|a| format!("0x{a:08X}")),
            "value": value.map(|v| format!("0x{v:08X}")),
            "expected": expected,
            "ok": ok,
            "severity": severity,
            "detail": detail.into(),
        }));
    }
    pub fn failures(&self) -> usize {
        self.list
            .iter()
            .filter(|c| c["ok"] == json!(false) && c["severity"] == json!("fail"))
            .count()
    }
}

/// DDR3 MR0 CAS latency: code {A6,A5,A4,A2}.
pub fn mr0_cl(mr0: u32) -> Option<u32> {
    let code = (((mr0 >> 4) & 0x7) << 1) | ((mr0 >> 2) & 1);
    match code {
        0 => None,
        c if c & 1 == 0 => Some(c / 2 + 4),
        c => Some((c >> 1) + 12),
    }
}

/// DDR3 MR2 CAS write latency: A5:A3 + 5.
pub fn mr2_cwl(mr2: u32) -> u32 {
    ((mr2 >> 3) & 0x7) + 5
}

fn fdiv(v: u32) -> u32 {
    (v >> 12) & 0x7F
}

/// Pure evaluation of the DDR / PLL registers (host-testable).
pub fn evaluate(regs: &BTreeMap<String, u32>, checks: &mut Checks) -> Value {
    let get = |n: &str| regs.get(n).copied();
    let mut derived = serde_json::Map::new();
    let arm_pll = get("ARM_PLL_CTRL").map(|v| fdiv(v) as f64 * PS_CLK_HZ);
    let ddr_pll = get("DDR_PLL_CTRL").map(|v| fdiv(v) as f64 * PS_CLK_HZ);
    let io_pll = get("IO_PLL_CTRL").map(|v| fdiv(v) as f64 * PS_CLK_HZ);
    let mhz = |x: f64| round3(x / 1e6);
    if let (Some(p), Some(c)) = (arm_pll, get("ARM_CLK_CTRL")) {
        let div = ((c >> 8) & 0x3F).max(1) as f64;
        derived.insert("cpu_6x4x_mhz".into(), json!(mhz(p / div)));
    }
    let mut ddr_mhz = None;
    if let (Some(p), Some(c)) = (ddr_pll, get("DDR_CLK_CTRL")) {
        let div = ((c >> 20) & 0x3F).max(1) as f64;
        ddr_mhz = Some(p / div / 1e6);
        derived.insert("ddr_mhz".into(), json!(mhz(p / div)));
    }
    for (name, reg) in [("fclk0_mhz", "FPGA0_CLK_CTRL"), ("fclk1_mhz", "FPGA1_CLK_CTRL"), ("fclk2_mhz", "FPGA2_CLK_CTRL"), ("fclk3_mhz", "FPGA3_CLK_CTRL")] {
        if let Some(c) = get(reg) {
            let src = match (c >> 4) & 3 {
                2 => arm_pll,
                3 => ddr_pll,
                _ => io_pll,
            };
            if let Some(p) = src {
                let d0 = ((c >> 8) & 0x3F).max(1) as f64;
                let d1 = ((c >> 20) & 0x3F).max(1) as f64;
                derived.insert(name.into(), json!(mhz(p / d0 / d1)));
            }
        }
    }

    if let Some(v) = get("DDR_PLL_CTRL") {
        let f = fdiv(v);
        let (ok, detail) = match f {
            0x20 => (true, "533 MHz DDR (FSBL default)".to_string()),
            0x24 => (false, "overclock FSBL: CL7 at 600 MHz violates tAA for MT41K256M16TW-107".to_string()),
            other => (false, format!("unexpected DDR PLL FDIV 0x{other:X}")),
        };
        checks.add("ddr_pll_fdiv", Some(0xF800_0104), Some(v as u64), json!("FDIV 0x20"), ok, "fail", detail);
    }
    if let Some(v) = get("DDR_CLK_CTRL") {
        checks.add("ddr_clk_ctrl", Some(0xF800_0124), Some(v as u64), json!("0x0C200003"), v == 0x0C20_0003, "fail", "DDR 3x/2x clock dividers");
    }
    let mut cl = None;
    if let Some(v) = get("DRAM_EMR_MR_REG") {
        cl = mr0_cl(v & 0xFFFF);
        checks.add(
            "dram_emr_mr",
            Some(0xF800_6030),
            Some(v as u64),
            json!("0x00040B30"),
            v == 0x0004_0B30,
            "fail",
            format!("MR0 CL = {:?} (0x00040B30 = CL7; the P25 XSA ps7_init has 0x00040B70 = CL11)", cl),
        );
        derived.insert("cas_latency".into(), json!(cl));
    }
    if let Some(v) = get("DRAM_EMR_REG") {
        derived.insert("cas_write_latency".into(), json!(mr2_cwl(v & 0xFFFF)));
    }
    if let (Some(cl), Some(f)) = (cl, ddr_mhz) {
        let taa = cl as f64 * 1000.0 / f;
        derived.insert("taa_ns".into(), json!(round3(taa)));
        checks.add(
            "ddr_taa",
            None,
            None,
            json!(format!(">= {TAA_MIN_NS} ns")),
            taa >= TAA_MIN_NS - 0.01,
            "fail",
            format!("tAA = CL{cl} / {f:.1} MHz = {taa:.3} ns (MT41K256M16TW-107 min {TAA_MIN_NS} ns)"),
        );
    }
    if let Some(v) = get("ARM_PLL_CTRL") {
        checks.add("arm_pll_fdiv", Some(0xF800_0100), Some(v as u64), json!("FDIV 0x28"), fdiv(v) == 0x28, "warn", "ARM PLL 1333 MHz");
    }
    if let Some(v) = get("IO_PLL_CTRL") {
        checks.add("io_pll_fdiv", Some(0xF800_0108), Some(v as u64), json!("FDIV 0x1E"), fdiv(v) == 0x1E, "warn", "IO PLL 1000 MHz");
    }
    if let Some(v) = get("FPGA0_CLK_CTRL") {
        checks.add("fclk0", Some(0xF800_0170), Some(v as u64), json!("0x00200500 & 0x03F03F30 (100 MHz)"), v & 0x03F0_3F30 == 0x0020_0500, "warn", "FCLK0 (AXI-Lite)");
    }
    if let Some(v) = get("FPGA1_CLK_CTRL") {
        checks.add("fclk1", Some(0xF800_0180), Some(v as u64), json!("0x00100500 & 0x03F03F30 (200 MHz)"), v & 0x03F0_3F30 == 0x0010_0500, "warn", "FCLK1 (IDELAYCTRL ref)");
    }
    for p in 0..4 {
        let n = format!("AXI_PRIORITY_WR_PORT{p}");
        if let Some(v) = get(&n) {
            checks.add(&n.to_ascii_lowercase(), Some(0xF800_6208 + 4 * p), Some(v as u64), json!("(v & 0x000703FF) == 0x3FF"), v & 0x0007_03FF == 0x3FF, "warn", "DDRC write port priority");
        }
        let n = format!("AXI_PRIORITY_RD_PORT{p}");
        if let Some(v) = get(&n) {
            checks.add(&n.to_ascii_lowercase(), Some(0xF800_6218 + 4 * p), Some(v as u64), json!("(v & 0x000F03FF) == 0x3FF"), v & 0x000F_03FF == 0x3FF, "warn", "DDRC read port priority");
        }
    }
    if let Some(v) = get("L2C_CONTROL") {
        checks.add("l2c_enabled", Some(0xF8F0_2100), Some(v as u64), json!("bit0 = 1"), v & 1 == 1, "warn", "PL310 enabled");
    }
    if let Some(v) = get("L2C_AUX_CONTROL") {
        derived.insert(
            "l2c_prefetch".into(),
            json!({"data": (v >> 28) & 1, "instr": (v >> 29) & 1, "early_bresp": (v >> 30) & 1,
                   "prefetch_ctrl": get("L2C_PREFETCH_CTRL").map(hex32)}),
        );
    }
    Value::Object(derived)
}

fn find_ko(dir: &std::path::Path, names: &[&str], depth: u32, out: &mut Vec<std::path::PathBuf>) {
    if depth > 6 {
        return;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                find_ko(&p, names, depth + 1, out);
            } else if names.iter().any(|n| p.file_name().map(|f| f == *n).unwrap_or(false)) {
                out.push(p);
            }
        }
    }
}

fn kmod_json() -> Value {
    let loaded: Vec<Value> = sys::modules()
        .into_iter()
        .filter(|m| m["name"].as_str().map(|n| n.contains("maia")).unwrap_or(false))
        .collect();
    let mut files = Vec::new();
    if let Some(rel) = sys::kernel_release() {
        find_ko(&std::path::Path::new("/lib/modules").join(rel), &["maia-sdr.ko", "maia_sdr.ko"], 0, &mut files);
    }
    let files: Vec<Value> = files
        .iter()
        .map(|p| {
            let md = std::fs::metadata(p).ok();
            json!({
                "path": p.display().to_string(),
                "size": md.as_ref().map(|m| m.len()),
                "mtime": md.and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs()),
                "sha256": sha256::file_hex(p).ok(),
            })
        })
        .collect();
    json!({"loaded": loaded, "files": files,
           "note": "F8: the P25 image may carry a stale maia-sdr.ko (upstream v0.10.0) that invalidates L1 before L2"})
}

fn dmesg_lines() -> Vec<String> {
    let out = match std::process::Command::new("dmesg").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return Vec::new(),
    };
    let keys = ["reserved mem", "l2c", "l2x0", "cma", "outer cache", "maia", "no-map", "rxbuffer"];
    out.lines()
        .filter(|l| {
            let lc = l.to_ascii_lowercase();
            keys.iter().any(|k| lc.contains(k))
        })
        .take(120)
        .map(|s| s.to_string())
        .collect()
}

/// Parses `--afi-ports 1,2` into HP port numbers 0..=3.
fn afi_ports(args: &Args) -> AResult<Vec<u32>> {
    let mut ports = Vec::new();
    for p in args.list("afi-ports")?.unwrap_or_default() {
        match p.trim().parse::<u32>() {
            Ok(n) if n <= 3 => ports.push(n),
            _ => return Err(AgentError::new(Code::Usage, format!("--afi-ports: '{p}' is not an HP port 0..3"))),
        }
    }
    Ok(ports)
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    // AFI (S_AXI_HP bridge) registers live in the HP port's PL clock domain:
    // reading the block of a port whose PL clock is not running never
    // completes and the watchdog resets the board (observed 2026-09-26 on
    // the P25 image, where HP0/HP3 are unused). They are read only for the
    // ports the caller names, and the caller must know those are clocked.
    let afi_ports = afi_ports(args)?;
    args.finish()?;
    let maps = ctx.maps();
    let mut regs: BTreeMap<String, u32> = BTreeMap::new();
    let mut errors = Vec::new();
    let mut notes = Vec::new();
    if afi_ports.is_empty() {
        notes.push(json!("AFI registers skipped (pass --afi-ports with clocked HP ports only)"));
    }
    for core_name in ["slcr", "ddrc", "l2c", "afi"] {
        if core_name == "afi" && afi_ports.is_empty() {
            continue;
        }
        let core = maps.core(core_name)?;
        match PhysMap::open(core.base, core.size as usize, false) {
            Ok(mut pm) => {
                let mut a = RegAccess::new(core, &mut pm);
                for r in core.regs.clone() {
                    if core_name == "afi" && !afi_ports.contains(&(r.offset >> 12)) {
                        continue;
                    }
                    match a.read_reg(&r, false) {
                        Ok(v) => {
                            let key = if core_name == "l2c" { format!("L2C_{}", r.name) } else { r.name.clone() };
                            regs.insert(key, v);
                        }
                        Err(e) => errors.push(format!("{core_name}.{}: {}", r.name, e.msg)),
                    }
                }
            }
            Err(e) => errors.push(format!("{core_name}: {}", e.msg)),
        }
    }
    let mut checks = Checks::default();
    let derived = evaluate(&regs, &mut checks);

    // XADC DDR rail (DDR3L, 1.35 V +-5 %).
    let xadc = iio::xadc_read();
    if let Some(x) = &xadc {
        if let Some(v) = x.get("vccoddr").and_then(|v| v.as_f64()) {
            let ok = (v - 1.35).abs() <= 1.35 * 0.05;
            checks.add("vccoddr", None, None, json!("1.35 V +- 5 %"), ok, "fail", format!("{v:.3} V (DDR3L MT41K256M16TW-107)"));
        }
    }

    // Reserved memory vs /proc/iomem.
    let iomem = sys::iomem();
    let resmem: Vec<Value> = sys::reserved_memory()
        .iter()
        .map(|r| {
            let overlap = sys::overlaps_system_ram(&iomem, r.base, r.size);
            if r.no_map {
                checks.add(
                    &format!("reserved:{}", r.node),
                    Some(r.base),
                    None,
                    json!("no-map region outside System RAM"),
                    !overlap || iomem.is_empty(),
                    "fail",
                    format!("0x{:08X}+0x{:X}", r.base, r.size),
                );
            }
            json!({"node": r.node, "base": format!("0x{:08X}", r.base), "size": format!("0x{:X}", r.size),
                   "no_map": r.no_map, "label": r.label, "compatible": r.compatible,
                   "overlaps_system_ram": overlap})
        })
        .collect();

    // Leftover services on a P25 image.
    let services = sys::services_running();
    if sys::uio_names().iter().any(|u| u == "p25-core") && services.iter().any(|s| s == "maia-httpd") {
        checks.add("leftover_maia_httpd", None, None, json!("not running on the P25 image"), false, "warn", "maia-httpd is running next to the scanner");
    }
    let cmdline = sys::cmdline().unwrap_or_default();
    checks.add(
        "sd_boot_bootargs",
        None,
        None,
        json!("cmdline contains uio_pdrv_genirq.of_id"),
        cmdline.contains("uio_pdrv_genirq.of_id"),
        "warn",
        "boot image swap needs an SD-booted unit",
    );
    if let Some(v) = regs.get("REBOOT_STATUS") {
        checks.add("reboot_status", Some(0xF800_0258), Some(*v as u64), json!("info"), true, "info", format!("reason bits 0x{:02X} (BootROM/SWDT/AWDT/SLC/POR)", v >> 16 & 0xFF));
    }

    let failures = checks.failures();
    let regs_json: serde_json::Map<String, Value> = regs.iter().map(|(k, v)| (k.clone(), json!(hex32(*v)))).collect();
    Ok(json!({
        "pass": failures == 0 && errors.is_empty(),
        "failures": failures,
        "checks": checks.list,
        "derived": derived,
        "regs": regs_json,
        "xadc": xadc,
        "reserved_memory": resmem,
        "iomem": iomem.iter().map(|(a, b, n)| json!({"start": format!("0x{a:08X}"), "end": format!("0x{b:08X}"), "name": n})).collect::<Vec<_>>(),
        "cmdline": cmdline,
        "kmod": kmod_json(),
        "modules": sys::modules(),
        "services": services,
        "dmesg": dmesg_lines(),
        "read_errors": errors,
        "afi_ports": afi_ports,
        "notes": notes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mr_decoding() {
        assert_eq!(mr0_cl(0x0B30), Some(7));
        assert_eq!(mr0_cl(0x0B70), Some(11));
        assert_eq!(mr0_cl(0x0120), Some(6));
        assert_eq!(mr2_cwl(0x0008), 6);
        assert_eq!(mr2_cwl(0x0018), 8);
    }

    fn base_regs(fdiv: u32) -> BTreeMap<String, u32> {
        let mut r = BTreeMap::new();
        r.insert("DDR_PLL_CTRL".into(), fdiv << 12);
        r.insert("DDR_CLK_CTRL".into(), 0x0C20_0003);
        r.insert("DRAM_EMR_MR_REG".into(), 0x0004_0B30);
        r.insert("ARM_PLL_CTRL".into(), 0x28 << 12);
        r.insert("ARM_CLK_CTRL".into(), 0x1F00_0200);
        r.insert("IO_PLL_CTRL".into(), 0x1E << 12);
        r.insert("FPGA0_CLK_CTRL".into(), 0x0020_0500);
        r.insert("AXI_PRIORITY_WR_PORT0".into(), 0x0008_03FF);
        r.insert("AXI_PRIORITY_RD_PORT0".into(), 0x0000_03FF);
        r
    }

    #[test]
    fn stock_fsbl_passes() {
        let mut c = Checks::default();
        let d = evaluate(&base_regs(0x20), &mut c);
        assert_eq!(c.failures(), 0, "{:?}", c.list);
        assert_eq!(d["cas_latency"], 7);
        assert!((d["ddr_mhz"].as_f64().unwrap() - 533.333).abs() < 0.01);
        assert!((d["fclk0_mhz"].as_f64().unwrap() - 100.0).abs() < 0.01);
        assert!((d["cpu_6x4x_mhz"].as_f64().unwrap() - 666.667).abs() < 0.01);
    }

    #[test]
    fn overclock_fsbl_fails_with_message() {
        let mut c = Checks::default();
        evaluate(&base_regs(0x24), &mut c);
        let f: Vec<&Value> = c.list.iter().filter(|x| x["ok"] == json!(false)).collect();
        assert!(f.iter().any(|x| x["detail"].as_str().unwrap().contains("overclock FSBL: CL7 at 600 MHz violates tAA for MT41K256M16TW-107")));
        assert!(f.iter().any(|x| x["name"] == "ddr_taa"));
        assert_eq!(c.failures(), 2);
    }
}

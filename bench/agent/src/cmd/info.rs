use super::{build_info, hw_serial, ini_value, open_core, Ctx, BENCH_ROOT};
use crate::access::RegAccess;
use crate::cli::Args;
use crate::err::AResult;
use crate::iio;
use crate::safety;
use crate::sys;
use crate::util::{hex32, read_trim};
use serde_json::{json, Value};

fn ascii_id(v: u32) -> String {
    v.to_be_bytes()
        .iter()
        .map(|b| if b.is_ascii_graphic() { *b as char } else { '.' })
        .collect()
}

/// Identifies the bitstream by reading the product-ID registers of the
/// core whose UIO device is present (never probing absent cores).
fn bitstream(ctx: &Ctx) -> (String, Value) {
    let uios = sys::uio_names();
    if uios.iter().any(|u| u == "p25-core") {
        let r = open_core(ctx, "p25", false).and_then(|(core, mut pm)| {
            let mut a = RegAccess::new(core, &mut pm);
            let id = a.read("product_id")?;
            let ver = a.read("version")?;
            let ctl = a.read("control")?;
            Ok(json!({
                "core": "p25", "product_id": hex32(id), "name": ascii_id(id),
                "version": format!("{}.{}.{}", (ver >> 16) & 0xFF, (ver >> 8) & 0xFF, ver & 0xFF),
                "platform": ver >> 24, "sdr_reset": ctl & 1,
            }))
        });
        return (
            "p25".into(),
            r.unwrap_or_else(|e| json!({"core": "p25", "error": e.msg, "code": e.code.as_str()})),
        );
    }
    if uios.iter().any(|u| u == "hwval-core") {
        let r = open_core(ctx, "hwval", false).and_then(|(core, mut pm)| {
            let mut a = RegAccess::new(core, &mut pm);
            let id = a.read("ID")?;
            let ver = a.read("VERSION")?;
            let mut v = json!({
                "core": "hwval", "product_id": hex32(id), "name": ascii_id(id),
                "version": format!("{}.{}.{}", (ver >> 16) & 0xFF, (ver >> 8) & 0xFF, ver & 0xFF),
                "features": a.try_read("FEATURES").map(hex32),
                "map_source": core.source,
            });
            if let (Some(lo), Some(hi), Some(st)) = (a.try_read("DNA_LO"), a.try_read("DNA_HI"), a.try_read("DNA_STATUS")) {
                let dna = ((hi as u64 & 0x1FF_FFFF) << 32) | lo as u64;
                v["fpga_dna"] = if st & 1 == 1 { json!(format!("0x{dna:015X}")) } else { Value::Null };
                v["dna_status"] = json!(st);
            }
            Ok(v)
        });
        return (
            "hwval".into(),
            r.unwrap_or_else(|e| json!({"core": "hwval", "error": e.msg, "code": e.code.as_str()})),
        );
    }
    let versions = std::fs::read_to_string("/opt/VERSIONS").unwrap_or_default();
    let image = if uios.iter().any(|u| u == "maia-sdr") {
        "maia"
    } else if versions.contains("plutosdr-fw") || versions.contains("device-fw v0.") {
        "factory"
    } else {
        "unknown"
    };
    (image.into(), json!({"core": null, "uio": uios}))
}

fn boot_medium(cmdline: &str) -> &'static str {
    if cmdline.contains("uio_pdrv_genirq.of_id") {
        "sd"
    } else if cmdline.contains("root=/dev/ram") || cmdline.contains("ramdisk") {
        "flash"
    } else if cmdline.is_empty() {
        "unknown"
    } else {
        "other"
    }
}

fn sd_json() -> Value {
    let mounted = sys::mount_of("/mnt/sd");
    let st = sys::statvfs(std::path::Path::new("/mnt/sd"));
    let bench = std::path::Path::new(BENCH_ROOT);
    let list = |d: &str| -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(bench.join(d))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        v.sort();
        v
    };
    json!({
        "mounted": mounted.is_some(),
        "device": mounted.as_ref().map(|m| m.0.clone()),
        "fstype": mounted.as_ref().map(|m| m.1.clone()),
        "total_mb": st.map(|s| s.total / (1 << 20)),
        "free_mb": st.map(|s| s.avail / (1 << 20)),
        "bench_dir": bench.is_dir(),
        "agent_installed": bench.join("bin/fbench-agent").exists(),
        "share": list("share"),
        "images": list("images"),
    })
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    args.finish()?;
    let cmdline = sys::cmdline().unwrap_or_default();
    let (image, bs) = bitstream(ctx);
    let phy = iio::Ad9361::open().ok();
    let mem = sys::meminfo();
    let mut warnings = Vec::new();
    if phy.is_none() {
        warnings.push("ad9361-phy IIO device not found".to_string());
    }
    if cmdline.is_empty() {
        warnings.push("/proc/cmdline unreadable".to_string());
    }
    let fpga_dna = bs.get("fpga_dna").cloned().unwrap_or(Value::Null);
    Ok(json!({
        "model": read_trim("/proc/device-tree/model"),
        "serial": hw_serial(),
        "hw_model": ini_value("/etc/libiio.ini", "hw_model"),
        "fw_version": ini_value("/etc/libiio.ini", "fw_version"),
        "hostname": sys::hostname(),
        "image": image,
        "bitstream": bs,
        "fpga_dna": fpga_dna,
        "ad936x_compatible": phy.as_ref().and_then(|p| p.compatible()),
        "ad936x_note": "AD9361 and AD9363 cannot be told apart in software; see bench config",
        "sample_rate_hz": phy.as_ref().and_then(|p| p.sample_rate()),
        "rx_lo_hz": phy.as_ref().and_then(|p| p.rx_lo()),
        "cmdline": cmdline,
        "boot_medium": boot_medium(&cmdline),
        "uptime_s": sys::uptime_s(),
        "boot_id": sys::boot_id(),
        "kernel": sys::kernel_release(),
        "kernel_version": sys::kernel_version(),
        "cpus": sys::ncpus(),
        "mem": {"total_kb": mem.get("MemTotal"), "available_kb": mem.get("MemAvailable"),
                "cma_total_kb": mem.get("CmaTotal"), "cma_free_kb": mem.get("CmaFree")},
        "sd": sd_json(),
        "iio_devices": sys::iio_devices().iter().map(|d| json!({"id": d.id, "name": d.name})).collect::<Vec<_>>(),
        "uio": sys::uio_names(),
        "modules": sys::modules(),
        "services": sys::services_running(),
        "services_detail": sys::services_json(),
        "maintenance": safety::in_maintenance(),
        "agent_path": std::env::current_exe().ok().map(|p| p.display().to_string()),
        "build": build_info(),
        "warnings": warnings,
    }))
}

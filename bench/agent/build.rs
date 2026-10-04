//! Build script: embeds build metadata and generates the built-in P25 register
//! allow-list from `p25-httpd/p25-pac/p25.svd`.
//!
//! The SVD does not mark read-to-clear (Rsticky) registers or clock domains,
//! so those safety annotations are added here from the facts in
//! `scanner-hdl/radio_core/p25_top.py` (bank 0 is the AXI-Lite domain, every other
//! bank crosses into `sync` through a RegisterCDC; the listed status words
//! carry Rsticky fields). If the SVD is not available (partial checkout) the
//! checked-in `maps/p25_regs.fallback.json` is used instead.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Word offsets whose read clears a sticky bit (Access.Rsticky in p25_top.py).
const P25_READ_TO_CLEAR: &[u32] = &[0x0C, 0x60, 0x80, 0xA4, 0xC4, 0xE0, 0x144, 0x184, 0x1A0, 0x1C0];

/// Bank names (bits [8:5] of the byte address), from p25_top.py. Traffic chain 2's LSM bank
/// is 16 words, so it spans two 32-byte banks.
const P25_BANKS: &[(u32, &str)] = &[
    (0x00, "control"),
    (0x20, "sdr"),
    (0x40, "traffic_sdr"),
    (0x60, "traffic_iq"),
    (0x80, "iq"),
    (0xA0, "lsm"),
    (0xC0, "traffic_lsm"),
    (0xE0, "wideband_iq"),
    (0x100, "lsm_seed"),
    (0x120, "traffic2_sdr"),
    (0x140, "traffic2_lsm"),
    (0x160, "traffic2_lsm_seed"),
    (0x180, "spectrometer"),
    (0x1A0, "pre_diff_iq"),
    (0x1C0, "traffic_pre_diff_iq"),
];

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());

    // ── build metadata ────────────────────────────────────────────────
    let git = git_describe(&manifest).unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=FBENCH_GIT={git}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=FBENCH_BUILD_UNIX={now}");
    println!(
        "cargo:rustc-env=FBENCH_TARGET={}",
        env::var("TARGET").unwrap_or_default()
    );
    println!(
        "cargo:rustc-env=FBENCH_PROFILE={}",
        env::var("PROFILE").unwrap_or_default()
    );
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let rustc_v = Command::new(rustc)
        .arg("-V")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    println!("cargo:rustc-env=FBENCH_RUSTC={rustc_v}");
    let head = manifest.join("../../.git/HEAD");
    if head.exists() {
        println!("cargo:rerun-if-changed={}", head.display());
    }

    // ── P25 register map from the SVD ─────────────────────────────────
    let svd = manifest.join("../../p25-httpd/p25-pac/p25.svd");
    let fallback = manifest.join("maps/p25_regs.fallback.json");
    println!("cargo:rerun-if-changed={}", svd.display());
    println!("cargo:rerun-if-changed={}", fallback.display());
    println!("cargo:rerun-if-changed=build.rs");
    let json = match fs::read_to_string(&svd) {
        Ok(text) => {
            let map = p25_map_from_svd(&text);
            println!("cargo:rustc-env=FBENCH_P25_MAP_SOURCE=svd");
            serde_json::to_string_pretty(&map).unwrap()
        }
        Err(_) => {
            println!("cargo:rustc-env=FBENCH_P25_MAP_SOURCE=fallback");
            fs::read_to_string(&fallback).expect("p25 SVD and fallback map both missing")
        }
    };
    fs::write(out_dir.join("p25_regs.json"), json).unwrap();
}

fn git_describe(dir: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let mut s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--", "."])
        .current_dir(dir)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    if dirty {
        s.push_str("-dirty");
    }
    Some(s)
}

/// Returns the text of every `<tag>…</tag>` element (non-nested) in `s`.
fn elements<'a>(s: &'a str, tag: &str) -> Vec<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find(&open) {
        let after = &rest[i + open.len()..];
        match after.find(&close) {
            Some(j) => {
                out.push(&after[..j]);
                rest = &after[j + close.len()..];
            }
            None => break,
        }
    }
    out
}

fn first<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
    elements(s, tag).into_iter().next().map(|x| x.trim())
}

fn parse_num(s: &str) -> u32 {
    let s = s.trim();
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(h, 16).unwrap()
    } else {
        s.parse().unwrap()
    }
}

fn access_code(svd_access: &str) -> &'static str {
    match svd_access.trim() {
        "read-only" => "ro",
        "write-only" => "wo",
        _ => "rw",
    }
}

fn p25_map_from_svd(svd: &str) -> serde_json::Value {
    use serde_json::{json, Value};
    let mut blocks: Vec<(u32, &str, Vec<Value>)> = P25_BANKS
        .iter()
        .map(|(off, name)| (*off, *name, Vec::new()))
        .collect();
    for reg in elements(svd, "register") {
        let name = first(reg, "name").unwrap();
        let off = parse_num(first(reg, "addressOffset").unwrap());
        let access = access_code(first(reg, "access").unwrap_or("read-write"));
        let desc = first(reg, "description").unwrap_or(name);
        let mut fields = Vec::new();
        if let Some(fs) = first(reg, "fields") {
            for f in elements(fs, "field") {
                let fname = first(f, "name").unwrap();
                let range = first(f, "bitRange").unwrap();
                let inner = range.trim_start_matches('[').trim_end_matches(']');
                let mut it = inner.split(':');
                let msb = parse_num(it.next().unwrap());
                let lsb = parse_num(it.next().unwrap());
                let facc = access_code(first(f, "access").unwrap_or("read-write"));
                fields.push(json!({
                    "name": fname,
                    "lsb": lsb,
                    "width": msb - lsb + 1,
                    "access": facc,
                    "desc": first(f, "description").unwrap_or(fname),
                }));
            }
        }
        let bank = off & !0x1F;
        let domain = if bank == 0 { "axi_lite" } else { "sync" };
        let rtc = P25_READ_TO_CLEAR.contains(&off);
        let entry = json!({
            "name": name,
            "offset": format!("0x{off:03X}"),
            "access": access,
            "width": 32,
            "reset": null,
            "snapshot": null,
            "desc": desc,
            "domain": domain,
            "read_side_effect": rtc,
            "fields": fields,
        });
        match blocks.iter_mut().find(|b| b.0 == bank) {
            Some(b) => b.2.push(entry),
            None => panic!("p25 register {name} at 0x{off:X} is in an unknown bank"),
        }
    }
    let blocks: Vec<Value> = blocks
        .into_iter()
        .filter(|b| !b.2.is_empty())
        .map(|(off, name, regs)| {
            json!({
                "name": name,
                "offset": format!("0x{off:03X}"),
                "domain": if off == 0 { "axi_lite" } else { "sync" },
                "regs": regs,
            })
        })
        .collect();
    json!({
        "schema": "fbench.regmap/1",
        "core": "p25",
        "base": "0x7C460000",
        "size": 4096,
        "id_reg": "product_id",
        "id_value": "0x70323566",
        "snapshot_domains": {},
        "requires": {"uio": "p25-core"},
        "reset_gate": {
            "reg": "control",
            "bit": 0,
            "domains": ["sync"],
            "desc": "control.sdr_reset (powers up 1) holds the sync domain in reset; any access to a sync-domain bank hangs the AXI-Lite bus while it is set"
        },
        "vacant": [["0x1E0", "0x200"]],
        "decode_limit": "0x200",
        "source": "generated from p25-httpd/p25-pac/p25.svd by bench/agent/build.rs",
        "blocks": blocks,
    })
}

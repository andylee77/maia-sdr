//! Build script: embeds build metadata, and the radio core's register map
//! (`bench/share/p25_regs.json`, written by `scanner-hdl`'s `radio_core.bench_map` from the
//! core's own register banks, with its read-to-clear registers and clock domains).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    // The stamp follows new commits: HEAD names the branch, whose ref file moves with each one.
    let git_dir = manifest.join("../../.git");
    let head = git_dir.join("HEAD");
    if let Ok(text) = fs::read_to_string(&head) {
        println!("cargo:rerun-if-changed={}", head.display());
        if let Some(branch) = text.trim().strip_prefix("ref: ") {
            println!("cargo:rerun-if-changed={}", git_dir.join(branch).display());
        }
    }

    // ── The radio core's register map ─────────────────────────────────
    let map = manifest.join("../share/p25_regs.json");
    println!("cargo:rerun-if-changed={}", map.display());
    println!("cargo:rerun-if-changed=build.rs");
    let json = fs::read_to_string(&map).expect("bench/share/p25_regs.json is missing");
    println!("cargo:rustc-env=FBENCH_P25_MAP_SOURCE=share/p25_regs.json");
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

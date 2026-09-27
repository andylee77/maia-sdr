//! End-to-end tests of the agent binary on the host (no hardware): the
//! one-JSON-object contract, exit codes, allow-list refusals, share-map
//! loading and the ring checker on synthetic captures.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("fbench_it_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Runs the agent; returns (reply, exit code). Asserts the stdout contract.
fn agent(args: &[&str], root: Option<&Path>) -> (Value, i32) {
    let mut c = Command::new(env!("CARGO_BIN_EXE_fbench-agent"));
    c.args(args);
    if let Some(r) = root {
        c.env("FBENCH_EXTRA_WRITE_ROOT", r);
    }
    let out = c.output().expect("run agent");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(lines.len(), 1, "exactly one JSON line for {args:?}, got: {stdout}");
    let v: Value = serde_json::from_str(lines[0]).expect("valid JSON");
    let code = out.status.code().unwrap_or(-1);
    assert_eq!(v["ok"] == Value::Bool(true), code == 0, "ok <-> exit 0 for {args:?}: {v}");
    (v, code)
}

#[test]
fn version_and_help() {
    let (v, c) = agent(&["version"], None);
    assert_eq!(c, 0);
    assert_eq!(v["cmd"], "version");
    assert!(v["version"].is_string());
    assert!(v["build"]["git"].is_string());
    let (h, _) = agent(&["help"], None);
    assert!(h["commands"].as_array().unwrap().len() >= 20);
}

#[test]
fn usage_and_unknown_command_codes() {
    let (v, c) = agent(&["version", "--bogus", "1"], None);
    assert_eq!((v["code"].as_str(), c), (Some("usage"), 2));
    let (v, c) = agent(&["frobnicate"], None);
    assert_eq!((v["code"].as_str(), c), (Some("unknown_command"), 3));
    let (v, c) = agent(&["reg", "read", "--core", "p25"], None);
    assert_eq!((v["code"].as_str(), c), (Some("usage"), 2));
}

#[test]
fn boot_install_without_from_never_swaps() {
    // A bare `install` must not fall through to `select` (which rewrites the
    // SD boot files); it is a usage error before any filesystem access.
    let (v, c) = agent(&["boot", "install", "hwval"], None);
    assert_eq!((v["code"].as_str(), c), (Some("usage"), 2), "{v}");
}

#[test]
fn allow_list_refusals_happen_before_any_access() {
    // Vacant P25 bank: not in the map.
    let (v, c) = agent(&["reg", "read", "--core", "p25", "--reg", "0x120"], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
    // Read-to-clear status word.
    let (v, c) = agent(&["reg", "read", "--core", "p25", "--reg", "wideband_iq_dma_status"], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
    // PS registers are read-only for the agent.
    let (v, c) = agent(&["reg", "write", "--core", "ddrc", "--reg", "DRAM_EMR_MR_REG", "--value", "0"], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
    // TX-affecting DAC write without --tx-ok.
    let (v, c) = agent(&["reg", "write", "--core", "adi_dac", "--reg", "DAC_CHAN0_CNTRL_7", "--value", "9"], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
    // Unknown core.
    let (v, c) = agent(&["reg", "read", "--core", "nope", "--reg", "X"], None);
    assert_eq!((v["code"].as_str(), c), (Some("not_found"), 3), "{v}");
}

#[test]
fn share_maps_override_builtins() {
    let share = tmpdir("share");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/hwval_small.json"),
        share.join("hwval_regs.json"),
    )
    .unwrap();
    let s = share.to_str().unwrap();
    let (v, c) = agent(&["reg", "list", "--core", "hwval", "--share", s], None);
    assert_eq!(c, 0, "{v}");
    assert_eq!(v["regs"].as_array().unwrap().len(), 7);
    assert!(v["core"]["source"].as_str().unwrap().starts_with("share:"));
    // The built-in safety floor (UIO requirement) survived the override.
    assert_eq!(v["core"]["requires"]["uio"], "hwval-core");
    let (v, _) = agent(&["reg", "list"], None);
    let cores: Vec<&str> = v["cores"].as_array().unwrap().iter().map(|c| c["core"].as_str().unwrap()).collect();
    for want in ["p25", "adi_adc", "adi_dac", "rx_dmac", "tx_dmac", "slcr", "ddrc", "l2c", "afi", "hwval"] {
        assert!(cores.contains(&want), "{want} missing from {cores:?}");
    }
    let _ = std::fs::remove_dir_all(share);
}

#[test]
fn writes_outside_allowed_roots_are_refused() {
    let d = std::env::temp_dir().join("not_fbench_dir");
    let (v, c) = agent(&["sd", "bench", "--mb", "1", "--dir", d.to_str().unwrap()], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
    let (v, c) = agent(&["ring", "synth", "--pattern", "ramp64", "--out", d.join("x").to_str().unwrap()], None);
    assert_eq!((v["code"].as_str(), c), (Some("safety"), 4), "{v}");
}

#[test]
fn synth_then_check_every_pattern() {
    let root = tmpdir("ring");
    for (p, sub, n) in [
        ("ramp64", "64k", "4"),
        ("tagged", "64k", "4"),
        ("prbs31", "64k", "4"),
        ("iqramp", "64k", "4"),
        ("tone", "64k", "4"),
        ("pn0fn", "256k", "16"),
    ] {
        let base = root.join(p);
        let (s, c) = agent(
            &["ring", "synth", "--pattern", p, "--out", base.to_str().unwrap(), "--subbufs", "16",
              "--subbuf-bytes", sub, "--ring-subbufs", n],
            Some(&root),
        );
        assert_eq!(c, 0, "{s}");
        let data = format!("{}.sigmf-data", base.display());
        let (r, c) = agent(&["ring", "check", "--file", &data], Some(&root));
        assert_eq!(c, 0, "{r}");
        assert_eq!(r["counts"], s["expected_counts"], "{p}: {r}");
        assert!(r["anomalies"].as_array().unwrap().len() as u64 <= 32);
        let anomaly_file = r["anomaly_file"].as_str().unwrap();
        let lines = std::fs::read_to_string(anomaly_file).unwrap().lines().count() as u64;
        assert_eq!(lines, r["anomalies_total"].as_u64().unwrap(), "{p}");
        assert!(Path::new(&format!("{}.sigmf-meta", base.display())).exists());
        assert!(Path::new(&format!("{}.subbuf.jsonl", base.display())).exists());
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn check_file_with_explicit_pattern_and_no_meta() {
    let root = tmpdir("raw");
    let base = root.join("raw");
    let (_, c) = agent(
        &["ring", "synth", "--pattern", "ramp64", "--inject", "lap@2:1", "--out", base.to_str().unwrap(),
          "--subbufs", "4", "--subbuf-bytes", "32k", "--ring-subbufs", "4"],
        Some(&root),
    );
    assert_eq!(c, 0);
    let _ = std::fs::remove_file(format!("{}.sigmf-meta", base.display()));
    let data = format!("{}.sigmf-data", base.display());
    // Without meta the geometry must be given (and the pattern).
    let (r, c) = agent(
        &["ring", "check", "--file", &data, "--pattern", "ramp64", "--subbuf-bytes", "32k", "--num-subbufs", "4"],
        Some(&root),
    );
    assert_eq!(c, 0, "{r}");
    assert_eq!(r["counts"]["lap"], 1);
    assert_eq!(r["lost_units"], 4 * 4096);
    let _ = std::fs::remove_dir_all(root);
}

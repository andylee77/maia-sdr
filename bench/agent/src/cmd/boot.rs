//! `boot status | install <name> [--from DIR] | select <name> [--reboot]`
//! (design doc section 2.4). The only writes outside /mnt/sd/bench are the
//! two boot files at the SD root, always from a hash-verified source, via
//! temp file + fsync + rename, with a verified backup of the production pair
//! in /mnt/sd/bench/images/p25/ made once.

use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code, Context};
use crate::sha256;
use crate::sys;
use crate::util;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SD_ROOT: &str = "/mnt/sd";
pub const IMAGES: &str = "/mnt/sd/bench/images";
pub const BOOT_FILES: &[&str] = &["BOOT.bin", "devicetree.dtb"];
pub const MANIFEST: &str = "SHA256SUMS";

/// Parses `sha256sum` output: "<hex>  <name>" (also "<hex> *<name>").
pub fn parse_manifest(text: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    for line in text.lines() {
        let mut it = line.split_whitespace();
        if let (Some(h), Some(n)) = (it.next(), it.next()) {
            if h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit()) {
                let n = n.trim_start_matches('*');
                let n = n.rsplit('/').next().unwrap_or(n);
                m.insert(n.to_string(), h.to_ascii_lowercase());
            }
        }
    }
    m
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn hash(p: &Path) -> Option<String> {
    sha256::file_hex(p).ok()
}

/// Verifies an image directory against its manifest.
fn verify_dir(dir: &Path) -> Value {
    let manifest = std::fs::read_to_string(dir.join(MANIFEST)).ok().map(|t| parse_manifest(&t));
    let mut files = serde_json::Map::new();
    let mut all_present = true;
    let mut all_ok = manifest.is_some();
    for f in BOOT_FILES {
        let p = dir.join(f);
        let present = p.is_file();
        all_present &= present;
        let h = if present { hash(&p) } else { None };
        let want = manifest.as_ref().and_then(|m| m.get(*f).cloned());
        let ok = matches!((&h, &want), (Some(a), Some(b)) if a == b);
        all_ok &= ok;
        files.insert(
            f.to_string(),
            json!({"present": present, "size": std::fs::metadata(&p).ok().map(|m| m.len()),
                   "sha256": h, "manifest": want, "sha256_ok": ok}),
        );
    }
    json!({"present": all_present, "sha256_ok": all_present && all_ok, "manifest": manifest.is_some(), "files": files})
}

fn root_pair() -> Value {
    let mut m = serde_json::Map::new();
    for f in BOOT_FILES {
        let p = Path::new(SD_ROOT).join(f);
        m.insert(
            f.to_string(),
            json!({"present": p.is_file(), "size": std::fs::metadata(&p).ok().map(|m| m.len()), "sha256": hash(&p)}),
        );
    }
    Value::Object(m)
}

fn list_images() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(IMAGES)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

fn status() -> Value {
    let root = root_pair();
    let mut images = serde_json::Map::new();
    let mut active = None;
    for name in list_images() {
        let v = verify_dir(&Path::new(IMAGES).join(&name));
        let matches = BOOT_FILES.iter().all(|f| {
            let a = v["files"][*f]["sha256"].as_str();
            a.is_some() && a == root[*f]["sha256"].as_str()
        });
        if matches && active.is_none() {
            active = Some(name.clone());
        }
        let mut v = v;
        v["active"] = json!(matches);
        images.insert(name, v);
    }
    let cmdline = sys::cmdline().unwrap_or_default();
    json!({
        "active": active,
        "images": images,
        "root": root,
        "sd_mounted": sys::mount_of(SD_ROOT).is_some(),
        "sd_boot": cmdline.contains("uio_pdrv_genirq.of_id"),
        "backup_present": Path::new(IMAGES).join("p25").join(BOOT_FILES[0]).is_file(),
    })
}

/// Copies with fsync and verifies the destination hash.
fn copy_verified(src: &Path, dst: &Path, want: &str) -> AResult<()> {
    let tmp = dst.with_extension("fbench-new");
    {
        let mut i = std::fs::File::open(src).ctx(format!("open {}", src.display()))?;
        let mut o = std::fs::File::create(&tmp).ctx(format!("create {}", tmp.display()))?;
        let mut buf = vec![0u8; 1 << 16];
        loop {
            let n = i.read(&mut buf)?;
            if n == 0 {
                break;
            }
            o.write_all(&buf[..n])?;
        }
        o.sync_all()?;
    }
    let got = hash(&tmp).unwrap_or_default();
    if got != want {
        let _ = std::fs::remove_file(&tmp);
        return Err(AgentError::new(
            Code::Error,
            format!("verification of {} failed after copy ({got} != {want})", tmp.display()),
        ));
    }
    std::fs::rename(&tmp, dst).ctx(format!("rename {} -> {}", tmp.display(), dst.display()))?;
    if let Some(d) = dst.parent() {
        sys::fsync_dir(d);
    }
    let after = hash(dst).unwrap_or_default();
    if after != want {
        return Err(AgentError::new(Code::Error, format!("{} hash mismatch after rename", dst.display())));
    }
    Ok(())
}

fn write_manifest(dir: &Path, hashes: &BTreeMap<String, String>) -> AResult<()> {
    let mut s = String::new();
    for (f, h) in hashes {
        s.push_str(&format!("{h}  {f}\n"));
    }
    let p = util::check_write_path(dir.join(MANIFEST))?;
    let mut f = std::fs::File::create(&p)?;
    f.write_all(s.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Backs up the current root pair to images/p25 once.
fn backup_production(steps: &mut Vec<Value>) -> AResult<()> {
    let dir = PathBuf::from(IMAGES).join("p25");
    if BOOT_FILES.iter().all(|f| dir.join(f).is_file()) {
        steps.push(json!("production backup already present (images/p25), not touched"));
        return Ok(());
    }
    let dir = util::ensure_dir(&dir)?;
    let mut hashes = BTreeMap::new();
    for f in BOOT_FILES {
        let src = Path::new(SD_ROOT).join(f);
        let h = hash(&src).ok_or_else(|| AgentError::new(Code::Precondition, format!("{} missing: cannot back up", src.display())))?;
        let dst = util::check_write_path(dir.join(f))?;
        copy_verified(&src, &dst, &h)?;
        hashes.insert(f.to_string(), h);
    }
    write_manifest(&dir, &hashes)?;
    steps.push(json!({"backup": dir.display().to_string(), "sha256": hashes}));
    Ok(())
}

fn select(name: &str, reboot: bool) -> AResult<Value> {
    if sys::mount_of(SD_ROOT).is_none() && !cfg!(test) {
        return Err(AgentError::new(Code::Precondition, "/mnt/sd is not mounted"));
    }
    let src_dir = PathBuf::from(IMAGES).join(name);
    let mut steps = Vec::new();
    // Back up first (before verifying 'p25' itself, so a first swap back
    // to p25 is refused rather than creating a backup of a non-p25 pair).
    if name != "p25" {
        backup_production(&mut steps)?;
    }
    let v = verify_dir(&src_dir);
    if v["sha256_ok"] != json!(true) {
        return Err(AgentError::new(
            Code::Precondition,
            format!("image '{name}' is missing files or fails its {MANIFEST} verification"),
        )
        .with_detail(v));
    }
    let root = root_pair();
    let already = BOOT_FILES
        .iter()
        .all(|f| root[*f]["sha256"].as_str() == v["files"][*f]["sha256"].as_str());
    if already {
        return Ok(json!({"image": name, "changed": false, "reboot_required": false,
                          "steps": steps, "status": status()}));
    }
    for f in BOOT_FILES {
        let want = v["files"][*f]["sha256"].as_str().unwrap().to_string();
        let dst = Path::new(SD_ROOT).join(f);
        copy_verified(&src_dir.join(f), &dst, &want)?;
        steps.push(json!({"installed": dst.display().to_string(), "sha256": want}));
    }
    sys::sync_all();
    let st = status();
    let mut out = json!({"image": name, "changed": true, "reboot_required": true, "steps": steps, "status": st});
    if reboot {
        let r = std::process::Command::new("reboot").status();
        out["reboot"] = json!(r.map(|s| s.success()).unwrap_or(false));
    }
    Ok(out)
}

fn stage(name: &str, from: &str, sha_boot: Option<String>, sha_dtb: Option<String>) -> AResult<Value> {
    let from = PathBuf::from(from);
    let mut want: BTreeMap<String, String> = std::fs::read_to_string(from.join(MANIFEST))
        .ok()
        .map(|t| parse_manifest(&t))
        .unwrap_or_default();
    if let Some(h) = sha_boot {
        want.insert("BOOT.bin".into(), h.to_ascii_lowercase());
    }
    if let Some(h) = sha_dtb {
        want.insert("devicetree.dtb".into(), h.to_ascii_lowercase());
    }
    let dir = util::ensure_dir(PathBuf::from(IMAGES).join(name))?;
    let mut hashes = BTreeMap::new();
    for f in BOOT_FILES {
        let src = from.join(f);
        let h = hash(&src).ok_or_else(|| AgentError::new(Code::NotFound, format!("{} missing", src.display())))?;
        let exp = want.get(*f).ok_or_else(|| {
            AgentError::new(Code::Precondition, format!("no expected sha256 for {f} ({}/SHA256SUMS or --sha256-boot/--sha256-dtb)", from.display()))
        })?;
        if &h != exp {
            return Err(AgentError::new(Code::Error, format!("{} sha256 {h} != expected {exp}", src.display())));
        }
        copy_verified(&src, &util::check_write_path(dir.join(f))?, &h)?;
        hashes.insert(f.to_string(), h);
    }
    write_manifest(&dir, &hashes)?;
    Ok(json!({"image": name, "staged": dir.display().to_string(), "sha256": hashes, "changed": false,
              "reboot_required": false, "status": status()}))
}

pub fn run(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 1, &["status", "install", "select"])?;
    let name = args.opt("image")?.or_else(|| args.word(2).map(|s| s.to_string()));
    let from = args.opt("from")?;
    let sha_boot = args.opt("sha256-boot")?;
    let sha_dtb = args.opt("sha256-dtb")?;
    let reboot = args.flag("reboot");
    args.finish()?;
    if op == "status" {
        return Ok(status());
    }
    let name = name.ok_or_else(|| AgentError::new(Code::Usage, "image name required (boot select <name> | --image NAME)"))?;
    if !valid_name(&name) {
        return Err(AgentError::new(Code::Usage, "image names are [A-Za-z0-9_-]{1,32}"));
    }
    match (op, from) {
        ("install", Some(dir)) => stage(&name, &dir, sha_boot, sha_dtb),
        // `install` only stages; swapping the SD boot files is always an
        // explicit `select`.
        ("install", None) => Err(AgentError::new(
            Code::Usage,
            "boot install requires --from DIR (it only stages an image; use `boot select <name>` to switch)",
        )),
        _ => select(&name, reboot),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parse() {
        let m = parse_manifest(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855  BOOT.bin\n\
             E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855 *images/hwval/devicetree.dtb\n\
             garbage line\n",
        );
        assert_eq!(m.len(), 2);
        assert!(m["devicetree.dtb"].starts_with("e3b0"));
        assert!(valid_name("hwval"));
        assert!(!valid_name("../x"));
    }
}

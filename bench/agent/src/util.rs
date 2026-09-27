//! Small pure helpers: number parsing, time formatting, path safety.

use crate::err::{AResult, AgentError, Code};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Parses decimal, `0x` hex, `0b` binary or `0o` octal; `_` separators allowed.
pub fn parse_u64(s: &str) -> Option<u64> {
    let t: String = s.trim().chars().filter(|c| *c != '_').collect();
    let (digits, radix) = if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        (h, 16)
    } else if let Some(b) = t.strip_prefix("0b").or_else(|| t.strip_prefix("0B")) {
        (b, 2)
    } else if let Some(o) = t.strip_prefix("0o") {
        (o, 8)
    } else {
        (t.as_str(), 10)
    };
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, radix).ok()
}

/// Parses a size: plain integer (bytes) or with K/M/G (binary) suffix,
/// optionally followed by `B`/`iB`: `4096`, `64k`, `16M`, `1GiB`, `0x1000000`.
pub fn parse_size(s: &str) -> Option<u64> {
    let t = s.trim();
    if t.starts_with("0x") || t.starts_with("0X") {
        return parse_u64(t);
    }
    let lower = t.to_ascii_lowercase();
    let lower = lower
        .strip_suffix("ib")
        .or_else(|| lower.strip_suffix('b'))
        .unwrap_or(&lower)
        .to_string();
    let (num, mult) = match lower.chars().last()? {
        'k' => (&lower[..lower.len() - 1], 1u64 << 10),
        'm' => (&lower[..lower.len() - 1], 1u64 << 20),
        'g' => (&lower[..lower.len() - 1], 1u64 << 30),
        _ => (&lower[..], 1u64),
    };
    if let Ok(v) = num.parse::<u64>() {
        return v.checked_mul(mult);
    }
    let f: f64 = num.parse().ok()?;
    if f < 0.0 {
        return None;
    }
    Some((f * mult as f64).round() as u64)
}

pub fn hex32(v: u32) -> String {
    format!("0x{v:08X}")
}

pub fn hex64(v: u64) -> String {
    format!("0x{v:016X}")
}

pub fn hexn(v: u64) -> String {
    format!("0x{v:X}")
}

/// Process-wide monotonic clock origin.
fn origin() -> Instant {
    use std::sync::OnceLock;
    static O: OnceLock<Instant> = OnceLock::new();
    *O.get_or_init(Instant::now)
}

/// Monotonic nanoseconds. On Linux this is CLOCK_MONOTONIC (comparable
/// across processes on the same boot); elsewhere it is relative to the first
/// call in this process.
pub fn mono_ns() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: valid pointer to a timespec.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
    #[cfg(not(target_os = "linux"))]
    {
        origin().elapsed().as_nanos() as u64
    }
}

pub fn since_start() -> Duration {
    origin().elapsed()
}

pub fn unix_now() -> (u64, u32) {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

pub fn unix_now_f64() -> f64 {
    let (s, n) = unix_now();
    s as f64 + n as f64 * 1e-9
}

/// Days since 1970-01-01 -> (year, month, day). Howard Hinnant's algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`
pub fn iso8601_utc(secs: u64, nanos: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60,
        nanos / 1_000_000
    )
}

pub fn iso_now() -> String {
    let (s, n) = unix_now();
    iso8601_utc(s, n)
}

/// `YYYYmmdd_HHMMSS` (UTC) for default run ids / file names.
pub fn stamp_now() -> String {
    let (s, _) = unix_now();
    let days = (s / 86_400) as i64;
    let rem = s % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}_{:02}{:02}{:02}",
        rem / 3600,
        (rem / 60) % 60,
        rem % 60
    )
}

pub fn read_trim(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim_end_matches(['\n', '\r', '\0', ' ']).to_string())
}

/// Lexically normalises a path (resolves `.` and `..` without touching the
/// filesystem).
pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Environment override used only by host-side tests: an extra directory
/// under which the agent may write. Never set on the board.
pub const EXTRA_ROOT_ENV: &str = "FBENCH_EXTRA_WRITE_ROOT";

/// True if `p` (already normalised, absolute) is inside an allowed write
/// root: `/mnt/sd/bench/**`, `/tmp/fbench*` (a `/tmp` entry whose name
/// starts with `fbench`, and anything below it), or the test override.
pub fn is_allowed_write_path(p: &Path) -> bool {
    let comps: Vec<String> = p
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().to_string()),
            _ => None,
        })
        .collect();
    let is_root_abs = p.has_root();
    if is_root_abs {
        if comps.len() >= 3 && comps[0] == "mnt" && comps[1] == "sd" && comps[2] == "bench" {
            return true;
        }
        if comps.len() >= 2 && comps[0] == "tmp" && comps[1].starts_with("fbench") {
            return true;
        }
    }
    if let Ok(extra) = std::env::var(EXTRA_ROOT_ENV) {
        if !extra.is_empty() {
            let root = normalize(Path::new(&extra));
            if p.starts_with(&root) {
                return true;
            }
        }
    }
    false
}

/// Validates a path the agent is about to create/write/delete. Resolves
/// symlinks of the deepest existing ancestor so a link cannot escape the
/// allowed roots. Returns the normalised path.
pub fn check_write_path(p: impl AsRef<Path>) -> AResult<PathBuf> {
    let p = p.as_ref();
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let norm = normalize(&abs);
    if !is_allowed_write_path(&norm) {
        return Err(AgentError::new(
            Code::Safety,
            format!(
                "refusing to write {}: only /mnt/sd/bench/** and /tmp/fbench* are writable (safety rule 6)",
                norm.display()
            ),
        ));
    }
    // Resolve symlinks on the existing part of the path.
    let mut probe = norm.clone();
    let mut tail = Vec::new();
    while !probe.exists() {
        match probe.file_name() {
            Some(n) => tail.push(n.to_os_string()),
            None => break,
        }
        if !probe.pop() {
            break;
        }
    }
    if probe.exists() {
        if let Ok(real) = std::fs::canonicalize(&probe) {
            let mut resolved = real;
            for t in tail.iter().rev() {
                resolved.push(t);
            }
            // On Windows canonicalize returns \\?\ paths; compare only on unix.
            #[cfg(unix)]
            {
                if !is_allowed_write_path(&resolved) {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!(
                            "refusing to write {}: resolves outside the allowed roots ({})",
                            norm.display(),
                            resolved.display()
                        ),
                    ));
                }
            }
            #[cfg(not(unix))]
            let _ = resolved;
        }
    }
    Ok(norm)
}

/// Creates a directory (and parents) after the path check.
pub fn ensure_dir(p: impl AsRef<Path>) -> AResult<PathBuf> {
    let p = check_write_path(p)?;
    std::fs::create_dir_all(&p)?;
    Ok(p)
}

/// Writes a small file atomically-ish (temp + rename) after the path check.
pub fn write_file(p: impl AsRef<Path>, data: &[u8]) -> AResult<PathBuf> {
    let p = check_write_path(p)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = p.with_extension("fbench-tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, &p)?;
    Ok(p)
}

pub fn sext12(v: u32) -> i32 {
    ((v << 20) as i32) >> 20
}

/// Reverses the low 12 bits.
pub fn rev12(v: u32) -> u32 {
    (v & 0xFFF).reverse_bits() >> 20
}

pub fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

pub fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(parse_u64("0x7C46_0000"), Some(0x7C46_0000));
        assert_eq!(parse_u64("42"), Some(42));
        assert_eq!(parse_u64("0b101"), Some(5));
        assert_eq!(parse_u64("x"), None);
        assert_eq!(parse_size("16M"), Some(16 << 20));
        assert_eq!(parse_size("64k"), Some(65536));
        assert_eq!(parse_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_size("0x1000"), Some(4096));
        assert_eq!(parse_size("1.5M"), Some(3 << 19));
        assert_eq!(parse_size("100"), Some(100));
    }

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(iso8601_utc(0, 0), "1970-01-01T00:00:00.000Z");
        // 2026-09-26T12:34:56Z
        assert_eq!(iso8601_utc(1_790_426_096, 5_000_000), "2026-09-26T12:34:56.005Z");
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
    }

    #[test]
    fn write_roots() {
        assert!(is_allowed_write_path(Path::new("/mnt/sd/bench/runs/x/y.bin")));
        assert!(is_allowed_write_path(Path::new("/tmp/fbench_sd/a")));
        assert!(is_allowed_write_path(Path::new("/tmp/fbench")));
        assert!(!is_allowed_write_path(Path::new("/mnt/sd/BOOT.bin")));
        assert!(!is_allowed_write_path(Path::new("/tmp/other/fbench")));
        assert!(!is_allowed_write_path(Path::new("/etc/passwd")));
        let n = normalize(Path::new("/mnt/sd/bench/../BOOT.bin"));
        assert!(!is_allowed_write_path(&n));
    }

    #[test]
    fn bits() {
        assert_eq!(sext12(0xFFF), -1);
        assert_eq!(sext12(0x7FF), 2047);
        assert_eq!(sext12(0x800), -2048);
        assert_eq!(rev12(0x001), 0x800);
        assert_eq!(rev12(0x800), 0x001);
        assert_eq!(rev12(0xA5F), 0xFA5);
    }
}

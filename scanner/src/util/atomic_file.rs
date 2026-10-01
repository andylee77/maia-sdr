//! Crash-safe file replacement: a power cut leaves either the old file or the new one.
//!
//! Write a temporary file beside the target, fsync it, rename it over the target, then fsync
//! the directory so the rename itself is durable (JFFS2 and FAT both need it).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

/// Replace `path` with `bytes`, creating its directory if needed.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = temp_path(path);
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(dir)
}

/// Replace `path` with `value` as pretty JSON.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let mut body = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    body.push(b'\n');
    write(path, &body)
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp"))
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Directories cannot be opened for syncing on other systems (host tests on Windows).
#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_the_file_and_leaves_no_temporary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("a.json");
        write(&path, b"one").unwrap();
        write_json(&path, &serde_json::json!({"two": 2})).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"two\": 2\n}\n");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap()).unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["a.json"]);
    }
}

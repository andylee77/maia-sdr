//! Recording file names and folders, and the listing of the SD card at boot.
//!
//! A name is `rec_<start ms>_<call id>_tg<tg>[_from<source>][.<site>].wav`, so the list is rebuilt
//! from the names alone (files made before sites were kept have no site part). Files the parser
//! does not recognise are left alone, never deleted.
//!
//! On the card a recording sits in the folder of its call's UTC date, `YYYY-MM-DD`, or
//! `YYYY-MM-DD_<n>` once the day's earlier folders hold the cap (FAT32 allows a directory 65,536
//! entries, 7-8 for each recording, and looks a name up by scanning its directory); locked ones in
//! the same folders under `locked/`.

use std::path::{Path, PathBuf};

use super::{wav, Recording, Store};
use crate::util::time::utc_date;

/// The tree of locked recordings, in the recordings directory.
pub const LOCKED: &str = "locked";

/// The folder of the `n`th (from 1) of a day's folders.
pub fn folder_name(day: &str, n: usize) -> String {
    if n <= 1 { day.to_string() } else { format!("{day}_{n}") }
}

/// The `n`th (from 1) folder of the day of `started_unix_ms`, in `dir` or in its locked tree.
pub fn day_folder(dir: &Path, started_unix_ms: u64, locked: bool, n: usize) -> PathBuf {
    let base = if locked { dir.join(LOCKED) } else { dir.to_path_buf() };
    base.join(folder_name(&utc_date(started_unix_ms), n))
}

/// Whether `name` is a day's folder (`YYYY-MM-DD` or `YYYY-MM-DD_<n>`).
pub fn is_day_folder(name: &str) -> bool {
    let (day, n) = match name.split_once('_') {
        Some((d, n)) => (d, Some(n)),
        None => (name, None),
    };
    let b = day.as_bytes();
    let digits = |r: std::ops::Range<usize>| b[r].iter().all(u8::is_ascii_digit);
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && digits(0..4) && digits(5..7) && digits(8..10)
        && n.is_none_or(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub started_unix_ms: u64,
    pub call: u64,
    pub tg: u32,
    pub source: Option<u32>,
    /// "" when the name has none.
    pub site: String,
}

pub fn file_name(started_unix_ms: u64, call: u64, tg: u32, source: Option<u32>, site: &str) -> String {
    let from = source.map(|s| format!("_from{s}")).unwrap_or_default();
    format!("rec_{started_unix_ms}_{call}_tg{tg}{from}{}.wav", site_suffix(site))
}

/// `.<site>`, or "" when the site is unknown or not safe in a file name.
fn site_suffix(site: &str) -> String {
    let ok = !site.is_empty() && site.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok { format!(".{site}") } else { String::new() }
}

pub fn parse(name: &str) -> Option<Parsed> {
    let body = name.strip_prefix("rec_")?.strip_suffix(".wav")?;
    let (body, site) = match body.split_once('.') {
        Some((b, s)) if !s.is_empty() && !s.contains('.') => (b, s.to_string()),
        Some(_) => return None,
        None => (body, String::new()),
    };
    let mut it = body.split('_');
    let started_unix_ms = it.next()?.parse().ok()?;
    let call = it.next()?.parse().ok()?;
    let tg = it.next()?.strip_prefix("tg")?.parse().ok()?;
    let source = match it.next() {
        Some(s) => Some(s.strip_prefix("from")?.parse().ok()?),
        None => None,
    };
    if it.next().is_some() {
        return None;
    }
    Some(Parsed { started_unix_ms, call, tg, source, site })
}

/// The recordings in `dir`'s day folders and its locked tree's, oldest first, and a note for the
/// status; an error when `dir` cannot be read. Only the names are read: on the card's FAT each
/// file's size costs a scan of its whole directory (2,260 files in one took 16 s), so sizes come
/// from the history (`reconcile`), which measures only the files it does not know. Listing names
/// alone is quick: 8,640 in 0.3 s with the caches dropped.
pub fn list(dir: &Path) -> Result<(Vec<Recording>, String), String> {
    let rd = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut loose = 0usize;
    let mut folders: Vec<(PathBuf, bool)> = Vec::new();
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if is_day_folder(&name) {
            folders.push((e.path(), false));
        } else if name == LOCKED {
            for l in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
                if is_day_folder(&l.file_name().to_string_lossy()) {
                    folders.push((l.path(), true));
                }
            }
        } else {
            loose += usize::from(name.ends_with(".wav"));
        }
    }
    for (folder, locked) in folders {
        for e in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(p) = parse(&name) else {
                skipped += usize::from(name.ends_with(".wav"));
                continue;
            };
            out.push(Recording {
                id: p.call,
                site: p.site,
                tg: p.tg,
                source: p.source,
                started_unix_ms: p.started_unix_ms,
                file: name,
                store: Store::Sd,
                sources: p.source.into_iter().collect(),
                locked,
                path: e.path(),
                ..Recording::default()
            });
        }
    }
    // Two files with one id (ids restarted after a boot that listed nothing): the newer is
    // listed, the other stays on the card untouched.
    out.sort_by_key(|r| r.id);
    out.dedup_by(|b, a| {
        if a.id != b.id {
            return false;
        }
        if b.started_unix_ms > a.started_unix_ms {
            std::mem::swap(a, b);
        }
        true
    });
    // Retention removes from the front, so the order is by time, not id.
    out.sort_by_key(|r| (r.started_unix_ms, r.id));
    let skipped = if skipped > 0 { format!(" ({skipped} unrecognised .wav names skipped)") } else { String::new() };
    let loose = if loose > 0 { format!(" ({loose} .wav files outside the day folders not listed)") } else { String::new() };
    let note = format!("indexed {} recording(s) in {}{skipped}{loose}", out.len(), dir.display());
    Ok((out, note))
}

/// A listed file's size and length, from the file.
pub fn measure(r: &mut Recording) {
    r.bytes = std::fs::metadata(&r.path).map(|m| m.len()).unwrap_or(0);
    r.duration_ms = wav::duration_ms(r.bytes);
}

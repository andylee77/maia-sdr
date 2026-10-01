//! Recording file names, and the listing of the SD card at boot.
//!
//! A name is `rec_<start ms>_<call id>_tg<tg>[_from<source>][.<site>].wav`, so the list is rebuilt
//! from the names alone (files made before sites were kept have no site part). Files the parser
//! does not recognise are left alone, never deleted.

use std::path::Path;

use super::{wav, Recording, Store};

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

/// The recordings in `dir`, oldest first, and a note for the status. Only the names and sizes
/// are read: a call's other details are not kept across a restart.
pub fn list(dir: &Path) -> (Vec<Recording>, String) {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => return (Vec::new(), format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    let mut skipped = 0usize;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(p) = parse(&name) else {
            skipped += usize::from(name.ends_with(".wav"));
            continue;
        };
        let bytes = e.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(Recording {
            id: p.call,
            site: p.site,
            tg: p.tg,
            source: p.source,
            started_unix_ms: p.started_unix_ms,
            duration_ms: wav::duration_ms(bytes),
            bytes,
            file: name,
            store: Store::Sd,
            sources: p.source.into_iter().collect(),
            path: e.path(),
            ..Recording::default()
        });
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
    let note = format!("indexed {} recording(s) in {}{skipped}", out.len(), dir.display());
    (out, note)
}

//! Copy p25-httpd's history (v1) into a new v2 database, in one transaction. The v1 file is only
//! read. Its schema is recognised by shape: `user_version` is 1 with or without the `channel`
//! column.
//!
//! - calls keep their row ids (the radios refer to them), with v1's columns renamed (`chain` →
//!   `lane`, `imbe` → `frames`, `vocoder_errors` → `frame_errors`) and the codec of their site's
//!   protocol;
//! - `call_units` become `transmissions` (v1 kept no per-radio times);
//! - the hour tables are copied as stored, not rebuilt, so the totals already seen stay;
//! - the sites keep their counts and get their systems from the configuration;
//! - each system's talkgroups and radios are computed from the calls.
//!
//! Old rows keep their known faults: not-followed calls stored with no grant time cannot be
//! repaired, and calls filed under the wrong site stay where they are.

use std::path::Path;

use rusqlite::{params, Connection};

use super::store::{SiteInfo, Store};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub calls: u64,
    pub transmissions: u64,
    pub hours: u64,
    pub radio_events: u64,
}

/// Is `path` a v1 history? (It has `calls.imbe`.)
pub fn is_v1(path: &Path) -> bool {
    let Ok(conn) = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY) else { return false };
    conn.prepare("SELECT 1 FROM pragma_table_info('calls') WHERE name = 'imbe'").and_then(|mut s| s.exists([])).unwrap_or(false)
}

/// Copy `v1` into `store` (a new, empty v2 database). `sites` are the configured sites, so each
/// call's system and codec are known.
pub fn migrate(v1: &Path, store: &Store, sites: &[SiteInfo]) -> rusqlite::Result<Report> {
    for s in sites {
        store.note_site(s)?;
    }
    store.with_writer(|conn| {
        let uri = format!("file:{}?mode=ro", v1.display().to_string().replace('\\', "/"));
        conn.execute("ATTACH DATABASE ?1 AS v1", params![uri])?;
        let res = copy(conn);
        let _ = conn.execute("DETACH DATABASE v1", []);
        res
    })
}

fn copy(conn: &mut Connection) -> rusqlite::Result<Report> {
    let has_channel = conn.prepare("SELECT 1 FROM pragma_table_info('calls', 'v1') WHERE name = 'channel'")?.exists([])?;
    let channel = if has_channel { "c.channel" } else { "NULL" };
    let tx = conn.transaction()?;
    let calls = tx.execute(
        &format!(
            "INSERT INTO calls (id, site, call_id, started_ms, ended_ms, target, tg, source, freq_hz, channel, lane,
             encrypted, followed, not_followed, voice_ms, grant_ms, codec, frames, frame_errors, close_reason)
             SELECT c.id, c.site, c.call_id, c.started_ms, c.ended_ms, 'group', c.tg, c.source, c.freq_hz, {channel},
             c.chain, c.encrypted, c.followed, c.not_followed, c.voice_ms, c.grant_ms,
             CASE WHEN c.imbe = 0 THEN NULL
                  WHEN (SELECT y.protocol FROM sites x JOIN systems y ON y.id = x.system WHERE x.id = c.site) = 'dmr_tier3'
                  THEN 'ambe2' ELSE 'imbe' END,
             c.imbe, c.vocoder_errors, c.close_reason
             FROM v1.calls c ORDER BY c.id"
        ),
        [],
    )?;
    let transmissions = tx.execute(
        "INSERT INTO transmissions (call, site, unit, started_ms, primary_src)
         SELECT call, site, unit, started_ms, primary_src FROM v1.call_units ORDER BY rowid",
        [],
    )?;
    let tg_hours = tx.execute(
        "INSERT INTO tg_hour (site, t, tg, calls, followed, encrypted, voice_ms, clear_grant_ms, enc_grant_ms,
         voiced_grant_ms, first_ms, last_ms)
         SELECT site, t, tg, calls, followed, encrypted, voice_ms, clear_grant_ms, enc_grant_ms, voiced_grant_ms,
         first_ms, last_ms FROM v1.hour_tg",
        [],
    )?;
    let radio_hours = tx.execute(
        "INSERT INTO radio_hour (site, t, unit, tg, calls, encrypted, voice_ms, clear_grant_ms, enc_grant_ms, last_ms)
         SELECT site, t, unit, tg, calls, encrypted, voice_ms, clear_grant_ms, enc_grant_ms, last_ms FROM v1.hour_unit",
        [],
    )?;
    let radio_events = tx.execute(
        "INSERT INTO radio_events (site, unit, tg, kind, first_ms, last_ms, count)
         SELECT site, unit, tg, kind, first_ms, last_ms, count FROM v1.unit_events",
        [],
    )?;
    tx.execute(
        "INSERT INTO sites (id, calls, first_ms, last_ms) SELECT site, calls, first_ms, last_ms FROM v1.site_stats WHERE true
         ON CONFLICT(id) DO UPDATE SET calls = excluded.calls, first_ms = excluded.first_ms, last_ms = excluded.last_ms",
        [],
    )?;
    tx.execute(
        "INSERT INTO talkgroups (system, tg, first_ms, last_ms, calls, first_encrypted_ms, last_encrypted_ms, last_clear_ms)
         SELECT COALESCE(s.system, c.site), c.tg, MIN(c.started_ms), MAX(c.started_ms), COUNT(*),
         MIN(CASE WHEN c.encrypted THEN c.started_ms END), MAX(CASE WHEN c.encrypted THEN c.started_ms END),
         MAX(CASE WHEN NOT c.encrypted THEN c.started_ms END)
         FROM calls c LEFT JOIN sites s ON s.id = c.site GROUP BY 1, 2",
        [],
    )?;
    tx.execute(
        "INSERT INTO radios (system, unit, first_ms, last_ms, calls)
         SELECT COALESCE(s.system, t.site), t.unit, MIN(t.started_ms), MAX(t.started_ms), COUNT(*)
         FROM transmissions t LEFT JOIN sites s ON s.id = t.site GROUP BY 1, 2",
        [],
    )?;
    tx.commit()?;
    Ok(Report {
        calls: calls as u64,
        transmissions: transmissions as u64,
        hours: (tg_hours + radio_hours) as u64,
        radio_events: radio_events as u64,
    })
}

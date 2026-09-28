//! Change 072: per-site activity history, in SQLite.
//!
//! Every finished call (followed or not) is stored with its site and the
//! radios in it; group affiliations and registrations are kept per
//! (site, radio, talkgroup). The queries answer "who talks, on what, and
//! how much": totals, per talkgroup, per radio, which talkgroups a radio
//! uses, encryption, and time series per hour / day.
//!
//! Two kinds of time, never added together as if they were the same:
//!
//! - voice: decoded on the voice channel (IMBE frames x 20 ms), for the
//!   calls that were followed. Measured.
//! - grant: from the grant to the last grant update on the control
//!   channel. The only time known for a call that was not followed
//!   (encrypted, or no chain free). It covers the whole grant: hang
//!   time, and any other radio keying up on the channel meanwhile,
//!   which the control channel does not show. It is credited to the
//!   radio that was granted the channel.
//!
//! `voice_per_grant` (decoded voice per grant second on the followed
//! calls) scales grant time to a voice estimate.
//!
//! Totals come from per-hour tables (`hour_tg`, `hour_unit`) kept with
//! the calls, so a month on a busy site (10k calls a day) reads a few
//! thousand rows, not 300k calls. Their windows start at the hour
//! (`Range::first_hour`). The calls themselves serve listings and CSV.
//!
//! The database lives on the SD card (`/mnt/sd/p25-history.sqlite`), or
//! in RAM when there is no card. All calls are blocking: callers use
//! `spawn_blocking`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

pub const SD_PATH: &str = "/mnt/sd/p25-history.sqlite";
pub const RAM_PATH: &str = "/tmp/p25-history.sqlite";
/// Calls older than this are pruned (daily).
pub const RETENTION_DAYS: u64 = 365;
/// Most space the data may use, on the SD card / in RAM; the oldest
/// calls go first. (Freed pages are reused: the file stops growing.) A
/// busy site (10k calls a day) uses about 6 MB a day.
pub const MAX_BYTES_SD: u64 = 2 << 30;
pub const MAX_BYTES_RAM: u64 = 16 << 20;

const HOUR_MS: u64 = 3_600_000;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS calls (
    id INTEGER PRIMARY KEY,
    site TEXT NOT NULL,
    call_id INTEGER NOT NULL,
    started_ms INTEGER NOT NULL,
    ended_ms INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    source INTEGER,
    freq_hz INTEGER,
    chain INTEGER NOT NULL DEFAULT 0,
    encrypted INTEGER NOT NULL,
    followed INTEGER NOT NULL,
    not_followed TEXT,
    voice_ms INTEGER NOT NULL,
    grant_ms INTEGER NOT NULL,
    imbe INTEGER NOT NULL DEFAULT 0,
    vocoder_errors INTEGER NOT NULL DEFAULT 0,
    close_reason TEXT,
    UNIQUE(site, call_id, started_ms)
);
CREATE INDEX IF NOT EXISTS calls_site_time ON calls(site, started_ms);
CREATE INDEX IF NOT EXISTS calls_site_tg ON calls(site, tg, started_ms);
CREATE TABLE IF NOT EXISTS call_units (
    call INTEGER NOT NULL REFERENCES calls(id) ON DELETE CASCADE,
    site TEXT NOT NULL,
    unit INTEGER NOT NULL,
    started_ms INTEGER NOT NULL,
    primary_src INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS call_units_site_unit ON call_units(site, unit, started_ms);
CREATE INDEX IF NOT EXISTS call_units_call ON call_units(call);
-- Per hour (t: its start, UTC) and talkgroup. voiced_grant_ms: grant
-- time of the calls with decoded voice (for voice_per_grant).
CREATE TABLE IF NOT EXISTS hour_tg (
    site TEXT NOT NULL,
    t INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    followed INTEGER NOT NULL,
    encrypted INTEGER NOT NULL,
    voice_ms INTEGER NOT NULL,
    clear_grant_ms INTEGER NOT NULL,
    enc_grant_ms INTEGER NOT NULL,
    voiced_grant_ms INTEGER NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    PRIMARY KEY(site, t, tg)
) WITHOUT ROWID;
-- Per hour, radio and talkgroup: the calls it took part in, the time
-- of those it was the primary radio of.
CREATE TABLE IF NOT EXISTS hour_unit (
    site TEXT NOT NULL,
    t INTEGER NOT NULL,
    unit INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    calls INTEGER NOT NULL,
    encrypted INTEGER NOT NULL,
    voice_ms INTEGER NOT NULL,
    clear_grant_ms INTEGER NOT NULL,
    enc_grant_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    PRIMARY KEY(site, t, unit, tg)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS hour_unit_unit ON hour_unit(site, unit, t);
CREATE TABLE IF NOT EXISTS site_stats (
    site TEXT PRIMARY KEY,
    calls INTEGER NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS unit_events (
    site TEXT NOT NULL,
    unit INTEGER NOT NULL,
    tg INTEGER NOT NULL,
    kind TEXT NOT NULL,
    first_ms INTEGER NOT NULL,
    last_ms INTEGER NOT NULL,
    count INTEGER NOT NULL,
    PRIMARY KEY(site, unit, tg, kind)
);
PRAGMA user_version = 1;
";

/// One finished call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallRow {
    pub site: String,
    pub call_id: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub tg: u16,
    /// Primary source (grant owner, else the voice's).
    pub source: Option<u32>,
    /// Every radio heard in the call, the primary first.
    pub sources: Vec<u32>,
    pub freq_hz: Option<u64>,
    pub chain: u8,
    pub encrypted: bool,
    pub followed: bool,
    pub not_followed: Option<String>,
    /// Decoded voice (IMBE frames x 20 ms).
    pub voice_ms: u64,
    /// Grant to the last grant update (control channel).
    pub grant_ms: u64,
    pub imbe: u64,
    pub vocoder_errors: u64,
    pub close_reason: String,
}

impl CallRow {
    /// The call's grant time when no voice was decoded, else 0.
    pub fn grant_only_ms(&self) -> u64 {
        if self.voice_ms > 0 { 0 } else { self.grant_ms }
    }
}

/// Radio events that tie a radio to a talkgroup or the system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitEventKind {
    GroupAffiliation,
    Registration,
    Deregistration,
}

impl UnitEventKind {
    fn as_str(self) -> &'static str {
        match self {
            UnitEventKind::GroupAffiliation => "group_affiliation",
            UnitEventKind::Registration => "registration",
            UnitEventKind::Deregistration => "deregistration",
        }
    }
}

/// A radio event seen `count` times from `first_ms` to `last_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitNote {
    pub unit: u32,
    pub tg: u16,
    pub kind: UnitEventKind,
    pub first_ms: u64,
    pub last_ms: u64,
    pub count: u64,
}

/// A query window on one site.
#[derive(Debug, Clone)]
pub struct Range {
    pub site: String,
    pub from_ms: u64,
    pub to_ms: u64,
}

impl Range {
    /// Where totals start: the hour `from_ms` is in.
    pub fn first_hour(&self) -> u64 {
        self.from_ms / HOUR_MS * HOUR_MS
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Summary {
    pub calls: u64,
    pub followed: u64,
    pub encrypted: u64,
    /// Decoded voice of the followed calls.
    pub voice_s: f64,
    /// Grant time of clear calls with no decoded voice.
    pub clear_grant_s: f64,
    /// Grant time of encrypted calls.
    pub encrypted_grant_s: f64,
    /// Decoded voice per grant second, over the calls with voice.
    pub voice_per_grant: Option<f64>,
    pub talkgroups: u64,
    pub radios: u64,
    pub first_ms: Option<u64>,
    pub last_ms: Option<u64>,
}

/// Per talkgroup, radio, or radio on a talkgroup: `voice_s` decoded,
/// `grant_s` the grant time of its calls with no decoded voice.
#[derive(Debug, Clone, Serialize)]
pub struct TgStat {
    pub tg: u16,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub grant_s: f64,
    pub radios: u64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadioStat {
    pub unit: u32,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub grant_s: f64,
    pub talkgroups: u64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadioTg {
    pub tg: u16,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub grant_s: f64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitEvent {
    pub tg: u16,
    pub kind: String,
    pub first_ms: u64,
    pub last_ms: u64,
    pub count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RadioDetail {
    pub unit: u32,
    pub talkgroups: Vec<RadioTg>,
    pub events: Vec<UnitEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TgRadio {
    pub unit: u32,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub grant_s: f64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TgDetail {
    pub tg: u16,
    pub radios: Vec<TgRadio>,
    pub calls: u64,
    pub encrypted: u64,
    pub first_encrypted_ms: Option<u64>,
    pub last_encrypted_ms: Option<u64>,
    pub last_clear_ms: Option<u64>,
    pub affiliated_radios: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bucket {
    /// Bucket start, unix ms.
    pub t: u64,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub clear_grant_s: f64,
    pub encrypted_grant_s: f64,
}

impl Bucket {
    fn empty(t: u64) -> Self {
        Bucket { t, calls: 0, encrypted: 0, voice_s: 0.0, clear_grant_s: 0.0, encrypted_grant_s: 0.0 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SiteStat {
    pub site: String,
    pub calls: u64,
    pub first_ms: u64,
    pub last_ms: u64,
}

/// What a series or a listing is about.
#[derive(Debug, Clone, Copy, Default)]
pub struct SeriesFilter {
    pub tg: Option<u16>,
    pub unit: Option<u32>,
}

pub struct HistoryStore {
    conn: Mutex<Connection>,
    pub path: PathBuf,
    pub on_sd: bool,
}

fn to_u64(v: i64) -> u64 {
    v.max(0) as u64
}

fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Site and hour window of the per-hour tables: ?1 site, ?2 first
/// hour, ?3 end.
const HOURS: &str = "site = ?1 AND t >= ?2 AND t < ?3";

fn hours(q: &Range) -> (String, i64, i64) {
    (q.site.clone(), q.first_hour() as i64, q.to_ms as i64)
}

impl HistoryStore {
    pub fn open(path: &Path, on_sd: bool) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn, path.to_path_buf(), on_sd)
    }

    /// An in-memory store (tests).
    pub fn open_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?, PathBuf::from(":memory:"), false)
    }

    fn init(conn: Connection, path: PathBuf, on_sd: bool) -> rusqlite::Result<Self> {
        // WAL: readers do not block the writer; NORMAL sync: a power cut
        // loses at most the last transactions, never the database.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(HistoryStore { conn: Mutex::new(conn), path, on_sd })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Store calls (ignoring ones already stored) and add them to the
    /// hourly totals. Returns how many were new. The call's time goes to
    /// its primary radio; the others are counted as taking part.
    pub fn insert_calls(&self, rows: &[CallRow]) -> rusqlite::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let mut added = 0;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO calls (site, call_id, started_ms, ended_ms, tg, source, freq_hz, chain,
                 encrypted, followed, not_followed, voice_ms, grant_ms, imbe, vocoder_errors, close_reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            )?;
            let mut unit = tx.prepare_cached(
                "INSERT INTO call_units (call, site, unit, started_ms, primary_src) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            let mut tg_hour = tx.prepare_cached(
                "INSERT INTO hour_tg (site, t, tg, calls, followed, encrypted, voice_ms, clear_grant_ms, enc_grant_ms,
                 voiced_grant_ms, first_ms, last_ms) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 ON CONFLICT(site, t, tg) DO UPDATE SET calls = calls + 1, followed = followed + excluded.followed,
                 encrypted = encrypted + excluded.encrypted, voice_ms = voice_ms + excluded.voice_ms,
                 clear_grant_ms = clear_grant_ms + excluded.clear_grant_ms,
                 enc_grant_ms = enc_grant_ms + excluded.enc_grant_ms,
                 voiced_grant_ms = voiced_grant_ms + excluded.voiced_grant_ms,
                 first_ms = MIN(first_ms, excluded.first_ms), last_ms = MAX(last_ms, excluded.last_ms)",
            )?;
            let mut unit_hour = tx.prepare_cached(
                "INSERT INTO hour_unit (site, t, unit, tg, calls, encrypted, voice_ms, clear_grant_ms, enc_grant_ms, last_ms)
                 VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(site, t, unit, tg) DO UPDATE SET calls = calls + 1,
                 encrypted = encrypted + excluded.encrypted, voice_ms = voice_ms + excluded.voice_ms,
                 clear_grant_ms = clear_grant_ms + excluded.clear_grant_ms,
                 enc_grant_ms = enc_grant_ms + excluded.enc_grant_ms, last_ms = MAX(last_ms, excluded.last_ms)",
            )?;
            let mut site = tx.prepare_cached(
                "INSERT INTO site_stats (site, calls, first_ms, last_ms) VALUES (?1, 1, ?2, ?2)
                 ON CONFLICT(site) DO UPDATE SET calls = calls + 1, first_ms = MIN(first_ms, excluded.first_ms),
                 last_ms = MAX(last_ms, excluded.last_ms)",
            )?;
            for r in rows {
                let n = ins.execute(params![
                    r.site, r.call_id as i64, r.started_ms as i64, r.ended_ms as i64, r.tg, r.source,
                    r.freq_hz.map(|f| f as i64), r.chain, r.encrypted, r.followed, r.not_followed,
                    r.voice_ms as i64, r.grant_ms as i64, r.imbe as i64, r.vocoder_errors as i64, r.close_reason,
                ])?;
                if n == 0 {
                    continue;
                }
                added += 1;
                let id = tx.last_insert_rowid();
                let t = (r.started_ms / HOUR_MS * HOUR_MS) as i64;
                let grant = r.grant_only_ms() as i64;
                let (clear_grant, enc_grant) = if r.encrypted { (0, grant) } else { (grant, 0) };
                let voiced_grant = if r.voice_ms > 0 { r.grant_ms as i64 } else { 0 };
                tg_hour.execute(params![
                    r.site, t, r.tg, r.followed, r.encrypted, r.voice_ms as i64, clear_grant, enc_grant, voiced_grant,
                    r.started_ms as i64,
                ])?;
                site.execute(params![r.site, r.started_ms as i64])?;
                let mut seen: Vec<u32> = Vec::new();
                for u in r.source.iter().chain(r.sources.iter()) {
                    if *u == 0 || seen.contains(u) {
                        continue;
                    }
                    let primary = seen.is_empty();
                    seen.push(*u);
                    unit.execute(params![id, r.site, u, r.started_ms as i64, primary])?;
                    let (voice, cg, eg) = if primary { (r.voice_ms as i64, clear_grant, enc_grant) } else { (0, 0, 0) };
                    unit_hour.execute(params![r.site, t, u, r.tg, r.encrypted, voice, cg, eg, r.started_ms as i64])?;
                }
            }
        }
        tx.commit()?;
        Ok(added)
    }

    pub fn note_unit(&self, site: &str, unit: u32, tg: u16, kind: UnitEventKind, at_ms: u64) -> rusqlite::Result<()> {
        self.note_units(site, &[UnitNote { unit, tg, kind, first_ms: at_ms, last_ms: at_ms, count: 1 }])
    }

    /// Add radio events, in one transaction.
    pub fn note_units(&self, site: &str, notes: &[UnitNote]) -> rusqlite::Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO unit_events (site, unit, tg, kind, first_ms, last_ms, count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(site, unit, tg, kind) DO UPDATE SET first_ms = MIN(first_ms, excluded.first_ms),
                 last_ms = MAX(last_ms, excluded.last_ms), count = count + excluded.count",
            )?;
            for n in notes {
                st.execute(params![site, n.unit, n.tg, n.kind.as_str(), n.first_ms as i64, n.last_ms as i64, n.count as i64])?;
            }
        }
        tx.commit()
    }

    /// Bytes in use (pages not on the free list).
    pub fn used_bytes(&self) -> rusqlite::Result<u64> {
        let conn = self.conn();
        let pragma = |p: &str| conn.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0));
        Ok(to_u64((pragma("page_count")? - pragma("freelist_count")?) * pragma("page_size")?))
    }

    /// Delete the oldest calls (a tenth at a time) until at most
    /// `max_bytes` are in use. Returns how many calls went.
    pub fn trim_to(&self, max_bytes: u64) -> rusqlite::Result<usize> {
        let mut gone = 0;
        for _ in 0..10 {
            if self.used_bytes()? <= max_bytes {
                break;
            }
            let cut: Option<i64> = self
                .conn()
                .query_row(
                    "SELECT started_ms FROM calls ORDER BY started_ms LIMIT 1 OFFSET (SELECT COUNT(*) / 10 FROM calls)",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(cut) = cut else { break };
            let n = self.prune(to_u64(cut) + 1)?;
            if n == 0 {
                break;
            }
            gone += n;
        }
        Ok(gone)
    }

    /// Delete calls started before `before_ms`, and the hours that ended
    /// by then. Returns how many calls.
    pub fn prune(&self, before_ms: u64) -> rusqlite::Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let before = before_ms as i64;
        let n = tx.execute("DELETE FROM calls WHERE started_ms < ?1", params![before])?;
        tx.execute("DELETE FROM hour_tg WHERE t + ?2 <= ?1", params![before, HOUR_MS as i64])?;
        tx.execute("DELETE FROM hour_unit WHERE t + ?2 <= ?1", params![before, HOUR_MS as i64])?;
        tx.execute("DELETE FROM unit_events WHERE last_ms < ?1", params![before])?;
        if n > 0 {
            tx.execute_batch(
                "UPDATE site_stats SET
                   calls = (SELECT COUNT(*) FROM calls c WHERE c.site = site_stats.site),
                   first_ms = COALESCE((SELECT MIN(started_ms) FROM calls c WHERE c.site = site_stats.site), first_ms);
                 DELETE FROM site_stats WHERE calls = 0;",
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn sites(&self) -> rusqlite::Result<Vec<SiteStat>> {
        let conn = self.conn();
        let mut st = conn.prepare("SELECT site, calls, first_ms, last_ms FROM site_stats ORDER BY last_ms DESC")?;
        let rows = st.query_map([], |r| {
            Ok(SiteStat { site: r.get(0)?, calls: to_u64(r.get(1)?), first_ms: to_u64(r.get(2)?), last_ms: to_u64(r.get(3)?) })
        })?;
        rows.collect()
    }

    pub fn summary(&self, q: &Range) -> rusqlite::Result<Summary> {
        let conn = self.conn();
        let w = hours(q);
        let (mut s, voiced_grant) = conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(calls), 0), COALESCE(SUM(followed), 0), COALESCE(SUM(encrypted), 0),
                 COALESCE(SUM(voice_ms), 0), COALESCE(SUM(clear_grant_ms), 0), COALESCE(SUM(enc_grant_ms), 0),
                 COALESCE(SUM(voiced_grant_ms), 0), COUNT(DISTINCT tg), MIN(first_ms), MAX(last_ms)
                 FROM hour_tg WHERE {HOURS}"
            ),
            params![w.0, w.1, w.2],
            |r| {
                Ok((
                    Summary {
                        calls: to_u64(r.get(0)?),
                        followed: to_u64(r.get(1)?),
                        encrypted: to_u64(r.get(2)?),
                        voice_s: secs(r.get(3)?),
                        clear_grant_s: secs(r.get(4)?),
                        encrypted_grant_s: secs(r.get(5)?),
                        voice_per_grant: None,
                        talkgroups: to_u64(r.get(7)?),
                        radios: 0,
                        first_ms: r.get::<_, Option<i64>>(8)?.map(to_u64),
                        last_ms: r.get::<_, Option<i64>>(9)?.map(to_u64),
                    },
                    r.get::<_, i64>(6)?,
                ))
            },
        )?;
        if voiced_grant > 0 {
            s.voice_per_grant = Some(s.voice_s * 1000.0 / voiced_grant as f64);
        }
        s.radios = to_u64(conn.query_row(
            &format!("SELECT COUNT(DISTINCT unit) FROM hour_unit WHERE {HOURS}"),
            params![w.0, w.1, w.2],
            |r| r.get(0),
        )?);
        Ok(s)
    }

    /// Talkgroups by time (voice + grant-only), then calls.
    pub fn talkgroups(&self, q: &Range, limit: usize) -> rusqlite::Result<Vec<TgStat>> {
        let conn = self.conn();
        let w = hours(q);
        let mut st = conn.prepare(&format!("SELECT tg, COUNT(DISTINCT unit) FROM hour_unit WHERE {HOURS} GROUP BY tg"))?;
        let radios: HashMap<u16, u64> = st
            .query_map(params![w.0, w.1, w.2], |r| Ok((r.get(0)?, to_u64(r.get(1)?))))?
            .collect::<rusqlite::Result<_>>()?;
        let mut st = conn.prepare(&format!(
            "SELECT tg, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM hour_tg WHERE {HOURS} GROUP BY tg
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC LIMIT ?4"
        ))?;
        let rows = st.query_map(params![w.0, w.1, w.2, limit as i64], |r| {
            let tg: u16 = r.get(0)?;
            Ok(TgStat {
                tg,
                calls: to_u64(r.get(1)?),
                encrypted: to_u64(r.get(2)?),
                voice_s: secs(r.get(3)?),
                grant_s: secs(r.get(4)?),
                radios: radios.get(&tg).copied().unwrap_or(0),
                last_ms: to_u64(r.get(5)?),
            })
        })?;
        rows.collect()
    }

    /// Radios by time (calls they took part in; time as the primary).
    pub fn radios(&self, q: &Range, limit: usize) -> rusqlite::Result<Vec<RadioStat>> {
        let conn = self.conn();
        let w = hours(q);
        let mut st = conn.prepare(&format!(
            "SELECT unit, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms),
             COUNT(DISTINCT tg), MAX(last_ms)
             FROM hour_unit WHERE {HOURS} GROUP BY unit
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC LIMIT ?4"
        ))?;
        let rows = st.query_map(params![w.0, w.1, w.2, limit as i64], |r| {
            Ok(RadioStat {
                unit: r.get(0)?,
                calls: to_u64(r.get(1)?),
                encrypted: to_u64(r.get(2)?),
                voice_s: secs(r.get(3)?),
                grant_s: secs(r.get(4)?),
                talkgroups: to_u64(r.get(5)?),
                last_ms: to_u64(r.get(6)?),
            })
        })?;
        rows.collect()
    }

    /// The talkgroups one radio used, and its affiliations.
    pub fn radio(&self, q: &Range, unit: u32) -> rusqlite::Result<RadioDetail> {
        let conn = self.conn();
        let w = hours(q);
        let mut st = conn.prepare(
            "SELECT tg, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM hour_unit WHERE site = ?1 AND unit = ?4 AND t >= ?2 AND t < ?3 GROUP BY tg
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC",
        )?;
        let talkgroups = st
            .query_map(params![w.0, w.1, w.2, unit], |r| {
                Ok(RadioTg {
                    tg: r.get(0)?,
                    calls: to_u64(r.get(1)?),
                    encrypted: to_u64(r.get(2)?),
                    voice_s: secs(r.get(3)?),
                    grant_s: secs(r.get(4)?),
                    last_ms: to_u64(r.get(5)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut st = conn.prepare(
            "SELECT tg, kind, first_ms, last_ms, count FROM unit_events WHERE site = ?1 AND unit = ?2
             ORDER BY last_ms DESC",
        )?;
        let events = st
            .query_map(params![q.site, unit], |r| {
                Ok(UnitEvent {
                    tg: r.get(0)?,
                    kind: r.get(1)?,
                    first_ms: to_u64(r.get(2)?),
                    last_ms: to_u64(r.get(3)?),
                    count: to_u64(r.get(4)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(RadioDetail { unit, talkgroups, events })
    }

    /// The radios of one talkgroup and its encryption history.
    pub fn talkgroup(&self, q: &Range, tg: u16) -> rusqlite::Result<TgDetail> {
        let conn = self.conn();
        let w = hours(q);
        let mut st = conn.prepare(&format!(
            "SELECT unit, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM hour_unit WHERE {HOURS} AND tg = ?4 GROUP BY unit
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC"
        ))?;
        let radios = st
            .query_map(params![w.0, w.1, w.2, tg], |r| {
                Ok(TgRadio {
                    unit: r.get(0)?,
                    calls: to_u64(r.get(1)?),
                    encrypted: to_u64(r.get(2)?),
                    voice_s: secs(r.get(3)?),
                    grant_s: secs(r.get(4)?),
                    last_ms: to_u64(r.get(5)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // One talkgroup's calls: few enough to read directly.
        let (calls, encrypted, first_enc, last_enc, last_clear): (i64, i64, Option<i64>, Option<i64>, Option<i64>) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(encrypted), 0),
             MIN(CASE WHEN encrypted THEN started_ms END), MAX(CASE WHEN encrypted THEN started_ms END),
             MAX(CASE WHEN NOT encrypted THEN started_ms END)
             FROM calls WHERE site = ?1 AND tg = ?4 AND started_ms >= ?2 AND started_ms < ?3",
            params![w.0, w.1, w.2, tg],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let affiliated: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT unit) FROM unit_events WHERE site = ?1 AND tg = ?2 AND kind = 'group_affiliation'",
            params![q.site, tg],
            |r| r.get(0),
        )?;
        Ok(TgDetail {
            tg,
            radios,
            calls: to_u64(calls),
            encrypted: to_u64(encrypted),
            first_encrypted_ms: first_enc.map(to_u64),
            last_encrypted_ms: last_enc.map(to_u64),
            last_clear_ms: last_clear.map(to_u64),
            affiliated_radios: to_u64(affiliated),
        })
    }

    /// Calls and time per bucket (`bucket_ms` = an hour or a day),
    /// buckets aligned to local time `tz_offset_min` east of UTC. Empty
    /// buckets are included. With a radio, its calls and credited time.
    pub fn series(&self, q: &Range, bucket_ms: u64, tz_offset_min: i64, f: SeriesFilter) -> rusqlite::Result<Vec<Bucket>> {
        let bucket_ms = bucket_ms.max(HOUR_MS) as i64;
        let tz = tz_offset_min * 60_000;
        let conn = self.conn();
        let w = hours(q);
        let (table, unit) = match f.unit {
            Some(u) => ("hour_unit", format!(" AND unit = {u}")),
            None => ("hour_tg", String::new()),
        };
        let sql = format!(
            "SELECT ((t + ?4) / ?5) * ?5 - ?4 AS b, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms),
             SUM(enc_grant_ms) FROM {table} WHERE {HOURS} AND (?6 IS NULL OR tg = ?6){unit}
             GROUP BY b ORDER BY b"
        );
        let mut st = conn.prepare(&sql)?;
        let got: Vec<Bucket> = st
            .query_map(params![w.0, w.1, w.2, tz, bucket_ms, f.tg], |r| {
                Ok(Bucket {
                    t: to_u64(r.get(0)?),
                    calls: to_u64(r.get(1)?),
                    encrypted: to_u64(r.get(2)?),
                    voice_s: secs(r.get(3)?),
                    clear_grant_s: secs(r.get(4)?),
                    encrypted_grant_s: secs(r.get(5)?),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // Fill the gaps.
        let first = ((w.1 + tz).div_euclid(bucket_ms)) * bucket_ms - tz;
        let mut out = Vec::new();
        let mut t = first;
        let mut it = got.into_iter().peekable();
        while t < w.2 {
            match it.peek() {
                Some(b) if b.t as i64 == t => out.push(it.next().unwrap()),
                _ => out.push(Bucket::empty(to_u64(t))),
            }
            t += bucket_ms;
            if out.len() > 2000 {
                break;
            }
        }
        Ok(out)
    }

    /// Calls in the window, newest first (for listings and CSV).
    pub fn calls(&self, q: &Range, f: SeriesFilter, limit: usize) -> rusqlite::Result<Vec<CallRow>> {
        let conn = self.conn();
        let mut st = conn.prepare(
            "SELECT c.site, c.call_id, c.started_ms, c.ended_ms, c.tg, c.source, c.freq_hz, c.chain, c.encrypted,
             c.followed, c.not_followed, c.voice_ms, c.grant_ms, c.imbe, c.vocoder_errors, c.close_reason,
             (SELECT GROUP_CONCAT(unit) FROM call_units u WHERE u.call = c.id)
             FROM calls c WHERE c.site = ?1 AND c.started_ms >= ?2 AND c.started_ms < ?3
             AND (?5 IS NULL OR c.tg = ?5)
             AND (?6 IS NULL OR c.id IN (SELECT call FROM call_units WHERE site = ?1 AND unit = ?6 AND started_ms >= ?2 AND started_ms < ?3))
             ORDER BY c.started_ms DESC LIMIT ?4",
        )?;
        let rows = st.query_map(params![q.site, q.from_ms as i64, q.to_ms as i64, limit as i64, f.tg, f.unit], |r| {
            let units: Option<String> = r.get(16)?;
            Ok(CallRow {
                site: r.get(0)?,
                call_id: to_u64(r.get(1)?),
                started_ms: to_u64(r.get(2)?),
                ended_ms: to_u64(r.get(3)?),
                tg: r.get(4)?,
                source: r.get(5)?,
                sources: units
                    .unwrap_or_default()
                    .split(',')
                    .filter_map(|s| s.parse().ok())
                    .collect(),
                freq_hz: r.get::<_, Option<i64>>(6)?.map(to_u64),
                chain: r.get(7)?,
                encrypted: r.get(8)?,
                followed: r.get(9)?,
                not_followed: r.get(10)?,
                voice_ms: to_u64(r.get(11)?),
                grant_ms: to_u64(r.get(12)?),
                imbe: to_u64(r.get(13)?),
                vocoder_errors: to_u64(r.get(14)?),
                close_reason: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
            })
        })?;
        rows.collect()
    }

    /// Newest stored call start on `site`.
    pub fn last_started_ms(&self, site: &str) -> rusqlite::Result<Option<u64>> {
        Ok(self
            .conn()
            .query_row("SELECT last_ms FROM site_stats WHERE site = ?1", params![site], |r| r.get::<_, i64>(0))
            .optional()?
            .map(to_u64))
    }

    /// Size on disk (database + WAL), bytes.
    pub fn size_bytes(&self) -> u64 {
        let f = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        f(&self.path) + f(&self.path.with_extension("sqlite-wal"))
    }
}

/// Calls as CSV (with a header).
pub fn to_csv(rows: &[CallRow]) -> String {
    let mut s = String::from(
        "site,call_id,started_utc,started_ms,ended_ms,tg,source,sources,freq_hz,chain,encrypted,followed,not_followed,voice_ms,grant_ms,imbe,vocoder_errors,close_reason\n",
    );
    for r in rows {
        let started = iso_utc(r.started_ms);
        s.push_str(&format!(
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
            r.site, r.call_id, started, r.started_ms, r.ended_ms, r.tg,
            r.source.map(|v| v.to_string()).unwrap_or_default(),
            r.sources.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "),
            r.freq_hz.map(|v| v.to_string()).unwrap_or_default(),
            r.chain, r.encrypted as u8, r.followed as u8, r.not_followed.clone().unwrap_or_default(),
            r.voice_ms, r.grant_ms, r.imbe, r.vocoder_errors, r.close_reason,
        ));
    }
    s
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) for CSV.
pub fn iso_utc(ms: u64) -> String {
    let s = ms / 1_000;
    let (d, r) = (s / 86_400, s % 86_400);
    let z = d as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}", r / 3_600, r % 3_600 / 60, r % 60)
}

#[cfg(test)]
#[path = "history_tests.rs"]
mod tests;

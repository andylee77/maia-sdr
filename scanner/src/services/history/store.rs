//! The history database: writes (calls with their hourly totals, radio events, sites, recordings,
//! pruning) on one connection, the Activity queries on a second, read-only one, so a long read
//! (a CSV export) never holds up the writer. Every call blocks: callers use `spawn_blocking` or
//! the writer thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::Serialize;

use super::schema::{ADDED, SCHEMA, VERSION};

pub const HOUR_MS: u64 = 3_600_000;
const READ_BUSY: Duration = Duration::from_secs(2);

/// One finished call.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CallRow {
    pub site: String,
    pub call_id: u64,
    pub started_ms: u64,
    pub ended_ms: u64,
    pub tg: u32,
    /// The primary radio: the grant's, else the voice's.
    pub source: Option<u32>,
    /// Every radio heard in the call, the primary first.
    pub sources: Vec<u32>,
    pub freq_hz: Option<u64>,
    /// The channel as announced (P25 `iden-number`, DMR LCN).
    pub channel: Option<String>,
    pub timeslot: Option<u8>,
    /// The lane that followed it (0 = none).
    pub lane: u8,
    pub encrypted: bool,
    pub emergency: bool,
    /// A unit-to-unit call (`tg` is the called radio).
    pub private: bool,
    pub first_voice_ms: Option<u64>,
    pub followed: bool,
    pub not_followed: Option<String>,
    /// Decoded voice (frames x 20 ms).
    pub voice_ms: u64,
    /// The grant to its last update on the control channel.
    pub grant_ms: u64,
    /// `imbe` or `ambe2`.
    pub codec: Option<String>,
    pub frames: u64,
    pub frame_errors: u64,
    pub close_reason: String,
    pub end_kind: Option<String>,
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
    pub fn as_str(self) -> &'static str {
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
    pub tg: u32,
    pub kind: UnitEventKind,
    pub first_ms: u64,
    pub last_ms: u64,
    pub count: u64,
}

/// A site as the history knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteInfo {
    pub id: String,
    pub system: String,
    pub protocol: String,
    pub label: String,
    pub system_label: String,
}

/// The decoded voice of a followed call, and its recording, known after the call closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceResult {
    pub site: String,
    pub call_id: u64,
    pub started_ms: u64,
    pub frames: u64,
    pub frame_errors: u64,
}

/// A recording file, for the `recordings` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingRow {
    pub file: String,
    pub store: String,
    pub site: String,
    pub call_id: u64,
    pub started_ms: u64,
    pub tg: u32,
    pub source: Option<u32>,
    pub bytes: u64,
    pub duration_ms: u64,
}

/// What a recording's name does not carry, from its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingInfo {
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub lane: u8,
    pub units: Vec<u32>,
    pub frames: u64,
    pub frame_errors: u64,
}

/// A query window on one site, or on every site of a system (`by_system`: `site` is the system).
#[derive(Debug, Clone)]
pub struct Range {
    pub site: String,
    pub by_system: bool,
    pub from_ms: u64,
    pub to_ms: u64,
}

impl Range {
    pub fn site(site: &str, from_ms: u64, to_ms: u64) -> Range {
        Range { site: site.to_string(), by_system: false, from_ms, to_ms }
    }

    /// Where totals start: the hour `from_ms` is in.
    pub fn first_hour(&self) -> u64 {
        self.from_ms / HOUR_MS * HOUR_MS
    }

    /// `column` in the range's sites (`?1` is the site or the system).
    fn scope(&self, column: &str) -> String {
        if self.by_system {
            format!("{column} IN (SELECT id FROM sites WHERE system = ?1)")
        } else {
            format!("{column} = ?1")
        }
    }

    /// The range's sites and hours in the per-hour tables: ?1 site or system, ?2 first hour, ?3 end.
    fn hours(&self) -> String {
        format!("{} AND t >= ?2 AND t < ?3", self.scope("site"))
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

/// Per talkgroup, radio, or radio on a talkgroup: `voice_s` decoded, `grant_s` the grant time
/// of its calls with no decoded voice.
#[derive(Debug, Clone, Serialize)]
pub struct TgStat {
    pub tg: u32,
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
    pub tg: u32,
    pub calls: u64,
    pub encrypted: u64,
    pub voice_s: f64,
    pub grant_s: f64,
    pub last_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UnitEvent {
    pub tg: u32,
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
    pub tg: u32,
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
    pub tg: Option<u32>,
    pub unit: Option<u32>,
}

pub struct Store {
    write: Mutex<Connection>,
    read: Mutex<Connection>,
    pub path: PathBuf,
}

fn to_u64(v: i64) -> u64 {
    v.max(0) as u64
}

fn secs(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

/// Bytes in use (pages not on the free list).
fn used_bytes(conn: &Connection) -> rusqlite::Result<u64> {
    let pragma = |p: &str| conn.query_row(&format!("PRAGMA {p}"), [], |r| r.get::<_, i64>(0));
    Ok(to_u64((pragma("page_count")? - pragma("freelist_count")?) * pragma("page_size")?))
}

fn lock(m: &Mutex<Connection>) -> std::sync::MutexGuard<'_, Connection> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn units_of(list: Option<String>) -> Vec<u32> {
    list.unwrap_or_default().split(',').filter_map(|s| s.parse().ok()).collect()
}

/// The columns `call_row` reads (table alias `c`).
const CALL_COLUMNS: &str = "c.site, c.call_id, c.started_ms, c.ended_ms, c.tg, c.source, c.freq_hz, c.channel,
     c.timeslot, c.lane, c.encrypted, c.followed, c.not_followed, c.voice_ms, c.grant_ms, c.codec, c.frames,
     c.frame_errors, c.close_reason, c.end_kind,
     (SELECT GROUP_CONCAT(unit) FROM (SELECT unit FROM transmissions t WHERE t.call = c.id ORDER BY t.rowid)),
     c.emergency, c.target = 'unit', c.first_voice_ms";

fn call_row(r: &rusqlite::Row) -> rusqlite::Result<CallRow> {
    Ok(CallRow {
        site: r.get(0)?,
        call_id: to_u64(r.get(1)?),
        started_ms: to_u64(r.get(2)?),
        ended_ms: to_u64(r.get(3)?),
        tg: r.get(4)?,
        source: r.get(5)?,
        freq_hz: r.get::<_, Option<i64>>(6)?.map(to_u64),
        channel: r.get(7)?,
        timeslot: r.get(8)?,
        lane: r.get(9)?,
        encrypted: r.get(10)?,
        followed: r.get(11)?,
        not_followed: r.get(12)?,
        voice_ms: to_u64(r.get(13)?),
        grant_ms: to_u64(r.get(14)?),
        codec: r.get(15)?,
        frames: to_u64(r.get(16)?),
        frame_errors: to_u64(r.get(17)?),
        close_reason: r.get::<_, Option<String>>(18)?.unwrap_or_default(),
        end_kind: r.get(19)?,
        sources: units_of(r.get(20)?),
        emergency: r.get(21)?,
        private: r.get(22)?,
        first_voice_ms: r.get::<_, Option<i64>>(23)?.map(to_u64),
    })
}

fn hours(q: &Range) -> (String, i64, i64) {
    (q.site.clone(), q.first_hour() as i64, q.to_ms as i64)
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let write = Connection::open(path)?;
        // WAL: readers do not block the writer; NORMAL sync: a power cut loses at most the
        // last transactions, never the database.
        let _: String = write.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        write.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        write.execute_batch(SCHEMA)?;
        for (table, column, kind) in ADDED {
            let has: i64 = write.query_row(
                &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
                params![column],
                |r| r.get(0),
            )?;
            if has == 0 {
                write.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind}"))?;
            }
        }
        write.execute(
            "INSERT INTO meta (key, value) VALUES ('schema', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![VERSION.to_string()],
        )?;
        let read = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        read.busy_timeout(READ_BUSY)?;
        Ok(Store { write: Mutex::new(write), read: Mutex::new(read), path: path.to_path_buf() })
    }

    /// Run `f` on the write connection.
    #[cfg(test)]
    pub fn with_writer<T>(&self, f: impl FnOnce(&mut Connection) -> rusqlite::Result<T>) -> rusqlite::Result<T> {
        f(&mut lock(&self.write))
    }

    /// Keep a site and its system.
    pub fn note_site(&self, s: &SiteInfo) -> rusqlite::Result<()> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO systems (id, protocol, label) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET protocol = excluded.protocol, label = excluded.label",
            params![s.system, s.protocol, s.system_label],
        )?;
        tx.execute(
            "INSERT INTO sites (id, system, label) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET system = excluded.system, label = excluded.label",
            params![s.id, s.system, s.label],
        )?;
        tx.commit()
    }

    /// Store calls (ignoring ones already stored) with their hourly totals, the sites' counts and
    /// the systems' talkgroups and radios. Returns how many were new. The call's time goes to its
    /// primary radio; the others are counted as taking part.
    pub fn insert_calls(&self, rows: &[CallRow]) -> rusqlite::Result<usize> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        let added = insert_calls(&tx, rows)?;
        tx.commit()?;
        Ok(added)
    }

    /// The vocoder's counts of a followed call, known after it closed.
    pub fn voice_results(&self, results: &[VoiceResult]) -> rusqlite::Result<()> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "UPDATE calls SET frames = ?4, frame_errors = ?5 WHERE site = ?1 AND call_id = ?2 AND started_ms = ?3",
            )?;
            for v in results {
                st.execute(params![v.site, v.call_id as i64, v.started_ms as i64, v.frames as i64, v.frame_errors as i64])?;
            }
        }
        tx.commit()
    }

    /// Add radio events, in one transaction.
    pub fn note_units(&self, site: &str, notes: &[UnitNote]) -> rusqlite::Result<()> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        {
            let mut st = tx.prepare_cached(
                "INSERT INTO radio_events (site, unit, tg, kind, first_ms, last_ms, count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(site, unit, tg, kind) DO UPDATE SET first_ms = MIN(first_ms, excluded.first_ms),
                 last_ms = MAX(last_ms, excluded.last_ms), count = count + excluded.count",
            )?;
            for n in notes {
                st.execute(params![site, n.unit, n.tg, n.kind.as_str(), n.first_ms as i64, n.last_ms as i64, n.count as i64])?;
            }
        }
        tx.commit()
    }

    /// Add recordings (a file already listed is left as it is), each linked to its call: the
    /// call of its id on its site that started nearest, within 10 s (the old recorder stamped
    /// its own start).
    pub fn add_recordings(&self, rows: &[RecordingRow]) -> rusqlite::Result<usize> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        let mut added = 0;
        {
            let mut ins = tx.prepare_cached(
                "INSERT OR IGNORE INTO recordings (file, store, site, call_id, started_ms, tg, source, bytes, duration_ms, call)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                   (SELECT id FROM calls WHERE call_id = ?4 AND (site = ?3 OR ?3 = '') AND ABS(started_ms - ?5) < 10000
                    ORDER BY ABS(started_ms - ?5) LIMIT 1))",
            )?;
            for r in rows {
                added += ins.execute(params![
                    r.file, r.store, r.site, r.call_id as i64, r.started_ms as i64, r.tg, r.source, r.bytes as i64,
                    r.duration_ms as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(added)
    }

    /// Forget recordings whose files are gone.
    pub fn remove_recordings(&self, files: &[String]) -> rusqlite::Result<usize> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        let mut n = 0;
        {
            let mut st = tx.prepare_cached("DELETE FROM recordings WHERE file = ?1")?;
            for f in files {
                n += st.execute(params![f])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// Every recording file listed.
    pub fn recording_files(&self) -> rusqlite::Result<Vec<String>> {
        let conn = lock(&self.read);
        let mut st = conn.prepare("SELECT file FROM recordings")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    /// The call details of the recordings of `files` (by file name).
    pub fn recording_info(&self, files: &[String]) -> rusqlite::Result<HashMap<String, RecordingInfo>> {
        let conn = lock(&self.read);
        let mut st = conn.prepare_cached(
            "SELECT c.freq_hz, c.channel, c.lane, c.frames, c.frame_errors,
             (SELECT GROUP_CONCAT(unit) FROM (SELECT unit FROM transmissions t WHERE t.call = c.id ORDER BY t.rowid))
             FROM recordings r JOIN calls c ON c.id = r.call WHERE r.file = ?1",
        )?;
        let mut out = HashMap::new();
        for f in files {
            let info = st
                .query_row(params![f], |r| {
                    Ok(RecordingInfo {
                        freq_hz: r.get::<_, Option<i64>>(0)?.map(to_u64),
                        channel: r.get(1)?,
                        lane: r.get(2)?,
                        frames: to_u64(r.get(3)?),
                        frame_errors: to_u64(r.get(4)?),
                        units: units_of(r.get(5)?),
                    })
                })
                .optional()?;
            if let Some(i) = info {
                out.insert(f.clone(), i);
            }
        }
        Ok(out)
    }

    /// Bytes in use (pages not on the free list).
    pub fn used_bytes(&self) -> rusqlite::Result<u64> {
        used_bytes(&lock(&self.read))
    }

    /// Delete the oldest hours (a tenth of the calls at a time) until at most `max_bytes` are in
    /// use. Returns how many calls went.
    pub fn trim_to(&self, max_bytes: u64) -> rusqlite::Result<usize> {
        let mut gone = 0;
        for _ in 0..10 {
            if used_bytes(&lock(&self.write))? <= max_bytes {
                break;
            }
            let cut: Option<i64> = lock(&self.write)
                .query_row(
                    "SELECT started_ms FROM calls ORDER BY started_ms LIMIT 1 OFFSET (SELECT COUNT(*) / 10 FROM calls)",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let Some(cut) = cut else { break };
            // Past the hour the cut is in, so at least that hour goes.
            let n = self.prune(to_u64(cut) / HOUR_MS * HOUR_MS + HOUR_MS)?;
            if n == 0 {
                break;
            }
            gone += n;
        }
        Ok(gone)
    }

    /// Delete every call, radio, talkgroup, site and recording row (a factory reset); the schema
    /// and the file's own notes stay. Returns the calls deleted.
    pub fn clear(&self) -> rusqlite::Result<usize> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        let calls = tx.execute("DELETE FROM calls", [])?;
        for table in ["transmissions", "talkgroups", "radios", "radio_events", "tg_hour", "radio_hour", "recordings", "sites", "systems"] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        Ok(calls)
    }

    /// Delete the hours that started before the hour `before_ms` is in, with their calls and
    /// totals, so the totals always match the calls. Returns how many calls.
    pub fn prune(&self, before_ms: u64) -> rusqlite::Result<usize> {
        let mut conn = lock(&self.write);
        let tx = conn.transaction()?;
        let before = (before_ms / HOUR_MS * HOUR_MS) as i64;
        let n = tx.execute("DELETE FROM calls WHERE started_ms < ?1", params![before])?;
        tx.execute("DELETE FROM tg_hour WHERE t < ?1", params![before])?;
        tx.execute("DELETE FROM radio_hour WHERE t < ?1", params![before])?;
        tx.execute("DELETE FROM radio_events WHERE last_ms < ?1", params![before])?;
        if n > 0 {
            tx.execute_batch(
                "UPDATE sites SET
                   calls = (SELECT COUNT(*) FROM calls c WHERE c.site = sites.id),
                   first_ms = (SELECT MIN(started_ms) FROM calls c WHERE c.site = sites.id),
                   last_ms = (SELECT MAX(started_ms) FROM calls c WHERE c.site = sites.id);",
            )?;
        }
        tx.commit()?;
        Ok(n)
    }

    /// Size on disk (database and WAL), bytes.
    pub fn size_bytes(&self) -> u64 {
        let f = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        f(&self.path) + f(Path::new(&format!("{}-wal", self.path.display())))
    }

    // ── Queries (the read connection) ─────────────────────────────────

    /// Sites with calls, the newest first.
    pub fn sites(&self) -> rusqlite::Result<Vec<SiteStat>> {
        let conn = lock(&self.read);
        let mut st = conn.prepare("SELECT id, calls, first_ms, last_ms FROM sites WHERE calls > 0 ORDER BY last_ms DESC")?;
        let rows = st.query_map([], |r| {
            Ok(SiteStat { site: r.get(0)?, calls: to_u64(r.get(1)?), first_ms: to_u64(r.get(2)?), last_ms: to_u64(r.get(3)?) })
        })?;
        rows.collect()
    }

    pub fn summary(&self, q: &Range) -> rusqlite::Result<Summary> {
        let conn = lock(&self.read);
        let w = hours(q);
        let (mut s, voiced_grant) = conn.query_row(
            &format!(
                "SELECT COALESCE(SUM(calls), 0), COALESCE(SUM(followed), 0), COALESCE(SUM(encrypted), 0),
                 COALESCE(SUM(voice_ms), 0), COALESCE(SUM(clear_grant_ms), 0), COALESCE(SUM(enc_grant_ms), 0),
                 COALESCE(SUM(voiced_grant_ms), 0), COUNT(DISTINCT tg), MIN(first_ms), MAX(last_ms)
                 FROM tg_hour WHERE {}",
                q.hours()
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
            &format!("SELECT COUNT(DISTINCT unit) FROM radio_hour WHERE {}", q.hours()),
            params![w.0, w.1, w.2],
            |r| r.get(0),
        )?);
        Ok(s)
    }

    /// Talkgroups by time (voice and grant-only), then calls.
    pub fn talkgroups(&self, q: &Range, limit: usize) -> rusqlite::Result<Vec<TgStat>> {
        let conn = lock(&self.read);
        let w = hours(q);
        let mut st = conn.prepare(&format!("SELECT tg, COUNT(DISTINCT unit) FROM radio_hour WHERE {} GROUP BY tg", q.hours()))?;
        let radios: HashMap<u32, u64> = st
            .query_map(params![w.0, w.1, w.2], |r| Ok((r.get(0)?, to_u64(r.get(1)?))))?
            .collect::<rusqlite::Result<_>>()?;
        let mut st = conn.prepare(&format!(
            "SELECT tg, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM tg_hour WHERE {} GROUP BY tg
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC LIMIT ?4",
            q.hours()
        ))?;
        let rows = st.query_map(params![w.0, w.1, w.2, limit as i64], |r| {
            let tg: u32 = r.get(0)?;
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

    /// Radios by time (the calls they took part in; time as the primary).
    pub fn radios(&self, q: &Range, limit: usize) -> rusqlite::Result<Vec<RadioStat>> {
        let conn = lock(&self.read);
        let w = hours(q);
        let mut st = conn.prepare(&format!(
            "SELECT unit, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms),
             COUNT(DISTINCT tg), MAX(last_ms)
             FROM radio_hour WHERE {} GROUP BY unit
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC LIMIT ?4",
            q.hours()
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
        let conn = lock(&self.read);
        let w = hours(q);
        let mut st = conn.prepare(&format!(
            "SELECT tg, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM radio_hour WHERE {} AND unit = ?4 GROUP BY tg
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC",
            q.hours()
        ))?;
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
        let mut st = conn.prepare(&format!(
            "SELECT tg, kind, MIN(first_ms), MAX(last_ms), SUM(count) FROM radio_events WHERE {} AND unit = ?2
             GROUP BY tg, kind ORDER BY MAX(last_ms) DESC",
            q.scope("site")
        ))?;
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
    pub fn talkgroup(&self, q: &Range, tg: u32) -> rusqlite::Result<TgDetail> {
        let conn = lock(&self.read);
        let w = hours(q);
        let mut st = conn.prepare(&format!(
            "SELECT unit, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms + enc_grant_ms), MAX(last_ms)
             FROM radio_hour WHERE {} AND tg = ?4 GROUP BY unit
             ORDER BY SUM(voice_ms + clear_grant_ms + enc_grant_ms) DESC, SUM(calls) DESC",
            q.hours()
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
            &format!(
                "SELECT COUNT(*), COALESCE(SUM(encrypted), 0),
                 MIN(CASE WHEN encrypted THEN started_ms END), MAX(CASE WHEN encrypted THEN started_ms END),
                 MAX(CASE WHEN NOT encrypted THEN started_ms END)
                 FROM calls WHERE {} AND tg = ?4 AND started_ms >= ?2 AND started_ms < ?3",
                q.scope("site")
            ),
            params![w.0, w.1, w.2, tg],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let affiliated: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(DISTINCT unit) FROM radio_events WHERE {} AND tg = ?2 AND kind = 'group_affiliation'",
                q.scope("site")
            ),
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

    /// Calls and time per bucket (`bucket_ms` = an hour or a day), buckets aligned to local time
    /// `tz_offset_min` east of UTC. Empty buckets are included. With a radio, its calls and
    /// credited time.
    pub fn series(&self, q: &Range, bucket_ms: u64, tz_offset_min: i64, f: SeriesFilter) -> rusqlite::Result<Vec<Bucket>> {
        let bucket_ms = bucket_ms.max(HOUR_MS) as i64;
        let tz = tz_offset_min * 60_000;
        let conn = lock(&self.read);
        let w = hours(q);
        let (table, unit) = match f.unit {
            Some(u) => ("radio_hour", format!(" AND unit = {u}")),
            None => ("tg_hour", String::new()),
        };
        let sql = format!(
            "SELECT ((t + ?4) / ?5) * ?5 - ?4 AS b, SUM(calls), SUM(encrypted), SUM(voice_ms), SUM(clear_grant_ms),
             SUM(enc_grant_ms) FROM {table} WHERE {} AND (?6 IS NULL OR tg = ?6){unit}
             GROUP BY b ORDER BY b",
            q.hours()
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
                Some(b) if b.t as i64 == t => out.extend(it.next()),
                _ => out.push(Bucket::empty(to_u64(t))),
            }
            t += bucket_ms;
            if out.len() > 2000 {
                break;
            }
        }
        Ok(out)
    }

    /// Calls in the window, newest first (listings and CSV).
    pub fn calls(&self, q: &Range, f: SeriesFilter, limit: usize) -> rusqlite::Result<Vec<CallRow>> {
        let conn = lock(&self.read);
        let mut st = conn.prepare(&calls_sql(q))?;
        let rows = st.query_map(params![q.site, q.from_ms as i64, q.to_ms as i64, limit as i64, f.tg, f.unit], call_row)?;
        rows.collect()
    }

    /// The calls of `calls` as CSV, handed to `chunk` about 64 KB at a time, read on a connection
    /// of the export's own (a long export never holds up the other queries). Stops early when
    /// `chunk` returns false.
    pub fn export_csv(&self, q: &Range, f: SeriesFilter, limit: usize, mut chunk: impl FnMut(String) -> bool) -> rusqlite::Result<()> {
        let conn = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        conn.busy_timeout(READ_BUSY)?;
        let mut st = conn.prepare(&calls_sql(q))?;
        let mut rows = st.query(params![q.site, q.from_ms as i64, q.to_ms as i64, limit as i64, f.tg, f.unit])?;
        let mut buf = String::from(CSV_HEADER);
        while let Some(r) = rows.next()? {
            push_csv(&mut buf, &call_row(r)?);
            if buf.len() >= CSV_CHUNK && !chunk(std::mem::take(&mut buf)) {
                return Ok(());
            }
        }
        if !buf.is_empty() {
            chunk(buf);
        }
        Ok(())
    }

    /// The newest calls of a site, newest first.
    pub fn latest_calls(&self, site: &str, limit: usize) -> rusqlite::Result<Vec<CallRow>> {
        let conn = lock(&self.read);
        let mut st = conn.prepare(&format!(
            "SELECT {CALL_COLUMNS} FROM calls c WHERE c.site = ?1 ORDER BY c.started_ms DESC LIMIT ?2"
        ))?;
        let rows = st.query_map(params![site, limit as i64], call_row)?;
        rows.collect()
    }

    /// The newest stored call with this id.
    pub fn call(&self, call_id: u64) -> rusqlite::Result<Option<CallRow>> {
        let conn = lock(&self.read);
        conn.query_row(
            &format!("SELECT {CALL_COLUMNS} FROM calls c WHERE c.call_id = ?1 ORDER BY c.started_ms DESC LIMIT 1"),
            params![call_id as i64],
            call_row,
        )
        .optional()
    }

    /// The highest call id stored (call ids continue after it).
    pub fn max_call_id(&self) -> rusqlite::Result<u64> {
        Ok(to_u64(lock(&self.read).query_row("SELECT COALESCE(MAX(call_id), 0) FROM calls", [], |r| r.get(0))?))
    }
}

/// The insert, on the caller's transaction.
fn insert_calls(tx: &rusqlite::Transaction, rows: &[CallRow]) -> rusqlite::Result<usize> {
    let mut added = 0;
    let mut ins = tx.prepare_cached(
        "INSERT OR IGNORE INTO calls (site, call_id, started_ms, ended_ms, tg, source, freq_hz, channel, timeslot, lane,
         encrypted, followed, not_followed, voice_ms, grant_ms, codec, frames, frame_errors, close_reason, end_kind,
         emergency, target, first_voice_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
    )?;
    let mut unit = tx.prepare_cached(
        "INSERT INTO transmissions (call, site, unit, started_ms, primary_src) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut tg_hour = tx.prepare_cached(
        "INSERT INTO tg_hour (site, t, tg, calls, followed, encrypted, voice_ms, clear_grant_ms, enc_grant_ms,
         voiced_grant_ms, first_ms, last_ms) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
         ON CONFLICT(site, t, tg) DO UPDATE SET calls = calls + 1, followed = followed + excluded.followed,
         encrypted = encrypted + excluded.encrypted, voice_ms = voice_ms + excluded.voice_ms,
         clear_grant_ms = clear_grant_ms + excluded.clear_grant_ms,
         enc_grant_ms = enc_grant_ms + excluded.enc_grant_ms,
         voiced_grant_ms = voiced_grant_ms + excluded.voiced_grant_ms,
         first_ms = MIN(first_ms, excluded.first_ms), last_ms = MAX(last_ms, excluded.last_ms)",
    )?;
    let mut radio_hour = tx.prepare_cached(
        "INSERT INTO radio_hour (site, t, unit, tg, calls, encrypted, voice_ms, clear_grant_ms, enc_grant_ms, last_ms)
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(site, t, unit, tg) DO UPDATE SET calls = calls + 1,
         encrypted = encrypted + excluded.encrypted, voice_ms = voice_ms + excluded.voice_ms,
         clear_grant_ms = clear_grant_ms + excluded.clear_grant_ms,
         enc_grant_ms = enc_grant_ms + excluded.enc_grant_ms, last_ms = MAX(last_ms, excluded.last_ms)",
    )?;
    let mut site = tx.prepare_cached(
        "INSERT INTO sites (id, calls, first_ms, last_ms) VALUES (?1, 1, ?2, ?2)
         ON CONFLICT(id) DO UPDATE SET calls = calls + 1, first_ms = MIN(COALESCE(first_ms, excluded.first_ms), excluded.first_ms),
         last_ms = MAX(COALESCE(last_ms, excluded.last_ms), excluded.last_ms)",
    )?;
    let mut talkgroup = tx.prepare_cached(
        "INSERT INTO talkgroups (system, tg, first_ms, last_ms, calls, first_encrypted_ms, last_encrypted_ms, last_clear_ms)
         SELECT COALESCE(system, id), ?2, ?3, ?3, 1, ?4, ?4, ?5 FROM sites WHERE id = ?1
         ON CONFLICT(system, tg) DO UPDATE SET first_ms = MIN(first_ms, excluded.first_ms),
         last_ms = MAX(last_ms, excluded.last_ms), calls = calls + 1,
         first_encrypted_ms = COALESCE(MIN(first_encrypted_ms, excluded.first_encrypted_ms), first_encrypted_ms, excluded.first_encrypted_ms),
         last_encrypted_ms = COALESCE(MAX(last_encrypted_ms, excluded.last_encrypted_ms), last_encrypted_ms, excluded.last_encrypted_ms),
         last_clear_ms = COALESCE(MAX(last_clear_ms, excluded.last_clear_ms), last_clear_ms, excluded.last_clear_ms)",
    )?;
    let mut radio = tx.prepare_cached(
        "INSERT INTO radios (system, unit, first_ms, last_ms, calls)
         SELECT COALESCE(system, id), ?2, ?3, ?3, 1 FROM sites WHERE id = ?1
         ON CONFLICT(system, unit) DO UPDATE SET first_ms = MIN(first_ms, excluded.first_ms),
         last_ms = MAX(last_ms, excluded.last_ms), calls = calls + 1",
    )?;
    for r in rows {
        let n = ins.execute(params![
            r.site, r.call_id as i64, r.started_ms as i64, r.ended_ms as i64, r.tg, r.source, r.freq_hz.map(|f| f as i64),
            r.channel, r.timeslot, r.lane, r.encrypted, r.followed, r.not_followed, r.voice_ms as i64, r.grant_ms as i64,
            r.codec, r.frames as i64, r.frame_errors as i64, r.close_reason, r.end_kind, r.emergency,
            if r.private { "unit" } else { "group" }, r.first_voice_ms.map(|t| t as i64),
        ])?;
        if n == 0 {
            continue;
        }
        added += 1;
        let id = tx.last_insert_rowid();
        let started = r.started_ms as i64;
        let t = (r.started_ms / HOUR_MS * HOUR_MS) as i64;
        let grant = r.grant_only_ms() as i64;
        let (clear_grant, enc_grant) = if r.encrypted { (0, grant) } else { (grant, 0) };
        let voiced_grant = if r.voice_ms > 0 { r.grant_ms as i64 } else { 0 };
        tg_hour.execute(params![r.site, t, r.tg, r.followed, r.encrypted, r.voice_ms as i64, clear_grant, enc_grant, voiced_grant, started])?;
        site.execute(params![r.site, started])?;
        let (enc_at, clear_at) = if r.encrypted { (Some(started), None) } else { (None, Some(started)) };
        talkgroup.execute(params![r.site, r.tg, started, enc_at, clear_at])?;
        let mut seen: Vec<u32> = Vec::new();
        for u in r.source.iter().chain(r.sources.iter()) {
            if *u == 0 || seen.contains(u) {
                continue;
            }
            let primary = seen.is_empty();
            seen.push(*u);
            unit.execute(params![id, r.site, u, started, primary])?;
            let (voice, cg, eg) = if primary { (r.voice_ms as i64, clear_grant, enc_grant) } else { (0, 0, 0) };
            radio_hour.execute(params![r.site, t, u, r.tg, r.encrypted, voice, cg, eg, started])?;
            radio.execute(params![r.site, u, started])?;
        }
    }
    Ok(added)
}

/// The calls query (`?1` site or system, `?2`..`?3` the window, `?4` limit, `?5` talkgroup, `?6`
/// radio), newest first.
fn calls_sql(q: &Range) -> String {
    format!(
        "SELECT {CALL_COLUMNS} FROM calls c WHERE {} AND c.started_ms >= ?2 AND c.started_ms < ?3
         AND (?5 IS NULL OR c.tg = ?5)
         AND (?6 IS NULL OR c.id IN (SELECT call FROM transmissions WHERE {} AND unit = ?6 AND started_ms >= ?2 AND started_ms < ?3))
         ORDER BY c.started_ms DESC LIMIT ?4",
        q.scope("c.site"),
        q.scope("site")
    )
}

const CSV_CHUNK: usize = 64 * 1024;
pub const CSV_HEADER: &str =
    "site,call_id,started_utc,started_ms,ended_ms,tg,source,sources,freq_hz,channel,timeslot,lane,encrypted,followed,not_followed,voice_ms,grant_ms,codec,frames,frame_errors,close_reason,end_kind,emergency,private,first_voice_ms\n";

/// One call as a CSV line.
pub fn push_csv(out: &mut String, r: &CallRow) {
    let opt = |v: Option<String>| v.unwrap_or_default();
    out.push_str(&format!(
        "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}\n",
        r.site,
        r.call_id,
        crate::util::time::iso_utc(r.started_ms),
        r.started_ms,
        r.ended_ms,
        r.tg,
        opt(r.source.map(|v| v.to_string())),
        r.sources.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "),
        opt(r.freq_hz.map(|v| v.to_string())),
        opt(r.channel.clone()),
        opt(r.timeslot.map(|v| v.to_string())),
        r.lane,
        u8::from(r.encrypted),
        u8::from(r.followed),
        opt(r.not_followed.clone()),
        r.voice_ms,
        r.grant_ms,
        opt(r.codec.clone()),
        r.frames,
        r.frame_errors,
        r.close_reason,
        opt(r.end_kind.clone()),
        u8::from(r.emergency),
        u8::from(r.private),
        opt(r.first_voice_ms.map(|v| v.to_string())),
    ));
}

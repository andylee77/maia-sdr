//! What is on the card as files to take away: the call recordings as one ZIP archive (a period,
//! a site, a talkgroup, a radio, or the locked ones), the history's database as a consistent
//! copy, and a TV recording's file.
//!
//! One download runs at a time. It is made on a thread of its own, below the receivers'
//! priority, and its bytes are handed to the response as the client takes them: a recording is
//! read when its turn comes, and nothing is kept of an archive but its directory.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use crate::services::config::aliases::AliasIndex;
use crate::services::events::EventLog;
use crate::services::history::store::Store as HistoryStore;
use crate::services::recordings::{storage, Recording};
use crate::util::time::{iso_utc, unix_ms, utc_date};
use crate::util::zip::ZipWriter;

/// A file is sent this much at a time.
const CHUNK: usize = 256 << 10;
/// Chunks made ahead of the client.
const AHEAD: usize = 4;
/// The card must have this much more free than a copy of the history takes.
const COPY_MARGIN: u64 = 64 << 20;
/// The archive's table of what it holds, its last file.
const INDEX: &str = "recordings.csv";
const INDEX_HEADER: &str = "file,started_utc,site,talkgroup,talkgroup_name,radio,radio_name,duration_s,bytes,locked,call\n";
/// The history's download, as the client saves it.
pub const HISTORY_NAME: &str = "scanner-history.sqlite";

/// A download's bytes as they are made; an error ends it short.
pub type Chunks = mpsc::Receiver<std::io::Result<Vec<u8>>>;
/// Each site's system's aliases, for the names in an archive's table.
pub type Indexes = HashMap<String, Arc<AliasIndex>>;

/// Which recordings: each field narrows, and none given takes them all.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Selection {
    /// Calls that started at or after this (unix ms)...
    pub from: Option<u64>,
    /// ...and before this.
    pub to: Option<u64>,
    pub site: Option<String>,
    pub tg: Option<u32>,
    /// A radio that talked in the call, as far as the list knows: the one in the file's name,
    /// and for the recordings made since the scanner started every one.
    pub unit: Option<u32>,
    /// Only the locked ones.
    pub locked: bool,
}

/// A recording picked for an archive: where its bytes are, and its line of the table.
#[derive(Debug, Clone)]
pub struct Picked {
    path: PathBuf,
    /// Not on the card yet: its bytes.
    pending: Option<Arc<Vec<u8>>>,
    site: String,
    call: u64,
    tg: u32,
    source: Option<u32>,
    started_unix_ms: u64,
    duration_ms: u64,
    locked: bool,
}

impl Selection {
    pub fn takes(&self, r: &Recording) -> bool {
        self.from.is_none_or(|t| r.started_unix_ms >= t)
            && self.to.is_none_or(|t| r.started_unix_ms < t)
            && self.site.as_ref().is_none_or(|s| &r.site == s)
            && self.tg.is_none_or(|tg| r.tg == tg)
            && self.unit.is_none_or(|u| r.source == Some(u) || r.sources.contains(&u))
            && (!self.locked || r.locked)
    }

    /// The recordings of `list` it takes, in the list's order (oldest first).
    pub fn pick(&self, list: &VecDeque<Recording>) -> Vec<Picked> {
        list.iter()
            .filter(|r| self.takes(r))
            .map(|r| Picked {
                path: r.path.clone(),
                pending: r.pending.clone(),
                site: r.site.clone(),
                call: r.id,
                tg: r.tg,
                source: r.source,
                started_unix_ms: r.started_unix_ms,
                duration_ms: r.duration_ms,
                locked: r.locked,
            })
            .collect()
    }

    /// How many of `list` it takes, and their bytes.
    pub fn count(&self, list: &VecDeque<Recording>) -> (usize, u64) {
        list.iter().filter(|r| self.takes(r)).fold((0, 0), |(n, b), r| (n + 1, b + r.bytes))
    }

    /// The archive's name: the days of its first and last recording (UTC), and what narrowed it.
    pub fn archive_name(&self, picked: &[Picked]) -> String {
        let day = |p: Option<&Picked>| p.map(|p| utc_date(p.started_unix_ms)).unwrap_or_default();
        let (first, last) = (day(picked.first()), day(picked.last()));
        let mut name = format!("recordings_{first}");
        if last != first {
            name.push_str(&format!("_{last}"));
        }
        if let Some(site) = &self.site {
            let safe: String = site.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            name.push_str(&format!("_{safe}"));
        }
        if let Some(tg) = self.tg {
            name.push_str(&format!("_tg{tg}"));
        }
        if let Some(unit) = self.unit {
            name.push_str(&format!("_radio{unit}"));
        }
        if self.locked {
            name.push_str("_locked");
        }
        name + ".zip"
    }
}

/// The recordings whose calls started in one span of time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Span {
    /// Where the span starts (unix ms); it is `span_ms` long.
    pub from_ms: u64,
    pub count: usize,
    pub bytes: u64,
    pub locked: usize,
}

/// The recordings by the span of time (`span_ms` long, counted from 1970 UTC) their calls
/// started in, the newest first; a span with none is left out. A reader adds spans up into the
/// days of its own zone, which the radio does not know (an hour is a whole number of spans in
/// every zone at 15 minutes, and in most at 60).
pub fn spans(list: &VecDeque<Recording>, span_ms: u64) -> Vec<Span> {
    let mut by_span: BTreeMap<u64, Span> = BTreeMap::new();
    for r in list {
        let from_ms = r.started_unix_ms / span_ms * span_ms;
        let s = by_span.entry(from_ms).or_insert(Span { from_ms, count: 0, bytes: 0, locked: 0 });
        s.count += 1;
        s.bytes += r.bytes;
        s.locked += usize::from(r.locked);
    }
    by_span.into_values().rev().collect()
}

/// A field of the table: quoted when it holds a comma, a quote or a line break.
fn csv_field(v: &str) -> String {
    if v.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", v.replace('"', "\"\""))
    } else {
        v.to_string()
    }
}

/// A recording's place in an archive: the folder of its call's UTC date, as on the card.
fn entry_name(p: &Picked) -> String {
    let file = p.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{}/{file}", utc_date(p.started_unix_ms))
}

fn index_line(p: &Picked, entry: &str, bytes: usize, names: &Indexes) -> String {
    let ix = names.get(&p.site);
    let tg_name = ix.and_then(|ix| ix.talkgroup(p.tg)).map(|a| a.name.as_str()).unwrap_or_default();
    let radio_name = ix.and_then(|ix| p.source.and_then(|s| ix.radio(s))).map(|a| a.name.as_str()).unwrap_or_default();
    format!(
        "{},{},{},{},{},{},{},{:.1},{bytes},{},{}\n",
        csv_field(entry),
        iso_utc(p.started_unix_ms),
        csv_field(&p.site),
        p.tg,
        csv_field(tg_name),
        p.source.map(|s| s.to_string()).unwrap_or_default(),
        csv_field(radio_name),
        p.duration_ms as f64 / 1000.0,
        u8::from(p.locked),
        p.call,
    )
}

/// The download running.
struct Running {
    what: &'static str,
    name: String,
    started: Instant,
    started_unix_ms: u64,
    files: u64,
    bytes: AtomicU64,
    files_done: AtomicU64,
    bytes_done: AtomicU64,
    /// Recordings left out: gone from the card since they were listed, or unreadable.
    skipped: AtomicU64,
    /// The history's copy is being made; nothing is sent yet.
    copying: AtomicBool,
    /// Everything was handed to the client.
    complete: AtomicBool,
}

/// A download as the pages show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Download {
    /// `recordings`, `history` or `tv`.
    pub what: &'static str,
    /// The file as the client saves it.
    pub name: String,
    pub started_unix_ms: u64,
    pub copying: bool,
    pub files: u64,
    pub files_done: u64,
    pub bytes: u64,
    pub bytes_done: u64,
    pub skipped: u64,
}

impl Running {
    fn view(&self) -> Download {
        Download {
            what: self.what,
            name: self.name.clone(),
            started_unix_ms: self.started_unix_ms,
            copying: self.copying.load(Ordering::Relaxed),
            files: self.files,
            files_done: self.files_done.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            bytes_done: self.bytes_done.load(Ordering::Relaxed),
            skipped: self.skipped.load(Ordering::Relaxed),
        }
    }
}

/// Why a download did not start.
#[derive(Debug)]
pub enum Refused {
    /// Another one is running.
    Busy(Download),
    Failed(String),
}

pub struct Files {
    running: Mutex<Option<Arc<Running>>>,
    log: Arc<EventLog>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The running download's place: given up when its thread ends, however it ends, with a line in
/// the radio's log.
struct Slot {
    files: Arc<Files>,
    run: Arc<Running>,
}

impl Drop for Slot {
    fn drop(&mut self) {
        *lock(&self.files.running) = None;
        let d = self.run.view();
        let mb = d.bytes_done as f64 / f64::from(1 << 20);
        let secs = self.run.started.elapsed().as_secs_f64();
        let left_out = if d.skipped > 0 { format!(", {} recording(s) no longer readable left out", d.skipped) } else { String::new() };
        self.files.log.radio(
            "files",
            if self.run.complete.load(Ordering::Relaxed) {
                format!("{} sent: {} file(s), {mb:.1} MB in {secs:.0} s{left_out}", d.name, d.files_done)
            } else {
                format!("{} stopped after {} of {} file(s), {mb:.1} MB in {secs:.0} s: the client left or a read failed{left_out}", d.name, d.files_done, d.files)
            },
        );
    }
}

/// The calling thread below the receivers': a download takes a processor only when they leave it.
fn below_the_receivers() {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain system call; on Linux, process 0 here is the calling thread.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 10);
    }
}

/// Send `path` as it is. False when it ended short.
fn send_file(run: &Running, path: &Path, tx: &mpsc::Sender<std::io::Result<Vec<u8>>>) -> bool {
    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            let _ = tx.blocking_send(Err(e));
            return false;
        }
    };
    loop {
        let mut buf = vec![0u8; CHUNK];
        match file.read(&mut buf) {
            Ok(0) => return true,
            Ok(n) => {
                buf.truncate(n);
                if tx.blocking_send(Ok(buf)).is_err() {
                    return false;
                }
                run.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(e) => {
                let _ = tx.blocking_send(Err(e));
                return false;
            }
        }
    }
}

/// Make and send the archive: each recording under its header, then the table of them, then the
/// directory. A recording that cannot be read any more is left out of both.
fn send_archive(run: &Running, picked: &[Picked], names: &Indexes, tx: &mpsc::Sender<std::io::Result<Vec<u8>>>) -> bool {
    let mut zip = ZipWriter::new();
    let mut index = String::from(INDEX_HEADER);
    for p in picked {
        let data = match &p.pending {
            Some(bytes) => Some(bytes.as_ref().clone()),
            None => std::fs::read(&p.path).ok(),
        };
        let entry = entry_name(p);
        let Some((data, head)) = data.and_then(|d| zip.file(&entry, p.started_unix_ms / 1000, &d).map(|h| (d, h))) else {
            run.skipped.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let n = data.len();
        index.push_str(&index_line(p, &entry, n, names));
        if tx.blocking_send(Ok(head)).is_err() || tx.blocking_send(Ok(data)).is_err() {
            return false;
        }
        run.files_done.fetch_add(1, Ordering::Relaxed);
        run.bytes_done.fetch_add(n as u64, Ordering::Relaxed);
    }
    let index = index.into_bytes();
    let Some(head) = zip.file(INDEX, unix_ms() / 1000, &index) else { return false };
    tx.blocking_send(Ok(head)).is_ok() && tx.blocking_send(Ok(index)).is_ok() && tx.blocking_send(Ok(zip.finish())).is_ok()
}

/// Where the history's copy is made: beside the database.
fn copy_path(database: &Path) -> PathBuf {
    PathBuf::from(format!("{}.download", database.display()))
}

/// Remove a copy of the history an earlier run left when it was stopped mid-download.
pub fn remove_stale_copy(database: &Path) {
    let _ = std::fs::remove_file(copy_path(database));
}

/// The copy removed when its download ends.
struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Files {
    pub fn new(log: Arc<EventLog>) -> Arc<Files> {
        Arc::new(Files { running: Mutex::new(None), log })
    }

    /// The download running, if one is.
    pub fn download(&self) -> Option<Download> {
        lock(&self.running).as_ref().map(|r| r.view())
    }

    fn begin(self: &Arc<Self>, what: &'static str, name: String, files: u64, bytes: u64) -> Result<Slot, Refused> {
        let mut running = lock(&self.running);
        if let Some(r) = running.as_ref() {
            return Err(Refused::Busy(r.view()));
        }
        let run = Arc::new(Running {
            what,
            name,
            started: Instant::now(),
            started_unix_ms: unix_ms(),
            files,
            bytes: AtomicU64::new(bytes),
            files_done: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            skipped: AtomicU64::new(0),
            copying: AtomicBool::new(false),
            complete: AtomicBool::new(false),
        });
        *running = Some(run.clone());
        Ok(Slot { files: self.clone(), run })
    }

    fn spawn(slot: Slot, work: impl FnOnce(&Running) -> bool + Send + 'static) -> Result<(), Refused> {
        std::thread::Builder::new()
            .name("files".into())
            .spawn(move || {
                below_the_receivers();
                let complete = work(&slot.run);
                slot.run.complete.store(complete, Ordering::Relaxed);
            })
            .map(|_| ())
            .map_err(|e| Refused::Failed(format!("starting the download's thread: {e}")))
    }

    /// The picked recordings as one ZIP archive called `name`, `bytes` of recordings in all.
    pub fn recordings(self: &Arc<Self>, picked: Vec<Picked>, bytes: u64, names: Indexes, name: String) -> Result<Chunks, Refused> {
        let slot = self.begin("recordings", name, picked.len() as u64, bytes)?;
        let (tx, rx) = mpsc::channel(AHEAD);
        Self::spawn(slot, move |run| send_archive(run, &picked, &names, &tx))?;
        Ok(rx)
    }

    /// The file at `path` as it is, saved as `name`; with its size.
    pub fn file(self: &Arc<Self>, what: &'static str, path: PathBuf, name: String) -> Result<(u64, Chunks), Refused> {
        let bytes = std::fs::metadata(&path).map_err(|e| Refused::Failed(format!("{name}: {e}")))?.len();
        let slot = self.begin(what, name, 1, bytes)?;
        let (tx, rx) = mpsc::channel(AHEAD);
        Self::spawn(slot, move |run| {
            let sent = send_file(run, &path, &tx);
            run.files_done.fetch_add(u64::from(sent), Ordering::Relaxed);
            sent
        })?;
        Ok((bytes, rx))
    }

    /// The history's database as one consistent file: a copy is made beside it first (the writer
    /// goes on meanwhile), sent, and removed. Returns once the copy is made, with its size.
    pub async fn history(self: &Arc<Self>, store: Arc<HistoryStore>) -> Result<(u64, Chunks), Refused> {
        let size = store.size_bytes();
        let copy = copy_path(&store.path);
        if let (_, Some(free)) = storage::fs_space(copy.parent().unwrap_or(Path::new("."))) {
            if free < size + COPY_MARGIN {
                return Err(Refused::Failed(format!("a copy of the history takes {} MB and the card has {} MB free", size >> 20, free >> 20)));
            }
        }
        let slot = self.begin("history", HISTORY_NAME.to_string(), 1, size)?;
        slot.run.copying.store(true, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(AHEAD);
        let (ready, made) = oneshot::channel::<Result<u64, String>>();
        Self::spawn(slot, move |run| {
            let copy = Copy(copy);
            let made = store.copy_to(&copy.0).map_err(|e| format!("copying the history: {e}"));
            run.copying.store(false, Ordering::Relaxed);
            if let Ok(bytes) = made {
                run.bytes.store(bytes, Ordering::Relaxed);
            }
            let ok = made.is_ok();
            // Nobody waits for it any more: the client left while the copy was made.
            if ready.send(made).is_err() || !ok {
                return false;
            }
            let sent = send_file(run, &copy.0, &tx);
            run.files_done.fetch_add(u64::from(sent), Ordering::Relaxed);
            sent
        })?;
        match made.await {
            Ok(Ok(bytes)) => Ok((bytes, rx)),
            Ok(Err(e)) => Err(Refused::Failed(e)),
            Err(_) => Err(Refused::Failed("the history's copy was not made".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::recordings::Store;
    use crate::util::zip;

    // 2026-11-11 13:29:53 UTC.
    const T: u64 = 1_794_403_793_000;

    fn rec(id: u64, at: u64, tg: u32, source: Option<u32>, site: &str, locked: bool, bytes: u64) -> Recording {
        Recording {
            id,
            site: site.into(),
            tg,
            source,
            started_unix_ms: at,
            bytes,
            store: Store::Sd,
            sources: source.into_iter().collect(),
            locked,
            ..Recording::default()
        }
    }

    fn list() -> VecDeque<Recording> {
        VecDeque::from([
            rec(1, T - 2 * 86_400_000, 300, Some(1012), "clay", false, 100),
            rec(2, T - 3_600_000, 300, Some(1015), "clay", true, 200),
            rec(3, T, 318, None, "clay", false, 300),
            rec(4, T + 1000, 300, Some(1012), "elec", false, 400),
        ])
    }

    fn ids(sel: &Selection) -> Vec<u64> {
        sel.pick(&list()).iter().map(|p| p.call).collect()
    }

    #[test]
    fn a_selection_narrows_by_each_field() {
        assert_eq!(ids(&Selection::default()), [1, 2, 3, 4], "nothing given: all, oldest first");
        assert_eq!(ids(&Selection { from: Some(T - 3_600_000), to: Some(T), ..Default::default() }), [2], "from is in, to is out");
        assert_eq!(ids(&Selection { tg: Some(300), ..Default::default() }), [1, 2, 4]);
        assert_eq!(ids(&Selection { tg: Some(300), site: Some("clay".into()), ..Default::default() }), [1, 2]);
        assert_eq!(ids(&Selection { unit: Some(1012), ..Default::default() }), [1, 4]);
        assert_eq!(ids(&Selection { locked: true, ..Default::default() }), [2]);
        assert_eq!(Selection { tg: Some(300), ..Default::default() }.count(&list()), (3, 700));
        // A radio that talked later in a call, known for this run's recordings.
        let mut l = list();
        l[2].sources = vec![2001, 1015];
        assert_eq!(Selection { unit: Some(1015), ..Default::default() }.pick(&l).iter().map(|p| p.call).collect::<Vec<_>>(), [2, 3]);
    }

    #[test]
    fn an_archive_is_named_by_its_days_and_what_narrowed_it() {
        let name = |sel: Selection| sel.archive_name(&sel.pick(&list()));
        assert_eq!(name(Selection::default()), "recordings_2026-11-09_2026-11-11.zip");
        assert_eq!(name(Selection { from: Some(T - 3_600_000), ..Default::default() }), "recordings_2026-11-11.zip");
        assert_eq!(
            name(Selection { tg: Some(300), unit: Some(1012), site: Some("cl/ay".into()), locked: true, ..Default::default() }),
            "recordings__clay_tg300_radio1012_locked.zip",
            "nothing picked: no day; a site's name keeps only what is safe in a file's"
        );
    }

    #[test]
    fn the_listing_is_by_span_of_time() {
        const HOUR: u64 = 3_600_000;
        // 13:00 UTC of the day: the hour of the calls at 13:29:53 and a second later.
        let h13 = 1_794_355_200_000 + 13 * HOUR;
        let by_hour = spans(&list(), HOUR);
        assert_eq!(
            by_hour.iter().map(|s| (s.from_ms, s.count, s.bytes, s.locked)).collect::<Vec<_>>(),
            [(h13, 2, 700, 0), (h13 - HOUR, 1, 200, 1), (h13 - 48 * HOUR, 1, 100, 0)],
            "the newest first, an hour with none left out"
        );
        // A span's bounds select its recordings.
        let sel = Selection { from: Some(h13), to: Some(h13 + HOUR), ..Default::default() };
        assert_eq!(sel.count(&list()), (2, 700));
        // Whole UTC days, as the card's folders.
        let by_day = spans(&list(), 24 * HOUR);
        assert_eq!(by_day.iter().map(|s| (utc_date(s.from_ms), s.count, s.locked)).collect::<Vec<_>>(), [("2026-11-11".to_string(), 3, 1), ("2026-11-09".to_string(), 1, 0)]);
        assert!(spans(&VecDeque::new(), HOUR).is_empty());
    }

    #[test]
    fn the_table_quotes_what_needs_it() {
        assert_eq!(csv_field("CCFR Primary"), "CCFR Primary");
        assert_eq!(csv_field("Fire, \"East\""), "\"Fire, \"\"East\"\"\"");
    }

    /// The service with a log to read back.
    fn service() -> (Arc<Files>, Arc<EventLog>) {
        let log = Arc::new(EventLog::default());
        (Files::new(log.clone()), log)
    }

    async fn all(mut chunks: Chunks) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(c) = chunks.recv().await {
            out.extend(c?);
        }
        Ok(out)
    }

    /// Wait for the download's thread to give its place up.
    async fn ended(files: &Files) {
        for _ in 0..200 {
            if files.download().is_none() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("the download did not end");
    }

    #[tokio::test]
    async fn recordings_come_as_an_archive_with_their_table() {
        let dir = tempfile::tempdir().unwrap();
        let mut l = list();
        for r in l.iter_mut() {
            r.file = format!("rec_{}_{}_tg{}.wav", r.started_unix_ms, r.id, r.tg);
            r.path = dir.path().join(&r.file);
            r.duration_ms = 2500;
        }
        std::fs::write(&l[0].path, b"first").unwrap();
        std::fs::write(&l[1].path, b"second").unwrap();
        // The third is not on the card yet, the fourth is gone from it.
        l[2].pending = Some(Arc::new(b"third, from memory".to_vec()));
        let sel = Selection::default();
        let picked = sel.pick(&l);
        let (files, log) = service();
        let chunks = files.recordings(picked, 600, Indexes::new(), "recordings_x.zip".into()).unwrap();
        let running = files.download().unwrap();
        assert_eq!((running.what, running.name.as_str(), running.files, running.bytes), ("recordings", "recordings_x.zip", 4, 600));
        // One at a time.
        assert!(matches!(files.file("tv", l[0].path.clone(), "rf19.ts".into()), Err(Refused::Busy(d)) if d.name == "recordings_x.zip"));
        let back = zip::tests::read(&all(chunks).await.unwrap());
        assert_eq!(back.len(), 4, "three recordings and the table");
        assert_eq!((back[0].0.as_str(), back[0].1, back[0].2.as_slice()), ("2026-11-09/rec_1794230993000_1_tg300.wav", (T / 1000 - 2 * 86_400) as u32, &b"first"[..]));
        assert_eq!(back[2].2, b"third, from memory");
        let (name, _, table) = &back[3];
        assert_eq!(name, INDEX);
        let table = String::from_utf8(table.clone()).unwrap();
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(lines.len(), 4, "the header and the three it holds: {table}");
        assert_eq!(lines[0], INDEX_HEADER.trim_end());
        assert_eq!(lines[2], "2026-11-11/rec_1794400193000_2_tg300.wav,2026-11-11 12:29:53,clay,300,,1015,,2.5,6,1,2");
        ended(&files).await;
        let line = log.since(0, 10, false, Some(crate::services::events::Part::Radio)).pop().unwrap();
        assert!(line.text.starts_with("recordings_x.zip sent: 3 file(s), ") && line.text.ends_with("1 recording(s) no longer readable left out"), "{}", line.text);
        // The place is free again.
        assert!(files.file("tv", l[0].path.clone(), "rf19.ts".into()).is_ok());
    }

    #[tokio::test]
    async fn a_file_comes_whole_and_a_client_that_leaves_ends_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rf19.ts");
        let body: Vec<u8> = (0..(3 * CHUNK + 17)).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &body).unwrap();
        let (files, log) = service();
        let (bytes, chunks) = files.file("tv", path.clone(), "rf19.ts".into()).unwrap();
        assert_eq!(bytes, body.len() as u64);
        assert_eq!(all(chunks).await.unwrap(), body);
        ended(&files).await;
        // A large file whose client leaves after the first chunk.
        std::fs::write(&path, vec![0u8; 12 * CHUNK]).unwrap();
        let (_, mut chunks) = files.file("tv", path, "rf19.ts".into()).unwrap();
        assert_eq!(chunks.recv().await.unwrap().unwrap().len(), CHUNK);
        drop(chunks);
        ended(&files).await;
        let lines: Vec<String> = log.since(0, 10, false, None).into_iter().map(|e| e.text).collect();
        assert!(lines[0].starts_with("rf19.ts sent: 1 file(s), 0.8 MB"), "{lines:?}");
        assert!(lines[1].starts_with("rf19.ts stopped after 0 of 1 file(s)"), "{lines:?}");
        assert!(matches!(files.file("tv", dir.path().join("none.ts"), "none.ts".into()), Err(Refused::Failed(_))));
    }

    #[tokio::test]
    async fn the_history_comes_as_a_copy_that_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(HISTORY_NAME);
        let store = Arc::new(HistoryStore::open(&path).unwrap());
        store.with_writer(|c| c.execute_batch("CREATE TABLE probe (n INTEGER); INSERT INTO probe VALUES (41), (1);")).unwrap();
        // A copy an earlier run left.
        std::fs::write(copy_path(&path), b"stale").unwrap();
        remove_stale_copy(&path);
        assert!(!copy_path(&path).exists());
        let (files, _) = service();
        let (bytes, chunks) = files.history(store.clone()).await.unwrap();
        // The writer goes on while the copy is sent.
        store.with_writer(|c| c.execute_batch("INSERT INTO probe VALUES (100);")).unwrap();
        let sent = all(chunks).await.unwrap();
        assert_eq!(sent.len() as u64, bytes);
        ended(&files).await;
        assert!(!copy_path(&path).exists(), "the copy is removed when it is sent");
        let saved = dir.path().join("saved.sqlite");
        std::fs::write(&saved, &sent).unwrap();
        let conn = rusqlite::Connection::open(&saved).unwrap();
        let sum: i64 = conn.query_row("SELECT SUM(n) FROM probe", [], |r| r.get(0)).unwrap();
        assert_eq!(sum, 42, "the rows as they were when the copy was made");
        let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
        assert_eq!(check, "ok");
    }
}

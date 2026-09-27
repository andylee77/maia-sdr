//! Change 056: operator settings the web UI edits and that must survive
//! a reboot — call-recording policy, talkgroup and radio-unit aliases,
//! and the talkgroup monitor list. Change 057 adds the call-close
//! timing (`call`) and the recording store (RAM or SD card).
//!
//! One JSON document on the persistent JFFS2 partition, next to
//! `app::autoppm`'s `p25-ppm-cal.json`, written atomically (tmp file +
//! rename, the `services::sites::save_site` pattern). Missing or unknown
//! fields fall back to defaults so an older or hand-edited file still
//! loads; a file that does not parse at all is left untouched and the
//! defaults are used (the next successful save replaces it).
//!
//! Live consumers read the settings without taking the document lock:
//!
//!   - the recorder: [`RecordingPolicy`] (atomics);
//!   - the call lifecycle: [`CallPolicy`] (atomics);
//!   - aliases and the monitor list: copied into the decoders /
//!     `MonitorList` by the HTTP handlers whenever they change (see
//!     `httpd::api::ui` and `httpd::api::talkgroups`).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

/// On-target settings file (persistent JFFS2 partition).
pub const SETTINGS_FILE: &str = "/mnt/jffs2/p25-ui-settings.json";

/// Default recording retention: the pre-056 fixed ring size
/// (`audio::recorder::MAX_RECORDINGS`).
pub const DEFAULT_MAX_RECORDINGS: usize = 40;

/// Upper bound on the RAM-store retention. Recordings live in tmpfs
/// (`/tmp`, ~490 MB free on the board); a 30 s call is ~480 KB, so
/// 500 worst-case long calls stay well inside it.
pub const MAX_RECORDINGS_LIMIT: usize = 500;

/// Change 057: SD-store retention by count. The index of every stored
/// recording is kept in RAM (a few hundred bytes each) and listed by
/// `/api/recordings`, so the count is capped even though the card is
/// large (~58 GB free on the bench cards).
pub const DEFAULT_SD_MAX_COUNT: usize = 2_000;
pub const SD_MAX_COUNT_LIMIT: usize = 5_000;

/// Change 057: SD-store size cap in MB (8 kHz 16-bit mono = 16 KB/s,
/// so 2 GB ≈ 36 h of voice).
pub const DEFAULT_SD_MAX_MB: u64 = 2_048;
pub const SD_MAX_MB_MIN: u64 = 16;
pub const SD_MAX_MB_LIMIT: u64 = 32_768;

/// Change 057: call close timing (see `app::grant_follower` and
/// doc/changes/057, which has the measurements).
///
/// `hang_ms`: a call with no keep-alive (voice of this call, HDU, CC
/// grant / grant update for its TG on its channel) for this long closes
/// ("timeout"). The fallback when no terminator is decoded (one
/// transmission in 719 in the SDRTrunk logs). 3 s: three times the
/// largest CC update gap of a live channel (0.96 s), SDRTrunk's (fork)
/// traffic-channel timeout.
///
/// `end_grace_ms`: after an end-of-transmission marker (LC-valid TDULC
/// after voice, at the last LDU in 97 % of transmissions) the call
/// closes this long later unless voice resumes ("call_end"); a reply
/// granted inside the window pre-empts it ("tg_change"). 2 s: the
/// system holds the channel 1.26–1.67 s after the last LDU, and 99 %
/// of same-TG replies start within 2 s of it, so the follower stays on
/// the conversation without holding a dead channel.
pub const DEFAULT_HANG_MS: u64 = 3_000;
pub const HANG_MS_MIN: u64 = 1_000;
pub const HANG_MS_MAX: u64 = 30_000;
pub const DEFAULT_END_GRACE_MS: u64 = 2_000;
pub const END_GRACE_MS_MAX: u64 = 10_000;

/// Alias display names are trimmed and capped at this many characters.
pub const MAX_ALIAS_CHARS: usize = 48;

/// Cap on the number of entries in each alias map and the monitor list.
pub const MAX_ENTRIES: usize = 4000;

/// Call ids the recorder skipped because recording was off, kept so the
/// call list can say "not recorded" instead of "missing".
const SKIPPED_IDS_KEEP: usize = 256;

/// Change 057: where new recordings are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageKind {
    /// tmpfs (`/tmp/p25_recordings`), lost on reboot. The pre-057
    /// behaviour.
    #[default]
    Ram,
    /// The FAT32 SD partition (`/mnt/sd/p25_recordings`), kept across
    /// reboots. Written off the hot path (`audio::rec_storage`).
    Sd,
}

impl StorageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StorageKind::Ram => "ram",
            StorageKind::Sd => "sd",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// Write a WAV for every followed call.
    pub enabled: bool,
    /// Number of recordings kept in RAM; the oldest RAM recording is
    /// deleted beyond this.
    pub max_count: usize,
    /// Change 057: store for NEW recordings. Existing recordings stay
    /// where they are when this changes.
    pub storage: StorageKind,
    /// Change 057: SD-store retention (count and size); only ever
    /// deletes SD recordings.
    pub sd_max_count: usize,
    pub sd_max_mb: u64,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        RecordingSettings {
            enabled: true,
            max_count: DEFAULT_MAX_RECORDINGS,
            storage: StorageKind::Ram,
            sd_max_count: DEFAULT_SD_MAX_COUNT,
            sd_max_mb: DEFAULT_SD_MAX_MB,
        }
    }
}

/// Change 057: call close timing (see the `DEFAULT_HANG_MS` doc).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CallSettings {
    pub hang_ms: u64,
    pub end_grace_ms: u64,
}

impl Default for CallSettings {
    fn default() -> Self {
        CallSettings {
            hang_ms: DEFAULT_HANG_MS,
            end_grace_ms: DEFAULT_END_GRACE_MS,
        }
    }
}

/// The persisted document.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub recording: RecordingSettings,
    /// Change 057.
    pub call: CallSettings,
    /// Talkgroup id -> display name.
    pub tg_aliases: BTreeMap<u16, String>,
    /// Radio unit id (source) -> display name.
    pub unit_aliases: BTreeMap<u32, String>,
    /// Talkgroup monitor list (empty = follow every clear grant).
    /// Order is priority order, as in `services::monitor::MonitorList`.
    pub monitor_tgs: Vec<u16>,
}

/// Partial update accepted by `PUT /api/ui/settings`. Maps and lists
/// replace the stored value wholesale; absent fields are unchanged.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub recording: Option<RecordingPatch>,
    pub call: Option<CallPatch>,
    pub tg_aliases: Option<BTreeMap<u16, String>>,
    pub unit_aliases: Option<BTreeMap<u32, String>>,
    pub monitor_tgs: Option<Vec<u16>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingPatch {
    pub enabled: Option<bool>,
    pub max_count: Option<usize>,
    pub storage: Option<StorageKind>,
    pub sd_max_count: Option<usize>,
    pub sd_max_mb: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallPatch {
    pub hang_ms: Option<u64>,
    pub end_grace_ms: Option<u64>,
}

/// What a patch touched, so the caller only re-applies the live state
/// that changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changed {
    pub recording: bool,
    pub call: bool,
    pub tg_aliases: bool,
    pub unit_aliases: bool,
    pub monitor_tgs: bool,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.recording || self.call || self.tg_aliases || self.unit_aliases || self.monitor_tgs
    }
}

fn check_range<T: PartialOrd + std::fmt::Display + Copy>(
    name: &str,
    v: T,
    lo: T,
    hi: T,
) -> Result<T, String> {
    if v < lo || v > hi {
        return Err(format!("{name} {v} out of range {lo}..={hi}"));
    }
    Ok(v)
}

fn clean_alias(name: &str) -> Option<String> {
    let t = name.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(MAX_ALIAS_CHARS).collect())
}

fn clean_aliases<K: Ord + Copy>(m: &BTreeMap<K, String>) -> BTreeMap<K, String> {
    m.iter()
        .filter_map(|(k, v)| clean_alias(v).map(|v| (*k, v)))
        .collect()
}

/// Validate `patch` against `base` and return the resulting document.
/// Pure (no I/O) so it is host-tested directly.
pub fn apply_patch(
    base: &UiSettings,
    patch: SettingsPatch,
) -> Result<(UiSettings, Changed), String> {
    let mut out = base.clone();
    let mut changed = Changed::default();

    if let Some(r) = patch.recording {
        if let Some(n) = r.max_count {
            out.recording.max_count =
                check_range("recording.max_count", n, 1, MAX_RECORDINGS_LIMIT)?;
        }
        if let Some(n) = r.sd_max_count {
            out.recording.sd_max_count =
                check_range("recording.sd_max_count", n, 1, SD_MAX_COUNT_LIMIT)?;
        }
        if let Some(n) = r.sd_max_mb {
            out.recording.sd_max_mb =
                check_range("recording.sd_max_mb", n, SD_MAX_MB_MIN, SD_MAX_MB_LIMIT)?;
        }
        if let Some(s) = r.storage {
            out.recording.storage = s;
        }
        if let Some(e) = r.enabled {
            out.recording.enabled = e;
        }
        changed.recording = out.recording != base.recording;
    }
    if let Some(c) = patch.call {
        if let Some(v) = c.hang_ms {
            out.call.hang_ms = check_range("call.hang_ms", v, HANG_MS_MIN, HANG_MS_MAX)?;
        }
        if let Some(v) = c.end_grace_ms {
            out.call.end_grace_ms = check_range("call.end_grace_ms", v, 0, END_GRACE_MS_MAX)?;
        }
        changed.call = out.call != base.call;
    }
    if let Some(m) = patch.tg_aliases {
        if m.len() > MAX_ENTRIES {
            return Err(format!("tg_aliases: {} entries (max {MAX_ENTRIES})", m.len()));
        }
        if m.contains_key(&0) {
            return Err("tg_aliases: talkgroup 0 is not a talkgroup".into());
        }
        out.tg_aliases = clean_aliases(&m);
        changed.tg_aliases = out.tg_aliases != base.tg_aliases;
    }
    if let Some(m) = patch.unit_aliases {
        if m.len() > MAX_ENTRIES {
            return Err(format!("unit_aliases: {} entries (max {MAX_ENTRIES})", m.len()));
        }
        if let Some(bad) = m.keys().find(|&&k| k == 0 || k > 0x00FF_FFFF) {
            return Err(format!("unit_aliases: {bad} is not a 24-bit radio id"));
        }
        out.unit_aliases = clean_aliases(&m);
        changed.unit_aliases = out.unit_aliases != base.unit_aliases;
    }
    if let Some(list) = patch.monitor_tgs {
        if list.len() > MAX_ENTRIES {
            return Err(format!("monitor_tgs: {} entries (max {MAX_ENTRIES})", list.len()));
        }
        if list.contains(&0) {
            return Err("monitor_tgs: talkgroup 0 is not a talkgroup".into());
        }
        // Dedup, keeping the first occurrence (priority order).
        let mut seen = std::collections::HashSet::new();
        out.monitor_tgs = list.into_iter().filter(|t| seen.insert(*t)).collect();
        changed.monitor_tgs = out.monitor_tgs != base.monitor_tgs;
    }
    Ok((out, changed))
}

/// Parse a settings file body. Unknown fields are ignored, missing ones
/// default; invalid values are clamped the same way `apply_patch`
/// would reject them, so a hand edit can never wedge the recorder.
pub fn parse_settings(body: &[u8]) -> Result<UiSettings, String> {
    let mut s: UiSettings = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    s.recording.max_count = s.recording.max_count.clamp(1, MAX_RECORDINGS_LIMIT);
    s.recording.sd_max_count = s.recording.sd_max_count.clamp(1, SD_MAX_COUNT_LIMIT);
    s.recording.sd_max_mb = s.recording.sd_max_mb.clamp(SD_MAX_MB_MIN, SD_MAX_MB_LIMIT);
    s.call.hang_ms = s.call.hang_ms.clamp(HANG_MS_MIN, HANG_MS_MAX);
    s.call.end_grace_ms = s.call.end_grace_ms.min(END_GRACE_MS_MAX);
    s.tg_aliases = clean_aliases(&s.tg_aliases);
    s.tg_aliases.remove(&0);
    s.unit_aliases = clean_aliases(&s.unit_aliases);
    s.unit_aliases.retain(|k, _| *k != 0 && *k <= 0x00FF_FFFF);
    let mut seen = std::collections::HashSet::new();
    s.monitor_tgs.retain(|t| *t != 0 && seen.insert(*t));
    Ok(s)
}

/// Atomic replace: write `<file>.tmp`, then rename over the target.
pub fn write_atomic(path: &Path, s: &UiSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
        }
    }
    let body = serde_json::to_vec_pretty(s).map_err(|e| format!("serialize: {e}"))?;
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, &body).map_err(|e| format!("write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {}: {e}", tmp.display(), path.display()))
}

/// Recording switches the recorder reads on every call open / finalise.
#[derive(Debug)]
pub struct RecordingPolicy {
    enabled: AtomicBool,
    max_count: AtomicUsize,
    /// Change 057: 0 = RAM, 1 = SD.
    storage: AtomicU8,
    sd_max_count: AtomicUsize,
    sd_max_mb: AtomicU64,
    skipped: Mutex<VecDeque<u64>>,
}

/// Change 057: retention limits of both stores at one instant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    pub ram_max_count: usize,
    pub sd_max_count: usize,
    pub sd_max_bytes: u64,
}

impl Default for Retention {
    fn default() -> Self {
        let d = RecordingSettings::default();
        Retention {
            ram_max_count: d.max_count,
            sd_max_count: d.sd_max_count,
            sd_max_bytes: d.sd_max_mb * 1024 * 1024,
        }
    }
}

impl RecordingPolicy {
    pub fn new(s: &RecordingSettings) -> Self {
        let p = RecordingPolicy {
            enabled: AtomicBool::new(s.enabled),
            max_count: AtomicUsize::new(s.max_count),
            storage: AtomicU8::new(0),
            sd_max_count: AtomicUsize::new(s.sd_max_count),
            sd_max_mb: AtomicU64::new(s.sd_max_mb),
            skipped: Mutex::new(VecDeque::new()),
        };
        p.set(s);
        p
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn max_count(&self) -> usize {
        self.max_count.load(Ordering::Relaxed).max(1)
    }

    /// Change 057: the store selected for new recordings.
    pub fn storage(&self) -> StorageKind {
        if self.storage.load(Ordering::Relaxed) == 1 {
            StorageKind::Sd
        } else {
            StorageKind::Ram
        }
    }

    /// Change 057: both stores' retention limits.
    pub fn retention(&self) -> Retention {
        Retention {
            ram_max_count: self.max_count(),
            sd_max_count: self.sd_max_count.load(Ordering::Relaxed).max(1),
            sd_max_bytes: self.sd_max_mb.load(Ordering::Relaxed).max(1) * 1024 * 1024,
        }
    }

    fn set(&self, s: &RecordingSettings) {
        self.enabled.store(s.enabled, Ordering::Relaxed);
        self.max_count.store(s.max_count, Ordering::Relaxed);
        self.storage.store(u8::from(s.storage == StorageKind::Sd), Ordering::Relaxed);
        self.sd_max_count.store(s.sd_max_count, Ordering::Relaxed);
        self.sd_max_mb.store(s.sd_max_mb, Ordering::Relaxed);
    }

    /// Recorder: remember a call that was not recorded because
    /// recording was off.
    pub fn note_skipped(&self, call_id: u64) {
        if let Ok(mut q) = self.skipped.lock() {
            if q.len() >= SKIPPED_IDS_KEEP {
                q.pop_front();
            }
            q.push_back(call_id);
        }
    }

    pub fn was_skipped(&self, call_id: u64) -> bool {
        self.skipped
            .lock()
            .map(|q| q.contains(&call_id))
            .unwrap_or(false)
    }
}

/// Change 057: call close timing the lifecycle reads on every tick.
#[derive(Debug)]
pub struct CallPolicy {
    hang_ms: AtomicU64,
    end_grace_ms: AtomicU64,
}

impl Default for CallPolicy {
    fn default() -> Self {
        CallPolicy::new(&CallSettings::default())
    }
}

impl CallPolicy {
    pub fn new(s: &CallSettings) -> Self {
        CallPolicy {
            hang_ms: AtomicU64::new(s.hang_ms),
            end_grace_ms: AtomicU64::new(s.end_grace_ms),
        }
    }

    /// Close a call with no keep-alive for this long.
    pub fn hang_ms(&self) -> u64 {
        self.hang_ms.load(Ordering::Relaxed)
    }

    /// Close a call this long after its end-of-transmission marker.
    pub fn end_grace_ms(&self) -> u64 {
        self.end_grace_ms.load(Ordering::Relaxed)
    }

    fn set(&self, s: &CallSettings) {
        self.hang_ms.store(s.hang_ms, Ordering::Relaxed);
        self.end_grace_ms.store(s.end_grace_ms, Ordering::Relaxed);
    }
}

/// Result of a successful `SettingsStore::update`.
#[derive(Debug, Clone)]
pub struct UpdateOutcome {
    pub settings: UiSettings,
    pub changed: Changed,
    /// True when the file on disk now matches `settings`.
    pub persisted: bool,
    /// Save error (the live settings were still applied).
    pub save_error: Option<String>,
}

/// Owner of the settings document.
#[derive(Debug)]
pub struct SettingsStore {
    path: Option<PathBuf>,
    current: RwLock<UiSettings>,
    rev: AtomicU64,
    /// How the boot load went ("loaded", "defaults: no file", ...).
    load_note: String,
    last_save_error: Mutex<Option<String>>,
    pub recording: Arc<RecordingPolicy>,
    /// Change 057.
    pub call: Arc<CallPolicy>,
}

impl SettingsStore {
    /// `P25_UI_SETTINGS_FILE` overrides the path (tests, bench); on the
    /// host build there is no default file (in-memory only).
    pub fn default_path() -> Option<PathBuf> {
        if let Ok(p) = std::env::var("P25_UI_SETTINGS_FILE") {
            if !p.is_empty() {
                return Some(PathBuf::from(p));
            }
        }
        if cfg!(target_os = "linux") {
            Some(PathBuf::from(SETTINGS_FILE))
        } else {
            None
        }
    }

    /// Load from `path` (defaults when absent or unreadable).
    pub fn load(path: Option<PathBuf>) -> Self {
        let (settings, load_note) = match path.as_deref() {
            None => (UiSettings::default(), "defaults: no settings file (in-memory only)".to_string()),
            Some(p) if !p.exists() => (UiSettings::default(), format!("defaults: {} not present", p.display())),
            Some(p) => match std::fs::read(p).map_err(|e| e.to_string()).and_then(|b| parse_settings(&b)) {
                Ok(s) => (s, format!("loaded {}", p.display())),
                Err(e) => (UiSettings::default(), format!("defaults: {} unreadable ({e})", p.display())),
            },
        };
        SettingsStore {
            path,
            recording: Arc::new(RecordingPolicy::new(&settings.recording)),
            call: Arc::new(CallPolicy::new(&settings.call)),
            current: RwLock::new(settings),
            rev: AtomicU64::new(1),
            load_note,
            last_save_error: Mutex::new(None),
        }
    }

    pub fn snapshot(&self) -> UiSettings {
        self.current.read().map(|g| g.clone()).unwrap_or_default()
    }

    pub fn rev(&self) -> u64 {
        self.rev.load(Ordering::Relaxed)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn load_note(&self) -> &str {
        &self.load_note
    }

    pub fn last_save_error(&self) -> Option<String> {
        self.last_save_error.lock().ok().and_then(|g| g.clone())
    }

    /// Validate and apply `patch`, update the live recording policy and
    /// persist. A validation error changes nothing; a save error keeps
    /// the (applied) live settings and is reported in the outcome.
    pub fn update(&self, patch: SettingsPatch) -> Result<UpdateOutcome, String> {
        let mut guard = self.current.write().map_err(|_| "settings lock poisoned".to_string())?;
        let (next, changed) = apply_patch(&guard, patch)?;
        if !changed.any() {
            return Ok(UpdateOutcome {
                settings: next,
                changed,
                persisted: self.path.is_some() && self.last_save_error().is_none(),
                save_error: None,
            });
        }
        *guard = next.clone();
        self.recording.set(&next.recording);
        self.call.set(&next.call);
        self.rev.fetch_add(1, Ordering::Relaxed);
        let save_error = match self.path.as_deref() {
            Some(p) => write_atomic(p, &next).err(),
            None => None,
        };
        if let Ok(mut e) = self.last_save_error.lock() {
            *e = save_error.clone();
        }
        Ok(UpdateOutcome {
            settings: next,
            changed,
            persisted: self.path.is_some() && save_error.is_none(),
            save_error,
        })
    }
}

#[cfg(test)]
#[path = "ui_settings_tests.rs"]
mod tests;

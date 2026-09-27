//! Change 056: operator settings the web UI edits and that must survive
//! a reboot — call-recording policy, talkgroup and radio-unit aliases,
//! and the talkgroup monitor list.
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
//!   - aliases and the monitor list: copied into the decoders /
//!     `MonitorList` by the HTTP handlers whenever they change (see
//!     `httpd::api::ui` and `httpd::api::talkgroups`).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

/// On-target settings file (persistent JFFS2 partition).
pub const SETTINGS_FILE: &str = "/mnt/jffs2/p25-ui-settings.json";

/// Default recording retention: the pre-056 fixed ring size
/// (`audio::recorder::MAX_RECORDINGS`).
pub const DEFAULT_MAX_RECORDINGS: usize = 40;

/// Upper bound on the recording retention. Recordings live in tmpfs
/// (`/tmp`, ~490 MB free on the board); a 30 s call is ~480 KB, so
/// 500 worst-case long calls stay well inside it.
pub const MAX_RECORDINGS_LIMIT: usize = 500;

/// Alias display names are trimmed and capped at this many characters.
pub const MAX_ALIAS_CHARS: usize = 48;

/// Cap on the number of entries in each alias map and the monitor list.
pub const MAX_ENTRIES: usize = 4000;

/// Call ids the recorder skipped because recording was off, kept so the
/// call list can say "not recorded" instead of "missing".
const SKIPPED_IDS_KEEP: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingSettings {
    /// Write a WAV for every followed call.
    pub enabled: bool,
    /// Number of recordings kept; the oldest is deleted beyond this.
    pub max_count: usize,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        RecordingSettings {
            enabled: true,
            max_count: DEFAULT_MAX_RECORDINGS,
        }
    }
}

/// The persisted document.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub recording: RecordingSettings,
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
    pub tg_aliases: Option<BTreeMap<u16, String>>,
    pub unit_aliases: Option<BTreeMap<u32, String>>,
    pub monitor_tgs: Option<Vec<u16>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordingPatch {
    pub enabled: Option<bool>,
    pub max_count: Option<usize>,
}

/// What a patch touched, so the caller only re-applies the live state
/// that changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changed {
    pub recording: bool,
    pub tg_aliases: bool,
    pub unit_aliases: bool,
    pub monitor_tgs: bool,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.recording || self.tg_aliases || self.unit_aliases || self.monitor_tgs
    }
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
            if !(1..=MAX_RECORDINGS_LIMIT).contains(&n) {
                return Err(format!(
                    "recording.max_count {n} out of range 1..={MAX_RECORDINGS_LIMIT}"
                ));
            }
            out.recording.max_count = n;
        }
        if let Some(e) = r.enabled {
            out.recording.enabled = e;
        }
        changed.recording = out.recording != base.recording;
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
    skipped: Mutex<VecDeque<u64>>,
}

impl RecordingPolicy {
    pub fn new(s: &RecordingSettings) -> Self {
        RecordingPolicy {
            enabled: AtomicBool::new(s.enabled),
            max_count: AtomicUsize::new(s.max_count),
            skipped: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn max_count(&self) -> usize {
        self.max_count.load(Ordering::Relaxed).max(1)
    }

    fn set(&self, s: &RecordingSettings) {
        self.enabled.store(s.enabled, Ordering::Relaxed);
        self.max_count.store(s.max_count, Ordering::Relaxed);
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

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

/// AD9361 gain control modes accepted by `/api/rx_gain`.
pub const GAIN_MODES: [&str; 4] = ["manual", "slow_attack", "fast_attack", "hybrid"];
/// AD9361 manual RX gain range in dB (`/api/rx_gain?db=`).
pub const GAIN_DB_MIN: i32 = -3;
pub const GAIN_DB_MAX: i32 = 76;

/// RX gain as last set through `/api/rx_gain` (the Radio view's AGC
/// switch and gain selector), applied at startup after the
/// `--hardwaregain` default. `None` = never set: the CLI default stands.
/// Bench 2026-09-27 on the site antenna: the boot default, manual 60 dB,
/// left the control channel at about 65 at the LSM input, below core
/// 0.2.0's no-signal gate (256), while `slow_attack` (73 dB) gave 83 %
/// TSBK decode. The operator's choice now survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioSettings {
    pub gain_mode: Option<String>,
    pub manual_gain_db: Option<i32>,
}

/// Change 067: where the board clock comes from (the radio has no
/// battery-backed clock and starts at 1970).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClockSource {
    /// The control channel's time broadcast (SYNC_BCST), as the site's
    /// radios use: works in the field without internet.
    #[default]
    Site,
    /// Internet time servers (NTP) at start and hourly.
    Ntp,
    /// Only when set by hand (a browser's "set from this device").
    Manual,
}

impl ClockSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockSource::Site => "site",
            ClockSource::Ntp => "ntp",
            ClockSource::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ClockSettings {
    pub source: ClockSource,
}

/// Change 063: talkgroup groups (e.g. "Primary" = 300, "TAC" = 301-310).
/// The order of the list is the priority order (first = highest).
pub const MAX_GROUPS: usize = 32;
pub const GROUP_NAME_CHARS: usize = 32;
pub const MAX_GROUP_TGS: usize = 2000;

/// Change 075a: the largest talkgroup id: 24 bits (DMR Tier III; P25
/// talkgroups are 16-bit). 0 is not a talkgroup.
pub const MAX_TG: u32 = 0x00FF_FFFF;

/// Change 075a: `tg` is a talkgroup id (1..=`MAX_TG`).
pub fn is_tg(tg: u32) -> bool {
    tg != 0 && tg <= MAX_TG
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TgGroup {
    pub name: String,
    pub tgs: Vec<u32>,
}

/// Change 063: speaker side of a group, or of the ungrouped talkgroups.
/// `Off` = not followed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    #[default]
    Both,
    Left,
    Right,
    Off,
}

/// Change 063: which groups play on which speaker (by name). A group on
/// neither side is not followed; `other` covers talkgroups in no group.
/// `preempt`: a grant of a higher-priority group takes the traffic
/// chain from a call of a lower one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Speakers {
    pub left: Vec<String>,
    pub right: Vec<String>,
    pub other: Side,
    pub preempt: bool,
}

impl Default for Speakers {
    fn default() -> Self {
        Speakers { left: Vec::new(), right: Vec::new(), other: Side::Both, preempt: true }
    }
}

/// The persisted document.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub recording: RecordingSettings,
    /// Change 057.
    pub call: CallSettings,
    /// RX gain (see [`RadioSettings`]).
    pub radio: RadioSettings,
    /// Change 063: talkgroup groups, in priority order.
    pub tg_groups: Vec<TgGroup>,
    /// Change 063: group -> speaker routing and priority pre-emption.
    pub speakers: Speakers,
    /// Talkgroup id -> display name.
    pub tg_aliases: BTreeMap<u32, String>,
    /// Radio unit id (source) -> display name.
    pub unit_aliases: BTreeMap<u32, String>,
    /// Talkgroup monitor list (empty = follow every clear grant).
    /// Order is priority order, as in `services::monitor::MonitorList`.
    pub monitor_tgs: Vec<u32>,
    /// Change 067: board clock source.
    pub clock: ClockSettings,
    /// Change 068: talkgroups never followed (the opposite of the monitor
    /// list; wins over it and over the speaker groups). Sorted.
    pub ignore_tgs: Vec<u32>,
    /// Change 069: the site whose settings are live in the fields above
    /// (`services::sites` name, e.g. "clay").
    pub site: String,
    /// Change 069: every site's names and profiles. The live site's entry
    /// is kept in step with the fields above on every change.
    pub sites: BTreeMap<String, SiteSettings>,
}

/// Change 069: one named setup of which talkgroups are followed and
/// where they play (e.g. "Fire", "Everything" for one site).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub name: String,
    pub tg_groups: Vec<TgGroup>,
    pub speakers: Speakers,
    pub monitor_tgs: Vec<u32>,
    pub ignore_tgs: Vec<u32>,
}

/// Change 069: what belongs to one site: its talkgroup and radio names
/// (numbers mean different things on different systems) and its
/// profiles.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SiteSettings {
    pub tg_aliases: BTreeMap<u32, String>,
    pub unit_aliases: BTreeMap<u32, String>,
    pub profiles: Vec<Profile>,
    pub active_profile: String,
}

impl UiSettings {
    /// Change 073: a site's talkgroup and radio names (the live ones for
    /// the active site, else the ones stored for it).
    pub fn aliases_for(&self, site: &str) -> (BTreeMap<u32, String>, BTreeMap<u32, String>) {
        if self.site == site {
            return (self.tg_aliases.clone(), self.unit_aliases.clone());
        }
        self.sites
            .get(site)
            .map(|e| (e.tg_aliases.clone(), e.unit_aliases.clone()))
            .unwrap_or_default()
    }
}

/// Change 069: name of the profile a site starts with.
pub const DEFAULT_PROFILE: &str = "Default";
pub const MAX_PROFILES: usize = 32;
pub const PROFILE_NAME_CHARS: usize = 32;

/// Change 069: a profile action on the live site (`PUT
/// /api/ui/settings` `{"profile": {...}}`, alone in its patch).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ProfileAction {
    /// Make this profile live.
    Select(String),
    /// Add a profile and make it live: a copy of the live one, or empty.
    Create { name: String, #[serde(default)] copy: bool },
    Rename { from: String, to: String },
    /// Remove a profile (not the last); the first one left goes live if
    /// it was the live one.
    Delete(String),
}

impl UiSettings {
    /// Change 069: the live fields as a profile named `name`.
    fn live_profile(&self, name: &str) -> Profile {
        Profile {
            name: name.to_string(),
            tg_groups: self.tg_groups.clone(),
            speakers: self.speakers.clone(),
            monitor_tgs: self.monitor_tgs.clone(),
            ignore_tgs: self.ignore_tgs.clone(),
        }
    }

    /// Change 069: write the live fields into the live site's entry
    /// (its names and its active profile).
    pub fn store_live(&mut self) {
        let live = self.live_profile("");
        let (tg, unit) = (self.tg_aliases.clone(), self.unit_aliases.clone());
        let entry = self.sites.entry(self.site.clone()).or_default();
        if entry.active_profile.is_empty() {
            entry.active_profile = DEFAULT_PROFILE.to_string();
        }
        let name = entry.active_profile.clone();
        let p = Profile { name: name.clone(), ..live };
        match entry.profiles.iter_mut().find(|x| x.name == name) {
            Some(slot) => *slot = p,
            None => entry.profiles.push(p),
        }
        entry.tg_aliases = tg;
        entry.unit_aliases = unit;
    }

    /// Change 069: make `site`'s profile `profile` (else its active one,
    /// else its first, else a new empty "Default") live.
    fn load_live(&mut self, site: &str, profile: Option<&str>) {
        let entry = self.sites.entry(site.to_string()).or_default();
        if entry.profiles.is_empty() {
            entry.profiles.push(Profile { name: DEFAULT_PROFILE.to_string(), ..Default::default() });
        }
        let want = profile.unwrap_or(&entry.active_profile).to_string();
        let p = entry.profiles.iter().find(|x| x.name == want)
            .unwrap_or(&entry.profiles[0]).clone();
        entry.active_profile = p.name.clone();
        let (tg, unit) = (entry.tg_aliases.clone(), entry.unit_aliases.clone());
        self.site = site.to_string();
        self.tg_groups = p.tg_groups;
        self.speakers = p.speakers;
        self.monitor_tgs = p.monitor_tgs;
        self.ignore_tgs = p.ignore_tgs;
        self.tg_aliases = tg;
        self.unit_aliases = unit;
    }

    /// Change 069: adopt `site` as the live site. A document without
    /// one (pre-069) files its live settings under `site` as the
    /// "Default" profile; a different stored site is switched away from.
    pub fn switch_site(&mut self, site: &str) {
        if self.site.is_empty() {
            self.site = site.to_string();
            self.store_live();
            return;
        }
        if self.site == site {
            self.store_live();
            return;
        }
        self.store_live();
        self.load_live(site, None);
    }

    /// Change 069: the live site's profile names and the active one.
    pub fn profiles(&self) -> (Vec<String>, String) {
        match self.sites.get(&self.site) {
            Some(e) => (e.profiles.iter().map(|p| p.name.clone()).collect(), e.active_profile.clone()),
            None => (vec![DEFAULT_PROFILE.to_string()], DEFAULT_PROFILE.to_string()),
        }
    }
}

fn clean_profile_name(name: &str) -> Result<String, String> {
    let n: String = name.trim().chars().take(PROFILE_NAME_CHARS).collect();
    if n.is_empty() {
        return Err("profile: a profile needs a name".into());
    }
    Ok(n)
}

/// Change 069: apply a profile action to `out` (live site).
fn apply_profile_action(out: &mut UiSettings, action: ProfileAction) -> Result<(), String> {
    out.store_live();
    let site = out.site.clone();
    let entry = out.sites.entry(site.clone()).or_default();
    let exists = |e: &SiteSettings, n: &str| e.profiles.iter().any(|p| p.name.eq_ignore_ascii_case(n));
    match action {
        ProfileAction::Select(name) => {
            let Some(p) = entry.profiles.iter().find(|p| p.name.eq_ignore_ascii_case(name.trim())) else {
                return Err(format!("profile: no profile {name:?}"));
            };
            let name = p.name.clone();
            out.load_live(&site, Some(&name));
        }
        ProfileAction::Create { name, copy } => {
            let name = clean_profile_name(&name)?;
            if exists(entry, &name) {
                return Err(format!("profile: {name:?} already exists"));
            }
            if entry.profiles.len() >= MAX_PROFILES {
                return Err(format!("profile: at most {MAX_PROFILES} per site"));
            }
            let p = if copy {
                Profile { name: name.clone(), ..out_live_copy(entry) }
            } else {
                Profile { name: name.clone(), ..Default::default() }
            };
            entry.profiles.push(p);
            out.load_live(&site, Some(&name));
        }
        ProfileAction::Rename { from, to } => {
            let to = clean_profile_name(&to)?;
            if !to.eq_ignore_ascii_case(&from) && exists(entry, &to) {
                return Err(format!("profile: {to:?} already exists"));
            }
            let Some(p) = entry.profiles.iter_mut().find(|p| p.name.eq_ignore_ascii_case(from.trim())) else {
                return Err(format!("profile: no profile {from:?}"));
            };
            let was_active = p.name == entry.active_profile;
            p.name = to.clone();
            if was_active {
                entry.active_profile = to;
            }
        }
        ProfileAction::Delete(name) => {
            if entry.profiles.len() <= 1 {
                return Err("profile: a site keeps at least one profile".into());
            }
            let Some(i) = entry.profiles.iter().position(|p| p.name.eq_ignore_ascii_case(name.trim())) else {
                return Err(format!("profile: no profile {name:?}"));
            };
            let removed = entry.profiles.remove(i);
            if removed.name == entry.active_profile {
                let first = entry.profiles[0].name.clone();
                out.load_live(&site, Some(&first));
            }
        }
    }
    Ok(())
}

/// The live profile of a site entry (after `store_live`).
fn out_live_copy(entry: &SiteSettings) -> Profile {
    entry.profiles.iter().find(|p| p.name == entry.active_profile).cloned().unwrap_or_default()
}

/// Partial update accepted by `PUT /api/ui/settings`. Maps and lists
/// replace the stored value wholesale; absent fields are unchanged.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsPatch {
    pub recording: Option<RecordingPatch>,
    pub call: Option<CallPatch>,
    pub radio: Option<RadioPatch>,
    /// Change 063: replace the group list / the speaker routing.
    pub tg_groups: Option<Vec<TgGroup>>,
    pub speakers: Option<Speakers>,
    pub tg_aliases: Option<BTreeMap<u32, String>>,
    pub unit_aliases: Option<BTreeMap<u32, String>>,
    pub monitor_tgs: Option<Vec<u32>>,
    /// Change 067.
    pub clock: Option<ClockSettings>,
    /// Change 068.
    pub ignore_tgs: Option<Vec<u32>>,
    /// Change 069: a profile action; must be alone in its patch.
    pub profile: Option<ProfileAction>,
    /// Change 069: make this site's settings live (the site switch in
    /// `POST /api/site`); must be alone in its patch.
    pub switch_site: Option<String>,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RadioPatch {
    pub gain_mode: Option<String>,
    pub manual_gain_db: Option<i32>,
}

/// What a patch touched, so the caller only re-applies the live state
/// that changed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changed {
    pub recording: bool,
    pub call: bool,
    pub radio: bool,
    pub tg_aliases: bool,
    pub unit_aliases: bool,
    pub monitor_tgs: bool,
    pub tg_groups: bool,
    pub speakers: bool,
    pub clock: bool,
    pub ignore_tgs: bool,
    /// Change 069: the profile list or the live site changed.
    pub profiles: bool,
}

impl Changed {
    pub fn any(&self) -> bool {
        self.recording || self.call || self.radio || self.tg_aliases || self.unit_aliases
            || self.monitor_tgs || self.tg_groups || self.speakers || self.clock
            || self.ignore_tgs || self.profiles
    }
}

/// Change 063: validated group list (names trimmed, unique ignoring
/// case; talkgroups 1..=`MAX_TG`, deduplicated in order).
fn clean_groups(groups: Vec<TgGroup>) -> Result<Vec<TgGroup>, String> {
    if groups.len() > MAX_GROUPS {
        return Err(format!("tg_groups: {} groups (max {MAX_GROUPS})", groups.len()));
    }
    let mut out: Vec<TgGroup> = Vec::with_capacity(groups.len());
    for g in groups {
        let name: String = g.name.trim().chars().take(GROUP_NAME_CHARS).collect();
        if name.is_empty() {
            return Err("tg_groups: a group has no name".into());
        }
        if out.iter().any(|o| o.name.eq_ignore_ascii_case(&name)) {
            return Err(format!("tg_groups: two groups are named {name:?}"));
        }
        if g.tgs.len() > MAX_GROUP_TGS {
            return Err(format!("tg_groups: {name}: {} talkgroups (max {MAX_GROUP_TGS})", g.tgs.len()));
        }
        if g.tgs.contains(&0) {
            return Err(format!("tg_groups: {name}: talkgroup 0 is not a talkgroup"));
        }
        if let Some(bad) = g.tgs.iter().find(|&&t| t > MAX_TG) {
            return Err(format!("tg_groups: {name}: {bad} is not a 24-bit talkgroup"));
        }
        let mut seen = std::collections::HashSet::new();
        let tgs = g.tgs.into_iter().filter(|t| seen.insert(*t)).collect();
        out.push(TgGroup { name, tgs });
    }
    Ok(out)
}

/// Change 063: speaker routing against `groups`: names resolved to the
/// groups' spelling, unknown names dropped (a deleted group), each group
/// on one side at most (`strict`: an error; else left wins).
fn clean_speakers(sp: Speakers, groups: &[TgGroup], strict: bool) -> Result<Speakers, String> {
    let resolve = |names: &[String]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for n in names {
            if let Some(g) = groups.iter().find(|g| g.name.eq_ignore_ascii_case(n.trim())) {
                if !out.contains(&g.name) {
                    out.push(g.name.clone());
                }
            }
        }
        out
    };
    let left = resolve(&sp.left);
    let mut right = resolve(&sp.right);
    if let Some(both) = right.iter().find(|n| left.contains(n)) {
        if strict {
            return Err(format!("speakers: group {both:?} is on both sides"));
        }
    }
    right.retain(|n| !left.contains(n));
    Ok(Speakers { left, right, other: sp.other, preempt: sp.preempt })
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

/// Change 068: a talkgroup set, sorted and deduplicated.
fn clean_tg_set(mut list: Vec<u32>) -> Vec<u32> {
    list.sort_unstable();
    list.dedup();
    list
}

/// Validate `patch` against `base` and return the resulting document.
/// Pure (no I/O) so it is host-tested directly.
pub fn apply_patch(
    base: &UiSettings,
    patch: SettingsPatch,
) -> Result<(UiSettings, Changed), String> {
    let mut out = base.clone();
    let mut changed = Changed::default();

    // Change 069: profile and site actions stand alone.
    if patch.profile.is_some() || patch.switch_site.is_some() {
        let others = patch.recording.is_some() || patch.call.is_some() || patch.radio.is_some()
            || patch.tg_groups.is_some() || patch.speakers.is_some() || patch.tg_aliases.is_some()
            || patch.unit_aliases.is_some() || patch.monitor_tgs.is_some() || patch.clock.is_some()
            || patch.ignore_tgs.is_some() || (patch.profile.is_some() && patch.switch_site.is_some());
        if others {
            return Err("profile / switch_site must be alone in a patch".into());
        }
        if let Some(a) = patch.profile {
            apply_profile_action(&mut out, a)?;
        }
        if let Some(site) = patch.switch_site {
            let site = site.trim();
            if site.is_empty() || site.len() > 64 {
                return Err("switch_site: a site name".into());
            }
            out.switch_site(site);
        }
        changed.tg_groups = out.tg_groups != base.tg_groups;
        changed.speakers = out.speakers != base.speakers;
        changed.monitor_tgs = out.monitor_tgs != base.monitor_tgs;
        changed.ignore_tgs = out.ignore_tgs != base.ignore_tgs;
        changed.tg_aliases = out.tg_aliases != base.tg_aliases;
        changed.unit_aliases = out.unit_aliases != base.unit_aliases;
        changed.profiles = out.sites != base.sites || out.site != base.site;
        return Ok((out, changed));
    }

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
    if let Some(r) = patch.radio {
        if let Some(m) = r.gain_mode {
            if !GAIN_MODES.contains(&m.as_str()) {
                return Err(format!("radio.gain_mode {m:?}: expected {}", GAIN_MODES.join("|")));
            }
            out.radio.gain_mode = Some(m);
        }
        if let Some(db) = r.manual_gain_db {
            out.radio.manual_gain_db =
                Some(check_range("radio.manual_gain_db", db, GAIN_DB_MIN, GAIN_DB_MAX)?);
        }
        changed.radio = out.radio != base.radio;
    }
    if let Some(groups) = patch.tg_groups {
        out.tg_groups = clean_groups(groups)?;
        // The routing refers to groups by name: a renamed or deleted
        // group drops out of it.
        out.speakers = clean_speakers(out.speakers.clone(), &out.tg_groups, false)?;
        changed.tg_groups = out.tg_groups != base.tg_groups;
    }
    if let Some(sp) = patch.speakers {
        out.speakers = clean_speakers(sp, &out.tg_groups, true)?;
    }
    changed.speakers = out.speakers != base.speakers;
    if let Some(m) = patch.tg_aliases {
        if m.len() > MAX_ENTRIES {
            return Err(format!("tg_aliases: {} entries (max {MAX_ENTRIES})", m.len()));
        }
        if m.contains_key(&0) {
            return Err("tg_aliases: talkgroup 0 is not a talkgroup".into());
        }
        if let Some(bad) = m.keys().find(|&&k| k > MAX_TG) {
            return Err(format!("tg_aliases: {bad} is not a 24-bit talkgroup"));
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
        if let Some(bad) = list.iter().find(|&&t| t > MAX_TG) {
            return Err(format!("monitor_tgs: {bad} is not a 24-bit talkgroup"));
        }
        // Dedup, keeping the first occurrence (priority order).
        let mut seen = std::collections::HashSet::new();
        out.monitor_tgs = list.into_iter().filter(|t| seen.insert(*t)).collect();
        changed.monitor_tgs = out.monitor_tgs != base.monitor_tgs;
    }
    if let Some(c) = patch.clock {
        out.clock = c;
        changed.clock = out.clock != base.clock;
    }
    if let Some(list) = patch.ignore_tgs {
        if list.len() > MAX_ENTRIES {
            return Err(format!("ignore_tgs: {} entries (max {MAX_ENTRIES})", list.len()));
        }
        if list.contains(&0) {
            return Err("ignore_tgs: talkgroup 0 is not a talkgroup".into());
        }
        if let Some(bad) = list.iter().find(|&&t| t > MAX_TG) {
            return Err(format!("ignore_tgs: {bad} is not a 24-bit talkgroup"));
        }
        out.ignore_tgs = clean_tg_set(list);
        changed.ignore_tgs = out.ignore_tgs != base.ignore_tgs;
    }
    // Change 069: edits of the live fields go to the active profile and
    // the site's names.
    if !out.site.is_empty() {
        out.store_live();
        changed.profiles = out.sites != base.sites;
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
    if s.radio.gain_mode.as_deref().is_some_and(|m| !GAIN_MODES.contains(&m)) {
        s.radio.gain_mode = None;
    }
    if s.radio.manual_gain_db.is_some_and(|db| !(GAIN_DB_MIN..=GAIN_DB_MAX).contains(&db)) {
        s.radio.manual_gain_db = None;
    }
    // Change 063: a hand-edited group list keeps its valid groups.
    let mut groups: Vec<TgGroup> = Vec::new();
    for g in std::mem::take(&mut s.tg_groups).into_iter().take(MAX_GROUPS) {
        let g = TgGroup { tgs: g.tgs.into_iter().filter(|t| is_tg(*t)).take(MAX_GROUP_TGS).collect(), ..g };
        let mut next = groups.clone();
        next.push(g);
        if let Ok(ok) = clean_groups(next) {
            groups = ok;
        }
    }
    s.tg_groups = groups;
    s.speakers = clean_speakers(s.speakers.clone(), &s.tg_groups, false)
        .unwrap_or_default();
    s.tg_aliases = clean_aliases(&s.tg_aliases);
    s.tg_aliases.retain(|k, _| is_tg(*k));
    s.unit_aliases = clean_aliases(&s.unit_aliases);
    s.unit_aliases.retain(|k, _| *k != 0 && *k <= 0x00FF_FFFF);
    let mut seen = std::collections::HashSet::new();
    s.monitor_tgs.retain(|t| is_tg(*t) && seen.insert(*t));
    s.ignore_tgs = clean_tg_set(std::mem::take(&mut s.ignore_tgs).into_iter().filter(|t| is_tg(*t)).take(MAX_ENTRIES).collect());
    // Change 069: stored profiles get the same cleaning; names unique.
    for entry in s.sites.values_mut() {
        let mut names: Vec<String> = Vec::new();
        entry.profiles.retain_mut(|p| {
            let Ok(n) = clean_profile_name(&p.name) else { return false };
            if names.iter().any(|x| x.eq_ignore_ascii_case(&n)) {
                return false;
            }
            names.push(n.clone());
            p.name = n;
            p.tg_groups = clean_groups(std::mem::take(&mut p.tg_groups)).unwrap_or_default();
            p.speakers = clean_speakers(p.speakers.clone(), &p.tg_groups, false).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            p.monitor_tgs.retain(|t| is_tg(*t) && seen.insert(*t));
            p.ignore_tgs = clean_tg_set(std::mem::take(&mut p.ignore_tgs).into_iter().filter(|t| is_tg(*t)).collect());
            true
        });
        entry.profiles.truncate(MAX_PROFILES);
        if !entry.profiles.iter().any(|p| p.name == entry.active_profile) {
            entry.active_profile = entry.profiles.first().map(|p| p.name.clone()).unwrap_or_default();
        }
        entry.tg_aliases = clean_aliases(&entry.tg_aliases);
        entry.unit_aliases = clean_aliases(&entry.unit_aliases);
    }
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

/// Change 063: priority rank of talkgroups in no group (lowest).
pub const OTHER_RANK: u16 = u16::MAX;

/// Change 063: how the follower treats one talkgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub side: Side,
    /// Position of its group in the group list (0 = highest priority);
    /// `OTHER_RANK` for a talkgroup in no group.
    pub rank: u16,
}

/// Change 063: the group routing as the follower reads it (pure, host-
/// tested). With no groups everything is followed at one rank, as
/// before 063.
#[derive(Debug, Clone, Default)]
pub struct Routing {
    by_tg: std::collections::HashMap<u32, Route>,
    other: Side,
    preempt: bool,
    /// Change 068: never followed.
    ignored: std::collections::HashSet<u32>,
}

impl Routing {
    pub fn new(groups: &[TgGroup], sp: &Speakers) -> Self {
        let mut by_tg = std::collections::HashMap::new();
        for (i, g) in groups.iter().enumerate() {
            let side = if sp.left.contains(&g.name) {
                Side::Left
            } else if sp.right.contains(&g.name) {
                Side::Right
            } else {
                Side::Off
            };
            for &tg in &g.tgs {
                // A talkgroup in several groups takes the first (highest).
                by_tg.entry(tg).or_insert(Route { side, rank: i as u16 });
            }
        }
        Routing { by_tg, other: sp.other, preempt: sp.preempt, ignored: Default::default() }
    }

    /// Change 068: with the ignore list.
    pub fn with_ignored(mut self, tgs: &[u32]) -> Self {
        self.ignored = tgs.iter().copied().collect();
        self
    }

    /// Change 068: the talkgroup is on the ignore list.
    pub fn ignored(&self, tg: u32) -> bool {
        self.ignored.contains(&tg)
    }

    /// `None`: not followed (its group is on neither speaker, or it is in
    /// no group and "other talkgroups" is off; change 068: or ignored).
    pub fn route(&self, tg: u32) -> Option<Route> {
        if self.ignored(tg) {
            return None;
        }
        let r = self.by_tg.get(&tg).copied()
            .unwrap_or(Route { side: self.other, rank: OTHER_RANK });
        (r.side != Side::Off).then_some(r)
    }

    /// Should a grant for `new_tg` take the chain from the call on
    /// `active_tg`? Only a strictly higher-priority group pre-empts, and
    /// only with pre-emption on. A call the routing no longer follows
    /// (settings changed mid-call) yields to any followed grant.
    pub fn preempts(&self, new_tg: u32, active_tg: u32) -> bool {
        if !self.preempt || new_tg == active_tg {
            return false;
        }
        match (self.route(new_tg), self.route(active_tg)) {
            (Some(n), Some(a)) => n.rank < a.rank,
            (Some(_), None) => true,
            (None, _) => false,
        }
    }

    /// Change 066: a group with talkgroups plays on `side`.
    pub fn side_has_groups(&self, side: Side) -> bool {
        self.by_tg.values().any(|r| r.side == side)
    }
}

/// Change 063: live [`Routing`] for the grant follower.
#[derive(Debug, Default)]
pub struct RoutingPolicy {
    current: RwLock<Routing>,
}

impl RoutingPolicy {
    pub fn new(s: &UiSettings) -> Self {
        RoutingPolicy { current: RwLock::new(Routing::new(&s.tg_groups, &s.speakers).with_ignored(&s.ignore_tgs)) }
    }

    fn set(&self, s: &UiSettings) {
        if let Ok(mut g) = self.current.write() {
            *g = Routing::new(&s.tg_groups, &s.speakers).with_ignored(&s.ignore_tgs);
        }
    }

    pub fn route(&self, tg: u32) -> Option<Route> {
        self.current.read().ok().and_then(|g| g.route(tg))
    }

    pub fn preempts(&self, new_tg: u32, active_tg: u32) -> bool {
        self.current.read().map(|g| g.preempts(new_tg, active_tg)).unwrap_or(false)
    }

    /// Change 066: the routing as of now (the chain choice reads several
    /// rules consistently).
    pub fn snapshot(&self) -> Routing {
        self.current.read().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Change 067: live clock source for the clock task and the UI.
#[derive(Debug, Default)]
pub struct ClockPolicy {
    source: std::sync::atomic::AtomicU8,
}

impl ClockPolicy {
    pub fn new(s: &ClockSettings) -> Self {
        let p = ClockPolicy::default();
        p.set(s);
        p
    }

    pub fn source(&self) -> ClockSource {
        match self.source.load(Ordering::Relaxed) {
            1 => ClockSource::Ntp,
            2 => ClockSource::Manual,
            _ => ClockSource::Site,
        }
    }

    fn set(&self, s: &ClockSettings) {
        let v = match s.source {
            ClockSource::Site => 0,
            ClockSource::Ntp => 1,
            ClockSource::Manual => 2,
        };
        self.source.store(v, Ordering::Relaxed);
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
    /// Change 063.
    pub routing: Arc<RoutingPolicy>,
    /// Change 067.
    pub clock: Arc<ClockPolicy>,
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
            routing: Arc::new(RoutingPolicy::new(&settings)),
            clock: Arc::new(ClockPolicy::new(&settings.clock)),
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

    /// Change 069: make `site` the live site at start-up (a pre-069 file
    /// files its settings under it; a file saved for another site
    /// switches to this one's). Persists when anything changed.
    pub fn adopt_site(&self, site: &str) -> Result<UpdateOutcome, String> {
        self.update(SettingsPatch { switch_site: Some(site.to_string()), ..Default::default() })
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
        self.routing.set(&next);
        self.clock.set(&next.clock);
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

//! `radio.json`: the hardware and the services around it. Written only when the user changes
//! something.

use serde::{Deserialize, Serialize};

use super::VERSION;
use crate::hardware::presets::find_preset;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioConfig {
    pub version: u32,
    pub gain: Gain,
    /// DDC presets the window planner may choose, narrowest first.
    pub presets_allowed: Vec<String>,
    /// Traffic lanes to run; `None` = every lane the radio core has.
    pub traffic_chains: Option<u8>,
    pub calls: Calls,
    pub recording: Recording,
    pub history: History,
    pub clock: Clock,
    pub crystal: CrystalTracking,
}

/// Space on the card for recordings or the history: between these (1 TB, far beyond a card).
pub const SD_MIN_MB: u64 = 16;
pub const SD_MAX_MB: u64 = 1 << 20;

impl RadioConfig {
    /// Every value in its range (what the settings endpoints each check for their part).
    pub fn check(&self) -> Result<(), String> {
        if self.gain.mode == GainMode::Manual && !self.gain.manual_db.is_some_and(|db| GAIN_DB_RANGE.contains(&db)) {
            return Err(format!("manual gain needs manual_db in {}..={}", GAIN_DB_RANGE.start(), GAIN_DB_RANGE.end()));
        }
        if self.presets_allowed.is_empty() {
            return Err("at least one preset".into());
        }
        if let Some(p) = self.presets_allowed.iter().find(|p| find_preset(p).is_none()) {
            return Err(format!("no preset {p}"));
        }
        if !(500..=30_000).contains(&self.calls.hang_ms) || self.calls.end_grace_ms > 10_000 {
            return Err("hang_ms 500..=30000, end_grace_ms up to 10000".into());
        }
        self.recording.check()?;
        if self.history.retention_days == 0 || !(SD_MIN_MB..=SD_MAX_MB).contains(&self.history.sd_max_mb) {
            return Err(format!("history: at least a day, and {SD_MIN_MB}..={SD_MAX_MB} MB"));
        }
        if self.crystal.anchor_hz > 1_000 {
            return Err("crystal anchor_hz up to 1000 (0: no limit)".into());
        }
        Ok(())
    }
}

impl Default for RadioConfig {
    fn default() -> Self {
        RadioConfig {
            version: VERSION,
            gain: Gain::default(),
            presets_allowed: ["8M", "12M", "16M"].map(String::from).to_vec(),
            traffic_chains: None,
            calls: Calls::default(),
            recording: Recording::default(),
            history: History::default(),
            clock: Clock::default(),
            crystal: CrystalTracking::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GainMode {
    Manual,
    /// The AD9361 AGC; the decoders do best with it on the site antenna.
    #[default]
    SlowAttack,
    FastAttack,
    Hybrid,
}

pub const GAIN_DB_RANGE: std::ops::RangeInclusive<i32> = -3..=76;

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Gain {
    pub mode: GainMode,
    /// Used in `Manual` mode.
    pub manual_db: Option<i32>,
}

/// When a call closes: `hang_ms` with no sign of life, or `end_grace_ms` after its
/// end-of-transmission marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Calls {
    pub hang_ms: u64,
    pub end_grace_ms: u64,
}

impl Default for Calls {
    fn default() -> Self {
        Calls { hang_ms: 3_000, end_grace_ms: 2_000 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Storage {
    /// RAM disk: lost on reboot.
    Ram,
    #[default]
    Sd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Recording {
    pub enabled: bool,
    /// Every followed call is recorded; off: only those whose alias says record.
    pub every_call: bool,
    pub storage: Storage,
    pub ram_max_count: u32,
    /// Days the card keeps a recording, and a locked one (a call with alert tones, or locked by
    /// hand).
    pub sd_keep_days: u32,
    pub locked_keep_days: u32,
    /// The card's share for recordings: past it the oldest unlocked go.
    pub sd_max_mb: u64,
    /// Recordings in one day folder (FAT32 allows a directory about 8,000 of them).
    pub folder_max_count: u32,
}

/// Days a recording may be kept for.
pub const KEEP_DAYS: std::ops::RangeInclusive<u32> = 1..=3_650;
pub const FOLDER_MAX: std::ops::RangeInclusive<u32> = 100..=6_000;

impl Default for Recording {
    fn default() -> Self {
        Recording {
            enabled: true,
            every_call: true,
            storage: Storage::Sd,
            ram_max_count: 40,
            sd_keep_days: 30,
            locked_keep_days: 365,
            sd_max_mb: 8_192,
            folder_max_count: 4_000,
        }
    }
}

impl Recording {
    pub fn check(&self) -> Result<(), String> {
        if self.ram_max_count == 0 {
            return Err("recording: RAM keeps at least one".into());
        }
        if !KEEP_DAYS.contains(&self.sd_keep_days) || !(self.sd_keep_days..=*KEEP_DAYS.end()).contains(&self.locked_keep_days) {
            return Err(format!("recording: sd_keep_days {}..={}, locked_keep_days from it to {}", KEEP_DAYS.start(), KEEP_DAYS.end(), KEEP_DAYS.end()));
        }
        if !(SD_MIN_MB..=SD_MAX_MB).contains(&self.sd_max_mb) {
            return Err(format!("recording: sd_max_mb {SD_MIN_MB}..={SD_MAX_MB}"));
        }
        if !FOLDER_MAX.contains(&self.folder_max_count) {
            return Err(format!("recording: folder_max_count {}..={}", FOLDER_MAX.start(), FOLDER_MAX.end()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub retention_days: u32,
    pub sd_max_mb: u64,
}

impl Default for History {
    fn default() -> Self {
        History { retention_days: 365, sd_max_mb: 2_048 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// The control channel's time broadcast: works without internet.
    #[default]
    Site,
    Ntp,
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Clock {
    pub source: ClockSource,
}

/// The crystal tracker (`services::crystal`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CrystalTracking {
    /// Apply the tracker's estimates (off: shown only).
    pub tracking: bool,
    /// How far the tracker may move from this run's calibration (0: no limit).
    pub anchor_hz: u32,
}

impl Default for CrystalTracking {
    fn default() -> Self {
        CrystalTracking { tracking: true, anchor_hz: 50 }
    }
}

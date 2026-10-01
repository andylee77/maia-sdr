//! `radio.json`: the hardware and the services around it. Written only when the user changes
//! something.

use serde::{Deserialize, Serialize};

use super::VERSION;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioConfig {
    pub version: u32,
    pub gain: Gain,
    /// DDC presets the window planner may choose, narrowest first.
    pub presets_allowed: Vec<String>,
    /// Traffic chains to run; `None` = every chain the gateware has.
    pub traffic_chains: Option<u8>,
    pub calls: Calls,
    pub recording: Recording,
    pub history: History,
    pub clock: Clock,
    pub crystal: CrystalTracking,
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
    pub storage: Storage,
    pub ram_max_count: u32,
    pub sd_max_count: u32,
    pub sd_max_mb: u64,
}

impl Default for Recording {
    fn default() -> Self {
        Recording { enabled: true, storage: Storage::Sd, ram_max_count: 40, sd_max_count: 2_000, sd_max_mb: 2_048 }
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

//! Change 056: wire types of the consolidated web-UI endpoints
//! (`GET /api/ui/state`, `GET /api/ui/calls`).
//!
//! The web UI renders from these two documents instead of stitching
//! call state together from `/api/grants`, `/api/traffic`,
//! `/api/grant_decode_stats` and `/api/recordings`. Every time in
//! here is Unix milliseconds of the BOARD clock; `now_unix_ms` is
//! included so a client can compute ages without trusting its own
//! clock (the board often runs at 1970 until something sets it).

use serde::{Deserialize, Serialize};

/// `GET /api/ui/state` — small (~1–2 KB) snapshot meant for a 1 Hz poll.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiState {
    /// Schema version of this document.
    pub v: u32,
    pub build: String,
    /// Board wall clock (Unix ms) at the time of the snapshot.
    pub now_unix_ms: u64,
    /// False while the board clock is before 2020 (never set). Wall
    /// times are then only meaningful relative to `now_unix_ms`.
    pub clock_valid: bool,
    pub uptime_s: u64,
    pub site: UiSite,
    /// The call the lifecycle currently holds open, `None` when idle.
    /// Change 066: traffic chain 1's (see `calls`).
    pub call: Option<UiCall>,
    /// Change 066: traffic chain 1 (see `chains`).
    pub chain: UiChain,
    /// Change 066: the open call of every traffic chain, chain 1 first.
    #[serde(default)]
    pub calls: Vec<UiCall>,
    /// Change 066: every traffic chain that runs, chain 1 first.
    #[serde(default)]
    pub chains: Vec<UiChain>,
    pub recording: UiRecordingStatus,
    pub audio: UiAudio,
    /// Changes whenever the recent-calls list or the recordings ring
    /// changes; refetch `/api/ui/calls` when it differs.
    pub calls_rev: String,
    /// Incremented on every settings change (`/api/ui/settings`).
    pub settings_rev: u64,
    /// Newest event-log sequence number (`/api/log?since=`).
    pub log_last_seq: u64,
}

/// Control-channel / site health.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSite {
    /// Active site baseline (`/api/sites`), e.g. "clay".
    pub name: Option<String>,
    pub label: Option<String>,
    pub nac: Option<String>,
    pub wacn: Option<String>,
    pub system_id: Option<String>,
    pub rfss_id: Option<u8>,
    pub site_id: Option<u8>,
    /// Operator-tuned control-channel frequency.
    pub cc_freq_hz: u64,
    /// "LSM" / "C4FM" / "Auto".
    pub modulation: String,
    /// True once the control channel has identified the system (WACN
    /// decoded).
    pub acquired: bool,
    /// Age of the newest decoded TSBK, `None` if none yet.
    pub last_tsbk_age_ms: Option<u64>,
    /// TSBKs with a good CRC per second over the last ~10 s.
    pub tsbk_per_s: Option<f64>,
    /// CRC-ok share of TSBK blocks over the same window, percent.
    pub tsbk_ok_pct: Option<f64>,
    /// Summary: "ok" (TSBKs flowing), "stale" (acquired but no TSBK for
    /// > 5 s), "searching" (not acquired).
    pub health: String,
    /// Change 067: the site's time from its control channel (SYNC_BCST),
    /// `None` until one is decoded.
    #[serde(default)]
    pub site_time: Option<UiSiteTime>,
    /// Change 067: where the board clock comes from: "site", "ntp" or
    /// "manual".
    #[serde(default)]
    pub clock_source: String,
}

/// Change 067: the site time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSiteTime {
    /// Site time (UTC unix ms) at the snapshot.
    pub unix_ms: u64,
    /// "precise" (locked micro-slots), "second" (from a minute
    /// rollover) or "minute" (±30 s).
    pub precision: String,
    /// The site clock is locked to an external reference (GPS).
    pub ext_locked: bool,
    /// Local time offset from UTC the site announces, minutes.
    pub local_offset_min: Option<i16>,
    /// Site time minus board clock, ms.
    pub board_offset_ms: i64,
    /// Age of the newest time broadcast, ms.
    pub age_ms: u64,
}

/// The call held open by the lifecycle (`app::grant_follower`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCall {
    pub call_id: u64,
    pub tg: u32,
    pub tg_alias: Option<String>,
    /// Primary source (CC grant SRC, else first voted LDU1 LC source).
    pub source: Option<u32>,
    pub source_alias: Option<String>,
    /// Every source seen in this call, in order.
    pub sources: Vec<u32>,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub encrypted: bool,
    pub started_unix_ms: u64,
    pub elapsed_ms: u64,
    /// "acquiring" (granted, no voice yet), "voice" (voice within the
    /// last 1.5 s) or "hang" (no voice; the lifecycle closes the call
    /// `close_in_ms` from now unless voice or a CC update arrives).
    /// Change 057: always "hang" once the end of the transmission was
    /// decoded (`close_via` "end").
    pub phase: String,
    /// Decoded voice of this call received so far (20 ms per frame).
    pub voice_ms: u64,
    pub first_voice_unix_ms: Option<u64>,
    pub last_voice_unix_ms: Option<u64>,
    pub close_in_ms: u64,
    /// Change 057: which close is pending: "end" (end-of-transmission
    /// marker decoded; closes `close_in_ms` from now unless voice
    /// resumes, CC updates do not extend it) or "timeout" (no
    /// keep-alive). `close_window_ms` is that rule's full length (the
    /// persisted `call.end_grace_ms` / `call.hang_ms`), for a countdown.
    #[serde(default)]
    pub close_via: String,
    #[serde(default)]
    pub close_window_ms: u64,
    /// Change 057: LC of the end-of-transmission marker, e.g.
    /// "talk_complete", "channel_user", "call_termination".
    #[serde(default)]
    pub end_lc: Option<String>,
    /// A WAV is being written for this call.
    pub recording: bool,
    /// Change 066: traffic chain (1 or 2; 0 = unknown).
    #[serde(default)]
    pub chain: u8,
}

/// Traffic chain as the follower sees it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiChain {
    /// "Idle" / "Acquiring" / "Active".
    pub state: String,
    /// Physical frequency the traffic DDC is parked on (kept between
    /// calls).
    pub parked_freq_hz: Option<u64>,
    pub follower_enabled: bool,
    /// Diagnostic lock (`/api/traffic?lock=on`) is holding the chain.
    pub lock_freq: bool,
    /// Traffic dibit reader mode ("airtime" / "poll" / "legacy").
    pub delivery_mode: String,
    /// Change 066: chain number (1 or 2; 0 = unknown).
    #[serde(default)]
    pub number: u8,
    /// Change 066: talkgroup the chain follows.
    #[serde(default)]
    pub tg: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiRecordingStatus {
    pub enabled: bool,
    /// RAM-store retention (`recording.max_count`).
    pub max_count: usize,
    /// Recordings listed, both stores.
    pub count: usize,
    /// Change 057: store selected for new recordings ("ram" / "sd").
    #[serde(default)]
    pub storage: String,
    /// Change 057: SD card state when it is selected or holds
    /// recordings ("ok", "absent", "read_only", "full", "error",
    /// "unknown"); `None` otherwise.
    #[serde(default)]
    pub sd_state: Option<String>,
    /// Change 057: recordings on the SD card / in RAM.
    #[serde(default)]
    pub sd_count: usize,
    #[serde(default)]
    pub ram_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiAudio {
    /// Browsers connected to `/ws/audio` right now.
    pub listeners: usize,
    /// Cumulative `/ws/audio` broadcast overruns since boot.
    pub lag_total: u64,
}

/// `GET /api/ui/calls` — recent calls, newest first, joined with
/// their recordings by call_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCalls {
    pub now_unix_ms: u64,
    pub calls_rev: String,
    pub recording_enabled: bool,
    pub items: Vec<UiCallSummary>,
    /// Change 073: the site listed (`None`: every site).
    #[serde(default)]
    pub site: Option<String>,
    /// Change 073: the sites with calls or recordings in the lists
    /// (for a selector), active site first.
    #[serde(default)]
    pub sites: Vec<UiSiteCount>,
}

/// Change 073: calls and recordings kept for one site ("" = recorded
/// before sites were kept).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UiSiteCount {
    pub site: String,
    pub label: String,
    pub calls: u64,
    pub recordings: u64,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiCallSummary {
    pub call_id: u64,
    pub tg: u32,
    pub tg_alias: Option<String>,
    pub source: Option<u32>,
    pub source_alias: Option<String>,
    pub sources: Vec<u32>,
    pub freq_hz: Option<u64>,
    pub channel: Option<String>,
    pub started_unix_ms: u64,
    pub ended_unix_ms: u64,
    /// How long the lifecycle held the call open. Includes the time
    /// after the last voice until the close (change 057: the end grace,
    /// or the reply's grant; pre-057 up to 10 s), so it is NOT the talk
    /// time; use `voice_ms`.
    pub open_ms: u64,
    /// Decoded voice: the recording's length when there is one, else
    /// IMBE frames x 20 ms.
    pub voice_ms: u64,
    /// Grant to last CC update for this TG (on-air estimate that does
    /// not depend on decoding).
    pub air_ms: Option<u64>,
    /// Grant to first voice (HDU or audio).
    pub first_voice_ms: Option<u64>,
    pub imbe: u64,
    pub ldu: u64,
    pub vocoder_errors: u64,
    pub vocoder_silent: u64,
    pub encrypted: bool,
    /// Why the follower did not follow this grant (`encrypted`,
    /// `sticky_lock`, `monitor_list`, `traffic_lock`), `None` if
    /// followed.
    pub not_followed: Option<String>,
    pub close_reason: String,
    pub recording: Option<UiRecordingRef>,
    /// "recorded", "saving" (WAV still being finalised), "not_recorded"
    /// (recording was off), "evicted" (rolled out of retention),
    /// "no_voice", "encrypted", "not_followed", "missing".
    pub audio_status: String,
    /// Change 066: traffic chain that followed the call (1 or 2; 0 =
    /// not followed or unknown).
    #[serde(default)]
    pub chain: u8,
    /// Change 073: the site the call was on ("" = unknown).
    #[serde(default)]
    pub site: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiRecordingRef {
    pub id: u64,
    pub url: String,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub filename: String,
    /// Change 057: "ram" or "sd".
    #[serde(default)]
    pub storage: String,
}

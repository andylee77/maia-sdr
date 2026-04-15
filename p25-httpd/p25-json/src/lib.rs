//! Fishball P25 JSON API types
//!
//! Shared types for the REST API and WebSocket events.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// P25 system identity information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub nac: Option<String>,
    pub wacn: Option<String>,
    pub system_id: Option<String>,
    pub rfss_id: Option<u8>,
    pub site_id: Option<u8>,
    pub lra: Option<u8>,
    pub control_channel: Option<String>,
    /// Phase 6F.11: backup primary control channel A from Secondary
    /// Control Channel Broadcast (TSBK opcode 0x39).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_cch_a: Option<String>,
    /// Phase 6F.11: backup primary control channel B.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_cch_b: Option<String>,
    /// Phase 6F.11: SNDCP downlink data channel from
    /// SNDCP_DCH_ANN_EX (TSBK opcode 0x16).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sndcp_downlink_channel: Option<String>,
    /// Phase 6F.11: SNDCP uplink data channel.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sndcp_uplink_channel: Option<String>,
    /// Phase 6F.11: most-recent system clock from TDMA_SYNC_BCST
    /// (opcode 0x30). ISO-8601 string + lock state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_clock: Option<String>,
    /// Build tag of the running p25-httpd binary. Lets the browser
    /// verify that the deployed binary is the one that was just built
    /// (Buildroot zeros file mtimes, so on-target file timestamps are
    /// useless for this check). Bumped on every feature-flag change.
    #[serde(default)]
    pub build: Option<String>,
}

/// Active voice channel grant
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelGrant {
    pub channel: String,
    pub talkgroup: u16,
    pub talkgroup_alias: Option<String>,
    pub source: Option<u32>,
    pub frequency_mhz: Option<f64>,
    pub age_secs: u64,
    /// Phase 7C: encryption flag from the GroupVoiceChannelGrant
    /// service options byte (mask 0x40). Defaults to false on
    /// `Deserialize` for forward-compat with older p25-httpd
    /// builds that don't surface this field.
    #[serde(default)]
    pub encrypted: bool,
    /// Phase 7C: emergency flag from the same service options byte
    /// (mask 0x80). Same forward-compat default.
    #[serde(default)]
    pub emergency: bool,
    /// Phase 7F.4 (2026-04-14): true if this TG has ever been
    /// observed with `encrypted=true` on any grant since boot.
    /// Computed from the follower's `encrypted_tg_history` HashSet.
    /// Lets the dashboard render an [ENC-HIST] badge on active
    /// grant rows whose latest TSBK lacked service options but
    /// whose TG is a known-encrypted one.
    #[serde(default)]
    pub in_encrypted_history: bool,
}

/// Frequency band info (from IDEN_UP)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BandInfo {
    pub identifier: u8,
    pub base_frequency_mhz: f64,
    pub channel_spacing_khz: f64,
    pub transmit_offset_mhz: f64,
    pub bandwidth_khz: f64,
}

/// Decoder and FPGA statistics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecoderStats {
    pub recent_messages: usize,
    pub active_grants: usize,
    pub bands_known: usize,
    pub system_acquired: bool,
    pub dibit_count: u32,
    pub overflow: bool,
    pub dma_next_address: u32,
    /// AD9361 RX hardware gain in dB (current AGC value, or None on read error).
    /// Slow-attack AGC parks high (~70-76 dB) on weak signals, low (~10-30) on strong.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_gain_db: Option<f64>,
    /// AD9361 RSSI in dB (relative scale). For 800 MHz P25, ~100-110 dB is
    /// the normal reception range we observed on this site (verified
    /// against PlutoSDR + SDRTrunk on the same antenna).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_rssi_db: Option<f64>,
    /// AD9361 RX LO frequency in Hz. Fixed at boot — the P25 follower
    /// retunes via DDC NCO, not by moving the analog LO.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_lo_hz: Option<u64>,
    /// AD9361 analog RX bandwidth in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rf_bandwidth_hz: Option<u32>,
    /// AD9361 ADC sampling frequency in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sampling_frequency_hz: Option<u32>,
    /// AD9361 gain control mode string ("manual" / "fast_attack" / ...).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain_control_mode: Option<String>,
    /// Signed control-channel offset from RX LO in Hz, i.e. the NCO
    /// offset the control-side DDC runs at. Computed from the boot-time
    /// control_freq / rx_lo / lo_ppm triple.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddc_control_offset_hz: Option<i64>,
    /// Human-readable DDC decimation chain (e.g. "/4 /4 /8 = /128").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddc_decimation: Option<String>,
    /// DDC output rate after all decimation stages, in Hz.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ddc_output_rate_hz: Option<u32>,
    /// Wall-clock time formatted for display. This is the Linux clock,
    /// which is only meaningful once NTP has synced at boot. Pre-NTP
    /// the value is "1970-..." and the operator should treat any
    /// event-log timestamp older than the first grant as suspect.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall_clock: Option<String>,
    /// Boot-relative uptime in seconds for the p25-httpd process.
    /// Always increasing, monotonic, NTP-independent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// Cumulative count of audio WS `Lagged` events since boot — each
    /// increment is one broadcast-channel overrun where the /ws/audio
    /// consumer fell behind and lost chunks. A healthy client on a LAN
    /// should hold this at 0 forever; non-zero means the browser
    /// scheduler or network was too slow for the 50 fps audio stream
    /// and the listener heard a gap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_ws_lag_total: Option<u64>,
    /// Current number of connected /ws/audio subscribers (broadcast
    /// channel receiver_count). 0 = nobody listening.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_ws_clients: Option<usize>,
}

/// Real-time TSBK event (sent over WebSocket)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TsbkEvent {
    /// ISO 8601 timestamp
    pub timestamp: String,
    /// Event type name (e.g., "GRP_GRANT", "NET_STS", "IDEN_UP")
    pub event_type: String,
    /// Human-readable summary
    pub summary: String,
    /// Talkgroup ID (if applicable)
    pub talkgroup: Option<u16>,
    /// Talkgroup alias (if known)
    pub talkgroup_alias: Option<String>,
    /// Channel string (if applicable)
    pub channel: Option<String>,
    /// Frequency in MHz (if applicable)
    pub frequency_mhz: Option<f64>,
    /// Source radio ID (if applicable)
    pub source: Option<u32>,
}

/// Talkgroup alias map: talkgroup_id -> display name
pub type AliasMap = HashMap<u16, String>;

/// DDC configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdcConfig {
    pub frequency_offset: f64,
    pub sample_rate: f64,
    pub decimation: [u32; 3],
}

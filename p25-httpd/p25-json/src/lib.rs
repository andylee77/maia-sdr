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

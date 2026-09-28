//! Types used by the ControlChannelDecoder state machine.
//!
//! Split from control_channel/mod.rs on 2026-04-19. Kept as a child
//! module so private fields remain visible to the parent without
//! `pub(super)` annotations on each one.

use std::time::Instant;

use crate::protocol::p25::types::{Channel, DataUnit, Nac, RadioId, Talkgroup};

/// Decoder state machine
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DecoderState {
    /// Searching for frame sync pattern
    Hunting,
    /// Found sync, reading NID (48 dibits after frame sync)
    ReadingNid { dibits_read: usize, nid_bits: u64 },
    /// Reading data unit payload
    ReadingDataUnit { duid: DataUnit },
}

/// Phase 6F.2h: snapshot of one full TSBK frame as it flows through
/// the LSM software decoder. Captured one-shot via the
/// `/api/control_iq_capture_aligned` endpoint, used for offline replay /
/// pattern analysis when the on-target dashboard counters say "TSBK
/// CRC fails on every frame" but we can't tell which pipeline stage is
/// at fault.
#[derive(Debug, Clone, Default)]
pub struct AlignedCapture {
    /// 24 dibits of the matched sync pattern (the contents of
    /// `sync_register` at the moment of the hit, dumped MSB-first
    /// into a Vec).
    pub sync_dibits: Vec<u8>,
    /// Hamming distance of the sync hit (0..SYNC_THRESHOLD).
    pub sync_distance: u32,
    /// 33 raw NID-window dibits including the in-NID status dibit at
    /// position 11.
    pub raw_nid_dibits: Vec<u8>,
    /// 64-bit `nid_bits` word fed to BCH (status dibit removed,
    /// packed MSB-first per the existing `process_dibit` logic).
    pub nid_bits: u64,
    /// BCH-corrected NAC, or `None` if BCH rejected the word.
    pub bch_nac: Option<u16>,
    /// BCH-corrected DUID, or `None` if BCH rejected the word.
    pub bch_duid: Option<u8>,
    /// Raw on-air DUID nibble before BCH (for diagnostic comparison
    /// against the corrected value).
    pub raw_duid: u8,
    /// 122 raw TSDU body dibits (only populated if BCH succeeded
    /// AND the corrected DUID was TSDU). Empty otherwise.
    pub raw_body_dibits: Vec<u8>,
    /// 98 trellis data dibits after the deinterleaver dropped the 3
    /// status dibits and 21 trailing nulls.
    pub trellis_dibits: Vec<u8>,
    /// 12 trellis-decoded TSBK bytes.
    pub tsbk_bytes: Vec<u8>,
    /// CRC validation result: "plain", "xored", or "fail".
    pub crc_result: String,
    /// `total_dibits` counter at the moment the sync hit fired
    /// (lets us correlate this snapshot with `/api/control_iq_capture`).
    pub total_dibits_at_capture: u64,
}

impl AlignedCapture {
    pub fn to_json(&self) -> serde_json::Value {
        let hex = |v: &[u8]| -> String {
            v.iter().map(|d| format!("{:1X}", d & 0x3)).collect()
        };
        let bytes_hex = |v: &[u8]| -> String {
            v.iter().map(|b| format!("{:02X}", b)).collect()
        };
        serde_json::json!({
            "status": "captured",
            "sync_dibits_hex":     hex(&self.sync_dibits),
            "sync_distance":       self.sync_distance,
            "raw_nid_dibits_hex":  hex(&self.raw_nid_dibits),
            "nid_bits_hex":        format!("{:016X}", self.nid_bits),
            "bch_nac":             self.bch_nac.map(|n| format!("{:03X}", n)),
            "bch_duid":            self.bch_duid.map(|d| format!("{:1X}", d)),
            "raw_duid":            format!("{:1X}", self.raw_duid),
            "raw_body_dibits_hex": hex(&self.raw_body_dibits),
            "trellis_dibits_hex":  hex(&self.trellis_dibits),
            "tsbk_bytes_hex":      bytes_hex(&self.tsbk_bytes),
            "crc_result":          self.crc_result.clone(),
            "total_dibits_at_capture": self.total_dibits_at_capture,
            "note": "Hex strings are 1 char per dibit (low 2 bits). \
                     tsbk_bytes_hex is 2 chars per byte. Replay with \
                     tools/p25_decode_capture.py to compare against the \
                     Python reference Viterbi.",
        })
    }
}

/// Internal builder for an in-flight capture: holds the partial state
/// while we're walking through the NID and body dibits.
#[derive(Debug, Clone, Default)]
pub(super) struct CaptureBuilder {
    pub(super) sync_dibits: Vec<u8>,
    pub(super) sync_distance: u32,
    pub(super) raw_nid_dibits: Vec<u8>,
    pub(super) nid_bits: u64,
    pub(super) bch_nac: Option<u16>,
    pub(super) bch_duid: Option<u8>,
    pub(super) raw_duid: u8,
    pub(super) raw_body_dibits: Vec<u8>,
    pub(super) total_dibits_at_capture: u64,
}

/// System identity from control channel broadcasts
#[derive(Debug, Clone, Default)]
pub struct SystemIdentity {
    pub nac: Option<Nac>,
    pub wacn: Option<u32>,
    pub system_id: Option<u16>,
    pub rfss_id: Option<u8>,
    pub site_id: Option<u8>,
    pub lra: Option<u8>,
    pub control_channel: Option<Channel>,
    /// Phase 6F.11: backup primary control channel "A" announced via
    /// Secondary Control Channel Broadcast (TSBK opcode 0x39). Used by
    /// the trunking failover mechanism in P25.
    pub secondary_cch_a: Option<Channel>,
    /// Phase 6F.11: backup primary control channel "B".
    pub secondary_cch_b: Option<Channel>,
    /// Phase 6F.11: SNDCP downlink data services channel announced
    /// via SNDCP_DCH_ANN_EX (TSBK opcode 0x16). The channel that
    /// carries packet-data subscribers on this site.
    pub sndcp_downlink_channel: Option<Channel>,
    /// Phase 6F.11: SNDCP uplink data services channel.
    pub sndcp_uplink_channel: Option<Channel>,
    /// Phase 6F.11: most-recent system clock from TDMA_SYNC_BCST
    /// (opcode 0x30). Format: `(year, month, day, hours, minutes,
    /// time_locked)`. Updated on every sync broadcast (~5/sec).
    pub last_sync_clock: Option<(u16, u8, u8, u8, u8, bool)>,
    /// Change 067: the site time from every SYNC_BCST (micro-slots,
    /// minute rollovers), for the board clock and the UI.
    pub site_clock: crate::services::site_clock::SiteClock,
    /// 2026-04-16: true if the site has advertised a Phase 2 TDMA
    /// frequency band via Identifier Update TDMA (TSBK opcode 0x33).
    /// Phase-1-only sites only emit 0x34 (VHF/UHF) and 0x3D (FDMA)
    /// band identifiers; Phase-2-capable sites ALSO emit 0x33 with
    /// a channel_type field describing TDMA slot count. The dashboard
    /// uses this to label the System Type as "P25 P1" vs "P25 P1+P2".
    /// SDRTrunk does the same inference from the TSBK stream.
    pub has_tdma_band: bool,
}

/// Active voice channel grant
#[derive(Debug, Clone)]
pub struct GrantInfo {
    pub channel: Channel,
    pub talkgroup: Talkgroup,
    pub source: Option<RadioId>,
    pub frequency_hz: Option<u64>,
    pub timestamp: Instant,
    /// Encryption flag from the `GroupVoiceChannelGrant` TSBK
    /// service options byte (mask 0x40). Phase 2e (2026-04-25):
    /// per-decoder grant retention is gone — `GrantInfo` is built
    /// once per TSBK, broadcast via `emit_grant_event`, then
    /// dropped. CallTracker latches encrypted on `CcGrantArrival`
    /// and ignores it on subsequent refreshes; cross-call
    /// inheritance lives in `imbe_forwarder.encrypted_tg_history`.
    pub encrypted: bool,
    /// Phase 7C: emergency flag from the same service options byte
    /// (mask 0x80). Surfaced for the dashboard "active calls" view
    /// so emergency calls can be visually highlighted.
    pub emergency: bool,
}


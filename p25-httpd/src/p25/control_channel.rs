//! P25 Control Channel decoder
//!
//! Processes the dibit stream from FPGA DMA:
//! 1. Unpack dibits from 64-bit DMA words
//! 2. NID sync word correlation (frame sync + Golay-decoded NAC/DUID)
//! 3. Data Unit framing (TSDU extraction)
//! 4. TSBK decoding (trellis + CRC)
//! 5. System state tracking
//!
//! At 4800 sym/sec the ARM has trivial CPU load for all of this.

use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

/// Simple ISO 8601-ish timestamp for events
fn chrono_timestamp() -> String {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let s = secs % 60;
    let ms = dur.subsec_millis();
    format!("{:02}:{:02}:{:02}.{:03}", hours, mins, s, ms)
}

use super::fec::{GolayDecoder, TrellisDecoder, TsduDeinterleaver};
use super::tsbk::{FrequencyBand, TsbkBlock, TsbkMessage};
use super::types::*;

/// Control channel decoder state
pub struct ControlChannelDecoder {
    /// Dibit shift register for frame sync correlation
    sync_register: u64,
    /// Number of dibits shifted in since last sync
    dibit_count: usize,
    /// Current decoder state
    state: DecoderState,
    /// Data unit buffer (dibits after NID)
    du_buffer: Vec<u8>,
    /// Expected data unit length
    du_expected_len: usize,

    /// System identity
    pub system: SystemIdentity,
    /// Frequency band table (from IDEN_UP messages)
    pub bands: HashMap<u8, FrequencyBand>,
    /// Active grants (channel -> grant info)
    pub grants: HashMap<u16, GrantInfo>,
    /// Recent TSBK messages for logging
    pub recent_messages: Vec<(Instant, TsbkMessage)>,
    /// Max recent messages to keep
    max_recent: usize,
    /// Talkgroup aliases (ID -> name)
    pub aliases: HashMap<u16, String>,
    /// Broadcast channel for WebSocket events
    event_tx: Option<broadcast::Sender<String>>,
}

/// Decoder state machine
#[derive(Debug, Clone, PartialEq, Eq)]
enum DecoderState {
    /// Searching for frame sync pattern
    Hunting,
    /// Found sync, reading NID (48 dibits after frame sync)
    ReadingNid { dibits_read: usize, nid_bits: u64 },
    /// Reading data unit payload
    ReadingDataUnit { duid: DataUnit },
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
}

/// Active voice channel grant
#[derive(Debug, Clone)]
pub struct GrantInfo {
    pub channel: Channel,
    pub talkgroup: Talkgroup,
    pub source: Option<RadioId>,
    pub frequency_hz: Option<u64>,
    pub timestamp: Instant,
}

/// Frame sync pattern as dibits packed into u64
/// The sync word is 24 symbols (48 bits): 0x5575F5FF77FF
/// Stored as 24 dibits in the low 48 bits
const FRAME_SYNC_DIBIT_PATTERN: u64 = 0x5575_F5FF_77FF;
const FRAME_SYNC_MASK: u64 = 0xFFFF_FFFF_FFFF; // 48 bits

/// Maximum Hamming distance for sync detection (allow a few bit errors)
const SYNC_THRESHOLD: u32 = 4;

/// NID is 48 dibits (96 bits) following frame sync
const NID_DIBITS: usize = 32;

impl ControlChannelDecoder {
    pub fn new() -> Self {
        ControlChannelDecoder {
            sync_register: 0,
            dibit_count: 0,
            state: DecoderState::Hunting,
            du_buffer: Vec::with_capacity(1024),
            du_expected_len: 0,
            system: SystemIdentity::default(),
            bands: HashMap::new(),
            grants: HashMap::new(),
            recent_messages: Vec::new(),
            max_recent: 100,
            aliases: HashMap::new(),
            event_tx: None,
        }
    }

    /// Set the broadcast channel for WebSocket events
    pub fn set_event_tx(&mut self, tx: broadcast::Sender<String>) {
        self.event_tx = Some(tx);
    }

    /// Process a 64-bit DMA word containing 32 packed dibits
    pub fn process_dma_word(&mut self, word: u64) {
        for i in 0..32 {
            let dibit = ((word >> (i * 2)) & 0x03) as u8;
            self.process_dibit(dibit);
        }
    }

    /// Process a single dibit
    pub fn process_dibit(&mut self, dibit: u8) {
        match self.state.clone() {
            DecoderState::Hunting => {
                // Shift dibit into sync register
                self.sync_register =
                    ((self.sync_register << 2) | (dibit as u64)) & FRAME_SYNC_MASK;
                self.dibit_count += 1;

                // Check for frame sync match (with error tolerance)
                let distance = (self.sync_register ^ FRAME_SYNC_DIBIT_PATTERN).count_ones();
                if distance <= SYNC_THRESHOLD && self.dibit_count >= 24 {
                    self.state = DecoderState::ReadingNid {
                        dibits_read: 0,
                        nid_bits: 0,
                    };
                }
            }

            DecoderState::ReadingNid {
                dibits_read,
                nid_bits,
            } => {
                let new_bits = (nid_bits << 2) | (dibit as u64);
                let new_count = dibits_read + 1;

                if new_count >= NID_DIBITS {
                    // NID complete — decode NAC and DUID with Golay FEC
                    let (nac_raw, duid_raw) = match GolayDecoder::decode_nid(new_bits) {
                        Some(v) => v,
                        None => {
                            self.state = DecoderState::Hunting;
                            self.dibit_count = 0;
                            return;
                        }
                    };

                    let nac = Nac::new(nac_raw);
                    if let Some(duid) = DataUnit::from_duid(duid_raw) {
                        // Update NAC if we see a valid one
                        self.system.nac = Some(nac);

                        let expected_len = duid.length_dibits();
                        if expected_len > 0 {
                            self.du_buffer.clear();
                            self.du_expected_len = expected_len;
                            self.state = DecoderState::ReadingDataUnit { duid };
                        } else {
                            // TDU or empty — back to hunting
                            self.state = DecoderState::Hunting;
                            self.dibit_count = 0;
                        }
                    } else {
                        // Invalid DUID, go back to hunting
                        self.state = DecoderState::Hunting;
                        self.dibit_count = 0;
                    }
                } else {
                    self.state = DecoderState::ReadingNid {
                        dibits_read: new_count,
                        nid_bits: new_bits,
                    };
                }
            }

            DecoderState::ReadingDataUnit { duid } => {
                self.du_buffer.push(dibit);

                if self.du_buffer.len() >= self.du_expected_len {
                    // Data unit complete
                    match duid {
                        DataUnit::Tsdu => self.process_tsdu(),
                        _ => {} // Other DU types handled in later phases
                    }
                    self.state = DecoderState::Hunting;
                    self.dibit_count = 0;
                }
            }
        }
    }

    /// Process a complete TSDU (Trunking Signaling Data Unit)
    ///
    /// Pipeline: de-interleave -> trellis decode -> CRC check -> TSBK parse -> state update
    fn process_tsdu(&mut self) {
        // 1. Remove status symbols from raw TSDU dibits
        let data_dibits = TsduDeinterleaver::deinterleave(&self.du_buffer);

        // 2. Extract individual TSBK blocks (196 dibits each)
        let tsbk_blocks = TsduDeinterleaver::extract_tsbk_blocks(&data_dibits);

        for block_dibits in tsbk_blocks {
            // 3. Trellis decode: 196 dibits -> 12 bytes
            let decoded = match TrellisDecoder::decode(block_dibits) {
                Some(bytes) => bytes,
                None => continue, // Decode failed, skip this block
            };

            // 4. Parse TSBK block and check CRC
            let block = TsbkBlock::parse(&decoded);
            if !block.crc_valid(&decoded) {
                continue; // CRC failed
            }

            // 5. Decode opcode-specific payload
            if let Some(msg) = block.decode() {
                // 6. Update system state
                self.handle_tsbk(msg);
            }
        }
    }

    /// Process a decoded TSBK message and update system state
    pub fn handle_tsbk(&mut self, msg: TsbkMessage) {
        match &msg {
            TsbkMessage::NetworkStatus {
                wacn,
                system_id,
                channel,
            } => {
                self.system.wacn = Some(*wacn);
                self.system.system_id = Some(*system_id);
                self.system.control_channel = Some(*channel);
            }
            TsbkMessage::RfssStatus {
                lra,
                rfss_id,
                site_id,
                channel,
            } => {
                self.system.lra = Some(*lra);
                self.system.rfss_id = Some(*rfss_id);
                self.system.site_id = Some(*site_id);
                self.system.control_channel = Some(*channel);
            }
            TsbkMessage::IdentifierUpdate { .. } => {
                if let Some(band) = FrequencyBand::from_tsbk(&msg) {
                    self.bands.insert(band.identifier, band);
                }
            }
            TsbkMessage::GroupVoiceChannelGrant {
                channel,
                talkgroup,
                source,
            } => {
                let freq = self.channel_to_frequency(*channel);
                self.grants.insert(
                    channel.0,
                    GrantInfo {
                        channel: *channel,
                        talkgroup: *talkgroup,
                        source: Some(*source),
                        frequency_hz: freq,
                        timestamp: Instant::now(),
                    },
                );
            }
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a,
                talkgroup_a,
                channel_b,
                talkgroup_b,
            } => {
                let freq_a = self.channel_to_frequency(*channel_a);
                self.grants.insert(
                    channel_a.0,
                    GrantInfo {
                        channel: *channel_a,
                        talkgroup: *talkgroup_a,
                        source: None,
                        frequency_hz: freq_a,
                        timestamp: Instant::now(),
                    },
                );
                if talkgroup_b.0 != 0 {
                    let freq_b = self.channel_to_frequency(*channel_b);
                    self.grants.insert(
                        channel_b.0,
                        GrantInfo {
                            channel: *channel_b,
                            talkgroup: *talkgroup_b,
                            source: None,
                            frequency_hz: freq_b,
                            timestamp: Instant::now(),
                        },
                    );
                }
            }
            _ => {}
        }

        // Broadcast event over WebSocket
        if let Some(ref tx) = self.event_tx {
            let event = self.tsbk_to_event(&msg);
            if let Ok(json) = serde_json::to_string(&event) {
                let _ = tx.send(json);
            }
        }

        // Log the message
        self.recent_messages.push((Instant::now(), msg));
        if self.recent_messages.len() > self.max_recent {
            self.recent_messages.remove(0);
        }
    }

    /// Convert a TSBK message to a WebSocket event
    fn tsbk_to_event(&self, msg: &TsbkMessage) -> p25_json::TsbkEvent {
        let now = chrono_timestamp();
        match msg {
            TsbkMessage::GroupVoiceChannelGrant {
                channel,
                talkgroup,
                source,
            } => {
                let freq = self.channel_to_frequency(*channel);
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "GRP_GRANT".into(),
                    summary: format!(
                        "TG:{:05} -> {} ({:.4} MHz)",
                        talkgroup.0,
                        channel,
                        freq.unwrap_or(0) as f64 / 1e6
                    ),
                    talkgroup: Some(talkgroup.0),
                    talkgroup_alias: self.aliases.get(&talkgroup.0).cloned(),
                    channel: Some(format!("{}", channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: Some(source.0),
                }
            }
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a,
                talkgroup_a,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "GRANT_UPD".into(),
                summary: format!("TG:{:05} -> {}", talkgroup_a.0, channel_a),
                talkgroup: Some(talkgroup_a.0),
                talkgroup_alias: self.aliases.get(&talkgroup_a.0).cloned(),
                channel: Some(format!("{}", channel_a)),
                frequency_mhz: self
                    .channel_to_frequency(*channel_a)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::NetworkStatus {
                wacn,
                system_id,
                channel,
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "NET_STS".into(),
                summary: format!("WACN:{:05X} SYS:{:03X} CH:{}", wacn, system_id, channel),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", channel)),
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::RfssStatus {
                rfss_id, site_id, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "RFSS_STS".into(),
                summary: format!("RFSS:{:02} SITE:{:02}", rfss_id, site_id),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::IdentifierUpdate {
                identifier,
                base_frequency,
                channel_spacing,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "IDEN_UP".into(),
                summary: format!(
                    "Band:{} base:{:.5} MHz spacing:{} Hz",
                    identifier,
                    *base_frequency as f64 / 1e6,
                    channel_spacing
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: Some(*base_frequency as f64 / 1e6),
                source: None,
            },
            TsbkMessage::AdjacentStatus {
                system_id,
                rfss_id,
                site_id,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "ADJ_STS".into(),
                summary: format!(
                    "SYS:{:03X} RFSS:{:02} SITE:{:02}",
                    system_id, rfss_id, site_id
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
        }
    }

    /// Resolve a logical channel number to an RF frequency using the band table
    pub fn channel_to_frequency(&self, channel: Channel) -> Option<u64> {
        self.bands
            .get(&channel.identifier())
            .map(|band| band.channel_frequency(channel.number()))
    }

    /// Expire old grants (calls that ended)
    pub fn expire_grants(&mut self, max_age_secs: u64) {
        let now = Instant::now();
        self.grants.retain(|_, grant| {
            now.duration_since(grant.timestamp).as_secs() < max_age_secs
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_identity_tracking() {
        let mut decoder = ControlChannelDecoder::new();

        // Simulate NET_STS_BCST
        decoder.handle_tsbk(TsbkMessage::NetworkStatus {
            wacn: 0xBEE00,
            system_id: 0x8A0,
            channel: Channel(0x0639),
        });

        assert_eq!(decoder.system.wacn, Some(0xBEE00));
        assert_eq!(decoder.system.system_id, Some(0x8A0));
        assert_eq!(decoder.system.control_channel.unwrap().0, 0x0639);
    }

    #[test]
    fn test_frequency_band_table() {
        let mut decoder = ControlChannelDecoder::new();

        // Add Clay County band 0
        decoder.handle_tsbk(TsbkMessage::IdentifierUpdate {
            identifier: 0,
            bw: 100, // 12500 Hz
            transmit_offset: -45_000_000,
            channel_spacing: 6_250,
            base_frequency: 851_006_250,
        });

        // Resolve control channel
        let freq = decoder
            .channel_to_frequency(Channel(0x0639))
            .unwrap();
        assert_eq!(freq, 860_962_500); // 860.9625 MHz
    }

    #[test]
    fn test_grant_tracking() {
        let mut decoder = ControlChannelDecoder::new();

        // Add band first
        decoder.handle_tsbk(TsbkMessage::IdentifierUpdate {
            identifier: 0,
            bw: 100,
            transmit_offset: -45_000_000,
            channel_spacing: 6_250,
            base_frequency: 851_006_250,
        });

        // Voice grant
        decoder.handle_tsbk(TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x045D), // band 0, ch 1117
            talkgroup: Talkgroup(300),
            source: RadioId(1011),
        });

        assert!(decoder.grants.contains_key(&0x045D));
        let grant = &decoder.grants[&0x045D];
        assert_eq!(grant.talkgroup.0, 300);
        assert_eq!(grant.frequency_hz, Some(857_987_500)); // 857.9875 MHz
    }
}

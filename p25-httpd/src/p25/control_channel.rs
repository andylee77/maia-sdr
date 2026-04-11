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

    // ── Diagnostic counters (for tracing/logging only) ──
    /// Cumulative dibit value histogram
    dibit_hist: [u64; 4],
    /// Total dibits processed
    total_dibits: u64,
    /// Best (lowest) sync Hamming distance seen since last log
    best_sync_distance: u32,
    /// Number of full sync matches (distance ≤ SYNC_THRESHOLD)
    sync_hits: u64,
    /// Number of near-syncs (distance ≤ SYNC_NEAR_LOG_THRESHOLD but > SYNC_THRESHOLD)
    sync_near_misses: u64,
    /// Last logged dibit count, for periodic histogram dumps
    last_log_dibits: u64,
    /// Rolling capture of recent dibits for /api/dibit_dump (oldest first)
    pub recent_dibits: std::collections::VecDeque<u8>,
    /// Histogram of the *raw* DUID values that decode_nid sees, before
    /// the temporary "always TSDU" hardcode (see fec::GolayDecoder::decode_nid).
    /// 16 buckets, indexed by raw 4-bit DUID. Lets us observe the actual
    /// on-air DUID distribution while the BCH(64,16) NID FEC is still missing
    /// -- a healthy control channel should be ~100% in bucket 7 (TSDU).
    raw_duid_hist: [u64; 16],

    // ── Phase 6F.2 diagnostic counters: pipeline failure breakdown ─
    /// Number of times a frame sync hit triggered a NID read attempt.
    /// This is the same as `sync_hits` but kept separate for clarity.
    pub nid_attempts: u64,
    /// NID payloads where `GolayDecoder::decode_nid` returned `None`
    /// (BCH FEC could not correct, parity check failed). High here =
    /// either real bit errors in the NID dibits or a frame-sync slip
    /// putting the read window at the wrong place.
    pub nid_decode_failures: u64,
    /// NID payloads where decode_nid succeeded but the recovered DUID
    /// nibble didn't map to a known DataUnit variant. Should be 0 in a
    /// healthy stream because the BCH would have rejected garbage.
    pub nid_invalid_duid: u64,
    /// NID payloads that fully validated and resulted in a Hunting ->
    /// ReadingDataUnit transition.
    pub nid_decoded_ok: u64,
    /// NID payloads that fully validated AND were TSDUs (DUID 0x7).
    /// On a healthy control channel this should converge on
    /// `nid_decoded_ok` because nearly every NID is a TSDU.
    pub nid_decoded_tsdu: u64,

    /// Number of TSDU data units that finished `process_tsdu` (the de-
    /// interleave + extract). This is the count of "we tried to decode
    /// a TSDU body".
    pub tsdu_attempts: u64,
    /// Sum of TSBK blocks attempted across all TSDUs (each TSDU yields
    /// 1-4 TSBK blocks).
    pub tsbk_block_attempts: u64,
    /// TSBK blocks where `TrellisDecoder::decode` returned None.
    pub tsbk_trellis_failures: u64,
    /// TSBK blocks where trellis succeeded but `block.crc_valid` was
    /// false. **This is the canonical "we have dibits but they're
    /// corrupted past trellis FEC capacity" indicator.**
    pub tsbk_crc_failures: u64,
    /// TSBK blocks that decoded cleanly through CRC and produced a
    /// `TsbkMessage`.
    pub tsbk_crc_ok: u64,
    /// TSBK blocks that survived CRC but the opcode parser couldn't
    /// turn into a known TsbkMessage variant.
    pub tsbk_unknown_opcode: u64,

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

/// Maximum Hamming distance for sync detection.
///
/// Temporarily widened from 4 → 10 while we still have a residual DC bias on
/// `sym_diff_re` that flips ~20% of outer (±3) symbols to inner (±1). The P25
/// frame sync is all outer symbols, so the bias produces ~5 systematic bit
/// errors per sync window plus ~5 from random noise = ~10 errors total. With
/// the old threshold of 4, less than 1% of sync windows matched. With 10, the
/// near-miss cluster (centred ~10-12 per the dashboard's "Near Misses" stat)
/// becomes acquirable, while NID Golay decode + DUID validation still reject
/// false syncs downstream.
///
/// Once the HDL DC blocker is added (see DEVPLAN), this should drop back to 4.
pub const SYNC_THRESHOLD: u32 = 10;

/// Logging threshold: any candidate with distance ≤ this is logged as a "near miss"
/// to give visibility into how close the bit stream is to a real sync.
const SYNC_NEAR_LOG_THRESHOLD: u32 = 14;

/// The P25 NID payload is 64 bits = 32 content dibits, but the first P25
/// status dibit lands inside the NID window at on-air index 11 (counting
/// from 0 at the first dibit after the frame sync), so the NID spans 33
/// on-air dibits. Matches `lsm::sync::NID_TRANSMITTED_DIBITS` and the
/// HDL `LsmSyncNidExtract` which both read 33 dibits and skip index 11
/// before packing the remaining 32 into the 64-bit BCH codeword.
///
/// Historical note: until 2026-04-10 this constant was 32 and the
/// decoder skipped nothing, which silently corrupted bits 41..40 of the
/// NID codeword with the status dibit value and shifted the remaining
/// parity bits out of position. The Phase 2A C4FM decoder "worked"
/// because its `decode_nid` stub only extracted bits 63..48 (NAC+DUID)
/// from the top of the word -- those come from on-air dibits 0..7, all
/// BEFORE the status dibit at index 11, so the stub got the right
/// NAC/raw_DUID despite the corrupted parity region. Porting the
/// validated BCH(63,16,11) FEC into the decoder exposed the bug: the
/// LSM-side decoder consistently miscorrected clean Clay County NIDs
/// (NAC=0x8A1, DUID=0x7) to a spurious fixed codeword
/// (NAC=0xE28, DUID=0x5) because the status-dibit corruption was
/// deterministic. See doc/changes/022 for the fix log.
const NID_TRANSMITTED_DIBITS: usize = 33;
/// Index within the 33-dibit on-air NID window where the first P25
/// status dibit lands. The decoder must read this dibit (so the
/// dibit-stream cursor keeps advancing) but must NOT fold its value
/// into the 64-bit BCH codeword.
const NID_STATUS_DIBIT_INDEX: usize = 11;

impl ControlChannelDecoder {
    pub fn new() -> Self {
        ControlChannelDecoder {
            sync_register: 0,
            dibit_count: 0,
            state: DecoderState::Hunting,
            du_buffer: Vec::with_capacity(1024),
            du_expected_len: 0,
            dibit_hist: [0; 4],
            total_dibits: 0,
            best_sync_distance: u32::MAX,
            sync_hits: 0,
            sync_near_misses: 0,
            last_log_dibits: 0,
            recent_dibits: std::collections::VecDeque::with_capacity(2048),
            raw_duid_hist: [0; 16],
            nid_attempts: 0,
            nid_decode_failures: 0,
            nid_invalid_duid: 0,
            nid_decoded_ok: 0,
            nid_decoded_tsdu: 0,
            tsdu_attempts: 0,
            tsbk_block_attempts: 0,
            tsbk_trellis_failures: 0,
            tsbk_crc_failures: 0,
            tsbk_crc_ok: 0,
            tsbk_unknown_opcode: 0,
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

    /// Snapshot of the dibit histogram (count per dibit value 0..3).
    pub fn dibit_histogram(&self) -> [u64; 4] {
        self.dibit_hist
    }

    /// Snapshot of the raw on-air DUID histogram (count per 4-bit DUID value).
    /// Diagnostic only -- a healthy decoder with the BCH FEC implemented
    /// should be ~100% in bucket 7 (TSDU) on a control channel.
    pub fn raw_duid_histogram(&self) -> [u64; 16] {
        self.raw_duid_hist
    }

    /// Total dibits processed since startup.
    pub fn total_dibits(&self) -> u64 {
        self.total_dibits
    }

    /// Number of full sync correlator hits.
    pub fn sync_hits(&self) -> u64 {
        self.sync_hits
    }

    /// Number of "near sync" matches (Hamming distance ≤ near threshold).
    pub fn sync_near_misses(&self) -> u64 {
        self.sync_near_misses
    }

    /// Best (lowest) Hamming distance to the sync pattern observed since
    /// the last periodic decoder log dump (resets every ~16k dibits).
    pub fn best_sync_distance(&self) -> u32 {
        self.best_sync_distance
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
        // ── Diagnostics: histogram + periodic dump ───────────────
        let d = dibit & 0x03;
        self.dibit_hist[d as usize] += 1;
        self.total_dibits += 1;
        // Capture recent dibits for /api/dibit_dump (rolling 2048)
        if self.recent_dibits.len() == 2048 {
            self.recent_dibits.pop_front();
        }
        self.recent_dibits.push_back(d);

        // Every 16384 dibits (~3.4 s of 4800 sym/s), dump stats
        if self.total_dibits - self.last_log_dibits >= 16384 {
            let n: u64 = self.dibit_hist.iter().sum();
            let pct = |v: u64| -> f64 {
                if n == 0 { 0.0 } else { 100.0 * v as f64 / n as f64 }
            };
            tracing::info!(
                target: "p25_decoder",
                "dibit stats @ {}: hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                 sync hits={} near={} best_dist={}",
                self.total_dibits,
                pct(self.dibit_hist[0]), pct(self.dibit_hist[1]),
                pct(self.dibit_hist[2]), pct(self.dibit_hist[3]),
                self.sync_hits, self.sync_near_misses,
                if self.best_sync_distance == u32::MAX { 99 } else { self.best_sync_distance },
            );
            // Raw on-air DUID distribution. With the BCH FEC stub still
            // in place, this tells us the actual DUID-bit-error pattern
            // we're fighting. A real control channel should be ~100% in
            // bucket 7 (TSDU). Anything else means the slicer is still
            // dropping bits in the NID field.
            let duid_total: u64 = self.raw_duid_hist.iter().sum();
            if duid_total > 0 {
                let dpct = |v: u64| -> f64 { 100.0 * v as f64 / duid_total as f64 };
                tracing::info!(
                    target: "p25_decoder",
                    "raw DUID histogram (n={}): \
                     0={:.0}% 1={:.0}% 2={:.0}% 3={:.0}% 4={:.0}% \
                     5={:.0}% 6={:.0}% 7={:.0}% 8={:.0}% 9={:.0}% \
                     A={:.0}% B={:.0}% C={:.0}% D={:.0}% E={:.0}% F={:.0}%",
                    duid_total,
                    dpct(self.raw_duid_hist[0x0]), dpct(self.raw_duid_hist[0x1]),
                    dpct(self.raw_duid_hist[0x2]), dpct(self.raw_duid_hist[0x3]),
                    dpct(self.raw_duid_hist[0x4]), dpct(self.raw_duid_hist[0x5]),
                    dpct(self.raw_duid_hist[0x6]), dpct(self.raw_duid_hist[0x7]),
                    dpct(self.raw_duid_hist[0x8]), dpct(self.raw_duid_hist[0x9]),
                    dpct(self.raw_duid_hist[0xA]), dpct(self.raw_duid_hist[0xB]),
                    dpct(self.raw_duid_hist[0xC]), dpct(self.raw_duid_hist[0xD]),
                    dpct(self.raw_duid_hist[0xE]), dpct(self.raw_duid_hist[0xF]),
                );
            }
            self.last_log_dibits = self.total_dibits;
            self.best_sync_distance = u32::MAX;
        }

        match self.state.clone() {
            DecoderState::Hunting => {
                // Shift dibit into sync register
                self.sync_register =
                    ((self.sync_register << 2) | (dibit as u64)) & FRAME_SYNC_MASK;
                self.dibit_count += 1;

                // Check for frame sync match (with error tolerance)
                let distance = (self.sync_register ^ FRAME_SYNC_DIBIT_PATTERN).count_ones();
                if distance < self.best_sync_distance && self.dibit_count >= 24 {
                    self.best_sync_distance = distance;
                }
                if distance <= SYNC_NEAR_LOG_THRESHOLD
                    && distance > SYNC_THRESHOLD
                    && self.dibit_count >= 24
                {
                    self.sync_near_misses += 1;
                    // Log the first few near-misses then sample sparsely
                    if self.sync_near_misses <= 8 || self.sync_near_misses % 256 == 0 {
                        tracing::info!(
                            target: "p25_decoder",
                            "near-sync #{}: distance={} reg=0x{:012X} (dibit #{})",
                            self.sync_near_misses, distance, self.sync_register,
                            self.total_dibits,
                        );
                    }
                }
                if distance <= SYNC_THRESHOLD && self.dibit_count >= 24 {
                    self.sync_hits += 1;
                    self.nid_attempts += 1;
                    tracing::info!(
                        target: "p25_decoder",
                        "SYNC HIT #{}: distance={} (dibit #{}) -> ReadingNid",
                        self.sync_hits, distance, self.total_dibits,
                    );
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
                // The on-air NID window is NID_TRANSMITTED_DIBITS (33)
                // dibits wide, with a P25 status dibit injected at
                // index NID_STATUS_DIBIT_INDEX (11). We ALWAYS advance
                // the cursor (dibits_read) on every incoming dibit so
                // the window length stays in sync with the on-air
                // stream, but we only fold the dibit into `nid_bits`
                // if it isn't the status slot. The resulting 64-bit
                // `nid_bits` is bit-for-bit compatible with the BCH
                // codeword layout produced by `lsm::nid_fec::encode_nid`.
                let new_bits = if dibits_read == NID_STATUS_DIBIT_INDEX {
                    nid_bits
                } else {
                    (nid_bits << 2) | (dibit as u64)
                };
                let new_count = dibits_read + 1;

                if new_count >= NID_TRANSMITTED_DIBITS {
                    // NID complete — decode NAC and DUID. The DUID is
                    // currently hardcoded to 0x7 (TSDU) inside decode_nid
                    // because the BCH(64,16) NID FEC is still a stub.
                    // `raw_duid` is the value before the hardcode, kept
                    // here for the diagnostic histogram.
                    let (nac_raw, duid_raw, on_air_duid) =
                        match GolayDecoder::decode_nid(new_bits) {
                            Some(v) => v,
                            None => {
                                self.nid_decode_failures += 1;
                                tracing::info!(
                                    target: "p25_decoder",
                                    "NID decode FAILED (raw=0x{:016X}) -> Hunting",
                                    new_bits,
                                );
                                self.state = DecoderState::Hunting;
                                self.dibit_count = 0;
                                return;
                            }
                        };

                    // Track the actual on-air DUID distribution. Useful
                    // for confirming the BCH-FEC hypothesis empirically:
                    // a working FEC would land bucket 7 at ~100%; a
                    // missing FEC + ~12-bit-error NIDs lands bits all
                    // over the place.
                    self.raw_duid_hist[(on_air_duid & 0x0F) as usize] += 1;

                    let nac = Nac::new(nac_raw);
                    if let Some(duid) = DataUnit::from_duid(duid_raw) {
                        // Update NAC if we see a valid one
                        self.system.nac = Some(nac);
                        self.nid_decoded_ok += 1;
                        if matches!(duid, DataUnit::Tsdu) {
                            self.nid_decoded_tsdu += 1;
                        }

                        let expected_len = duid.length_dibits();
                        tracing::info!(
                            target: "p25_decoder",
                            "NID OK: NAC=0x{:03X} DUID=0x{:X} ({:?}) raw_DUID=0x{:X} len={}",
                            nac_raw, duid_raw, duid, on_air_duid, expected_len,
                        );
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
                        self.nid_invalid_duid += 1;
                        tracing::info!(
                            target: "p25_decoder",
                            "NID DUID invalid: NAC=0x{:03X} DUID_raw=0x{:X} -> Hunting",
                            nac_raw, duid_raw,
                        );
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
        self.tsdu_attempts += 1;

        // 1. Remove status symbols from raw TSDU dibits
        let data_dibits = TsduDeinterleaver::deinterleave(&self.du_buffer);

        // 2. Extract individual TSBK blocks (196 dibits each)
        let tsbk_blocks = TsduDeinterleaver::extract_tsbk_blocks(&data_dibits);

        for block_dibits in tsbk_blocks {
            self.tsbk_block_attempts += 1;

            // 3. Trellis decode: 196 dibits -> 12 bytes
            let decoded = match TrellisDecoder::decode(block_dibits) {
                Some(bytes) => bytes,
                None => {
                    self.tsbk_trellis_failures += 1;
                    continue;
                }
            };

            // 4. Parse TSBK block and check CRC
            let block = TsbkBlock::parse(&decoded);
            if !block.crc_valid(&decoded) {
                self.tsbk_crc_failures += 1;
                continue;
            }
            self.tsbk_crc_ok += 1;

            // 5. Decode opcode-specific payload
            if let Some(msg) = block.decode() {
                // 6. Update system state
                self.handle_tsbk(msg);
            } else {
                self.tsbk_unknown_opcode += 1;
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

    /// Drive the decoder end-to-end with a frame sync + 33-dibit NID
    /// (with a deliberately-wrong status dibit injected at index 11)
    /// and verify the BCH FEC still decodes the correct NAC/DUID.
    ///
    /// This is the regression guard for doc/changes/022 -- before the
    /// NID_STATUS_DIBIT_INDEX skip was added, the decoder read 32
    /// consecutive dibits and deterministically miscorrected clean
    /// Clay County NIDs to a spurious fixed (NAC=0xE28, DUID=0x5)
    /// because the on-air status dibit at position 11 corrupted bits
    /// 41..40 of the BCH codeword and shifted the rest of the parity
    /// region.
    #[test]
    fn test_nid_status_dibit_skip_e2e() {
        use crate::lsm::nid_fec;

        // Helper: unpack a 48-bit pattern into 24 dibits MSB-first,
        // or a 64-bit word into 32 dibits.
        fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(n_dibits);
            for i in (0..n_dibits).rev() {
                out.push(((bits >> (i * 2)) & 0x3) as u8);
            }
            out
        }

        // Clean Clay County NID: NAC=0x8A1, DUID=0x7 (TSDU).
        let nid_bits = nid_fec::encode_nid(0x8A1, 0x7);
        let nid_dibits_32 = unpack_dibits(nid_bits, 32);

        // Build the 33-dibit on-air NID window: splice a DELIBERATELY
        // WRONG status dibit (value 0x3 = "-3") at index 11. If the
        // decoder folds this into nid_bits, the BCH codeword gets
        // corrupted and the test fails. If the decoder correctly
        // skips index 11, the test passes.
        let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
        on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
        on_air_nid.push(0x3); // garbage status dibit
        on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
        assert_eq!(on_air_nid.len(), 33);

        // Frame sync pattern unpacked into 24 dibits. Matches the
        // decoder's FRAME_SYNC_DIBIT_PATTERN constant at the top of
        // this file.
        let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);
        assert_eq!(fs_dibits.len(), 24);

        // Drive the decoder: first 24 dibits of frame sync (to arm
        // the correlator) followed by the 33 dibits of the on-air NID
        // window (with status spliced in).
        let mut decoder = ControlChannelDecoder::new();
        for &d in &fs_dibits {
            decoder.process_dibit(d);
        }
        for &d in &on_air_nid {
            decoder.process_dibit(d);
        }

        // After the NID is fully consumed the decoder should have
        // latched the Clay County NAC into `system.nac`. Anything
        // else means the BCH decoder either rejected the codeword or
        // miscorrected to a different NAC -- either way the status
        // dibit skip is broken.
        assert_eq!(
            decoder.system.nac,
            Some(Nac::new(0x8A1)),
            "decoder should land on the clean Clay County NAC after \
             skipping the status dibit at position 11; got {:?}",
            decoder.system.nac,
        );
    }
}

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
    /// Number of TSBK blocks already decoded from the current TSDU
    /// (0..=3). Phase 6F.3 multi-block TSBK support: when TSBK1
    /// finishes and `last_block` is not set, we extend `du_expected_len`
    /// to 231 (TSBK2) or 303 (TSBK3) and bump this counter on each
    /// successful decode. Reset to 0 on every Hunting transition.
    tsdu_blocks_decoded: usize,

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
    /// Subset of `tsbk_crc_ok` where the CRC validated under the
    /// **plain** convention (`crc16_ccitt(data) == msg_crc`). Phase
    /// 6F.2d diagnostic: lets us see in the dashboard which CRC
    /// convention real on-air TSBKs use, and whether the population
    /// is mixed or single-convention.
    pub tsbk_crc_ok_plain: u64,
    /// Subset of `tsbk_crc_ok` where the CRC validated under the
    /// **xor 0xFFFF** convention (`crc16_ccitt(data) ^ 0xFFFF ==
    /// msg_crc`). Phase 6F.2d diagnostic, see `tsbk_crc_ok_plain`.
    pub tsbk_crc_ok_xored: u64,
    /// TSBK blocks that survived CRC but the opcode parser couldn't
    /// turn into a known TsbkMessage variant.
    pub tsbk_unknown_opcode: u64,

    // ── Phase 6F.4 diagnostic histograms ──
    /// Per-opcode histogram of CRC-OK TSBK blocks. Indexed by the
    /// 6-bit opcode value (`bytes[0] & 0x3F`). Lets the dashboard show
    /// the actual on-air opcode distribution and figure out which
    /// opcodes we're missing parsers for. Phase 6F.4 added this so we
    /// can stop guessing why `bands_known` stays at 0.
    pub tsbk_opcode_hist_ok: [u64; 64],
    /// Per-opcode histogram of CRC-FAIL TSBK blocks. Same layout as
    /// `tsbk_opcode_hist_ok`. A high count for a particular opcode
    /// suggests the trellis-decoded bytes are mostly garbage (the
    /// "opcode" was randomly distributed) -- a low count and clean
    /// distribution match the CRC-OK histogram for confirmed real
    /// opcodes that just had bit errors past trellis correction.
    pub tsbk_opcode_hist_fail: [u64; 64],
    /// Per-vendor-mfid histogram on CRC-OK blocks. Index 0 = standard
    /// (mfid==0x00), other indices are bucketed by mfid value (we
    /// only track mfid 0x00, 0x90 = Motorola, 0x10 = Icom etc, and
    /// "other"). Diagnostic for "how much of our traffic is vendor
    /// proprietary?".
    pub tsbk_mfid_hist_ok: [u64; 4],
    /// Per-block-position attempt counters (block 0 = TSBK1,
    /// 1 = TSBK2, 2 = TSBK3). Increments when the decoder feeds the
    /// trellis for that block index. Confirms multi-block continuation
    /// is actually firing.
    pub tsbk_block_attempts_by_pos: [u64; 3],
    /// Per-block-position CRC-OK counters. Compares against
    /// `tsbk_block_attempts_by_pos` to give a per-position CRC success
    /// rate -- if block 1 / block 2 have substantially worse rates than
    /// block 0, the multi-block dibit alignment is wrong.
    pub tsbk_crc_ok_by_pos: [u64; 3],

    /// **Phase 6F.6 sync distance histogram.** Indexed by Hamming
    /// distance bucket (0..=23, with bucket 24 = "anything ≥ 24").
    /// Bumped on every dibit shift in Hunting state once we have a
    /// full 24-dibit sync window. Lets the dashboard see whether real
    /// syncs cluster at low distances (slicer is fine, just need to
    /// match) or high distances (slicer is corrupting half the
    /// outer-symbol bits in the all-outer sync pattern, sync widening
    /// can't help).
    ///
    /// 6F.4 verification showed the PS hard correlator and PL HDL hard
    /// correlator both stuck at ~4.7 sync hits/sec while the Phase 6D
    /// soft-decision IQ correlator gets 9/sec on the same signal. The
    /// dibit-correlator hard sync rate is the dominant throughput
    /// bottleneck. Without this histogram we can't tell whether the
    /// missing 4.3 syncs/sec are at salvageable distances (e.g. 9-14)
    /// or not (e.g. 18-24, where they overlap random data).
    pub sync_distance_hist: [u64; 25],

    // ── Phase 6F.2h aligned capture (one-shot diagnostic) ──
    /// Set to `true` by `/api/lsm_capture_aligned` to request a full
    /// pipeline trace on the NEXT sync hit. Cleared by the decoder as
    /// soon as it captures one frame.
    pub aligned_capture_armed: bool,
    /// Snapshot populated when `aligned_capture_armed` was true at the
    /// moment of a sync hit. Read by `/api/lsm_capture_aligned` and
    /// then cleared.
    pub aligned_capture: Option<AlignedCapture>,
    /// Internal: when non-None, the decoder is in the middle of
    /// capturing a frame and this is the buffer holding the in-flight
    /// raw NID + body dibits.
    capture_in_flight: Option<CaptureBuilder>,

    /// System identity
    pub system: SystemIdentity,
    /// Frequency band table (from IDEN_UP messages)
    pub bands: HashMap<u8, FrequencyBand>,
    /// Active grants (channel -> grant info)
    pub grants: HashMap<u16, GrantInfo>,
    /// Recent TSBK messages for logging. Tuple is `(instant, block_idx,
    /// message)` where `block_idx` is 0/1/2 = TSBK1/TSBK2/TSBK3 within
    /// the parent TSDU. Phase 6F.4: added `block_idx` so the dashboard
    /// can show which block each message came from, matching SDRTrunk's
    /// `decoded_messages.log` format ("TSBK1 NET_STS_BCAST...").
    pub recent_messages: Vec<(Instant, u8, TsbkMessage)>,
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

/// Phase 6F.2h: snapshot of one full TSBK frame as it flows through
/// the LSM software decoder. Captured one-shot via the
/// `/api/lsm_capture_aligned` endpoint, used for offline replay /
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
    /// (lets us correlate this snapshot with `/api/lsm_capture`).
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
struct CaptureBuilder {
    sync_dibits: Vec<u8>,
    sync_distance: u32,
    raw_nid_dibits: Vec<u8>,
    nid_bits: u64,
    bch_nac: Option<u16>,
    bch_duid: Option<u8>,
    raw_duid: u8,
    raw_body_dibits: Vec<u8>,
    total_dibits_at_capture: u64,
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
/// **Phase 6F.7 (2026-04-11):** sync threshold is now RUNTIME-TUNABLE
/// via the new `RUNTIME_SYNC_THRESHOLD` AtomicU32. The constant below
/// is just the boot default. Use `/api/sync_tune?threshold=N` to
/// experiment without reflashing -- the 6F.6 verification showed the
/// optimal threshold depends on PLL lock state and varies over time.
///
/// **Phase 6F.6 history (2026-04-11):** raised from 8 → 14 after 6F.5
/// verification showed widening 4 → 8 had no effect on sync hit rate.
/// The 6F.5 dibit dump:
///
/// ```text
/// SYNC_THRESHOLD       = 8
/// sync hits            = 470 (4.59/sec)
/// sync near (5..14)    = 3243 (31.67/sec)   ← still missing
/// ```
///
/// The hits/sec is the SAME at threshold 8 and threshold 4 because
/// there are essentially no real syncs at distance 5-8 in this
/// signal. The big mass of "near" sync events at distance 9-14 is
/// what we need to capture, so 6F.6 raises threshold to 14.
///
/// Cross-checked against the PL HDL gateware NID extractor (which
/// runs its OWN hard sync detector in firmware): also stuck at
/// ~4.7 NID events/sec. So both PS and PL hard correlators on the
/// HDL slicer's dibit stream agree -- the slicer is producing too
/// many bit errors per outer-symbol sync dibit for the dibit-level
/// correlator to find better matches. The Phase 6D soft-decision IQ
/// correlator on raw IQ samples gets 9/sec, confirming syncs ARE
/// out there at the sample level but the slicer is dropping them.
///
/// At threshold 14 the random-false-positive rate is much higher
/// than threshold 8: `P(48-bit random ≤ 14 of fixed)` ≈ 6×10⁻³.
/// At ~2400 sliding windows/sec that's ~14 false syncs/sec. The
/// downstream BCH(63,16,11) NID FEC catches them (~10⁻⁴ pass-through
/// rate for random 64-bit words → ~0.001 false TSDU events/sec,
/// negligible). Each false sync costs ~33 dibits of wasted NID
/// read work; at 14 false/sec that's 462 dibits/sec ≈ 10 % of the
/// 4800 sym/s budget -- still cheap on Cortex-A9.
///
/// **6F.6 also adds a `sync_distance_hist[25]` field** that buckets
/// every observed sync distance, exposed via `/api/lsm_dibit_dump`.
/// If the histogram shows a real sync cluster at 9-14, threshold 14
/// catches them. If the distribution is essentially flat random with
/// no cluster at any distance, the syncs aren't recoverable from the
/// current dibit stream and we need to either (a) fix the HDL DC
/// blocker / slicer or (b) wire the Phase 6D soft sync events into
/// the TSBK pipeline.
///
/// Phase 6F.2e history: dropped 10 → 4 because the LSM stream was
/// "much cleaner" than legacy C4FM. That was optimistic; both 6F.5
/// (8) and 6F.6 (14) have walked it back as the noise budget became
/// clear from on-target measurement.
/// Boot-time default for the runtime-tunable sync threshold. Code
/// reads `RUNTIME_SYNC_THRESHOLD.load(Relaxed)` everywhere instead of
/// this constant directly. The 6F.6 distance histogram showed this is
/// a moving target -- 6 is a sane middle ground between
/// "perfect-only" (4) and "noise-flooded" (14), but the optimum
/// shifts with PLL lock state, so the right tool is `/api/sync_tune`.
pub const SYNC_THRESHOLD: u32 = 6;

/// Phase 6F.7 runtime-tunable sync threshold. Reads inside the dibit
/// hot loop go through this AtomicU32 (Relaxed ordering -- the value
/// only changes when an operator hits `/api/sync_tune`, and a one-
/// dibit lag is fine). Initial value is set in
/// `ControlChannelDecoder::new()` from `SYNC_THRESHOLD`.
pub static RUNTIME_SYNC_THRESHOLD: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(SYNC_THRESHOLD);

/// Logging threshold: any candidate with distance ≤ this is logged as a "near miss"
/// to give visibility into how close the bit stream is to a real sync.
/// 6F.6: bumped from 14 to 20 since SYNC_THRESHOLD is now 14 -- we want
/// the near counter to show us the distance 15-20 bucket so we can
/// decide whether widening further is worthwhile.
const SYNC_NEAR_LOG_THRESHOLD: u32 = 20;

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
            tsdu_blocks_decoded: 0,
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
            tsbk_crc_ok_plain: 0,
            tsbk_crc_ok_xored: 0,
            tsbk_unknown_opcode: 0,
            tsbk_opcode_hist_ok: [0; 64],
            tsbk_opcode_hist_fail: [0; 64],
            tsbk_mfid_hist_ok: [0; 4],
            tsbk_block_attempts_by_pos: [0; 3],
            tsbk_crc_ok_by_pos: [0; 3],
            sync_distance_hist: [0; 25],
            aligned_capture_armed: false,
            aligned_capture: None,
            capture_in_flight: None,
            system: SystemIdentity::default(),
            bands: HashMap::new(),
            grants: HashMap::new(),
            recent_messages: Vec::new(),
            // 6F.10: bumped from 100 -> 1000. At the steady-state PS LSM
            // throughput of ~14 messages/sec the 100 cap saturates in 7
            // seconds, which made the verification script's
            // `messages / uptime` headline rate report a misleading
            // 1.1 msg/sec instead of the real 14.8/sec. 1000 holds ~70
            // seconds of activity, enough for `/api/recent_tsbks` to
            // show a representative window.
            max_recent: 1000,
            aliases: HashMap::new(),
            event_tx: None,
        }
    }

    /// Set the broadcast channel for WebSocket events
    pub fn set_event_tx(&mut self, tx: broadcast::Sender<String>) {
        self.event_tx = Some(tx);
    }

    /// Phase 6F.9: process a TSDU "directed" by an upstream sync
    /// detector (typically the Phase 6D soft-decision sync correlator
    /// on raw IQ). The caller knows the dibit position of the first
    /// NID dibit; we skip the Hunting state machine entirely and run
    /// NID extraction + multi-block TSBK decode on the supplied buffer.
    ///
    /// `nid_and_body` must be at least 33 dibits (just the NID); for a
    /// full multi-block TSDU it should be 33 + 303 = 336 dibits. Any
    /// length in between truncates the body read at the buffer end.
    ///
    /// **Why this exists.** The HDL dibit slicer is the dominant
    /// throughput bottleneck (60 / 40 inner / outer ratio costs us
    /// ~70% of TSDUs at the dibit-correlator hard sync stage). Phase
    /// 6D's soft-decision raw-IQ correlator picks up roughly twice as
    /// many syncs from the same signal -- bypassing the slicer means
    /// we can process those extra syncs through the same TSBK pipeline
    /// instead of just counting them in `LsmStats`. See doc 029.
    ///
    /// All counter updates flow through the same fields as the
    /// streaming `process_dibit` path so `/api/decoder_compare` and
    /// `/api/tsbk_opcodes` show a unified view of "what this decoder
    /// has seen", regardless of whether it came in via Hunting or via
    /// directed soft sync.
    pub fn process_directed_tsdu(&mut self, nid_and_body: &[u8]) {
        if nid_and_body.len() < NID_TRANSMITTED_DIBITS {
            return;
        }

        // 1. Extract NID, skipping the in-window status dibit at index 11.
        let mut nid_bits: u64 = 0;
        for j in 0..NID_TRANSMITTED_DIBITS {
            if j == NID_STATUS_DIBIT_INDEX {
                continue;
            }
            nid_bits = (nid_bits << 2) | (nid_and_body[j] as u64 & 0x3);
        }

        self.nid_attempts += 1;
        let (nac_raw, duid_raw, on_air_duid) =
            match GolayDecoder::decode_nid(nid_bits) {
                Some(v) => v,
                None => {
                    self.nid_decode_failures += 1;
                    return;
                }
            };
        self.raw_duid_hist[(on_air_duid & 0x0F) as usize] += 1;

        let duid = match DataUnit::from_duid(duid_raw) {
            Some(d) => d,
            None => {
                self.nid_invalid_duid += 1;
                return;
            }
        };
        self.system.nac = Some(Nac::new(nac_raw));
        self.nid_decoded_ok += 1;

        // 2. We only handle TSDU directed reads for now (Phase 6F.9).
        //    Other DUIDs (HDU / LDU / TDU) just bump the NID counters
        //    and return.
        if !matches!(duid, DataUnit::Tsdu) {
            return;
        }
        self.nid_decoded_tsdu += 1;
        self.tsdu_attempts += 1;

        // 3. Walk through up to 3 TSBK blocks. Body starts at offset
        //    NID_TRANSMITTED_DIBITS in the supplied buffer.
        let body = &nid_and_body[NID_TRANSMITTED_DIBITS..];
        for block_idx in 0..TsduDeinterleaver::MAX_BLOCKS {
            let num_blocks = block_idx + 1;
            let needed = TsduDeinterleaver::body_dibits_for_blocks(num_blocks)
                .expect("body_dibits_for_blocks returns Some for 1..=3");
            if body.len() < needed {
                break;
            }
            let body_slice = &body[..needed];

            let data_dibits =
                TsduDeinterleaver::deinterleave_multi(body_slice, num_blocks);
            let block_start = block_idx * TsduDeinterleaver::TRELLIS_DATA_DIBITS;
            let block_end = block_start + TsduDeinterleaver::TRELLIS_DATA_DIBITS;
            if data_dibits.len() < block_end {
                break;
            }
            let block_dibits = &data_dibits[block_start..block_end];
            self.tsbk_block_attempts += 1;
            self.tsbk_block_attempts_by_pos[block_idx] += 1;

            let decoded = match TrellisDecoder::decode(block_dibits) {
                Some(d) => d,
                None => {
                    self.tsbk_trellis_failures += 1;
                    // Continue to next block (matches the streaming
                    // process_tsdu_block "continue past failure" model).
                    continue;
                }
            };

            let block = TsbkBlock::parse(&decoded);
            let opcode_byte = (decoded[0] & 0x3F) as usize;
            let last_block_bit = block.last_block;
            match block.crc_valid(&decoded) {
                None => {
                    self.tsbk_crc_failures += 1;
                    self.tsbk_opcode_hist_fail[opcode_byte] += 1;
                    continue;
                }
                Some(crate::p25::tsbk::CrcConvention::Plain) => {
                    self.tsbk_crc_ok += 1;
                    self.tsbk_crc_ok_plain += 1;
                    self.tsbk_crc_ok_by_pos[block_idx] += 1;
                    self.tsbk_opcode_hist_ok[opcode_byte] += 1;
                    self.bump_mfid(block.manufacturer);
                }
                Some(crate::p25::tsbk::CrcConvention::Xored) => {
                    self.tsbk_crc_ok += 1;
                    self.tsbk_crc_ok_xored += 1;
                    self.tsbk_crc_ok_by_pos[block_idx] += 1;
                    self.tsbk_opcode_hist_ok[opcode_byte] += 1;
                    self.bump_mfid(block.manufacturer);
                }
            }

            if let Some(msg) = block.decode() {
                self.handle_tsbk(block_idx as u8, msg);
            } else {
                self.tsbk_unknown_opcode += 1;
            }

            // For directed reads we ALWAYS attempt all 3 blocks even
            // if a clean LB=1 was set early -- the soft sync correlator
            // gives us the dibit position for free, and at this layer
            // we don't know how much body is "really" supposed to follow
            // the LB bit. Reading 3 blocks always wastes at most 2 ×
            // 98 trellis dibits per "single block" TSDU, ~6 ms of CPU.
            // The CRC check still rejects garbage so the only "cost"
            // is a slightly higher tsbk_block_attempts denominator.
            let _ = last_block_bit;
        }
    }

    /// Phase 6F.8: clear ALL diagnostic counters and histograms (the
    /// `/api/decoder_reset` backend). Lets us measure a new
    /// `SYNC_THRESHOLD` value against a clean baseline window without
    /// rebooting. Preserves long-lived radio state (system identity,
    /// frequency band table, active grants, talkgroup aliases) so
    /// resetting doesn't wipe state the operator wants to keep.
    ///
    /// **6F.8 fix:** in 6F.7 the `/api/decoder_reset` handler only
    /// cleared a subset of counters and missed `sync_hits`,
    /// `sync_near_misses`, `total_dibits`, `dibit_hist`, and
    /// `recent_dibits`. The sweep tool divided the cumulative
    /// (lifetime) `sync_hits` by `total_dibits/4800` (also lifetime)
    /// and reported per-second rates that conflated lifetime average
    /// with the 30-second post-reset window. This method clears
    /// everything.
    pub fn reset_diagnostics(&mut self) {
        // NID/TSBK pipeline counters
        self.nid_attempts = 0;
        self.nid_decode_failures = 0;
        self.nid_invalid_duid = 0;
        self.nid_decoded_ok = 0;
        self.nid_decoded_tsdu = 0;
        self.tsdu_attempts = 0;
        self.tsbk_block_attempts = 0;
        self.tsbk_trellis_failures = 0;
        self.tsbk_crc_failures = 0;
        self.tsbk_crc_ok = 0;
        self.tsbk_crc_ok_plain = 0;
        self.tsbk_crc_ok_xored = 0;
        self.tsbk_unknown_opcode = 0;
        self.tsbk_opcode_hist_ok = [0; 64];
        self.tsbk_opcode_hist_fail = [0; 64];
        self.tsbk_mfid_hist_ok = [0; 4];
        self.tsbk_block_attempts_by_pos = [0; 3];
        self.tsbk_crc_ok_by_pos = [0; 3];
        // Sync stats (these were missed in 6F.7)
        self.sync_hits = 0;
        self.sync_near_misses = 0;
        self.best_sync_distance = u32::MAX;
        self.sync_distance_hist = [0; 25];
        // Dibit stats (also missed in 6F.7) -- total_dibits drives
        // the sweep tool's per-sec calculation, so it MUST be reset
        // for measurement windows to make sense.
        self.total_dibits = 0;
        self.dibit_hist = [0; 4];
        self.last_log_dibits = 0;
        self.raw_duid_hist = [0; 16];
        self.recent_dibits.clear();
        // Recent message log
        self.recent_messages.clear();
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

                // Phase 6F.6: bump the sync distance histogram on every
                // dibit shift once we have a full sync window. The hist
                // is the most informative diagnostic for "is the slicer
                // garbage?" -- if real syncs cluster at low distances
                // we just need to widen the threshold; if they smear
                // across distance 9-20 the slicer is corrupting half
                // the outer symbols and we need to fix the slicer.
                if self.dibit_count >= 24 {
                    let bucket = (distance as usize).min(24);
                    self.sync_distance_hist[bucket] += 1;
                }
                let runtime_threshold = RUNTIME_SYNC_THRESHOLD
                    .load(std::sync::atomic::Ordering::Relaxed);
                if distance <= SYNC_NEAR_LOG_THRESHOLD
                    && distance > runtime_threshold
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
                if distance <= runtime_threshold && self.dibit_count >= 24 {
                    self.sync_hits += 1;
                    self.nid_attempts += 1;
                    tracing::info!(
                        target: "p25_decoder",
                        "SYNC HIT #{}: distance={} (dibit #{}) -> ReadingNid",
                        self.sync_hits, distance, self.total_dibits,
                    );

                    // Phase 6F.2h: if armed, start a capture. Snapshot
                    // the 24 sync dibits from the sync_register and
                    // begin accumulating raw NID dibits.
                    if self.aligned_capture_armed {
                        let mut sync_d = Vec::with_capacity(24);
                        for x in 0..24 {
                            let shift = (23 - x) * 2;
                            sync_d.push(
                                ((self.sync_register >> shift) & 0x03) as u8,
                            );
                        }
                        self.capture_in_flight = Some(CaptureBuilder {
                            sync_dibits: sync_d,
                            sync_distance: distance,
                            raw_nid_dibits: Vec::with_capacity(NID_TRANSMITTED_DIBITS),
                            raw_body_dibits: Vec::new(),
                            total_dibits_at_capture: self.total_dibits,
                            ..Default::default()
                        });
                    }

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
                // Phase 6F.2h: append every NID-window dibit to the
                // in-flight capture (raw, including the status dibit).
                if let Some(cap) = self.capture_in_flight.as_mut() {
                    cap.raw_nid_dibits.push(dibit & 0x03);
                }

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
                                // Phase 6F.2h: finalize the in-flight
                                // capture as a BCH-reject snapshot.
                                if let Some(cap) = self.capture_in_flight.take() {
                                    self.aligned_capture = Some(AlignedCapture {
                                        sync_dibits: cap.sync_dibits,
                                        sync_distance: cap.sync_distance,
                                        raw_nid_dibits: cap.raw_nid_dibits,
                                        nid_bits: new_bits,
                                        bch_nac: None,
                                        bch_duid: None,
                                        raw_duid: 0,
                                        raw_body_dibits: Vec::new(),
                                        trellis_dibits: Vec::new(),
                                        tsbk_bytes: Vec::new(),
                                        crc_result: "nid_bch_reject".to_string(),
                                        total_dibits_at_capture: cap.total_dibits_at_capture,
                                    });
                                    self.aligned_capture_armed = false;
                                }
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

                        // Phase 6F.2h: stash BCH-success fields into the
                        // in-flight capture so process_tsdu can finalize.
                        if let Some(cap) = self.capture_in_flight.as_mut() {
                            cap.nid_bits = new_bits;
                            cap.bch_nac = Some(nac_raw);
                            cap.bch_duid = Some(duid_raw);
                            cap.raw_duid = on_air_duid;
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
                            self.tsdu_blocks_decoded = 0;
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

                // Phase 6F.2h: append every body dibit to the in-flight
                // capture so process_tsdu has the raw input to dump.
                if let Some(cap) = self.capture_in_flight.as_mut() {
                    cap.raw_body_dibits.push(dibit & 0x03);
                }

                if self.du_buffer.len() >= self.du_expected_len {
                    // Data unit complete (or one TSBK block complete in
                    // the multi-block case). Process and find out
                    // whether more dibits are needed.
                    let done = match duid {
                        DataUnit::Tsdu => self.process_tsdu_block(),
                        // Other DU types handled in later phases
                        _ => true,
                    };
                    if done {
                        self.state = DecoderState::Hunting;
                        self.dibit_count = 0;
                    }
                    // Otherwise stay in ReadingDataUnit and keep
                    // appending dibits until the new (extended)
                    // du_expected_len is reached for the next block.
                }
            }
        }
    }

    /// Process the next TSBK block of the in-flight TSDU.
    ///
    /// **Phase 6F.3 (2026-04-11) multi-block TSBK support, refined in
    /// 6F.4 to continue past CRC failures.**
    ///
    /// A single TSDU can carry one, two, or three TSBK blocks per
    /// SDRTrunk's `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_{1,2,3}`
    /// table; the block-1 header bit `LB` (last block) tells the
    /// receiver whether more blocks follow.
    ///
    /// **Phase 6F.4 change vs 6F.3:** when the current block fails
    /// trellis or CRC, we no longer abort the multi-block read.
    /// Instead, we treat it as `last_block=0` and continue to the next
    /// block boundary, mirroring SDRTrunk's
    /// `P25P1MessageFramer.dispatchTSBK()` behaviour:
    ///
    /// ```java
    /// else if(tsbk1.isValid() && tsbk1.isLastBlock()) {
    ///     mMessageAssembler = null;          // stop
    /// } else {
    ///     mMessageAssembler.reconfigure(TSBK_2);  // KEEP READING
    /// }
    /// ```
    ///
    /// On the Clay County test target almost every TSDU is a 3-block
    /// frame (TSBK1+TSBK2+TSBK3). Aborting on TSBK1 CRC fail meant we
    /// dropped TSBK2/TSBK3 ~55 % of the time, capping
    /// `tsbk_block_attempts/tsdu_attempts` at ~1.6 instead of the
    /// theoretical 3.0.
    ///
    /// This function is called once per `du_expected_len` boundary in
    /// the state machine. It:
    ///
    /// 1. Re-runs `TsduDeinterleaver::deinterleave_multi` over the
    ///    entire buffered body for the current block count
    ///    (`tsdu_blocks_decoded + 1`). O(303) once per TSDU.
    /// 2. Slices out the trellis dibits for THIS block (positions
    ///    `[block_idx*98 .. (block_idx+1)*98]`) and runs the Viterbi.
    /// 3. Validates the CRC, populates diagnostic histograms,
    ///    dispatches the parsed message via `handle_tsbk`.
    /// 4. Decides whether to continue:
    ///    - block index 2 (TSBK3): always done.
    ///    - CRC OK + LB=1: legitimately done.
    ///    - Anything else (CRC OK + LB=0, CRC fail, trellis fail):
    ///      extend `du_expected_len` and continue.
    fn process_tsdu_block(&mut self) -> bool {
        // The state machine resets tsdu_blocks_decoded=0 on entry into
        // ReadingDataUnit, so the first call into here is block 0 of a
        // fresh TSDU. tsdu_attempts is bumped on block 0 only.
        let block_idx = self.tsdu_blocks_decoded;
        if block_idx == 0 {
            self.tsdu_attempts += 1;
        }

        let num_blocks = block_idx + 1;
        let data_dibits =
            TsduDeinterleaver::deinterleave_multi(&self.du_buffer, num_blocks);
        let block_start = block_idx * TsduDeinterleaver::TRELLIS_DATA_DIBITS;
        let block_end = block_start + TsduDeinterleaver::TRELLIS_DATA_DIBITS;

        // The aligned capture is one-shot per TSDU and snapshots the
        // FIRST block's trellis input + bytes. Continuation blocks
        // don't update the capture.
        let trellis_dibits_for_capture: Vec<u8> = if block_idx == 0 {
            data_dibits.clone()
        } else {
            Vec::new()
        };
        let mut capture_decoded_bytes: Vec<u8> = Vec::new();
        let mut capture_crc_result: String = "no_block".to_string();

        // Defensive: short buffer means deinterleave_multi returned
        // less than expected. Treat as a hard failure for this block
        // but still continue to the next block boundary (Phase 6F.4
        // continue-past-failure model).
        let mut block_failed = false;
        let mut block_last_bit = false;

        if data_dibits.len() < block_end {
            block_failed = true;
            if block_idx == 0 {
                capture_crc_result = "short_dibits".to_string();
            }
        } else {
            let block_dibits = &data_dibits[block_start..block_end];
            self.tsbk_block_attempts += 1;
            self.tsbk_block_attempts_by_pos[block_idx] += 1;

            // 1. Trellis decode: 98 dibits -> 12 bytes
            match TrellisDecoder::decode(block_dibits) {
                None => {
                    self.tsbk_trellis_failures += 1;
                    block_failed = true;
                    if block_idx == 0 {
                        capture_crc_result = "trellis_fail".to_string();
                    }
                }
                Some(decoded) => {
                    if block_idx == 0 {
                        capture_decoded_bytes = decoded.to_vec();
                    }

                    // 2. Parse TSBK block and check CRC
                    let block = TsbkBlock::parse(&decoded);
                    let opcode_byte = (decoded[0] & 0x3F) as usize;
                    block_last_bit = block.last_block;
                    let crc = block.crc_valid(&decoded);
                    match crc {
                        None => {
                            self.tsbk_crc_failures += 1;
                            self.tsbk_opcode_hist_fail[opcode_byte] += 1;
                            block_failed = true;
                            if block_idx == 0 {
                                capture_crc_result = "crc_fail".to_string();
                            }
                        }
                        Some(crate::p25::tsbk::CrcConvention::Plain) => {
                            self.tsbk_crc_ok += 1;
                            self.tsbk_crc_ok_plain += 1;
                            self.tsbk_crc_ok_by_pos[block_idx] += 1;
                            self.tsbk_opcode_hist_ok[opcode_byte] += 1;
                            self.bump_mfid(block.manufacturer);
                            if block_idx == 0 {
                                capture_crc_result = "plain".to_string();
                            }
                        }
                        Some(crate::p25::tsbk::CrcConvention::Xored) => {
                            self.tsbk_crc_ok += 1;
                            self.tsbk_crc_ok_xored += 1;
                            self.tsbk_crc_ok_by_pos[block_idx] += 1;
                            self.tsbk_opcode_hist_ok[opcode_byte] += 1;
                            self.bump_mfid(block.manufacturer);
                            if block_idx == 0 {
                                capture_crc_result = "xored".to_string();
                            }
                        }
                    }

                    // 3. Decode opcode-specific payload (only on CRC OK)
                    if !block_failed {
                        if let Some(msg) = block.decode() {
                            self.handle_tsbk(block_idx as u8, msg);
                        } else {
                            self.tsbk_unknown_opcode += 1;
                        }
                    }
                }
            }
        }

        self.tsdu_blocks_decoded += 1;

        // Phase 6F.4 continue-past-failure: stop only if we got LB=1
        // from a CLEAN (CRC-OK) block, OR we just finished block 3.
        let cleanly_done = !block_failed && block_last_bit;
        let max_reached = self.tsdu_blocks_decoded >= TsduDeinterleaver::MAX_BLOCKS;

        if cleanly_done || max_reached {
            self.finalize_capture(
                trellis_dibits_for_capture,
                capture_decoded_bytes,
                capture_crc_result,
                block_idx,
            );
            return true;
        }

        // Extend du_expected_len to the next multi-block boundary.
        // body_dibits_for_blocks(2) = 231, (3) = 303.
        match TsduDeinterleaver::body_dibits_for_blocks(self.tsdu_blocks_decoded + 1) {
            Some(next_len) => {
                self.du_expected_len = next_len;
                // Block 0 finalises the capture immediately (capture
                // is the TSBK1 snapshot the diagnostic tools expect).
                if block_idx == 0 {
                    self.finalize_capture(
                        trellis_dibits_for_capture,
                        capture_decoded_bytes,
                        capture_crc_result,
                        0,
                    );
                }
                false
            }
            None => {
                self.finalize_capture(
                    trellis_dibits_for_capture,
                    capture_decoded_bytes,
                    capture_crc_result,
                    block_idx,
                );
                true
            }
        }
    }

    /// Phase 6F.4 mfid bucketing helper. We track three named
    /// vendors (standard 0x00, Motorola 0x90, Harris/Tait 0xA4) plus
    /// "other" so the dashboard can show the vendor mix without
    /// blowing up to a 256-entry histogram.
    fn bump_mfid(&mut self, mfid: u8) {
        match mfid {
            0x00 => self.tsbk_mfid_hist_ok[0] += 1,
            0x90 => self.tsbk_mfid_hist_ok[1] += 1,
            0xA4 => self.tsbk_mfid_hist_ok[2] += 1,
            _ => self.tsbk_mfid_hist_ok[3] += 1,
        }
    }

    /// Phase 6F.2h aligned-capture finaliser. Splits out so the
    /// multi-block process_tsdu_block has one place to drain the
    /// in-flight capture without duplicating the AlignedCapture
    /// construction.
    ///
    /// `block_idx` is purely for documentation; the capture itself is
    /// always the TSBK1 snapshot.
    fn finalize_capture(
        &mut self,
        trellis_dibits: Vec<u8>,
        tsbk_bytes: Vec<u8>,
        crc_result: String,
        _block_idx: usize,
    ) {
        if let Some(cap) = self.capture_in_flight.take() {
            self.aligned_capture = Some(AlignedCapture {
                sync_dibits: cap.sync_dibits,
                sync_distance: cap.sync_distance,
                raw_nid_dibits: cap.raw_nid_dibits,
                nid_bits: cap.nid_bits,
                bch_nac: cap.bch_nac,
                bch_duid: cap.bch_duid,
                raw_duid: cap.raw_duid,
                raw_body_dibits: cap.raw_body_dibits,
                trellis_dibits,
                tsbk_bytes,
                crc_result,
                total_dibits_at_capture: cap.total_dibits_at_capture,
            });
            self.aligned_capture_armed = false;
        }
    }

    /// Process a decoded TSBK message and update system state.
    /// `block_idx` is 0/1/2 = TSBK1/TSBK2/TSBK3 (used for diagnostic
    /// labels in the recent_messages log + WebSocket events).
    pub fn handle_tsbk(&mut self, block_idx: u8, msg: TsbkMessage) {
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
                // Drop any prior grant for this TG on a different
                // channel. The TSBK includes a fresh source RadioId
                // so we discard the preserved value here -- the new
                // call's caller is what we want to record.
                let _ = self.take_other_grants_for_talkgroup(*talkgroup);
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
            // Phase 6F.11: Secondary Control Channel Broadcast --
            // record the backup CCH A/B channels for the trunking
            // failover view. RFSS/SITE come along for the ride and
            // overwrite (always identical to the primary in practice).
            TsbkMessage::SecondaryControlChannelBroadcast {
                rfss_id,
                site_id,
                channel_a,
                channel_b,
            } => {
                self.system.rfss_id = Some(*rfss_id);
                self.system.site_id = Some(*site_id);
                self.system.secondary_cch_a = Some(*channel_a);
                self.system.secondary_cch_b = Some(*channel_b);
            }
            // Phase 6F.11: SNDCP Data Channel Announcement Explicit --
            // record the data services channels.
            TsbkMessage::SndcpDataChannelAnnouncementExplicit {
                downlink_channel,
                uplink_channel,
                ..
            } => {
                self.system.sndcp_downlink_channel = Some(*downlink_channel);
                self.system.sndcp_uplink_channel = Some(*uplink_channel);
            }
            // Phase 6F.11: TDMA Sync Broadcast -- snapshot system
            // clock for the activity feed / debug.
            TsbkMessage::TdmaSyncBroadcast {
                time_locked,
                year,
                month,
                day,
                hours,
                minutes,
                ..
            } => {
                self.system.last_sync_clock =
                    Some((*year, *month, *day, *hours, *minutes, *time_locked));
            }
            // Phase 6F.11: TELE_INT_VCH_GRANT_UPDATE -- another grant
            // type, but unit-to-phone (no talkgroup). Surface it via
            // the activity feed but DON'T push into `grants`, which
            // is talkgroup-keyed for now.
            TsbkMessage::TelephoneInterconnectVoiceChannelGrantUpdate {
                ..
            } => {}
            // Phase 6F.11: UU_ANS_REQ -- private call paging. Pure
            // event for the activity feed.
            TsbkMessage::UnitToUnitAnswerRequest { .. } => {}
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a,
                talkgroup_a,
                channel_b,
                talkgroup_b,
            } => {
                let freq_a = self.channel_to_frequency(*channel_a);
                // Drop any prior grant for talkgroup_a on a different
                // channel and PRESERVE its source -- the update TSBK
                // doesn't carry a source itself, so without this we'd
                // wipe the caller ID we recorded from the original
                // GroupVoiceChannelGrant.
                let preserved_source_a =
                    self.take_other_grants_for_talkgroup(*talkgroup_a);
                self.grants.insert(
                    channel_a.0,
                    GrantInfo {
                        channel: *channel_a,
                        talkgroup: *talkgroup_a,
                        source: preserved_source_a,
                        frequency_hz: freq_a,
                        timestamp: Instant::now(),
                    },
                );
                if talkgroup_b.0 != 0 {
                    let freq_b = self.channel_to_frequency(*channel_b);
                    let preserved_source_b =
                        self.take_other_grants_for_talkgroup(*talkgroup_b);
                    self.grants.insert(
                        channel_b.0,
                        GrantInfo {
                            channel: *channel_b,
                            talkgroup: *talkgroup_b,
                            source: preserved_source_b,
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
            let event = self.tsbk_to_event(block_idx, &msg);
            if let Ok(json) = serde_json::to_string(&event) {
                let _ = tx.send(json);
            }
        }

        // Log the message with its TSBK block index (0/1/2 = TSBK1/2/3).
        self.recent_messages.push((Instant::now(), block_idx, msg));
        if self.recent_messages.len() > self.max_recent {
            self.recent_messages.remove(0);
        }
    }

    /// Convert a TSBK message to a WebSocket event. Phase 6F.4: each
    /// event is now prefixed with the originating block label
    /// ("TSBK1"/"TSBK2"/"TSBK3") so the dashboard event feed matches
    /// the format of SDRTrunk's `decoded_messages.log`.
    fn tsbk_to_event(&self, block_idx: u8, msg: &TsbkMessage) -> p25_json::TsbkEvent {
        let now = chrono_timestamp();
        let block_label = match block_idx {
            0 => "TSBK1",
            1 => "TSBK2",
            2 => "TSBK3",
            _ => "TSBK?",
        };
        let block_prefix = format!("[{}] ", block_label);
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
                        "{}TG:{:05} -> {} ({:.4} MHz)",
                        block_prefix,
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
                summary: format!("{}TG:{:05} -> {}", block_prefix, talkgroup_a.0, channel_a),
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
                summary: format!("{}WACN:{:05X} SYS:{:03X} CH:{}", block_prefix, wacn, system_id, channel),
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
                summary: format!("{}RFSS:{:02} SITE:{:02}", block_prefix, rfss_id, site_id),
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
                    "{}Band:{} base:{:.5} MHz spacing:{} Hz",
                    block_prefix,
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
                    "{}SYS:{:03X} RFSS:{:02} SITE:{:02}",
                    block_prefix, system_id, rfss_id, site_id
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            // Phase 6F.11 new opcodes
            TsbkMessage::SecondaryControlChannelBroadcast {
                channel_a, channel_b, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "SCCB".into(),
                summary: format!(
                    "{}A:{} B:{}",
                    block_prefix, channel_a, channel_b
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", channel_a)),
                frequency_mhz: self
                    .channel_to_frequency(*channel_a)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::SndcpDataChannelAnnouncementExplicit {
                downlink_channel,
                uplink_channel,
                ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "SNDCP_ANN".into(),
                summary: format!(
                    "{}DL:{} UL:{}",
                    block_prefix, downlink_channel, uplink_channel
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: Some(format!("{}", downlink_channel)),
                frequency_mhz: self
                    .channel_to_frequency(*downlink_channel)
                    .map(|f| f as f64 / 1e6),
                source: None,
            },
            TsbkMessage::TdmaSyncBroadcast {
                year, month, day, hours, minutes, time_locked, ..
            } => p25_json::TsbkEvent {
                timestamp: now,
                event_type: "TDMA_SYNC".into(),
                summary: format!(
                    "{}{:04}-{:02}-{:02} {:02}:{:02} {}",
                    block_prefix, year, month, day, hours, minutes,
                    if *time_locked { "LOCKED" } else { "UNLOCKED" }
                ),
                talkgroup: None,
                talkgroup_alias: None,
                channel: None,
                frequency_mhz: None,
                source: None,
            },
            TsbkMessage::TelephoneInterconnectVoiceChannelGrantUpdate {
                channel, call_timer_secs, unit_id,
            } => {
                let freq = self.channel_to_frequency(*channel);
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "TEL_INT_GRANT_UPD".into(),
                    summary: format!(
                        "{}UNIT:{} CH:{} ({:.4} MHz) timer:{}s",
                        block_prefix, unit_id, channel,
                        freq.unwrap_or(0) as f64 / 1e6,
                        call_timer_secs,
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: Some(format!("{}", channel)),
                    frequency_mhz: freq.map(|f| f as f64 / 1e6),
                    source: Some(unit_id.0),
                }
            }
            TsbkMessage::UnitToUnitAnswerRequest { target, source } => {
                p25_json::TsbkEvent {
                    timestamp: now,
                    event_type: "UU_ANS_REQ".into(),
                    summary: format!(
                        "{}TGT:{} SRC:{}",
                        block_prefix, target, source
                    ),
                    talkgroup: None,
                    talkgroup_alias: None,
                    channel: None,
                    frequency_mhz: None,
                    source: Some(source.0),
                }
            }
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

    /// Drop any existing grants that match `talkgroup` and return the
    /// `source` RadioId from the first matching entry (if any), so the
    /// caller can preserve the original caller ID across a refresh.
    ///
    /// In real trunking, a single talkgroup is on one voice channel
    /// at a time -- when the system grants TG `T` to a new channel,
    /// any prior `T` grant on a different channel is by definition
    /// no longer active. The decoder's `grants` map is keyed by
    /// channel number (so a grant on channel A and a grant on
    /// channel B are two HashMap entries even if they're for the
    /// same TG), which means the natural insert path leaves the old
    /// A entry sitting around until `expire_grants` reaps it.
    ///
    /// Returning the prior `source` lets `GroupVoiceChannelGrantUpdate`
    /// preserve the caller ID across refreshes -- the
    /// `GroupVoiceChannelGrant` opcode includes a source RadioId, but
    /// `GroupVoiceChannelGrantUpdate` does NOT, so without this preserve
    /// path the source would get wiped to `None` the first time the
    /// trunking system refreshed an active call. The caller in
    /// `handle_tsbk` gets to decide whether to use the returned source
    /// (update path) or ignore it and use a fresh source from the TSBK
    /// itself (initial-grant path).
    ///
    /// Wildcard TG 0 is excluded because the grant-update path already
    /// filters it as a sentinel and dropping all "TG 0" entries would
    /// clobber unrelated state.
    fn take_other_grants_for_talkgroup(&mut self, talkgroup: Talkgroup) -> Option<RadioId> {
        if talkgroup.0 == 0 {
            return None;
        }
        let mut preserved_source: Option<RadioId> = None;
        self.grants.retain(|_, g| {
            if g.talkgroup == talkgroup {
                // Capture the source from the first match. Don't
                // overwrite if we already have one (in case the map
                // somehow holds two stale entries for the same TG).
                if preserved_source.is_none() && g.source.is_some() {
                    preserved_source = g.source;
                }
                false
            } else {
                true
            }
        });
        preserved_source
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_identity_tracking() {
        let mut decoder = ControlChannelDecoder::new();

        // Simulate NET_STS_BCST
        decoder.handle_tsbk(0, TsbkMessage::NetworkStatus {
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
        decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
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
        decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
            identifier: 0,
            bw: 100,
            transmit_offset: -45_000_000,
            channel_spacing: 6_250,
            base_frequency: 851_006_250,
        });

        // Voice grant
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x045D), // band 0, ch 1117
            talkgroup: Talkgroup(300),
            source: RadioId(1011),
        });

        assert!(decoder.grants.contains_key(&0x045D));
        let grant = &decoder.grants[&0x045D];
        assert_eq!(grant.talkgroup.0, 300);
        assert_eq!(grant.frequency_hz, Some(857_987_500)); // 857.9875 MHz
    }

    /// A new grant for the same talkgroup on a different channel
    /// must drop the prior grant entry. The dashboard's
    /// `/api/grants` was showing the same TG repeated 5+ times across
    /// different channels with ages spanning ~30 minutes -- the
    /// underlying state machine was carrying stale rows in
    /// `decoder.grants` because the map is keyed by channel.
    #[test]
    fn test_grant_dedup_by_talkgroup() {
        let mut decoder = ControlChannelDecoder::new();
        decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
            identifier: 0,
            bw: 100,
            transmit_offset: -45_000_000,
            channel_spacing: 6_250,
            base_frequency: 851_006_250,
        });

        // First grant: TG 202 on channel 0x0345.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x0345),
            talkgroup: Talkgroup(202),
            source: RadioId(1011),
        });
        // Independent TG on a third channel -- must NOT be cleared.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x0500),
            talkgroup: Talkgroup(300),
            source: RadioId(2022),
        });
        assert_eq!(decoder.grants.len(), 2);

        // Second grant: same TG 202 on a different channel. The
        // prior 0x0345 entry should be dropped, leaving exactly two
        // grants total (the new TG 202 + the unrelated TG 300).
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x045D),
            talkgroup: Talkgroup(202),
            source: RadioId(1011),
        });
        assert_eq!(decoder.grants.len(), 2);
        assert!(!decoder.grants.contains_key(&0x0345));
        assert!(decoder.grants.contains_key(&0x045D));
        assert!(decoder.grants.contains_key(&0x0500));

        // GroupVoiceChannelGrantUpdate must dedupe the same way.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrantUpdate {
            channel_a: Channel(0x0789),
            talkgroup_a: Talkgroup(202),
            channel_b: Channel(0),
            talkgroup_b: Talkgroup(0),
        });
        assert_eq!(decoder.grants.len(), 2);
        assert!(!decoder.grants.contains_key(&0x045D));
        assert!(decoder.grants.contains_key(&0x0789));
        assert!(decoder.grants.contains_key(&0x0500));
    }

    /// `GroupVoiceChannelGrantUpdate` does not carry a source RadioId
    /// field, but the original `GroupVoiceChannelGrant` does. When an
    /// update arrives for an existing TG the dedup path must
    /// **preserve** the prior source so the dashboard's caller ID
    /// doesn't drop to None on every periodic refresh.
    #[test]
    fn test_grant_update_preserves_source_id() {
        let mut decoder = ControlChannelDecoder::new();
        decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
            identifier: 0,
            bw: 100,
            transmit_offset: -45_000_000,
            channel_spacing: 6_250,
            base_frequency: 851_006_250,
        });

        // Initial grant: TG 202, source = radio 1011, on channel 0x0345.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x0345),
            talkgroup: Talkgroup(202),
            source: RadioId(1011),
        });
        assert_eq!(
            decoder.grants[&0x0345].source,
            Some(RadioId(1011)),
            "initial grant should record the source from the TSBK",
        );

        // Update on the SAME channel: source should be preserved.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrantUpdate {
            channel_a: Channel(0x0345),
            talkgroup_a: Talkgroup(202),
            channel_b: Channel(0),
            talkgroup_b: Talkgroup(0),
        });
        assert_eq!(decoder.grants.len(), 1);
        assert_eq!(
            decoder.grants[&0x0345].source,
            Some(RadioId(1011)),
            "update on the same channel must preserve the original source",
        );

        // Update that MOVES the call to a different channel: source
        // should still be preserved across the dedup-and-reinsert.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrantUpdate {
            channel_a: Channel(0x0789),
            talkgroup_a: Talkgroup(202),
            channel_b: Channel(0),
            talkgroup_b: Talkgroup(0),
        });
        assert_eq!(decoder.grants.len(), 1);
        assert!(!decoder.grants.contains_key(&0x0345));
        assert!(decoder.grants.contains_key(&0x0789));
        assert_eq!(
            decoder.grants[&0x0789].source,
            Some(RadioId(1011)),
            "update across channels must still preserve the original source",
        );

        // A NEW initial grant for the same TG with a DIFFERENT source
        // (a new caller starting a new call) must overwrite the source
        // with the fresh value, not preserve the stale 1011.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x0900),
            talkgroup: Talkgroup(202),
            source: RadioId(2022),
        });
        assert_eq!(decoder.grants.len(), 1);
        assert!(decoder.grants.contains_key(&0x0900));
        assert_eq!(
            decoder.grants[&0x0900].source,
            Some(RadioId(2022)),
            "a new initial grant must use the new source from the TSBK, \
             not preserve the prior caller",
        );

        // Update for a TG we've never seen before -- nothing to
        // preserve, source must be None.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrantUpdate {
            channel_a: Channel(0x0AA0),
            talkgroup_a: Talkgroup(555),
            channel_b: Channel(0),
            talkgroup_b: Talkgroup(0),
        });
        assert_eq!(
            decoder.grants[&0x0AA0].source,
            None,
            "update for a previously-unseen TG must have source = None",
        );
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

    /// **Phase 6F.3 multi-block TSBK e2e regression guard.** Build a
    /// real 2-block TSDU body (TSBK1 last_block=0, TSBK2 last_block=1)
    /// with valid CCITT_80 CRCs, trellis-encode each 12-byte block, and
    /// splice in the 7 status dibits at body raw positions
    /// {13,49,85,121,157,193,229} plus 28 trailing null padding dibits.
    /// Drive the decoder end-to-end (sync + NID + body) and verify:
    ///
    /// 1. `tsdu_attempts` == 1 (one TSDU sync hit)
    /// 2. `tsbk_block_attempts` == 2 (two TSBK blocks decoded)
    /// 3. `tsbk_crc_ok` == 2 (both CRCs validated)
    /// 4. The two messages dispatched correctly:
    ///    - Block 1: NetworkStatusBroadcast (0x3B) updates `system.wacn`
    ///    - Block 2: RfssStatusBroadcast (0x3A) updates `system.rfss_id`
    /// 5. Decoder returns to Hunting after the second block.
    ///
    /// Until 6F.3 the decoder always read 123 raw body dibits and
    /// stopped, so a 2-block TSDU would either get cut off at TSBK1
    /// (losing TSBK2 entirely) or fail TSBK1 CRC because the
    /// status-dibit positions for TSBK2 hadn't yet been consumed.
    #[test]
    fn test_multi_block_tsbk_e2e() {
        use crate::lsm::nid_fec;
        use crate::p25::fec::trellis_encode_bytes;
        use crate::p25::tsbk::ccitt80_crc;

        fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(n_dibits);
            for i in (0..n_dibits).rev() {
                out.push(((bits >> (i * 2)) & 0x3) as u8);
            }
            out
        }

        // Helper: take 12 TSBK bytes minus the trailing CRC, compute
        // the CCITT_80 CRC for "Plain" convention (residual==0), and
        // splice it into bytes[10..12]. Returns the finalized 12-byte
        // block ready for trellis_encode_bytes.
        fn finalize_tsbk(mut bytes: [u8; 12]) -> [u8; 12] {
            // The CRC covers the first 80 bits = bytes[0..10]. We want
            // residual = calc XOR msg_crc == 0, so msg_crc = calc.
            let calc = ccitt80_crc(&bytes);
            bytes[10] = (calc >> 8) as u8;
            bytes[11] = (calc & 0xFF) as u8;
            bytes
        }

        // ── TSBK1: NET_STS_BCST (opcode 0x3B), LB=0 (NOT last block) ──
        // Same payload layout as test_net_sts_bcst_decode but with
        // LB=0 in the header byte.
        let tsbk1_raw = [
            0x3B, // LB=0, P=0, opcode=0x3B (NetworkStatusBroadcast)
            0x00, // standard manufacturer
            0x00, // payload[0]: LRA
            0xBE, // payload[1]: WACN bits 19-12
            0xE0, // payload[2]: WACN bits 11-4
            0x08, // payload[3]: WACN bits 3-0 | system_id bits 11-8
            0xA0, // payload[4]: system_id bits 7-0
            0x06, // payload[5]: channel high
            0x39, // payload[6]: channel low
            0x00, // payload[7]: services
            0x00, 0x00, // CRC placeholder
        ];
        let tsbk1 = finalize_tsbk(tsbk1_raw);

        // ── TSBK2: RFSS_STS_BCST (opcode 0x3A), LB=1 (LAST block) ──
        // Phase 6F.4 layout (matches SDRTrunk RFSSStatusBroadcast.java):
        // payload[0] = LRA, payload[1..2] = system_id (12 bits at bits
        // 28-39), payload[3] = RFSS, payload[4] = SITE,
        // payload[5..6] = freq_band(4) | channel_number(12).
        let tsbk2_raw = [
            0xBA, // LB=1, P=0, opcode=0x3A
            0x00, // standard manufacturer
            0x00, // payload[0]: LRA
            0x00, // payload[1]: bits 24-27 reserved/active flag,
                  //              bits 28-31 = system high nibble (0)
            0x00, // payload[2]: bits 32-39 = system low byte (0)
            0x01, // payload[3]: RFSS ID = 1
            0x01, // payload[4]: SITE ID = 1
            0x06, // payload[5]: freq_band(4)=0 | channel_number high(4)=0x6
            0x39, // payload[6]: channel_number low(8)=0x39
            0x00, // payload[7]: system service class
            0x00, 0x00, // CRC placeholder
        ];
        let tsbk2 = finalize_tsbk(tsbk2_raw);

        // Trellis-encode each block to 98 on-air dibits.
        let tsbk1_dibits = trellis_encode_bytes(&tsbk1);
        let tsbk2_dibits = trellis_encode_bytes(&tsbk2);

        // Concatenate the two blocks → 196 trellis dibits, then
        // append 28 null dibits → 224 dibits, then splice in the 7
        // status dibits at positions {13,49,85,121,157,193,229} →
        // 231 raw body dibits. The decoder will reverse this.
        let mut data: Vec<u8> = Vec::with_capacity(224);
        data.extend_from_slice(&tsbk1_dibits);
        data.extend_from_slice(&tsbk2_dibits);
        // 28 trailing null dibits (value doesn't matter -- gets stripped)
        for _ in 0..28 {
            data.push(0);
        }
        assert_eq!(data.len(), 224);

        let status_positions = [13usize, 49, 85, 121, 157, 193, 229];
        let mut body: Vec<u8> = Vec::with_capacity(231);
        let mut data_iter = data.into_iter();
        for i in 0..231 {
            if status_positions.contains(&i) {
                body.push(0x01); // status dibit -- value gets stripped
            } else {
                body.push(data_iter.next().unwrap());
            }
        }
        assert_eq!(body.len(), 231);

        // Build sync + NID for Clay County NAC=0x8A1, DUID=0x7 (TSDU).
        let nid_bits = nid_fec::encode_nid(0x8A1, 0x7);
        let nid_dibits_32 = unpack_dibits(nid_bits, 32);
        let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
        on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
        on_air_nid.push(0x0); // status dibit (value irrelevant -- skipped)
        on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
        let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);

        // Drive the decoder.
        let mut decoder = ControlChannelDecoder::new();
        for &d in &fs_dibits {
            decoder.process_dibit(d);
        }
        for &d in &on_air_nid {
            decoder.process_dibit(d);
        }
        for &d in &body {
            decoder.process_dibit(d);
        }

        // Verify counters: one TSDU, two blocks, both CRCs OK.
        assert_eq!(
            decoder.tsdu_attempts, 1,
            "expected 1 TSDU attempt, got {}",
            decoder.tsdu_attempts
        );
        assert_eq!(
            decoder.tsbk_block_attempts, 2,
            "expected 2 TSBK block attempts (TSBK1 + TSBK2), got {}",
            decoder.tsbk_block_attempts
        );
        assert_eq!(
            decoder.tsbk_crc_ok, 2,
            "expected 2 TSBK CRC successes, got {} (failures: trellis={} crc={})",
            decoder.tsbk_crc_ok,
            decoder.tsbk_trellis_failures,
            decoder.tsbk_crc_failures,
        );

        // Verify both messages dispatched: TSBK1 set wacn,
        // TSBK2 set rfss_id.
        assert_eq!(
            decoder.system.wacn,
            Some(0xBEE00),
            "TSBK1 NetworkStatus should have set wacn=0xBEE00"
        );
        assert_eq!(
            decoder.system.rfss_id,
            Some(0x01),
            "TSBK2 RfssStatus should have set rfss_id=1"
        );
    }

    /// Single-block TSBK regression: confirm `last_block=1` on the
    /// FIRST block correctly terminates after TSBK1 without trying to
    /// read 108 more dibits for an imaginary TSBK2. Otherwise the
    /// decoder would silently consume the next sync window's dibits
    /// and fall out of sync.
    #[test]
    fn test_single_block_tsbk_terminates_on_lb1() {
        use crate::lsm::nid_fec;
        use crate::p25::fec::trellis_encode_bytes;
        use crate::p25::tsbk::ccitt80_crc;

        fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
            let mut out = Vec::with_capacity(n_dibits);
            for i in (0..n_dibits).rev() {
                out.push(((bits >> (i * 2)) & 0x3) as u8);
            }
            out
        }

        let mut tsbk1_raw = [
            0xBB, // LB=1, opcode=0x3B
            0x00, 0x00, 0xBE, 0xE0, 0x08, 0xA0, 0x06, 0x39, 0x00, 0x00, 0x00,
        ];
        let calc = ccitt80_crc(&tsbk1_raw);
        tsbk1_raw[10] = (calc >> 8) as u8;
        tsbk1_raw[11] = (calc & 0xFF) as u8;

        let tsbk1_dibits = trellis_encode_bytes(&tsbk1_raw);
        // 98 trellis + 21 null = 119 non-status dibits, then splice 4
        // status dibits at body positions {13,49,85,121} → 123 raw.
        let mut data: Vec<u8> = Vec::with_capacity(119);
        data.extend_from_slice(&tsbk1_dibits);
        for _ in 0..21 {
            data.push(0);
        }
        let status_positions = [13usize, 49, 85, 121];
        let mut body: Vec<u8> = Vec::with_capacity(123);
        let mut data_iter = data.into_iter();
        for i in 0..123 {
            if status_positions.contains(&i) {
                body.push(0x01);
            } else {
                body.push(data_iter.next().unwrap());
            }
        }

        let nid_bits = nid_fec::encode_nid(0x8A1, 0x7);
        let nid_dibits_32 = unpack_dibits(nid_bits, 32);
        let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
        on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
        on_air_nid.push(0x0);
        on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
        let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);

        let mut decoder = ControlChannelDecoder::new();
        for &d in &fs_dibits {
            decoder.process_dibit(d);
        }
        for &d in &on_air_nid {
            decoder.process_dibit(d);
        }
        for &d in &body {
            decoder.process_dibit(d);
        }

        assert_eq!(decoder.tsdu_attempts, 1);
        assert_eq!(
            decoder.tsbk_block_attempts, 1,
            "single-block TSBK with LB=1 must NOT trigger a second block read"
        );
        assert_eq!(decoder.tsbk_crc_ok, 1);
        assert_eq!(decoder.system.wacn, Some(0xBEE00));
        // After TSBK1 with LB=1, we should be back in Hunting.
        assert!(matches!(decoder.state, DecoderState::Hunting));
    }
}

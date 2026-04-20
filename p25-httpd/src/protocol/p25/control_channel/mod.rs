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

mod tsbk_handlers;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

/// Simple ISO 8601-ish timestamp for events
pub fn chrono_timestamp() -> String {
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

use super::fec::{TrellisDecoder, TsduDeinterleaver};
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
    /// (0..=3). When TSBK1 finishes and `last_block` is not set we
    /// extend `du_expected_len` to 231 (TSBK2) or 303 (TSBK3) and
    /// bump this counter on each successful decode. Reset to 0 on
    /// every Hunting transition.
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
    /// Histogram of the *raw* DUID values seen before BCH correction.
    /// 16 buckets, indexed by raw 4-bit DUID. A healthy control channel
    /// should be ~100% in bucket 7 (TSDU).
    raw_duid_hist: [u64; 16],

    // ── Diagnostic counters: pipeline failure breakdown ─
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
    /// false. Canonical "we have dibits but they're corrupted past
    /// trellis FEC capacity" indicator.
    pub tsbk_crc_failures: u64,
    /// TSBK blocks that decoded cleanly through CRC and produced a
    /// `TsbkMessage`.
    pub tsbk_crc_ok: u64,
    /// Subset of `tsbk_crc_ok` where CRC validated as **plain**
    /// (`crc16_ccitt(data) == msg_crc`). Diagnostic for which CRC
    /// convention real on-air TSBKs use.
    pub tsbk_crc_ok_plain: u64,
    /// Subset of `tsbk_crc_ok` where CRC validated as **xor 0xFFFF**
    /// (`crc16_ccitt(data) ^ 0xFFFF == msg_crc`).
    pub tsbk_crc_ok_xored: u64,
    /// TSBK blocks that survived CRC but the opcode parser couldn't
    /// turn into a known TsbkMessage variant.
    pub tsbk_unknown_opcode: u64,

    // ── Diagnostic histograms ──
    /// Per-opcode histogram of CRC-OK TSBK blocks. Indexed by the
    /// 6-bit opcode value (`bytes[0] & 0x3F`). Shows the on-air opcode
    /// distribution and which opcodes we're missing parsers for.
    pub tsbk_opcode_hist_ok: [u64; 64],
    /// Per-opcode histogram of CRC-FAIL TSBK blocks. Same layout as
    /// `tsbk_opcode_hist_ok`. A high count for a particular opcode
    /// with a clean distribution matches confirmed real opcodes that
    /// just had bit errors past trellis correction.
    pub tsbk_opcode_hist_fail: [u64; 64],
    /// Per-vendor-mfid histogram on CRC-OK blocks. Index 0 = standard
    /// (0x00), 1 = Motorola (0x90), 2 = Harris/Tait (0xA4),
    /// 3 = other.
    pub tsbk_mfid_hist_ok: [u64; 4],
    /// Per-block-position attempt counters (block 0 = TSBK1,
    /// 1 = TSBK2, 2 = TSBK3). Confirms multi-block continuation
    /// is actually firing.
    pub tsbk_block_attempts_by_pos: [u64; 3],
    /// Per-block-position CRC-OK counters. Compared against
    /// `tsbk_block_attempts_by_pos` for per-position CRC success rate.
    pub tsbk_crc_ok_by_pos: [u64; 3],

    /// Sync distance histogram. Indexed by Hamming distance bucket
    /// (0..=23, with bucket 24 = "anything ≥ 24"). Bumped on every
    /// dibit shift in Hunting state once we have a full 24-dibit sync
    /// window. Diagnostic for whether real syncs cluster at low
    /// distances (slicer fine, just need to widen threshold) or smear
    /// across higher distances (slicer is corrupting half the
    /// outer-symbol bits in the all-outer sync pattern, widening
    /// can't help).
    ///
    /// Verification showed the PS hard correlator and PL HDL hard
    /// correlator both stuck at ~4.7 sync hits/sec while the soft-
    /// decision IQ correlator gets 9/sec on the same signal. The
    /// dibit-correlator hard sync rate is the dominant throughput
    /// bottleneck.
    pub sync_distance_hist: [u64; 25],

    // ── Aligned capture (one-shot diagnostic) ──
    /// Set to `true` by `/api/control_iq_capture_aligned` to request a full
    /// pipeline trace on the NEXT sync hit. Cleared by the decoder as
    /// soon as it captures one frame.
    pub aligned_capture_armed: bool,
    /// Snapshot populated when `aligned_capture_armed` was true at the
    /// moment of a sync hit. Read by `/api/control_iq_capture_aligned` and
    /// then cleared.
    pub aligned_capture: Option<AlignedCapture>,
    /// Internal: when non-None, the decoder is in the middle of
    /// capturing a frame and this is the buffer holding the in-flight
    /// raw NID + body dibits.
    capture_in_flight: Option<CaptureBuilder>,

    // ── NID batch capture ring ──
    /// When `true`, the decoder pushes a minimal `AlignedCapture` (NID
    /// fields only, no trellis/TSBK) to `capture_ring` on every sync
    /// event with a populated NID window. Disarms automatically when
    /// the ring reaches `capture_ring_limit`. Armed via
    /// `/api/nid_capture?side=control|traffic&arm=1`.
    pub capture_ring_armed: bool,
    /// Ring buffer of NID-level captures populated while
    /// `capture_ring_armed` is true. Read + drained by
    /// `/api/nid_capture`. Sized at runtime via
    /// `arm_capture_ring(limit)`; default cap is 256 entries,
    /// hard ceiling 1024.
    pub capture_ring: std::collections::VecDeque<AlignedCapture>,
    /// Maximum entries in `capture_ring` before auto-disarm. Set by
    /// `arm_capture_ring`. Zero means the ring is disarmed.
    pub capture_ring_limit: usize,

    // ── Runtime BCH-t override ──
    /// When `Some(n)`, the decoder rejects any BCH-corrected NID
    /// whose `n_errors > n`. When `None`, the default `T_MAX_ERRORS`
    /// (11) threshold is used.
    pub bch_t_override: Option<u32>,
    /// Per-decoder sync correlator threshold override. When `Some(n)`,
    /// this decoder instance uses `n` as its Hamming-distance cutoff
    /// for sync hits regardless of `RUNTIME_SYNC_THRESHOLD`. Lets us
    /// tighten the traffic-side decoder (where inter-LDU noise
    /// generates sync false-positives) while leaving control-side
    /// permissive for marginal TSBK recovery. Tuned via
    /// `/api/sync_tune?side=traffic&value=N`.
    pub sync_threshold_override: Option<u32>,

    /// System identity
    pub system: SystemIdentity,
    /// Frequency band table (from IDEN_UP messages)
    pub bands: HashMap<u8, FrequencyBand>,
    /// Active grants (channel -> grant info)
    pub grants: HashMap<u16, GrantInfo>,
    /// Recent TSBK messages for logging. Tuple is `(instant, block_idx,
    /// message)` where `block_idx` is 0/1/2 = TSBK1/TSBK2/TSBK3 within
    /// the parent TSDU, matching SDRTrunk's `decoded_messages.log`
    /// format ("TSBK1 NET_STS_BCAST...").
    pub recent_messages: Vec<(Instant, u8, TsbkMessage)>,
    /// Max recent messages to keep
    max_recent: usize,
    /// Talkgroup aliases (ID -> name)
    pub aliases: HashMap<u16, String>,
    /// Broadcast channel for WebSocket events
    event_tx: Option<broadcast::Sender<String>>,
    /// Typed grant event channel for the grant follower task.
    grant_event_tx: Option<tokio::sync::mpsc::Sender<super::events::P25Event>>,
    /// Optional voice frame handler. When set, the decoder dispatches
    /// HDU/LDU1/LDU2/TDU/TDU_LC bodies to the handler in addition to
    /// the normal TSDU dispatch. Set on the `traffic_lsm_decoder`
    /// instance in main.rs; left `None` on the control-channel
    /// decoder instances which never see voice channel frames. The
    /// handler owns the decoded payload (e.g. `ImbeFrameRaw` for LDUs)
    /// and forwards it downstream.
    pub voice_handler: Option<Arc<dyn VoiceHandler + Send + Sync>>,
    /// Optional structured event log. When set, the decoder emits one
    /// `Duid` entry per successful NID decode (post-BCH, pre-dispatch)
    /// including the chain label, NAC, DUID, and BCH-error count.
    /// Paired with the `chain_label` field — "control" / "traffic" /
    /// "ps_c4fm" — so log consumers can filter by chain.
    pub event_log: Option<Arc<crate::services::event_log::EventLog>>,
    /// Label inserted into every `Duid` log entry emitted by this
    /// decoder. Defaults to "control"; main.rs overrides for the
    /// traffic and C4FM decoder instances.
    pub chain_label: &'static str,
    /// Cumulative count of LDU1 frames the decoder has successfully
    /// framed and dispatched. Per-call rate is computed downstream
    /// from successive snapshots.
    pub ldu1_count: u64,
    pub ldu2_count: u64,
    pub hdu_count: u64,
    pub tdu_count: u64,
    pub tdu_lc_count: u64,
}

/// Voice frame handler trait. Implementations consume the 9 raw IMBE
/// frames extracted from each LDU and forward them to a downstream
/// consumer (vocoder, RTP broadcaster, file recorder, etc).
///
/// Methods take `&self` so the trait object can be shared across
/// the decoder + the downstream consumer. Implementations are
/// expected to use interior mutability (e.g. an mpsc Sender, an
/// AtomicU64 counter) where state is needed.
///
/// The decoder lives in the `p25` module which has no knowledge of
/// `tokio::sync::mpsc`, `TrafficStats`, or any of the per-binary
/// types. A trait keeps the decoder library-style and lets `main.rs`
/// plug in whatever consumer it wants.
pub trait VoiceHandler {
    /// Called once per successfully-framed LDU1 with the 9 raw IMBE
    /// frames in transmission order. `body_raw` is passed so the
    /// handler can parse the LDU1 Link Control Word via
    /// `voice_frame::parse_ldu1_lcw` to recover the mid-call
    /// `FM:<source>` / `TO:<TG>` / encryption flag.
    ///
    /// Default impl is a no-op so implementations can choose to
    /// only override the methods they care about.
    fn on_ldu1(
        &self,
        _frames: &[crate::protocol::p25::voice_frame::ImbeFrameRaw; 9],
        _body_raw: &[u8],
    ) {
    }

    /// Called once per successfully-framed LDU2. `body_raw` is passed
    /// so the handler can parse the LDU2 Encryption Sync Signature via
    /// `voice_frame::parse_ldu2_ess` and recover the 72-bit MI +
    /// algorithm + key id refreshed by every LDU2.
    fn on_ldu2(
        &self,
        _frames: &[crate::protocol::p25::voice_frame::ImbeFrameRaw; 9],
        _body_raw: &[u8],
    ) {
    }

    /// Called once per HDU. `body_raw` is passed so the handler can
    /// run `voice_frame::parse_hdu_body` and recover the 120-bit
    /// header (MI, Algorithm, Key ID, TG) via Golay18 + RS(63,47,17).
    fn on_hdu(&self, _body_raw: &[u8]) {}

    /// Called once per TDU (DUID 0x3, no payload).
    fn on_tdu(&self) {}

    /// Called once per TDU_LC (DUID 0xF). `body_raw` is the raw body
    /// dibit slice (159 dibits incl. status) so the handler can parse
    /// the Link Control Word with `voice_frame::parse_tdulc_lcw` and
    /// pick up the Motorola `TALK_COMPLETE` BY: source.
    fn on_tdu_lc(&self, _body_raw: &[u8]) {}
}

mod types;
pub use types::{
    AlignedCapture, SystemIdentity, GrantInfo, PreservedGrantFields,
};
use types::{CaptureBuilder, DecoderState};


/// Frame sync pattern as dibits packed into u64
/// The sync word is 24 symbols (48 bits): 0x5575F5FF77FF
/// Stored as 24 dibits in the low 48 bits
const FRAME_SYNC_DIBIT_PATTERN: u64 = 0x5575_F5FF_77FF;
const FRAME_SYNC_MASK: u64 = 0xFFFF_FFFF_FFFF; // 48 bits

/// Maximum Hamming distance for sync detection.
///
/// Sync threshold is RUNTIME-TUNABLE via `RUNTIME_SYNC_THRESHOLD`
/// (AtomicU32). This constant is the boot default. Use
/// `/api/sync_tune?threshold=N` to experiment without reflashing --
/// the optimal threshold depends on PLL lock state and varies over
/// time.
///
/// History: widening 4 → 8 had no effect on sync hit rate because
/// there are essentially no real syncs at distance 5-8 in this
/// signal. A mass of "near" sync events at distance 9-14 suggested
/// widening to 14 — cross-checked against the PL HDL gateware NID
/// extractor (its own hard sync detector) which also stuck at
/// ~4.7 NID events/sec. Both PS and PL hard correlators on the HDL
/// slicer's dibit stream agree: the slicer is producing too many
/// bit errors per outer-symbol sync dibit for the dibit-level
/// correlator to find better matches. The soft-decision IQ
/// correlator on raw IQ samples gets 9/sec, confirming syncs ARE
/// out there at the sample level but the slicer is dropping them.
///
/// At threshold 14 the random-false-positive rate is ~6×10⁻³
/// (P(48-bit random ≤ 14 of fixed)). At ~2400 sliding windows/sec
/// that's ~14 false syncs/sec. The downstream BCH(63,16,11) NID FEC
/// catches them (~10⁻⁴ pass-through → ~0.001 false TSDU events/sec).
/// Each false sync costs ~33 dibits of wasted NID read work; at
/// 14 false/sec that's ~10 % of the 4800 sym/s budget -- cheap on
/// Cortex-A9.
///
/// The `sync_distance_hist[25]` field buckets every observed sync
/// distance, exposed via `/api/control_lsm_dibit_dump`. If the
/// histogram shows a real sync cluster at 9-14, widening catches
/// them. If flat random, syncs aren't recoverable from the current
/// dibit stream and we need to either (a) fix the HDL DC blocker /
/// slicer or (b) wire the soft sync events into the TSBK pipeline.
///
/// 6 is a sane middle ground between "perfect-only" (4) and
/// "noise-flooded" (14), but the optimum shifts with PLL lock state,
/// so the right tool is `/api/sync_tune`.
pub const SYNC_THRESHOLD: u32 = 6;

/// Runtime-tunable sync threshold. Reads inside the dibit hot loop
/// go through this AtomicU32 (Relaxed ordering -- the value only
/// changes when an operator hits `/api/sync_tune`, a one-dibit lag
/// is fine). Initialised from `SYNC_THRESHOLD`.
pub static RUNTIME_SYNC_THRESHOLD: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(SYNC_THRESHOLD);

/// Logging threshold: any candidate with distance ≤ this is logged
/// as a "near miss" to give visibility into how close the bit stream
/// is to a real sync. Kept wider than SYNC_THRESHOLD so the near
/// counter shows the margin bucket (useful for deciding whether
/// widening further is worthwhile).
const SYNC_NEAR_LOG_THRESHOLD: u32 = 20;

/// The P25 NID payload is 64 bits = 32 content dibits, but the first P25
/// status dibit lands inside the NID window at on-air index 11 (counting
/// from 0 at the first dibit after the frame sync), so the NID spans 33
/// on-air dibits. Matches `lsm::sync::NID_TRANSMITTED_DIBITS` and the
/// HDL `LsmSyncNidExtract` which both read 33 dibits and skip index 11
/// before packing the remaining 32 into the 64-bit BCH codeword.
///
/// Historical note: when this constant was 32 and the decoder skipped
/// nothing, bits 41..40 of the NID codeword were silently corrupted by
/// the status dibit value and the remaining parity bits were shifted
/// out of position. The earlier C4FM decoder "worked" because its
/// `decode_nid` stub only extracted bits 63..48 (NAC+DUID) from the top
/// of the word -- those come from on-air dibits 0..7, all BEFORE the
/// status dibit at index 11, so the stub got the right NAC/raw_DUID
/// despite the corrupted parity region. Porting the validated
/// BCH(63,16,11) FEC exposed the bug: the LSM-side decoder
/// consistently miscorrected clean Clay County NIDs (NAC=0x8A1,
/// DUID=0x7) to a spurious fixed codeword (NAC=0xE28, DUID=0x5)
/// because the status-dibit corruption was deterministic. See
/// doc/changes/022 for the fix log.
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
            capture_ring_armed: false,
            capture_ring: std::collections::VecDeque::new(),
            capture_ring_limit: 0,
            bch_t_override: None,
            sync_threshold_override: None,
            capture_in_flight: None,
            system: SystemIdentity::default(),
            bands: HashMap::new(),
            grants: HashMap::new(),
            recent_messages: Vec::new(),
            // At steady-state ~14 messages/sec a 100-entry cap
            // saturated in 7 seconds and skewed `/api/recent_tsbks`.
            // 1000 holds ~70 seconds of activity.
            max_recent: 1000,
            aliases: HashMap::new(),
            event_tx: None,
            grant_event_tx: None,
            // Voice handler is opt-in. Control-channel decoders
            // leave it None; the traffic_lsm_decoder sets it to
            // forward IMBE frames downstream.
            voice_handler: None,
            event_log: None,
            chain_label: "control",
            ldu1_count: 0,
            ldu2_count: 0,
            hdu_count: 0,
            tdu_count: 0,
            tdu_lc_count: 0,
        }
    }

    /// Set the broadcast channel for WebSocket events
    pub fn set_event_tx(&mut self, tx: broadcast::Sender<String>) {
        self.event_tx = Some(tx);
    }

    /// Set the typed grant event channel.
    pub fn set_grant_event_tx(
        &mut self,
        tx: tokio::sync::mpsc::Sender<super::events::P25Event>,
    ) {
        self.grant_event_tx = Some(tx);
    }

    /// Push a grant event to the typed channel (non-blocking).
    fn emit_grant_event(&self, info: &GrantInfo) {
        if let Some(ref tx) = self.grant_event_tx {
            let _ = tx.try_send(super::events::P25Event::Grant(
                super::events::GrantEvent {
                    channel: info.channel,
                    talkgroup: info.talkgroup,
                    source: info.source,
                    frequency_hz: info.frequency_hz,
                    encrypted: info.encrypted,
                    emergency: info.emergency,
                },
            ));
        }
    }

    /// Install a voice frame handler. The decoder will dispatch
    /// HDU/LDU1/LDU2/TDU/TDU_LC events to the handler in addition to
    /// the normal TSDU dispatch. Set on the `traffic_lsm_decoder`
    /// instance in `main.rs`; left unset on control-channel decoders
    /// which never see voice frames.
    pub fn set_voice_handler(
        &mut self,
        handler: Arc<dyn VoiceHandler + Send + Sync>,
    ) {
        self.voice_handler = Some(handler);
    }

    /// Arm the NID batch capture ring. Clears any previous contents
    /// and enables capture up to `limit` entries (hard ceiling 1024).
    /// Called by `/api/nid_capture?arm=1&limit=N`.
    pub fn arm_capture_ring(&mut self, limit: usize) {
        self.capture_ring.clear();
        self.capture_ring_limit = limit.min(1024);
        self.capture_ring_armed = self.capture_ring_limit > 0;
    }

    /// Snapshot the current ring contents without draining. Caller
    /// typically clones the Vec and pushes it into JSON. Returns an
    /// empty Vec if no captures yet.
    pub fn snapshot_capture_ring(&self) -> Vec<AlignedCapture> {
        self.capture_ring.iter().cloned().collect()
    }

    /// Drain the ring and disarm. Called by
    /// `/api/nid_capture?clear=1`.
    pub fn drain_capture_ring(&mut self) -> Vec<AlignedCapture> {
        let out = self.capture_ring.drain(..).collect();
        self.capture_ring_armed = false;
        self.capture_ring_limit = 0;
        out
    }

    /// Runtime BCH-t tuner. `None` restores the default
    /// (T_MAX_ERRORS=11). `Some(n)` rejects any BCH decode whose
    /// `n_errors > n`, regardless of what the ML codebook search
    /// returned. Lets `/api/bch_t` sweep the rejection threshold
    /// live without reflashing.
    pub fn set_bch_t_override(&mut self, t: Option<u32>) {
        self.bch_t_override = t;
    }

    /// Per-decoder sync threshold setter. `None` falls back to
    /// `RUNTIME_SYNC_THRESHOLD` (the global). `Some(n)` forces this
    /// decoder to reject any sync hit with distance > n.
    pub fn set_sync_threshold_override(&mut self, t: Option<u32>) {
        self.sync_threshold_override = t;
    }

    /// Reset ONLY the framer state machine, preserving cumulative
    /// counters. Called by the traffic-channel grant follower on
    /// every retune so the decoder doesn't carry `ReadingNid` /
    /// `ReadingDataUnit` state across a frequency change (which
    /// produced misaligned frame fetches on the new channel).
    /// Distinct from `reset_diagnostics()` which zeroes counters
    /// without touching framer state.
    pub fn reset_framer_state(&mut self) {
        self.state = DecoderState::Hunting;
        self.sync_register = 0;
        self.du_buffer.clear();
        self.du_expected_len = 0;
        self.tsdu_blocks_decoded = 0;
    }

    /// Clear ALL diagnostic counters and histograms (the
    /// `/api/decoder_reset` backend). Preserves long-lived radio
    /// state (system identity, frequency band table, active grants,
    /// talkgroup aliases). `total_dibits` MUST be reset for the
    /// sweep tool's per-sec calculation to make sense in a
    /// post-reset measurement window.
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
        // Sync stats
        self.sync_hits = 0;
        self.sync_near_misses = 0;
        self.best_sync_distance = u32::MAX;
        self.sync_distance_hist = [0; 25];
        // Dibit stats
        self.total_dibits = 0;
        self.dibit_hist = [0; 4];
        self.last_log_dibits = 0;
        self.raw_duid_hist = [0; 16];
        self.recent_dibits.clear();
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

                // Bump the sync distance histogram on every dibit
                // shift once we have a full sync window. Most
                // informative diagnostic for "is the slicer garbage?"
                // -- real syncs at low distances = widen threshold;
                // smeared across 9-20 = slicer is corrupting half the
                // outer symbols.
                if self.dibit_count >= 24 {
                    let bucket = (distance as usize).min(24);
                    self.sync_distance_hist[bucket] += 1;
                }
                // Per-decoder override takes precedence over the
                // global runtime threshold. Lets us tighten the
                // traffic-side decoder (inter-LDU noise generates
                // false positives) while leaving control-side
                // permissive for marginal TSBK recovery.
                let runtime_threshold = self.sync_threshold_override
                    .unwrap_or_else(|| {
                        RUNTIME_SYNC_THRESHOLD
                            .load(std::sync::atomic::Ordering::Relaxed)
                    });
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

                    // If EITHER the one-shot aligned capture OR the
                    // batch ring is armed, start an in-flight capture
                    // so both paths populate from the same source.
                    if self.aligned_capture_armed || self.capture_ring_armed {
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
                // Append every NID-window dibit to the in-flight
                // capture (raw, including the status dibit).
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
                    // NID complete. Call the underlying ML decoder
                    // directly so we get `n_errors` back, then apply
                    // the runtime `bch_t_override` if set. The
                    // standard `GolayDecoder::decode_nid` wrapper
                    // discards `n_errors`, which we need for the
                    // tunable rejection threshold and per-entry
                    // ring-capture reporting.
                    let on_air_duid_raw =
                        ((new_bits >> 48) & 0xF) as u8;
                    let bch_result =
                        crate::lsm::nid_fec::decode_nid(new_bits);
                    // Apply runtime tolerance (None = default
                    // T_MAX_ERRORS=11). If n_errors exceeds, reject.
                    let bch_result = match bch_result {
                        Some(d) => {
                            let limit = self.bch_t_override
                                .unwrap_or(crate::lsm::nid_fec::T_MAX_ERRORS);
                            if d.n_errors as u32 > limit {
                                None
                            } else {
                                Some(d)
                            }
                        }
                        None => None,
                    };

                    let (nac_raw, duid_raw, on_air_duid, n_errors) =
                        match bch_result {
                            Some(d) => (d.nac, d.duid, on_air_duid_raw,
                                        d.n_errors),
                            None => {
                                self.nid_decode_failures += 1;
                                tracing::info!(
                                    target: "p25_decoder",
                                    "NID decode FAILED (raw=0x{:016X}) -> Hunting",
                                    new_bits,
                                );
                                // Push a ring entry for BCH rejects
                                // BEFORE consuming the in-flight
                                // capture for the one-shot path.
                                // Both paths can fire.
                                if self.capture_ring_armed {
                                    let entry = self.capture_in_flight
                                        .as_ref()
                                        .map(|cap| AlignedCapture {
                                            sync_dibits: cap.sync_dibits.clone(),
                                            sync_distance: cap.sync_distance,
                                            raw_nid_dibits: cap.raw_nid_dibits.clone(),
                                            nid_bits: new_bits,
                                            bch_nac: None,
                                            bch_duid: None,
                                            raw_duid: on_air_duid_raw,
                                            raw_body_dibits: Vec::new(),
                                            trellis_dibits: Vec::new(),
                                            tsbk_bytes: Vec::new(),
                                            crc_result: "nid_bch_reject".to_string(),
                                            total_dibits_at_capture: cap.total_dibits_at_capture,
                                        });
                                    if let Some(e) = entry {
                                        if self.capture_ring.len() < self.capture_ring_limit {
                                            self.capture_ring.push_back(e);
                                        }
                                        if self.capture_ring.len() >= self.capture_ring_limit {
                                            self.capture_ring_armed = false;
                                        }
                                    }
                                }
                                // Finalize the in-flight capture as
                                // a BCH-reject snapshot.
                                if let Some(cap) = self.capture_in_flight.take() {
                                    self.aligned_capture = Some(AlignedCapture {
                                        sync_dibits: cap.sync_dibits,
                                        sync_distance: cap.sync_distance,
                                        raw_nid_dibits: cap.raw_nid_dibits,
                                        nid_bits: new_bits,
                                        bch_nac: None,
                                        bch_duid: None,
                                        raw_duid: on_air_duid_raw,
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
                    // Track actual on-air DUID distribution. A working
                    // FEC lands bucket 7 at ~100%; a broken one scatters.
                    self.raw_duid_hist[(on_air_duid & 0x0F) as usize] += 1;

                    let nac = Nac::new(nac_raw);
                    if let Some(duid) = DataUnit::from_duid(duid_raw) {
                        // Update NAC if we see a valid one
                        self.system.nac = Some(nac);
                        self.nid_decoded_ok += 1;

                        // Structured DUID log. Fires for every
                        // successful NID decode on every chain
                        // (control / traffic / ps_c4fm) so
                        // `/api/log?category=duid` returns a
                        // timestamped trail independent of downstream
                        // dispatch. Logs chain + DUID + NAC + BCH
                        // errors + raw-vs-corrected DUID for spotting
                        // FEC corrections.
                        if let Some(ref log) = self.event_log {
                            let duid_name: &'static str = match duid {
                                DataUnit::Hdu => "HDU",
                                DataUnit::Tdu => "TDU",
                                DataUnit::Ldu1 => "LDU1",
                                DataUnit::Tsdu => "TSDU",
                                DataUnit::Ldu2 => "LDU2",
                                DataUnit::Pdu => "PDU",
                                DataUnit::TduLc => "TDU_LC",
                            };
                            log.push(
                                crate::services::event_log::LogCategory::Duid,
                                format!(
                                    "{} {} NAC=0x{:03X} bch_err={}",
                                    self.chain_label,
                                    duid_name,
                                    nac_raw,
                                    n_errors,
                                ),
                                serde_json::json!({
                                    "chain":      self.chain_label,
                                    "duid":       duid_name,
                                    "nac":        format!("0x{:03X}", nac_raw),
                                    "bch_errors": n_errors,
                                    "raw_duid":   format!("0x{:X}", on_air_duid & 0xF),
                                    "bch_duid":   format!("0x{:X}", duid_raw & 0xF),
                                }),
                            );
                        }
                        if matches!(duid, DataUnit::Tsdu) {
                            self.nid_decoded_tsdu += 1;
                        }

                        // Stash BCH-success fields into the in-flight
                        // capture so process_tsdu can finalize.
                        if let Some(cap) = self.capture_in_flight.as_mut() {
                            cap.nid_bits = new_bits;
                            cap.bch_nac = Some(nac_raw);
                            cap.bch_duid = Some(duid_raw);
                            cap.raw_duid = on_air_duid;
                        }

                        // Push a ring entry with BCH-success fields.
                        // Trellis/TSBK fields stay empty -- the ring
                        // is NID-only for DUID sweep analysis.
                        if self.capture_ring_armed {
                            let entry = self.capture_in_flight
                                .as_ref()
                                .map(|cap| AlignedCapture {
                                    sync_dibits: cap.sync_dibits.clone(),
                                    sync_distance: cap.sync_distance,
                                    raw_nid_dibits: cap.raw_nid_dibits.clone(),
                                    nid_bits: new_bits,
                                    bch_nac: Some(nac_raw),
                                    bch_duid: Some(duid_raw),
                                    raw_duid: on_air_duid,
                                    raw_body_dibits: Vec::new(),
                                    trellis_dibits: Vec::new(),
                                    tsbk_bytes: Vec::new(),
                                    crc_result: "nid_ok".to_string(),
                                    total_dibits_at_capture: cap.total_dibits_at_capture,
                                });
                            if let Some(e) = entry {
                                if self.capture_ring.len() < self.capture_ring_limit {
                                    self.capture_ring.push_back(e);
                                }
                                if self.capture_ring.len() >= self.capture_ring_limit {
                                    self.capture_ring_armed = false;
                                }
                            }
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

                // Append every body dibit to the in-flight capture
                // so process_tsdu has the raw input to dump.
                if let Some(cap) = self.capture_in_flight.as_mut() {
                    cap.raw_body_dibits.push(dibit & 0x03);
                }

                if self.du_buffer.len() >= self.du_expected_len {
                    // Data unit complete (or one TSBK block complete in
                    // the multi-block case). Process and find out
                    // whether more dibits are needed.
                    let done = match duid {
                        DataUnit::Tsdu => self.process_tsdu_block(),
                        // Voice channel data unit dispatch. The framer
                        // has already read `length_dibits()` raw dibits
                        // (status dibits in place); the payload
                        // extractors below strip status dibits and apply
                        // the SDRTrunk-documented bit positions. The
                        // voice handler is opt-in -- on control-channel
                        // decoders it's None and these arms reduce to
                        // "count + return true".
                        DataUnit::Ldu1 => {
                            self.ldu1_count += 1;
                            if let Some(handler) = self.voice_handler.clone() {
                                if let Some(frames) = crate::protocol::p25::voice_frame::extract_imbe_frames(&self.du_buffer) {
                                    handler.on_ldu1(&frames, &self.du_buffer);
                                }
                            }
                            true
                        }
                        DataUnit::Ldu2 => {
                            self.ldu2_count += 1;
                            if let Some(handler) = self.voice_handler.clone() {
                                if let Some(frames) = crate::protocol::p25::voice_frame::extract_imbe_frames(&self.du_buffer) {
                                    handler.on_ldu2(&frames, &self.du_buffer);
                                }
                            }
                            true
                        }
                        DataUnit::Hdu => {
                            self.hdu_count += 1;
                            if let Some(handler) = self.voice_handler.clone() {
                                handler.on_hdu(&self.du_buffer);
                            }
                            true
                        }
                        DataUnit::Tdu => {
                            // length_dibits() is 15 so the framer DOES
                            // read the trailing 15 raw dibits before
                            // we get here -- just dispatch and return.
                            self.tdu_count += 1;
                            if let Some(handler) = self.voice_handler.clone() {
                                handler.on_tdu();
                            }
                            true
                        }
                        DataUnit::TduLc => {
                            self.tdu_lc_count += 1;
                            if let Some(handler) = self.voice_handler.clone() {
                                handler.on_tdu_lc(&self.du_buffer);
                            }
                            true
                        }
                        // PDU is rare on voice channels; consume + return.
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
    /// A single TSDU can carry one, two, or three TSBK blocks per
    /// SDRTrunk's `P25P1DataUnitID.TRUNKING_SIGNALING_BLOCK_{1,2,3}`
    /// table; the block-1 header bit `LB` (last block) tells the
    /// receiver whether more blocks follow.
    ///
    /// When the current block fails trellis or CRC we do NOT abort
    /// the multi-block read -- we treat it as `last_block=0` and
    /// continue to the next block boundary, mirroring SDRTrunk's
    /// `P25P1MessageFramer.dispatchTSBK()`:
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
    /// frame; aborting on TSBK1 CRC fail dropped TSBK2/TSBK3 ~55% of
    /// the time (capped `tsbk_block_attempts/tsdu_attempts` at ~1.6
    /// instead of the theoretical 3.0).
    ///
    /// Called once per `du_expected_len` boundary:
    ///
    /// 1. Re-runs `TsduDeinterleaver::deinterleave_multi` over the
    ///    entire buffered body for the current block count.
    /// 2. Slices out trellis dibits for THIS block (positions
    ///    `[block_idx*98 .. (block_idx+1)*98]`) and runs Viterbi.
    /// 3. Validates CRC, populates diagnostic histograms,
    ///    dispatches via `handle_tsbk`.
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
        // but still continue to the next block boundary
        // (continue-past-failure model).
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
                        Some(crate::protocol::p25::tsbk::CrcConvention::Plain) => {
                            self.tsbk_crc_ok += 1;
                            self.tsbk_crc_ok_plain += 1;
                            self.tsbk_crc_ok_by_pos[block_idx] += 1;
                            self.tsbk_opcode_hist_ok[opcode_byte] += 1;
                            self.bump_mfid(block.manufacturer);
                            if block_idx == 0 {
                                capture_crc_result = "plain".to_string();
                            }
                        }
                        Some(crate::protocol::p25::tsbk::CrcConvention::Xored) => {
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
                            // Mirror of the handle_tsbk call site
                            // above: 0x33 = IDEN_UPDATE_TDMA flags a
                            // Phase-2-capable site.
                            if opcode_byte == 0x33 {
                                self.system.has_tdma_band = true;
                            }
                            self.handle_tsbk(block_idx as u8, msg);
                        } else {
                            self.tsbk_unknown_opcode += 1;
                        }
                    }
                }
            }
        }

        self.tsdu_blocks_decoded += 1;

        // Continue-past-failure: stop only if we got LB=1 from a
        // CLEAN (CRC-OK) block, OR we just finished block 3.
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

    /// MFID bucketing helper. Tracks three named vendors (standard
    /// 0x00, Motorola 0x90, Harris/Tait 0xA4) plus "other" so the
    /// dashboard gets the vendor mix without a 256-entry histogram.
    fn bump_mfid(&mut self, mfid: u8) {
        match mfid {
            0x00 => self.tsbk_mfid_hist_ok[0] += 1,
            0x90 => self.tsbk_mfid_hist_ok[1] += 1,
            0xA4 => self.tsbk_mfid_hist_ok[2] += 1,
            _ => self.tsbk_mfid_hist_ok[3] += 1,
        }
    }

    /// Aligned-capture finaliser. Single drain point for the in-flight
    /// capture from the multi-block `process_tsdu_block`.
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
    /// preserved fields (source RadioId, encryption flag, emergency
    /// flag) from the first matching entry, so the caller can keep
    /// state across a refresh.
    ///
    /// In real trunking, a single talkgroup is on one voice channel
    /// at a time -- when the system grants TG `T` to a new channel,
    /// any prior `T` grant on a different channel is no longer active.
    /// The `grants` map is keyed by channel, so without this dedup
    /// the old entry sits around until `expire_grants` reaps it.
    ///
    /// `GroupVoiceChannelGrantUpdate` does NOT carry a source RadioId,
    /// so without preservation the source would get wiped to `None` on
    /// the first update after an initial grant. Encryption + emergency
    /// flags are similarly absent from update TSBKs. The "any prior
    /// grant said true" rule for flags (boolean OR) is intentional: a
    /// TG that was once marked encrypted/emergency stays so for the
    /// call's duration -- matches SDRTrunk's call-session semantics.
    ///
    /// Wildcard TG 0 is excluded because the grant-update path already
    /// filters it as a sentinel.
    fn take_other_grants_for_talkgroup(
        &mut self,
        talkgroup: Talkgroup,
    ) -> PreservedGrantFields {
        if talkgroup.0 == 0 {
            return PreservedGrantFields::default();
        }
        let mut preserved = PreservedGrantFields::default();
        self.grants.retain(|_, g| {
            if g.talkgroup == talkgroup {
                // Capture the source from the first match.
                if preserved.source.is_none() && g.source.is_some() {
                    preserved.source = g.source;
                }
                // Preserve encryption + emergency flags (absent from
                // GVCG_UPDATE). See function-level docstring for the
                // boolean-OR rationale.
                if g.encrypted {
                    preserved.encrypted = true;
                }
                if g.emergency {
                    preserved.emergency = true;
                }
                false
            } else {
                true
            }
        });
        preserved
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
            service_options: 0, // clear voice, no emergency
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
            service_options: 0,
        });
        // Independent TG on a third channel -- must NOT be cleared.
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x0500),
            talkgroup: Talkgroup(300),
            source: RadioId(2022),
            service_options: 0,
        });
        assert_eq!(decoder.grants.len(), 2);

        // Second grant: same TG 202 on a different channel. The
        // prior 0x0345 entry should be dropped, leaving exactly two
        // grants total (the new TG 202 + the unrelated TG 300).
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: Channel(0x045D),
            talkgroup: Talkgroup(202),
            source: RadioId(1011),
            service_options: 0,
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
            service_options: 0,
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
            service_options: 0,
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
    /// Regression guard for doc/changes/022 (see NID_TRANSMITTED_DIBITS
    /// const docstring for the full incident).
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

    /// Multi-block TSBK end-to-end regression guard. Builds a real
    /// 2-block TSDU body (TSBK1 last_block=0, TSBK2 last_block=1)
    /// with valid CCITT_80 CRCs, trellis-encodes each 12-byte block,
    /// and splices the 7 status dibits at body positions
    /// {13,49,85,121,157,193,229} plus 28 trailing null padding
    /// dibits. Verifies:
    ///
    /// 1. `tsdu_attempts` == 1
    /// 2. `tsbk_block_attempts` == 2
    /// 3. `tsbk_crc_ok` == 2
    /// 4. Both messages dispatched:
    ///    - Block 1: NetworkStatusBroadcast (0x3B) → `system.wacn`
    ///    - Block 2: RfssStatusBroadcast (0x3A) → `system.rfss_id`
    /// 5. Decoder returns to Hunting after the second block.
    #[test]
    fn test_multi_block_tsbk_e2e() {
        use crate::lsm::nid_fec;
        use crate::protocol::p25::fec::trellis_encode_bytes;
        use crate::protocol::p25::tsbk::ccitt80_crc;

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

        // ── TSBK1: NET_STS_BCST (opcode 0x3B), LB=0 ──
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

        // ── TSBK2: RFSS_STS_BCST (opcode 0x3A), LB=1 ──
        // Matches SDRTrunk RFSSStatusBroadcast.java:
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
        use crate::protocol::p25::fec::trellis_encode_bytes;
        use crate::protocol::p25::tsbk::ccitt80_crc;

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

//! ImbeForwarder — traffic-chain voice handler.
//!
//! Implements crate::protocol::p25::control_channel::VoiceHandler over the
//! raw LDU/TDU callbacks from the traffic LSM decoder. Owns the IMBE-batch
//! mpsc sender to the vocoder task and the call-boundary broadcast tx that
//! feeds the recorder.

use crate::app::dibit_airtime::{DibitRingShared, EpochKind, SegmentContext};
use crate::audio;
use crate::protocol::p25;

/// One LDU's worth of IMBE frames on its way to the vocoder, labelled
/// with the call context it was decoded under. Change 054: `encrypted`
/// travels with the batch (the vocoder used to read the live flag at
/// vocode time, which a retune could flip under in-flight frames), and
/// in airtime mode all labels come from the dibits' air-time epoch.
#[derive(Debug, Clone)]
pub struct ImbeBatch {
    pub talkgroup: u16,
    pub source: u32,
    pub call_id: u64,
    /// Unix ms. Airtime mode: estimated production (air) time of the
    /// dibit that completed the LDU. Other modes: dispatch wall time.
    pub captured_at_ms: u64,
    pub encrypted: bool,
    /// True when the labels are air-time epoch attributed (recorder
    /// routes by `call_id` instead of the capture-time window).
    pub airtime: bool,
    pub frames: [p25::voice_frame::ImbeFrameRaw; 9],
}

pub type ImbeBatchTx = tokio::sync::mpsc::Sender<ImbeBatch>;
pub type ImbeBatchRx = tokio::sync::mpsc::Receiver<ImbeBatch>;

/// Voice frame handler that counts IMBE events and forwards raw frames
/// to the vocoder task via an mpsc channel.
///
/// Implements `p25::control_channel::VoiceHandler`. Installed on the
/// `traffic_lsm_decoder` via `set_voice_handler`. Held as
/// `Arc<dyn VoiceHandler + Send + Sync>`.
///
/// Uses `try_send` (non-async) because `VoiceHandler` methods take
/// `&self` and are called from synchronous `process_dibit` code. If the
/// channel is full the batch is dropped and `imbe_frames_dropped` is
/// incremented — the vocoder task is expected to keep up at ~50 frames/sec
/// (one LDU every ~180 ms).
pub struct ImbeForwarder {
    pub hdu_count: std::sync::atomic::AtomicU64,
    pub ldu1_count: std::sync::atomic::AtomicU64,
    pub ldu2_count: std::sync::atomic::AtomicU64,
    pub tdu_count: std::sync::atomic::AtomicU64,
    pub tdu_lc_count: std::sync::atomic::AtomicU64,
    // 2026-04-30 framer-divergence diagnostic. Incremented by
    // VoiceHandler::on_dispatch_arm_<duid> hooks, fired by the
    // decoder AFTER reaching the per-DUID arm but BEFORE body
    // extraction. Counterpart to {hdu,ldu1,ldu2,tdu,tdu_lc}_count
    // which fire AFTER body extraction. Per-call delta of the diff
    // = body-extraction failure count for that DUID.
    pub framer_arm_hdu:    std::sync::atomic::AtomicU64,
    pub framer_arm_ldu1:   std::sync::atomic::AtomicU64,
    pub framer_arm_ldu2:   std::sync::atomic::AtomicU64,
    pub framer_arm_tdu:    std::sync::atomic::AtomicU64,
    pub framer_arm_tdu_lc: std::sync::atomic::AtomicU64,
    pub imbe_frames_extracted: std::sync::atomic::AtomicU64,
    /// Incremented in `forward_frames` when the vocoder input queue is
    /// full. Arc-wrapped so the recorder task can hold a cloneable
    /// handle and log per-call drop-delta into each finalise event.
    /// Surfaced via `/api/traffic`.
    pub imbe_frames_dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Legacy counter for LDU batches dropped because follower was Idle.
    /// No longer incremented (TG-0 drop gate removed, see `forward_frames`),
    /// retained so existing dashboard fields don't break.
    pub imbe_frames_dropped_idle: std::sync::atomic::AtomicU64,
    pub last_imbe_at_millis: std::sync::atomic::AtomicU64,
    /// Vocoder stats -- updated by the vocoder task, read by /api/traffic.
    pub vocoder_pcm_produced: std::sync::atomic::AtomicU64,
    pub vocoder_errors: std::sync::atomic::AtomicU64,
    pub vocoder_frames_encrypted: std::sync::atomic::AtomicU64,
    /// Observability-only count of silent frames (peak below
    /// `SILENT_PEAK`) emitted by JMBE. Silent frames pass through to
    /// both recorder and audio broadcast — an earlier drop-gate was
    /// reverted because bursts of silent frames within one speaker's
    /// turn were exceeding the recorder grace window and splitting
    /// calls into multiple files.
    pub vocoder_frames_silent_observed: std::sync::atomic::AtomicU64,
    /// Set by the grant follower task when it locks onto a TG. The
    /// vocoder task reads this to skip encrypted calls.
    pub call_encrypted: std::sync::atomic::AtomicBool,
    /// Current talkgroup (set by grant follower, read by vocoder
    /// to tag AudioChunks). 0 = idle / unknown.
    pub current_talkgroup: std::sync::atomic::AtomicU16,
    /// Current source radio ID, set by the grant follower from
    /// `GRP_VCH_GRANT.FM`. 0 = unknown (grant carried no source, or
    /// only a `GRP_VCH_GRNT_UPD` which doesn't carry source). Read by
    /// the recorder to stamp the filename as soon as audio starts
    /// flowing — no need to wait for LDU1 LC / TDULC Motorola end-code.
    /// Mid-call source switches on a rebroadcast grant refresh here so
    /// most-recent wins.
    pub current_source: std::sync::atomic::AtomicU32,
    /// 2026-04-24: traffic-channel frequency (Hz) for the active
    /// call, set by the grant follower from `GRP_VCH_GRANT.frequency_hz`.
    /// Allows `RecordingEntry` and `GrantDecodeSummary` to be tagged
    /// with which physical channel a call landed on, so per-channel
    /// quality (drop rate, silent rate, first_imbe_ms) can be diffed
    /// across the site's traffic frequencies. 0 = unknown.
    pub current_frequency_hz: std::sync::atomic::AtomicU64,
    /// 2026-04-24: channel string from the grant, e.g. "0-1117".
    /// Mutex<String> rather than an atomic — short, only written on
    /// retune. Empty = unknown.
    pub current_channel: std::sync::Mutex<String>,
    /// Phase 2h (2026-04-25): GrantFollower call_id stamped onto
    /// every IMBE batch in `forward_frames` so the vocoder can
    /// attach it to emitted AudioChunks. Recorder routes by
    /// chunk.call_id directly, eliminating the prior tg+source
    /// heuristic. Set on `CallTrackerEvent::CallOpen` from
    /// `spawn_call_lifecycle`; reset to 0 on `CallClose`.
    pub current_call_id: std::sync::atomic::AtomicU64,
    /// TGs that have ever been observed encrypted. Once a TG is in
    /// this set, the follower defaults to encrypted even if the
    /// current grant doesn't carry service options.
    pub encrypted_tg_history: std::sync::Mutex<std::collections::HashSet<u16>>,
    /// Set by the grant follower on call boundary (new TG lock or
    /// Idle→Active). The vocoder task checks this and resets mbelib
    /// state to avoid cross-call artifacts.
    pub vocoder_reset_pending: std::sync::atomic::AtomicBool,
    /// Ring buffer of the last N raw IMBE frames for diagnostic capture
    /// via `/api/imbe_dump`. Stores (talkgroup, encrypted, frame_bytes).
    /// `VecDeque` so the 128-cap eviction on `push_back` is `O(1)` rather
    /// than `O(n)` — `/api/imbe_dump` is a hot path when 10 traffic chains
    /// are active.
    pub imbe_ring: std::sync::Mutex<std::collections::VecDeque<(u16, bool, [u8; 18])>>,
    /// Channel to the vocoder task. Each send is `(talkgroup_at_send_time,
    /// batch_of_9_frames)`. TG is captured at send time so tail frames
    /// of call N keep OLD TG even after follower retunes to call N+1 —
    /// prevents OLD tail being decoded with NEW JMBE state and routed
    /// into NEW call's recording. 2026-04-24 field-observed bug.
    imbe_tx: ImbeBatchTx,
    /// Change 054: traffic dibit ring shared state (epoch recorder).
    /// Unset on host builds / before `set_airtime`.
    airtime: std::sync::OnceLock<std::sync::Arc<DibitRingShared>>,
    /// Change 054: decode-side context of the traffic segment currently
    /// being fed by the airtime reader. While `seg_active` is true the
    /// voice handlers read TG / source / call_id / encrypted from here
    /// instead of the live follower atomics, so every frame carries the
    /// context that was in effect when its dibits were on the air.
    /// Only toggled while the reader holds the traffic decoder's write
    /// lock, so other feeders (sw_demod) always see `false`.
    seg_active: std::sync::atomic::AtomicBool,
    seg_tg: std::sync::atomic::AtomicU16,
    seg_source: std::sync::atomic::AtomicU32,
    seg_call_id: std::sync::atomic::AtomicU64,
    seg_encrypted: std::sync::atomic::AtomicBool,
    /// Air time (unix ms) of the dibit word being fed.
    seg_air_ms: std::sync::atomic::AtomicU64,
    /// Optional broadcast channel for call-boundary events emitted
    /// from the voice handler. `None` on decoder instances that don't
    /// split calls (e.g. control-channel decoders).
    call_boundary_tx:
        std::sync::OnceLock<audio::CallBoundaryTx>,
    /// Last NAC observed by the software framer, used to tag
    /// `CallBoundary` events emitted from `on_tdu_lc`. Set by the
    /// traffic-LSM heartbeat whenever it forwards a NID event.
    pub last_observed_nac: std::sync::atomic::AtomicU16,
    /// Event-log ring reference so TDULC LCW parses emit into the
    /// dashboard activity feed (matching SDRTrunk's `decoded_messages.log`
    /// style). `None` until `set_event_log` is called post-construction.
    pub event_log:
        std::sync::OnceLock<std::sync::Arc<crate::services::event_log::EventLog>>,
    /// WebSocket event tx for the live activity feed. Same channel the
    /// control-channel decoder uses so TDULC LCW events appear inline
    /// with TSBK events.
    pub ws_event_tx: std::sync::OnceLock<
        tokio::sync::broadcast::Sender<String>,
    >,

    // TDULC LCW parse diagnostics. All fire from `on_tdu_lc`; sum is
    // the number of TDULC bodies the parser actually ran against
    // (tg != 0 at dispatch). Surfaces via /api/traffic to see whether:
    //   - parser is reaching every TDULC (attempts track tdu_lc_count
    //     once follower is active), and
    //   - site emits Motorola `TALK_COMPLETE` (motorola vs gvcu vs
    //     other distribution).
    pub tdulc_parse_attempts: std::sync::atomic::AtomicU64,
    pub tdulc_parse_motorola: std::sync::atomic::AtomicU64,
    pub tdulc_parse_gvcu: std::sync::atomic::AtomicU64,
    /// GroupVoiceChannelUpdate LCW (opcode 0x02) — mid-call channel-
    /// reuse beacon, does NOT close. Separated from
    /// `tdulc_parse_other` 2026-04-24 so "how many GVU vs CallTerm"
    /// is directly readable from /api/traffic instead of needing to
    /// subtract counters manually.
    pub tdulc_parse_gvu: std::sync::atomic::AtomicU64,
    /// CallTermination LCW (opcode 0x0F MFID 0x00) — fires SpeakerEnd.
    /// Split out from `other` so we can measure the ratio against
    /// HDU count (should be ~1 per call).
    pub tdulc_parse_callterm: std::sync::atomic::AtomicU64,
    pub tdulc_parse_other: std::sync::atomic::AtomicU64,
    pub tdulc_parse_none: std::sync::atomic::AtomicU64,
    /// First 9 bytes (72 bits) of the most recent post-dibits LC
    /// payload, for offline inspection when the Motorola counter is
    /// stuck at zero. Mutex-behind because it's a single sample, not
    /// a hot counter.
    pub tdulc_last_lc_bytes: std::sync::Mutex<[u8; 9]>,
    /// IMBE frames pushed to `imbe_tx`. Snapshotted into
    /// `CallBoundary::expected_submit_count` on dispatch to drive
    /// recorder close.
    pub frames_submitted: std::sync::atomic::AtomicU64,
    /// IMBE frames popped by the vocoder task (synthesised or skipped).
    /// Recorder waits for this to reach `expected_submit_count` before
    /// finalising so trailing PCM appends to the closing recording.
    /// `Arc` so the recorder task holds an independent handle.
    pub frames_consumed: std::sync::Arc<std::sync::atomic::AtomicU64>,

    /// 2026-04-24: TG of the most recent IMBE batch the vocoder actually
    /// consumed. Distinct from `current_talkgroup` (which is the
    /// follower's intent) — this is what the AUDIO PATH is decoding
    /// right now. Disagreement between the two means tail frames are
    /// in flight.
    pub last_batch_tg: std::sync::atomic::AtomicU16,

    /// 2026-04-24: baseline counters snapshotted on each `on_hdu`.
    /// /api/traffic exposes `current_call_* = <global> - <baseline>`
    /// so the dashboard can show per-CURRENT-call metrics separate
    /// from cumulative session totals.
    pub call_baseline_hdu:            std::sync::atomic::AtomicU64,
    pub call_baseline_ldu1:           std::sync::atomic::AtomicU64,
    pub call_baseline_ldu2:           std::sync::atomic::AtomicU64,
    pub call_baseline_tdu:            std::sync::atomic::AtomicU64,
    pub call_baseline_tdu_lc:         std::sync::atomic::AtomicU64,
    pub call_baseline_imbe_extracted: std::sync::atomic::AtomicU64,
    pub call_baseline_imbe_dropped:   std::sync::atomic::AtomicU64,
    pub call_baseline_pcm:            std::sync::atomic::AtomicU64,
    pub call_baseline_errors:         std::sync::atomic::AtomicU64,
    pub call_baseline_silent:         std::sync::atomic::AtomicU64,
    pub call_baseline_unix_ms:        std::sync::atomic::AtomicU64,

    /// 2026-04-24: high-water mark on the imbe_tx queue depth. Updated
    /// on every successful `try_send` in `forward_frames`. Live depth
    /// is derived from `imbe_tx.max_capacity() - imbe_tx.capacity()`
    /// via the public methods below — no atomic needed for the gauge
    /// itself, only the running max.
    pub queue_high_water: std::sync::atomic::AtomicU64,

    /// 2026-04-24 SpeakerEnd cooldown: timestamp (unix ms) of the
    /// most recent SpeakerEnd we emitted. Subsequent SpeakerEnds
    /// within `SPEAKER_END_COOLDOWN_MS` are dropped silently —
    /// they're almost always false-positive LCW decodes
    /// (Motorola TALK_COMPLETE / standard CallTermination /
    /// phantom-TDU) on degenerate dibit streams that happen to
    /// pass BCH by chance. Pre-cooldown we saw ~7 closes per
    /// real call; first-one-wins gets it to ~1/call.
    pub last_speaker_end_ms: std::sync::atomic::AtomicU64,
    /// Count of SpeakerEnds rejected by the cooldown gate. Watch
    /// this vs real-call count — if it's near 6× HDU count the
    /// cooldown is doing its job.
    pub speaker_end_deduplicated: std::sync::atomic::AtomicU64,
    /// 2026-04-24 validity rejects: SpeakerEnd candidates dropped
    /// because the follower wasn't on a real call (current_talkgroup
    /// == 0) or the LCW source field was nonsensical (0 or a
    /// well-known system-controller address for a path that's
    /// supposed to carry a real speaker RID). Separate counter from
    /// dedup so we can see which filter is doing the work.
    pub speaker_end_invalid: std::sync::atomic::AtomicU64,

    /// 2026-04-26 per-call AGC tracking: the most recent
    /// `traffic_lsm_agc_debug.agc_gain_dbg` (Q9.7 raw u16) sampled
    /// by the periodic AGC poller spawned in `main.rs::cfg(linux)`.
    /// `grant_stats` reads this at CallClose to record the
    /// converged AGC gain per call. Useful for: (a) per-frequency
    /// AGC seed cache (next step), (b) sanity-checking which calls
    /// settled vs which never reached steady state.
    pub last_traffic_agc_gain_q97: std::sync::atomic::AtomicU16,

    /// 2026-04-26 per-freq AGC seed cache. Key = freq_hz of the
    /// call. Value = exponential-moving-average of converged AGC
    /// gain (Q9.7 raw u16) observed at CallClose for clear calls
    /// on that freq. EMA smooths per-call variance (different
    /// speakers, different signal levels) so the seed lands in
    /// the ballpark instead of chasing one-shot values.
    /// `update_agc_cache` adds samples; `agc_seed_for_freq` reads
    /// for retune. In-memory only — first call after boot on a
    /// new freq still cold-starts.
    pub traffic_agc_freq_cache:
        std::sync::Mutex<std::collections::HashMap<u64, u16>>,

    /// 2026-04-24 CC-centric refactor: rolling window of recent
    /// LDU1 LC FM: decodes used to gate the emission of
    /// `TdulcComplete { source }` boundary events.
    ///
    /// LDU1 LC FEC is Hamming(10,6,3) + RS(24,12,13) — the weakest
    /// FEC in the P25 stack. A single LDU1 LC decode can produce a
    /// plausible-looking but wrong RID (2026-04-24 log audit found
    /// two 8-digit RIDs that never appear as any CC grant SRC).
    /// Requiring N-of-M agreement across consecutive LDU1 packets
    /// before emitting filters isolated flips — the protocol hands us
    /// ~326 ms between LDU1s and real speakers hold PTT for seconds,
    /// so a legitimate FM: will repeat ≥ 3 times within the ring
    /// before we emit.
    ///
    /// Resets when `current_talkgroup` changes (new call). Emission
    /// de-duplicates: only fires when the voted-consensus FM:
    /// CHANGES from the last emitted value (avoids flooding grant_
    /// stats with the same stamp every 326 ms mid-call).
    pub ldu1_fm_history: std::sync::Mutex<Ldu1FmHistory>,

    /// Count of LDU1-LC-source-derived `TdulcComplete` boundary
    /// events emitted (passed plausibility + N-of-M voting). Watch
    /// vs `ldu1_count` — should be roughly one per distinct speaker
    /// per call.
    pub ldu1_lc_source_emitted: std::sync::atomic::AtomicU64,

    /// Count of LDU1 LC FM: values rejected by the plausibility
    /// gate (source == 0, system-controller address, or not in
    /// 24-bit range). Primarily for diagnostics — these would be
    /// FEC corrupted in almost every case.
    pub ldu1_lc_source_rejected_implausible:
        std::sync::atomic::AtomicU64,

    /// 2026-04-25: count of LDU1 LC FM: values that disagreed with
    /// the CC `current_source` (Trellis+CRC, authoritative) when
    /// both were known. Operator philosophy: log every disagreement,
    /// don't act on any of them. CC stays authoritative; LDU1 LC
    /// provides forensic data on FEC corruption / signal quality.
    /// Watching this counter alongside per-call IMBE counts tells
    /// us whether LC FEC is failing in lockstep with whole-frame
    /// quality issues.
    pub ldu1_lc_cc_mismatch_count:
        std::sync::atomic::AtomicU64,

    /// 2026-04-25: most recent LDU1 LC ↔ CC SRC disagreement, with
    /// raw body bytes for forensic inspection. Lets us pull the
    /// failing LC body off the live board and replay through an
    /// independent decoder (or compare against SDRTrunk) without
    /// having to retrigger the failure. Single sample; overwritten
    /// on each new mismatch.
    pub ldu1_last_mismatch:
        std::sync::Mutex<Option<Ldu1LcMismatch>>,

    /// Change 057: decode counters per call_id (`app::call_counters`),
    /// attributed where each frame is decoded (voice handlers here, the
    /// vocoder thread for PCM / silent / error / encrypted). The global
    /// counters above are unchanged.
    pub call_counts: crate::app::call_counters::CallCounterBook,

    /// Change 057: the traffic LSM was paused (`traffic_lsm_enable = 0`)
    /// by an encrypted teardown (follower or `/api/encrypted_tgs`). A
    /// cross-frequency retune re-enables the chain, but the same-freq
    /// resume path writes no register, so before 057 a same-channel
    /// grant after such a teardown left the chain dead until the next
    /// cross-frequency retune. The resume path now re-enables it.
    pub traffic_paused_by_teardown: std::sync::atomic::AtomicBool,
    /// Traffic PLL watchdog resets (`app::traffic_pll_watchdog`): at a
    /// signal's return after a gap, and with the PLL pinned at the clamp.
    pub pll_wd_resets_onset: std::sync::atomic::AtomicU64,
    pub pll_wd_resets_pinned: std::sync::atomic::AtomicU64,
    /// The watchdog runs (gateware without the LSM signal hold, < 0.2.0).
    pub pll_wd_enabled: std::sync::atomic::AtomicBool,
    /// Change 059: the active call's pending end-of-transmission marker
    /// (`grant_follower::pack_end_marker`, 0 = none), written by the
    /// call lifecycle, read by the follower's sticky gate.
    pub active_end_marker: std::sync::atomic::AtomicU64,
}

/// Forensic snapshot of a single LDU1-LC-vs-CC-SRC disagreement.
#[derive(Debug, Clone)]
pub struct Ldu1LcMismatch {
    /// Wall-clock unix_ms at the moment of disagreement.
    pub timestamp_ms: u64,
    /// Active TG when the LDU1 LC was decoded.
    pub tg: u16,
    /// What the CC's `GRP_VCH_GRANT.SRC` had stamped (authoritative).
    pub cc_source: u32,
    /// What our LDU1 LC parser produced from the body bytes.
    pub ldu1_lc_source: u32,
    /// LDU1 body raw bytes (first 24 — covers the LC field across
    /// all 5 segments after status-dibit removal). Replay through
    /// `voice_frame::parse_ldu1_lcw` to verify the parser is reading
    /// the correct offset.
    pub raw_body_first_24: [u8; 24],
}

/// N-of-M voting ring for LDU1 LC FM: source stabilisation.
/// M = ring capacity; N = minimum agreement count to emit.
#[derive(Debug)]
pub struct Ldu1FmHistory {
    /// Talkgroup the ring applies to. Ring clears on TG change
    /// (new call = fresh voting).
    pub tg: u16,
    /// Recent plausibility-passed FM: values. Newest on the right.
    pub recent: std::collections::VecDeque<u32>,
    /// Most recently emitted consensus FM: value. `0` = never
    /// emitted within this TG's ring. Emission suppressed until
    /// consensus differs from this.
    pub last_emitted: u32,
}

impl Ldu1FmHistory {
    pub fn new() -> Self {
        Self {
            tg: 0,
            recent: std::collections::VecDeque::with_capacity(
                LDU1_FM_VOTE_M,
            ),
            last_emitted: 0,
        }
    }
}

/// Size of the LDU1 LC FM: voting ring. Four packets span ~1.3 s
/// of voice (LDU1 cadence 326 ms). Short enough to catch a real
/// speaker change within ~1 s; long enough that isolated FEC flips
/// can't reach the agreement threshold.
pub const LDU1_FM_VOTE_M: usize = 4;

/// Minimum agreement count within the ring to emit consensus.
/// 3-of-4 means one FEC flip is harmless; two consecutive flips
/// with the SAME corrupted value would be needed to fool the vote,
/// which is astronomically unlikely for random Hamming10 errors.
pub const LDU1_FM_VOTE_N: usize = 3;

/// Cooldown window for SpeakerEnd emission. 1500 ms is longer than
/// any real TDU/TDU_LC cluster inside a single call (typical spacing
/// ~180 ms) and shorter than a realistic new-call gap between two
/// speakers (seconds at a minimum with retune + HDU).
#[cfg(target_os = "linux")]
const SPEAKER_END_COOLDOWN_MS: u64 = 1500;

// Same constant but non-linux (for compilation on Windows host).
#[cfg(not(target_os = "linux"))]
#[allow(dead_code)]
const SPEAKER_END_COOLDOWN_MS: u64 = 1500;

impl ImbeForwarder {
    /// Total capacity of the IMBE batch queue (the mpsc bound).
    pub fn queue_capacity_max(&self) -> usize {
        self.imbe_tx.max_capacity()
    }
    /// Remaining slots in the IMBE batch queue. `max_capacity() -
    /// this` = current depth.
    pub fn queue_capacity_remaining(&self) -> usize {
        self.imbe_tx.capacity()
    }
    /// Current depth of the IMBE batch queue. Racy by at most one
    /// slot under concurrent producer/consumer — for a gauge that's
    /// fine.
    pub fn queue_depth_now(&self) -> usize {
        self.queue_capacity_max()
            .saturating_sub(self.queue_capacity_remaining())
    }

    /// 2026-04-26 per-freq AGC cache: blend a new sample.
    /// EMA alpha = 0.3 (new samples weighed 30%, history 70%) —
    /// smooths per-call variance from speaker level differences
    /// and end-of-call AGC drift while still tracking medium-term
    /// signal-strength shifts. Q9.7 arithmetic done in u32 to
    /// avoid overflow on the multiply.
    pub fn update_agc_cache(&self, freq_hz: u64, sample_q97: u16) {
        if sample_q97 == 0 {
            return; // ignore zero samples (uninitialised reads)
        }
        let Ok(mut cache) = self.traffic_agc_freq_cache.lock() else {
            return;
        };
        let entry = cache.entry(freq_hz).or_insert(sample_q97);
        // 0.3 * sample + 0.7 * existing, in fixed-point.
        let blended = ((*entry as u32) * 7 + (sample_q97 as u32) * 3) / 10;
        *entry = blended.min(u16::MAX as u32) as u16;
    }

    /// 2026-04-26 per-freq AGC cache lookup. Returns Q9.7 EMA
    /// gain for `freq_hz` if any clear call has previously closed
    /// on it. None on cache miss → caller falls back to GAIN_INIT
    /// (= 0 in the seed register, which the HDL Mux loads as 1.0×).
    pub fn agc_seed_for_freq(&self, freq_hz: u64) -> Option<u16> {
        self.traffic_agc_freq_cache.lock().ok()
            .and_then(|c| c.get(&freq_hz).copied())
    }

    /// 2026-04-26 snapshot of the AGC cache for /api/traffic.
    /// Returns sorted (freq_hz, q97_gain, gain_float) tuples so
    /// the dashboard can render a small "AGC seeds" panel.
    pub fn agc_cache_snapshot(&self) -> Vec<(u64, u16, f32)> {
        let Ok(cache) = self.traffic_agc_freq_cache.lock() else {
            return Vec::new();
        };
        let mut v: Vec<(u64, u16, f32)> = cache.iter()
            .map(|(&f, &g)| (f, g, g as f32 / 128.0))
            .collect();
        v.sort_by_key(|&(f, _, _)| f);
        v
    }
}

impl ImbeForwarder {
    pub fn new(
        imbe_tx: ImbeBatchTx,
    ) -> Self {
        Self {
            hdu_count: 0.into(),
            ldu1_count: 0.into(),
            ldu2_count: 0.into(),
            tdu_count: 0.into(),
            tdu_lc_count: 0.into(),
            framer_arm_hdu: 0.into(),
            framer_arm_ldu1: 0.into(),
            framer_arm_ldu2: 0.into(),
            framer_arm_tdu: 0.into(),
            framer_arm_tdu_lc: 0.into(),
            imbe_frames_extracted: 0.into(),
            imbe_frames_dropped: std::sync::Arc::new(0.into()),
            imbe_frames_dropped_idle: 0.into(),
            last_imbe_at_millis: 0.into(),
            vocoder_pcm_produced: 0.into(),
            vocoder_errors: 0.into(),
            vocoder_frames_encrypted: 0.into(),
            vocoder_frames_silent_observed: 0.into(),
            call_encrypted: false.into(),
            current_talkgroup: 0.into(),
            current_source: 0.into(),
            current_frequency_hz: 0.into(),
            current_call_id: 0.into(),
            current_channel: std::sync::Mutex::new(String::new()),
            encrypted_tg_history: std::sync::Mutex::new(std::collections::HashSet::new()),
            vocoder_reset_pending: false.into(),
            imbe_ring: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(128)),
            imbe_tx,
            airtime: std::sync::OnceLock::new(),
            seg_active: false.into(),
            seg_tg: 0.into(),
            seg_source: 0.into(),
            seg_call_id: 0.into(),
            seg_encrypted: false.into(),
            seg_air_ms: 0.into(),
            call_boundary_tx: std::sync::OnceLock::new(),
            last_observed_nac: 0.into(),
            tdulc_parse_attempts: 0.into(),
            tdulc_parse_motorola: 0.into(),
            tdulc_parse_gvcu: 0.into(),
            tdulc_parse_gvu: 0.into(),
            tdulc_parse_callterm: 0.into(),
            tdulc_parse_other: 0.into(),
            tdulc_parse_none: 0.into(),
            tdulc_last_lc_bytes: std::sync::Mutex::new([0u8; 9]),
            event_log: std::sync::OnceLock::new(),
            ws_event_tx: std::sync::OnceLock::new(),
            frames_submitted: 0.into(),
            frames_consumed: std::sync::Arc::new(0.into()),
            last_batch_tg: 0.into(),
            call_baseline_hdu: 0.into(),
            call_baseline_ldu1: 0.into(),
            call_baseline_ldu2: 0.into(),
            call_baseline_tdu: 0.into(),
            call_baseline_tdu_lc: 0.into(),
            call_baseline_imbe_extracted: 0.into(),
            call_baseline_imbe_dropped: 0.into(),
            call_baseline_pcm: 0.into(),
            call_baseline_errors: 0.into(),
            call_baseline_silent: 0.into(),
            call_baseline_unix_ms: 0.into(),
            queue_high_water: 0.into(),
            last_speaker_end_ms: 0.into(),
            speaker_end_deduplicated: 0.into(),
            speaker_end_invalid: 0.into(),
            last_traffic_agc_gain_q97: 0.into(),
            traffic_agc_freq_cache: std::sync::Mutex::new(
                std::collections::HashMap::new()),
            ldu1_fm_history: std::sync::Mutex::new(Ldu1FmHistory::new()),
            ldu1_lc_source_emitted: 0.into(),
            ldu1_lc_source_rejected_implausible: 0.into(),
            ldu1_lc_cc_mismatch_count: 0.into(),
            ldu1_last_mismatch: std::sync::Mutex::new(None),
            call_counts: crate::app::call_counters::CallCounterBook::default(),
            traffic_paused_by_teardown: false.into(),
            pll_wd_resets_onset: 0.into(),
            pll_wd_resets_pinned: 0.into(),
            pll_wd_enabled: false.into(),
            active_end_marker: 0.into(),
        }
    }

    /// Change 057: add to the per-call counters of the call the frame
    /// being decoded belongs to (air-time segment call in airtime mode,
    /// else the live call).
    fn count(&self, f: impl FnOnce(&mut crate::app::call_counters::CallCounts)) {
        self.call_counts.update(self.eff_call_id(), f);
    }

    /// Change 057: send `CallBoundaryKind::VoiceEnd` for the first
    /// LC-FEC-valid TDULC after voice of the segment's call (see
    /// `CallCounts::note_end_marker`). The lifecycle closes the call a
    /// short grace later unless voice resumes.
    fn maybe_emit_voice_end(
        &self,
        tx: &audio::CallBoundaryTx,
        tg: u16,
        lcw: &p25::voice_frame::TdulcLcw,
    ) {
        use std::sync::atomic::Ordering;
        use p25::voice_frame::TdulcLcw;
        let call_id = self.eff_call_id();
        let send = self
            .call_counts
            .update(call_id, |c| c.note_end_marker())
            .unwrap_or(false);
        if !send {
            return;
        }
        let lc = match lcw {
            TdulcLcw::MotorolaTalkComplete { .. } => "talk_complete",
            // SDRTrunk `LCCallTermination.isNetworkCommandedTeardown`:
            // system-controller addresses (0xFFFFFE is the TIA one).
            TdulcLcw::CallTermination { by_radio_id }
                if matches!(*by_radio_id, 0 | 0xFF_FFFD | 0xFF_FFFE | 0xFF_FFFF) =>
            {
                "network_teardown"
            }
            TdulcLcw::CallTermination { .. } => "call_termination",
            TdulcLcw::GroupVoiceChannelUser { .. } => "channel_user",
            _ => "link_control",
        };
        let air_ms = self.eff_captured_at_ms();
        let nac = self.last_observed_nac.load(Ordering::Relaxed);
        let _ = tx.send(audio::CallBoundary {
            kind: audio::CallBoundaryKind::VoiceEnd { call_id, air_ms, lc },
            nac,
            talkgroup: Some(tg),
            expected_submit_count: self.frames_submitted.load(Ordering::Relaxed),
        });
        let summary = format!("VOICE END call={} TG={} LC={}", call_id, tg, lc);
        self.emit_activity(&summary, serde_json::json!({
            "timestamp":  p25::control_channel::chrono_timestamp(),
            "event_type": "TRF_VOICE_END",
            "summary":    summary,
            "call_id":    call_id,
            "tg":         tg,
            "lc":         lc,
            "air_ms":     air_ms,
        }));
    }

    /// Validity precondition for any SpeakerEnd candidate — the
    /// follower must be actively locked on a talkgroup. A SpeakerEnd
    /// arriving while the follower is idle is either (a) a late tail
    /// of a prior call that already closed or (b) a false BCH-decoded
    /// LCW during a gap. Either way there's no active call for it to
    /// end, so reject.
    fn speaker_end_precondition_ok(&self) -> bool {
        use std::sync::atomic::Ordering;
        let tg = self.eff_tg();
        if tg == 0 {
            self.speaker_end_invalid.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Secondary validity check for LCW variants that are supposed
    /// to carry a real individual RID as the "BY:" field (Motorola
    /// TALK_COMPLETE). Reject if zero or a well-known system-
    /// controller address — those values indicate the LC FEC
    /// corrected garbage into a plausible-looking codeword.
    ///
    /// Standard LCCallTermination addresses (FFFFFD = MOTOROLA
    /// SYSTEM CONTROLLER 1, FFFFFF = MOTOROLA SYSTEM CONTROLLER 2,
    /// FFFFFE = TIA STANDARD) are LEGITIMATE on standard
    /// CallTermination but NOT on Motorola TALK_COMPLETE.
    fn radio_id_plausible(rid: u32) -> bool {
        rid != 0 && rid < 0xFF_FFFD
    }

    /// Common entry-point for firing `SpeakerEnd`. Checks the
    /// cooldown window and returns false (counting the dedup) if
    /// a SpeakerEnd was emitted too recently. Callers use the
    /// result to decide whether to still run their other side
    /// effects (activity-feed entry, counter bumps, etc.) — those
    /// are informational and safe either way; only the broadcast
    /// send is gated.
    fn try_emit_speaker_end(
        &self,
        tx: &audio::CallBoundaryTx,
        boundary: audio::CallBoundary,
    ) -> bool {
        use std::sync::atomic::Ordering;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last = self.last_speaker_end_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < SPEAKER_END_COOLDOWN_MS {
            self.speaker_end_deduplicated
                .fetch_add(1, Ordering::Relaxed);
            return false;
        }
        self.last_speaker_end_ms.store(now_ms, Ordering::Relaxed);
        let _ = tx.send(boundary);
        true
    }

    pub fn set_event_log(
        &self,
        log: std::sync::Arc<crate::services::event_log::EventLog>,
    ) {
        let _ = self.event_log.set(log);
    }

    pub fn set_ws_event_tx(
        &self,
        tx: tokio::sync::broadcast::Sender<String>,
    ) {
        let _ = self.ws_event_tx.set(tx);
    }

    /// Wire the `CallBoundaryTx` so the software decoder's TDULC LCW
    /// path can publish source-stamped boundary events. `OnceLock`
    /// keeps the setter lock-free on the hot path (TDULC arrives
    /// ~1×/call).
    pub fn set_boundary_tx(&self, tx: audio::CallBoundaryTx) {
        let _ = self.call_boundary_tx.set(tx);
    }

    // ── Change 054: air-time epoch plumbing ─────────────────────────

    /// Wire the traffic dibit ring shared state (epoch recorder).
    pub fn set_airtime(&self, shared: std::sync::Arc<DibitRingShared>) {
        let _ = self.airtime.set(shared);
    }

    /// True when the traffic reader runs in airtime mode: chain-meaning
    /// changes must be recorded as epoch cuts (`mark_epoch`) and framer
    /// resets are applied by the reader at the cut, not in real time.
    pub fn epochs_active(&self) -> bool {
        self.airtime.get().map(|s| s.epochs_active()).unwrap_or(false)
    }

    /// Snapshot of the live (follower / lifecycle) call context.
    pub fn live_context(&self) -> SegmentContext {
        use std::sync::atomic::Ordering;
        SegmentContext {
            tg: self.current_talkgroup.load(Ordering::Relaxed),
            source: self.current_source.load(Ordering::Relaxed),
            call_id: self.current_call_id.load(Ordering::Relaxed),
            encrypted: self.call_encrypted.load(Ordering::Relaxed),
            freq_hz: self.current_frequency_hz.load(Ordering::Relaxed),
        }
    }

    /// Record a chain epoch cut carrying the current live context. Call
    /// right AFTER the live atomics were changed. No-op unless the
    /// traffic reader runs in airtime mode.
    pub fn mark_epoch(&self, kind: EpochKind, framer_reset: bool) {
        if let Some(s) = self.airtime.get() {
            s.record_sw(kind, self.live_context(), framer_reset);
        }
    }

    /// Record a cut carrying an explicit context (e.g. the follower's
    /// grant hold: gate closed until a retune lands).
    pub fn mark_epoch_ctx(&self, kind: EpochKind, ctx: SegmentContext, framer_reset: bool) {
        if let Some(s) = self.airtime.get() {
            s.record_sw(kind, ctx, framer_reset);
        }
    }

    /// Store a new live call_id; records a call_id-only `CallOpen`
    /// epoch when it changed. Used by the call lifecycle (the only
    /// call_id writer). Only the id is applied at the cut: the
    /// follower's TG / retune actions carry the rest of the context.
    pub fn set_live_call_id(&self, call_id: u64) {
        use std::sync::atomic::Ordering;
        let prev = self.current_call_id.swap(call_id, Ordering::Relaxed);
        if prev != call_id {
            if let Some(s) = self.airtime.get() {
                s.record_call_id_at(call_id, crate::app::dibit_airtime::mono_us());
            }
        }
    }

    /// Reader: start feeding a segment decoded under `ctx`. Must be
    /// called with the traffic decoder's write lock held.
    pub fn begin_segment(&self, ctx: &SegmentContext) {
        use std::sync::atomic::Ordering;
        self.seg_tg.store(ctx.tg, Ordering::Relaxed);
        self.seg_source.store(ctx.source, Ordering::Relaxed);
        self.seg_call_id.store(ctx.call_id, Ordering::Relaxed);
        self.seg_encrypted.store(ctx.encrypted, Ordering::Relaxed);
        self.seg_active.store(true, Ordering::Relaxed);
    }

    /// Reader: air time (unix ms) of the dibit word about to be fed.
    pub fn set_segment_air_ms(&self, ms: u64) {
        self.seg_air_ms
            .store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// Reader: segment done. Returns the (possibly in-band latched)
    /// encrypted flag so the reader can carry it within the call.
    pub fn end_segment(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.seg_active.store(false, Ordering::Relaxed);
        self.seg_encrypted.load(Ordering::Relaxed)
    }

    fn seg(&self) -> bool {
        self.seg_active.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Effective TG for the frame being decoded.
    fn eff_tg(&self) -> u16 {
        use std::sync::atomic::Ordering;
        if self.seg() {
            self.seg_tg.load(Ordering::Relaxed)
        } else {
            self.current_talkgroup.load(Ordering::Relaxed)
        }
    }

    fn eff_source(&self) -> u32 {
        use std::sync::atomic::Ordering;
        if self.seg() {
            self.seg_source.load(Ordering::Relaxed)
        } else {
            self.current_source.load(Ordering::Relaxed)
        }
    }

    fn eff_call_id(&self) -> u64 {
        use std::sync::atomic::Ordering;
        if self.seg() {
            self.seg_call_id.load(Ordering::Relaxed)
        } else {
            self.current_call_id.load(Ordering::Relaxed)
        }
    }

    fn eff_encrypted(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.seg() {
            self.seg_encrypted.load(Ordering::Relaxed)
        } else {
            self.call_encrypted.load(Ordering::Relaxed)
        }
    }

    /// In-band (HDU / LDU2) encryption latch. In a segment it latches the
    /// segment context, and mirrors into the live flag only while the
    /// live call is still the segment's call — a late-decoded tail of a
    /// previous call must not mark the next call encrypted.
    fn latch_encrypted(&self) {
        use std::sync::atomic::Ordering;
        if self.seg() {
            self.seg_encrypted.store(true, Ordering::Relaxed);
            if self.current_call_id.load(Ordering::Relaxed)
                == self.seg_call_id.load(Ordering::Relaxed)
            {
                self.call_encrypted.store(true, Ordering::Relaxed);
            }
        } else {
            self.call_encrypted.store(true, Ordering::Relaxed);
        }
    }

    /// Capture time stamped on an IMBE batch: air time in a segment,
    /// dispatch wall time otherwise.
    fn eff_captured_at_ms(&self) -> u64 {
        use std::sync::atomic::Ordering;
        if self.seg() {
            let t = self.seg_air_ms.load(Ordering::Relaxed);
            if t != 0 {
                return t;
            }
        }
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn touch_imbe(&self, n_frames: u64) {
        use std::sync::atomic::Ordering;
        self.imbe_frames_extracted.fetch_add(n_frames, Ordering::Relaxed);
        self.count(|c| c.imbe_extracted += n_frames);
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_imbe_at_millis.store(now_millis, Ordering::Relaxed);
    }

    fn forward_frames(&self, frames: &[p25::voice_frame::ImbeFrameRaw; 9]) {
        use std::sync::atomic::Ordering;
        let tg = self.eff_tg();
        let enc = self.eff_encrypted();

        // No TG-0 drop gate: `current_talkgroup` briefly flickering to
        // 0 (grant refresh races, Idle bounce on retune) was dropping
        // real mid-call frames and producing audible skips. The
        // ring-buffer trace below still records tg=0 frames for
        // diagnostics via `/api/imbe_dump`.
        if let Ok(mut ring) = self.imbe_ring.lock() {
            for f in frames {
                if ring.len() >= 128 {
                    ring.pop_front();
                }
                ring.push_back((tg, enc, f.bits));
            }
        }

        // TG + source + call_id captured HERE, not on the receiver
        // side. If the follower retunes or updates current_* between
        // this send and the vocoder pulling the batch, the batch
        // still carries THIS call's labels — vocoder decodes + routes
        // correctly, no tail-drain-mislabelling splits at the
        // recorder. Phase 2h (2026-04-25) added call_id so the
        // recorder routes by GrantFollower call_id directly instead
        // of the prior tg+source heuristic.
        let src = self.eff_source();
        let call_id = self.eff_call_id();
        // Capture time for the recorder. Change 054: in airtime mode
        // this is the estimated production (air) time of the dibit that
        // completed the LDU, and the batch's call_id comes from the
        // air-time epoch, so the recorder routes by call_id. Otherwise
        // wall-clock dispatch time (pre-054 behaviour: routing by
        // capture time vs the session's [open_at_ms, close_at_ms]).
        let airtime = self.seg();
        let captured_at_ms = self.eff_captured_at_ms();
        let batch = ImbeBatch {
            talkgroup: tg,
            source: src,
            call_id,
            captured_at_ms,
            encrypted: enc,
            airtime,
            frames: *frames,
        };
        match self.imbe_tx.try_send(batch) {
            Ok(()) => {
                // Only advance when frames actually entered the queue —
                // a dropped send never produces PCM, so advancing would
                // make the recorder wait forever.
                self.frames_submitted.fetch_add(9, Ordering::Relaxed);
                // Update the session high-water mark on the queue
                // depth. Racy read (capacity can change between the
                // send returning and this load) but a gauge is fine.
                let depth = self.queue_depth_now() as u64;
                let hw = self.queue_high_water.load(Ordering::Relaxed);
                if depth > hw {
                    self.queue_high_water.store(depth, Ordering::Relaxed);
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
                self.call_counts.update(call_id, |c| c.imbe_dropped += 9);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
                self.call_counts.update(call_id, |c| c.imbe_dropped += 9);
            }
        }
    }
}

impl ImbeForwarder {
    /// Dual-dispatch every activity-log event: WebSocket for the
    /// dashboard, event_log ring for /api/log +
    /// /api/recordings/{id}/events. Ensures the dashboard feed and the
    /// structured log stay in sync so exported logs reconstruct what
    /// was seen.
    fn emit_activity(&self, summary: &str, evt: serde_json::Value) {
        if let Some(ws) = self.ws_event_tx.get() {
            let _ = ws.send(evt.to_string());
        }
        if let Some(log) = self.event_log.get() {
            log.push(
                crate::services::event_log::LogCategory::Voice,
                summary.to_string(),
                evt,
            );
        }
    }

    /// Emit one Duid-category log entry per dispatched data unit.
    /// Fires at the top of every on_hdu / on_ldu1 / on_ldu2 / on_tdu /
    /// on_tdu_lc handler so the `duid` log shows exactly what the
    /// framer dispatched — independent of anything we subsequently
    /// act on. Comparing this to the Grant / Imbe / Recorder trails
    /// answers "did we see it?" separately from "did we act on it?".
    fn log_duid(&self, duid: &'static str) {
        use std::sync::atomic::Ordering;
        if let Some(log) = self.event_log.get() {
            let nac = self.last_observed_nac.load(Ordering::Relaxed);
            let tg = self.eff_tg();
            let src = self.eff_source();
            log.push(
                crate::services::event_log::LogCategory::Duid,
                format!("traffic {} TG={} NAC=0x{:03X}", duid, tg, nac),
                serde_json::json!({
                    "chain":  "traffic",
                    "duid":   duid,
                    "tg":     tg,
                    "nac":    format!("0x{:03X}", nac),
                    "source": src,
                }),
            );
        }
    }
}

impl p25::control_channel::VoiceHandler for ImbeForwarder {
    // 2026-04-30 framer-divergence diagnostic. Decoder fires these
    // AFTER reaching the per-DUID arm but BEFORE body extraction.
    // Per-call delta of (framer_arm_X - X_count) = body-extraction
    // failures for that DUID (extract_imbe_frames returned None,
    // or the X arm reached but body never dispatched).
    // Change 057: each also counts for the frame's call (`count`).
    fn on_dispatch_arm_hdu(&self) {
        use std::sync::atomic::Ordering;
        self.framer_arm_hdu.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.framer_arm_hdu += 1);
    }
    fn on_dispatch_arm_ldu1(&self) {
        use std::sync::atomic::Ordering;
        self.framer_arm_ldu1.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.framer_arm_ldu1 += 1);
    }
    fn on_dispatch_arm_ldu2(&self) {
        use std::sync::atomic::Ordering;
        self.framer_arm_ldu2.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.framer_arm_ldu2 += 1);
    }
    fn on_dispatch_arm_tdu(&self) {
        use std::sync::atomic::Ordering;
        self.framer_arm_tdu.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.framer_arm_tdu += 1);
    }
    fn on_dispatch_arm_tdu_lc(&self) {
        use std::sync::atomic::Ordering;
        self.framer_arm_tdu_lc.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.framer_arm_tdu_lc += 1);
    }

    /// Surface the HDL-validated NAC (latched on every t=4 BCH-passing
    /// NID event from the LSM hardware in the traffic_lsm heartbeat,
    /// see main.rs:1406) to the PS framer so it can reject any of its
    /// own BCH-passing NIDs whose NAC disagrees. Returns 0 before the
    /// chain has produced a confirmed NAC, which the framer treats as
    /// "guard disabled" and accepts the decode.
    fn expected_nac(&self) -> u16 {
        use std::sync::atomic::Ordering;
        self.last_observed_nac.load(Ordering::Relaxed)
    }

    fn on_ldu1(
        &self,
        frames: &[p25::voice_frame::ImbeFrameRaw; 9],
        body_raw: &[u8],
    ) {
        use std::sync::atomic::Ordering;
        self.ldu1_count.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.ldu1 += 1);
        self.touch_imbe(9);
        self.forward_frames(frames);

        // Parse the embedded LDU1 Link Control Word to mirror
        // SDRTrunk's `LDU1 VOICE ... GROUP VOICE CHANNEL USER FM:<src>
        // TO:<TG>` line into the activity feed.
        let tg_locked = self.eff_tg();
        if tg_locked == 0 {
            return;
        }
        // LDU1 LC FEC = Hamming(10,6,3) + RS(24,12,13), weaker than
        // TDULC's Golay(24,12,7) + RS. GVCU-only here: end-of-call
        // MotorolaTalkComplete / CallTermination routed through LDU1
        // false-positive'd and over-split calls. TDULC owns end-of-call.
        let Some(source) = p25::voice_frame::parse_ldu1_source(body_raw)
        else {
            return;
        };
        let nac = self.last_observed_nac.load(Ordering::Relaxed);
        // service_options byte renders as SDRTrunk's
        // "SERVICE OPTIONS:PRI<n> CIRCUIT [ENCRYPTED]".
        let svc_opts = match p25::voice_frame::parse_ldu1_lcw(body_raw) {
            Some(p25::voice_frame::TdulcLcw::GroupVoiceChannelUser {
                service_options, ..
            }) => service_options,
            _ => 0,
        };
        let svc_opts_render =
            p25::tsbk::service_options::render(svc_opts);
        let summary = format!(
            "LDU1 GROUP VOICE CHANNEL USER TG={} SRC={} OPTS:{}",
            tg_locked, source, svc_opts_render,
        );
        // 2026-04-25: enrich the activity log entry with CC cross-
        // check fields so the dashboard can filter LDU1 LC entries
        // for cc_match=false. Operator philosophy: log every
        // disagreement, don't act on any of them.
        let cc_source_now = self.eff_source();
        let cc_known = cc_source_now != 0;
        let cc_match = !cc_known || cc_source_now == source;
        self.emit_activity(&summary, serde_json::json!({
            "timestamp":  p25::control_channel::chrono_timestamp(),
            "event_type": "TRF_LDU1_LC",
            "summary":    summary,
            "tg":         tg_locked,
            "source":     source,
            "nac":        nac,
            "service_options": svc_opts,
            "cc_src":     if cc_known { Some(cc_source_now) } else { None },
            "cc_match":   cc_match,
        }));

        // 2026-04-24 CC-centric refactor: LDU1 LC FM: is now used for
        // source enrichment via `CallBoundaryKind::TdulcComplete`, but
        // ONLY after two FEC-defence gates:
        //
        //   1. Plausibility — reject `0` (null), system-controller
        //      addresses (`>= 0xFF_FFFD`), and out-of-24-bit values.
        //      Catches isolated corruptions into system-reserved
        //      codeword regions.
        //   2. N-of-M voting over a rolling ring (`ldu1_fm_history`) —
        //      emit only when `LDU1_FM_VOTE_N` of the last
        //      `LDU1_FM_VOTE_M` decodes agree. The P25 redundancy
        //      principle: 326 ms LDU1 cadence × a real speaker holding
        //      PTT for seconds = ≥ 3 repeats of a legitimate FM:
        //      value before emission, while isolated FEC flips appear
        //      once and lose the vote.
        //
        // Downstream (`grant_stats::handle_boundary`) uses TdulcComplete
        // fill-in-only: updates OpenGrant.source if it was None (rare —
        // CC GRANT.SRC usually populates it first), never replaces an
        // existing CC-derived source. History audit 2026-04-24: 312/345
        // LDU1 LC FM: matched CC SRC exactly; the 3 true anomalies
        // (8-digit corrupted RIDs) are exactly what these gates block.
        if !Self::radio_id_plausible(source) {
            self.ldu1_lc_source_rejected_implausible
                .fetch_add(1, Ordering::Relaxed);
            return;
        }

        // 2026-04-25 CC-cross-check observation (NON-suppressing).
        //
        // Operator design philosophy: LDU1 LC FM: is the ACTUAL
        // CURRENT SPEAKER — the radio that's keying right now. CC
        // `GRP_VCH_GRANT.SRC` is the CHANNEL OWNER — who reserved
        // the call. They're DIFFERENT facts and we want BOTH.
        //
        // When they disagree, we still emit `TdulcComplete` with
        // the voted LDU1 LC FM so downstream (call_tracker /
        // grant_stats) can populate the `actual_speaker` field
        // separately from the CC `source`. The recorder's Fix 1
        // (fill-in-only on c.source) keeps the recording filename
        // stamped with CC SRC; the LDU1 LC value flows through as
        // a parallel signal, not a competing source.
        //
        // We ALSO capture forensic data on each disagreement:
        //
        //   1. Counter `ldu1_lc_cc_mismatch_count` increments so
        //      mismatch rate is observable per-call vs per-site.
        //   2. Most recent failing LC body bytes get latched into
        //      `ldu1_last_mismatch` (with timestamp + cc_src +
        //      ldu1_lc_source) for replay through an independent
        //      decoder / SDRTrunk comparison.
        //   3. Activity log entry above carries `cc_src`
        //      + `cc_match=false` so the dashboard can filter for
        //      these without touching extra endpoints.
        //
        // The IMBE frames themselves were already submitted to the
        // vocoder via `forward_frames()` at the top of `on_ldu1`,
        // BEFORE we reached this LC-parsing path — so the LC
        // observation has zero effect on audio decode either way.
        if cc_known && cc_source_now != source {
            self.ldu1_lc_cc_mismatch_count.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut slot) = self.ldu1_last_mismatch.lock() {
                let mut raw = [0u8; 24];
                let n = body_raw.len().min(24);
                raw[..n].copy_from_slice(&body_raw[..n]);
                *slot = Some(Ldu1LcMismatch {
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    tg: tg_locked,
                    cc_source: cc_source_now,
                    ldu1_lc_source: source,
                    raw_body_first_24: raw,
                });
            }
            // No early return — fall through to the voting + emission
            // path. CC stays authoritative for `source`; LDU1 LC fills
            // the `actual_speaker` slot.
        }

        let emit_consensus: Option<u32> = {
            let Ok(mut hist) = self.ldu1_fm_history.lock() else {
                return;
            };
            // Reset ring on TG change so a new call starts fresh
            // (previous speaker's FM: can't pollute the new vote).
            if hist.tg != tg_locked {
                hist.tg = tg_locked;
                hist.recent.clear();
                hist.last_emitted = 0;
            }
            if hist.recent.len() >= LDU1_FM_VOTE_M {
                hist.recent.pop_front();
            }
            hist.recent.push_back(source);
            // Count how many of the ring agree with the just-pushed
            // value. Self-counting `source` means at least 1; the
            // gate requires ≥ N across the ring.
            let votes = hist.recent.iter()
                .filter(|&&x| x == source)
                .count();
            if votes >= LDU1_FM_VOTE_N && hist.last_emitted != source {
                hist.last_emitted = source;
                Some(source)
            } else {
                None
            }
        };
        if let Some(consensus) = emit_consensus {
            self.ldu1_lc_source_emitted.fetch_add(1, Ordering::Relaxed);
            if let Some(tx) = self.call_boundary_tx.get() {
                let _ = tx.send(audio::CallBoundary {
                    kind: audio::CallBoundaryKind::TdulcComplete {
                        source: Some(consensus),
                    },
                    nac,
                    talkgroup: Some(tg_locked),
                    expected_submit_count: self
                        .frames_submitted
                        .load(Ordering::Relaxed),
                });
            }
        }
    }

    fn on_ldu2(
        &self,
        frames: &[p25::voice_frame::ImbeFrameRaw; 9],
        body_raw: &[u8],
    ) {
        use std::sync::atomic::Ordering;
        self.ldu2_count.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.ldu2 += 1);
        self.touch_imbe(9);
        self.forward_frames(frames);

        // Decode LDU2 ESS (Hamming10 + RS(24,16,9)) and mirror
        // SDRTrunk's `LDU2 VOICE LSD:... <encryption>` line. Emit for
        // both encrypted and unencrypted (SDRTrunk emits every LDU2).
        let Some(ess) = p25::voice_frame::parse_ldu2_ess(body_raw)
        else {
            return;
        };
        let (summary, fields) = if ess.is_encrypted() {
            // Phantom-ENC guard: LDU2 ESS FEC (RS(24,16,9)) is too weak
            // on its own — it accepts near-valid codewords on clear
            // weak-SNR transmissions, producing random algorithm_id
            // bytes (observed 0x08, 0x50, 0xB4). Two checks:
            //  1. `is_spec_algorithm` rejects bytes not in TIA-102.AABD
            //     or SDRTrunk Motorola extensions.
            //  2. HDU-trust: if the HDU said UNENCRYPTED (Golay18 +
            //     RS(63,47,17) is much stronger, thus authoritative),
            //     never let a later LDU2 ESS flip the gate. Without
            //     this, one bit-corrupt LDU2 trips sticky-true and the
            //     vocoder drops the rest of the call (observed 4,401
            //     frames `Encrypted Skipped` on TG 301).
            // Mid-call key-refresh still works: encrypted HDU sets the
            // gate, every LDU2 after refreshes here.
            if !ess.is_spec_algorithm() {
                return;
            }
            if !self.eff_encrypted() {
                // HDU said clear — silently drop (don't trip the gate).
                return;
            }
            // Refresh sticky-true from ESS in case of mid-call rekey.
            self.latch_encrypted();
            let mi_hex: String = ess
                .message_indicator
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect();
            let summary = format!(
                "LDU2 VOICE ENCRYPTED ENCRYPTION:0x{:02X} KEY:{} MI:{}",
                ess.algorithm_id, ess.key_id, mi_hex
            );
            let fields = serde_json::json!({
                "timestamp":  p25::control_channel::chrono_timestamp(),
                "event_type": "TRF_LDU2_ESS",
                "summary":    summary,
                "encrypted":  true,
                "algorithm":  ess.algorithm_id,
                "key_id":     ess.key_id,
                "mi":         mi_hex,
            });
            (summary, fields)
        } else {
            let summary = "LDU2 VOICE UNENCRYPTED".to_string();
            let fields = serde_json::json!({
                "timestamp":  p25::control_channel::chrono_timestamp(),
                "event_type": "TRF_LDU2_ESS",
                "summary":    summary,
                "encrypted":  false,
            });
            (summary, fields)
        };
        self.emit_activity(&summary, fields);
    }

    fn on_hdu(&self, body_raw: &[u8]) {
        use std::sync::atomic::Ordering;
        self.hdu_count.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.hdu += 1);

        // Snapshot current cumulative counters as the baseline for
        // this call. /api/traffic exposes `current_call_* = global
        // - baseline` so the dashboard shows per-current-call numbers
        // distinct from session totals.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.call_baseline_hdu.store(
            self.hdu_count.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_ldu1.store(
            self.ldu1_count.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_ldu2.store(
            self.ldu2_count.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_tdu.store(
            self.tdu_count.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_tdu_lc.store(
            self.tdu_lc_count.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_imbe_extracted.store(
            self.imbe_frames_extracted.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_imbe_dropped.store(
            self.imbe_frames_dropped.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_pcm.store(
            self.vocoder_pcm_produced.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_errors.store(
            self.vocoder_errors.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_silent.store(
            self.vocoder_frames_silent_observed.load(Ordering::Relaxed), Ordering::Relaxed);
        self.call_baseline_unix_ms.store(now_ms, Ordering::Relaxed);

        // Do NOT clear `current_source` here. In observed operation
        // the CC grant with fresh SRC arrives 1-2 s BEFORE HDU on the
        // traffic chain, so clearing at HDU time wipes the good
        // CC-grant SRC.
        // With LDU1 LC stamping removed, the grant follower is the only
        // writer to `current_source`; end-of-speaker MOT_TC TDULC
        // (Golay24+RS) provides a second-opinion stamp at finalise.

        // Decode HDU body (Golay18 + RS(63,47,17)). SDRTrunk equivalent:
        // `HDU TALKGROUP:<tg> [ENCRYPTION:<alg> KEY:<id> MI:<hex> |
        // UNENCRYPTED]`. HDU fires once per speaker, so always emit.
        let Some(hdr) = p25::voice_frame::parse_hdu_body(body_raw)
        else {
            return;
        };
        // HDU gates encryption for the speaker: algorithm_id is
        // authoritative per-speaker state, and mid-grant rekey is only
        // visible here (the grant flag would be stale). `is_spec_algorithm`
        // is symmetric with the LDU2 path so a bit-corrupt HDU decode
        // can't trip the gate on a nonsense algorithm byte.
        if hdr.is_encrypted() && hdr.is_spec_algorithm() {
            self.latch_encrypted();
        }
        let summary = if hdr.is_encrypted() {
            let mi_hex: String = hdr
                .message_indicator
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect();
            format!(
                "HDU TALKGROUP:{} ENCRYPTION:0x{:02X} KEY:{} MI:{}",
                hdr.talkgroup, hdr.algorithm_id, hdr.key_id, mi_hex,
            )
        } else {
            format!("HDU TALKGROUP:{} UNENCRYPTED", hdr.talkgroup)
        };
        self.emit_activity(&summary, serde_json::json!({
            "timestamp":  p25::control_channel::chrono_timestamp(),
            "event_type": "TRF_HDU_INFO",
            "summary":    summary,
            "tg":         hdr.talkgroup,
            "encrypted":  hdr.is_encrypted(),
            "algorithm":  hdr.algorithm_id,
            "key_id":     hdr.key_id,
        }));
    }

    fn on_tdu(&self) {
        use std::sync::atomic::Ordering;
        self.tdu_count.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.tdu += 1);
        // Bare TDU (DUID=0x3, no LCW) does NOT close calls. Per
        // operator: end-of-call must come from LCW-FEC-decoded
        // TDULC (CallTermination or Motorola TalkComplete) only —
        // both carry an inner Golay24+RS(24,12,13) payload that
        // gates against over-correction at the framer's wider BCH
        // sphere. A bare TDU has no inner FEC to verify, so
        // anything BCH-passing into the 0x3 nibble that came from
        // biased noise would otherwise emit a phantom call-end.
        // Counter still bumps for diagnostic visibility.
    }

    fn on_tdu_lc(&self, body_raw: &[u8]) {
        use std::sync::atomic::Ordering;
        self.tdu_lc_count.fetch_add(1, Ordering::Relaxed);
        self.count(|c| c.tdu_lc += 1);

        // Motorola TALK_COMPLETE LCW -> boundary event with BY: source.
        // Non-Motorola sites always land on Other / GroupVoiceChannelUser.
        let Some(tx) = self.call_boundary_tx.get() else { return; };
        let tg = self.eff_tg();
        if tg == 0 {
            return;
        }

        self.tdulc_parse_attempts.fetch_add(1, Ordering::Relaxed);
        let checked = p25::voice_frame::parse_tdulc_lcw_checked(body_raw);
        // Change 057: end of transmission. Any TDULC whose LC passes
        // RS(24,12,13) after this call's voice ends the transmission,
        // like SDRTrunk's `processTDULC` (valid LCW → call event end).
        // Independent of the source-stamp path below (cooldown, BY:
        // checks), which only feeds `SpeakerEnd`.
        if let Some((lcw, true)) = checked.as_ref() {
            self.maybe_emit_voice_end(tx, tg, lcw);
        }
        let parsed = checked.map(|(lcw, _)| lcw);

        // Snapshot first 9 bytes of the post-extraction LC so a live
        // `/api/traffic` poll shows what the parser is seeing when the
        // Motorola counter won't budge.
        if let Some(bytes) = p25::voice_frame::tdulc_lc_bytes(body_raw) {
            if let Ok(mut slot) = self.tdulc_last_lc_bytes.lock() {
                *slot = bytes;
            }
        }

        match parsed {
            Some(p25::voice_frame::TdulcLcw::MotorolaTalkComplete {
                by_radio_id,
            }) => {
                self.tdulc_parse_motorola.fetch_add(1, Ordering::Relaxed);
                let nac = self.last_observed_nac.load(Ordering::Relaxed);
                // Three-way cross-check for validity:
                //   1. current_talkgroup != 0 (follower active)
                //   2. by_radio_id is a plausible individual RID
                //      (non-zero, not a system controller)
                //   3. **by_radio_id matches current_source** — the
                //      grant follower already stamped the speaker's
                //      RID from the CC's GRP_VCH_GRANT.SRC. A
                //      legitimate TDULC Motorola TALK_COMPLETE on
                //      this call carries the SAME RID in its BY:
                //      field. A false BCH-corrected LCW producing a
                //      different RID fails this check and is
                //      rejected.
                //
                //   If current_source is 0 (grant didn't carry SRC,
                //   pre-LDU1 timing, or follower not yet populated),
                //   fall back to (1)+(2) only — can't cross-check
                //   against a blank.
                let current_src = self.eff_source();
                let src_ok = current_src == 0
                    || current_src == by_radio_id;
                if self.speaker_end_precondition_ok()
                    && Self::radio_id_plausible(by_radio_id)
                    && src_ok
                {
                    self.try_emit_speaker_end(tx, audio::CallBoundary {
                        kind: audio::CallBoundaryKind::SpeakerEnd {
                            source: Some(by_radio_id),
                            kind: audio::TerminatorKind::MotTalkComplete,
                        },
                        nac,
                        expected_submit_count: self
                            .frames_submitted
                            .load(Ordering::Relaxed),
                        talkgroup: Some(tg),
                    });
                } else {
                    self.speaker_end_invalid
                        .fetch_add(1, Ordering::Relaxed);
                    // Log the rejection — helps pattern-match
                    // "phantom TalkComplete with wrong BY:" in the
                    // SDRTrunk-style timeseries log.
                    if current_src != 0 && current_src != by_radio_id {
                        tracing::debug!(
                            target: "p25_traffic",
                            "TDULC MOT_TC BY:{} does not match \
                             current_source:{} on TG:{} — rejected",
                            by_radio_id, current_src, tg,
                        );
                    }
                }
                // Mirror SDRTrunk's `TDULC MOTOROLA TALK COMPLETE BY:<src>`
                // line into the activity feed + event log.
                let summary = format!(
                    "TDULC MOTOROLA TALK COMPLETE TG={} SRC={}",
                    tg, by_radio_id,
                );
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_MOT",
                    "summary":    summary,
                    "tg":         tg,
                    "source":     by_radio_id,
                    "nac":        nac,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::GroupVoiceChannelUser {
                talkgroup: lc_tg,
                source_radio_id: _,
                service_options: _,
            }) => {
                self.tdulc_parse_gvcu.fetch_add(1, Ordering::Relaxed);
                // Mirror SDRTrunk's `TDULC GROUP VOICE CHANNEL USER
                // FM:0 TO:<TG>`. Fires many times per call (tail burst);
                // dashboard collapses duplicates by type.
                let summary = format!(
                    "TDULC GROUP VOICE CHANNEL USER TG={}", lc_tg,
                );
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC",
                    "summary":    summary,
                    "tg":         tg,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::GroupVoiceChannelUpdate {
                talkgroup_a, channel_a_band, channel_a_number,
                talkgroup_b, channel_b_band, channel_b_number,
                has_channel_b,
            }) => {
                self.tdulc_parse_gvu.fetch_add(1, Ordering::Relaxed);
                let summary = if has_channel_b {
                    format!(
                        "TDULC GROUP VOICE CHANNEL UPDATE TG_A:{} CH_A:{}-{} TG_B:{} CH_B:{}-{}",
                        talkgroup_a, channel_a_band, channel_a_number,
                        talkgroup_b, channel_b_band, channel_b_number,
                    )
                } else {
                    format!(
                        "TDULC GROUP VOICE CHANNEL UPDATE TG_A:{} CH_A:{}-{}",
                        talkgroup_a, channel_a_band, channel_a_number,
                    )
                };
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_GVU",
                    "summary":    summary,
                    "tg":         tg,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::CallTermination { by_radio_id }) => {
                self.tdulc_parse_callterm.fetch_add(1, Ordering::Relaxed);
                // Standard CALL_TERMINATION: `by_radio_id` should be
                // a well-known system-controller address (FFFFFD /
                // FFFFFE / FFFFFF). If it ISN'T one of those AND
                // isn't zero, the LC FEC probably corrected noise
                // into a plausible-looking codeword — reject rather
                // than fire a spurious SpeakerEnd on a call we don't
                // actually think ended.
                //
                // Precondition also applies: current_talkgroup must
                // be non-zero (follower actively on a call).
                let legit_controller = by_radio_id == 0xFFFFFD
                    || by_radio_id == 0xFFFFFE
                    || by_radio_id == 0xFFFFFF;
                if self.speaker_end_precondition_ok()
                    && legit_controller
                {
                    let nac = self.last_observed_nac.load(Ordering::Relaxed);
                    // Carry current_source (set by grant follower
                    // from GVCG.SRC) on the boundary event. Observed
                    // 2026-04-20 log pattern: a call ends with 4
                    // consecutive CALL_TERMINATION packets followed
                    // by 1 MOT_TC. The first CALL_TERMINATION wins
                    // the cooldown and dedups the later MOT_TC, so
                    // if we leave source=None here the recorder
                    // loses the speaker RID on grants that didn't
                    // carry SRC. Passing current_source keeps the
                    // stamp intact.
                    let current_src = self.eff_source();
                    let src_for_boundary = if current_src != 0
                        { Some(current_src) } else { None };
                    self.try_emit_speaker_end(tx, audio::CallBoundary {
                        kind: audio::CallBoundaryKind::SpeakerEnd {
                            source: src_for_boundary,
                            kind: audio::TerminatorKind::CallTermination,
                        },
                        nac,
                        talkgroup: Some(tg),
                        expected_submit_count: self
                            .frames_submitted
                            .load(Ordering::Relaxed),
                    });
                } else {
                    self.speaker_end_invalid
                        .fetch_add(1, Ordering::Relaxed);
                }
                // Relabel the well-known system-controller teardown
                // addresses per SDRTrunk `LCCallTermination`
                // (MOTOROLA_SYSTEM_CONTROLLER_1 = 0xFFFFFD,
                // MOTOROLA_SYSTEM_CONTROLLER_2 = 0xFFFFFF,
                // HARRIS_SYSTEM_CONTROLLER = 0x000000).
                let by_label = match by_radio_id {
                    0xFFFFFD => "MOTOROLA SYS CTRL (0xFFFFFD)".to_string(),
                    0xFFFFFF => "MOTOROLA SYS CTRL (0xFFFFFF)".to_string(),
                    0x000000 => "HARRIS SYS CTRL".to_string(),
                    id => format!("{}", id),
                };
                let summary = format!("TDULC CALL TERMINATION BY:{}", by_label);
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_CALL_TERM",
                    "summary":    summary,
                    "tg":         tg,
                    "by":         by_radio_id,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::RfssStatusBroadcast {
                lra, system_id, rfss_id, site_id,
                channel_band, channel_number, service_class,
            }) => {
                self.tdulc_parse_other.fetch_add(1, Ordering::Relaxed);
                let summary = format!(
                    "TDULC RFSS STATUS BROADCAST LRA:{} SYS:{:03X} RFSS:{} SITE:{} CH:{}-{} SVC:0x{:02X}",
                    lra, system_id, rfss_id, site_id,
                    channel_band, channel_number, service_class,
                );
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_RFSS_STS",
                    "summary":    summary,
                    "tg":         tg,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::NetStatusBroadcast {
                wacn, system_id,
                channel_band, channel_number, service_class,
            }) => {
                self.tdulc_parse_other.fetch_add(1, Ordering::Relaxed);
                let summary = format!(
                    "TDULC NET STATUS BROADCAST WACN:{:05X} SYS:{:03X} CH:{}-{} SVC:0x{:02X}",
                    wacn, system_id,
                    channel_band, channel_number, service_class,
                );
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_NET_STS",
                    "summary":    summary,
                    "tg":         tg,
                }));
            }
            Some(p25::voice_frame::TdulcLcw::Other { opcode, mfid }) => {
                self.tdulc_parse_other.fetch_add(1, Ordering::Relaxed);
                let summary = format!(
                    "TDULC OTHER OP:0x{:02X} MFID:0x{:02X}", opcode, mfid
                );
                self.emit_activity(&summary, serde_json::json!({
                    "timestamp":  p25::control_channel::chrono_timestamp(),
                    "event_type": "TRF_TDULC_OTHER",
                    "summary":    summary,
                    "tg":         tg,
                }));
            }
            None => {
                self.tdulc_parse_none.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

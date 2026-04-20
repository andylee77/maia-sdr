//! ImbeForwarder — traffic-chain voice handler.
//!
//! Extracted from main.rs on 2026-04-19. Implements
//! crate::protocol::p25::control_channel::VoiceHandler over the raw LDU/TDU
//! callbacks from the traffic LSM decoder. Owns the IMBE-batch
//! mpsc sender to the vocoder task and the call-boundary broadcast
//! tx that feeds the recorder.

use std::sync::atomic::Ordering;

use crate::audio;
use crate::protocol::p25;

/// Phase 7D: voice frame handler that counts IMBE events AND forwards
/// raw frames to the vocoder task via an mpsc channel.
///
/// Implements `p25::control_channel::VoiceHandler`. Installed on
/// the `traffic_lsm_decoder` via `set_voice_handler`. Held as
/// `Arc<dyn VoiceHandler + Send + Sync>`.
///
/// Uses `try_send` (non-async) on the mpsc channel because the
/// `VoiceHandler` trait methods take `&self` and are called from
/// synchronous `process_dibit` code inside a tokio task. If the
/// channel is full the frame batch is dropped and `imbe_frames_dropped`
/// is incremented -- the vocoder task is expected to keep up at
/// ~50 frames/sec (one LDU every ~180 ms).
pub struct ImbeForwarder {
    pub hdu_count: std::sync::atomic::AtomicU64,
    pub ldu1_count: std::sync::atomic::AtomicU64,
    pub ldu2_count: std::sync::atomic::AtomicU64,
    pub tdu_count: std::sync::atomic::AtomicU64,
    pub tdu_lc_count: std::sync::atomic::AtomicU64,
    pub imbe_frames_extracted: std::sync::atomic::AtomicU64,
    /// 2026-04-19 Arc-wrapped so the recorder task can hold a
    /// cloneable handle and log per-call drop-delta into each
    /// finalise event. Same underlying counter still surfaced via
    /// `/api/traffic` and incremented in `forward_frames` when
    /// the vocoder input queue is full.
    pub imbe_frames_dropped: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Phase 7F.3 (2026-04-14): count of LDU frame batches the
    /// forwarder refused to hand to the vocoder because
    /// `current_talkgroup == 0` (follower is Idle). These are
    /// framer false-positives extracted from residual dibits on the
    /// traffic DDC between calls -- the source of the "TG=0 phantom
    /// call" events in the log.
    pub imbe_frames_dropped_idle: std::sync::atomic::AtomicU64,
    pub last_imbe_at_millis: std::sync::atomic::AtomicU64,
    /// Vocoder stats -- updated by the vocoder task, read by /api/traffic.
    pub vocoder_pcm_produced: std::sync::atomic::AtomicU64,
    pub vocoder_errors: std::sync::atomic::AtomicU64,
    pub vocoder_frames_encrypted: std::sync::atomic::AtomicU64,
    /// Observability-only count of silent frames (peak below
    /// `SILENT_PEAK`) emitted by JMBE. Originally (Phase 10.6 /
    /// 2026-04-18) this gate DROPPED silent frames, but 2026-04-19
    /// late we reverted the drop — a burst of JMBE-silent frames
    /// during a single speaker's turn was exceeding the recorder's
    /// grace window and splitting one call into multiple files.
    /// Silent frames now pass through to both recorder and audio
    /// broadcast; the counter just tallies them for diagnostics.
    pub vocoder_frames_silent_observed: std::sync::atomic::AtomicU64,
    /// Set by the grant follower task when it locks onto a TG. The
    /// vocoder task reads this to skip encrypted calls.
    pub call_encrypted: std::sync::atomic::AtomicBool,
    /// Current talkgroup (set by grant follower, read by vocoder
    /// to tag AudioChunks). 0 = idle / unknown.
    pub current_talkgroup: std::sync::atomic::AtomicU16,
    /// 2026-04-19: current source radio ID, set by the grant
    /// follower from `GRP_VCH_GRANT.FM`. 0 = unknown (grant carried
    /// no source, or we only have a `GRP_VCH_GRNT_UPD` which doesn't
    /// carry source). Read by the recorder to stamp the filename as
    /// soon as audio starts flowing — no need to wait for the
    /// traffic-channel LDU1 LC / TDULC Motorola end-code path. The
    /// two traffic-side sources still refresh this atomic so the
    /// most-recent wins (mid-call source switches on a rebroadcast
    /// grant would otherwise miss the recorder).
    pub current_source: std::sync::atomic::AtomicU32,
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
    /// Channel to the vocoder task. Each send is a batch of 9 frames
    /// (one LDU's worth = 180 ms of audio).
    imbe_tx: tokio::sync::mpsc::Sender<[p25::voice_frame::ImbeFrameRaw; 9]>,
    /// 2026-04-19: optional broadcast channel for call-boundary
    /// events emitted from the software decoder's voice handler. Set
    /// after `ImbeForwarder::new` via `set_boundary_tx`. `None` on
    /// decoder instances that don't split calls (e.g. control-channel
    /// decoders — they never see traffic DUIDs).
    call_boundary_tx:
        std::sync::OnceLock<audio::CallBoundaryTx>,
    /// 2026-04-19: last NAC observed by the software framer, used to
    /// tag the `CallBoundary` events emitted from `on_tdu_lc`. Set by
    /// the traffic-LSM heartbeat whenever it forwards a NID event.
    pub last_observed_nac: std::sync::atomic::AtomicU16,
    /// 2026-04-19: event-log ring reference so TDULC LCW parses can
    /// emit `MOTOROLA TALK COMPLETE BY:<src>` / `GROUP VOICE CHANNEL
    /// USER` entries into the dashboard activity feed (matching
    /// SDRTrunk's `decoded_messages.log` style). `None` until
    /// `set_event_log` is called post-construction.
    pub event_log:
        std::sync::OnceLock<std::sync::Arc<crate::services::event_log::EventLog>>,
    /// 2026-04-19: WebSocket event tx for the live activity feed.
    /// Plain `String` broadcast — same channel the control-channel
    /// decoder uses — so TDULC LCW events appear inline with TSBK
    /// events in the dashboard's Live Activity stream.
    pub ws_event_tx: std::sync::OnceLock<
        tokio::sync::broadcast::Sender<String>,
    >,

    // 2026-04-19 TDULC LCW parse diagnostics. All four counters fire
    // from `on_tdu_lc` — the sum is the number of TDULC bodies the
    // parser actually ran against (i.e. tg != 0 at dispatch time).
    // Surfaces via /api/traffic so we can see whether:
    //   - the parser is reaching every TDULC (attempts should track
    //     tdu_lc_count once the follower is active), and
    //   - the site is emitting Motorola `TALK_COMPLETE` at all
    //     (motorola vs gvcu vs other distribution).
    pub tdulc_parse_attempts: std::sync::atomic::AtomicU64,
    pub tdulc_parse_motorola: std::sync::atomic::AtomicU64,
    pub tdulc_parse_gvcu: std::sync::atomic::AtomicU64,
    pub tdulc_parse_other: std::sync::atomic::AtomicU64,
    pub tdulc_parse_none: std::sync::atomic::AtomicU64,
    /// First 9 bytes (72 bits) of the most recent post-dibits LC
    /// payload, for offline inspection when the Motorola counter is
    /// stuck at zero. Mutex-behind because it's a single sample, not
    /// a hot counter.
    pub tdulc_last_lc_bytes: std::sync::Mutex<[u8; 9]>,
    /// 2026-04-19 count-based recorder close. Total IMBE frames
    /// successfully pushed to the vocoder's `imbe_tx` queue
    /// (increments by 9 per successful `forward_frames`). When a
    /// `CallBoundary` is dispatched, this value is snapshot into
    /// `CallBoundary::expected_submit_count`. The recorder waits
    /// until `frames_consumed` reaches the snapshotted value before
    /// actually calling `finalize()`, ensuring trailing PCM chunks
    /// append to the closing recording rather than opening a new
    /// to-be-discarded tail fragment.
    pub frames_submitted: std::sync::atomic::AtomicU64,
    /// 2026-04-19 count-based recorder close. Incremented by the
    /// vocoder task each time it pops a batch of frames off
    /// `imbe_rx` (by 9 per recv), regardless of whether those
    /// frames were synthesised into PCM or skipped (encrypted
    /// call, malformed IMBE). Read by the recorder task to match
    /// against `CallBoundary::expected_submit_count`.
    ///
    /// Wrapped in `Arc` so the recorder task can hold an
    /// independent handle without needing a reference to the whole
    /// `ImbeForwarder`.
    pub frames_consumed: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl ImbeForwarder {
    pub fn new(
        imbe_tx: tokio::sync::mpsc::Sender<[p25::voice_frame::ImbeFrameRaw; 9]>,
    ) -> Self {
        Self {
            hdu_count: 0.into(),
            ldu1_count: 0.into(),
            ldu2_count: 0.into(),
            tdu_count: 0.into(),
            tdu_lc_count: 0.into(),
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
            encrypted_tg_history: std::sync::Mutex::new(std::collections::HashSet::new()),
            vocoder_reset_pending: false.into(),
            imbe_ring: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(128)),
            imbe_tx,
            call_boundary_tx: std::sync::OnceLock::new(),
            last_observed_nac: 0.into(),
            tdulc_parse_attempts: 0.into(),
            tdulc_parse_motorola: 0.into(),
            tdulc_parse_gvcu: 0.into(),
            tdulc_parse_other: 0.into(),
            tdulc_parse_none: 0.into(),
            tdulc_last_lc_bytes: std::sync::Mutex::new([0u8; 9]),
            event_log: std::sync::OnceLock::new(),
            ws_event_tx: std::sync::OnceLock::new(),
            frames_submitted: 0.into(),
            frames_consumed: std::sync::Arc::new(0.into()),
        }
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

    fn touch_imbe(&self, n_frames: u64) {
        use std::sync::atomic::Ordering;
        self.imbe_frames_extracted.fetch_add(n_frames, Ordering::Relaxed);
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_imbe_at_millis.store(now_millis, Ordering::Relaxed);
    }

    fn forward_frames(&self, frames: &[p25::voice_frame::ImbeFrameRaw; 9]) {
        use std::sync::atomic::Ordering;
        let tg = self.current_talkgroup.load(Ordering::Relaxed);
        let enc = self.call_encrypted.load(Ordering::Relaxed);

        // 2026-04-19 late: TG-0 frame drop gate removed. The original
        // Phase 7F.3 gate was meant to suppress "phantom LDU" frames
        // the framer extracted from between-call noise, but in
        // practice it was also dropping real mid-call frames whenever
        // `current_talkgroup` flickered to 0 transiently (grant
        // refresh races, brief Idle bounce on retune). Result was
        // audible audio skips in legit calls. The ring-buffer trace
        // below still records tg=0 frames for diagnostic use via
        // `/api/imbe_dump`; only the vocoder-side drop is gone.
        // `imbe_frames_dropped_idle` is preserved as-a-counter so
        // existing dashboard fields don't break, but it no longer
        // increments.
        if let Ok(mut ring) = self.imbe_ring.lock() {
            for f in frames {
                if ring.len() >= 128 {
                    ring.pop_front();
                }
                ring.push_back((tg, enc, f.bits));
            }
        }

        match self.imbe_tx.try_send(*frames) {
            Ok(()) => {
                // 2026-04-19 count-based recorder close — advance the
                // submitted counter only when the frames actually
                // entered the vocoder queue. A dropped send (queue
                // full / closed) never produces PCM, so we mustn't
                // advance or the recorder would wait forever for
                // chunks that will never come.
                self.frames_submitted.fetch_add(9, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                self.imbe_frames_dropped.fetch_add(9, Ordering::Relaxed);
            }
        }
    }
}

impl ImbeForwarder {
    /// Dual-dispatch every activity-log event: WebSocket for the
    /// dashboard, event_log ring for /api/log + /api/recordings/{id}/events.
    /// 2026-04-19 late: previously every decoded-message emit site had
    /// its own `if let Some(ws) = self.ws_event_tx.get() { ... }` block
    /// and a fraction of them also pushed to event_log. Result: the
    /// dashboard activity log showed decoded messages that the
    /// structured log didn't, so exported log files couldn't be used
    /// to reconstruct what was seen. This helper makes every activity
    /// emit land in both sinks.
    fn emit_activity(&self, summary: &str, evt: serde_json::Value) {
        if let Some(ws) = self.ws_event_tx.get() {
            let _ = ws.send(evt.to_string());
        }
        if let Some(log) = self.event_log.get() {
            log.push(
                crate::services::event_log::LogCategory::Imbe,
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
            let tg = self.current_talkgroup.load(Ordering::Relaxed);
            let src = self.current_source.load(Ordering::Relaxed);
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
    fn on_ldu1(
        &self,
        frames: &[p25::voice_frame::ImbeFrameRaw; 9],
        body_raw: &[u8],
    ) {
        use std::sync::atomic::Ordering;
        self.log_duid("LDU1");
        self.ldu1_count.fetch_add(1, Ordering::Relaxed);
        self.touch_imbe(9);
        self.forward_frames(frames);

        // 2026-04-19: parse the embedded LDU1 Link Control Word so we
        // get the mid-call source (FM:) / TG (TO:) the way SDRTrunk
        // logs `LDU1 VOICE ... GROUP VOICE CHANNEL USER FM:<src>
        // TO:<TG>`. When the parser finds a non-zero source, push a
        // boundary event so the recorder stamps it into the active
        // call's filename — no need to wait for the end-of-speaker
        // Motorola TALK_COMPLETE TDULC.
        let tg_locked = self.current_talkgroup.load(Ordering::Relaxed);
        if tg_locked == 0 {
            return;
        }
        // LDU1 LC FEC = Hamming(10,6,3) + RS(24,12,13) is weaker than
        // TDULC's Golay(24,12,7) + RS chain. Earlier this session we
        // also routed LDU1-classified MotorolaTalkComplete /
        // CallTermination LCWs through SpeakerEnd (end-of-call finalise)
        // but that false-positive'd on noisy LDU1 LCs and over-split
        // single-speaker calls into 4+ files. Reverted to GVCU-only:
        // mid-call source stamp, no end-of-call routing on LDU1. TDULC
        // is FEC-stronger and still carries the end-of-call signal.
        let Some(source) = p25::voice_frame::parse_ldu1_source(body_raw)
        else {
            return;
        };
        let nac = self.last_observed_nac.load(Ordering::Relaxed);
        // Pull service_options byte off the LDU1 GVCU LCW to render
        // "PRI<n> CIRCUIT" / "... ENCRYPTED" matching SDRTrunk's LDU1
        // VOICE line format ("SERVICE OPTIONS:PRI4 CIRCUIT").
        let svc_opts = match p25::voice_frame::parse_ldu1_lcw(body_raw) {
            Some(p25::voice_frame::TdulcLcw::GroupVoiceChannelUser {
                service_options, ..
            }) => service_options,
            _ => 0,
        };
        let svc_opts_render =
            p25::tsbk::service_options::render(svc_opts);
        let summary = format!(
            "LDU1 VOICE GROUP VOICE CHANNEL USER FM:{} TO:{} SERVICE OPTIONS:{}",
            source, tg_locked, svc_opts_render,
        );
        self.emit_activity(&summary, serde_json::json!({
            "timestamp":  p25::control_channel::chrono_timestamp(),
            "event_type": "TRF_LDU1_LC",
            "summary":    summary,
            "tg":         tg_locked,
            "source":     source,
            "nac":        nac,
            "service_options": svc_opts,
        }));
        // 2026-04-19 LDU1-LC-as-stamp removal. Previously we both
        //   (a) emitted a `TdulcComplete { source }` boundary event
        //       which the recorder used to stamp `c.source`, AND
        //   (b) stored `source` into `self.current_source` atomic
        //       which every subsequent AudioChunk then inherits.
        //
        // Both paths turned out to be wrong for source attribution:
        // LDU1 LC FEC is Hamming10 + RS(24,12,13), which accepts
        // near-valid codewords even on bit-corrupt input. In a
        // single 3.5-second speaker turn on 2026-04-19 we observed
        // `FM:` decode to 3599082, 3596970, 12511914, 2392716, and
        // back to 3599082 — only the first and last of which are
        // plausible radio IDs. Stamping from any one of those
        // produced a filename that LIES about who was talking.
        //
        // SDRTrunk uses the control-channel GRP_VCH_GRANT SRC field
        // (TSBK trellis + CRC — strong FEC) as the authoritative
        // source for per-speaker call attribution. The grant
        // follower in this daemon already writes `current_source`
        // from that path (main.rs around line 2644). We keep the
        // activity-log emit above so the operator can see what
        // LDU1 LC *thinks* the source is (useful telemetry when FEC
        // is working), but we stop using it to stamp recordings.
        //
        // End-of-speaker MOT_TC TDULC still fires a SpeakerEnd
        // boundary with `source: Some(by_radio_id)` (Golay24 +
        // RS — stronger FEC), which the recorder uses as a
        // second-opinion source stamp at call finalise.
        let _ = (nac, tg_locked);
    }

    fn on_ldu2(
        &self,
        frames: &[p25::voice_frame::ImbeFrameRaw; 9],
        body_raw: &[u8],
    ) {
        use std::sync::atomic::Ordering;
        self.log_duid("LDU2");
        self.ldu2_count.fetch_add(1, Ordering::Relaxed);
        self.touch_imbe(9);
        self.forward_frames(frames);

        // 2026-04-19: decode LDU2 ESS via Hamming10 + RS(24,16,9) and
        // mirror SDRTrunk's `LDU2 VOICE LSD:... <encryption>` line.
        // Emit for both encrypted and unencrypted cases (SDRTrunk does
        // — `LDU2 VOICE LSD:0000 UNENCRYPTED` appears every LDU2).
        let Some(ess) = p25::voice_frame::parse_ldu2_ess(body_raw)
        else {
            return;
        };
        let (summary, fields) = if ess.is_encrypted() {
            // 2026-04-19 phantom-ENC fix. Two guards apply before we
            // trust this frame:
            //
            //  1. `is_spec_algorithm`: reject algorithm_id bytes not
            //     in TIA-102.AABD + the SDRTrunk Motorola extensions.
            //     RS(24,16,9) will accept near-valid codewords; on a
            //     clear transmission with weak SNR the FEC produces
            //     random bytes (observed 0x08, 0x50, 0xB4 in the
            //     2026-04-19 19:55 log).
            //
            //  2. HDU-trust: if the HDU of this call said UNENCRYPTED
            //     we do NOT let a subsequent LDU2 ESS flip the gate.
            //     The HDU FEC (Golay18 + RS(63,47,17)) is much
            //     stronger than LDU2 ESS FEC, so HDU is authoritative.
            //     Without this guard, a single bit-corrupt LDU2 on a
            //     clear call trips `call_encrypted` sticky-true and
            //     the vocoder drops every subsequent IMBE frame until
            //     the next grant arrives (observed 4,401 frames
            //     `Encrypted Skipped` on TG 301).
            //
            // Legit mid-call key-refresh still works: an encrypted HDU
            // sets the gate, and every LDU2 after it refreshes here.
            if !ess.is_spec_algorithm() {
                return;
            }
            if !self.call_encrypted.load(Ordering::Relaxed) {
                // HDU said clear — silently drop this ESS. Don't
                // emit (log stays clean) and don't trip the gate.
                return;
            }
            // Refresh call_encrypted sticky-true from ESS — grant flag
            // is stale if a mid-call key change happens.
            self.call_encrypted.store(true, Ordering::Relaxed);
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
        self.log_duid("HDU");
        self.hdu_count.fetch_add(1, Ordering::Relaxed);

        // 2026-04-19 earlier: added `current_source.store(0)` here to
        // prevent the next recording from inheriting the prior
        // speaker's FM:<id>. REVERTED 2026-04-19 late. On Clay County
        // the CC grant with a fresh SRC field arrives 1-2 s BEFORE
        // the HDU fires on the traffic chain. Clearing current_source
        // at HDU time wiped that good CC-grant SRC, and since LDU1 LC
        // stamping was removed in the same flash, there was nothing
        // left to re-populate the atomic. Result: recordings opened
        // with source=0 → filename stamped `_from0.wav`.
        //
        // With LDU1 LC stamping gone and the grant follower as the
        // only writer to `current_source`, a stale atomic value from
        // the previous speaker can only persist if:
        //   (a) no fresh CC grant for the active TG arrives between
        //       speakers — but Clay County issues a fresh
        //       `GRP_VCH_GRANT` with a new SRC for every speaker
        //       change, so this is rare in practice, AND
        //   (b) the recorder's HDU-start handler doesn't get to run
        //       before the first chunk of the new speaker arrives.
        // Both conditions would have to hit at once, and the end-of-
        // speaker TDULC MOT_TC boundary (Golay24+RS — strong FEC)
        // still stamps `c.source` at finalise time from its BY:<id>
        // field as a second opinion.

        // 2026-04-19: decode HDU body via Golay18 + RS(63,47,17).
        // SDRTrunk equivalent: `HDU TALKGROUP:<tg> [ENCRYPTION:<alg>
        // KEY:<id> MI:<hex> | UNENCRYPTED]`. Always emit — unlike
        // LDU2_ESS we don't fire every frame; HDU is once per speaker.
        let Some(hdr) = p25::voice_frame::parse_hdu_body(body_raw)
        else {
            return;
        };
        // 2026-04-19 late: gate encryption from the HDU. Every new
        // speaker begins with an HDU; its algorithm_id field is the
        // authoritative per-speaker encryption state. If the HDU says
        // encrypted, route the vocoder into skip mode — earlier we only
        // trusted the grant, which is stale if a subsequent HDU on the
        // same grant re-keys. Sticky-true within the call (matches how
        // the grant path sets it).
        //
        // 2026-04-19 phantom-ENC fix: also require `is_spec_algorithm`
        // so a bit-corrupt HDU FEC decode can't trip the gate with a
        // nonsense algorithm byte. HDU FEC is stronger than LDU2 ESS
        // so this is belt-and-suspenders, but the LDU2 path has the
        // same guard and the two should stay symmetric.
        if hdr.is_encrypted() && hdr.is_spec_algorithm() {
            self.call_encrypted.store(true, Ordering::Relaxed);
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
        self.log_duid("TDU");
        self.tdu_count.fetch_add(1, Ordering::Relaxed);
        // 2026-04-19 late: bare TDU is a real end-of-call signal too
        // (it just lacks the Link Control payload that TDU_LC carries).
        // Previously we didn't route it through SpeakerEnd, which
        // meant calls that ended with bare TDU fell back to the 1.5 s
        // grace window — inflating grace-finalise rate to ~37 % of all
        // finalises on Clay County. Fire SpeakerEnd here so end-of-call
        // recorder splits happen on the protocol signal instead of
        // on the grace timeout. `source: None` — bare TDU carries no
        // speaker ID, so we don't overwrite whatever the LDU1 LC / grant
        // already stamped into the active recording.
        let nac = self.last_observed_nac.load(Ordering::Relaxed);
        let tg = self.current_talkgroup.load(Ordering::Relaxed);
        if let Some(tx) = self.call_boundary_tx.get() {
            let _ = tx.send(audio::CallBoundary {
                kind: audio::CallBoundaryKind::SpeakerEnd { source: None },
                nac,
                talkgroup: if tg == 0 { None } else { Some(tg) },
                expected_submit_count: self
                    .frames_submitted
                    .load(Ordering::Relaxed),
            });
        }
    }

    fn on_tdu_lc(&self, body_raw: &[u8]) {
        use std::sync::atomic::Ordering;
        self.log_duid("TDU_LC");
        self.tdu_lc_count.fetch_add(1, Ordering::Relaxed);

        // 2026-04-19: Motorola TALK_COMPLETE LCW -> boundary event
        // with the BY: source field. Only emit when the parser
        // returned the `MotorolaTalkComplete` variant; `Other` /
        // `GroupVoiceChannelUser` have nothing to contribute (and on
        // non-Motorola sites we'll always land there).
        let Some(tx) = self.call_boundary_tx.get() else { return; };
        let tg = self.current_talkgroup.load(Ordering::Relaxed);
        if tg == 0 {
            return;
        }

        self.tdulc_parse_attempts.fetch_add(1, Ordering::Relaxed);
        let parsed = p25::voice_frame::parse_tdulc_lcw(body_raw);

        // Snapshot the first 9 bytes of the post-extraction LC so a
        // live `/api/traffic` poll shows what the parser is seeing
        // when the Motorola counter won't budge. Only update on the
        // handful of TDULCs that aren't Motorola TALK_COMPLETE (the
        // interesting failure case).
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
                // 2026-04-19 late: end-of-speaker -> SpeakerEnd (was
                // TdulcComplete). The recorder now finalises on this
                // signal, so A -> B turn-taking splits on the actual
                // protocol marker rather than relying on HDU detection.
                let _ = tx.send(audio::CallBoundary {
                    kind: audio::CallBoundaryKind::SpeakerEnd {
                        source: Some(by_radio_id),
                    },
                    nac,
                    expected_submit_count: self
                        .frames_submitted
                        .load(Ordering::Relaxed),
                    talkgroup: Some(tg),
                });
                // 2026-04-19: mirror SDRTrunk's
                // `TDULC MOTOROLA TALK COMPLETE BY:<src>` line into
                // the dashboard activity feed + event log so the
                // decoded end-of-speaker marker is visible.
                let summary = format!(
                    "TDULC MOTOROLA TALK COMPLETE BY:{} TG:{}",
                    by_radio_id, tg,
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
                // FM:0 TO:<TG>` line. These fire many times per call
                // (tail burst), so route them at Imbe category level
                // — the dashboard already collapses duplicates for
                // TRF_TDU_LC events by type.
                let summary = format!(
                    "TDULC GROUP VOICE CHANNEL USER FM:0 TO:{}", lc_tg
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
                self.tdulc_parse_other.fetch_add(1, Ordering::Relaxed);
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
                self.tdulc_parse_other.fetch_add(1, Ordering::Relaxed);
                // 2026-04-19 late: standard CALL_TERMINATION is the
                // protocol end-of-call. Route it through SpeakerEnd so
                // the recorder finalises immediately rather than
                // waiting for the 1500 ms grace window. `by_radio_id`
                // here is the system controller's address, not a real
                // speaker — don't stamp it as source.
                let nac = self.last_observed_nac.load(Ordering::Relaxed);
                let _ = tx.send(audio::CallBoundary {
                    kind: audio::CallBoundaryKind::SpeakerEnd { source: None },
                    nac,
                    talkgroup: Some(tg),
                    expected_submit_count: self
                        .frames_submitted
                        .load(Ordering::Relaxed),
                });
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

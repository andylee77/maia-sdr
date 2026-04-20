//! ImbeForwarder — traffic-chain voice handler.
//!
//! Implements crate::protocol::p25::control_channel::VoiceHandler over the
//! raw LDU/TDU callbacks from the traffic LSM decoder. Owns the IMBE-batch
//! mpsc sender to the vocoder task and the call-boundary broadcast tx that
//! feeds the recorder.

use crate::audio;
use crate::protocol::p25;

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

        match self.imbe_tx.try_send(*frames) {
            Ok(()) => {
                // Only advance when frames actually entered the queue —
                // a dropped send never produces PCM, so advancing would
                // make the recorder wait forever.
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

        // Parse the embedded LDU1 Link Control Word to mirror
        // SDRTrunk's `LDU1 VOICE ... GROUP VOICE CHANNEL USER FM:<src>
        // TO:<TG>` line into the activity feed.
        let tg_locked = self.current_talkgroup.load(Ordering::Relaxed);
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
        // LDU1 LC is NOT used to stamp the recording source. Its FEC
        // (Hamming10 + RS(24,12,13)) accepts near-valid codewords on
        // bit-corrupt input — a single speaker's turn produced 5
        // different `FM:` values in one observed 3.5 s call, only 2
        // plausible. Authoritative source comes from the control-channel
        // GRP_VCH_GRANT SRC field (TSBK trellis + CRC) via the grant
        // follower, with end-of-speaker MOT_TC TDULC (Golay24 + RS) as
        // a second-opinion stamp at finalise. The activity-log emit
        // above is kept purely as telemetry.
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
            if !self.call_encrypted.load(Ordering::Relaxed) {
                // HDU said clear — silently drop (don't trip the gate).
                return;
            }
            // Refresh sticky-true from ESS in case of mid-call rekey.
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
        // Bare TDU is a real end-of-call signal (just without the Link
        // Control payload TDU_LC carries). Route through SpeakerEnd so
        // end-of-call splits happen on the protocol signal rather than
        // the 1.5 s grace timeout (on the test target this pulled
        // grace-finalise rate from ~37 % back into the noise).
        // `source: None` — bare TDU carries no speaker ID.
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

        // Motorola TALK_COMPLETE LCW -> boundary event with BY: source.
        // Non-Motorola sites always land on Other / GroupVoiceChannelUser.
        let Some(tx) = self.call_boundary_tx.get() else { return; };
        let tg = self.current_talkgroup.load(Ordering::Relaxed);
        if tg == 0 {
            return;
        }

        self.tdulc_parse_attempts.fetch_add(1, Ordering::Relaxed);
        let parsed = p25::voice_frame::parse_tdulc_lcw(body_raw);

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
                // End-of-speaker -> SpeakerEnd. Recorder finalises on
                // this protocol marker rather than on HDU detection,
                // so A -> B turn-taking splits cleanly.
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
                // Mirror SDRTrunk's `TDULC MOTOROLA TALK COMPLETE BY:<src>`
                // line into the activity feed + event log.
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
                // FM:0 TO:<TG>`. Fires many times per call (tail burst);
                // dashboard collapses duplicates by type.
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
                // Standard CALL_TERMINATION -> SpeakerEnd so the
                // recorder finalises immediately (no 1500 ms grace).
                // `by_radio_id` is a system-controller address, not a
                // real speaker — don't stamp it as source.
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

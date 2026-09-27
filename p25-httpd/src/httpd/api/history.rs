//! Time-series data: event log, recordings, TSBK history.
//!
//! Consumer orientation: "what happened, in order?" Replayable data
//! with built-in retention limits:
//!
//!   - `/api/log` — event ring (bounded, `?since=<seq>` for
//!     incremental reads). One entry per system event (grant
//!     received, call start/end, retune, error).
//!   - `/api/recordings` + `/api/recordings/{id}` — completed call
//!     recordings. The recorder task owns retention; this API is
//!     read-only.
//!   - `/api/recent_tsbks` — last 50 TSBKs as one-line summaries,
//!     cheap to poll for a scrolling display.
//!   - `/api/tsbk_opcodes` — per-opcode + per-block-position
//!     histogram. Diagnostic, not time-series-per-se, but belongs
//!     with retrospective views.
//!
//! Everything here is process-lifetime only — a daemon restart is
//! a clean slate.

use std::sync::Arc;

use axum::{
    extract::State,
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::httpd::AppState;
#[allow(unused_imports)]
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};

/// `GET /api/recordings[?limit=N]`
///
/// Returns the ring of recent call recordings, newest first. Each
/// entry has {id, talkgroup, started_unix_ms, duration_ms,
/// size_bytes, storage, ...}. Download via `/api/recordings/{id}.wav`
/// or `/api/recordings/{id}`. Change 057: recordings of both stores
/// (RAM and SD card), `storage` per entry; `max` is the live RAM
/// retention (was the fixed default), `max_sd` / `max_sd_bytes` the SD
/// store's; optional `limit` (the SD store can list thousands).
/// Change 065: `DELETE /api/recordings?store=sd|ram|all` — delete every
/// recording of that store (files and list entries). The recording of a
/// call still being written is not in the list yet and is kept.
pub async fn delete_recordings(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let store = match params.get("store").map(String::as_str) {
        Some("sd") => Some(crate::audio::rec_storage::STORE_SD),
        Some("ram") => Some(crate::audio::rec_storage::STORE_RAM),
        Some("all") => None,
        other => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "ok": false,
                    "error": format!("store {other:?}: expected sd, ram or all"),
                })),
            )
                .into_response();
        }
    };
    let deleted = {
        let mut ring = state.recordings.lock().await;
        crate::audio::recorder::clear_recordings(&mut ring, store, &state.rec_storage)
    };
    state.event_log.push(
        crate::services::event_log::LogCategory::Recorder,
        format!("deleted {deleted} recording(s) ({})", store.unwrap_or("all")),
        serde_json::json!({ "store": store.unwrap_or("all"), "deleted": deleted }),
    );
    Json(serde_json::json!({ "ok": true, "deleted": deleted })).into_response()
}

pub async fn get_recordings(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(usize::MAX);
    let ring = state.recordings.lock().await;
    let total = ring.len();
    let items: Vec<_> = ring.iter().rev().take(limit).cloned().collect();
    drop(ring);
    let r = state.ui_settings.recording.retention();
    Json(serde_json::json!({
        "count": items.len(),
        "total": total,
        "max": r.ram_max_count,
        "max_sd": r.sd_max_count,
        "max_sd_bytes": r.sd_max_bytes,
        "storage": state.ui_settings.recording.storage().as_str(),
        "items": items,
    }))
}


/// `GET /api/grant_decode_stats[?include_enc=1]`
///
/// Ring of recent completed grants with per-call decode deltas:
/// IMBE extracted / dropped, vocoder samples / errors / silent,
/// duration, first_imbe_ms, encryption, source, etc. Populated by
/// the `grant_stats` subscriber on the CallBoundary broadcast.
/// Newest-first.
///
/// 2026-04-29: split into two rings — clear (followed, audio-
/// bearing) and encrypted/not_followed. Default returns clear
/// only so heavy ENC site activity doesn't crowd out clear-call
/// entries that recordings need to pair against by call_id.
/// `?include_enc=1` merges the encrypted ring on top.
///
/// Designed to attribute audio-quality issues per-call: short vs
/// long, first-LDU latency, drop-count distribution, silent-frame
/// rate — without cross-referencing global counters.
pub async fn get_grant_decode_stats(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let include_enc = params.get("include_enc")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);
    let mut items: Vec<_> = {
        match state.grant_decode_stats.lock() {
            Ok(r) => r.iter().rev().cloned().collect(),
            Err(_) => Vec::new(),
        }
    };
    let enc_count = state.enc_grant_decode_stats.lock()
        .map(|r| r.len()).unwrap_or(0);
    if include_enc {
        let enc_items: Vec<_> = state.enc_grant_decode_stats.lock()
            .map(|r| r.iter().rev().cloned().collect())
            .unwrap_or_default();
        items.extend(enc_items);
        // Sort merged list newest-first by started_unix_ms.
        items.sort_by_key(|x| std::cmp::Reverse(x.started_unix_ms));
    }
    Json(serde_json::json!({
        "count": items.len(),
        "items": items,
        "include_enc": include_enc,
        "enc_ring_size": enc_count,
    }))
}


/// `GET /api/recordings/{id}/events`
///
/// Returns the per-recording event timeline: every `recorder`-category
/// log entry whose `fields.recording_id` matches the requested id.
/// This is the SDRTrunk `decoded_messages.log` equivalent for this one
/// recording — every boundary event, source stamp, finalise reason,
/// discard reason in wall-clock order. Mount with the recording
/// itself to trace "why does this file exist / why was it split /
/// why does the filename have this source?"
pub async fn get_recording_events(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Json<serde_json::Value> {
    // Pull the whole ring (recorder entries are rare vs DUIDs) and
    // filter by recording_id. 2026-04-19 fix: was capped at 10 000,
    // which missed events for recordings whose entries were near the
    // top of a full 16 384-entry ring.
    let all = state.event_log.recent_since(0, usize::MAX);
    let items: Vec<_> = all.into_iter()
        .filter(|e| {
            e.category == "recorder"
                && e.fields
                    .get("recording_id")
                    .and_then(|v| v.as_u64())
                    == Some(id)
        })
        .collect();
    Json(serde_json::json!({
        "recording_id": id,
        "count": items.len(),
        "items": items,
    }))
}

/// `GET /api/recordings/{id}/sync_trace`
///
/// Per-NID PLL / AGC / sync register samples captured during the
/// recording's call. Each sample is one traffic-LSM `nid_event`
/// while `current_call_id == id`. nid_valid=false samples are
/// included on purpose — they're the diagnostic gold for "framer
/// locking onto jittery symbols mid-call." See
/// `project_post_pacer_next_steps.md`.
///
/// Response shape:
/// ```json
/// {
///   "recording_id": <u64>,
///   "count":        <usize>,
///   "samples":      [SyncTraceSample, ...]
/// }
/// ```
pub async fn get_recording_sync_trace(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<u64>,
) -> Json<serde_json::Value> {
    let samples: Vec<_> = match state.sync_trace_ring.lock() {
        Ok(r) => r.iter().filter(|s| s.call_id == id).cloned().collect(),
        Err(_) => Vec::new(),
    };
    Json(serde_json::json!({
        "recording_id": id,
        "count":        samples.len(),
        "samples":      samples,
    }))
}

/// `GET /api/recordings/{id}`
///
/// Streams the WAV file for a recording by id. Trailing `.wav` in
/// the path is tolerated (strip it). Returns 404 if the id isn't in
/// the current ring (evicted or never existed).
pub async fn get_recording_file(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    axum::extract::Path(id_str): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    // Strip optional .wav suffix so both `/api/recordings/123` and
    // `/api/recordings/123.wav` work.
    let id_clean = id_str.trim_end_matches(".wav");
    let id: u64 = match id_clean.parse() {
        Ok(v) => v,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("bad id '{id_str}'"),
            )
                .into_response();
        }
    };

    let found = {
        let ring = state.recordings.lock().await;
        ring.iter()
            .find(|e| e.id == id)
            .map(|e| (e.path.clone(), e.pending.as_ref().map(|p| p.0.clone())))
    };
    let Some((path, pending)) = found else {
        return (StatusCode::NOT_FOUND, "recording not found").into_response();
    };

    // Change 057: a recording still waiting for the SD writer is served
    // from its RAM copy. Otherwise read the file (tokio::fs runs on the
    // blocking pool, so a slow SD card delays only this request).
    // WAVs are at most a few MB. Avoid axum's Body::from_stream
    // machinery.
    let bytes = match pending {
        Some(b) => b.as_ref().clone(),
        None => match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("file read failed: {e}"),
                )
                    .into_response();
            }
        },
    };
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("recording.wav")
        .to_string();
    let total_len = bytes.len() as u64;

    // Phase 10-prep: HTTP Range support. Without this, the HTML5
    // <audio> element in the dashboard stutters or restarts mid-
    // playback -- it issues `Range: bytes=0-` probes to test for
    // seek capability, gets 200 OK with the full body, and re-
    // interprets the re-send as a stream restart. Implementing
    // minimal single-range support (206 Partial Content) makes
    // <audio> happy. Download (`<a href download>`) still works
    // because the download path doesn't issue Range requests.
    //
    // We intentionally parse ONLY `bytes=<start>-<end?>` (single
    // range, no multipart) since that covers every browser we care
    // about. Malformed ranges fall back to 200 OK with the full body.
    let range_hdr = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("bytes="))
        .and_then(|s| s.split_once('-'))
        .and_then(|(start, end)| {
            let start: u64 = start.parse().ok()?;
            let end: u64 = if end.is_empty() {
                total_len.saturating_sub(1)
            } else {
                end.parse().ok()?
            };
            if start > end || start >= total_len {
                return None;
            }
            let end = end.min(total_len - 1);
            Some((start, end))
        });

    let (status, start, end) = match range_hdr {
        Some((s, e)) => (StatusCode::PARTIAL_CONTENT, s, e),
        None => (StatusCode::OK, 0u64, total_len - 1),
    };

    let body: Vec<u8> = if status == StatusCode::PARTIAL_CONTENT {
        bytes[start as usize..=end as usize].to_vec()
    } else {
        bytes
    };
    let content_length = body.len() as u64;

    let mut builder = axum::response::Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "audio/wav")
        .header(
            header::CONTENT_DISPOSITION,
            format!("inline; filename=\"{filename}\""),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length);

    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{total_len}"),
        );
    }

    match builder.body(axum::body::Body::from(body)) {
        Ok(resp) => resp,
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("response build failed: {e}"),
        )
            .into_response(),
    }
}


/// Phase 6F.4: per-opcode histogram of CRC-OK and CRC-FAIL TSBK
/// blocks decoded by the LSM software decoder. Lets the dashboard
/// see the on-air opcode distribution and pinpoint missing parsers.
pub async fn get_tsbk_opcodes(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;

    // Map opcode index → SDRTrunk-style label so the dashboard
    // doesn't have to mirror the table. Covers all opcodes that
    // appear in `Opcode.java` for the OSP direction (control-channel
    // outbound). Lowercase here means we don't recognise it as a P25
    // opcode at all (probably trellis-decode garbage).
    // Labels sourced from SDRTrunk `Opcode.java` (the `OSP_*` outbound
    // table). 2026-04-19 fix: previous table had 0x27 / 0x28 / 0x2A /
    // 0x2B / 0x2C mis-labeled against their SDRTrunk equivalents,
    // which is what let the on-target histogram claim things like
    // "NET_STS_BCST_EXP" for a frame that SDRTrunk's own log called
    // `GRP_AFFIL_QUERY`.
    fn label(op: u8) -> &'static str {
        match op {
            0x00 => "GRP_V_CH_GRANT",
            0x02 => "GRP_V_CH_GRANT_UPDT",
            0x03 => "GRP_V_CH_GRANT_UPDT_EXP",
            0x04 => "UU_V_CH_GRANT",
            0x05 => "UU_ANS_REQ",
            0x06 => "UU_V_CH_GRANT_UPDT",
            0x08 => "TELE_INT_V_CH_GRANT",
            0x09 => "TELE_INT_V_CH_GRANT_UPDT",
            0x0A => "TELE_INT_ANS_REQ",
            0x0B => "RAD_MON_CMD",
            0x14 => "SNDCP_DCH_GRANT",
            0x15 => "SNDCP_DCH_PAG_RQ",
            0x16 => "SNDCP_DCH_ANN_EX",
            0x18 => "STS_UPDT",
            0x1A => "STS_Q",
            0x1C => "MSG_UPDT",
            0x1D => "RAD_MON_ENH_CMD",
            0x1F => "CALL_ALERT",
            0x20 => "ACK_RESP_FNE",
            0x21 => "QUE_RSP",
            0x24 => "EXT_FNCT_CMD",
            0x27 => "DENY_RSP",
            0x28 => "GRP_AFF_RSP",
            0x29 => "SCCB_EXP",
            0x2A => "GRP_AFF_Q",
            0x2B => "LOC_RG_RSP",
            0x2C => "U_REG_RSP",
            0x2D => "U_REG_CMD",
            0x2F => "U_DE_REG_ACK",
            0x30 => "TDMA_SYNC_BCST",
            0x31 => "AUTH_DMD",
            0x32 => "AUTH_FNE_RESP",
            0x33 => "IDEN_UPDATE_TDMA",
            0x34 => "IDEN_UPDATE_VUHF",
            0x36 => "TIME_DATE_ANN",
            0x37 => "ROAM_ADDR_CMD",
            0x38 => "SYS_SRV_BCST",
            0x39 => "SEC_CCH_BROADCST",
            0x3A => "RFSS_STATUS_BCST",
            0x3B => "NET_STATUS_BCAST",
            0x3C => "ADJ_STS_BCAST",
            0x3D => "IDEN_UPDATE",
            0x3E => "PROT_PARAM_BCST",
            0x3F => "PROT_PARAM_UPDT",
            _ => "(unknown)",
        }
    }

    let mut entries = Vec::with_capacity(64);
    let mut total_ok = 0u64;
    let mut total_fail = 0u64;
    for op in 0u8..64 {
        let ok = dec.tsbk_opcode_hist_ok[op as usize];
        let fail = dec.tsbk_opcode_hist_fail[op as usize];
        total_ok += ok;
        total_fail += fail;
        if ok > 0 || fail > 0 {
            let parsed = matches!(
                op,
                // 6F.4 + 6F.5: voice grants, IDEN_UPDATE variants,
                // RFSS / NET / ADJ status broadcasts.
                0x00 | 0x02 | 0x33 | 0x34 | 0x3A | 0x3B | 0x3C | 0x3D
                // 6F.11: 5 new parsers added in this phase.
                | 0x05 | 0x09 | 0x16 | 0x30 | 0x39
                // 2026-04-19: registration/affiliation + SNDCP data +
                // radio-monitor + FNE ack. See tsbk.rs decode().
                | 0x03 | 0x0B | 0x14 | 0x15 | 0x20
                | 0x28 | 0x2A | 0x2B | 0x2C | 0x2F
            );
            entries.push(serde_json::json!({
                "opcode": format!("0x{:02X}", op),
                "label": label(op),
                "ok": ok,
                "fail": fail,
                "parsed": parsed,
            }));
        }
    }
    // Sort by ok-count descending so the most common live opcodes
    // float to the top of the list.
    entries.sort_by(|a, b| {
        let a_ok = a["ok"].as_u64().unwrap_or(0);
        let b_ok = b["ok"].as_u64().unwrap_or(0);
        b_ok.cmp(&a_ok)
    });

    // Per-block-position rates (TSBK1 / TSBK2 / TSBK3 attempts and
    // CRC successes). If TSBK2 / TSBK3 success rates are massively
    // worse than TSBK1, the multi-block continuation alignment is
    // wrong somewhere upstream.
    let attempts_pos = dec.tsbk_block_attempts_by_pos;
    let crc_ok_pos = dec.tsbk_crc_ok_by_pos;
    let pos_pct = |a: u64, ok: u64| -> f64 {
        if a == 0 { 0.0 } else { 100.0 * ok as f64 / a as f64 }
    };

    Json(serde_json::json!({
        "tsdu_attempts": dec.tsdu_attempts,
        "tsbk_block_attempts_total": dec.tsbk_block_attempts,
        "blocks_per_tsdu": if dec.tsdu_attempts == 0 { 0.0 }
            else { dec.tsbk_block_attempts as f64 / dec.tsdu_attempts as f64 },
        "crc_ok_total": total_ok,
        "crc_fail_total": total_fail,
        "crc_ok_pct": if (total_ok + total_fail) == 0 { 0.0 }
            else { 100.0 * total_ok as f64 / (total_ok + total_fail) as f64 },
        "by_position": {
            "tsbk1": {
                "attempts": attempts_pos[0],
                "crc_ok": crc_ok_pos[0],
                "crc_ok_pct": pos_pct(attempts_pos[0], crc_ok_pos[0]),
            },
            "tsbk2": {
                "attempts": attempts_pos[1],
                "crc_ok": crc_ok_pos[1],
                "crc_ok_pct": pos_pct(attempts_pos[1], crc_ok_pos[1]),
            },
            "tsbk3": {
                "attempts": attempts_pos[2],
                "crc_ok": crc_ok_pos[2],
                "crc_ok_pct": pos_pct(attempts_pos[2], crc_ok_pos[2]),
            },
        },
        "mfid_breakdown": {
            "standard_0x00": dec.tsbk_mfid_hist_ok[0],
            "motorola_0x90": dec.tsbk_mfid_hist_ok[1],
            "harris_0xA4":   dec.tsbk_mfid_hist_ok[2],
            "other":         dec.tsbk_mfid_hist_ok[3],
        },
        "opcodes": entries,
    }))
}


/// Phase 6F.4: dump the most recent TSBK messages with their
/// originating block index (TSBK1/2/3), so the dashboard can show a
/// live activity feed in the same format as SDRTrunk's
/// decoded_messages.log.
pub async fn get_recent_tsbks(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec = state.lsm_decoder.read().await;
    let now = std::time::Instant::now();

    let summarize = |msg: &crate::protocol::p25::tsbk::TsbkMessage| -> String {
        use crate::protocol::p25::tsbk::TsbkMessage::*;
        match msg {
            NetworkStatus { wacn, system_id, channel } => format!(
                "NET_STATUS_BCAST WACN:{:05X} SYS:{:03X} CH:{}",
                wacn, system_id, channel
            ),
            RfssStatus { lra, rfss_id, site_id, channel } => format!(
                "RFSS_STATUS_BCST LRA:{} RFSS:{} SITE:{} CH:{}",
                lra, rfss_id, site_id, channel
            ),
            AdjacentStatus { lra, rfss_id, site_id, channel, system_id } => format!(
                "ADJ_STS_BCAST LRA:{} SYS:{:03X} RFSS:{} SITE:{} CH:{}",
                lra, system_id, rfss_id, site_id, channel
            ),
            IdentifierUpdate {
                identifier,
                bw,
                transmit_offset,
                channel_spacing,
                base_frequency,
            } => format!(
                "IDEN_UPDATE ID:{} OFFSET:{} SPACING:{} BASE:{} BW:{}",
                identifier, transmit_offset, channel_spacing, base_frequency, bw
            ),
            GroupVoiceChannelGrant { channel, talkgroup, source, service_options } => format!(
                "GRP_V_CH_GRANT CH:{} TG:{} SRC:{}{}",
                channel, talkgroup, source,
                if crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                }
            ),
            GroupVoiceChannelGrantUpdate {
                channel_a, talkgroup_a, channel_b, talkgroup_b,
            } => format!(
                "GRP_V_CH_GRANT_UPDT CH_A:{} TG_A:{} CH_B:{} TG_B:{}",
                channel_a, talkgroup_a, channel_b, talkgroup_b
            ),
            GroupVoiceChannelGrantUpdateExplicit {
                transmit_channel, receive_channel, talkgroup, service_options,
            } => format!(
                "GRP_V_CH_GRANT_UPDT_EXP TX:{} RX:{} TG:{}{}",
                transmit_channel, receive_channel, talkgroup,
                if crate::protocol::p25::tsbk::service_options::is_encrypted(*service_options) {
                    " [ENC]"
                } else {
                    ""
                }
            ),
            // Phase 6F.11 new opcodes
            SecondaryControlChannelBroadcast {
                rfss_id, site_id, channel_a, channel_b,
            } => format!(
                "SEC_CCH_BROADCST RFSS:{} SITE:{} A:{} B:{}",
                rfss_id, site_id, channel_a, channel_b
            ),
            SndcpDataChannelAnnouncementExplicit {
                downlink_channel, uplink_channel, autonomous_access,
                requested_access, ..
            } => format!(
                "SNDCP_DCH_ANN_EX DL:{} UL:{} {}{}",
                downlink_channel, uplink_channel,
                if *autonomous_access { "AUTO " } else { "" },
                if *requested_access { "REQ" } else { "" },
            ),
            TdmaSyncBroadcast {
                year, month, day, hours, minutes, time_locked, ..
            } => format!(
                "TDMA_SYNC_BCST {:04}-{:02}-{:02} {:02}:{:02} {}",
                year, month, day, hours, minutes,
                if *time_locked { "LOCKED" } else { "UNLOCKED" }
            ),
            TelephoneInterconnectVoiceChannelGrantUpdate {
                channel, call_timer_secs, unit_id,
            } => format!(
                "TEL_INT_VCH_GRNT_UPDT UNIT:{} CH:{} timer:{}s",
                unit_id, channel, call_timer_secs
            ),
            UnitToUnitAnswerRequest { target, source } => format!(
                "UU_ANS_REQ TGT:{} SRC:{}", target, source
            ),
            // 2026-04-19 new parsers:
            RadioUnitMonitorCommand { source, target } => format!(
                "RAD_MON_CMD SRC:{} TGT:{}", source, target
            ),
            SndcpDataChannelGrant {
                service_options, downlink_channel, uplink_channel, target,
            } => format!(
                "SNDCP_DCH_GRANT DL:{} UL:{} TGT:{} OPTS:0x{:02X}",
                downlink_channel, uplink_channel, target, service_options
            ),
            SndcpDataPageRequest {
                service_options, target, source,
            } => format!(
                "SNDCP_DCH_PAG_RQ TGT:{} SRC:{} OPTS:0x{:02X}",
                target, source, service_options
            ),
            AcknowledgeResponseFne {
                service_type, additional_info, extended_info, source, target,
            } => format!(
                "ACK_RESP_FNE SVC:0x{:02X}{}{} SRC:{} TGT:{}",
                service_type,
                if *additional_info { " AI" } else { "" },
                if *extended_info { " EI" } else { "" },
                source, target
            ),
            GroupAffiliationResponse {
                response, announcement_group, group, target,
            } => format!(
                "GRP_AFF_RSP RSP:{} TG:{} ANN_TG:{} TGT:{}",
                response, group, announcement_group, target
            ),
            GroupAffiliationQuery { target, source } => format!(
                "GRP_AFF_Q TGT:{} SRC:{}", target, source
            ),
            LocationRegistrationResponse {
                response, group, rfss_id, site_id, target,
            } => format!(
                "LOC_RG_RSP RSP:{} TG:{} RFSS:{} SITE:{} TGT:{}",
                response, group, rfss_id, site_id, target
            ),
            UnitRegistrationResponse {
                response, system_id, source_id, source_address,
            } => format!(
                "U_REG_RSP RSP:{} SYS:{:03X} SRC_ID:{} SRC_ADDR:{}",
                response, system_id, source_id, source_address
            ),
            UnitDeRegistrationAcknowledge { wacn, system_id, target } => format!(
                "U_DE_REG_ACK WACN:{:05X} SYS:{:03X} TGT:{}",
                wacn, system_id, target
            ),
            ManufacturerSpecific { mfid, opcode, .. } => {
                let vendor = match mfid {
                    0x90 => "MOT",
                    0xA4 => "HAR",
                    0x68 => "DVSI",
                    _ => "VEN",
                };
                format!(
                    "{} MFID:0x{:02X} OP:0x{:02X}",
                    vendor, mfid, opcode
                )
            }
        }
    };

    // Iterate newest-first.
    let entries: Vec<serde_json::Value> = dec
        .recent_messages
        .iter()
        .rev()
        .take(50)
        .map(|(t, block_idx, msg)| {
            let block_label = match block_idx {
                0 => "TSBK1",
                1 => "TSBK2",
                2 => "TSBK3",
                _ => "TSBK?",
            };
            serde_json::json!({
                "age_secs": now.duration_since(*t).as_secs_f64(),
                "block": block_label,
                "summary": summarize(msg),
            })
        })
        .collect();

    Json(serde_json::json!({
        "count": entries.len(),
        "messages": entries,
    }))
}


/// GET /api/log -- tail the structured event log.
///   ?since=N   -- return entries with seq > N (default 0 = all)
///   ?limit=N   -- return at most N entries (default 200, cap 16384)
///   ?category=grant|traffic|voice|vocoder|system|recorder|duid
///              -- server-side filter (optional; dashboard also
///              filters client-side so the ring is one source of truth)
///   ?tail=1    -- change 056: the NEWEST `limit` matches (still
///              oldest-first in the response). Without it the OLDEST
///              `limit` entries after `since` are returned, which on a
///              first read are boot-time entries.
///   ?from_ms=T&to_ms=T -- change 056: wall-clock window on
///              `timestamp_ms` (inclusive), e.g. one call's events.
///   ?tsbk=0    -- change 056: drop the control-channel TSBK mirror
///              lines (`grant` entries with `fields.event_type`, one
///              per decoded TSBK; most of the ring on a busy site).
///
/// Response shape:
/// ```json
/// {
///   "last_seq": 12345,
///   "count":    42,
///   "entries":  [ { "seq": ..., "timestamp_ms": ..., "category": ...,
///                    "message": ..., "fields": { ... } }, ... ]
/// }
/// ```
pub async fn get_event_log(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    let since = params
        .get("since")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    // Cap aligned with the event-log ring size (16 384) so callers
    // can pull the full history if needed. Was .min(1000); raised
    // 2026-04-19 after adding the high-volume Duid category where
    // 1000 entries cover only ~30-60 s of decode activity and drop
    // sparse Recorder / Grant entries below the filter window.
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(200)
        .min(16384);
    let category_filter = params.get("category").map(|s| s.to_string());
    let flag = |k: &str| {
        params.get(k).map(|v| v == "1" || v == "true")
    };
    let num = |k: &str| params.get(k).and_then(|v| v.parse::<u64>().ok());

    // Filters (category, time window, TSBK lines) apply before
    // `limit` — otherwise a sparse category like `recorder` or `grant`
    // can return empty because the newest entries are all another
    // category. Change 056 moved this into `EventLog::query`.
    let q = crate::services::event_log::LogQuery {
        since,
        limit,
        tail: flag("tail").unwrap_or(false),
        category: category_filter,
        from_ms: num("from_ms"),
        to_ms: num("to_ms"),
        include_tsbk: flag("tsbk").unwrap_or(true),
    };
    let entries = state.event_log.query(&q);
    let last_seq = state.event_log.last_seq();
    Json(serde_json::json!({
        "last_seq": last_seq,
        "count":    entries.len(),
        "entries":  entries,
    }))
}

// ── Phase 7F.4 (2026-04-14): NID batch capture + runtime BCH-t ──────



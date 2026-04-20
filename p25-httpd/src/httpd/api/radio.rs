//! Live radio state: stats, grants, bands, decoder chains.
//!
//! Consumer orientation: "show me the radio right now." Every
//! endpoint here is read-only and reflects the current on-air
//! decoder state. This is the data plane that drives the dashboard's
//! main screen and would drive an Android app's home screen.
//!
//! Reads from whichever control-chain decoder `AppState::active_modulation`
//! has picked (C4FM vs LSM vs Auto); all endpoints route through
//! `state.active_control_decoder()` so switching modulation is
//! transparent to callers. `/api/decoder_compare` is the exception —
//! it reads all three decoders at once (PS C4FM, PS LSM, PL HDL)
//! for side-by-side comparison during bring-up.

use std::sync::Arc;

use axum::{
    extract::State,
    Json,
};

#[allow(unused_imports)]
use p25_json::*;

#[allow(unused_imports)]
use crate::httpd::{AppState, ts_to_ymd_hms};
#[allow(unused_imports)]
use crate::protocol::p25::control_channel::{
    ControlChannelDecoder, RUNTIME_SYNC_THRESHOLD, CC_SYNC_THRESHOLD,
};

pub async fn get_grants(State(state): State<Arc<AppState>>) -> Json<Vec<ChannelGrant>> {
    // Phase 9 retirement (2026-04-15): used to union grants from
    // `lsm_decoder` + `iq_lsm_decoder`. The software pipeline is
    // gone so this is now a single-decoder read. As a side effect,
    // the stale-age bug on the Active Grants panel (iq_lsm_decoder
    // had no expire_grants loop; its grants sat forever) is fixed.
    //
    // Phase 7F.4 (2026-04-14): cross-reference each grant against
    // the persistent `encrypted_tg_history` HashSet. Grants whose
    // current TSBK service options lack the encrypted bit but whose
    // TG has ever been seen encrypted get `in_encrypted_history=true`
    // so the dashboard can badge them even when the latest
    // transmission forgot to set the flag.
    let encrypted_history: std::collections::HashSet<u16> = state
        .imbe_forwarder
        .encrypted_tg_history
        .lock()
        .map(|h| h.clone())
        .unwrap_or_default();

    // 2026-04-16: read from whichever decoder the modulation selector
    // picks (LSM for Clay/Duval, C4FM for FP&L/St Johns).
    let dec = state.active_control_decoder().read().await;
    let mut by_channel: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    for g in dec.grants.values() {
        let in_history = encrypted_history.contains(&g.talkgroup.0);
        let cg = ChannelGrant {
            channel: format!("{}", g.channel),
            talkgroup: g.talkgroup.0,
            talkgroup_alias: dec.aliases.get(&g.talkgroup.0).cloned(),
            source: g.source.map(|s| s.0),
            frequency_mhz: g.frequency_hz.map(|f| f as f64 / 1_000_000.0),
            age_secs: g.timestamp.elapsed().as_secs(),
            encrypted: g.encrypted,
            emergency: g.emergency,
            in_encrypted_history: in_history,
        };
        by_channel.insert(g.channel.0, cg);
    }

    // Second pass: collapse by talkgroup, picking the youngest
    // surviving channel entry per TG. TG 0 is excluded from the
    // dedup (matches the decoder-side wildcard sentinel) so we
    // never collapse multiple unrelated "no-talkgroup" entries.
    let mut by_talkgroup: std::collections::HashMap<u16, ChannelGrant> =
        std::collections::HashMap::new();
    let mut tg0_passthrough: Vec<ChannelGrant> = Vec::new();
    for cg in by_channel.into_values() {
        if cg.talkgroup == 0 {
            tg0_passthrough.push(cg);
            continue;
        }
        match by_talkgroup.get(&cg.talkgroup) {
            Some(existing) if existing.age_secs <= cg.age_secs => {}
            _ => {
                by_talkgroup.insert(cg.talkgroup, cg);
            }
        }
    }
    let mut grants: Vec<ChannelGrant> = by_talkgroup.into_values().collect();
    grants.extend(tg0_passthrough);
    grants.sort_by_key(|g| g.age_secs);
    Json(grants)
}


pub async fn get_bands(State(state): State<Arc<AppState>>) -> Json<Vec<BandInfo>> {
    // Phase 9 retirement: single-decoder read (was unioning
    // `lsm_decoder` with the retired Phase 6D `iq_lsm_decoder`).
    // 2026-04-16: now respects active_modulation.
    let dec = state.active_control_decoder().read().await;
    let mut bands: Vec<BandInfo> = dec
        .bands
        .values()
        .map(|b| BandInfo {
            identifier: b.identifier,
            base_frequency_mhz: b.base_frequency_hz as f64 / 1_000_000.0,
            channel_spacing_khz: b.channel_spacing_hz as f64 / 1_000.0,
            transmit_offset_mhz: b.transmit_offset_hz as f64 / 1_000_000.0,
            bandwidth_khz: b.bandwidth_hz as f64 / 1_000.0,
        })
        .collect();
    bands.sort_by_key(|b| b.identifier);
    Json(bands)
}


pub async fn get_stats(State(state): State<Arc<AppState>>) -> Json<DecoderStats> {
    // 2026-04-16: stats read from the active control-chain decoder.
    let decoder = state.active_control_decoder().read().await;

    #[cfg(target_os = "linux")]
    let (dibit_count, overflow, dma_next_address) = {
        let core = state.ip_core.lock().await;
        (
            core.dibit_count() as u32,
            core.demod_overflow(),
            core.dibit_next_address(),
        )
    };
    #[cfg(not(target_os = "linux"))]
    let (dibit_count, overflow, dma_next_address) = (0u32, false, 0u32);

    // AD9361 health: AGC gain (high = AGC searching for weak signal) and
    // RSSI (relative dB scale; for this band, ~100-110 dB is normal P25
    // reception, lower = quieter). Surfacing these via /api/stats so we
    // never have to ssh in and devmem just to find out the radio is alive.
    //
    // Board Info extension: rf_bandwidth, sampling_frequency, gain mode,
    // and the live RX LO are read the same way so the dashboard's
    // "Board Info" panel has one endpoint to poll for everything.
    #[cfg(target_os = "linux")]
    let (
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
    ) = {
        let g = state.ad9361.get_rx_gain().await.ok();
        let r = state.ad9361.get_rx_rssi().await.ok();
        let lo = state.ad9361.get_rx_lo_frequency().await.ok();
        let bw = state.ad9361.get_rx_rf_bandwidth().await.ok();
        let sr = state.ad9361.get_sampling_frequency().await.ok();
        let gm = state
            .ad9361
            .get_rx_gain_mode()
            .await
            .ok()
            .map(|m| m.to_string());
        (g, r, lo, bw, sr, gm)
    };
    #[cfg(not(target_os = "linux"))]
    let (
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
    ): (
        Option<f64>,
        Option<f64>,
        Option<u64>,
        Option<u32>,
        Option<u32>,
        Option<String>,
    ) = (None, None, None, None, None, None);

    // DDC geometry: the control-side DDC NCO sits at a fixed offset
    // from the LO (plus a small crystal-ppm correction). Report that
    // offset so the operator can see "which DDC frequency is the
    // control channel" without re-deriving it from /api/reinit.
    let ddc_control_offset_hz: Option<i64> = rx_lo_hz.map(|lo| {
        let nco_lo_shift_hz = -state.boot_lo_ppm * 1e-6 * lo as f64;
        (state.boot_control_freq as f64 - lo as f64 + nco_lo_shift_hz) as i64
    });
    // Decimation chain is a compile-time constant of the HDL build.
    // Phase 10-prep redesign: /4 /4 /8 Parks-McClellan split.
    // 8 MSPS ADC / 128 = 62.5 kSPS into the demod.
    let ddc_decimation = Some("/4 /4 /8 = /128".to_string());
    let ddc_output_rate_hz = sampling_frequency_hz.map(|sr| sr / 128);

    // Wall clock: Linux clock value. Pre-NTP this will read 1970-...;
    // post-NTP it's real. We format it here so the browser doesn't
    // have to parse a raw u64 seconds-since-epoch.
    let wall_clock = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|d| {
                let secs = d.as_secs();
                // Minimal ISO-ish formatter without pulling chrono in.
                let (year, month, day, h, m, s) = ts_to_ymd_hms(secs);
                format!(
                    "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                    year, month, day, h, m, s
                )
            })
    };
    let uptime_secs = Some(state.boot_instant.elapsed().as_secs());

    let audio_ws_lag_total = Some(
        state
            .audio_ws_lag_total
            .load(std::sync::atomic::Ordering::Relaxed),
    );
    let audio_ws_clients = Some(state.audio_tx.receiver_count());

    Json(DecoderStats {
        recent_messages: decoder.recent_messages.len(),
        active_grants: decoder.grants.len(),
        bands_known: decoder.bands.len(),
        system_acquired: decoder.system.wacn.is_some(),
        dibit_count,
        overflow,
        dma_next_address,
        rx_gain_db,
        rx_rssi_db,
        rx_lo_hz,
        rf_bandwidth_hz,
        sampling_frequency_hz,
        gain_control_mode,
        ddc_control_offset_hz,
        ddc_decimation,
        ddc_output_rate_hz,
        wall_clock,
        uptime_secs,
        audio_ws_lag_total,
        audio_ws_clients,
    })
}


/// Phase 6F.2: PL HDL LSM chain runtime snapshot.
///
/// Reads the shared `HdlLsmRuntime` populated by the heartbeat task.
/// Includes: live register snapshot, cumulative NID counts, NAC
/// histogram, and the last 32 NID events from the ring buffer.
pub async fn get_hdl_lsm(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let rt = state.hdl_lsm.lock().await;
    let now = Instant::now();
    let uptime_secs = rt
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_tick_ms_ago = rt
        .last_tick_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let last_nid_ms_ago = rt
        .last_nid_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);

    let total_nac_hits: u64 = rt.nac_hist.values().sum();
    let top_nacs: Vec<serde_json::Value> = rt
        .top_nacs(10)
        .into_iter()
        .map(|(nac, count)| {
            let pct = if total_nac_hits == 0 {
                0.0
            } else {
                100.0 * (count as f64) / (total_nac_hits as f64)
            };
            serde_json::json!({
                "nac":   format!("0x{:03X}", nac),
                "count": count,
                "pct":   pct,
            })
        })
        .collect();

    Json(serde_json::json!({
        "running":              rt.started_at.is_some(),
        "uptime_secs":          uptime_secs,
        "last_tick_ms_ago":     last_tick_ms_ago,
        "last_nid_ms_ago":      last_nid_ms_ago,

        "live": {
            "pll_dbg":               rt.pll_dbg,
            "sp_dbg":                rt.sp_dbg,
            "sync_distance":         rt.sync_distance,
            "bch_busy":              rt.bch_busy,
            "in_nid_window":         rt.in_nid_window,
            "dibit_overflow_latch":  rt.dibit_overflow_latched,
            "iq_overflow_latch":     rt.iq_overflow_latched,
            "last_nac":              format!("0x{:03X}", rt.last_nac),
            "last_duid":             rt.last_duid,
            "last_drop_count":       rt.last_drop_count,
            "last_nid_valid":        rt.last_nid_valid,
            "last_nid_n_errors":     rt.last_nid_n_errors,
        },

        "cumulative": {
            "total_nid_events":      rt.total_nid_events,
            "valid_nid_events":      rt.valid_nid_events,
            "valid_pct":             if rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (rt.valid_nid_events as f64) / (rt.total_nid_events as f64)
            },
            "dibit_overflow_ticks":  rt.dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.iq_overflow_ticks,
        },

        "last_window": {
            "pll_min":               rt.hb_pll_min,
            "pll_max":               rt.hb_pll_max,
            "sp_min":                rt.hb_sp_min,
            "sp_max":                rt.hb_sp_max,
            "sync_dist_best":        rt.hb_sync_dist_best,
            "bch_busy_ticks":        rt.hb_bch_busy_ticks,
            "in_window_ticks":       rt.hb_in_window_ticks,
            "nid_event_ticks":       rt.hb_nid_event_ticks,
            "dibit_overflow_ticks":  rt.hb_dibit_overflow_ticks,
            "iq_overflow_ticks":     rt.hb_iq_overflow_ticks,
            "iq_kbps":               rt.hb_iq_kbps,
            "iq_buf_rolls":          rt.hb_iq_buf_rolls,
            "valid_count":           rt.hb_window_valid_count,
            "event_count":           rt.hb_window_event_count,
        },

        "top_nacs":  top_nacs,
        "nid_ring":  rt.nid_ring.clone(),
    }))
}


/// Phase 6F.2: per-source IRQ counters from the InterruptHandler task.
pub async fn get_irq_stats(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    use std::time::Instant;
    let s = state.irq_stats.lock().await;
    let now = Instant::now();
    let uptime_secs = s
        .started_at
        .map(|t| now.saturating_duration_since(t).as_secs())
        .unwrap_or(0);
    let last_at_ms_ago = s
        .last_at
        .map(|t| now.saturating_duration_since(t).as_millis() as u64);
    let rate = |n: u64| -> f64 {
        if uptime_secs == 0 { 0.0 } else { (n as f64) / (uptime_secs as f64) }
    };
    Json(serde_json::json!({
        "running":            s.started_at.is_some(),
        "uptime_secs":        uptime_secs,
        "last_at_ms_ago":     last_at_ms_ago,
        "total":              s.total,
        "dibit":              s.dibit,
        "traffic":            s.traffic,
        "iq":                 s.iq,
        "lsm_dibit":          s.lsm_dibit,
        "traffic_lsm_dibit":  s.traffic_lsm_dibit,
        "traffic_iq":         s.traffic_iq,
        "rate_per_sec": {
            "total":             rate(s.total),
            "dibit":             rate(s.dibit),
            "traffic":           rate(s.traffic),
            "iq":                rate(s.iq),
            "lsm_dibit":         rate(s.lsm_dibit),
            "traffic_lsm_dibit": rate(s.traffic_lsm_dibit),
            "traffic_iq":        rate(s.traffic_iq),
        },
    }))
}


/// Phase 9: side-by-side comparison matrix of the surviving decoder
/// sources. Phase 6D software-LSM columns (`ps_iq_lsm` + `ps_phase6d`)
/// were retired along with the iq_lsm_decoder and LsmStats; the
/// dashboard decoder matrix is now PS-C4FM (dormant, kept for
/// future C4FM sites), PS-LSM framer (software framer on HDL LSM
/// dibits — the current production control-channel path), and
/// PL-HDL (the FPGA LSM chain's own runtime stats tapped directly
/// from the register bank).
pub async fn get_decoder_compare(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let dec_c4fm = state.decoder.read().await;
    let dec_lsm = state.lsm_decoder.read().await;
    let hdl_rt = state.hdl_lsm.lock().await;

    fn fmt_nac(n: Option<crate::protocol::p25::types::Nac>) -> serde_json::Value {
        match n {
            Some(v) => serde_json::Value::String(format!("{}", v)),
            None => serde_json::Value::Null,
        }
    }
    fn fmt_nac_u16(n: u16) -> String { format!("0x{:03X}", n) }

    let hdl_winner_nac = hdl_rt
        .top_nacs(1)
        .first()
        .map(|(n, _)| fmt_nac_u16(*n))
        .unwrap_or_else(|| "--".to_string());

    Json(serde_json::json!({
        "ps_c4fm": {
            "label":           "PS C4FM (software, HDL C4FM dibit-fed — DORMANT on LSM sites)",
            "system_nac":      fmt_nac(dec_c4fm.system.nac),
            "messages":        dec_c4fm.recent_messages.len(),
            "active_grants":   dec_c4fm.grants.len(),
            "bands_known":     dec_c4fm.bands.len(),
            "sync_hits":       dec_c4fm.sync_hits(),
            "sync_near":       dec_c4fm.sync_near_misses(),
            "sync_best_dist":  if dec_c4fm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_c4fm.best_sync_distance())
            },
            "total_dibits":    dec_c4fm.total_dibits(),
            "nid_attempts":          dec_c4fm.nid_attempts,
            "nid_decode_failures":   dec_c4fm.nid_decode_failures,
            "nid_invalid_duid":      dec_c4fm.nid_invalid_duid,
            "nid_decoded_ok":        dec_c4fm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_c4fm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_c4fm.tsdu_attempts,
            "tsbk_block_attempts":   dec_c4fm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_c4fm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_c4fm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_c4fm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_c4fm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_c4fm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_c4fm.tsbk_unknown_opcode,
        },
        "ps_lsm": {
            "label":           "PS LSM framer (software framer on HDL LSM dibits — production)",
            "system_nac":      fmt_nac(dec_lsm.system.nac),
            "messages":        dec_lsm.recent_messages.len(),
            "active_grants":   dec_lsm.grants.len(),
            "bands_known":     dec_lsm.bands.len(),
            "sync_hits":       dec_lsm.sync_hits(),
            "sync_near":       dec_lsm.sync_near_misses(),
            "sync_best_dist":  if dec_lsm.best_sync_distance() == u32::MAX {
                serde_json::Value::Null
            } else {
                serde_json::Value::from(dec_lsm.best_sync_distance())
            },
            "total_dibits":    dec_lsm.total_dibits(),
            "nid_attempts":          dec_lsm.nid_attempts,
            "nid_decode_failures":   dec_lsm.nid_decode_failures,
            "nid_invalid_duid":      dec_lsm.nid_invalid_duid,
            "nid_decoded_ok":        dec_lsm.nid_decoded_ok,
            "nid_decoded_tsdu":      dec_lsm.nid_decoded_tsdu,
            "tsdu_attempts":         dec_lsm.tsdu_attempts,
            "tsbk_block_attempts":   dec_lsm.tsbk_block_attempts,
            "tsbk_trellis_failures": dec_lsm.tsbk_trellis_failures,
            "tsbk_crc_failures":     dec_lsm.tsbk_crc_failures,
            "tsbk_crc_ok":           dec_lsm.tsbk_crc_ok,
            "tsbk_crc_ok_plain":     dec_lsm.tsbk_crc_ok_plain,
            "tsbk_crc_ok_xored":     dec_lsm.tsbk_crc_ok_xored,
            "tsbk_unknown_opcode":   dec_lsm.tsbk_unknown_opcode,
        },
        "pl_hdl": {
            "label":           "PL HDL LSM chain (FPGA gateware)",
            "winner_nac":      hdl_winner_nac,
            "total_nids":      hdl_rt.total_nid_events,
            "valid_nids":      hdl_rt.valid_nid_events,
            "valid_pct":       if hdl_rt.total_nid_events == 0 {
                0.0
            } else {
                100.0 * (hdl_rt.valid_nid_events as f64)
                    / (hdl_rt.total_nid_events as f64)
            },
            "drop_count":      hdl_rt.last_drop_count,
            "pll_dbg":         hdl_rt.pll_dbg,
            "sp_dbg":          hdl_rt.sp_dbg,
            "sync_distance":   hdl_rt.sync_distance,
            "dibit_overflow_ticks": hdl_rt.dibit_overflow_ticks,
            "iq_overflow_ticks":    hdl_rt.iq_overflow_ticks,
        },
    }))
}



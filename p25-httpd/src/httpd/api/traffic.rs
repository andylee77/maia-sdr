//! Current-call view: traffic chain, IMBE, vocoded audio.
//!
//! Consumer orientation: "what's happening on the traffic channel
//! right now?" These endpoints are all tied to the in-flight call:
//! follower state (`/api/traffic`), raw IMBE frames being vocoded
//! (`/api/imbe_dump`), and the vocoded PCM stream (`/api/audio` as
//! an open-ended WAV, `/ws/audio` as binary 20-ms frames over
//! WebSocket — see `api::ws`).
//!
//! `/api/traffic` doubles as a manual-override surface: `?retune_hz`,
//! `?demod_enable`, `?follower=on|off`. The grant follower task in
//! `main.rs` respects `follower_enabled` on every 50 ms poll, so
//! toggling it off takes effect within one tick.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
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

/// Phase 7A.1: GET /api/traffic -- traffic-channel grant follower
/// state + dibit DMA counters, with optional manual control via
/// query parameters.
///
/// **Read-side** (no params): returns a snapshot of the TrafficChain
/// state machine, the TrafficStats counters, and the traffic_dma IRQ
/// count.
///
/// **Write-side** (query params, applied in this order before reading
/// the snapshot below):
///
/// 1. `?reset_stats=1` -- zero out the TrafficStats counters
///    (wakeups, total_*, dibit_hist). Useful for clean A/B
///    comparisons after a config change.
/// 2. `?follower=on|off` -- pause/resume the 50 ms grant-follower
///    polling task in main.rs. When `off`, manual retunes won't be
///    immediately overridden by the next snapshot. Default state is
///    `on`; the override does NOT persist across p25-httpd restarts.
/// 3. `?retune_hz=<i64>` -- manually retune the traffic DDC to the
///    supplied NCO offset in Hz, signed, relative to the AD9361 RX
///    LO. Routes through the full `retune_traffic_chain` sequence
///    (disable -> NCO write -> 2 ms FIR flush -> re-enable -> reset
///    pulse -> demod enable), same as the grant follower. Bypasses
///    the follower's PPM correction, so the offset you supply is
///    what the register sees -- useful for measuring PPM error
///    directly. Resets the traffic framer before the retune.
/// 4. `?demod_enable=0|1` -- manually flip the
///    `traffic_demod_control.demod_enable` register bit. Rarely
///    needed now that #3 leaves demod_enable=1, but kept for
///    explicit debug control (e.g. forcing demod_enable=0 to
///    snapshot the chain in a quiet state).
///
/// All four params can be combined in one call:
/// `GET /api/traffic?follower=off&reset_stats=1&retune_hz=2862500&demod_enable=1`
/// will pause the follower, zero the counters, retune to RX LO + 2.8625
/// MHz, and turn on the demod -- in that order, so the histogram
/// counts only what arrives after the retune.
///
/// At Phase 7A.1 the traffic chain is C4FM-only, so the dibit *content*
/// is expected garbage on real LSM voice channels. The histogram is
/// included as a sanity check: a dead chain produces all-zero dibits,
/// a live chain produces a roughly even spread across all four dibit
/// values. Phase 7A.2 will add an LSM parallel chain on the traffic
/// side and the histogram will become decode-quality data.
pub async fn get_traffic(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(params): axum::extract::Query<
        std::collections::HashMap<String, String>,
    >,
) -> Json<serde_json::Value> {
    use std::sync::atomic::Ordering;

    // Track which write actions actually fired so the JSON response
    // can echo them back -- gives the caller a confirmation that the
    // params were parsed and applied (vs. silently ignored due to a
    // typo).
    let mut applied: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    // ── 1. reset_stats ──
    if params.get("reset_stats").map(String::as_str) == Some("1") {
        let mut s = state.traffic_stats.lock().await;
        *s = crate::TrafficStats::default();
        applied.push("reset_stats=1".into());
    }

    // ── 2. follower on/off ──
    if let Some(v) = params.get("follower") {
        match v.as_str() {
            "on" | "1" | "true" => {
                state
                    .traffic_follower_enabled
                    .store(true, Ordering::Relaxed);
                applied.push("follower=on".into());
            }
            "off" | "0" | "false" => {
                state
                    .traffic_follower_enabled
                    .store(false, Ordering::Relaxed);
                applied.push("follower=off".into());
            }
            other => {
                errors.push(format!(
                    "follower={other}: expected on|off|1|0|true|false"
                ));
            }
        }
    }

    // ── 2b. lock_freq (diagnostic: park chain on current freq) ──
    //   When `lock=on`, the grant follower stops dispatching
    //   retunes and the idle-timeout teardown is suppressed. The
    //   chain stays on whatever freq is currently set so the
    //   operator can measure AGC/PLL/sync settle behaviour against
    //   a known signal without retunes interfering.
    if let Some(v) = params.get("lock") {
        match v.as_str() {
            "on" | "1" | "true" => {
                state
                    .traffic_lock_freq
                    .store(true, Ordering::Relaxed);
                applied.push("lock=on".into());
            }
            "off" | "0" | "false" => {
                state
                    .traffic_lock_freq
                    .store(false, Ordering::Relaxed);
                applied.push("lock=off".into());
            }
            other => {
                errors.push(format!(
                    "lock={other}: expected on|off|1|0|true|false"
                ));
            }
        }
    }

    // ── 3. retune_hz (manual NCO write) ──
    //    Linux-only because it touches the FPGA registers via the
    //    ip_core lock. The non-Linux build path simply records an
    //    error so host-side cargo test of the routing still works.
    if let Some(v) = params.get("retune_hz") {
        match v.parse::<i64>() {
            Ok(offset_hz) => {
                #[cfg(target_os = "linux")]
                {
                    // 2026-04-16 fix: route manual retune through the
                    // full retune_traffic_chain sequence (disable ->
                    // NCO write -> 2 ms FIR flush -> re-enable ->
                    // reset pulse -> demod enable) so the debug path
                    // matches the grant follower's production path.
                    // Also reset the traffic framer so stale dibits
                    // from the previous NCO don't feed a half-
                    // processed state on the new frequency.
                    // Change 054: in airtime mode the retune's epoch
                    // cut (recorded by the IpCore hook) resets the
                    // framer at the right dibit instead.
                    if !state.imbe_forwarder.epochs_active() {
                        let mut dec = state.traffic_lsm_decoder
                            .write().await;
                        dec.reset_framer_state();
                    }
                    // 2026-05-03 dual-DDC: manual retune writes the
                    // traffic DDC NCO directly (mirror of the control
                    // chain). Used for diagnostics — sweep ?retune_hz=
                    // to find the channel offset; production grant
                    // follower uses the same retune_traffic_chain path.
                    let sample_rate_hz =
                        state.current_sample_rate_hz
                            .load(std::sync::atomic::Ordering::Relaxed);
                    let retune_result = {
                        let core = state.ip_core.lock().await;
                        // Manual debug retune always passes freq_changed=true
                        // — operator-initiated sweeps want the chain re-seeded
                        // regardless of the previous offset.
                        // 2026-05-03 quality-gated coast: manual
                        // /api/traffic retune always pulses reset —
                        // operator sweeps want a clean cold-start to
                        // expose the freq-vs-acquisition relation
                        // without inheriting state from whatever the
                        // chain was doing. Seeds parameter retained
                        // for API symmetry but unused.
                        core.retune_traffic_chain(
                            offset_hz as f64,
                            sample_rate_hz as f64,
                            true,
                            None,
                        )
                    };
                    if let Err(ref e) = retune_result {
                        errors.push(format!(
                            "retune_traffic_chain: {e}"));
                    }
                    {
                        let mut mgr = state.traffic_chain.lock().await;
                        mgr.last_offset_hz = offset_hz;
                        let nco_frac = offset_hz as f64
                            / sample_rate_hz as f64;
                        mgr.nco_word = (nco_frac * (1u64 << 28) as f64)
                            as i32 as u32 & 0x0FFF_FFFF;
                    }
                    applied.push(format!(
                        "retune_hz={offset_hz} (dual-DDC)"
                    ));
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = offset_hz;
                    errors.push(
                        "retune_hz requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            Err(_) => {
                errors.push(format!(
                    "retune_hz={v}: expected signed integer Hz offset"
                ));
            }
        }
    }

    // ── 4. demod_enable ── (retired in Phase 10.8 along with the
    //    HDL C4FM chain). The param now just routes to `traffic_lsm_
    //    enable` so old external scripts don't error out, but LSM
    //    enablement is normally driven by the grant follower's
    //    retune_traffic_chain path.
    if let Some(v) = params.get("demod_enable") {
        let parsed = match v.as_str() {
            "1" | "on" | "true" => Some(true),
            "0" | "off" | "false" => Some(false),
            _ => None,
        };
        match parsed {
            Some(bit) => {
                #[cfg(target_os = "linux")]
                {
                    let core = state.ip_core.lock().await;
                    core.set_traffic_lsm_enable(bit);
                    applied.push(format!(
                        "demod_enable={bit} (→ traffic_lsm_enable)"));
                }
                #[cfg(not(target_os = "linux"))]
                {
                    let _ = bit;
                    errors.push(
                        "demod_enable requires hardware (target_os=linux)"
                            .into(),
                    );
                }
            }
            None => {
                errors.push(format!(
                    "demod_enable={v}: expected 0|1|on|off|true|false"
                ));
            }
        }
    }

    // ── Snapshot read (always, even after a write) ──
    let (
        state_label,
        current_channel,
        current_talkgroup,
        current_frequency_hz,
        nco_word,
        last_offset_hz,
        grants_seen,
        grants_seen_new,
        grants_seen_update,
        retunes,
        grants_rejected_encrypted,
        last_retune_at_secs_ago,
        last_duid,
        last_nac,
        hdus_seen,
        ldus_seen,
        tdus_seen,
        nco_skips,
        parked_freq_hz,
    ) = {
        let mgr = state.traffic_chain.lock().await;
        let label = mgr.state_label();
        let ch = mgr.current_channel().map(|c| c.0);
        let tg = mgr.current_talkgroup().map(|t| t.0);
        let freq = mgr.current_frequency();
        let nco = mgr.nco_word;
        let offset = mgr.last_offset_hz;
        let seen = mgr.grants_seen;
        let seen_new = mgr.grants_seen_new;
        let seen_update = mgr.grants_seen_update;
        let retunes = mgr.retunes;
        let rejected_enc = mgr.grants_rejected_encrypted;
        let age = mgr
            .last_retune_at
            .map(|t| t.elapsed().as_secs_f64());
        let duid = mgr.last_duid;
        let nac = mgr.last_nac;
        let hdus = mgr.hdus_seen;
        let ldus = mgr.ldus_seen;
        let tdus = mgr.tdus_seen;
        let skips = mgr.nco_skips;
        let parked = mgr.parked_freq_hz;
        (label, ch, tg, freq, nco, offset, seen, seen_new, seen_update,
         retunes, rejected_enc, age, duid, nac, hdus, ldus, tdus, skips,
         parked)
    };

    // M2B 2026-05-02: traffic_lsm_* registers restored. Mirrors the
    // control-side `/api/hdl_lsm` snapshot but on the M2B
    // traffic_lsm bank (mux-fed chain).
    #[cfg(target_os = "linux")]
    let traffic_lsm_chain_json = {
        let core = state.ip_core.lock().await;
        let s = core.traffic_lsm_status();
        let drop_count = core.traffic_lsm_drop_count();
        let last_buffer = core.traffic_lsm_dibit_last_buffer();
        let next_addr = core.traffic_lsm_dibit_next_address();
        let (pll_dbg, sample_point_dbg) = core.traffic_lsm_debug();
        let (agc_gain_q9_7, agc_mag_q1_15) = core.traffic_lsm_agc_debug();
        let traffic_threshold = core.traffic_lsm_agc_threshold();
        let (en, dma_en, dc_block, agc) = core.traffic_lsm_control_readback();
        let ver = core.core_version();
        let agc_gain = (agc_gain_q9_7 as f64) / 128.0;
        let agc_mag  = (agc_mag_q1_15 as f64) / 32768.0;
        let agc_product = agc_gain * agc_mag;
        serde_json::json!({
            "core_version":       ver.to_string(),
            "signal_hold":        ver.has_lsm_signal_hold(),
            "pll_clamp_q13":      ver.pll_clamp_q213(),
            "enabled":            en,
            "dibit_dma_enabled":  dma_en,
            "dc_block_enabled":   dc_block,
            "agc_enabled":        agc,
            "bch_busy":           s.bch_busy,
            "in_nid_window":      s.in_nid_window,
            "nid_event":          s.nid_event,
            "nid_valid":          s.nid_valid,
            "n_errors":           s.n_errors,
            "sync_distance":      s.sync_distance,
            "dibit_overflow":     s.dibit_overflow,
            "drop_count":         drop_count,
            "dibit_last_buffer":  last_buffer,
            "dibit_next_addr":    format!("0x{:08X}", next_addr),
            "pll_dbg":            pll_dbg,
            "sample_point_dbg":   sample_point_dbg,
            "agc_gain":           agc_gain,
            "agc_mag":            agc_mag,
            "agc_product":        agc_product,
            "agc_gain_raw_q9_7":  agc_gain_q9_7,
            "agc_mag_raw_q1_15":  agc_mag_q1_15,
            "mag_update_threshold":   traffic_threshold,
            "mag_update_threshold_f": (traffic_threshold as f64) / 32768.0,
        })
    };
    #[cfg(not(target_os = "linux"))]
    let traffic_lsm_chain_json = serde_json::json!(null);

    // Control-chain AGC snapshot, sibling of traffic_lsm_chain.
    // Added so a single /api/traffic call surfaces both AGC states
    // for diagnosis ("is the control AGC at the expected product?").
    #[cfg(target_os = "linux")]
    let control_agc_json = {
        let core = state.ip_core.lock().await;
        let (pll_dbg, sp_dbg) = core.lsm_debug();
        let (gain_q9_7, mag_q1_15) = core.lsm_agc_debug();
        let threshold = core.lsm_agc_threshold();
        let gain = (gain_q9_7 as f64) / 128.0;
        let mag  = (mag_q1_15 as f64) / 32768.0;
        serde_json::json!({
            "pll_dbg":            pll_dbg,
            "sample_point_dbg":   sp_dbg,
            "agc_gain":           gain,
            "agc_mag":            mag,
            "agc_product":        gain * mag,
            "agc_gain_raw_q9_7":  gain_q9_7,
            "agc_mag_raw_q1_15":  mag_q1_15,
            "mag_update_threshold":     threshold,
            "mag_update_threshold_f":   (threshold as f64) / 32768.0,
        })
    };
    #[cfg(not(target_os = "linux"))]
    let control_agc_json = serde_json::json!(null);

    let stats_json = {
        let s = state.traffic_stats.lock().await;
        let total: u64 = s.dibit_hist.iter().sum();
        let pct = |v: u64| -> f64 {
            if total == 0 {
                0.0
            } else {
                100.0 * v as f64 / total as f64
            }
        };
        serde_json::json!({
            "wakeups":        s.wakeups,
            "total_buffers":  s.total_buffers,
            "total_bytes":    s.total_bytes,
            "total_dibits":   s.total_dibits,
            "dibit_hist":     s.dibit_hist,
            "dibit_hist_pct": [
                pct(s.dibit_hist[0]), pct(s.dibit_hist[1]),
                pct(s.dibit_hist[2]), pct(s.dibit_hist[3])
            ],
            "started_secs_ago": s.started_at.map(|t| t.elapsed().as_secs_f64()),
            "last_secs_ago":    s.last_at.map(|t| t.elapsed().as_secs_f64()),
        })
    };

    let irq_json = {
        let s = state.irq_stats.lock().await;
        serde_json::json!({
            "traffic_dma_total":       s.traffic,
            "traffic_lsm_dibit_total": s.traffic_lsm_dibit,
        })
    };

    // Phase 7C: IMBE counter snapshot from the traffic LSM voice
    // Phase 7D: read IMBE extraction + vocoder stats from the
    // ImbeForwarder's atomics. Updated synchronously by the voice
    // handler (extraction counters) and by the vocoder task (PCM
    // produced, errors, encrypted skips).
    let imbe_json = {
        use std::sync::atomic::Ordering;
        let c = &state.imbe_forwarder;
        let last_imbe_at_millis = c.last_imbe_at_millis.load(Ordering::Relaxed);
        let now_millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let last_imbe_secs_ago = if last_imbe_at_millis == 0 {
            None
        } else {
            Some((now_millis.saturating_sub(last_imbe_at_millis)) as f64 / 1000.0)
        };
        // Current-call deltas: cumulative minus baseline snapshotted
        // on the most recent HDU. 0 when no call has started this
        // session.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let hdu_tot = c.hdu_count.load(Ordering::Relaxed);
        let ldu1_tot = c.ldu1_count.load(Ordering::Relaxed);
        let ldu2_tot = c.ldu2_count.load(Ordering::Relaxed);
        let tdu_tot = c.tdu_count.load(Ordering::Relaxed);
        let tdulc_tot = c.tdu_lc_count.load(Ordering::Relaxed);
        let ext_tot = c.imbe_frames_extracted.load(Ordering::Relaxed);
        let drop_tot = c.imbe_frames_dropped.load(Ordering::Relaxed);
        let pcm_tot = c.vocoder_pcm_produced.load(Ordering::Relaxed);
        let err_tot = c.vocoder_errors.load(Ordering::Relaxed);
        let silent_tot = c.vocoder_frames_silent_observed.load(Ordering::Relaxed);
        let call_start_ms = c.call_baseline_unix_ms.load(Ordering::Relaxed);
        let call_duration_ms = if call_start_ms == 0 { 0 }
            else { now_ms.saturating_sub(call_start_ms) };
        let cc_tg = c.current_talkgroup.load(Ordering::Relaxed);
        let cc_src = c.current_source.load(Ordering::Relaxed);
        let current_call_json = serde_json::json!({
            "started_unix_ms":  if call_start_ms == 0 { None }
                                else { Some(call_start_ms) },
            "duration_ms":      call_duration_ms,
            "tg":               if cc_tg == 0 { None } else { Some(cc_tg) },
            "source":           if cc_src == 0 { None } else { Some(cc_src) },
            "hdu":              hdu_tot.saturating_sub(
                c.call_baseline_hdu.load(Ordering::Relaxed)),
            "ldu1":             ldu1_tot.saturating_sub(
                c.call_baseline_ldu1.load(Ordering::Relaxed)),
            "ldu2":             ldu2_tot.saturating_sub(
                c.call_baseline_ldu2.load(Ordering::Relaxed)),
            "tdu":              tdu_tot.saturating_sub(
                c.call_baseline_tdu.load(Ordering::Relaxed)),
            "tdu_lc":           tdulc_tot.saturating_sub(
                c.call_baseline_tdu_lc.load(Ordering::Relaxed)),
            "imbe_extracted":   ext_tot.saturating_sub(
                c.call_baseline_imbe_extracted.load(Ordering::Relaxed)),
            "imbe_dropped":     drop_tot.saturating_sub(
                c.call_baseline_imbe_dropped.load(Ordering::Relaxed)),
            "vocoder_pcm":      pcm_tot.saturating_sub(
                c.call_baseline_pcm.load(Ordering::Relaxed)),
            "vocoder_errors":   err_tot.saturating_sub(
                c.call_baseline_errors.load(Ordering::Relaxed)),
            "vocoder_silent":   silent_tot.saturating_sub(
                c.call_baseline_silent.load(Ordering::Relaxed)),
        });
        serde_json::json!({
            "hdu_count":              hdu_tot,
            "ldu1_count":             ldu1_tot,
            "ldu2_count":             ldu2_tot,
            "tdu_count":              tdu_tot,
            "tdu_lc_count":           tdulc_tot,
            "imbe_frames_extracted":  ext_tot,
            "imbe_frames_dropped":    drop_tot,
            "imbe_frames_dropped_idle": c.imbe_frames_dropped_idle.load(Ordering::Relaxed),
            "last_imbe_secs_ago":     last_imbe_secs_ago,
            "last_batch_tg":          c.last_batch_tg.load(Ordering::Relaxed),
            "queue_depth":            c.queue_depth_now(),
            "queue_capacity":         c.queue_capacity_max(),
            "queue_high_water":       c.queue_high_water.load(Ordering::Relaxed),
            "current_call":           current_call_json,
            "vocoder_pcm_produced":   pcm_tot,
            "vocoder_errors":         err_tot,
            "vocoder_frames_encrypted": c.vocoder_frames_encrypted.load(Ordering::Relaxed),
            "vocoder_frames_silent_observed": silent_tot,
            // 2026-04-19 TDULC LCW parse diagnostics. Sum of the
            // four should equal `tdulc_parse_attempts`, which is in
            // turn `tdu_lc_count` minus the entries that arrived
            // while the traffic follower was Idle (current_tg==0).
            "tdulc_parse_attempts":
                c.tdulc_parse_attempts.load(Ordering::Relaxed),
            "tdulc_parse_motorola":
                c.tdulc_parse_motorola.load(Ordering::Relaxed),
            "tdulc_parse_gvcu":
                c.tdulc_parse_gvcu.load(Ordering::Relaxed),
            "tdulc_parse_gvu":
                c.tdulc_parse_gvu.load(Ordering::Relaxed),
            "tdulc_parse_callterm":
                c.tdulc_parse_callterm.load(Ordering::Relaxed),
            "tdulc_parse_other":
                c.tdulc_parse_other.load(Ordering::Relaxed),
            "tdulc_parse_none":
                c.tdulc_parse_none.load(Ordering::Relaxed),
            "speaker_end_deduplicated":
                c.speaker_end_deduplicated.load(Ordering::Relaxed),
            "speaker_end_invalid":
                c.speaker_end_invalid.load(Ordering::Relaxed),
            "tdulc_last_lc_bytes":
                c.tdulc_last_lc_bytes
                    .lock()
                    .map(|b| b.iter().map(|x| format!("{:02X}", x))
                        .collect::<Vec<_>>().join(" "))
                    .unwrap_or_else(|_| "(poisoned)".into()),
            // 2026-04-19: recorder-side boundary event counts — lets
            // us pinpoint whether a Motorola source stamp fired from
            // the framer but arrived at the recorder after `active`
            // was already finalised by the grace window.
            "recorder_boundaries_hdu":
                state.recorder_diag.boundaries_hdu.load(Ordering::Relaxed),
            "recorder_boundaries_tdulc_with_source":
                state.recorder_diag
                    .boundaries_tdulc_with_source
                    .load(Ordering::Relaxed),
            "recorder_boundaries_tdulc_without_source":
                state.recorder_diag
                    .boundaries_tdulc_without_source
                    .load(Ordering::Relaxed),
            "recorder_source_stamps_applied":
                state.recorder_diag
                    .source_stamps_applied
                    .load(Ordering::Relaxed),
            "recorder_source_stamps_lost_no_active":
                state.recorder_diag
                    .source_stamps_lost_no_active
                    .load(Ordering::Relaxed),
            "recorder_boundary_lag_events":
                state.recorder_diag
                    .boundary_lag_events
                    .load(Ordering::Relaxed),
            // 2026-04-26 routing-loss diagnostics. Each = 1 IMBE
            // frame = 20 ms of dropped audio. Should both stay near
            // 0; non-zero values quantify the audio lost between
            // vocoder and recorder.
            //
            // `chunks_dropped_call_id_mismatch`: forwarder stamped
            // a chunk with a call_id that didn't match the active
            // recording's. Suggests `mirror_active` is propagating
            // `current_call_id` slower than the audio path.
            //
            // `chunks_dropped_no_active`: chunk arrived when the
            // recorder had no active recording — typically the gap
            // between trailing-PCM drain end and the next CallOpen.
            // The dominant short-call loss path observed 2026-04-26.
            "recorder_chunks_dropped_call_id_mismatch":
                state.recorder_diag
                    .chunks_dropped_call_id_mismatch
                    .load(Ordering::Relaxed),
            "recorder_chunks_dropped_no_active":
                state.recorder_diag
                    .chunks_dropped_no_active
                    .load(Ordering::Relaxed),
            // 2026-04-30: chunks dropped by the recorder's TG-match
            // gate (chunk.tg != 0 && != active.tg). Catches cross-TG
            // bleed during same-freq channel-reuse where the vocoder
            // queue holds in-flight OLD-TG batches past the retune.
            // Each = 20 ms of correctly-rejected old-TG audio. Non-
            // zero is healthy; the alternative is the same audio
            // bleeding into the new recording.
            "recorder_chunks_dropped_tg_mismatch":
                state.recorder_diag
                    .chunks_dropped_tg_mismatch
                    .load(Ordering::Relaxed),
        })
    };

    // Phase 7C: also surface the traffic_lsm_decoder's own
    // sync/decode counters so the dashboard can see whether the
    // decoder's framer is finding sync hits on the traffic dibit
    // stream (the most important bring-up signal -- if sync_hits
    // == 0 we know the dibit ring isn't carrying anything the
    // decoder recognises as P25).
    let traffic_lsm_decoder_json = {
        let d = state.traffic_lsm_decoder.read().await;
        serde_json::json!({
            "sync_hits":          d.sync_hits(),
            "sync_near_misses":   d.sync_near_misses(),
            "best_sync_distance": if d.best_sync_distance() == u32::MAX { 99 } else { d.best_sync_distance() },
            "recent_msg_count":   d.recent_messages.len(),
            "ldu1":               d.ldu1_count,
            "ldu2":               d.ldu2_count,
            "hdu":                d.hdu_count,
            "tdu":                d.tdu_count,
            "tdu_lc":             d.tdu_lc_count,
        })
    };

    // 2026-04-26 per-freq AGC seed cache snapshot. Each entry is
    // the EMA of converged AGC gain seen at CallClose for clear
    // calls on that freq. Used by retune_traffic_chain to seed
    // the chain's AGC at known-good values instead of the cold
    // GAIN_INIT (1.0×). Empty = no clear calls have closed yet.
    let agc_freq_cache_json: Vec<serde_json::Value> = state.imbe_forwarder
        .agc_cache_snapshot()
        .into_iter()
        .map(|(f, q97, gain)| serde_json::json!({
            "freq_hz": f,
            "gain_q97": q97,
            "gain": gain,
        }))
        .collect();

    // Phase 7C: pull the encryption flag from the currently-locked
    // grant, if any. The grant follower's TrafficChain holds the
    // Phase 7D: the encryption flag shown on the dashboard comes from
    // the ImbeForwarder's `call_encrypted` atomic, which is the same
    // flag the vocoder task reads to decide whether to decode or skip.
    // This is set by the grant follower task whenever it observes a
    // grant for the locked TG, and cleared on Idle transition.
    // Single source of truth: what the vocoder sees = what the
    // dashboard shows.
    let current_call_encrypted = {
        let mgr = state.traffic_chain.lock().await;
        if mgr.current_talkgroup().is_some() {
            Some(state.imbe_forwarder.call_encrypted.load(
                std::sync::atomic::Ordering::Relaxed,
            ))
        } else {
            None
        }
    };

    let follower_on =
        state.traffic_follower_enabled.load(Ordering::Relaxed);
    let lock_on = state.traffic_lock_freq.load(Ordering::Relaxed);

    Json(serde_json::json!({
        "state":                     state_label,
        "follower_enabled":          follower_on,
        "lock_freq":                 lock_on,
        "current_channel":           current_channel,
        "current_talkgroup":         current_talkgroup,
        "current_frequency_hz":      current_frequency_hz,
        // 2026-04-25: physical freq the FPGA chain is parked on.
        // Stays populated between calls (chain doesn't pause), so the
        // dashboard can show "Traffic Channel: 858.4625 MHz" even
        // when no call is active. None at boot before first retune.
        "parked_freq_hz":            parked_freq_hz,
        "nco_word":                  nco_word,
        "nco_word_hex":              format!("0x{:08X}", nco_word),
        "last_offset_hz":            last_offset_hz,
        "grants_seen":               grants_seen,
        // 2026-04-19 dedup counters: `grants_seen` counts every decode
        // of any grant-family TSBK (SDRTrunk-unfriendly over-count);
        // `_new` / `_update` split by the 2 s (TG, freq) dedup window
        // so callers can reconcile against SDRTrunk semantics.
        "grants_seen_new":           grants_seen_new,
        "grants_seen_update":        grants_seen_update,
        "retunes":                   retunes,
        "nco_skips":                 nco_skips,
        "pll_watchdog": {
            "enabled":       state.imbe_forwarder.pll_wd_enabled.load(Ordering::Relaxed),
            "resets_onset":  state.imbe_forwarder.pll_wd_resets_onset.load(Ordering::Relaxed),
            "resets_pinned": state.imbe_forwarder.pll_wd_resets_pinned.load(Ordering::Relaxed),
        },
        "grants_rejected_encrypted": grants_rejected_encrypted,
        "last_retune_secs_ago":      last_retune_at_secs_ago,
        // Phase 7A.2: NID event counters and post-TDU hold
        "last_duid":                 last_duid,
        "last_duid_hex":             last_duid.map(|d| format!("0x{:X}", d)),
        "last_duid_label":           last_duid.map(|d| match d {
            0x0 => "HDU",
            0x3 => "TDU",
            0x5 => "LDU1",
            0x7 => "TSDU",
            0xA => "LDU2",
            0xC => "PDU",
            0xF => "TDU_LC",
            _   => "?",
        }),
        "last_nac":                  last_nac,
        "last_nac_hex":              last_nac.map(|n| format!("0x{:03X}", n)),
        "hdus_seen":                 hdus_seen,
        "ldus_seen":                 ldus_seen,
        "tdus_seen":                 tdus_seen,
        "stats":                     stats_json,
        "irq":                       irq_json,
        "traffic_lsm_chain":         traffic_lsm_chain_json,
        "control_lsm_agc":           control_agc_json,
        "agc_freq_cache":            agc_freq_cache_json,
        "applied":                   applied,
        "errors":                    errors,
        "phase":                     "7C",
        "modulation":                "C4FM + LSM (parallel chains, LSM is the active one for HDU/TDU/LDU dispatch + IMBE extraction)",
        // Phase 7C: encryption flag from the control channel grant
        // for the currently-locked TG (None if no call active or
        // no grant in store). Phase 7D will read this to skip the
        // vocoder for encrypted calls.
        "current_call_encrypted":    current_call_encrypted,
        // Phase 7C: IMBE frame counter snapshot from the
        // traffic_lsm_decoder's voice handler.
        "imbe":                      imbe_json,
        // Phase 7C: traffic_lsm_decoder framer state (sync hits,
        // recent msgs, per-DUID counters from the decoder itself).
        // Distinct from `imbe` above which counts via the voice
        // handler atomic counters: these are the decoder-internal
        // counters and should track 1:1 with the imbe ones.
        "traffic_lsm_decoder":       traffic_lsm_decoder_json,
        "controls": {
            "reset_stats":   "?reset_stats=1            -- zero TrafficStats",
            "follower":      "?follower=on|off          -- pause/resume 50 ms poll",
            "retune_hz":     "?retune_hz=<i64>          -- manual NCO offset (Hz, signed)",
            "demod_enable":  "?demod_enable=0|1         -- manual demod_enable bit (C4FM chain only -- LSM chain has its own enable in HDL)"
        },
        "note": "Phase 7C: traffic-side LSM dibit reader feeds a \
                 ControlChannelDecoder that runs the same Hunting -> \
                 ReadingNid -> ReadingDataUnit state machine as the \
                 control side. On LDU1/LDU2 dispatch the decoder \
                 strips status dibits, applies the 9 IMBE bit \
                 positions from SDRTrunk LDUMessage.java, and emits \
                 raw 144-bit IMBE frames via the VoiceHandler trait. \
                 Phase 7D will plug a vocoder into the same \
                 callback chain.",
    }))
}


/// GET /api/imbe_dump -- return the last 128 raw IMBE frames from the
/// ring buffer. Each entry has talkgroup, encrypted flag, and the raw
/// 18 bytes (144 bits) in hex. Use for offline vocoder testing.
pub async fn get_imbe_dump(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    // Recover from a poisoned mutex rather than killing the HTTP task:
    // the ring is a diagnostic buffer, no invariant is lost on poison.
    let ring = state.imbe_forwarder.imbe_ring
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let frames: Vec<serde_json::Value> = ring
        .iter()
        .map(|(tg, enc, bits)| {
            serde_json::json!({
                "talkgroup": tg,
                "encrypted": enc,
                "hex": bits.iter().map(|b| format!("{:02x}", b)).collect::<String>(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "count": frames.len(),
        "frames": frames,
    }))
}


/// GET /api/audio_test -- decode the IMBE ring buffer on-device and
/// return a WAV file. Filters to clear (non-encrypted) frames only.
/// Use to verify vocoder output without VLC streaming.
///
///   curl -o test.wav http://192.168.2.1:8080/api/audio_test
pub async fn get_audio_test(
    State(state): State<Arc<AppState>>,
) -> impl axum::response::IntoResponse {
    // Same poison-recovery as /api/imbe_dump above.
    let ring = state.imbe_forwarder.imbe_ring
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let clear: Vec<_> = ring.iter().filter(|(_, enc, _)| !enc).collect();

    let mut decoder = crate::vocoder::JmbeDecoder::new();
    let mut all_pcm: Vec<i16> = Vec::new();

    for (_tg, _enc, bits) in &clear {
        let frame = crate::protocol::p25::voice_frame::ImbeFrameRaw { bits: *bits };
        let pcm = decoder.decode_frame(&frame);
        all_pcm.extend_from_slice(&pcm);
    }

    // Build WAV
    let header = crate::audio::wav_header_8k_16bit_mono();
    let data_size = (all_pcm.len() * 2) as u32;
    let file_size = 36 + data_size;

    let mut wav = Vec::with_capacity(44 + all_pcm.len() * 2);
    wav.extend_from_slice(&header[..4]);   // RIFF
    wav.extend_from_slice(&file_size.to_le_bytes()); // actual size
    wav.extend_from_slice(&header[8..40]); // WAVEfmt...data
    wav.extend_from_slice(&data_size.to_le_bytes()); // actual data size
    for &sample in &all_pcm {
        wav.extend_from_slice(&sample.to_le_bytes());
    }

    (
        [
            (axum::http::header::CONTENT_TYPE, "audio/wav"),
            (
                axum::http::header::CONTENT_DISPOSITION,
                "attachment; filename=\"imbe_test.wav\"",
            ),
        ],
        wav,
    )
}

// ── Phase 7B: Monitor list ─────────────────────────────────────────────


/// GET /api/audio -- stream PCM audio.
///   ?format=wav  -- prepend a WAV header (default: raw PCM)
///   Content-Type: audio/L16;rate=8000;channels=1 (raw) or audio/wav
///
/// The response is a chunked HTTP stream that runs until the client
/// disconnects. Each chunk is 320 bytes (160 samples * 2 bytes).
/// Pipe to `aplay -r 8000 -f S16_LE -c 1` or open in VLC.
pub async fn get_audio(
    State(state): State<Arc<AppState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl axum::response::IntoResponse {
    let format = params
        .get("format")
        .map(String::as_str)
        .unwrap_or("raw");
    let want_wav = format == "wav";
    let mut rx = state.audio_tx.subscribe();

    let stream = async_stream::stream! {
        if want_wav {
            yield Ok::<_, std::io::Error>(bytes::Bytes::copy_from_slice(
                &crate::audio::wav_header_8k_16bit_mono()
            ));
        }
        loop {
            match rx.recv().await {
                Ok(chunk) => {
                    let mut buf = [0u8; 320];
                    for (i, &sample) in chunk.pcm.iter().enumerate() {
                        let le = sample.to_le_bytes();
                        buf[i * 2] = le[0];
                        buf[i * 2 + 1] = le[1];
                    }
                    yield Ok(bytes::Bytes::copy_from_slice(&buf));
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    let content_type = if want_wav {
        "audio/wav"
    } else {
        "audio/L16;rate=8000;channels=1"
    };

    (
        [(axum::http::header::CONTENT_TYPE, content_type)],
        axum::body::Body::from_stream(stream),
    )
}



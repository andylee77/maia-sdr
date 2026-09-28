//! Traffic LSM heartbeat, one task per traffic chain (change 066: moved
//! from `main.rs`, M2B 2026-05-02). Polls the chain's LSM status every
//! 16 ms — fine enough not to miss back-to-back NIDs (~14 ms apart). On
//! `nid_event` it dispatches NAC + DUID into the chain's `TrafficChain`
//! (HDU/LDU lifecycle) and broadcasts to sync_trace, call_boundary and the
//! activity feed.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::audio;
use crate::hardware::fpga;
use crate::protocol::p25::traffic_chain::TrafficChain;
use crate::services::event_log::{EventLog, LogCategory};
use crate::services::sync_trace::{self, SyncTraceRing, SyncTraceSample};

/// Poll tick (~60 Hz), as the control-channel heartbeat.
const TICK_MS: u64 = 16;

#[allow(clippy::too_many_arguments)]
pub fn spawn_traffic_heartbeat(
    core: Arc<Mutex<fpga::IpCore>>,
    mgr: Arc<Mutex<TrafficChain>>,
    imbe: Arc<ImbeForwarder>,
    event_tx: tokio::sync::broadcast::Sender<String>,
    event_log: Arc<EventLog>,
    boundary_tx: audio::CallBoundaryTx,
    sync_trace_ring: SyncTraceRing,
) {
    let lane = imbe.lane;
    tokio::spawn(async move {
        tracing::info!(
            "{lane} LSM heartbeat task started (polling LSM status @ {TICK_MS} ms)"
        );
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(TICK_MS));
        tick.tick().await;
        let mut total_polls: u64 = 0;
        let mut nid_events: u64 = 0;
        loop {
            tick.tick().await;
            total_polls += 1;
            let regs = {
                let core = core.lock().await;
                core.lane(lane).map(|l| {
                    let s = l.status();
                    let (n, d) = l.nid();
                    let (pll, sp) = l.debug();
                    let (g, m) = l.agc_debug();
                    (s, n, d, pll, sp, g, m)
                })
            };
            let Some((status, nac, duid, pll_dbg, sample_point_dbg, agc_gain, agc_mag)) = regs else {
                tracing::error!("{lane} chain is not present; heartbeat stops");
                return;
            };

            if !status.nid_event {
                continue;
            }
            nid_events += 1;

            // 2026-05-03 loss-of-sync detector: every traffic-LSM NID
            // event broadcasts a TrafficNidObserved boundary so the
            // lifecycle layer can stamp the active call's
            // `last_nid_at_ms`. Fired BEFORE the TG-gate below so pre-call
            // NIDs (chain still acquiring) also count toward "framer
            // alive". TG/NAC/DUID intentionally omitted — LoS is a pure
            // framer-state signal.
            // Change 057: `voice` = valid HDU / LDU1 / LDU2 NID; the
            // lifecycle uses a pair of them to see voice resume after an
            // end-of-transmission marker.
            // Change 071a: only voice NIDs matter to the lifecycle (the
            // other ones only stamped a field nothing reads); sending
            // every NID of both lanes crowded the boundary channel.
            if status.nid_valid && matches!(duid, 0x0 | 0x5 | 0xA) {
                let _ = boundary_tx.send(audio::CallBoundary {
                    kind: audio::CallBoundaryKind::TrafficNidObserved { voice: true },
                    nac,
                    talkgroup: None,
                    expected_submit_count: 0,
                    lane: Some(lane),
                });
            }

            let sync_trace_call_id = imbe.current_call_id.load(Ordering::Relaxed);
            if sync_trace_call_id != 0 {
                let unix_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                sync_trace::push(
                    &sync_trace_ring,
                    SyncTraceSample {
                        call_id: sync_trace_call_id,
                        unix_ms,
                        duid,
                        nac,
                        nid_valid: status.nid_valid,
                        bch_busy: status.bch_busy,
                        n_errors: status.n_errors,
                        sync_distance: status.sync_distance,
                        pll_dbg,
                        sample_point_dbg,
                        agc_gain,
                        agc_mag,
                    },
                );
            }

            if imbe.current_talkgroup.load(Ordering::Relaxed) == 0 {
                continue;
            }

            if !status.nid_valid {
                if total_polls % 64 == 0 || total_polls < 16 {
                    tracing::debug!(
                        target: "p25_traffic_lsm",
                        "{lane} NID event with nid_valid=false n_errors={} sync_dist={}",
                        status.n_errors, status.sync_distance,
                    );
                }
                continue;
            }

            let now = std::time::Instant::now();
            let locked_tg_snapshot = {
                let mut mgr = mgr.lock().await;
                match duid {
                    0x0 => mgr.hdu_received(now, nac),
                    0x5 => mgr.ldu_received(now, nac, false),
                    0xA => mgr.ldu_received(now, nac, true),
                    0x3 | 0xF => {
                        mgr.tdus_seen += 1;
                        mgr.last_duid = Some(duid);
                        mgr.last_nac = Some(nac);
                    }
                    _ => {
                        mgr.last_duid = Some(duid);
                        mgr.last_nac = Some(nac);
                    }
                }
                mgr.current_talkgroup().map(|t| t.0).unwrap_or(0)
            };

            imbe.last_observed_nac.store(nac, Ordering::Relaxed);
            if locked_tg_snapshot != 0 && duid == 0x0 {
                let _ = boundary_tx.send(audio::CallBoundary {
                    kind: audio::CallBoundaryKind::HduStart,
                    nac,
                    talkgroup: Some(locked_tg_snapshot),
                    expected_submit_count: imbe.frames_submitted.load(Ordering::Relaxed),
                    lane: Some(lane),
                });
            }

            let duid_label = match duid {
                0x0 => "HDU",
                0x3 => "TDU",
                0x5 => "LDU1",
                0xA => "LDU2",
                0xF => "TDU_LC",
                _ => "DUID?",
            };
            if locked_tg_snapshot != 0 && duid_label != "DUID?" {
                event_log.push(
                    LogCategory::Voice,
                    format!("{} TG={} NAC=0x{:03X}", duid_label, locked_tg_snapshot, nac),
                    serde_json::json!({
                        "duid":  duid_label,
                        "tg":    locked_tg_snapshot,
                        "nac":   nac,
                        "chain": lane.label(),
                    }),
                );
            }

            {
                let mgr = mgr.lock().await;
                let tg = mgr.current_talkgroup()
                    .map(|t| format!("TG:{:05}", t.0))
                    .unwrap_or_else(|| "--".into());
                let ch = mgr.current_channel()
                    .map(|c| format!("{}", c))
                    .unwrap_or_else(|| "--".into());
                let now_str = {
                    let d = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default();
                    let total_secs = d.as_secs();
                    let millis = d.subsec_millis();
                    let h = (total_secs / 3600) % 24;
                    let m = (total_secs / 60) % 60;
                    let s = total_secs % 60;
                    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, millis)
                };
                let evt = serde_json::json!({
                    "timestamp": now_str,
                    "event_type": format!("TRF_{}", duid_label),
                    "summary": format!("{} NAC:0x{:03X} {} CH:{}", duid_label, nac, tg, ch),
                    "talkgroup": mgr.current_talkgroup().map(|t| t.0),
                    "channel": ch,
                    "chain": lane.label(),
                });
                if let Ok(json) = serde_json::to_string(&evt) {
                    let _ = event_tx.send(json);
                }
            }

            if nid_events <= 10 || nid_events % 50 == 0 {
                let mgr = mgr.lock().await;
                tracing::info!(
                    target: "p25_traffic_lsm",
                    "{lane} NID #{nid_events}: NAC=0x{:03X} DUID=0x{:X} \
                     (hdus={} ldus={} tdus={} state={})",
                    nac, duid,
                    mgr.hdus_seen, mgr.ldus_seen, mgr.tdus_seen,
                    mgr.state_label(),
                );
            }
        }
    });
}

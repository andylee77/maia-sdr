//! Dibit reader tasks — DMA waiters that feed the P25 framers.
//!
//! Linux-only: both readers depend on `fpga::IpCore` and the DMA
//! ring accessors. Gated with `#![cfg(target_os = "linux")]` so the
//! Windows `cargo check` path still works.
//!
//! ## Phase 10.8 trim (2026-04-23)
//!
//! The PS C4FM control + traffic readers were retired along with the
//! HDL C4FM chain. Only the HDL LSM readers remain.
//!
//! - `spawn_hdl_lsm_control_reader` — HDL LSM reader that feeds the
//!   LSM control-channel decoder.
//! - `spawn_hdl_lsm_traffic_reader` — HDL LSM reader on the traffic
//!   chain with an `ImbeForwarder`-gated framer enable.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::hardware::fpga;
use crate::protocol::p25::control_channel::ControlChannelDecoder;

/// Zero-copy reinterpretation of a byte buffer as packed u64 dibit
/// words. Caller guarantees the buffer length is a multiple of 8.
fn bytemuck_cast(buffer: &[u8]) -> &[u64] {
    unsafe {
        std::slice::from_raw_parts(
            buffer.as_ptr() as *const u64,
            buffer.len() / 8,
        )
    }
}

pub fn spawn_hdl_lsm_control_reader(
    lsm_dibit_waiter: fpga::InterruptWaiter,
    lsm_dibit_core: Arc<Mutex<fpga::IpCore>>,
    lsm_dibit_decoder: Arc<RwLock<ControlChannelDecoder>>,
) {
        tokio::spawn(async move {
            tracing::info!(
                "HDL LSM dibit reader + TSBK decoder task started (Phase 6E)"
            );
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            let mut hist = [0u64; 4];
            loop {
                lsm_dibit_waiter.wait().await;
                wakeups += 1;
                let buffers = {
                    let mut core = lsm_dibit_core.lock().await;
                    core.read_lsm_dibit_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect::<Vec<_>>()
                };

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    // Histogram for bring-up diagnostics + feed into
                    // the LSM-side ControlChannelDecoder. The decoder
                    // does its own frame sync / NID / TSU / trellis /
                    // CRC chain internally, and emits TsbkMessage
                    // events on the shared WebSocket broadcast when
                    // a CRC-valid TSBK lands.
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                    let mut dec = lsm_dibit_decoder.write().await;
                    for &word in words {
                        dec.process_dma_word(word);
                    }
                }
                total_buffers += buffers.len() as u64;
                total_bytes += wake_bytes as u64;

                if wakeups <= 5 || wakeups % 16 == 0 {
                    let total_dibits: u64 = hist.iter().sum();
                    let pct = |v: u64| -> f64 {
                        if total_dibits == 0 { 0.0 }
                        else { 100.0 * v as f64 / total_dibits as f64 }
                    };
                    // Snapshot the LSM decoder's cumulative sync /
                    // near-miss / recent-messages counters so we can
                    // see in the log whether the LSM dibit stream is
                    // actually producing frame sync hits (the single
                    // most important bring-up signal).
                    let (lsm_sync_hits, lsm_near, lsm_best, lsm_msg_count) = {
                        let d = lsm_dibit_decoder.read().await;
                        (
                            d.sync_hits(),
                            d.sync_near_misses(),
                            d.best_sync_distance(),
                            d.recent_messages.len(),
                        )
                    };
                    tracing::info!(
                        target: "p25_hdl_lsm",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                         | LSM decoder: sync_hits={lsm_sync_hits} \
                         near={lsm_near} best_dist={} recent_msgs={lsm_msg_count}",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                        if lsm_best == u32::MAX { 99 } else { lsm_best },
                    );
                }
            }
        });
}

/// 2026-05-03 dual-DDC: traffic-side LSM dibit reader hanging off
/// `m_axi_traffic_lsm_dibit`. Mirror of the control-chain reader:
/// dedicated `traffic_ddc` → LsmDecimator2 → LPF → RRC → LsmDemod →
/// DibitPacker → DMA. PS-side parsing identical to control side; gate
/// framer dispatch on `current_talkgroup != 0` so noise dibits don't
/// drive false NID events between calls.
pub fn spawn_hdl_lsm_traffic_reader(
    traffic_lsm_dibit_waiter: fpga::InterruptWaiter,
    traffic_lsm_core: Arc<Mutex<fpga::IpCore>>,
    traffic_lsm_decoder_task: Arc<RwLock<ControlChannelDecoder>>,
    traffic_reader_imbe: Arc<ImbeForwarder>,
) {
    tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        tracing::info!(
            "Traffic LSM dibit reader + voice frame decoder task \
             started (M2B)"
        );
        let mut wakeups: u64 = 0;
        let mut total_buffers: u64 = 0;
        let mut total_bytes: u64 = 0;
        let mut hist = [0u64; 4];
        loop {
            traffic_lsm_dibit_waiter.wait().await;
            wakeups += 1;
            let buffers = {
                let mut core = traffic_lsm_core.lock().await;
                core.read_traffic_lsm_dibit_buffers()
                    .iter()
                    .map(|b| b.to_vec())
                    .collect::<Vec<_>>()
            };

            // Idle gate: chain emits dibits even when no TG locked
            // (RRC + LsmDemod can't tell the difference between live
            // signal and traffic_ddc residual when no carrier is
            // tuned). Counters update either way so
            // `/api/traffic.stats` shows the raw rate.
            let locked = traffic_reader_imbe
                .current_talkgroup
                .load(Ordering::Relaxed) != 0;

            let mut wake_bytes = 0usize;
            let mut wake_dibits = 0usize;
            for buffer in &buffers {
                wake_bytes += buffer.len();
                let words: &[u64] = bytemuck_cast(buffer);
                for &word in words {
                    for i in 0..32 {
                        let d = ((word >> (i * 2)) & 0x03) as usize;
                        hist[d] += 1;
                        wake_dibits += 1;
                    }
                }
                if locked {
                    let mut dec = traffic_lsm_decoder_task.write().await;
                    for &word in words {
                        dec.process_dma_word(word);
                    }
                }
            }
            total_buffers += buffers.len() as u64;
            total_bytes += wake_bytes as u64;

            if wakeups <= 5 || wakeups % 16 == 0 {
                let total_dibits: u64 = hist.iter().sum();
                let pct = |v: u64| -> f64 {
                    if total_dibits == 0 { 0.0 }
                    else { 100.0 * v as f64 / total_dibits as f64 }
                };
                let (sync_hits, msg_count, ldu1, ldu2, hdu, tdu, tdu_lc) = {
                    let d = traffic_lsm_decoder_task.read().await;
                    (
                        d.sync_hits(),
                        d.recent_messages.len(),
                        d.ldu1_count,
                        d.ldu2_count,
                        d.hdu_count,
                        d.tdu_count,
                        d.tdu_lc_count,
                    )
                };
                tracing::info!(
                    target: "p25_traffic_lsm",
                    "wake #{wakeups}: bufs={} bytes={} dibits={} \
                     (cum bufs={total_buffers} bytes={total_bytes}) \
                     hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}% \
                     | traffic_lsm decoder: sync_hits={sync_hits} \
                     hdu={hdu} ldu1={ldu1} ldu2={ldu2} tdu={tdu} \
                     tdu_lc={tdu_lc} recent_msgs={msg_count}",
                    buffers.len(), wake_bytes, wake_dibits,
                    pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                );
            }
        }
    });
}

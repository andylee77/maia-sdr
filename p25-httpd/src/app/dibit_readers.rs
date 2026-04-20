//! Dibit reader tasks — DMA waiters that feed the P25 framers.
//!
//! Linux-only: all four readers depend on `fpga::IpCore` and the DMA
//! ring accessors. Gated with `#![cfg(target_os = "linux")]` so the
//! Windows `cargo check` path still works.
//!
//! - `spawn_ps_c4fm_control_reader` — original PS C4FM control reader.
//!   Honors the 2026-04-19 phantom-TSBK gate that skips
//!   `process_dma_word` when `active_modulation == LSM`.
//! - `spawn_hdl_lsm_control_reader` — Phase 6E HDL LSM reader that
//!   feeds the LSM control-channel decoder.
//! - `spawn_hdl_lsm_traffic_reader` — Phase 7C HDL LSM reader on the
//!   traffic chain with an `ImbeForwarder`-gated framer enable.
//! - `spawn_ps_c4fm_traffic_reader` — Phase 7A.1 traffic PS C4FM
//!   reader that keeps `TrafficStats` + `TrafficManager` fresh.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use tokio::sync::{Mutex, RwLock};

use crate::app::imbe_forwarder::ImbeForwarder;
use crate::hardware::fpga;
use crate::protocol::p25::control_channel::ControlChannelDecoder;
use crate::protocol::p25::traffic_manager::TrafficManager;

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

pub fn spawn_ps_c4fm_control_reader(
    mut dibit_waiter: fpga::InterruptWaiter,
    reader_core: Arc<Mutex<fpga::IpCore>>,
    reader_decoder: Arc<RwLock<ControlChannelDecoder>>,
    reader_active_mod: Arc<AtomicU8>,
) {
        // 6. Spawn dibit reader task
        let reader_core = ip_core.clone();
        let reader_decoder = decoder.clone();
        // 2026-04-19 phantom-TSBK fix. When active_modulation is LSM
        // (mode == 2, the default for simulcast sites) the PS C4FM
        // framer + trellis + CRC pipeline below produces nothing of
        // value: the control DDC carries an LSM signal and the C4FM
        // slicer produces random-ish dibits that occasionally pass
        // BCH/CRC with phantom NACs (observed 4348 CRC-OK phantom
        // TSBKs, system_nac=0xE21, in a 30-min boot probe). Those
        // phantoms get fed into `grant_event_tx` and can trigger
        // bogus retunes.
        //
        // We still drain the DMA buffers every wake so the ring
        // doesn't back up and the histogram keeps advancing, but we
        // skip `process_dma_word` (which runs the expensive framer)
        // when mode != C4FM. Mode is the u8 atomic at
        // `AppState::active_modulation`: 0=auto, 1=c4fm, 2=lsm.
        let reader_active_mod = active_modulation.clone();
        tokio::spawn(async move {
            tracing::info!("dibit reader task started");
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            // Cumulative dibit histogram across all reads
            let mut hist = [0u64; 4];
            let mut framer_skipped: u64 = 0;
            loop {
                dibit_waiter.wait().await;
                wakeups += 1;
                let buffers = {
                    let mut core = reader_core.lock().await;
                    core.read_dibit_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect::<Vec<_>>()
                };

                // Snapshot mode once per wake so the decision is
                // atomic across all buffers in this batch.
                let mode = reader_active_mod
                    .load(std::sync::atomic::Ordering::Relaxed);
                let feed_framer = mode == 0 /* auto */ || mode == 1 /* c4fm */;

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    // Tally dibit histogram for this batch
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                    if feed_framer {
                        let mut dec = reader_decoder.write().await;
                        for &word in words {
                            dec.process_dma_word(word);
                        }
                    } else {
                        framer_skipped += words.len() as u64;
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
                    let mode_label = match mode {
                        0 => "auto",
                        1 => "c4fm",
                        2 => "lsm-gate",
                        _ => "?",
                    };
                    tracing::info!(
                        target: "p25_reader",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         mode={mode_label} framer_skipped={framer_skipped} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    );
                }
            }
        });
}

pub fn spawn_hdl_lsm_control_reader(
    mut lsm_dibit_waiter: fpga::InterruptWaiter,
    lsm_dibit_core: Arc<Mutex<fpga::IpCore>>,
    lsm_dibit_decoder: Arc<RwLock<ControlChannelDecoder>>,
) {
        let lsm_dibit_core = ip_core.clone();
        let lsm_dibit_decoder = lsm_decoder.clone();
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

pub fn spawn_hdl_lsm_traffic_reader(
    mut traffic_lsm_dibit_waiter: fpga::InterruptWaiter,
    traffic_lsm_core: Arc<Mutex<fpga::IpCore>>,
    traffic_lsm_decoder_task: Arc<RwLock<ControlChannelDecoder>>,
    traffic_reader_imbe: Arc<ImbeForwarder>,
) {
        // Phase 7C: traffic-side LSM dibit reader + voice frame
        // decoder task. Mirrors the control-side LSM dibit reader
        // task immediately above (lines ~952-1032), feeding the
        // dibit stream into the new `traffic_lsm_decoder` instance
        // (which has the IMBE counter voice handler installed).
        //
        // Data flow:
        //   traffic_lsm_dibit_dma (DMA ring, ~3.4 sec sub-buffer fill)
        //     -> read_traffic_lsm_dibit_buffers (Vec<&[u8]>)
        //     -> bytemuck_cast (&[u64] of packed dibits)
        //     -> traffic_lsm_decoder.process_dma_word (Hunting ->
        //        ReadingNid -> ReadingDataUnit state machine)
        //     -> on_ldu1 / on_ldu2 / on_hdu / on_tdu / on_tdu_lc
        //        callbacks on the ImbeForwarder voice handler
        //     -> ImbeForwarder atomic counters incremented
        //     -> /api/traffic snapshot reads the atomics
        //
        // **Phase 7D will tap the same callback chain** to push raw
        // IMBE frames to the vocoder mpsc channel. Phase 7E will
        // wrap that with RTP audio output.
        //
        // The decoder runs the same state machine as the control
        // side -- it'll go through Hunting until a sync hit, decode
        // the NID, then dispatch by DUID. On a real call (voice
        // channel locked) we expect:
        //   - first event: HDU on call start
        //   - then 9-10x LDU1 / LDU2 alternation per second
        //   - final event: TDU or TDU_LC on call end
        let traffic_lsm_core = ip_core.clone();
        let traffic_lsm_decoder_task = traffic_lsm_decoder.clone();
        let traffic_reader_imbe = imbe_forwarder.clone();
        tokio::spawn(async move {
            use std::sync::atomic::Ordering;
            tracing::info!(
                "Traffic LSM dibit reader + voice frame decoder task \
                 started (Phase 7C)"
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

                // Phase 7F.5 (2026-04-14) root-cause gate: if the
                // follower is Idle (current_talkgroup == 0), the
                // traffic channel isn't "open" -- but the HDL LSM
                // chain is still producing dibits from whatever
                // frequency the NCO is pointed at. Previously the
                // software framer happily processed every dibit,
                // extracted noise-shaped LDUs / TDU_LCs via sync
                // correlator false positives, dispatched them to
                // the WS activity feed + event log + vocoder, and
                // the dashboard read as "traffic channel active,
                // playing packets" when there was no real call.
                //
                // Drain the DMA ring (already done above by the
                // `read_traffic_lsm_dibit_buffers` call so the
                // hardware doesn't overflow) but DO NOT feed the
                // dibits to the framer. Counters and hist still
                // update so we can see raw dibit rate / histogram
                // via /api/traffic.stats even during Idle.
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

pub fn spawn_ps_c4fm_traffic_reader(
    mut traffic_dibit_waiter: fpga::InterruptWaiter,
    traffic_reader_core: Arc<Mutex<fpga::IpCore>>,
    traffic_reader_stats: Arc<Mutex<crate::TrafficStats>>,
    traffic_reader_mgr: Arc<Mutex<TrafficManager>>,
) {
        // Phase 7A.1 (a): traffic dibit reader task. Wakes on every
        // traffic_dma sub-buffer interrupt, drains the ring via
        // `read_traffic_buffers()`, counts dibits + maintains a per-dibit
        // histogram, and updates the shared `TrafficStats`. Does NOT
        // feed a decoder -- the C4FM chain produces garbage on LSM
        // voice channels. The histogram alone is enough to confirm
        // "the chain is alive": a dead chain produces all-zero dibits,
        // a live chain produces an even-ish spread across {0,1,2,3}
        // (LSM through a C4FM slicer looks essentially random).
        //
        // Mirrors the control-channel dibit reader at line ~358 above
        // but against the traffic_dma ring + traffic stats sink. Bumps
        // `note_activity()` on the TrafficManager whenever new bytes
        // arrive so the call_timeout_ms inactivity detector resets.
        let traffic_reader_core = ip_core.clone();
        let traffic_reader_stats = traffic_stats.clone();
        let traffic_reader_mgr = traffic_manager.clone();
        tokio::spawn(async move {
            tracing::info!("traffic dibit reader task started (Phase 7A.1)");
            loop {
                traffic_dibit_waiter.wait().await;
                // Snapshot under the lock, then drop it before CPU work.
                let buffers: Vec<Vec<u8>> = {
                    let mut core = traffic_reader_core.lock().await;
                    core.read_traffic_buffers()
                        .iter()
                        .map(|b| b.to_vec())
                        .collect()
                };
                if buffers.is_empty() {
                    continue;
                }

                let mut wake_bytes = 0usize;
                let mut wake_dibits = 0usize;
                let mut wake_hist = [0u64; 4];
                for buffer in &buffers {
                    wake_bytes += buffer.len();
                    let words: &[u64] = bytemuck_cast(buffer);
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            wake_hist[d] += 1;
                            wake_dibits += 1;
                        }
                    }
                }

                {
                    let mut s = traffic_reader_stats.lock().await;
                    let now = std::time::Instant::now();
                    if s.started_at.is_none() {
                        s.started_at = Some(now);
                    }
                    s.last_at = Some(now);
                    s.wakeups += 1;
                    s.total_buffers += buffers.len() as u64;
                    s.total_bytes += wake_bytes as u64;
                    s.total_dibits += wake_dibits as u64;
                    for i in 0..4 {
                        s.dibit_hist[i] += wake_hist[i];
                    }
                    // Use plain modulo, not `is_multiple_of` -- the
                    // latter is unstable (`int_roundings` feature gate)
                    // and the Tezuka Buildroot Rust toolchain is older
                    // stable.
                    let log_now = s.wakeups <= 5 || s.wakeups % 64 == 0;
                    if log_now {
                        let total_d: u64 = s.dibit_hist.iter().sum();
                        let pct = |v: u64| -> f64 {
                            if total_d == 0 {
                                0.0
                            } else {
                                100.0 * v as f64 / total_d as f64
                            }
                        };
                        tracing::info!(
                            target: "p25_traffic",
                            "wake #{}: bufs={} bytes={} dibits={} \
                             (cum bufs={} bytes={} dibits={}) \
                             hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                            s.wakeups, buffers.len(), wake_bytes, wake_dibits,
                            s.total_buffers, s.total_bytes, s.total_dibits,
                            pct(s.dibit_hist[0]), pct(s.dibit_hist[1]),
                            pct(s.dibit_hist[2]), pct(s.dibit_hist[3]),
                        );
                    }
                }

                // Pet the TrafficManager so its 3-second
                // call-inactivity timeout doesn't fire while real bytes
                // are still arriving.
                {
                    let mut mgr = traffic_reader_mgr.lock().await;
                    mgr.note_activity();
                }
            }
        });
}


//! Live software P25 demodulator task (Stage 2B).
//!
//! Receives Complex32 chunks teed from `wideband_iq_task`, runs them
//! through `MultistageDdc` → `LsmPipeline` → `ControlChannelDecoder`
//! framer. The framer's voice handler is the existing `ImbeForwarder`,
//! so audio comes out the same `audio_tx` channel the HDL path uses.
//!
//! Linux-only: depends on `fpga::IpCore` for HDL chain idle and on
//! the wideband_iq_dma waiter inside `wideband_iq_task`.

#![cfg(target_os = "linux")]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Instant;

use tokio::sync::{mpsc, Mutex, RwLock};

use crate::hardware::fpga;
use crate::lsm::{Complex32, LsmPipeline};
use crate::protocol::p25::control_channel::ControlChannelDecoder;
use crate::protocol::p25::traffic_chain::TrafficChain;
use crate::sw_demod::MultistageDdc;

/// Wideband IQ tap rate from `rxiq_cdc` (8 MSPS).
const WIDEBAND_IQ_RATE_HZ: f64 = 8_000_000.0;
/// Output rate of the software DDC, matching what `LsmPipeline::process_iq`
/// expects natively. 2026-05-03: dropped from 62.5 kSPS → **25 kSPS** to
/// match SDRTrunk's effective decimation rate. MultistageDdc factorises
/// 8 MSPS / 25 kSPS = 320 = 8×8×5 (with the /5 stage added in the same
/// commit). The LsmPipeline no longer does an internal /2 — input is the
/// final operating rate.
const DDC_OUT_RATE_HZ: f64 = 25_000.0;

/// Cumulative runtime stats for the live software demod task. Mirrors
/// the HDL-side `traffic_stats` shape — exposed via a future
/// `/api/sw_demod` endpoint, useful for "is the live path actually
/// firing?" diagnosis.
#[derive(Debug, Default)]
pub struct SwDemodStats {
    /// `std::sync::Mutex` (cheap) so the task can `Arc::clone` and
    /// still mutate. Held only briefly (single-line update); never
    /// across `.await` so it can't deadlock with tokio.
    pub started_at: StdMutex<Option<Instant>>,
    pub last_chunk_at: StdMutex<Option<Instant>>,
    pub chunks_in: AtomicU64,
    pub samples_in: AtomicU64,
    pub samples_out_62k5: AtomicU64,
    pub dibits_emitted: AtomicU64,
    pub framer_dispatches: AtomicU64,
    pub retunes: AtomicU64,
    /// Most recent NCO offset (Hz) the DDC was set to.
    pub nco_offset_hz: std::sync::atomic::AtomicI64,
}

/// Spawn the live software demodulator task.
///
/// `rx`        — Complex32 chunks teed from `wideband_iq_task`.
/// `enabled`   — runtime gate. When false the task drains chunks but
///               does NOT feed the framer, so HDL stays in charge.
/// `core`      — for `set_traffic_lsm_enable(false)` when SW takes
///               over (avoids HDL co-producing dibits into the same
///               decoder).
/// `traffic_chain` — source of the current grant frequency (NCO target).
/// `traffic_lsm_decoder` — shared framer; its voice handler is already
///               wired to `ImbeForwarder` → vocoder → audio_tx.
/// `current_rx_lo` / `current_lo_shift_hz` — for computing the NCO offset
///               (= grant_freq - rx_lo + lo_shift_hz).
pub fn spawn_sw_demod(
    mut rx: mpsc::Receiver<Vec<Complex32>>,
    enabled: Arc<AtomicBool>,
    core: Arc<Mutex<fpga::IpCore>>,
    traffic_chain: Arc<Mutex<TrafficChain>>,
    traffic_lsm_decoder: Arc<RwLock<ControlChannelDecoder>>,
    current_rx_lo: Arc<AtomicI64>,
    current_lo_shift_hz: Arc<AtomicI64>,
    stats: Arc<SwDemodStats>,
) {
    tokio::spawn(async move {
        tracing::info!(
            target: "p25_sw_demod",
            "live software demod task started (8 MSPS → 62.5 kSPS DDC + LsmPipeline)"
        );

        // DDC starts with NCO=0 (no target). LsmPipeline starts in
        // hunting state. Framer is the shared `traffic_lsm_decoder` —
        // we only feed it when both `enabled` is true AND a grant
        // frequency is published.
        //
        // 2026-05-03: MultistageDdc, not the single-stage 65-tap path.
        // Offline A/B on Clay County 1777702789_859187843_4000000.wav
        // showed 858.4375 IMBE 198→495 + LDU2 6→26 (the single-stage
        // LPF leaked a strong neighbour at 25 kHz, breaking sync mid-
        // frame). Multistage final stage rejects 6.25 kHz at 60 dB,
        // costs ~70–135 Mmul/sec extra per channel — well within
        // single-chain scanner budget on Cortex-A9.
        let mut ddc = MultistageDdc::new(
            WIDEBAND_IQ_RATE_HZ,
            DDC_OUT_RATE_HZ,
            0.0,
        );
        let mut pipeline = LsmPipeline::new();
        let mut last_known_freq: Option<u64> = None;
        // Track the last freq we ACTUALLY programmed the NCO to. Stays
        // Some(f) across idle gaps so a new call on the same freq
        // doesn't trigger a pipeline reset. Mirrors SDRTrunk's
        // "channel allocation persists across PTT bursts" model: the
        // Costas/Gardner/AGC stay locked from the previous call,
        // dramatically reducing cold-start acquisition failures.
        let mut last_nco_target_hz: Option<u64> = None;
        let mut last_known_enabled: bool = !enabled.load(Ordering::Relaxed);

        // ─── Initial HDL idle ─────────────────────────────────────────
        // When sw_demod is the active source we MUST stop the HDL
        // traffic LSM chain from also feeding `traffic_lsm_decoder`,
        // otherwise the two dibit streams interleave and the framer
        // resyncs on garbage. Set traffic_lsm_enable=0 at boot if
        // we're starting in SW mode.
        if enabled.load(Ordering::Relaxed) {
            let core_lock = core.lock().await;
            core_lock.set_traffic_lsm_enable(false);
            drop(core_lock);
            tracing::info!(
                target: "p25_sw_demod",
                "boot: HDL traffic LSM idled (sw_demod_enabled=true)"
            );
        }

        let mut last_log = Instant::now();
        let mut local_chunks: u64 = 0;
        let mut local_samples_out: u64 = 0;
        let mut local_dibits: u64 = 0;

        loop {
            let Some(chunk) = rx.recv().await else {
                tracing::warn!(
                    target: "p25_sw_demod",
                    "channel closed; sw_demod task exiting"
                );
                return;
            };
            let now = Instant::now();
            stats.chunks_in.fetch_add(1, Ordering::Relaxed);
            stats.samples_in.fetch_add(chunk.len() as u64, Ordering::Relaxed);
            // Update last_chunk_at IMMEDIATELY on receive (not after
            // processing) so /api/sw_demod's `last_chunk_secs_ago`
            // reflects channel liveness, not whether the chunk
            // happened to fall inside an active call window.
            *stats.last_chunk_at.lock().unwrap() = Some(now);
            if stats.started_at.lock().unwrap().is_none() {
                *stats.started_at.lock().unwrap() = Some(now);
            }

            // ─── Runtime enable toggle ────────────────────────────────
            let enabled_now = enabled.load(Ordering::Relaxed);
            if enabled_now != last_known_enabled {
                let core_lock = core.lock().await;
                core_lock.set_traffic_lsm_enable(!enabled_now);
                drop(core_lock);
                tracing::info!(
                    target: "p25_sw_demod",
                    "enable toggle: sw_demod_enabled={enabled_now}, \
                     HDL traffic LSM enable={}",
                    !enabled_now
                );
                last_known_enabled = enabled_now;
                // Reset SW state on every transition so we start clean.
                ddc.reset();
                pipeline.reset();
                {
                    let mut d = traffic_lsm_decoder.write().await;
                    d.reset_framer_state();
                }
                last_known_freq = None;
                last_nco_target_hz = None;
            }

            // If disabled, drop the chunk on the floor.
            if !enabled_now {
                continue;
            }

            // ─── Track grant freq → DDC retune ────────────────────────
            let chain_freq = {
                let chain = traffic_chain.lock().await;
                chain.current_frequency()
            };
            if chain_freq != last_known_freq {
                if let Some(f) = chain_freq {
                    if last_nco_target_hz == Some(f) {
                        // SAME-FREQ resume — a new call started on the
                        // freq the DDC is already tuned to. Costas /
                        // Gardner / AGC stay locked from the previous
                        // call (or the inter-call idle period); just
                        // reset the framer's dibit position counter
                        // so it re-syncs cleanly on the new HDU.
                        // SDRTrunk-style "channel reuse across PTT".
                        {
                            let mut d = traffic_lsm_decoder.write().await;
                            d.reset_framer_state();
                        }
                        tracing::info!(
                            target: "p25_sw_demod",
                            "same-freq resume: grant {} Hz \
                             (no NCO/pipeline reset; framer rearmed)", f
                        );
                    } else {
                        // Real freq change — full retune.
                        let rx_lo = current_rx_lo.load(Ordering::Relaxed) as f64;
                        let lo_shift = current_lo_shift_hz.load(Ordering::Relaxed) as f64;
                        let nco_offset = (f as f64) - rx_lo + lo_shift;
                        ddc.set_nco_offset(nco_offset);
                        pipeline.reset();
                        {
                            let mut d = traffic_lsm_decoder.write().await;
                            d.reset_framer_state();
                        }
                        stats.retunes.fetch_add(1, Ordering::Relaxed);
                        stats.nco_offset_hz.store(
                            nco_offset.round() as i64,
                            Ordering::Relaxed,
                        );
                        last_nco_target_hz = Some(f);
                        tracing::info!(
                            target: "p25_sw_demod",
                            "retune: grant {} Hz, rx_lo {} Hz, lo_shift {} Hz \
                             → NCO {:+.0} Hz",
                            f, rx_lo as i64, lo_shift as i64, nco_offset,
                        );
                    }
                } else {
                    // Grant cleared. Don't touch DDC, don't reset
                    // pipeline — keep Costas/Gardner/AGC locked so the
                    // next call on this freq starts pre-warmed.
                    tracing::debug!(
                        target: "p25_sw_demod", "grant cleared"
                    );
                }
                last_known_freq = chain_freq;
            }

            // ─── Idle gate ────────────────────────────────────────────
            // 2026-05-03: when no grant is active, drop the chunk on
            // the floor instead of burning the full 8 MSPS DSP path on
            // samples we'd discard anyway. Without this gate the DDC +
            // LPF + LsmPipeline ran 100 % of the time and pegged both
            // Cortex-A9 cores between calls. With the gate, idle CPU
            // is near-zero and active CPU rises only while a call is
            // in progress.
            if last_known_freq.is_none() {
                continue;
            }

            // ─── DDC → LsmPipeline → framer ──────────────────────────
            let iq_62k5 = ddc.process(&chunk);
            local_samples_out += iq_62k5.len() as u64;
            stats.samples_out_62k5.fetch_add(iq_62k5.len() as u64, Ordering::Relaxed);
            if iq_62k5.is_empty() {
                continue;
            }

            let batch = pipeline.process_iq(&iq_62k5);
            let dibits = &batch.demod.hard_dibits;
            local_dibits += dibits.len() as u64;
            stats.dibits_emitted.fetch_add(dibits.len() as u64, Ordering::Relaxed);

            if !dibits.is_empty() {
                let mut d = traffic_lsm_decoder.write().await;
                for &dibit in dibits {
                    d.process_dibit(dibit);
                }
                stats.framer_dispatches.fetch_add(1, Ordering::Relaxed);
            }

            local_chunks += 1;

            // ─── Periodic stats ───────────────────────────────────────
            if last_log.elapsed().as_secs_f64() >= 5.0 {
                let elapsed = last_log.elapsed().as_secs_f64();
                tracing::info!(
                    target: "p25_sw_demod",
                    "chunks={local_chunks} samples_out={local_samples_out} \
                     dibits={local_dibits} freq={:?} retunes={}",
                    last_known_freq,
                    stats.retunes.load(Ordering::Relaxed),
                );
                let _ = elapsed; // (rate calc handy in future logs)
                last_log = Instant::now();
                local_chunks = 0;
                local_samples_out = 0;
                local_dibits = 0;
            }
        }
    });
}

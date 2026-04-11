//! Fishball P25 Trunking Radio - PS Application
//!
//! Runs on the Zynq-7020 ARM cores. Responsibilities:
//! - Configure AD9361 via IIO
//! - Configure FPGA DDC + demod via UIO registers
//! - Read dibit DMA stream from FPGA
//! - Decode P25 control channel (sync, TSBK parsing, state machine)
//! - Serve web UI for monitoring talkgroups and grants

use std::sync::Arc;

use clap::Parser;
use tokio::sync::{broadcast, RwLock};

#[cfg(target_os = "linux")]
mod fpga;
mod httpd;
#[cfg(target_os = "linux")]
mod iio;
mod lsm;
mod p25;
#[cfg(target_os = "linux")]
mod rxbuffer;
#[cfg(target_os = "linux")]
mod uio;

use p25::control_channel::ControlChannelDecoder;

/// Build tag, logged at startup and exposed via `/api/system`.
///
/// **Bump this string whenever a feature flag changes** so on-target
/// "is the binary I just flashed actually the one I just built?" is a
/// trivial check (`grep "p25-httpd build" /var/log/p25-httpd.log` or
/// `wget -qO- http://target:8080/api/system | grep build`). Don't try
/// to be clever with mtimes (Buildroot zeros them) or doc-comment
/// strings (they don't survive into the binary).
pub const BUILD_TAG: &str = "2026-04-11-phase6g.1-hdl-dc-blocker-and-tg-grant-dedup";

/// Cumulative + snapshot stats for the HDL LSM chain (Phase 6E PL
/// gateware). Populated by the HDL LSM heartbeat task and read by
/// `/api/hdl_lsm`. Single source of truth for everything the
/// heartbeat task used to keep in task-local variables.
#[derive(Debug, Clone, Default)]
pub struct HdlLsmRuntime {
    pub started_at: Option<std::time::Instant>,
    pub last_tick_at: Option<std::time::Instant>,

    // Last raw register snapshot (read every 16 ms).
    pub pll_dbg: i16,
    pub sp_dbg: i16,
    pub sync_distance: u8,
    pub bch_busy: bool,
    pub in_nid_window: bool,
    pub dibit_overflow_latched: bool,
    pub iq_overflow_latched: bool,
    pub last_nac: u16,
    pub last_duid: u8,
    pub last_drop_count: u16,

    // Last successful NID event.
    pub last_nid_at: Option<std::time::Instant>,
    pub last_nid_valid: bool,
    pub last_nid_n_errors: u8,

    // Cumulative counters since boot.
    pub total_nid_events: u64,
    pub valid_nid_events: u64,
    pub dibit_overflow_ticks: u64,
    pub iq_overflow_ticks: u64,

    // NAC histogram across all NID events. Winner = locked site ID.
    pub nac_hist: std::collections::HashMap<u16, u64>,

    // Last completed heartbeat window snapshot (1 s cadence).
    pub hb_pll_min: i16,
    pub hb_pll_max: i16,
    pub hb_sp_min: i16,
    pub hb_sp_max: i16,
    pub hb_sync_dist_best: u8,
    pub hb_bch_busy_ticks: u32,
    pub hb_in_window_ticks: u32,
    pub hb_nid_event_ticks: u32,
    pub hb_dibit_overflow_ticks: u32,
    pub hb_iq_overflow_ticks: u32,
    pub hb_iq_kbps: u64,
    pub hb_iq_buf_rolls: u32,
    pub hb_window_valid_count: u32,
    pub hb_window_event_count: u32,

    // Last 32 NID events (chronological, oldest first after wrap).
    pub nid_ring: Vec<HdlNidEntry>,
}

#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct HdlNidEntry {
    pub seq: u64,
    pub t_ms_since_boot: u64,
    pub nac: u16,
    pub duid: u8,
    pub valid: bool,
    pub n_errors: u8,
    pub sync_distance: u8,
    pub drop_count: u16,
    pub pll_dbg: i16,
    pub sp_dbg: i16,
}

impl HdlLsmRuntime {
    pub fn top_nacs(&self, n: usize) -> Vec<(u16, u64)> {
        let mut v: Vec<(u16, u64)> = self
            .nac_hist
            .iter()
            .map(|(k, v)| (*k, *v))
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v.truncate(n);
        v
    }
}

/// Per-source IRQ counters maintained by the InterruptHandler task.
#[derive(Debug, Clone, Copy, Default)]
pub struct IrqStats {
    pub total: u64,
    pub dibit: u64,
    pub traffic: u64,
    pub iq: u64,
    pub lsm_dibit: u64,
    pub last_at_secs_ago: f64,
    /// Set to None until the first IRQ; updated only by the IRQ task.
    pub started_at: Option<std::time::Instant>,
    pub last_at: Option<std::time::Instant>,
}

#[derive(Parser)]
#[command(name = "p25-httpd", about = "Fishball P25 Trunking Radio")]
struct Args {
    /// HTTP listen address
    #[arg(long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// AD9361 RX LO frequency in Hz
    #[arg(long, default_value_t = 858_100_000)]
    rx_lo: u64,

    /// AD9361 sample rate in Hz
    #[arg(long, default_value_t = 8_000_000)]
    sample_rate: u64,

    /// P25 control channel frequency in Hz
    #[arg(long, default_value_t = 860_962_500)]
    control_freq: u64,

    /// Pluto LO PPM offset for crystal calibration.
    ///
    /// Compensates the AD9361 crystal frequency error by shifting the
    /// DDC NCO (NOT the AD9361 LO request -- the LO synthesizer step
    /// at our operating range is much coarser than the typical PPM-
    /// scale shift, so a small LO shift gets rounded back to the
    /// nominal value while the NCO computation still moves, doubling
    /// the post-DDC offset and breaking lock. The DDC NCO is generated
    /// in fabric at 1 Hz precision and is the only place a sub-step
    /// shift can actually be applied).
    ///
    /// Negative ppm means the Pluto crystal is slow (real signals
    /// appear above their expected IF). For the Clay County test
    /// Pluto: -0.54 ppm. SDRTrunk's tuner panel exposes the same
    /// setting and is the reference for the value to use here.
    ///
    /// Math: nco_shift = -ppm * 1e-6 * rx_lo Hz. With rx_lo=858 MHz
    /// and ppm=-0.54, that is +463 Hz added to the nominal NCO.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    lo_ppm: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Default log level: info for our crate, warn for everything else.
    // Honour RUST_LOG when set, otherwise emit a sensible default so the
    // user actually sees the dibit reader / IRQ / decoder logs without
    // having to manually configure tracing.
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,p25_httpd=info"));
    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_level(true)
        .init();

    let args = Args::parse();

    // Build marker. Bump the BUILD_TAG string whenever a feature flag changes
    // so on-target verification of "is this binary the one I just built" is a
    // single grep instead of guessing from mtimes (Buildroot zeros mtimes to
    // 1970) or doc-comment strings (which don't survive into the binary).
    //
    // BUILD_TAG also lands in /api/system as the `build` field so the browser
    // can show the deployed build at a glance.
    tracing::info!(
        "p25-httpd build: {} (dashboard_source=lsm_decoder, Phase 6F.1)",
        BUILD_TAG
    );

    // Pluto crystal calibration: shift the DDC NCO by -ppm * 1e-6 * rx_lo Hz.
    // See the doc comment on Args::lo_ppm for why this only moves the NCO and
    // not the LO request. nco_lo_shift_hz is folded into nco_offset further
    // down inside the cfg(linux) block where the IP core gets configured.
    let nco_lo_shift_hz = -args.lo_ppm * 1e-6 * args.rx_lo as f64;

    tracing::info!(
        "Fishball P25 starting: RX LO={} Hz, control_freq={} Hz, lo_ppm={:+} ({:+.1} Hz NCO shift)",
        args.rx_lo,
        args.control_freq,
        args.lo_ppm,
        nco_lo_shift_hz
    );

    let (event_tx, _) = broadcast::channel::<String>(256);
    let mut decoder = ControlChannelDecoder::new();
    decoder.set_event_tx(event_tx.clone());
    let decoder = Arc::new(RwLock::new(decoder));

    // Phase 6E.10 bring-up: a second independent `ControlChannelDecoder`
    // instance fed by the HDL LSM dibit stream from `lsm_dibit_dma`.
    // Runs the identical Hunting -> ReadingNid -> ReadingDu -> trellis
    // -> CRC -> TsbkMessage pipeline as the C4FM decoder above, just
    // against a different dibit source. Shares the same event_tx
    // broadcast channel so both decoders' TSBKs land on the same
    // dashboard WebSocket (with distinct trace targets in the server
    // log so they can be separated post-hoc). Both instances operate
    // in parallel against the same RF capture on bring-up days -- this
    // is how we validate the HDL LSM port (Phase 6E.0-6E.9) against the
    // working Phase 2A C4FM path.
    let mut lsm_decoder = ControlChannelDecoder::new();
    lsm_decoder.set_event_tx(event_tx.clone());
    let lsm_decoder = Arc::new(RwLock::new(lsm_decoder));

    // Phase 6F.9: third parallel `ControlChannelDecoder` driven by the
    // Phase 6D `LsmPipeline` running on RAW IQ. Soft-decision sync
    // events from `find_sync_events_soft` get dispatched into
    // `process_directed_tsdu` to bypass the Hunting state machine
    // entirely. The hope is to capture the syncs the dibit-domain hard
    // correlator misses (~9/sec soft vs ~5/sec hard) and turn them
    // into TSBKs through the same parser path.
    let mut iq_lsm_decoder = ControlChannelDecoder::new();
    iq_lsm_decoder.set_event_tx(event_tx.clone());
    let iq_lsm_decoder = Arc::new(RwLock::new(iq_lsm_decoder));

    // Phase 6D dashboard wiring: shared LsmStats mutex, populated by the
    // LSM IRQ task below and read by the /api/lsm handler. Kept out of
    // the cfg(linux) block so non-Linux builds still expose the (empty)
    // endpoint -- useful for host-side cargo test of httpd routing.
    let lsm_stats = Arc::new(tokio::sync::Mutex::new(lsm::LsmStats::default()));

    // Phase 6F.2: shared PL HDL LSM runtime + IRQ stats, populated by
    // their respective tasks below and read by /api/hdl_lsm and
    // /api/irq_stats. Same out-of-cfg(linux) treatment.
    let hdl_lsm = Arc::new(tokio::sync::Mutex::new(HdlLsmRuntime::default()));
    let irq_stats = Arc::new(tokio::sync::Mutex::new(IrqStats::default()));

    #[cfg(target_os = "linux")]
    let (ip_core, ad9361) = {
        use tokio::sync::Mutex;

        // 1. Initialize FPGA IP core via UIO
        let (ip_core, interrupt_handler) = fpga::IpCore::take().await?;
        tracing::info!("FPGA IP core initialized");

        // 2. Configure AD9361 via IIO
        let ad9361 = iio::Ad9361::new().await?;
        ad9361.set_rx_lo_frequency(args.rx_lo).await?;
        ad9361
            .set_sampling_frequency(args.sample_rate as u32)
            .await?;
        ad9361.set_rx_rf_bandwidth(5_000_000).await?;
        ad9361
            .set_rx_gain_mode(iio::GainMode::SlowAttack)
            .await?;
        tracing::info!(
            "AD9361 configured: LO={} Hz, Fs={} Hz, BW=5 MHz, AGC=slow_attack",
            args.rx_lo,
            args.sample_rate
        );

        // 3. Configure control channel DDC (FIR filters + decimation + NCO).
        // The lo_ppm crystal calibration is folded into the NCO here -- see
        // the doc comment on Args::lo_ppm for the rationale and the math.
        let nco_offset =
            args.control_freq as f64 - args.rx_lo as f64 + nco_lo_shift_hz;
        ip_core.configure_ddc(nco_offset, args.sample_rate as f64)?;
        ip_core.set_ddc_enable(true);
        // Ring DMA: enable bit is level-triggered, starts continuous writes
        ip_core.set_demod_enable(true);
        // Phase 6D: also enable the post-DDC IQ ring DMA so the LSM task
        // can pull raw 62.5 kSPS IQ samples in parallel with the dibit
        // pipeline. Both rings share the control DDC's output.
        ip_core.set_iq_dma_enable(true);
        // Phase 6E.9/6E.10: enable the HDL LSM demod chain (runs alongside
        // the C4FM demod on the same control DDC output) and its dedicated
        // dibit ring DMA. NID events themselves are PS-polled via
        // lsm_status below.
        // Phase 6G.1: also turn on the front-end DC blocker -- this is
        // the production-correct state and removes the slow IQ DC bias
        // that otherwise gives the slicer a 60/40 inner/outer dibit
        // ratio for 2-3 minutes after PLL start. See doc/changes/031.
        ip_core.set_lsm_enable(true);
        ip_core.set_lsm_dibit_dma_enable(true);
        ip_core.set_lsm_dc_block_enable(true);
        // Read back lsm_control to confirm the bits actually stuck in the
        // register bank. If the readback disagrees with what we wrote we
        // have a register-bank wiring bug (rare; would surface as obvious
        // garbage in lsm_status / lsm_nid downstream).
        let (lsm_en_rb, lsm_dma_en_rb, lsm_dc_block_rb) =
            ip_core.lsm_control_readback();
        tracing::info!(
            "Control DDC: offset={nco_offset} Hz, dibit + iq + lsm ring DMA enabled \
             (lsm_control readback: lsm_enable={lsm_en_rb}, \
             lsm_dibit_dma_enable={lsm_dma_en_rb}, \
             lsm_dc_block_enable={lsm_dc_block_rb})"
        );
        if !lsm_en_rb || !lsm_dma_en_rb {
            tracing::error!(
                "lsm_control readback mismatch -- expected lsm_enable=true and \
                 lsm_dibit_dma_enable=true, got ({lsm_en_rb},{lsm_dma_en_rb}); \
                 HDL LSM chain WILL NOT be active"
            );
        }
        if !lsm_dc_block_rb {
            tracing::warn!(
                "lsm_dc_block_enable readback is false -- expected true; \
                 LSM front-end DC blocker is NOT active and the PLL \
                 acquisition transient will be 2-3 minutes instead of \
                 a few seconds (Phase 6G.1)"
            );
        }

        let ip_core = Arc::new(Mutex::new(ip_core));
        let ad9361 = Arc::new(ad9361);

        // 4. Get interrupt waiters before spawning handler
        let dibit_waiter = interrupt_handler.waiter_dibit_dma();
        let iq_waiter = interrupt_handler.waiter_iq_dma();
        let lsm_dibit_waiter = interrupt_handler.waiter_lsm_dibit_dma();

        // 5. Spawn interrupt handler
        let irq_stats_for_handler = irq_stats.clone();
        tokio::spawn(async move {
            if let Err(e) = interrupt_handler.run(irq_stats_for_handler).await {
                tracing::error!("interrupt handler error: {e}");
            }
        });

        // 6. Spawn dibit reader task
        let reader_core = ip_core.clone();
        let reader_decoder = decoder.clone();
        tokio::spawn(async move {
            tracing::info!("dibit reader task started");
            let mut wakeups: u64 = 0;
            let mut total_buffers: u64 = 0;
            let mut total_bytes: u64 = 0;
            // Cumulative dibit histogram across all reads
            let mut hist = [0u64; 4];
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
                    let mut dec = reader_decoder.write().await;
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
                    tracing::info!(
                        target: "p25_reader",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    );
                }
            }
        });

        // 6b. Spawn Phase 6D LSM reader task. Pulls 62.5 kSPS post-DDC IQ
        //     from the iq_dma ring, runs the streaming LSM pipeline
        //     (decimate /2 -> LPF -> RRC -> AGC+PLL+Gardner+slicer ->
        //     hard+soft sync detectors -> BCH(63,16,11) FEC), logs
        //     per-IRQ NID accuracy stats, and updates the shared
        //     `LsmStats` that feeds `GET /api/lsm` on the dashboard.
        //     Runs in parallel with the dibit reader above; both consume
        //     the same control DDC output but via separate ring DMAs.
        let lsm_core = ip_core.clone();
        let lsm_stats_task = lsm_stats.clone();
        let iq_lsm_decoder_task = iq_lsm_decoder.clone();
        tokio::spawn(async move {
            tracing::info!("LSM IQ reader task started (Phase 6D)");
            let mut pipeline = lsm::LsmPipeline::new();
            // Phase 6F.10 cross-batch carry-over with pending event
            // queue. The Phase 6D LSM IRQ task wakes about every
            // ~17 ms (one IRQ per ~84 dibits), so a soft sync event
            // landing near the end of a batch will not have its full
            // 336-dibit (NID + TSBK1+TSBK2+TSBK3) body in the current
            // batch -- it needs to wait for the NEXT 1-3 batches.
            //
            // 6F.9 simply capped each dispatch at `combined.len()` so
            // late events only got a fragment of their body (often
            // just NID + TSBK1, dropping TSBK2/TSBK3). That pulled
            // `ps_iq_lsm` blocks/TSDU down to 1.86 instead of 3.0.
            //
            // 6F.10 tracks each event by its absolute stream position
            // and stashes it in a pending queue if the full body
            // isn't available yet. Every batch we walk the queue and
            // dispatch any event whose 336-dibit body has arrived,
            // then drop events older than MAX_AGE_DIBITS.
            let mut prev_tail: Vec<u8> = Vec::new();
            const CARRY_DIBITS: usize = 1024;
            // Global stream offset of the FIRST dibit currently in
            // `prev_tail`. Together with prev_tail.len() and the
            // new batch's hard_dibits, this lets us address any
            // dibit by its absolute stream position.
            let mut stream_offset_at_prev_tail_start: u64 = 0;
            // Pending events: absolute stream positions of first NID
            // dibits whose bodies haven't fully arrived yet.
            let mut pending_events: Vec<u64> = Vec::new();
            // Drop pending events whose first NID dibit is more than
            // this many dibits behind the latest data we have. With
            // CARRY_DIBITS = 1024 anything older than ~700 dibits has
            // already fallen off the front of prev_tail so it's
            // unrecoverable.
            const MAX_AGE_DIBITS: u64 = 700;
            loop {
                iq_waiter.wait().await;

                // Snapshot the new sub-buffers and overflow latch under
                // the lock, then drop it before doing CPU work.
                let (iq_complex, overflow) = {
                    let mut core = lsm_core.lock().await;
                    let buffers = core.read_iq_buffers();
                    let owned: Vec<Vec<u8>> =
                        buffers.iter().map(|b| b.to_vec()).collect();
                    let overflow = core.iq_overflow();
                    drop(core);
                    let refs: Vec<&[u8]> =
                        owned.iter().map(|b| b.as_slice()).collect();
                    (lsm::ring::sub_buffers_to_complex(&refs), overflow)
                };

                if overflow {
                    // Phase 6C HDL hotfix (doc 020): iq_packer.overflow
                    // used to be a latched level that never cleared, so
                    // the Rsticky register wrapper re-accumulated it on
                    // every cycle and this PS-side read saw overflow=1
                    // on every sub-buffer -- which used to trigger a
                    // full pipeline.reset() that wiped PLL / Gardner /
                    // sync-detector state every ~128 ms, so the Rust LSM
                    // pipeline never converged. With the pulse fix in
                    // iq_packer.py, this branch now only fires on a real
                    // back-pressure event. We log + count it, but do NOT
                    // pipeline.reset(): sample math in doc 014 proved no
                    // actual data loss, so a streaming reset was always
                    // the wrong reaction. If a real back-pressure event
                    // ever causes actual sample loss we need to detect
                    // it at a higher layer (gap in sample timestamps),
                    // not here.
                    tracing::warn!(
                        target: "p25_lsm",
                        "iq_dma overflow latched -- counting, NOT resetting pipeline (see doc 020)"
                    );
                    lsm_stats_task.lock().await.record_overflow();
                }
                if iq_complex.is_empty() {
                    continue;
                }

                let batch = pipeline.process_iq(&iq_complex);
                let wake_iq = iq_complex.len();
                let wake_dibits = batch.demod.n_symbols();
                let wake_hard = batch.hard_events.len();
                let wake_soft = batch.soft_events.len();

                // Phase 6F.10: dispatch soft sync events into the
                // directed-decode TSBK pipeline with cross-batch
                // pending-queue defer. Each event is identified by
                // its absolute stream position; if the full 336-dibit
                // body isn't available yet, it stays in
                // `pending_events` until enough dibits have arrived.
                {
                    let prev_tail_len = prev_tail.len();
                    // Combined buffer: prev_tail || new dibits.
                    let mut combined: Vec<u8> =
                        Vec::with_capacity(prev_tail_len + wake_dibits);
                    combined.extend_from_slice(&prev_tail);
                    combined.extend_from_slice(&batch.demod.hard_dibits);

                    // Absolute stream offset of combined[0].
                    let combined_start_offset = stream_offset_at_prev_tail_start;
                    // Absolute stream offset just past combined[len-1].
                    let combined_end_offset =
                        combined_start_offset + combined.len() as u64;

                    // Step 1: register every new soft event from this
                    // batch as an absolute stream position.
                    for ev in &batch.soft_events {
                        // ev.symbol_idx is the position of the FIRST
                        // NID dibit relative to THIS batch's
                        // hard_dibits, NOT relative to combined. Add
                        // the offset of "where the new dibits start
                        // in combined" + the global offset.
                        let abs_in_combined =
                            (prev_tail_len + ev.symbol_idx) as u64;
                        let abs_stream = combined_start_offset + abs_in_combined;
                        pending_events.push(abs_stream);
                    }

                    // Step 2: dispatch every pending event whose body
                    // is fully present in combined. Keep the rest for
                    // next batch.
                    if !pending_events.is_empty() {
                        let mut still_pending: Vec<u64> =
                            Vec::with_capacity(pending_events.len());
                        let mut dec = iq_lsm_decoder_task.write().await;
                        for &abs_stream in &pending_events {
                            // Translate absolute stream position into
                            // an offset within `combined`.
                            if abs_stream < combined_start_offset {
                                // Fell off the front of prev_tail
                                // before its body could complete.
                                // Lost -- drop silently.
                                continue;
                            }
                            let abs_in_combined =
                                (abs_stream - combined_start_offset) as usize;
                            // Need 336 dibits to dispatch (full
                            // 3-block TSDU). Below that, defer.
                            if abs_in_combined + 336 <= combined.len() {
                                dec.process_directed_tsdu(
                                    &combined[abs_in_combined..abs_in_combined + 336],
                                );
                            } else if abs_in_combined + 33 <= combined.len()
                                && combined_end_offset
                                    .saturating_sub(abs_stream)
                                    >= MAX_AGE_DIBITS
                            {
                                // Body never arrived in time. We have
                                // the NID at minimum, so dispatch what
                                // we have (fewer than 3 blocks) and
                                // give up on the rest.
                                let end = combined.len();
                                dec.process_directed_tsdu(
                                    &combined[abs_in_combined..end],
                                );
                            } else {
                                // Body partially arrived but we still
                                // have headroom -- keep waiting.
                                still_pending.push(abs_stream);
                            }
                        }
                        pending_events = still_pending;
                    }

                    // Step 3: trim combined to the last CARRY_DIBITS
                    // dibits and use that as the next prev_tail.
                    // Update the stream offset accordingly.
                    let new_prev_tail_start_offset =
                        if combined.len() > CARRY_DIBITS {
                            let drop = combined.len() - CARRY_DIBITS;
                            prev_tail = combined[drop..].to_vec();
                            combined_start_offset + drop as u64
                        } else {
                            prev_tail = combined;
                            combined_start_offset
                        };
                    stream_offset_at_prev_tail_start = new_prev_tail_start_offset;
                }

                // Fold into shared stats + snapshot cumulative totals and
                // top-3 NACs under the lock, then drop it before logging.
                let (wakeups, cum_iq, cum_dibits, cum_hard, cum_soft, top3) = {
                    let mut stats = lsm_stats_task.lock().await;
                    stats.record_batch(wake_iq, &batch);
                    (
                        stats.wakeups,
                        stats.iq_samples,
                        stats.dibits,
                        stats.hard_events,
                        stats.soft_events,
                        stats.top_nacs(3),
                    )
                };

                if wakeups <= 5 || wakeups % 16 == 0 {
                    let top_str = top3
                        .iter()
                        .map(|(n, c)| format!("0x{:03X}={}", n, c))
                        .collect::<Vec<_>>()
                        .join(",");
                    tracing::info!(
                        target: "p25_lsm",
                        "wake #{wakeups}: iq_samples={wake_iq} dibits={wake_dibits} \
                         hard_syncs={wake_hard} soft_syncs={wake_soft} \
                         (cum iq={cum_iq} dibits={cum_dibits} hard={cum_hard} soft={cum_soft}) \
                         top_nacs=[{top_str}]"
                    );
                }
            }
        });

        // 6c. Spawn HDL LSM dibit ring drain + TSBK decode task
        //     (Phase 6E.9/6E.10 bring-up).
        //
        //     The HDL LSM demod chain produces its own dibit stream via
        //     `lsm_dibit_dma`, parallel to the C4FM `dibit_dma` ring on
        //     the same control DDC output. We feed that stream into a
        //     SECOND, independent `ControlChannelDecoder` instance
        //     (`lsm_decoder`) which runs the identical Hunting ->
        //     ReadingNid -> ReadingDu -> trellis -> CRC -> TsbkMessage
        //     pipeline as the C4FM decoder above, just against a
        //     different dibit source. Both decoders land events on the
        //     same WebSocket broadcast channel so the dashboard sees a
        //     unified TSBK stream; the distinct trace targets
        //     (`p25_decoder` vs `p25_hdl_lsm_decoder`) let operators
        //     separate them in the server log.
        //
        //     **Why a separate instance instead of feeding into the
        //     existing decoder:** the two dibit streams come from two
        //     independent HDL demod chains with independent symbol
        //     timing loops. Frame sync alignment, NID boundaries, and
        //     TSU framing state are all specific to the stream they
        //     came from -- sharing state would corrupt either or both
        //     decoders. Two parallel instances is cheap (~300 bytes of
        //     state each on an ARM Cortex-A9) and gives us the
        //     cross-validation we want for bring-up: both decoders
        //     should emit IDENTICAL TSBK streams against the same RF
        //     capture, confirming the HDL LSM port is equivalent to
        //     the working Phase 2A C4FM path.
        //
        //     The existing Phase 6D in-PS Rust LSM pipeline keeps
        //     running in parallel (task 6b below) as a third
        //     independent sanity check. Retiring it is a Phase 6F
        //     decision after all three paths converge on hardware.
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

        // 6d. Spawn HDL LSM heartbeat / NID event poller (Phase 6E.9/6E.10).
        //
        //     Reads `lsm_status` + `lsm_debug` on EVERY tick at 60 Hz,
        //     not just when `nid_event` fires. This lets us see the
        //     state of the HDL LSM chain even when it isn't producing
        //     NID events, which is exactly the bring-up situation we
        //     hit on real RF.
        //
        //     Outputs per loop iteration:
        //     1. **NID event log** -- as before, fires only when
        //        `lsm_status.nid_event` (Rsticky) is high. Throttled to
        //        5 Hz on a busy site (~70 NIDs/sec) but always logs the
        //        first 10 events. Every event is ALSO captured into a
        //        32-deep ring buffer for the crash-transition dump
        //        described below.
        //     2. **Heartbeat log** -- fires every ~1 s regardless of
        //        whether NIDs are being decoded, dumping the windowed
        //        min/max of `pll_dbg` + `sample_point_dbg` + the lowest
        //        `sync_distance` seen in the window + iq_dma health
        //        + how many ticks of the window observed `bch_busy` /
        //        `in_nid_window` / `nid_event` / `dibit_overflow`.
        //     3. **Crash dump** -- the FIRST time `nid_evts == 0` in
        //        a heartbeat window after we've seen any healthy
        //        traffic, dump the full 32-deep NID ring buffer. This
        //        captures the exact NID events leading up to the
        //        transition from healthy to stalled, with no log
        //        throttling.
        //
        //     **Watchdog removed** (was Phase 6E.6 doc 023). Investigation
        //     after the 2026-04-10 CORDIC bake confirmed sdr_reset is
        //     fundamentally unsafe to pulse during operation -- it
        //     resets the entire `sync` clock domain, which interrupts
        //     in-flight AXI HP DMA writes, deadlocks the AXI HP slave
        //     in the PS DDR controller, and causes a hard kernel panic
        //     reboot. The previous "watchdog" was silently broken
        //     (CDC corruption swallowed its writes after the chain
        //     transitioned to the degraded state) -- if it had ever
        //     fired correctly, it would have crashed the board. The
        //     code is removed entirely; recovery from the degraded
        //     state requires either a power cycle or a future safer
        //     reset mechanism that drains in-flight AXI before
        //     asserting reset.
        let lsm_nid_core = ip_core.clone();
        let lsm_nid_runtime = hdl_lsm.clone();
        tokio::spawn(async move {
            tracing::info!("HDL LSM heartbeat + NID poller task started (Phase 6E)");
            // Stamp the start time as soon as we run.
            {
                let mut rt = lsm_nid_runtime.lock().await;
                rt.started_at = Some(std::time::Instant::now());
            }
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(16),
            );
            tick.tick().await;

            // ── NID event tracking (cumulative) ─────────────────────
            let mut event_count: u64 = 0;
            let mut valid_count: u64 = 0;
            let mut last_drop_count: u16 = 0;
            let mut last_event_log = std::time::Instant::now();

            // ── NID event ring buffer for crash-transition dump ────
            // Captures the last 32 NID events with full state. Dumped
            // unconditionally on the first "nid_evts == 0" heartbeat
            // after at least one healthy heartbeat has been seen.
            const NID_RING_DEPTH: usize = 32;
            #[derive(Clone, Copy, Default)]
            struct NidRingEntry {
                seq: u64,
                t_ms_since_boot: u128,
                nac: u16,
                duid: u8,
                valid: bool,
                n_errors: u8,
                sync_distance: u8,
                drop_count: u16,
                pll_dbg: i16,
                sp_dbg: i16,
            }
            let mut nid_ring: [NidRingEntry; NID_RING_DEPTH] =
                [NidRingEntry::default(); NID_RING_DEPTH];
            let mut nid_ring_pos: usize = 0;
            let mut nid_ring_count: usize = 0;
            let mut crash_dump_armed = false;
            let mut crash_dump_fired = false;
            let task_start = std::time::Instant::now();

            // ── iq_dma drain-rate tracking for the heartbeat ───────
            let mut last_iq_next_addr: u32 = 0;
            let mut last_iq_last_buffer: u8 = 0xFF;
            let mut hb_iq_addr_advance: u64 = 0;
            let mut hb_iq_buffer_changes: u32 = 0;
            let mut hb_iq_overflow_ticks: u32 = 0;
            let mut last_iq_overflow_log = std::time::Instant::now();

            // ── Heartbeat windowed stats (reset on each emission) ──
            // Reset every ~1 s of polling = ~60 ticks at 16 ms.
            let mut hb_ticks: u32 = 0;
            let mut hb_pll_min: i16 = i16::MAX;
            let mut hb_pll_max: i16 = i16::MIN;
            let mut hb_sp_min: i16 = i16::MAX;
            let mut hb_sp_max: i16 = i16::MIN;
            // sync_distance is u8 0..47; track the BEST (lowest) hit in
            // the window. 99 is a "no observations yet" sentinel.
            let mut hb_sync_dist_best: u8 = 99;
            let mut hb_bch_busy_ticks: u32 = 0;
            let mut hb_in_window_ticks: u32 = 0;
            let mut hb_nid_event_ticks: u32 = 0;
            let mut hb_overflow_ticks: u32 = 0;
            // For NIDs that DID arrive in the window, what did we see?
            let mut hb_window_event_count: u32 = 0;
            let mut hb_window_valid_count: u32 = 0;

            let mut last_hb = std::time::Instant::now();

            loop {
                tick.tick().await;

                // EVERY tick: snapshot the full status + debug pair
                // PLUS the iq_dma health indicators. We read coherently
                // under the mutex so the heartbeat observes the same
                // instant the optional nid_event payload would
                // describe.
                let (
                    status,
                    nac,
                    duid,
                    drop_count,
                    pll_dbg,
                    sp_dbg,
                    iq_overflow,
                    iq_last_buffer,
                    iq_next_addr,
                ) = {
                    let core = lsm_nid_core.lock().await;
                    let s = core.lsm_status();
                    let (nac, duid) = core.lsm_nid();
                    let drop_count = core.lsm_drop_count();
                    let (pll_dbg, sp_dbg) = core.lsm_debug();
                    let iq_overflow = core.iq_overflow();
                    let iq_last_buffer = core.iq_last_buffer();
                    let iq_next_addr = core.iq_next_address();
                    (
                        s, nac, duid, drop_count, pll_dbg, sp_dbg,
                        iq_overflow, iq_last_buffer, iq_next_addr,
                    )
                };

                // ── iq_dma health bookkeeping for this tick ─────────
                // Track AW address advance + last_buffer rollover rate.
                // In healthy operation iq_next_addr advances ~5 KB per
                // tick (62.5kSPS * 4 bytes/sample / 60 Hz). If it
                // stops advancing or advances at <50 % of nominal, the
                // iq_dma write side has stalled.
                if last_iq_next_addr != 0 {
                    // Compute forward delta with wrap. Buffers in the
                    // ring are 4 KB and the ring wraps every 32 KB,
                    // so a single tick should never advance by more
                    // than 8 KB even at peak rate.
                    let delta = iq_next_addr.wrapping_sub(last_iq_next_addr);
                    // Filter out spurious huge backward jumps that
                    // would happen on a CDC read race.
                    if delta < 0x10000 {
                        hb_iq_addr_advance += delta as u64;
                    }
                }
                last_iq_next_addr = iq_next_addr;
                if iq_last_buffer != last_iq_last_buffer && last_iq_last_buffer != 0xFF {
                    hb_iq_buffer_changes += 1;
                }
                last_iq_last_buffer = iq_last_buffer;
                if iq_overflow {
                    hb_iq_overflow_ticks += 1;
                    // Throttle the per-tick warning to once per second
                    // so a stuck overflow doesn't drown the log.
                    if last_iq_overflow_log.elapsed()
                        >= std::time::Duration::from_secs(1)
                    {
                        last_iq_overflow_log = std::time::Instant::now();
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "iq_dma overflow latched (rate-limited; \
                             one warn per second of stuck state)"
                        );
                    }
                }

                // ── Fold this tick into the heartbeat window ────────
                hb_ticks += 1;
                if pll_dbg < hb_pll_min { hb_pll_min = pll_dbg; }
                if pll_dbg > hb_pll_max { hb_pll_max = pll_dbg; }
                if sp_dbg  < hb_sp_min  { hb_sp_min  = sp_dbg; }
                if sp_dbg  > hb_sp_max  { hb_sp_max  = sp_dbg; }
                if status.bch_busy      { hb_bch_busy_ticks += 1; }
                if status.in_nid_window { hb_in_window_ticks += 1; }
                if status.nid_event     { hb_nid_event_ticks += 1; }
                if status.dibit_overflow { hb_overflow_ticks += 1; }
                // Track the lowest sync_distance we ever see across
                // the window, regardless of whether nid_event fires.
                // The HDL latches sync_distance at the moment a sync
                // hit fires so it stays constant between events.
                // sync_distance == 0 sentinel after a perfect hit is
                // also legitimate, and the rsticky bits decay on read,
                // so we just take min over the raw reads.
                if status.sync_distance < hb_sync_dist_best {
                    hb_sync_dist_best = status.sync_distance;
                }

                // ── Per-tick: dibit overflow latch warning ──────────
                if status.dibit_overflow {
                    // Throttled to 1 Hz so a stuck overflow doesn't
                    // dominate the log (it used to fire every poll).
                    if last_iq_overflow_log.elapsed()
                        >= std::time::Duration::from_secs(1)
                    {
                        // (Reuse the same throttle as iq overflow --
                        // both signal the same upstream stall and
                        // we want one warn per second total.)
                    }
                    // Don't reset the throttle here; let the iq side
                    // manage it. We just count for the heartbeat.
                }

                // ── Phase 6F.2: live PL register snapshot to shared
                //    HdlLsmRuntime so /api/hdl_lsm can read it ───────
                {
                    let mut rt = lsm_nid_runtime.lock().await;
                    rt.last_tick_at = Some(std::time::Instant::now());
                    rt.pll_dbg = pll_dbg;
                    rt.sp_dbg = sp_dbg;
                    rt.sync_distance = status.sync_distance;
                    rt.bch_busy = status.bch_busy;
                    rt.in_nid_window = status.in_nid_window;
                    rt.dibit_overflow_latched = status.dibit_overflow;
                    rt.iq_overflow_latched = iq_overflow;
                    rt.last_nac = nac;
                    rt.last_duid = duid;
                    rt.last_drop_count = drop_count;
                    if status.dibit_overflow {
                        rt.dibit_overflow_ticks += 1;
                    }
                    if iq_overflow {
                        rt.iq_overflow_ticks += 1;
                    }
                }

                // ── Per-tick: NID event handling ────────────────────
                if status.nid_event {
                    event_count += 1;
                    hb_window_event_count += 1;
                    if status.nid_valid {
                        valid_count += 1;
                        hb_window_valid_count += 1;
                    }

                    // ALWAYS push the event into the ring buffer
                    // (regardless of throttling). This is what the
                    // crash dump reads.
                    let entry = NidRingEntry {
                        seq: event_count,
                        t_ms_since_boot: task_start.elapsed().as_millis(),
                        nac,
                        duid,
                        valid: status.nid_valid,
                        n_errors: status.n_errors,
                        sync_distance: status.sync_distance,
                        drop_count,
                        pll_dbg,
                        sp_dbg,
                    };
                    nid_ring[nid_ring_pos] = entry;
                    nid_ring_pos = (nid_ring_pos + 1) % NID_RING_DEPTH;
                    if nid_ring_count < NID_RING_DEPTH {
                        nid_ring_count += 1;
                    }
                    // Arm the crash-dump trigger as soon as we've
                    // seen any healthy traffic.
                    crash_dump_armed = true;

                    // ── Phase 6F.2: NID-event update to shared runtime
                    {
                        let mut rt = lsm_nid_runtime.lock().await;
                        rt.total_nid_events = event_count;
                        rt.valid_nid_events = valid_count;
                        rt.last_nid_at = Some(std::time::Instant::now());
                        rt.last_nid_valid = status.nid_valid;
                        rt.last_nid_n_errors = status.n_errors;
                        if status.nid_valid {
                            *rt.nac_hist.entry(nac).or_insert(0) += 1;
                        }
                        // Rebuild the ring as a chronological Vec for
                        // the dashboard.
                        let depth = nid_ring_count;
                        let start = if nid_ring_count < NID_RING_DEPTH {
                            0
                        } else {
                            nid_ring_pos
                        };
                        rt.nid_ring.clear();
                        for i in 0..depth {
                            let idx = (start + i) % NID_RING_DEPTH;
                            let e = nid_ring[idx];
                            rt.nid_ring.push(HdlNidEntry {
                                seq: e.seq,
                                t_ms_since_boot: e.t_ms_since_boot as u64,
                                nac: e.nac,
                                duid: e.duid,
                                valid: e.valid,
                                n_errors: e.n_errors,
                                sync_distance: e.sync_distance,
                                drop_count: e.drop_count,
                                pll_dbg: e.pll_dbg,
                                sp_dbg: e.sp_dbg,
                            });
                        }
                    }

                    if drop_count != last_drop_count {
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "lsm_drop_count bumped {} -> {} -- sync detector emitted a NID while BCH was busy",
                            last_drop_count, drop_count,
                        );
                        last_drop_count = drop_count;
                    }
                    // Throttle event logging to 5 Hz on a busy site
                    // (~70 NIDs/sec); always log the first 10.
                    let log_now = event_count <= 10
                        || last_event_log.elapsed() >= std::time::Duration::from_millis(200);
                    if log_now {
                        last_event_log = std::time::Instant::now();
                        tracing::info!(
                            target: "p25_hdl_lsm",
                            "NID event #{event_count}: nac=0x{:03X} duid={} \
                             valid={} n_errors={} sync_dist={} drop_count={} \
                             in_nid_window={} bch_busy={} pll={} sp={} \
                             (valid totals: {}/{})",
                            nac, duid, status.nid_valid, status.n_errors,
                            status.sync_distance, drop_count,
                            status.in_nid_window, status.bch_busy,
                            pll_dbg, sp_dbg,
                            valid_count, event_count,
                        );
                    }
                }

                // ── Heartbeat log (every ~1 s of polling) ───────────
                if last_hb.elapsed() >= std::time::Duration::from_secs(1) {
                    let pll_range = if hb_pll_min == i16::MAX {
                        "[no samples]".to_string()
                    } else {
                        format!("[{},{}]", hb_pll_min, hb_pll_max)
                    };
                    let sp_range = if hb_sp_min == i16::MAX {
                        "[no samples]".to_string()
                    } else {
                        format!("[{},{}]", hb_sp_min, hb_sp_max)
                    };
                    let best_str = if hb_sync_dist_best == 99 {
                        "n/a".to_string()
                    } else {
                        hb_sync_dist_best.to_string()
                    };
                    // iq_dma health: KB/s and buffer rollover rate.
                    let iq_kbps = hb_iq_addr_advance / 1024;
                    tracing::info!(
                        target: "p25_hdl_lsm",
                        "HB {hb_ticks}t: pll{pll_range} sp{sp_range} \
                         best_sync_dist={best_str} \
                         bch_busy={hb_bch_busy_ticks} \
                         in_window={hb_in_window_ticks} \
                         nid_evts={hb_nid_event_ticks} \
                         dibit_overflow={hb_overflow_ticks} \
                         iq_overflow={hb_iq_overflow_ticks} \
                         iq_kbps={iq_kbps} \
                         iq_buf_rolls={hb_iq_buffer_changes} \
                         (window NIDs: {hb_window_valid_count}/{hb_window_event_count} valid; \
                         cum NIDs: {valid_count}/{event_count})"
                    );

                    // ── Crash-transition NID ring dump ──────────────
                    //
                    // The MOMENT this is the first heartbeat with
                    // zero NID events after we've seen at least one
                    // healthy heartbeat, dump the full ring buffer.
                    // This captures up to 32 NID events with full
                    // pll/sp/sync_dist/n_errors/drop_count state, no
                    // throttling. Compare entries N..N+5 (the
                    // tail) for the moment things went wrong.
                    //
                    // Fires exactly once per boot. Subsequent stuck
                    // heartbeats just emit the regular HB line.
                    if crash_dump_armed
                        && !crash_dump_fired
                        && hb_window_event_count == 0
                    {
                        crash_dump_fired = true;
                        let depth = nid_ring_count;
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "CRASH TRANSITION: HDL LSM chain produced 0 \
                             NID events in the past 1s window after a \
                             healthy run -- dumping the last {} NID \
                             events from the ring buffer:",
                            depth,
                        );
                        // Walk the ring in chronological order
                        // (oldest -> newest) so the log reads
                        // top-to-bottom in time order.
                        let start = if nid_ring_count < NID_RING_DEPTH {
                            0
                        } else {
                            nid_ring_pos
                        };
                        for i in 0..depth {
                            let idx = (start + i) % NID_RING_DEPTH;
                            let e = nid_ring[idx];
                            tracing::warn!(
                                target: "p25_hdl_lsm",
                                "  ring[{:2}] t={:>6}ms #{:>5} \
                                 nac=0x{:03X} duid={} valid={:>5} \
                                 n_errors={:>2} sync_dist={:>2} \
                                 drop_count={:>5} pll={:>6} sp={:>6}",
                                i,
                                e.t_ms_since_boot,
                                e.seq,
                                e.nac,
                                e.duid,
                                e.valid,
                                e.n_errors,
                                e.sync_distance,
                                e.drop_count,
                                e.pll_dbg,
                                e.sp_dbg,
                            );
                        }
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "CRASH TRANSITION: end of ring dump. Chain \
                             will likely remain stuck until power \
                             cycle (sdr_reset is unsafe to use during \
                             operation -- see doc/changes/024)."
                        );
                    }

                    // ── Phase 6F.2: snapshot completed window into the
                    //    shared runtime BEFORE we reset accumulators ──
                    {
                        let mut rt = lsm_nid_runtime.lock().await;
                        rt.hb_pll_min = if hb_pll_min == i16::MAX { 0 } else { hb_pll_min };
                        rt.hb_pll_max = if hb_pll_max == i16::MIN { 0 } else { hb_pll_max };
                        rt.hb_sp_min = if hb_sp_min == i16::MAX { 0 } else { hb_sp_min };
                        rt.hb_sp_max = if hb_sp_max == i16::MIN { 0 } else { hb_sp_max };
                        rt.hb_sync_dist_best = hb_sync_dist_best;
                        rt.hb_bch_busy_ticks = hb_bch_busy_ticks;
                        rt.hb_in_window_ticks = hb_in_window_ticks;
                        rt.hb_nid_event_ticks = hb_nid_event_ticks;
                        rt.hb_dibit_overflow_ticks = hb_overflow_ticks;
                        rt.hb_iq_overflow_ticks = hb_iq_overflow_ticks;
                        rt.hb_iq_kbps = iq_kbps;
                        rt.hb_iq_buf_rolls = hb_iq_buffer_changes;
                        rt.hb_window_valid_count = hb_window_valid_count;
                        rt.hb_window_event_count = hb_window_event_count;
                    }

                    // Reset windowed accumulators for the next second.
                    hb_ticks = 0;
                    hb_pll_min = i16::MAX;
                    hb_pll_max = i16::MIN;
                    hb_sp_min = i16::MAX;
                    hb_sp_max = i16::MIN;
                    hb_sync_dist_best = 99;
                    hb_bch_busy_ticks = 0;
                    hb_in_window_ticks = 0;
                    hb_nid_event_ticks = 0;
                    hb_overflow_ticks = 0;
                    hb_iq_addr_advance = 0;
                    hb_iq_buffer_changes = 0;
                    hb_iq_overflow_ticks = 0;
                    hb_window_event_count = 0;
                    hb_window_valid_count = 0;
                    last_hb = std::time::Instant::now();
                }
            }
        });

        // 7. Spawn periodic stats task — polls FPGA registers every 2s
        let stats_core = ip_core.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
            tick.tick().await; // discard first immediate tick
            loop {
                tick.tick().await;
                let core = stats_core.lock().await;
                tracing::info!(
                    target: "p25_stats",
                    "regs: dibit_count={} overflow={} last_buffer={} next_addr=0x{:08X} \
                     lsm_last_buffer={} lsm_next_addr=0x{:08X}",
                    core.dibit_count(),
                    core.demod_overflow(),
                    core.dibit_last_buffer(),
                    core.dibit_next_address(),
                    core.lsm_dibit_last_buffer(),
                    core.lsm_dibit_next_address(),
                );
            }
        });

        // 8. Spawn periodic grant-expiry task. The control channel decoder
        //    accumulates voice grants in a HashMap as it sees TSBK_GRANT
        //    messages. Without periodic pruning the table only ever grows
        //    -- the dashboard's "Active Grants" count would never decay
        //    even after a call ended. Expire any grant whose TSBK was last
        //    seen more than 30 seconds ago (P25 typical call timeout).
        let expiry_decoder = decoder.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut dec = expiry_decoder.write().await;
                dec.expire_grants(30);
            }
        });

        // Phase 6F.1: same expiry sweep for the LSM decoder. Without it,
        // grants accumulated by the LSM decoder (which now feeds the
        // dashboard's Active Grants panel) would never time out.
        let lsm_expiry_decoder = lsm_decoder.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
            tick.tick().await;
            loop {
                tick.tick().await;
                let mut dec = lsm_expiry_decoder.write().await;
                dec.expire_grants(30);
            }
        });

        (ip_core, ad9361)
    };

    // Build app state
    let state = Arc::new(httpd::AppState {
        decoder: decoder.clone(),
        lsm_decoder: lsm_decoder.clone(),
        iq_lsm_decoder: iq_lsm_decoder.clone(),
        event_tx,
        #[cfg(target_os = "linux")]
        ip_core,
        #[cfg(target_os = "linux")]
        ad9361,
        lsm_stats: lsm_stats.clone(),
        hdl_lsm: hdl_lsm.clone(),
        irq_stats: irq_stats.clone(),
    });

    // Start HTTP server
    let app = httpd::router(state);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    tracing::info!("Dashboard at http://{}", args.listen);
    axum::serve(listener, app).await?;
    Ok(())
}

/// Safe cast of a byte buffer to u64 slice (assumes alignment from DMA).
#[cfg(target_os = "linux")]
fn bytemuck_cast(buffer: &[u8]) -> &[u64] {
    let len = buffer.len() / 8;
    if len == 0 {
        return &[];
    }
    unsafe { std::slice::from_raw_parts(buffer.as_ptr() as *const u64, len) }
}

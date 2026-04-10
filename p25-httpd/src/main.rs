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

    tracing::info!(
        "Fishball P25 starting: RX LO={} Hz, control_freq={} Hz",
        args.rx_lo,
        args.control_freq
    );

    let (event_tx, _) = broadcast::channel::<String>(256);
    let mut decoder = ControlChannelDecoder::new();
    decoder.set_event_tx(event_tx.clone());
    let decoder = Arc::new(RwLock::new(decoder));

    // Phase 6D dashboard wiring: shared LsmStats mutex, populated by the
    // LSM IRQ task below and read by the /api/lsm handler. Kept out of
    // the cfg(linux) block so non-Linux builds still expose the (empty)
    // endpoint -- useful for host-side cargo test of httpd routing.
    let lsm_stats = Arc::new(tokio::sync::Mutex::new(lsm::LsmStats::default()));

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

        // 3. Configure control channel DDC (FIR filters + decimation + NCO)
        let nco_offset = args.control_freq as f64 - args.rx_lo as f64;
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
        ip_core.set_lsm_enable(true);
        ip_core.set_lsm_dibit_dma_enable(true);
        tracing::info!(
            "Control DDC: offset={nco_offset} Hz, dibit + iq + lsm ring DMA enabled"
        );

        let ip_core = Arc::new(Mutex::new(ip_core));
        let ad9361 = Arc::new(ad9361);

        // 4. Get interrupt waiters before spawning handler
        let dibit_waiter = interrupt_handler.waiter_dibit_dma();
        let iq_waiter = interrupt_handler.waiter_iq_dma();
        let lsm_dibit_waiter = interrupt_handler.waiter_lsm_dibit_dma();

        // 5. Spawn interrupt handler
        tokio::spawn(async move {
            if let Err(e) = interrupt_handler.run().await {
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
        tokio::spawn(async move {
            tracing::info!("LSM IQ reader task started (Phase 6D)");
            let mut pipeline = lsm::LsmPipeline::new();
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

        // 6c. Spawn HDL LSM dibit ring drain task (Phase 6E.9/6E.10).
        //     The HDL LSM demod chain produces its own dibit stream via
        //     `lsm_dibit_dma`, parallel to the C4FM `dibit_dma` ring.
        //     We drain it to keep the ring from back-pressuring, but we
        //     deliberately do NOT feed it to the C4FM control-channel
        //     decoder -- LSM dibits have different symbol-phase timing
        //     and feeding them into the C4FM TSBK parser would corrupt
        //     state. For now the dibits are only counted + histogrammed,
        //     so bring-up can confirm "gateware is producing plausible
        //     symbols" without the risk of cross-polluting the working
        //     Phase 2A decoder. A dedicated LSM TSBK decoder is Phase 6F.
        let lsm_dibit_core = ip_core.clone();
        tokio::spawn(async move {
            tracing::info!("HDL LSM dibit reader task started (Phase 6E)");
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
                    for &word in words {
                        for i in 0..32 {
                            let d = ((word >> (i * 2)) & 0x03) as usize;
                            hist[d] += 1;
                            wake_dibits += 1;
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
                    tracing::info!(
                        target: "p25_hdl_lsm",
                        "wake #{wakeups}: bufs={} bytes={} dibits={} \
                         (cum bufs={total_buffers} bytes={total_bytes}) \
                         hist 0={:.1}% 1={:.1}% 2={:.1}% 3={:.1}%",
                        buffers.len(), wake_bytes, wake_dibits,
                        pct(hist[0]), pct(hist[1]), pct(hist[2]), pct(hist[3]),
                    );
                }
            }
        });

        // 6d. Spawn HDL LSM NID event poller (Phase 6E.9/6E.10).
        //     `lsm_status.nid_event` is Rsticky: the HDL latches it on
        //     each `nid_event_strobe` and clears it on read. At one NID
        //     per ~14 ms a 60 Hz polling loop catches every event with
        //     plenty of headroom, and we explicitly avoid IRQ-driving
        //     NID events to keep the IRQ budget for the dibit ring.
        //     Reads `lsm_status` + `lsm_nid` + `lsm_drop_count` in one
        //     pass on the event -- the HDL guarantees these form a
        //     coherent per-event snapshot (see fpga.rs::lsm_status()).
        let lsm_nid_core = ip_core.clone();
        tokio::spawn(async move {
            tracing::info!("HDL LSM NID poller task started (Phase 6E)");
            let mut tick = tokio::time::interval(
                std::time::Duration::from_millis(16),
            );
            tick.tick().await;
            let mut event_count: u64 = 0;
            let mut valid_count: u64 = 0;
            let mut last_drop_count: u16 = 0;
            let mut last_log = std::time::Instant::now();
            loop {
                tick.tick().await;
                let (status, nac, duid, drop_count, pll_dbg, sp_dbg) = {
                    let core = lsm_nid_core.lock().await;
                    let s = core.lsm_status();
                    if !s.nid_event && !s.dibit_overflow {
                        // nothing to report; skip the rest of the reads
                        // to avoid racing with other fields -- we only
                        // burn a few cycles per tick on the happy path
                        continue;
                    }
                    let (nac, duid) = core.lsm_nid();
                    let drop_count = core.lsm_drop_count();
                    let (pll_dbg, sp_dbg) = core.lsm_debug();
                    (s, nac, duid, drop_count, pll_dbg, sp_dbg)
                };

                if status.dibit_overflow {
                    tracing::warn!(
                        target: "p25_hdl_lsm",
                        "lsm_dibit_overflow latched -- PS not draining the LSM dibit ring fast enough"
                    );
                }

                if status.nid_event {
                    event_count += 1;
                    if status.nid_valid {
                        valid_count += 1;
                    }
                    if drop_count != last_drop_count {
                        tracing::warn!(
                            target: "p25_hdl_lsm",
                            "lsm_drop_count bumped {} -> {} -- sync detector emitted a NID while BCH was busy",
                            last_drop_count, drop_count,
                        );
                        last_drop_count = drop_count;
                    }
                    // Throttle event logging to 5 Hz so we don't flood
                    // on a healthy site (~70 NIDs/sec); always log the
                    // first 10 for sanity.
                    let log_now = event_count <= 10
                        || last_log.elapsed() >= std::time::Duration::from_millis(200);
                    if log_now {
                        last_log = std::time::Instant::now();
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

        (ip_core, ad9361)
    };

    // Build app state
    let state = Arc::new(httpd::AppState {
        decoder: decoder.clone(),
        event_tx,
        #[cfg(target_os = "linux")]
        ip_core,
        #[cfg(target_os = "linux")]
        ad9361,
        lsm_stats: lsm_stats.clone(),
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

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
        tracing::info!(
            "Control DDC: offset={nco_offset} Hz, dibit + iq ring DMA enabled"
        );

        let ip_core = Arc::new(Mutex::new(ip_core));
        let ad9361 = Arc::new(ad9361);

        // 4. Get interrupt waiters before spawning handler
        let dibit_waiter = interrupt_handler.waiter_dibit_dma();
        let iq_waiter = interrupt_handler.waiter_iq_dma();

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
                    tracing::warn!(
                        target: "p25_lsm",
                        "iq_dma overflow latched -- resetting LSM streaming state"
                    );
                    pipeline.reset();
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
                    "regs: dibit_count={} overflow={} last_buffer={} next_addr=0x{:08X}",
                    core.dibit_count(),
                    core.demod_overflow(),
                    core.dibit_last_buffer(),
                    core.dibit_next_address(),
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

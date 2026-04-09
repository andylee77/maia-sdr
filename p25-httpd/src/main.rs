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
    tracing_subscriber::fmt::init();
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

    #[cfg(target_os = "linux")]
    let ip_core = {
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
        tracing::info!("Control DDC: offset={nco_offset} Hz, ring DMA enabled");

        let ip_core = Arc::new(Mutex::new(ip_core));

        // 4. Get interrupt waiters before spawning handler
        let dibit_waiter = interrupt_handler.waiter_dibit_dma();

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

        ip_core
    };

    // Build app state
    let state = Arc::new(httpd::AppState {
        decoder: decoder.clone(),
        event_tx,
        #[cfg(target_os = "linux")]
        ip_core,
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

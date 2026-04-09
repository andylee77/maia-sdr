//! FPGA IP core driver.
//!
//! Accesses the P25 core registers via UIO (userspace I/O) and reads
//! dibit DMA buffers via the maia-sdr kernel module rxbuffer device.
//! Pattern adapted from maia-httpd/src/fpga.rs.

use anyhow::{Context, Result};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::Notify;

use crate::rxbuffer::RxBuffer;
use crate::uio::{Mapping, Uio};

/// Expected product ID in the FPGA register (ASCII "p25f" = 0x70323566).
const PRODUCT_ID: u32 = 0x7032_3566;

// ── Register access ──────────────────────────────────────────────────

/// Wrapper over a UIO mapping that derefs to the p25-pac RegisterBlock.
#[derive(Debug, Clone)]
struct Registers(Mapping);

impl Deref for Registers {
    type Target = p25_pac::fishball_p25::RegisterBlock;

    fn deref(&self) -> &Self::Target {
        unsafe { &*(self.0.addr() as *const p25_pac::fishball_p25::RegisterBlock) }
    }
}

// ── IP core ──────────────────────────────────────────────────────────

/// FPGA IP core handle.
///
/// Provides register access for DDC configuration, demod control, and
/// DMA buffer reading for both control and traffic channels.
pub struct IpCore {
    registers: Registers,
    dibit_dma: RxBuffer,
    traffic_dma: RxBuffer,
    dibit_last_addr: Option<u32>,
    traffic_last_addr: Option<u32>,
}

impl IpCore {
    /// Opens and initializes the P25 FPGA IP core.
    ///
    /// Returns the IP core handle and an interrupt handler that should
    /// be spawned as a background task.
    pub async fn take() -> Result<(IpCore, InterruptHandler)> {
        let uio = Uio::from_name("p25-core")
            .await
            .context("failed to open p25-core UIO device")?;
        let mapping = uio
            .map_mapping(0)
            .await
            .context("failed to mmap p25-core registers")?;

        let registers = Registers(mapping.clone());
        let interrupt_registers = Registers(mapping);

        // Validate FPGA product ID
        let id = registers.product_id().read().product_id().bits();
        if id != PRODUCT_ID {
            anyhow::bail!(
                "FPGA product ID mismatch: expected 0x{PRODUCT_ID:08x}, got 0x{id:08x}"
            );
        }

        // Read version
        let ver = registers.version().read();
        tracing::info!(
            "P25 FPGA core v{}.{}.{} (platform {})",
            ver.major().bits(),
            ver.minor().bits(),
            ver.bugfix().bits(),
            ver.platform().bits(),
        );

        // De-assert SDR reset
        registers
            .control()
            .modify(|_, w| w.sdr_reset().clear_bit());

        // Open DMA buffer devices
        let dibit_dma = RxBuffer::new("p25-dibit")
            .await
            .context("failed to open p25-dibit DMA buffer")?;
        let traffic_dma = RxBuffer::new("p25-traffic")
            .await
            .context("failed to open p25-traffic DMA buffer")?;

        let ip_core = IpCore {
            registers,
            dibit_dma,
            traffic_dma,
            dibit_last_addr: None,
            traffic_last_addr: None,
        };

        let interrupt_handler = InterruptHandler::new(uio, interrupt_registers);
        Ok((ip_core, interrupt_handler))
    }

    // ── Control channel DDC ──────────────────────────────────────

    /// Sets the control channel DDC NCO frequency.
    ///
    /// `frequency_hz` is the offset from the RX LO center.
    /// `sample_rate_hz` is the AD9361 ADC sample rate.
    pub fn set_ddc_frequency(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        let half = 0.5 * sample_rate_hz;
        if !(-half..=half).contains(&frequency_hz) {
            anyhow::bail!(
                "DDC frequency {frequency_hz} Hz out of range ±{half} Hz"
            );
        }
        let nco_word = freq_to_nco(frequency_hz, sample_rate_hz);
        self.registers
            .ddc_frequency()
            .modify(|_, w| unsafe { w.frequency().bits(nco_word) });
        tracing::debug!("DDC frequency: {frequency_hz} Hz (NCO 0x{nco_word:07x})");
        Ok(())
    }

    /// Enables or disables the control channel DDC input.
    pub fn set_ddc_enable(&self, enable: bool) {
        self.registers
            .ddc_control()
            .modify(|_, w| w.enable_input().bit(enable));
    }

    // ── Control channel demod ────────────────────────────────────

    /// Enables or disables the C4FM demodulator.
    pub fn set_demod_enable(&self, enable: bool) {
        self.registers
            .demod_control()
            .modify(|_, w| w.demod_enable().bit(enable));
    }

    /// Starts the dibit DMA.
    pub fn demod_start(&self) {
        self.registers
            .demod_control()
            .modify(|_, w| w.start().set_bit());
    }

    /// Stops the dibit DMA.
    pub fn demod_stop(&self) {
        self.registers
            .demod_control()
            .modify(|_, w| w.stop().set_bit());
    }

    /// Returns the dibit counter value.
    pub fn dibit_count(&self) -> u16 {
        self.registers
            .demod_status()
            .read()
            .dibit_count()
            .bits()
    }

    /// Returns true if the demod has overflowed.
    pub fn demod_overflow(&self) -> bool {
        self.registers
            .demod_status()
            .read()
            .demod_overflow()
            .bit()
    }

    /// Returns the current DMA write address for the dibit channel.
    pub fn dibit_next_address(&self) -> u32 {
        self.registers
            .dibit_next_address()
            .read()
            .next_address()
            .bits()
    }

    /// Reads new dibit DMA buffers since the last call.
    ///
    /// Returns an iterator of byte slices, each containing packed 64-bit
    /// dibit words. Call `cache_invalidate` is handled internally.
    pub fn read_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::Dibit)
    }

    // ── Traffic channel DDC ──────────────────────────────────────

    /// Sets the traffic channel DDC NCO frequency word directly.
    pub fn set_traffic_ddc_frequency_word(&self, nco_word: u32) {
        self.registers
            .traffic_ddc_frequency()
            .modify(|_, w| unsafe { w.frequency().bits(nco_word) });
    }

    /// Sets the traffic channel DDC NCO frequency in Hz.
    pub fn set_traffic_ddc_frequency(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        let half = 0.5 * sample_rate_hz;
        if !(-half..=half).contains(&frequency_hz) {
            anyhow::bail!(
                "traffic DDC frequency {frequency_hz} Hz out of range"
            );
        }
        let nco_word = freq_to_nco(frequency_hz, sample_rate_hz);
        self.set_traffic_ddc_frequency_word(nco_word);
        Ok(())
    }

    /// Enables or disables the traffic channel DDC input.
    pub fn set_traffic_ddc_enable(&self, enable: bool) {
        self.registers
            .traffic_ddc_control()
            .modify(|_, w| w.enable_input().bit(enable));
    }

    /// Enables or disables the traffic channel demodulator.
    pub fn set_traffic_demod_enable(&self, enable: bool) {
        self.registers
            .traffic_demod_control()
            .modify(|_, w| w.demod_enable().bit(enable));
    }

    /// Starts the traffic channel DMA.
    pub fn traffic_demod_start(&self) {
        self.registers
            .traffic_demod_control()
            .modify(|_, w| w.start().set_bit());
    }

    /// Stops the traffic channel DMA.
    pub fn traffic_demod_stop(&self) {
        self.registers
            .traffic_demod_control()
            .modify(|_, w| w.stop().set_bit());
    }

    /// Returns the traffic DMA write address.
    pub fn traffic_next_address(&self) -> u32 {
        self.registers
            .traffic_next_address()
            .read()
            .next_address()
            .bits()
    }

    /// Reads new traffic DMA buffers since the last call.
    pub fn read_traffic_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::Traffic)
    }

    // ── DMA buffer helpers ───────────────────────────────────────

    fn read_dma_buffers(&mut self, channel: DmaChannel) -> Vec<&[u8]> {
        let (dma, last_addr, current_addr) = match channel {
            DmaChannel::Dibit => (
                &self.dibit_dma,
                &mut self.dibit_last_addr,
                self.registers
                    .dibit_next_address()
                    .read()
                    .next_address()
                    .bits(),
            ),
            DmaChannel::Traffic => (
                &self.traffic_dma,
                &mut self.traffic_last_addr,
                self.registers
                    .traffic_next_address()
                    .read()
                    .next_address()
                    .bits(),
            ),
        };

        let buf_size = dma.buffer_size();
        let num_bufs = dma.num_buffers();
        if buf_size == 0 || num_bufs == 0 {
            return Vec::new();
        }

        // Convert addresses to buffer indices
        let current_idx = (current_addr as usize / buf_size) % num_bufs;

        let start_idx = match *last_addr {
            Some(addr) => ((addr as usize / buf_size) + 1) % num_bufs,
            None => {
                // First call — start from current buffer
                *last_addr = Some(current_addr);
                return Vec::new();
            }
        };

        *last_addr = Some(current_addr);

        // Collect new buffers
        let mut result = Vec::new();
        let mut idx = start_idx;
        while idx != current_idx {
            if let Err(e) = dma.cache_invalidate(idx) {
                tracing::warn!("cache invalidate failed for buffer {idx}: {e}");
                break;
            }
            result.push(dma.buffer_as_slice(idx));
            idx = (idx + 1) % num_bufs;
        }
        result
    }
}

enum DmaChannel {
    Dibit,
    Traffic,
}

/// Converts a frequency offset to a 28-bit NCO phase increment.
fn freq_to_nco(frequency_hz: f64, sample_rate_hz: f64) -> u32 {
    const NCO_WIDTH: u32 = 28;
    let scale = (1u64 << NCO_WIDTH) as f64;
    let cycles_per_sample = frequency_hz / sample_rate_hz;
    (cycles_per_sample * scale).round() as i32 as u32
}

// ── Interrupt handler ────────────────────────────────────────────────

/// Waiter for a specific interrupt source.
#[derive(Clone)]
pub struct InterruptWaiter {
    notify: Arc<Notify>,
}

impl InterruptWaiter {
    /// Waits for the next interrupt notification.
    pub async fn wait(&self) {
        self.notify.notified().await;
    }
}

/// Interrupt handler for the P25 FPGA core.
///
/// Runs in a background task, reads the UIO interrupt, checks the
/// interrupt status register, and notifies the appropriate waiters.
pub struct InterruptHandler {
    uio: Uio,
    registers: Registers,
    notify_dibit_dma: Arc<Notify>,
    notify_traffic_dma: Arc<Notify>,
}

impl InterruptHandler {
    fn new(uio: Uio, registers: Registers) -> Self {
        InterruptHandler {
            uio,
            registers,
            notify_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_dma: Arc::new(Notify::new()),
        }
    }

    /// Returns a waiter for dibit DMA completion interrupts.
    pub fn waiter_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_dibit_dma.clone(),
        }
    }

    /// Returns a waiter for traffic DMA completion interrupts.
    pub fn waiter_traffic_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_dma.clone(),
        }
    }

    /// Runs the interrupt handler loop.
    ///
    /// This should be spawned as a background tokio task.
    pub async fn run(mut self) -> Result<()> {
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            if interrupts.dibit_dma().bit() {
                self.notify_dibit_dma.notify_waiters();
            }
            if interrupts.traffic_dma().bit() {
                self.notify_traffic_dma.notify_waiters();
            }
        }
    }
}

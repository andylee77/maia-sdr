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

    /// Configures the complete DDC: FIR coefficients, decimation, NCO.
    ///
    /// This must be called before enabling the DDC. Loads the 3-stage
    /// FIR filters (P25 12.5 kHz channel filter, 128x decimation) and
    /// programs the NCO frequency for the given channel offset.
    pub fn configure_ddc(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        // P25 channel filter: 8 MSPS -> 62.5 kSPS (16x4x2 = 128x)
        // Designed with scipy.signal.firwin, Kaiser window, 18-bit quantized
        self.load_fir1(&P25_FIR1_COEFFS, P25_DEC1)?;
        self.load_fir2(&P25_FIR2_COEFFS, P25_DEC2)?;
        self.load_fir3(&P25_FIR3_COEFFS, P25_DEC3)?;

        // Enable all 3 stages (no bypass)
        self.registers.ddc_control().modify(|_, w| {
            w.bypass2().clear_bit().bypass3().clear_bit()
        });

        self.set_ddc_frequency(frequency_hz, sample_rate_hz)?;

        tracing::info!(
            "DDC configured: 3-stage FIR ({}/{}/{} taps), {}x{}x{}={}x decimation",
            P25_FIR1_COEFFS.len(), P25_FIR2_COEFFS.len(), P25_FIR3_COEFFS.len(),
            P25_DEC1, P25_DEC2, P25_DEC3, P25_DEC1 * P25_DEC2 * P25_DEC3,
        );
        Ok(())
    }

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

    // ── FIR coefficient loading (private) ───────────────────────

    /// Load FIR1 (stage 1, FIR4DSP): 4 DSPs, folded coefficient layout.
    fn load_fir1(&self, coefficients: &[i32], decimation: usize) -> Result<()> {
        self.load_fir_4dsp(coefficients, decimation, 0)?;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        let odd = branch_len % 2 == 1;
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.ddc_decimation().modify(|_, w| unsafe {
            w.decimation1().bits(dec)
        });
        self.registers.ddc_control().modify(|_, w| unsafe {
            w.operations_minus_one1().bits(opm1)
                .odd_operations1().bit(odd)
        });
        Ok(())
    }

    /// Load FIR2 (stage 2, FIR2DSP): 2 DSPs, no folding.
    fn load_fir2(&self, coefficients: &[i32], decimation: usize) -> Result<()> {
        self.load_fir_2dsp(coefficients, decimation, 256)?;
        let operations = coefficients.len().div_ceil(decimation);
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.ddc_decimation().modify(|_, w| unsafe {
            w.decimation2().bits(dec)
        });
        self.registers.ddc_control().modify(|_, w| unsafe {
            w.operations_minus_one2().bits(opm1)
        });
        Ok(())
    }

    /// Load FIR3 (stage 3, FIR4DSP): 4 DSPs, folded coefficient layout.
    fn load_fir3(&self, coefficients: &[i32], decimation: usize) -> Result<()> {
        self.load_fir_4dsp(coefficients, decimation, 512)?;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        let odd = branch_len % 2 == 1;
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.ddc_decimation().modify(|_, w| unsafe {
            w.decimation3().bits(dec)
        });
        self.registers.ddc_control().modify(|_, w| unsafe {
            w.operations_minus_one3().bits(opm1)
                .odd_operations3().bit(odd)
        });
        Ok(())
    }

    /// Write polyphase-reordered coefficients for a FIR4DSP stage (folded).
    fn load_fir_4dsp(
        &self,
        coefficients: &[i32],
        decimation: usize,
        addr_offset: usize,
    ) -> Result<()> {
        const NUM_ADDR: usize = 256;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        if operations * decimation > NUM_ADDR / 2 {
            anyhow::bail!("FIR4DSP coefficients too long for RAM");
        }
        for addr in 0..NUM_ADDR {
            let (off, fold) = if addr >= NUM_ADDR / 2 {
                (1, NUM_ADDR / 2)
            } else {
                (0, 0)
            };
            let k = (addr - fold) / operations;
            let coeff = if k >= decimation {
                0
            } else {
                let j = (addr - fold) % operations;
                let n = (2 * j + off) * decimation + (decimation - 1 - k);
                *coefficients.get(n).unwrap_or(&0)
            };
            let waddr = u16::try_from(addr + addr_offset).unwrap();
            self.registers
                .ddc_coeff_addr()
                .modify(|_, w| unsafe { w.coeff_waddr().bits(waddr) });
            self.registers.ddc_coeff().modify(|_, w| unsafe {
                w.coeff_wren().bit(true).coeff_wdata().bits(coeff as u32)
            });
        }
        Ok(())
    }

    /// Write polyphase-reordered coefficients for a FIR2DSP stage (no fold).
    fn load_fir_2dsp(
        &self,
        coefficients: &[i32],
        decimation: usize,
        addr_offset: usize,
    ) -> Result<()> {
        const NUM_ADDR: usize = 128;
        let operations = coefficients.len().div_ceil(decimation);
        if operations * decimation > NUM_ADDR {
            anyhow::bail!("FIR2DSP coefficients too long for RAM");
        }
        for addr in 0..NUM_ADDR {
            let k = addr / operations;
            let coeff = if k >= decimation {
                0
            } else {
                let j = addr % operations;
                let n = j * decimation + (decimation - 1 - k);
                *coefficients.get(n).unwrap_or(&0)
            };
            let waddr = u16::try_from(addr + addr_offset).unwrap();
            self.registers
                .ddc_coeff_addr()
                .modify(|_, w| unsafe { w.coeff_waddr().bits(waddr) });
            self.registers.ddc_coeff().modify(|_, w| unsafe {
                w.coeff_wren().bit(true).coeff_wdata().bits(coeff as u32)
            });
        }
        Ok(())
    }

    // ── Control channel demod ────────────────────────────────────

    /// Enables or disables the C4FM demodulator.
    pub fn set_demod_enable(&self, enable: bool) {
        self.registers
            .demod_control()
            .modify(|_, w| w.demod_enable().bit(enable));
    }

    /// Returns the dibit counter value (16-bit, wraps).
    pub fn dibit_count(&self) -> u16 {
        self.registers
            .demod_status()
            .read()
            .dibit_count()
            .bits()
    }

    /// Returns true if the demod has overflowed (sticky).
    pub fn demod_overflow(&self) -> bool {
        self.registers
            .demod_status()
            .read()
            .demod_overflow()
            .bit()
    }

    /// Returns the index of the most recently completed sub-buffer.
    /// Initialised to all-ones (-1) so the first read after enable
    /// indicates "no buffers completed yet".
    pub fn dibit_last_buffer(&self) -> u8 {
        self.registers
            .demod_status()
            .read()
            .last_buffer()
            .bits()
    }

    /// Returns the current AW write address for the dibit channel (debug).
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

    /// Returns the index of the most recently completed traffic sub-buffer.
    pub fn traffic_last_buffer(&self) -> u8 {
        self.registers
            .traffic_demod_status()
            .read()
            .last_buffer()
            .bits()
    }

    /// Returns the current traffic DMA AW write address (debug).
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

    /// Reads new dibit/traffic ring sub-buffers since the last call.
    ///
    /// Uses the FPGA's `last_buffer` field (updated by the ring DMA's
    /// B-channel completion logic) to determine which sub-buffers are
    /// newly available. Sub-buffer N becomes valid the cycle after the
    /// FPGA finishes writing it (response received from DDR).
    fn read_dma_buffers(&mut self, channel: DmaChannel) -> Vec<&[u8]> {
        let (dma, last_seen_idx, current_last) = match channel {
            DmaChannel::Dibit => (
                &self.dibit_dma,
                &mut self.dibit_last_addr,
                self.registers
                    .demod_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::Traffic => (
                &self.traffic_dma,
                &mut self.traffic_last_addr,
                self.registers
                    .traffic_demod_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
        };

        let num_bufs = dma.num_buffers();
        if num_bufs == 0 {
            return Vec::new();
        }

        // Mask to num_buffers_log2 bits (last_buffer is N bits wide)
        let mask = (num_bufs - 1) as u32;
        let current_idx = (current_last & mask) as usize;

        let start_idx = match *last_seen_idx {
            Some(prev) => {
                let prev_idx = (prev & mask) as usize;
                if prev_idx == current_idx {
                    // No new buffers since last poll
                    return Vec::new();
                }
                (prev_idx + 1) % num_bufs
            }
            None => {
                // First call: snapshot current and return empty.
                // last_buffer initialises to all-1s in HW; on the first
                // sub-buffer completion it wraps to 0.
                *last_seen_idx = Some(current_last);
                return Vec::new();
            }
        };

        *last_seen_idx = Some(current_last);

        // Collect new sub-buffers, walking the ring forward to current_idx
        let mut result = Vec::new();
        let mut idx = start_idx;
        loop {
            if let Err(e) = dma.cache_invalidate(idx) {
                tracing::warn!("cache invalidate failed for buffer {idx}: {e}");
                break;
            }
            result.push(dma.buffer_as_slice(idx));
            if idx == current_idx {
                break;
            }
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

// ── P25 DDC filter coefficients ──────────────────────────────────────
//
// 3-stage FIR decimation: 8 MSPS -> 62.5 kSPS (128x = 16 x 4 x 2)
// P25 Phase 1 channel: 12.5 kHz (±6.25 kHz passband)
// Output: 62.5 kSPS = 13.0 samples/symbol at 4800 baud
//
// Designed with scipy.signal.firwin, Kaiser window, 18-bit quantized.

const P25_DEC1: usize = 16;
const P25_DEC2: usize = 4;
const P25_DEC3: usize = 2;

// Stage 1 (FIR4DSP): 48 taps, 200 kHz cutoff, Kaiser beta=6, >137 dB stopband
#[rustfmt::skip]
const P25_FIR1_COEFFS: &[i32] = &[
    -277, -402, -419, -220, 325, 1372, 3086, 5640,
    9190, 13870, 19769, 26920, 35285, 44750, 55123, 66131,
    77436, 88647, 99339, 109082, 117460, 124103, 128712, 131071,
    131071, 128712, 124103, 117460, 109082, 99339, 88647, 77436,
    66131, 55123, 44750, 35285, 26920, 19769, 13870, 9190,
    5640, 3086, 1372, 325, -220, -419, -402, -277,
];

// Stage 2 (FIR2DSP): 32 taps, 50 kHz cutoff, Kaiser beta=7, >140 dB stopband
#[rustfmt::skip]
const P25_FIR2_COEFFS: &[i32] = &[
    -25, 87, 530, 1287, 1841, 1156, -1803, -7068,
    -12694, -14613, -7853, 11060, 41616, 78199, 111332, 131071,
    131071, 111332, 78199, 41616, 11060, -7853, -14613, -12694,
    -7068, -1803, 1156, 1841, 1287, 530, 87, -25,
];

// Stage 3 (FIR4DSP): 64 taps, 8 kHz cutoff, Kaiser beta=9, >166 dB stopband
#[rustfmt::skip]
const P25_FIR3_COEFFS: &[i32] = &[
    1, -8, -37, -92, -173, -258, -305, -250,
    -21, 434, 1121, 1959, 2766, 3261, 3102, 1965,
    -357, -3835, -8116, -12478, -15864, -17007, -14631, -7704,
    4302, 21207, 42029, 65020, 87870, 108017, 123044, 131071,
    131071, 123044, 108017, 87870, 65020, 42029, 21207, 4302,
    -7704, -14631, -17007, -15864, -12478, -8116, -3835, -357,
    1965, 3102, 3261, 2766, 1959, 1121, 434, -21,
    -250, -305, -258, -173, -92, -37, -8, 1,
];

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
        let mut total_irqs: u64 = 0;
        let mut dibit_irqs: u64 = 0;
        let mut traffic_irqs: u64 = 0;
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            let dibit = interrupts.dibit_dma().bit();
            let traffic = interrupts.traffic_dma().bit();
            total_irqs += 1;
            if dibit {
                dibit_irqs += 1;
                self.notify_dibit_dma.notify_waiters();
            }
            if traffic {
                traffic_irqs += 1;
                self.notify_traffic_dma.notify_waiters();
            }
            // Log first 10 then every 64th to avoid flooding
            if total_irqs <= 10 || total_irqs % 64 == 0 {
                tracing::info!(
                    target: "p25_irq",
                    "IRQ #{total_irqs}: dibit={dibit} traffic={traffic} \
                     (totals dibit={dibit_irqs} traffic={traffic_irqs})"
                );
            }
        }
    }
}

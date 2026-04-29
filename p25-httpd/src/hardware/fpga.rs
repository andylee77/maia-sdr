//! FPGA IP core driver.
//!
//! Accesses the P25 core registers via UIO (userspace I/O) and reads
//! ring-DMA buffers via the maia-sdr kernel module rxbuffer device.
//! Pattern adapted from maia-httpd/src/fpga.rs.
//!
//! ## Phase 10.8 PS-side refactor (2026-04-23)
//!
//! The HDL was reorganised so the PS side no longer owns:
//!
//! - the PS C4FM chain (both control `dibit_dma` and traffic `traffic_dma`
//!   rings, plus every `*_demod_*` register), and
//! - the post-RRC matched-filter IQ taps (`lsm_iq_dma`,
//!   `traffic_lsm_iq_dma`), and
//! - the post-PLL IQ taps (`post_pll_iq_dma`,
//!   `traffic_post_pll_iq_dma`).
//!
//! In their place the gateware now exposes two new taps that sit
//! *directly on the LSM decision-point symbols* — one per chain:
//!
//! - `pre_diff_iq_dma` / `traffic_pre_diff_iq_dma` at 9.6 kSPS
//!   (2 samples/symbol), carrier-derotated + AGC-scaled, but taken
//!   *before* the differential-demod / slicer stage. That means the
//!   4-cluster LSM constellation sits at (±1, ±1) — the ideal shape
//!   the dashboard plots want anyway — without any PS post-processing.
//!
//! Everything retired here is also gone from the PAC; this file now
//! only touches register banks that still exist in `p25-pac` post-HDL-
//! refactor.

use anyhow::{Context, Result};
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::Notify;

use crate::hardware::ddc_presets::DdcPreset;
use crate::hardware::rxbuffer::RxBuffer;
use crate::hardware::uio::{Mapping, Uio};

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
/// Provides register access for DDC configuration, LSM demod control,
/// and DMA buffer reading for both control and traffic chains.
pub struct IpCore {
    registers: Registers,
    iq_dma: RxBuffer,
    lsm_dibit_dma: RxBuffer,
    /// Phase 7A.2: traffic-side LSM dibit DMA ring. Mirrors
    /// `lsm_dibit_dma` on the control side. UIO device
    /// `p25-traffic-lsm-dibit`.
    traffic_lsm_dibit_dma: RxBuffer,
    /// Traffic-side post-DDC IQ ring, mirror of `iq_dma` on the
    /// control side. UIO device `p25-traffic-iq`.
    traffic_iq_dma: RxBuffer,
    /// Phase 10.8 (2026-04-23): control-chain pre-differential IQ ring.
    /// Tapped inside `LsmDemod` after `LsmPllRotate` + AGC but BEFORE
    /// the diff-demod / slicer. Samples sit on the LSM ideal
    /// constellation (±1, ±1). 9.6 kSPS (2 samples per symbol
    /// interleaved). UIO `p25-pre-diff-iq`. Feeds the Plots tab
    /// constellation + eye + `/api/deviation` + `/api/distribution`.
    pre_diff_iq_dma: RxBuffer,
    /// Phase 10.8 traffic-side twin of `pre_diff_iq_dma`. UIO
    /// `p25-traffic-pre-diff-iq`.
    traffic_pre_diff_iq_dma: RxBuffer,
    /// Wideband spectrometer output ring (4096-bin FFT, HW-integrated,
    /// pre-DDC tap on `rxiq_cdc`). UIO `p25-wideband-spec`. Feeds
    /// `/api/spectrum_wide`; no PS FFT.
    wideband_spec_dma: RxBuffer,

    iq_last_addr: Option<u32>,
    lsm_dibit_last_addr: Option<u32>,
    traffic_lsm_dibit_last_addr: Option<u32>,
    traffic_iq_last_addr: Option<u32>,
    pre_diff_iq_last_addr: Option<u32>,
    traffic_pre_diff_iq_last_addr: Option<u32>,
    wideband_spec_last_buffer: Option<u8>,
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

        // Open DMA buffer devices. Every Phase 10.8 flashed image MUST
        // expose exactly this set — the DT carve-outs are the single
        // source of truth for what the PS can tap. Any `open` failure
        // here bails the whole `take()` so bring-up doesn't silently
        // run against a partial ring set.
        let iq_dma = RxBuffer::new("p25-iq")
            .await
            .context("failed to open p25-iq DMA buffer")?;
        let lsm_dibit_dma = RxBuffer::new("p25-lsm-dibit")
            .await
            .context("failed to open p25-lsm-dibit DMA buffer")?;
        let traffic_lsm_dibit_dma = RxBuffer::new("p25-traffic-lsm-dibit")
            .await
            .context("failed to open p25-traffic-lsm-dibit DMA buffer")?;
        let traffic_iq_dma = RxBuffer::new("p25-traffic-iq")
            .await
            .context("failed to open p25-traffic-iq DMA buffer")?;
        let pre_diff_iq_dma = RxBuffer::new("p25-pre-diff-iq")
            .await
            .context("failed to open p25-pre-diff-iq DMA buffer")?;
        let traffic_pre_diff_iq_dma = RxBuffer::new("p25-traffic-pre-diff-iq")
            .await
            .context("failed to open p25-traffic-pre-diff-iq DMA buffer")?;
        let wideband_spec_dma = RxBuffer::new("p25-wideband-spec")
            .await
            .context("failed to open p25-wideband-spec DMA buffer")?;

        let ip_core = IpCore {
            registers,
            iq_dma,
            lsm_dibit_dma,
            traffic_lsm_dibit_dma,
            traffic_iq_dma,
            pre_diff_iq_dma,
            traffic_pre_diff_iq_dma,
            wideband_spec_dma,
            iq_last_addr: None,
            lsm_dibit_last_addr: None,
            traffic_lsm_dibit_last_addr: None,
            traffic_iq_last_addr: None,
            pre_diff_iq_last_addr: None,
            traffic_pre_diff_iq_last_addr: None,
            wideband_spec_last_buffer: None,
        };

        let interrupt_handler = InterruptHandler::new(uio, interrupt_registers);
        Ok((ip_core, interrupt_handler))
    }

    // ── Control channel DDC ──────────────────────────────────────

    /// Configures the complete DDC: FIR coefficients, decimation, NCO.
    ///
    /// `preset` selects the AD9361 sample rate and the matching FIR
    /// coefficient / decimation tables. Every preset produces 62.5 kSPS
    /// at the DDC output by construction, so the downstream LSM chain
    /// stays valid across preset changes. `frequency_hz` is the NCO
    /// offset from the RX LO at `preset.sample_rate_hz`.
    pub fn configure_ddc(
        &self,
        frequency_hz: f64,
        preset: &DdcPreset,
    ) -> Result<()> {
        self.load_fir1(preset.fir1_coeffs, preset.decim1)?;
        self.load_fir2(preset.fir2_coeffs, preset.decim2)?;
        self.load_fir3(preset.fir3_coeffs, preset.decim3)?;

        // Enable all 3 stages (no bypass)
        self.registers.ddc_control().modify(|_, w| {
            w.bypass2().clear_bit().bypass3().clear_bit()
        });

        self.set_ddc_frequency(frequency_hz, preset.sample_rate_hz as f64)?;

        tracing::info!(
            "DDC configured: preset={} NCO={} Hz, 3-stage FIR ({}/{}/{} taps), \
             {}x{}x{}={}x decimation, output={} Hz (25 kHz rejection {:+.1} dB)",
            preset.name,
            frequency_hz as i64,
            preset.fir1_coeffs.len(),
            preset.fir2_coeffs.len(),
            preset.fir3_coeffs.len(),
            preset.decim1, preset.decim2, preset.decim3,
            preset.total_decim(),
            preset.sample_rate_hz as u64 / preset.total_decim() as u64,
            preset.rejection_25k_db,
        );
        Ok(())
    }

    /// Sets the control channel DDC NCO frequency.
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

    // ── Traffic channel DDC ──────────────────────────────────────

    /// Configures the traffic channel DDC: decimation, operations, NCO.
    ///
    /// Mirrors `configure_ddc()` but writes the `traffic_*` register
    /// bank instead of the control bank. FIR coefficients are shared
    /// with the control DDC at the HDL level (see
    /// `maia-hdl/p25_hdl/p25_top.py`), so `configure_ddc()` must be
    /// called first.
    pub fn configure_traffic_ddc(
        &self,
        frequency_hz: f64,
        preset: &DdcPreset,
    ) -> Result<()> {
        let dec1 = u8::try_from(preset.decim1).unwrap();
        let dec2 = u8::try_from(preset.decim2).unwrap();
        let dec3 = u8::try_from(preset.decim3).unwrap();

        // FIR1 (FIR4DSP, folded): same math as load_fir1.
        let fir1_branch_len = preset.fir1_coeffs.len().div_ceil(preset.decim1);
        let fir1_operations = fir1_branch_len.div_ceil(2);
        let fir1_odd = fir1_branch_len % 2 == 1;
        let opm1_1 = u8::try_from(fir1_operations - 1).unwrap();

        // FIR2 (FIR2DSP, no folding): same math as load_fir2.
        let fir2_operations = preset.fir2_coeffs.len().div_ceil(preset.decim2);
        let opm1_2 = u8::try_from(fir2_operations - 1).unwrap();

        // FIR3 (FIR4DSP, folded): same math as load_fir3.
        let fir3_branch_len = preset.fir3_coeffs.len().div_ceil(preset.decim3);
        let fir3_operations = fir3_branch_len.div_ceil(2);
        let fir3_odd = fir3_branch_len % 2 == 1;
        let opm1_3 = u8::try_from(fir3_operations - 1).unwrap();

        self.registers
            .traffic_ddc_decimation()
            .modify(|_, w| unsafe {
                w.decimation1()
                    .bits(dec1)
                    .decimation2()
                    .bits(dec2)
                    .decimation3()
                    .bits(dec3)
            });

        self.registers.traffic_ddc_control().modify(|_, w| unsafe {
            w.operations_minus_one1()
                .bits(opm1_1)
                .operations_minus_one2()
                .bits(opm1_2)
                .operations_minus_one3()
                .bits(opm1_3)
                .odd_operations1()
                .bit(fir1_odd)
                .odd_operations3()
                .bit(fir3_odd)
                .bypass2()
                .clear_bit()
                .bypass3()
                .clear_bit()
        });

        self.set_traffic_ddc_frequency(
            frequency_hz, preset.sample_rate_hz as f64)?;

        tracing::info!(
            "Traffic DDC configured: preset={} NCO={} Hz, \
             {}x{}x{}={}x decimation (FIR coeffs shared with control DDC), \
             output={} Hz",
            preset.name,
            frequency_hz as i64,
            preset.decim1, preset.decim2, preset.decim3,
            preset.total_decim(),
            preset.sample_rate_hz as u64 / preset.total_decim() as u64,
        );
        Ok(())
    }

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

    // ── IQ ring (control, post-DDC) ──────────────────────────────
    //
    // Per 64-bit DMA word: { im[1] s16, re[1] s16, im[0] s16, re[0] s16 }
    // — i.e. natural little-endian interleaved-IQ byte order.

    /// Enables or disables the post-DDC IQ ring DMA. Level-triggered.
    pub fn set_iq_dma_enable(&self, enable: bool) {
        self.registers
            .iq_dma_control()
            .modify(|_, w| w.iq_enable().bit(enable));
    }

    /// Returns the index of the most recently completed IQ sub-buffer.
    pub fn iq_last_buffer(&self) -> u8 {
        self.registers
            .iq_dma_status()
            .read()
            .last_buffer()
            .bits()
    }

    /// Reads and clears the IQ ring overflow latch (Rsticky bit).
    pub fn iq_overflow(&self) -> bool {
        self.registers
            .iq_dma_status()
            .read()
            .iq_overflow()
            .bit()
    }

    /// Returns the current IQ DMA AW write address (debug).
    pub fn iq_next_address(&self) -> u32 {
        self.registers
            .iq_next_address()
            .read()
            .next_address()
            .bits()
    }

    /// Reads new IQ ring sub-buffers since the last call.
    pub fn read_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::Iq)
    }

    // ── Traffic-channel post-DDC IQ ring DMA ─────────────────────

    /// Enables or disables the traffic-side post-DDC IQ ring DMA.
    pub fn set_traffic_iq_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_iq_dma_control()
            .modify(|_, w| w.traffic_iq_enable().bit(enable));
    }

    /// Reads new traffic IQ ring sub-buffers since the last call.
    pub fn read_traffic_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::TrafficIq)
    }

    // ── Pre-differential IQ rings (Phase 10.8 2026-04-23) ─────────
    //
    // Tap inside `LsmDemod` after `LsmPllRotate` + per-symbol AGC but
    // before the differential demod / slicer. Samples sit on the
    // 4-cluster (±1, ±1) LSM constellation at 9.6 kSPS (2 samples per
    // symbol interleaved, same packing as the retired `post_pll_iq_dma`).
    // This is the "direct constellation" — no PS post-processing is
    // needed to recover the clusters.

    /// Enables or disables the control-chain pre-diff IQ ring DMA.
    pub fn set_pre_diff_iq_dma_enable(&self, enable: bool) {
        self.registers
            .pre_diff_iq_dma_control()
            .modify(|_, w| w.pre_diff_iq_enable().bit(enable));
    }

    /// Enables or disables the traffic-chain pre-diff IQ ring DMA.
    pub fn set_traffic_pre_diff_iq_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_pre_diff_iq_dma_control()
            .modify(|_, w| w.traffic_pre_diff_iq_enable().bit(enable));
    }

    /// Reads new control-chain pre-diff IQ sub-buffers since the last call.
    pub fn read_pre_diff_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::PreDiffIq)
    }

    /// Reads new traffic-chain pre-diff IQ sub-buffers since the last call.
    pub fn read_traffic_pre_diff_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::TrafficPreDiffIq)
    }

    // ── Wideband spectrometer ───────────────────────────────────
    //
    // Pre-DDC FFT tapped off `rxiq_cdc` at the full AD9361 sample rate
    // (preset-dependent, 2–16 MSPS). 4096-bin output, hardware-
    // averaged at `spec_num_integrations` samples per spectrum.

    /// Master enable for the wideband spectrometer.
    pub fn set_wideband_spec_enable(&self, enable: bool) {
        self.registers
            .spec_control()
            .modify(|_, w| w.spec_enable().bit(enable));
    }

    /// Sets the number of FFT frames averaged per output spectrum.
    pub fn set_wideband_spec_integrations(&self, n: u16) {
        self.registers
            .spec_control()
            .modify(|_, w| unsafe {
                w.spec_num_integrations().bits(n & 0x3FF)
            });
    }

    /// Peak-hold mode (max-value integration) versus the default
    /// average-power integration.
    pub fn set_wideband_spec_peak_detect(&self, enable: bool) {
        self.registers
            .spec_control()
            .modify(|_, w| w.spec_peak_detect().bit(enable));
    }

    /// Fires a 1-cycle abort pulse: ends the in-flight integration
    /// early and flushes to the next DMA buffer.
    pub fn wideband_spec_abort(&self) {
        self.registers
            .spec_control()
            .modify(|_, w| w.spec_abort().bit(true));
    }

    /// Returns the most recently completed wideband spectrum as 32 KB
    /// of packed mantissa + exponent (see
    /// `services/spectrum.rs::wideband_power_db` for unpacking).
    /// Returns `None` if no new integration has completed since the
    /// last call.
    pub fn read_wideband_spec_buffer(&mut self) -> Option<&[u8]> {
        // `spec_last_buffer` is `BitReader` (1 bit) now that
        // wideband_spec_dma_num_buffers_log2 = 1. svd2rust emits
        // `.bit() -> bool` instead of `.bits() -> u8` when the
        // field is a single bit. Cast to u8 so the surrounding code
        // (which stores it as Option<u8>) keeps working.
        let last: u8 = self
            .registers
            .spec_status()
            .read()
            .spec_last_buffer()
            .bit() as u8;
        if self.wideband_spec_last_buffer == Some(last) {
            return None;
        }
        self.wideband_spec_last_buffer = Some(last);
        let idx = last as usize;
        if let Err(e) = self.wideband_spec_dma.cache_invalidate(idx) {
            tracing::warn!(
                "cache invalidate failed for wideband_spec buf {idx}: {e}"
            );
            return None;
        }
        Some(self.wideband_spec_dma.buffer_as_slice(idx))
    }

    // ── LSM chain (control) ─────────────────────────────────────

    /// Master enable for the LSM chain (decimator + FIRs + LsmDemod).
    pub fn set_lsm_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_enable().bit(enable));
    }

    /// Enables or disables the LSM dibit ring DMA. Level-triggered.
    pub fn set_lsm_dibit_dma_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_dibit_dma_enable().bit(enable));
    }

    /// Enables or disables the front-end LSM DC blocker. Production
    /// code should always set this to `true`.
    pub fn set_lsm_dc_block_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_dc_block_enable().bit(enable));
    }

    /// Enables or disables the per-symbol LSM AGC. Production code
    /// should always set this to `true` after boot.
    pub fn set_lsm_agc_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_agc_enable().bit(enable));
    }

    /// Reads back the `lsm_control` register as `(lsm_enable,
    /// lsm_dibit_dma_enable, lsm_dc_block_enable, lsm_agc_enable)`.
    pub fn lsm_control_readback(&self) -> (bool, bool, bool, bool) {
        let c = self.registers.lsm_control().read();
        (
            c.lsm_enable().bit(),
            c.lsm_dibit_dma_enable().bit(),
            c.lsm_dc_block_enable().bit(),
            c.lsm_agc_enable().bit(),
        )
    }

    /// Reads the `lsm_status` register and returns a coherent snapshot.
    pub fn lsm_status(&self) -> LsmStatusSnapshot {
        let s = self.registers.lsm_status().read();
        LsmStatusSnapshot {
            bch_busy: s.bch_busy().bit(),
            in_nid_window: s.in_nid_window().bit(),
            nid_event: s.nid_event().bit(),
            nid_valid: s.nid_valid().bit(),
            n_errors: s.n_errors().bits(),
            sync_distance: s.sync_distance().bits(),
            dibit_overflow: s.lsm_dibit_overflow().bit(),
        }
    }

    /// Reads the latched NAC/DUID of the most recent NID event.
    pub fn lsm_nid(&self) -> (u16, u8) {
        let n = self.registers.lsm_nid().read();
        (n.nac().bits(), n.duid().bits())
    }

    /// Reads the saturating NID drop counter.
    pub fn lsm_drop_count(&self) -> u16 {
        self.registers.lsm_drop_count().read().drop_count().bits()
    }

    /// Returns the index of the most recently completed LSM dibit
    /// sub-buffer.
    pub fn lsm_dibit_last_buffer(&self) -> u8 {
        self.registers
            .lsm_drop_count()
            .read()
            .lsm_dibit_last_buffer()
            .bits()
    }

    /// Returns the current AW write address for the LSM dibit channel.
    pub fn lsm_dibit_next_address(&self) -> u32 {
        self.registers
            .lsm_dibit_next()
            .read()
            .next_address()
            .bits()
    }

    /// Reads the debug taps: `pll_dbg` (signed Q2.13, live PLL accum)
    /// and `sample_point_dbg` (signed Q4.10, Gardner sample point).
    pub fn lsm_debug(&self) -> (i16, i16) {
        let d = self.registers.lsm_debug().read();
        (d.pll_dbg().bits() as i16, d.sample_point_dbg().bits() as i16)
    }

    /// Reads new LSM dibit DMA buffers since the last call.
    pub fn read_lsm_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::LsmDibit)
    }

    // ── Traffic-side LSM chain ──────────────────────────────────

    /// Master enable for the traffic-side LSM chain.
    pub fn set_traffic_lsm_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_enable().bit(enable));
    }

    /// Enables or disables the traffic LSM dibit ring DMA.
    pub fn set_traffic_lsm_dibit_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_dibit_dma_enable().bit(enable));
    }

    /// Enables or disables the traffic LSM front-end DC blocker.
    pub fn set_traffic_lsm_dc_block_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_dc_block_enable().bit(enable));
    }

    /// Enables or disables the per-symbol LSM AGC on the traffic chain.
    pub fn set_traffic_lsm_agc_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_agc_enable().bit(enable));
    }

    /// Pulse the traffic-side LSM chain runtime reset (W1P,
    /// self-clearing). Used between the DDC retune and the LSM
    /// re-enable to start the PLL acquisition from cold-boot
    /// semantics after a retune.
    pub fn pulse_traffic_lsm_reset(&self) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_reset().bit(true));
    }

    /// Write the traffic LSM warm-start seeds. The PLL seed (Q2.13
    /// signed) is latched into the traffic Costas accumulator and
    /// the AGC seed (Q9.7 unsigned, FPGA pads to Q9.11 internally)
    /// is latched into the traffic AGC gain register on the next
    /// `pulse_traffic_lsm_reset()`. Zero values fall back to the
    /// legacy cold-start (pll=0, gain=GAIN_INIT). See
    /// `retune_traffic_chain` for the standard call site.
    pub fn set_traffic_lsm_seeds(&self, pll_q213: i16, agc_q97: u16) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| unsafe {
                w.traffic_pll_seed().bits(pll_q213 as u16)
            });
        self.registers
            .traffic_lsm_agc_config()
            .modify(|_, w| unsafe { w.traffic_agc_seed().bits(agc_q97) });
    }

    /// Freeze-reset-thaw the traffic LSM chain across a DDC retune.
    ///
    /// Sequence:
    ///   1. `traffic_lsm_enable = 0`  — synchronous reset of the
    ///      `lsm_traffic_dom` clock domain inside LsmDemod.
    ///   2. Write the new DDC NCO frequency.
    ///   3. Wait 2 ms for the DDC FIR pipeline to flush so the LSM
    ///      PLL doesn't chase an old-NCO convolution transient.
    ///   4. `traffic_lsm_enable = 1`  — domain reset deasserts.
    ///   5. Pulse `traffic_lsm_reset` — explicit `reset_in` clears
    ///      the `reset_less=True` accumulators (PLL, timing, diff
    ///      slicer, sync, BCH sweep) inside one sync cycle.
    ///
    /// Without steps 1+3+5 the carryover of PLL state from the
    /// previous carrier produces corrupted dibits for hundreds of
    /// milliseconds — the Phase 7 "1 in 20 calls intelligible"
    /// symptom documented in doc/changes/037.
    pub fn retune_traffic_chain(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
        agc_seed_q97: u16,
    ) -> Result<SeedLoadDiag> {
        // Order matters here. Pre-2026-04-26 we did seed+reset AFTER
        // re-enabling the chain, which left a microsecond window
        // where the demod ran with stale pll_reg/gain values from
        // the previous freq before the reset pulse latched the new
        // seeds. On-target observation showed retune-path calls
        // still hit the cold-acquire fingerprint (~3.5 s to first
        // IMBE) while nco_skip-path calls converged in <100 ms —
        // the difference was that brief stale-state window. Now we
        // write seeds + pulse reset WHILE THE CHAIN IS DISABLED,
        // then flip enable. The chain comes alive already at the
        // seeded lock value with no transient pipeline state.
        self.set_traffic_lsm_enable(false);
        self.set_traffic_ddc_frequency(frequency_hz, sample_rate_hz)?;
        // Let the DDC FIR cascade flush before un-freezing the LSM
        // chain. 2 ms is ~3× the worst-case pipeline depth (600 us at
        // 8 MSPS with /4 /4 /8 = 176+128+256 taps). See
        // doc/changes/037 for the measurement that motivated this.
        std::thread::sleep(std::time::Duration::from_millis(2));
        // Read control-chain converged PLL + AGC. Both chains share
        // the same crystal trim, so this is the correct lock value
        // for any P25 carrier on this board (per the SDRTrunk PPM-
        // sweep evidence in CHANNELIZER_REDESIGN.md).
        //
        // 2026-04-26: AGC seeding disabled. Field observation: when
        // control chain converged to a high gain (e.g., 33×) on a
        // weak control-freq signal, seeding that into the traffic
        // chain on a different freq with a stronger signal saturated
        // the slicer — every dibit biased to 0b11, BCH "corrected"
        // every NID to DUID=0xF (TDU_LC), producing 16 false
        // TDU_LC dispatches per second and zero real LDU frames.
        // PLL seeding is fine (shared crystal trim makes it portable
        // across freqs); AGC seeding isn't (per-freq signal level
        // varies). Pass 0 → HDL Mux loads GAIN_INIT (= 1.0); the
        // AGC re-converges from unity in ~100 ms.
        let (pll_seed, _) = self.lsm_debug();
        let (traffic_pll_pre, _) = self.traffic_lsm_debug();
        let (traffic_agc_pre, _) = self.traffic_lsm_agc_debug();
        // 2026-04-26 per-freq AGC seed. Caller passes a Q9.7 cache
        // hit (or 0 for cold-start fallback to GAIN_INIT). PLL seed
        // is always the control chain's converged value.
        self.set_traffic_lsm_seeds(pll_seed, agc_seed_q97);
        // Pulse reset while still disabled. The reset_in pulse
        // propagates to the LSM submodules even with strobes gated
        // (gating is at LsmDecimator2's strobe input; reset_in is a
        // separate sync-domain signal that still drives the FSM/reg
        // assignments). After this the demod state is at the seeded
        // values, ready to run on the first strobe post-enable.
        self.pulse_traffic_lsm_reset();
        // 2026-04-26 seed-load diagnostic. PLL diff > 2 = HDL/CDC
        // bug. AGC diff is informational — when seed=0, HDL loads
        // GAIN_INIT (=128 Q9.7 = 1.0×), so post != 0 in that case
        // is expected. When seed!=0, post should match seed within
        // CDC jitter.
        let (traffic_pll_post, _) = self.traffic_lsm_debug();
        let (traffic_agc_post, _) = self.traffic_lsm_agc_debug();
        self.set_traffic_lsm_enable(true);
        Ok(SeedLoadDiag {
            pll_seed_written: pll_seed,
            traffic_pll_pre_reset: traffic_pll_pre,
            traffic_pll_post_reset: traffic_pll_post,
            agc_seed_written: agc_seed_q97,
            traffic_agc_pre_reset: traffic_agc_pre,
            traffic_agc_post_reset: traffic_agc_post,
        })
    }

    /// Quiesce the traffic LSM chain between calls. Counterpart to
    /// `retune_traffic_chain` — used by the follower task on
    /// Idle→timeout and on encryption tear-down so the LSM chain
    /// stops producing phantom NID events during the gap between
    /// calls.
    pub fn pause_traffic_chain(&self) {
        self.set_traffic_lsm_enable(false);
    }

    /// Reads back the `traffic_lsm_control` register as
    /// `(traffic_lsm_enable, traffic_lsm_dibit_dma_enable,
    /// traffic_lsm_dc_block_enable, traffic_lsm_agc_enable)`.
    pub fn traffic_lsm_control_readback(&self) -> (bool, bool, bool, bool) {
        let c = self.registers.traffic_lsm_control().read();
        (
            c.traffic_lsm_enable().bit(),
            c.traffic_lsm_dibit_dma_enable().bit(),
            c.traffic_lsm_dc_block_enable().bit(),
            c.traffic_lsm_agc_enable().bit(),
        )
    }

    /// Reads the `traffic_lsm_status` register as a coherent snapshot.
    pub fn traffic_lsm_status(&self) -> LsmStatusSnapshot {
        let s = self.registers.traffic_lsm_status().read();
        LsmStatusSnapshot {
            bch_busy: s.bch_busy().bit(),
            in_nid_window: s.in_nid_window().bit(),
            nid_event: s.nid_event().bit(),
            nid_valid: s.nid_valid().bit(),
            n_errors: s.n_errors().bits(),
            sync_distance: s.sync_distance().bits(),
            dibit_overflow: s.traffic_lsm_dibit_overflow().bit(),
        }
    }

    /// Reads the latched NAC/DUID of the most recent traffic LSM NID event.
    pub fn traffic_lsm_nid(&self) -> (u16, u8) {
        let n = self.registers.traffic_lsm_nid().read();
        (n.nac().bits(), n.duid().bits())
    }

    /// Reads the saturating traffic LSM NID drop counter.
    pub fn traffic_lsm_drop_count(&self) -> u16 {
        self.registers
            .traffic_lsm_drop_count()
            .read()
            .drop_count()
            .bits()
    }

    /// Returns the index of the most recently completed traffic LSM
    /// dibit sub-buffer.
    pub fn traffic_lsm_dibit_last_buffer(&self) -> u8 {
        self.registers
            .traffic_lsm_drop_count()
            .read()
            .traffic_lsm_dibit_last_buffer()
            .bits()
    }

    /// Returns the current AW write address for the traffic LSM dibit channel.
    pub fn traffic_lsm_dibit_next_address(&self) -> u32 {
        self.registers
            .traffic_lsm_dibit_next()
            .read()
            .next_address()
            .bits()
    }

    /// Reads the traffic LSM debug taps: `pll_dbg` (signed Q2.13) and
    /// `sample_point_dbg` (signed Q4.10).
    pub fn traffic_lsm_debug(&self) -> (i16, i16) {
        let d = self.registers.traffic_lsm_debug().read();
        (d.pll_dbg().bits() as i16, d.sample_point_dbg().bits() as i16)
    }

    /// Reads the control LSM AGC debug taps: `gain_dbg` (unsigned
    /// Q9.7 truncation of the Q9.11 gain register, range 0..500) and
    /// `mag_dbg` (unsigned Q1.15 most recent L2 magnitude of the
    /// AGC's input sample). At AGC steady state: `gain × mag / 2^11
    /// ≈ TARGET_RAW (= 32768 = 1.0 in Q1.15)`. If the product is
    /// consistently below TARGET the AGC is under-shooting (loop
    /// bandwidth / clamp / timing issue); if consistently above,
    /// over-shooting / saturation.
    pub fn lsm_agc_debug(&self) -> (u16, u16) {
        let d = self.registers.lsm_agc_debug().read();
        (d.agc_gain_dbg().bits(), d.agc_mag_dbg().bits())
    }

    /// Reads the traffic LSM AGC debug taps; see `lsm_agc_debug`.
    pub fn traffic_lsm_agc_debug(&self) -> (u16, u16) {
        let d = self.registers.traffic_lsm_agc_debug().read();
        (d.agc_gain_dbg().bits(), d.agc_mag_dbg().bits())
    }

    /// Reads the control LSM AGC idle-gate threshold (Q1.15 raw).
    /// Samples with `mag < mag_update_threshold` don't trigger an
    /// AGC update — the gate keeps idle-channel noise from dragging
    /// gain around. Default 256 (Q1.15 = -42 dBFS). 0 disables the
    /// gate (update on any non-zero mag).
    pub fn lsm_agc_threshold(&self) -> u16 {
        self.registers.lsm_agc_config().read()
            .mag_update_threshold().bits()
    }

    /// Writes the control LSM AGC idle-gate threshold.
    pub fn set_lsm_agc_threshold(&self, v: u16) {
        self.registers.lsm_agc_config()
            .modify(|_, w| unsafe { w.mag_update_threshold().bits(v) });
    }

    /// Reads the traffic LSM AGC idle-gate threshold; see
    /// `lsm_agc_threshold`.
    pub fn traffic_lsm_agc_threshold(&self) -> u16 {
        self.registers.traffic_lsm_agc_config().read()
            .mag_update_threshold().bits()
    }

    /// Writes the traffic LSM AGC idle-gate threshold.
    pub fn set_traffic_lsm_agc_threshold(&self, v: u16) {
        self.registers.traffic_lsm_agc_config()
            .modify(|_, w| unsafe { w.mag_update_threshold().bits(v) });
    }

    /// Reads new traffic LSM dibit DMA buffers since the last call.
    pub fn read_traffic_lsm_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::TrafficLsmDibit)
    }

    // ── DMA buffer helpers ───────────────────────────────────────

    /// Reads new ring sub-buffers since the last call.
    fn read_dma_buffers(&mut self, channel: DmaChannel) -> Vec<&[u8]> {
        let (dma, last_seen_idx, current_last) = match channel {
            DmaChannel::Iq => (
                &self.iq_dma,
                &mut self.iq_last_addr,
                self.registers
                    .iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::LsmDibit => (
                &self.lsm_dibit_dma,
                &mut self.lsm_dibit_last_addr,
                self.registers
                    .lsm_drop_count()
                    .read()
                    .lsm_dibit_last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::TrafficLsmDibit => (
                &self.traffic_lsm_dibit_dma,
                &mut self.traffic_lsm_dibit_last_addr,
                self.registers
                    .traffic_lsm_drop_count()
                    .read()
                    .traffic_lsm_dibit_last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::TrafficIq => (
                &self.traffic_iq_dma,
                &mut self.traffic_iq_last_addr,
                self.registers
                    .traffic_iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::PreDiffIq => (
                &self.pre_diff_iq_dma,
                &mut self.pre_diff_iq_last_addr,
                self.registers
                    .pre_diff_iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::TrafficPreDiffIq => (
                &self.traffic_pre_diff_iq_dma,
                &mut self.traffic_pre_diff_iq_last_addr,
                self.registers
                    .traffic_pre_diff_iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
        };

        let num_bufs = dma.num_buffers();
        if num_bufs == 0 {
            return Vec::new();
        }

        let mask = (num_bufs - 1) as u32;
        let current_idx = (current_last & mask) as usize;

        let start_idx = match *last_seen_idx {
            Some(prev) => {
                let prev_idx = (prev & mask) as usize;
                if prev_idx == current_idx {
                    return Vec::new();
                }
                (prev_idx + 1) % num_bufs
            }
            None => {
                *last_seen_idx = Some(current_last);
                return Vec::new();
            }
        };

        *last_seen_idx = Some(current_last);

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
    Iq,
    LsmDibit,
    TrafficLsmDibit,
    TrafficIq,
    PreDiffIq,
    TrafficPreDiffIq,
}

/// 2026-04-26 PLL seed-load diagnostic returned from
/// `retune_traffic_chain`. Lets the caller emit an event_log
/// entry visible via `/api/log` (no SSH/journal required).
///
/// `pll_seed_written` is the Q2.13 value the PS pulled from
/// `lsm_debug()` (control chain converged) and wrote to the
/// `traffic_pll_seed` register before pulsing reset.
/// `traffic_pll_post_reset` is the Q2.13 value read back from
/// `traffic_lsm_debug.pll_dbg` immediately after the reset pulse
/// — should equal `pll_seed_written` within CDC sync jitter
/// (~+/-2 LSB) if the seed is loading correctly.
/// `traffic_pll_pre_reset` is the value before the seed write,
/// useful for spotting drift between calls.
#[derive(Debug, Clone, Copy)]
pub struct SeedLoadDiag {
    pub pll_seed_written: i16,
    pub traffic_pll_pre_reset: i16,
    pub traffic_pll_post_reset: i16,
    /// 2026-04-26 AGC seed (Q9.7 raw u16). 0 = no cache hit; HDL
    /// Mux loads GAIN_INIT (= 1.0×) on seed_in==0. Non-zero =
    /// per-freq cache value passed in by the caller.
    pub agc_seed_written: u16,
    pub traffic_agc_pre_reset: u16,
    pub traffic_agc_post_reset: u16,
}

impl SeedLoadDiag {
    /// Drift between intended PLL seed and observed pll_dbg post-reset.
    pub fn drift(&self) -> i32 {
        (self.traffic_pll_post_reset as i32)
            - (self.pll_seed_written as i32)
    }
    /// True if the PLL readback is far enough from the intended
    /// seed that we suspect the seed didn't propagate.
    pub fn looks_buggy(&self) -> bool {
        self.drift().abs() > 2
    }
    /// AGC drift (post - intended). For seed=0 the HDL loads
    /// GAIN_INIT, so post can be quite different — only check
    /// AGC drift when a non-zero seed was passed.
    pub fn agc_drift(&self) -> i32 {
        (self.traffic_agc_post_reset as i32)
            - (self.agc_seed_written as i32)
    }
}

/// Snapshot of the `lsm_status` / `traffic_lsm_status` register read in
/// a single bus access.
#[derive(Debug, Clone, Copy)]
pub struct LsmStatusSnapshot {
    pub bch_busy: bool,
    pub in_nid_window: bool,
    pub nid_event: bool,
    pub nid_valid: bool,
    pub n_errors: u8,
    pub sync_distance: u8,
    pub dibit_overflow: bool,
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
pub struct InterruptHandler {
    uio: Uio,
    registers: Registers,
    notify_iq_dma: Arc<Notify>,
    notify_lsm_dibit_dma: Arc<Notify>,
    notify_traffic_lsm_dibit_dma: Arc<Notify>,
    notify_traffic_iq_dma: Arc<Notify>,
    notify_pre_diff_iq_dma: Arc<Notify>,
    notify_traffic_pre_diff_iq_dma: Arc<Notify>,
}

impl InterruptHandler {
    fn new(uio: Uio, registers: Registers) -> Self {
        InterruptHandler {
            uio,
            registers,
            notify_iq_dma: Arc::new(Notify::new()),
            notify_lsm_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_lsm_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_iq_dma: Arc::new(Notify::new()),
            notify_pre_diff_iq_dma: Arc::new(Notify::new()),
            notify_traffic_pre_diff_iq_dma: Arc::new(Notify::new()),
        }
    }

    /// Returns a waiter for post-DDC IQ DMA completion interrupts.
    pub fn waiter_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_iq_dma.clone(),
        }
    }

    /// Returns a waiter for LSM control-channel dibit DMA completion
    /// interrupts. NID events themselves are PS-polled via
    /// `IpCore::lsm_status()` rather than IRQ-driven.
    pub fn waiter_lsm_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_lsm_dibit_dma.clone(),
        }
    }

    /// Returns a waiter for traffic-side LSM dibit DMA completion interrupts.
    pub fn waiter_traffic_lsm_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_lsm_dibit_dma.clone(),
        }
    }

    /// Returns a waiter for traffic-side post-DDC IQ DMA completion interrupts.
    pub fn waiter_traffic_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_iq_dma.clone(),
        }
    }

    /// Returns a waiter for control-side pre-diff IQ DMA completion interrupts.
    pub fn waiter_pre_diff_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_pre_diff_iq_dma.clone(),
        }
    }

    /// Returns a waiter for traffic-side pre-diff IQ DMA completion interrupts.
    pub fn waiter_traffic_pre_diff_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_pre_diff_iq_dma.clone(),
        }
    }

    /// Runs the interrupt handler loop.
    ///
    /// `irq_stats` is shared with the dashboard wiring; the handler
    /// updates it on every IRQ so `/api/irq_stats` returns live counters
    /// without having to grep the log.
    pub async fn run(
        mut self,
        irq_stats: std::sync::Arc<tokio::sync::Mutex<crate::IrqStats>>,
    ) -> Result<()> {
        let mut total_irqs: u64 = 0;
        let mut iq_irqs: u64 = 0;
        let mut lsm_dibit_irqs: u64 = 0;
        let mut traffic_lsm_dibit_irqs: u64 = 0;
        let mut traffic_iq_irqs: u64 = 0;
        let mut pre_diff_iq_irqs: u64 = 0;
        let mut traffic_pre_diff_iq_irqs: u64 = 0;
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            let iq = interrupts.iq_dma().bit();
            let lsm_dibit = interrupts.lsm_dibit_dma().bit();
            let traffic_lsm_dibit = interrupts.traffic_lsm_dibit_dma().bit();
            let traffic_iq = interrupts.traffic_iq_dma().bit();
            let pre_diff_iq = interrupts.pre_diff_iq_dma().bit();
            let traffic_pre_diff_iq = interrupts.traffic_pre_diff_iq_dma().bit();
            total_irqs += 1;
            if iq {
                iq_irqs += 1;
                self.notify_iq_dma.notify_waiters();
            }
            if lsm_dibit {
                lsm_dibit_irqs += 1;
                self.notify_lsm_dibit_dma.notify_waiters();
            }
            if traffic_lsm_dibit {
                traffic_lsm_dibit_irqs += 1;
                self.notify_traffic_lsm_dibit_dma.notify_waiters();
            }
            if traffic_iq {
                traffic_iq_irqs += 1;
                self.notify_traffic_iq_dma.notify_waiters();
            }
            if pre_diff_iq {
                pre_diff_iq_irqs += 1;
                self.notify_pre_diff_iq_dma.notify_waiters();
            }
            if traffic_pre_diff_iq {
                traffic_pre_diff_iq_irqs += 1;
                self.notify_traffic_pre_diff_iq_dma.notify_waiters();
            }
            // Update shared stats. Cheap async lock, no contention
            // because nothing else writes this struct.
            {
                let now = std::time::Instant::now();
                let mut s = irq_stats.lock().await;
                if s.started_at.is_none() {
                    s.started_at = Some(now);
                }
                s.total = total_irqs;
                s.iq = iq_irqs;
                s.lsm_dibit = lsm_dibit_irqs;
                s.traffic_lsm_dibit = traffic_lsm_dibit_irqs;
                s.traffic_iq = traffic_iq_irqs;
                s.pre_diff_iq = pre_diff_iq_irqs;
                s.traffic_pre_diff_iq = traffic_pre_diff_iq_irqs;
                s.last_at = Some(now);
            }
            // Log first 10 then every 64th to avoid flooding
            if total_irqs <= 10 || total_irqs % 64 == 0 {
                tracing::info!(
                    target: "p25_irq",
                    "IRQ #{total_irqs}: iq={iq} lsm_dibit={lsm_dibit} \
                     traffic_lsm_dibit={traffic_lsm_dibit} \
                     traffic_iq={traffic_iq} pre_diff_iq={pre_diff_iq} \
                     traffic_pre_diff_iq={traffic_pre_diff_iq} \
                     (totals iq={iq_irqs} lsm_dibit={lsm_dibit_irqs} \
                     traffic_lsm_dibit={traffic_lsm_dibit_irqs} \
                     traffic_iq={traffic_iq_irqs} \
                     pre_diff_iq={pre_diff_iq_irqs} \
                     traffic_pre_diff_iq={traffic_pre_diff_iq_irqs})"
                );
            }
        }
    }
}

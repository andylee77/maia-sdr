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

use crate::hardware::core_version::CoreVersion;
use crate::hardware::ddc_presets::DdcPreset;
use crate::hardware::dibit_ring::{
    copy_plan, mono_us, ChainEpochSink, HwAction, RingGeometry, RingSnapshot,
};
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
    /// M2B 2026-05-02: traffic-side LSM dibit ring fed off the
    /// polyphase-channelizer + per_target_ddc + mux output. Same
    /// 64-bit packing as `lsm_dibit_dma`. UIO `p25-traffic-lsm-dibit`.
    traffic_lsm_dibit_dma: RxBuffer,
    /// Phase 10.8 (2026-04-23): control-chain pre-differential IQ ring.
    /// Tapped inside `LsmDemod` after `LsmPllRotate` + AGC but BEFORE
    /// the diff-demod / slicer. Samples sit on the LSM ideal
    /// constellation (±1, ±1). 9.6 kSPS (2 samples per symbol
    /// interleaved). UIO `p25-pre-diff-iq`. Feeds the Plots tab
    /// constellation + eye + `/api/deviation` + `/api/distribution`.
    pre_diff_iq_dma: RxBuffer,
    /// Wideband spectrometer output ring (4096-bin FFT, HW-integrated,
    /// pre-DDC tap on `rxiq_cdc`). UIO `p25-wideband-spec`. Feeds
    /// `/api/spectrum_wide`; no PS FFT.
    wideband_spec_dma: RxBuffer,
    /// 2026-05-03: pre-DDC raw 8 MSPS / 8 MHz BW IQ ring fed straight
    /// from `rxiq_cdc`. 12-bit signed I/Q sign-extended to 16-bit, two
    /// samples per 64-bit DMA word — same packing as `iq_dma`. Backs
    /// the PS-side software P25 stack (polyphase channelizer ->
    /// per-target DDC -> LSM demod). UIO `p25-wideband-iq`.
    wideband_iq_dma: RxBuffer,
    /// 2026-05-03 dual-DDC pivot: traffic-chain post-DDC narrowband IQ
    /// ring. Tap is `traffic_ddc.re_out / im_out` at 50 kSPS post the
    /// dual-DDC retune (Nyquist ±25 kHz). Same 64-bit packing as
    /// `iq_dma`. UIO `p25-traffic-iq`. Feeds
    /// `/api/spectrum?chain=traffic`.
    traffic_iq_dma: RxBuffer,

    iq_last_addr: Option<u32>,
    lsm_dibit_last_addr: Option<u32>,
    traffic_lsm_dibit_last_addr: Option<u32>,
    pre_diff_iq_last_addr: Option<u32>,
    wideband_spec_last_buffer: Option<u8>,
    wideband_iq_last_addr: Option<u32>,
    traffic_iq_last_addr: Option<u32>,

    /// Change 054: receiver of traffic-chain hardware actions (retune,
    /// NCO write, LSM reset, enable/pause) for air-time epoch cuts and
    /// the traffic production clock. `None` until main wires it.
    traffic_epoch_sink: Option<Arc<dyn ChainEpochSink>>,

    /// Change 059: `version` register, read once at `take`.
    core_version: CoreVersion,
}

/// Change 054: the two P25 dibit rings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DibitRing {
    /// `lsm_dibit_dma` (control channel, 0x1A00_0000).
    Control,
    /// `traffic_lsm_dibit_dma` (traffic channel, 0x1B00_0000).
    Traffic,
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
        let core_version = CoreVersion::new(
            ver.major().bits(),
            ver.minor().bits(),
            ver.bugfix().bits(),
        );
        tracing::info!(
            "P25 FPGA core v{} (platform {}), LSM signal hold: {}",
            core_version,
            ver.platform().bits(),
            core_version.has_lsm_signal_hold(),
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
        let pre_diff_iq_dma = RxBuffer::new("p25-pre-diff-iq")
            .await
            .context("failed to open p25-pre-diff-iq DMA buffer")?;
        let wideband_spec_dma = RxBuffer::new("p25-wideband-spec")
            .await
            .context("failed to open p25-wideband-spec DMA buffer")?;
        let wideband_iq_dma = RxBuffer::new("p25-wideband-iq")
            .await
            .context("failed to open p25-wideband-iq DMA buffer")?;
        // 2026-05-03 dual-DDC: traffic-chain post-DDC narrowband IQ.
        // DT entry `p25-traffic-iq` carved out at 0x1c000000 (32 KB
        // buffers) — see Tezuka `fishball-p25.dtsi`.
        let traffic_iq_dma = RxBuffer::new("p25-traffic-iq")
            .await
            .context("failed to open p25-traffic-iq DMA buffer")?;

        let ip_core = IpCore {
            registers,
            iq_dma,
            lsm_dibit_dma,
            traffic_lsm_dibit_dma,
            pre_diff_iq_dma,
            wideband_spec_dma,
            wideband_iq_dma,
            traffic_iq_dma,
            iq_last_addr: None,
            lsm_dibit_last_addr: None,
            traffic_lsm_dibit_last_addr: None,
            pre_diff_iq_last_addr: None,
            wideband_spec_last_buffer: None,
            wideband_iq_last_addr: None,
            traffic_iq_last_addr: None,
            traffic_epoch_sink: None,
            core_version,
        };

        let interrupt_handler = InterruptHandler::new(uio, interrupt_registers);
        Ok((ip_core, interrupt_handler))
    }

    /// Change 059: the core's `version` register (read at `take`).
    pub fn core_version(&self) -> CoreVersion {
        self.core_version
    }

    // ── Control channel DDC ──────────────────────────────────────

    /// Configures the complete DDC: FIR coefficients, decimation, NCO.
    ///
    /// `preset` selects the AD9361 sample rate and the matching FIR
    /// coefficient / decimation tables. Every preset produces 50 kSPS
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

    // 2026-05-02: traffic-side DDC config (configure_traffic_ddc /
    // set_traffic_ddc_* / set_traffic_ddc_enable) removed with the
    // old single-LSM-chain traffic path. Replaced by the polyphase
    // channelizer + per_target_ddc pool. New API for setting target
    // bin + NCO is the `traffic_pipe` register bank (M2A) — wiring
    // pending in p25-httpd.

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

    /// Reads new traffic-chain post-DDC IQ ring sub-buffers since the
    /// last call. 2026-05-03 dual-DDC pivot: tap is `traffic_ddc.re_out
    /// / im_out` at 50 kSPS (Nyquist ±25 kHz). Same 64-bit packing as
    /// `read_iq_buffers`. Feeds `/api/spectrum?chain=traffic`.
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

    // 2026-05-02: traffic_pre_diff_iq_dma retired with the old chain.

    /// Reads new control-chain pre-diff IQ sub-buffers since the last call.
    pub fn read_pre_diff_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::PreDiffIq)
    }

    // ── Wideband raw IQ ring (2026-05-03) ────────────────────────
    //
    // Pre-DDC tap of `rxiq_cdc` at 8 MSPS / 8 MHz BW. Same per-word
    // packing as `iq_dma` ({ im[1] s16, re[1] s16, im[0] s16, re[0] s16 }
    // — natural little-endian interleaved-IQ). 16 sub-buffers of 1 MB
    // each = 16 MB ring (~0.5 s in flight at 32 MB/s). Backs the
    // PS-side software P25 stack.

    /// Enables or disables the wideband raw-IQ ring DMA. Level-triggered.
    pub fn set_wideband_iq_dma_enable(&self, enable: bool) {
        self.registers
            .wideband_iq_dma_control()
            .modify(|_, w| w.wideband_iq_enable().bit(enable));
    }

    /// Returns the index of the most recently completed wideband-IQ sub-buffer.
    pub fn wideband_iq_last_buffer(&self) -> u8 {
        self.registers
            .wideband_iq_dma_status()
            .read()
            .last_buffer()
            .bits()
    }

    /// Reads and clears the wideband-IQ ring overflow latch (Rsticky bit).
    pub fn wideband_iq_overflow(&self) -> bool {
        self.registers
            .wideband_iq_dma_status()
            .read()
            .wideband_iq_overflow()
            .bit()
    }

    /// Returns the current wideband-IQ DMA AW write address (debug).
    pub fn wideband_iq_next_address(&self) -> u32 {
        self.registers
            .wideband_iq_next_address()
            .read()
            .next_address()
            .bits()
    }

    /// Reads new wideband-IQ ring sub-buffers since the last call.
    pub fn read_wideband_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::WidebandIq)
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

    // ── LSM chain (traffic, M2B 2026-05-02) ─────────────────────
    // Mirror of the control LSM bank, fed off the polyphase
    // channelizer + per_target_ddc + mux output (`traffic_pipeline`
    // in HDL). Same register shape as `lsm_*`, prefixed
    // `traffic_lsm_*`. Selected target slot drives a single LSM
    // chain — for multi-target follow we'd need multiple LSM chains
    // hanging off the mux, deferred.

    /// Master enable for the traffic LSM chain. Change 054: a change of
    /// state is reported to the epoch sink (Pause / Resume cut).
    pub fn set_traffic_lsm_enable(&self, enable: bool) {
        let before = self.traffic_lsm_enabled();
        self.write_traffic_lsm_enable(enable);
        if before != enable {
            self.traffic_hw_epoch(HwAction::Enable(enable), before);
        }
    }

    fn write_traffic_lsm_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_enable().bit(enable));
    }

    /// Current `traffic_lsm_enable` (register readback, no side effect).
    pub fn traffic_lsm_enabled(&self) -> bool {
        self.registers
            .traffic_lsm_control()
            .read()
            .traffic_lsm_enable()
            .bit()
    }

    /// Change 054: install the traffic-chain epoch sink.
    pub fn set_traffic_epoch_sink(&mut self, sink: Arc<dyn ChainEpochSink>) {
        self.traffic_epoch_sink = Some(sink);
    }

    /// Report a traffic-chain hardware action with the next-address
    /// register read right after it. We hold `&self`, i.e. the IpCore
    /// lock, so this is ordered with the dibit reader's snapshots.
    fn traffic_hw_epoch(&self, action: HwAction, enabled_before: bool) {
        if let Some(sink) = self.traffic_epoch_sink.as_ref() {
            let next = self.traffic_lsm_dibit_next_address();
            sink.record_hw(action, mono_us(), next, enabled_before);
        }
    }

    /// Enables or disables the traffic LSM dibit ring DMA.
    pub fn set_traffic_lsm_dibit_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_dibit_dma_enable().bit(enable));
    }

    /// Enables or disables the traffic-chain front-end DC blocker.
    pub fn set_traffic_lsm_dc_block_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_dc_block_enable().bit(enable));
    }

    /// Enables or disables the per-symbol traffic LSM AGC.
    pub fn set_traffic_lsm_agc_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_agc_enable().bit(enable));
    }

    /// Pulses the traffic LSM reset (Wpulse). Drives a 1-cycle
    /// `reset_in` into LsmDemod, clearing PLL accumulator + AGC +
    /// timing-recovery state.
    pub fn pulse_traffic_lsm_reset(&self) {
        let before = self.traffic_lsm_enabled();
        self.write_traffic_lsm_reset_pulse();
        self.traffic_hw_epoch(HwAction::LsmReset, before);
    }

    fn write_traffic_lsm_reset_pulse(&self) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_reset().bit(true));
    }

    /// 2026-05-03 seeding bake: writes warm-start seeds for the traffic
    /// LSM chain. Caller MUST follow with `pulse_traffic_lsm_reset()`
    /// to actually load them into the AGC / Costas PLL / Gardner
    /// timing accumulators.
    ///
    /// Q-formats (raw register bits):
    /// - `agc_seed`    : 20-bit unsigned Q9.11. Snapshot from
    ///   control-chain `agc_gain_dbg` (Q9.7) shifted left by 4.
    /// - `pll_seed`    : 16-bit signed Q2.13. Snapshot from
    ///   `pll_dbg`. Empirical sweep: -13 ± 4 Hz uniform across
    ///   active P25 voice channels (memory: 2026-05-03 session).
    /// - `timing_seed` : 18-bit signed Q5.12. Snapshot from
    ///   `sample_point_dbg`. Pass 0 to fall back to cold-start init
    ///   (Gardner sweep deferred — see doc/changes/050).
    ///
    /// CDC fence: bank 8 (seeds) and bank 6 (`traffic_lsm_reset`) cross
    /// independent RegisterCDC instances. Without a fence between
    /// "seed writes committed in s_axi_lite" and "reset pulse fires
    /// in sync", the reset can latch the previous seed value. We
    /// round-trip a read on `traffic_lsm_pll_seed` after the writes;
    /// the read can't return until it has crossed the CDC, which
    /// proves the writes have crossed too. Cost: ~100 ns per retune.
    pub fn write_traffic_seeds(
        &self,
        agc_seed: u32,
        pll_seed: i16,
        timing_seed: i32,
    ) {
        // svd2rust generates `Writable` but not `Resettable` for
        // these single-field RW registers, so `.write()` fails its
        // trait bound at cross-compile (host check passes only
        // because hardware/fpga.rs is cfg(target_os = "linux")).
        // `.modify()` requires only `Readable + Writable` and
        // semantically does the right thing — these registers have
        // a single field each, so the read-modify-write reduces to
        // a plain register update.
        self.registers
            .traffic_lsm_agc_seed()
            .modify(|_, w| unsafe {
                w.agc_seed().bits(agc_seed & 0x000F_FFFF)
            });
        self.registers
            .traffic_lsm_pll_seed()
            .modify(|_, w| unsafe {
                w.pll_seed().bits(pll_seed as u16)
            });
        self.registers
            .traffic_lsm_timing_seed()
            .modify(|_, w| unsafe {
                w.timing_seed().bits((timing_seed as u32) & 0x0003_FFFF)
            });
        // CDC fence: round-trip read forces all writes above to have
        // crossed s_axi_lite -> sync before the caller pulses reset.
        let _ = self.registers
            .traffic_lsm_pll_seed()
            .read()
            .pll_seed()
            .bits();
    }

    /// Mirror of `write_traffic_seeds` for the control LSM chain.
    /// Rarely needed in operation (control chain stays locked once
    /// acquired) but exposed for symmetry + manual diagnostic use.
    pub fn write_control_seeds(
        &self,
        agc_seed: u32,
        pll_seed: i16,
        timing_seed: i32,
    ) {
        self.registers
            .lsm_agc_seed()
            .modify(|_, w| unsafe {
                w.agc_seed().bits(agc_seed & 0x000F_FFFF)
            });
        self.registers
            .lsm_pll_seed()
            .modify(|_, w| unsafe {
                w.pll_seed().bits(pll_seed as u16)
            });
        self.registers
            .lsm_timing_seed()
            .modify(|_, w| unsafe {
                w.timing_seed().bits((timing_seed as u32) & 0x0003_FFFF)
            });
        let _ = self.registers
            .lsm_pll_seed()
            .read()
            .pll_seed()
            .bits();
    }

    /// Reads back the currently-latched traffic seed register values.
    /// Useful for `/api/system` surfacing and post-write verification.
    pub fn read_traffic_seeds(&self) -> (u32, i16, i32) {
        let agc = self.registers
            .traffic_lsm_agc_seed()
            .read()
            .agc_seed()
            .bits();
        let pll = self.registers
            .traffic_lsm_pll_seed()
            .read()
            .pll_seed()
            .bits() as i16;
        // Sign-extend the 18-bit timing seed (PAC returns u32) into
        // an i32 so consumers see a signed value.
        let timing_raw = self.registers
            .traffic_lsm_timing_seed()
            .read()
            .timing_seed()
            .bits();
        let timing = sign_extend_18(timing_raw);
        (agc, pll, timing)
    }

    /// Reads back the `traffic_lsm_control` register as
    /// `(enable, dibit_dma_enable, dc_block_enable, agc_enable)`.
    pub fn traffic_lsm_control_readback(&self) -> (bool, bool, bool, bool) {
        let c = self.registers.traffic_lsm_control().read();
        (
            c.traffic_lsm_enable().bit(),
            c.traffic_lsm_dibit_dma_enable().bit(),
            c.traffic_lsm_dc_block_enable().bit(),
            c.traffic_lsm_agc_enable().bit(),
        )
    }

    /// Reads `traffic_lsm_status` as a coherent snapshot.
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

    /// Reads the latched NAC/DUID of the most recent traffic NID event.
    pub fn traffic_lsm_nid(&self) -> (u16, u8) {
        let n = self.registers.traffic_lsm_nid().read();
        (n.nac().bits(), n.duid().bits())
    }

    /// Reads the saturating traffic NID drop counter.
    pub fn traffic_lsm_drop_count(&self) -> u16 {
        self.registers.traffic_lsm_drop_count().read().drop_count().bits()
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

    /// Returns the current AW write address for the traffic LSM
    /// dibit channel.
    pub fn traffic_lsm_dibit_next_address(&self) -> u32 {
        self.registers
            .traffic_lsm_dibit_next()
            .read()
            .next_address()
            .bits()
    }

    /// Reads the traffic LSM debug taps: `pll_dbg` (signed Q2.13),
    /// `sample_point_dbg` (signed Q4.10).
    pub fn traffic_lsm_debug(&self) -> (i16, i16) {
        let d = self.registers.traffic_lsm_debug().read();
        (
            d.pll_dbg().bits() as i16,
            d.sample_point_dbg().bits() as i16,
        )
    }

    /// Reads the traffic LSM AGC debug taps. Same layout as
    /// `lsm_agc_debug` (gain Q9.7, mag Q1.15).
    pub fn traffic_lsm_agc_debug(&self) -> (u16, u16) {
        let d = self.registers.traffic_lsm_agc_debug().read();
        (d.agc_gain_dbg().bits(), d.agc_mag_dbg().bits())
    }

    /// Reads the traffic LSM AGC idle-gate threshold.
    pub fn traffic_lsm_agc_threshold(&self) -> u16 {
        self.registers
            .traffic_lsm_agc_config()
            .read()
            .mag_update_threshold()
            .bits()
    }

    /// Writes the traffic LSM AGC idle-gate threshold.
    pub fn set_traffic_lsm_agc_threshold(&self, v: u16) {
        self.registers
            .traffic_lsm_agc_config()
            .modify(|_, w| unsafe { w.mag_update_threshold().bits(v) });
    }

    /// Reads new traffic LSM dibit DMA buffers since the last call.
    pub fn read_traffic_lsm_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::TrafficLsmDibit)
    }

    // ── Traffic DDC config (2026-05-03 dual-DDC pivot) ──────────
    //
    // Mirror of `configure_ddc` / `set_ddc_frequency` / `set_ddc_enable`
    // for the traffic DDC instance. Polyphase channelizer + per_target
    // _ddc API has been retired; the traffic chain is now a dedicated
    // P25DDC mirroring the control side.

    /// Loads the traffic DDC FIR coefficient tables + per-stage
    /// decimation + sets the NCO frequency. `preset` picks the AD9361
    /// sample rate match; `frequency_hz` is the NCO offset from the
    /// traffic RX LO at `preset.sample_rate_hz`.
    pub fn configure_traffic_ddc(
        &self,
        frequency_hz: f64,
        preset: &DdcPreset,
    ) -> Result<()> {
        self.load_traffic_fir1(preset.fir1_coeffs, preset.decim1)?;
        self.load_traffic_fir2(preset.fir2_coeffs, preset.decim2)?;
        self.load_traffic_fir3(preset.fir3_coeffs, preset.decim3)?;

        // Enable all 3 stages (no bypass)
        self.registers.traffic_ddc_control().modify(|_, w| {
            w.traffic_bypass2().clear_bit().traffic_bypass3().clear_bit()
        });

        self.set_traffic_ddc_frequency(
            frequency_hz, preset.sample_rate_hz as f64)?;

        tracing::info!(
            "Traffic DDC configured: preset={} NCO={} Hz, \
             3-stage FIR ({}/{}/{} taps), \
             {}x{}x{}={}x decimation, output={} Hz \
             (25 kHz rejection {:+.1} dB)",
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

    /// Sets the traffic DDC NCO frequency. Mirror of
    /// `set_ddc_frequency`. Range check uses the same ±sample_rate/2
    /// bound — caller is responsible for keeping the offset inside
    /// the IF window.
    pub fn set_traffic_ddc_frequency(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        let before = self.traffic_lsm_enabled();
        self.write_traffic_ddc_frequency(frequency_hz, sample_rate_hz)?;
        self.traffic_hw_epoch(HwAction::NcoWrite, before);
        Ok(())
    }

    fn write_traffic_ddc_frequency(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        let half = 0.5 * sample_rate_hz;
        if !(-half..=half).contains(&frequency_hz) {
            anyhow::bail!(
                "Traffic DDC frequency {frequency_hz} Hz out of range \
                 ±{half} Hz"
            );
        }
        let nco_word = freq_to_nco(frequency_hz, sample_rate_hz);
        self.registers
            .traffic_ddc_frequency()
            .modify(|_, w| unsafe { w.traffic_frequency().bits(nco_word) });
        Ok(())
    }

    /// Enables or disables the traffic DDC input.
    pub fn set_traffic_ddc_enable(&self, enable: bool) {
        self.registers
            .traffic_ddc_control()
            .modify(|_, w| w.traffic_enable_input().bit(enable));
    }

    // ── Traffic FIR coefficient loading (private) ───────────────
    //
    // Mirrors `load_fir1` / `load_fir2` / `load_fir3` /
    // `load_fir_4dsp` / `load_fir_2dsp` for the traffic_ddc bank.
    // Same polyphase reordering / RAM addressing — only the target
    // register pair changes.

    fn load_traffic_fir1(
        &self, coefficients: &[i32], decimation: usize,
    ) -> Result<()> {
        self.load_traffic_fir_4dsp(coefficients, decimation, 0)?;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        let odd = branch_len % 2 == 1;
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.traffic_ddc_decimation().modify(|_, w| unsafe {
            w.traffic_decimation1().bits(dec)
        });
        self.registers.traffic_ddc_control().modify(|_, w| unsafe {
            w.traffic_operations_minus_one1().bits(opm1)
                .traffic_odd_operations1().bit(odd)
        });
        Ok(())
    }

    fn load_traffic_fir2(
        &self, coefficients: &[i32], decimation: usize,
    ) -> Result<()> {
        self.load_traffic_fir_2dsp(coefficients, decimation, 256)?;
        let operations = coefficients.len().div_ceil(decimation);
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.traffic_ddc_decimation().modify(|_, w| unsafe {
            w.traffic_decimation2().bits(dec)
        });
        self.registers.traffic_ddc_control().modify(|_, w| unsafe {
            w.traffic_operations_minus_one2().bits(opm1)
        });
        Ok(())
    }

    fn load_traffic_fir3(
        &self, coefficients: &[i32], decimation: usize,
    ) -> Result<()> {
        self.load_traffic_fir_4dsp(coefficients, decimation, 512)?;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        let odd = branch_len % 2 == 1;
        let dec = u8::try_from(decimation).unwrap();
        let opm1 = u8::try_from(operations - 1).unwrap();
        self.registers.traffic_ddc_decimation().modify(|_, w| unsafe {
            w.traffic_decimation3().bits(dec)
        });
        self.registers.traffic_ddc_control().modify(|_, w| unsafe {
            w.traffic_operations_minus_one3().bits(opm1)
                .traffic_odd_operations3().bit(odd)
        });
        Ok(())
    }

    fn load_traffic_fir_4dsp(
        &self,
        coefficients: &[i32],
        decimation: usize,
        addr_offset: usize,
    ) -> Result<()> {
        const NUM_ADDR: usize = 256;
        let branch_len = coefficients.len().div_ceil(decimation);
        let operations = branch_len.div_ceil(2);
        if operations * decimation > NUM_ADDR / 2 {
            anyhow::bail!("Traffic FIR4DSP coefficients too long for RAM");
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
                .traffic_ddc_coeff_addr()
                .modify(|_, w| unsafe {
                    w.traffic_coeff_waddr().bits(waddr)
                });
            self.registers.traffic_ddc_coeff().modify(|_, w| unsafe {
                w.traffic_coeff_wren().bit(true)
                    .traffic_coeff_wdata().bits(coeff as u32)
            });
        }
        Ok(())
    }

    fn load_traffic_fir_2dsp(
        &self,
        coefficients: &[i32],
        decimation: usize,
        addr_offset: usize,
    ) -> Result<()> {
        const NUM_ADDR: usize = 128;
        let operations = coefficients.len().div_ceil(decimation);
        if operations * decimation > NUM_ADDR {
            anyhow::bail!("Traffic FIR2DSP coefficients too long for RAM");
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
                .traffic_ddc_coeff_addr()
                .modify(|_, w| unsafe {
                    w.traffic_coeff_waddr().bits(waddr)
                });
            self.registers.traffic_ddc_coeff().modify(|_, w| unsafe {
                w.traffic_coeff_wren().bit(true)
                    .traffic_coeff_wdata().bits(coeff as u32)
            });
        }
        Ok(())
    }

    // ── Traffic chain retune (2026-05-03 dual-DDC) ──────────────
    //
    // Now identical in shape to the control-side retune: write the
    // NCO frequency (offset from the traffic RX LO at the active
    // preset's sample rate), pulse the LSM reset, ensure the chain
    // master enable is on. The DDC FIR coefficients stay programmed
    // across retunes — the AD9361 / preset / IF window is shared
    // with the control chain.

    /// Programs the traffic DDC NCO + pulses the traffic LSM reset
    /// + enables the chain.
    ///
    /// `freq_changed` controls whether `pulse_traffic_lsm_reset` fires.
    /// When `true` (frequency actually moved), the LSM chain's AGC /
    /// Costas PLL / Gardner timing-recovery / sync correlator all reset
    /// because the IF position changed and any retained state would be
    /// stale. When `false` (next grant on the same frequency), the
    /// pulse is skipped — the chain has already converged to this
    /// channel's signal characteristics during the previous PTT, so
    /// preserving that state lets the next grant acquire in
    /// ~100-250 ms instead of paying a 3-5 second cold-start tax.
    ///
    /// 2026-05-02 operator-confirmed pattern: every "first call after
    /// a frequency change" misses (3-5 s First IMBE, often longer than
    /// the call itself); subsequent same-freq grants acquire in
    /// ~150 ms. This gate preserves the cross-PTT warmth.
    pub fn retune_traffic_chain(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
        // 2026-05-03 quality-gated coast (Option C):
        //   `should_reset = false` → coast: NCO write only, no reset
        //     pulse. Chain re-acquires through FIR flush. AGC + timing
        //     + PLL preserved from prior call.
        //   `should_reset = true`  → pulse `lsm_reset`. Clears all
        //     accumulators to their HDL init values
        //     (AGC = GAIN_INIT 1.0, PLL = 0, timing = warmup offset).
        //     Required when the prior call left the chain in a
        //     degenerate state (saturated AGC from idle-noise gain
        //     pumping; PLL drifted to a bad attractor) — coasting
        //     into the next call would inherit that state.
        //
        // The caller (`spawn_grant_follower`) computes `should_reset`
        // via the `was_clean()` check on `LastCallQuality`. Pure
        // coast (no reset) was tested under build
        // `2026-05-03-coast-no-reset` and showed sub-100 ms First-IMBE
        // when the chain was healthy but ~60 % of calls returned 0
        // IMBE (the failure mode above).
        should_reset: bool,
        // Seeds parameter retained for API contract but currently
        // unused. The bank-8 seed registers + heartbeat snapshot are
        // dormant primitives kept in place for a future "soft PLL
        // reset" mode (zero PLL accumulator only, preserve AGC +
        // timing — closer mirror of SDRTrunk's resetPLL semantics).
        _seeds: Option<(u32, i16, i32)>,
    ) -> Result<()> {
        // Change 054: raw register writes + ONE epoch report for the
        // whole sequence (the public setters would report three).
        let before = self.traffic_lsm_enabled();
        self.write_traffic_ddc_frequency(frequency_hz, sample_rate_hz)?;
        if should_reset {
            self.write_traffic_lsm_reset_pulse();
        }
        self.write_traffic_lsm_enable(true);
        self.traffic_hw_epoch(HwAction::Retune { lsm_reset: should_reset }, before);
        Ok(())
    }

    /// Quiesces the traffic LSM chain — flips `traffic_lsm_enable`
    /// off so the RRC + LsmDemod stop receiving sample strobes.
    /// Useful when the grant follower lets the chain idle between
    /// calls. Traffic DDC NCO + FIRs stay programmed.
    pub fn pause_traffic_chain(&self) {
        self.set_traffic_lsm_enable(false);
    }

    // ── Traffic IQ ring (2026-05-03 dual-DDC parity) ────────────

    /// Enables or disables the traffic post-DDC IQ ring DMA.
    /// Level-triggered. Mirror of `set_iq_dma_enable`.
    pub fn set_traffic_iq_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_iq_dma_control()
            .modify(|_, w| w.traffic_iq_enable().bit(enable));
    }

    /// Returns the index of the most recently completed traffic IQ
    /// sub-buffer.
    pub fn traffic_iq_last_buffer(&self) -> u8 {
        self.registers
            .traffic_iq_dma_status()
            .read()
            .traffic_iq_last_buffer()
            .bits()
    }

    /// Reads and clears the traffic IQ ring overflow latch.
    pub fn traffic_iq_overflow(&self) -> bool {
        self.registers
            .traffic_iq_dma_status()
            .read()
            .traffic_iq_overflow()
            .bit()
    }

    // ── Traffic pre-diff IQ ring (2026-05-03 dual-DDC parity) ───

    /// Enables or disables the traffic pre-diff IQ ring DMA.
    pub fn set_traffic_pre_diff_iq_dma_enable(&self, enable: bool) {
        self.registers
            .traffic_pre_diff_iq_dma_control()
            .modify(|_, w| w.traffic_pre_diff_iq_enable().bit(enable));
    }

    /// Returns the index of the most recently completed traffic
    /// pre-diff IQ sub-buffer.
    pub fn traffic_pre_diff_iq_last_buffer(&self) -> u8 {
        self.registers
            .traffic_pre_diff_iq_dma_status()
            .read()
            .traffic_pre_diff_iq_last_buffer()
            .bits()
    }

    /// Reads and clears the traffic pre-diff IQ ring overflow latch.
    pub fn traffic_pre_diff_iq_overflow(&self) -> bool {
        self.registers
            .traffic_pre_diff_iq_dma_status()
            .read()
            .traffic_pre_diff_iq_overflow()
            .bit()
    }

    // Control LSM AGC debug + threshold (kept — control side intact).

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

    // ── Change 054: position-based dibit ring access ─────────────

    fn dibit_dma(&self, ring: DibitRing) -> &RxBuffer {
        match ring {
            DibitRing::Control => &self.lsm_dibit_dma,
            DibitRing::Traffic => &self.traffic_lsm_dibit_dma,
        }
    }

    /// Geometry of a dibit ring as mapped by maia-kmod.
    pub fn dibit_ring_geometry(&self, ring: DibitRing) -> RingGeometry {
        let dma = self.dibit_dma(ring);
        RingGeometry {
            sub_buffer_bytes: dma.buffer_size() as u64,
            num_sub_buffers: dma.num_buffers() as u64,
        }
    }

    /// One register reading for the low-latency reader: next burst
    /// address (0xB0 / 0xD0), `last_buffer` (0xAC / 0xCC bits [18:16])
    /// and the chain enable bit (0xA0 / 0xC0). All plain-R reads: never
    /// touches the read-to-clear status words (0xA4 / 0xC4 / 0x0C).
    pub fn dibit_ring_snapshot(&self, ring: DibitRing) -> RingSnapshot {
        match ring {
            DibitRing::Control => {
                let next_address = self.lsm_dibit_next_address();
                let t_us = mono_us();
                RingSnapshot {
                    next_address,
                    last_buffer: self.lsm_dibit_last_buffer(),
                    chain_enabled: self.registers.lsm_control().read().lsm_enable().bit(),
                    t_us,
                }
            }
            DibitRing::Traffic => {
                let next_address = self.traffic_lsm_dibit_next_address();
                let t_us = mono_us();
                RingSnapshot {
                    next_address,
                    last_buffer: self.traffic_lsm_dibit_last_buffer(),
                    chain_enabled: self.traffic_lsm_enabled(),
                    t_us,
                }
            }
        }
    }

    /// Copy the absolute byte range `[start, end)` of a dibit ring into
    /// `out`, invalidating every covering sub-buffer first (maia-kmod
    /// maps the ring cacheable; invalidating a sub-buffer that is still
    /// being written is safe, and must precede every read of newly
    /// available bytes). The caller guarantees the range has landed.
    pub fn copy_dibit_ring(
        &self,
        ring: DibitRing,
        start: u64,
        end: u64,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let dma = self.dibit_dma(ring);
        let geom = self.dibit_ring_geometry(ring);
        let plan = copy_plan(&geom, start, end);
        let mut invalidated: Vec<usize> = Vec::with_capacity(2);
        for piece in &plan {
            if !invalidated.contains(&piece.sub_buffer) {
                dma.cache_invalidate(piece.sub_buffer)
                    .with_context(|| format!(
                        "cache invalidate {ring:?} sub-buffer {}", piece.sub_buffer))?;
                invalidated.push(piece.sub_buffer);
            }
        }
        for piece in &plan {
            let slice = dma.buffer_as_slice(piece.sub_buffer);
            out.extend_from_slice(&slice[piece.offset..piece.offset + piece.len]);
        }
        Ok(())
    }

    /// Legacy whole-sub-buffer read that also reports the index of the
    /// first returned sub-buffer (for age accounting).
    pub fn read_dibit_buffers_indexed(
        &mut self,
        ring: DibitRing,
    ) -> (Option<usize>, Vec<Vec<u8>>) {
        let n = self.dibit_dma(ring).num_buffers();
        let cursor = self.legacy_dibit_cursor(ring);
        let bufs: Vec<Vec<u8>> = match ring {
            DibitRing::Control => self.read_dma_buffers(DmaChannel::LsmDibit),
            DibitRing::Traffic => self.read_dma_buffers(DmaChannel::TrafficLsmDibit),
        }
        .iter()
        .map(|b| b.to_vec())
        .collect();
        let first = match cursor {
            Some(c) if n > 0 && !bufs.is_empty() => Some((c as usize % n + 1) % n),
            _ => None,
        };
        (first, bufs)
    }

    /// Legacy path cursor: index of the last sub-buffer it delivered.
    pub fn legacy_dibit_cursor(&self, ring: DibitRing) -> Option<u32> {
        match ring {
            DibitRing::Control => self.lsm_dibit_last_addr,
            DibitRing::Traffic => self.traffic_lsm_dibit_last_addr,
        }
    }

    /// Set the legacy path cursor (poll to legacy hand-over).
    pub fn set_legacy_dibit_cursor(&mut self, ring: DibitRing, cursor: Option<u32>) {
        match ring {
            DibitRing::Control => self.lsm_dibit_last_addr = cursor,
            DibitRing::Traffic => self.traffic_lsm_dibit_last_addr = cursor,
        }
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
            DmaChannel::PreDiffIq => (
                &self.pre_diff_iq_dma,
                &mut self.pre_diff_iq_last_addr,
                self.registers
                    .pre_diff_iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::WidebandIq => (
                &self.wideband_iq_dma,
                &mut self.wideband_iq_last_addr,
                self.registers
                    .wideband_iq_dma_status()
                    .read()
                    .last_buffer()
                    .bits() as u32,
            ),
            DmaChannel::TrafficIq => (
                &self.traffic_iq_dma,
                &mut self.traffic_iq_last_addr,
                self.registers
                    .traffic_iq_dma_status()
                    .read()
                    .traffic_iq_last_buffer()
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
    PreDiffIq,
    WidebandIq,
    /// 2026-05-03 dual-DDC pivot: traffic-chain post-DDC narrowband IQ.
    TrafficIq,
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

/// Sign-extends an 18-bit value (held in the low bits of a u32) into
/// a signed i32. Used for the `timing_seed` field which the PAC
/// surfaces as u32 even though the underlying register is 18-bit
/// signed Q5.12.
fn sign_extend_18(raw: u32) -> i32 {
    let masked = raw & 0x0003_FFFF;
    if masked & 0x0002_0000 != 0 {
        (masked | 0xFFFC_0000) as i32
    } else {
        masked as i32
    }
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
    /// M2B 2026-05-02: traffic LSM dibit DMA notifier.
    notify_traffic_lsm_dibit_dma: Arc<Notify>,
    notify_pre_diff_iq_dma: Arc<Notify>,
    /// 2026-05-03: wideband raw IQ DMA notifier (PS-side software stack).
    notify_wideband_iq_dma: Arc<Notify>,
}

impl InterruptHandler {
    fn new(uio: Uio, registers: Registers) -> Self {
        InterruptHandler {
            uio,
            registers,
            notify_iq_dma: Arc::new(Notify::new()),
            notify_lsm_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_lsm_dibit_dma: Arc::new(Notify::new()),
            notify_pre_diff_iq_dma: Arc::new(Notify::new()),
            notify_wideband_iq_dma: Arc::new(Notify::new()),
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

    /// Returns a waiter for traffic LSM dibit DMA completion
    /// interrupts (M2B 2026-05-02). Mirrors `waiter_lsm_dibit_dma`
    /// for the new mux-fed chain.
    pub fn waiter_traffic_lsm_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_lsm_dibit_dma.clone(),
        }
    }

    /// Returns a waiter for control-side pre-diff IQ DMA completion interrupts.
    pub fn waiter_pre_diff_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_pre_diff_iq_dma.clone(),
        }
    }

    /// Returns a waiter for wideband raw-IQ DMA completion interrupts
    /// (2026-05-03). Each notification means one or more 1 MB sub-buffers
    /// are ready for the PS-side software P25 stack.
    pub fn waiter_wideband_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_wideband_iq_dma.clone(),
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
        let mut pre_diff_iq_irqs: u64 = 0;
        let mut wideband_iq_irqs: u64 = 0;
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            let iq = interrupts.iq_dma().bit();
            let lsm_dibit = interrupts.lsm_dibit_dma().bit();
            let traffic_lsm_dibit = interrupts.traffic_lsm_dibit_dma().bit();
            let pre_diff_iq = interrupts.pre_diff_iq_dma().bit();
            let wideband_iq = interrupts.wideband_iq_dma().bit();
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
            if pre_diff_iq {
                pre_diff_iq_irqs += 1;
                self.notify_pre_diff_iq_dma.notify_waiters();
            }
            if wideband_iq {
                wideband_iq_irqs += 1;
                self.notify_wideband_iq_dma.notify_waiters();
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
                s.pre_diff_iq = pre_diff_iq_irqs;
                s.wideband_iq = wideband_iq_irqs;
                s.last_at = Some(now);
            }
            // Log first 10 then every 64th to avoid flooding
            if total_irqs <= 10 || total_irqs % 64 == 0 {
                tracing::info!(
                    target: "p25_irq",
                    "IRQ #{total_irqs}: iq={iq} lsm_dibit={lsm_dibit} \
                     traffic_lsm_dibit={traffic_lsm_dibit} \
                     pre_diff_iq={pre_diff_iq} wideband_iq={wideband_iq} \
                     (totals iq={iq_irqs} lsm_dibit={lsm_dibit_irqs} \
                     traffic_lsm_dibit={traffic_lsm_dibit_irqs} \
                     pre_diff_iq={pre_diff_iq_irqs} \
                     wideband_iq={wideband_iq_irqs})"
                );
            }
        }
    }
}

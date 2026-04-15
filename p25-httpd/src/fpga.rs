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
    iq_dma: RxBuffer,
    lsm_dibit_dma: RxBuffer,
    /// Phase 7A.2: traffic-side LSM dibit DMA ring (parallel to the
    /// existing C4FM `traffic_dma` ring). Mirrors `lsm_dibit_dma` on
    /// the control side. UIO device `p25-traffic-lsm-dibit`,
    /// physical address `0x1B00_0000` (8 x 4 KB ring).
    traffic_lsm_dibit_dma: RxBuffer,
    dibit_last_addr: Option<u32>,
    traffic_last_addr: Option<u32>,
    iq_last_addr: Option<u32>,
    lsm_dibit_last_addr: Option<u32>,
    traffic_lsm_dibit_last_addr: Option<u32>,
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
        // Phase 6D: post-DDC IQ ring (8 x 32 KB), parallel to the dibit
        // path. Optional — older boots without the iq_dma DT entry will
        // simply skip the LSM pipeline.
        let iq_dma = RxBuffer::new("p25-iq")
            .await
            .context("failed to open p25-iq DMA buffer")?;
        // Phase 6E.9/6E.10: LSM control-channel dibit ring (8 x 4 KB),
        // parallel to the C4FM dibit path so the PS can A/B both demods
        // on one RF capture. Requires Tezuka DT carve-out for
        // p25_lsm_dibit_dma@1a000000.
        let lsm_dibit_dma = RxBuffer::new("p25-lsm-dibit")
            .await
            .context("failed to open p25-lsm-dibit DMA buffer")?;
        // Phase 7A.2: LSM traffic-channel dibit ring (8 x 4 KB),
        // parallel to the C4FM traffic_dma ring on the traffic side.
        // The traffic_lsm HDL chain decodes voice-channel NIDs (HDU,
        // TDU, LDU1, LDU2) so the PS dispatcher can implement
        // sub-second TDU release on followed calls. Requires Tezuka
        // DT carve-out for p25_traffic_lsm_dibit_dma@1b000000.
        let traffic_lsm_dibit_dma = RxBuffer::new("p25-traffic-lsm-dibit")
            .await
            .context("failed to open p25-traffic-lsm-dibit DMA buffer")?;

        let ip_core = IpCore {
            registers,
            dibit_dma,
            traffic_dma,
            iq_dma,
            lsm_dibit_dma,
            traffic_lsm_dibit_dma,
            dibit_last_addr: None,
            traffic_last_addr: None,
            iq_last_addr: None,
            lsm_dibit_last_addr: None,
            traffic_lsm_dibit_last_addr: None,
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

        let total_dec = P25_DEC1 * P25_DEC2 * P25_DEC3;
        tracing::info!(
            "DDC configured: NCO={} Hz, 3-stage FIR ({}/{}/{} taps), \
             {}x{}x{}={}x decimation, output={} Hz",
            frequency_hz as i64,
            P25_FIR1_COEFFS.len(), P25_FIR2_COEFFS.len(), P25_FIR3_COEFFS.len(),
            P25_DEC1, P25_DEC2, P25_DEC3, total_dec,
            sample_rate_hz as u64 / total_dec as u64,
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

    /// Configures the traffic channel DDC: decimation, operations, NCO.
    ///
    /// Mirrors `configure_ddc()` but writes the **traffic_** register
    /// bank (offset 0x60) instead of the control DDC (`sdr_*`, offset
    /// 0x20). FIR coefficients are NOT loaded here -- the traffic DDC
    /// shares its FIR coefficient ROM with the control DDC at the HDL
    /// level (see `maia-hdl/p25_hdl/p25_top.py` lines 801-807, which
    /// drive `traffic_ddc.coeff_*` from the control `sdr_registers.ddc_coeff_*`).
    /// Therefore `configure_ddc()` must be called BEFORE this function
    /// so that the shared coefficient RAM is loaded by the time the
    /// traffic DDC is enabled.
    ///
    /// `frequency_hz` is the initial NCO offset from the RX LO. The
    /// caller will typically pass 0.0 here and call
    /// `set_traffic_ddc_frequency()` later when a grant is followed.
    /// `sample_rate_hz` is the AD9361 ADC sample rate.
    ///
    /// Phase 7A.1: this is the first PS-side use of the traffic chain.
    /// The traffic chain has been instantiated and wired in HDL since
    /// doc 007 (Phase 4) but never driven from PS until now. The
    /// traffic_dma RxBuffer, IRQ counter, and helper functions have
    /// also been in fpga.rs since Phase 4 -- only the startup init
    /// (this function) and the runtime grant-follower task in main.rs
    /// were missing.
    pub fn configure_traffic_ddc(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        // Compute decimation / operations / odd-operations for each FIR
        // stage from the same constants the control DDC uses, then write
        // them into the traffic_ddc_decimation + traffic_ddc_control
        // register bank. We do NOT touch coefficient RAM (shared with
        // control DDC).
        let dec1 = u8::try_from(P25_DEC1).unwrap();
        let dec2 = u8::try_from(P25_DEC2).unwrap();
        let dec3 = u8::try_from(P25_DEC3).unwrap();

        // FIR1 (FIR4DSP, folded): same math as load_fir1.
        let fir1_branch_len = P25_FIR1_COEFFS.len().div_ceil(P25_DEC1);
        let fir1_operations = fir1_branch_len.div_ceil(2);
        let fir1_odd = fir1_branch_len % 2 == 1;
        let opm1_1 = u8::try_from(fir1_operations - 1).unwrap();

        // FIR2 (FIR2DSP, no folding): same math as load_fir2.
        let fir2_operations = P25_FIR2_COEFFS.len().div_ceil(P25_DEC2);
        let opm1_2 = u8::try_from(fir2_operations - 1).unwrap();

        // FIR3 (FIR4DSP, folded): same math as load_fir3.
        let fir3_branch_len = P25_FIR3_COEFFS.len().div_ceil(P25_DEC3);
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

        // Initial NCO. Caller will typically retune this on every grant.
        self.set_traffic_ddc_frequency(frequency_hz, sample_rate_hz)?;

        let total_dec = P25_DEC1 * P25_DEC2 * P25_DEC3;
        tracing::info!(
            "Traffic DDC configured: NCO={} Hz, {}x{}x{}={}x decimation \
             (FIR coeffs shared with control DDC), output={} Hz",
            frequency_hz as i64,
            P25_DEC1,
            P25_DEC2,
            P25_DEC3,
            total_dec,
            sample_rate_hz as u64 / total_dec as u64,
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

    // ── IQ ring (Phase 6C/6D) ────────────────────────────────────
    //
    // The third DDC tap streams post-decimation 62.5 kSPS interleaved
    // 16-bit signed I/Q to a separate 256 KB ring (8 x 32 KB sub-buffers)
    // at physical 0x19000000. The PS LSM demod reads from this ring while
    // the existing dibit pipeline keeps running unchanged.
    //
    // Per 64-bit DMA word: { im[1] s16, re[1] s16, im[0] s16, re[0] s16 }
    // — i.e. natural little-endian interleaved-IQ byte order. See
    // doc/changes/013_phase6c_iq_dma.md and the Phase 6D entry-point note.

    /// Enables or disables the post-DDC IQ ring DMA. Level-triggered.
    /// When false, the AW channel is held idle and the packer back-pressures.
    pub fn set_iq_dma_enable(&self, enable: bool) {
        self.registers
            .iq_dma_control()
            .modify(|_, w| w.iq_enable().bit(enable));
    }

    /// Returns the index of the most recently completed IQ sub-buffer.
    /// Initialised to all-ones (-1) by the gateware so the first read after
    /// enable indicates "no buffers completed yet".
    pub fn iq_last_buffer(&self) -> u8 {
        self.registers
            .iq_dma_status()
            .read()
            .last_buffer()
            .bits()
    }

    /// Reads and clears the IQ ring overflow latch (Rsticky bit).
    /// True means the packer stalled at least once since the last read —
    /// indicates the PS isn't draining sub-buffers fast enough or the AW
    /// channel was disabled.
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

    /// Reads new IQ ring sub-buffers since the last call. Each returned
    /// slice is 32 KB of interleaved 16-bit signed I/Q (8192 complex
    /// samples = ~131 ms at 62.5 kSPS).
    pub fn read_iq_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::Iq)
    }

    // ── LSM chain (Phase 6E.9/6E.10) ─────────────────────────────
    //
    // LSM demod chain (LsmDecimator2 -> LsmFir(LPF) -> LsmFir(RRC) ->
    // LsmDemod) sits alongside the C4FM chain on the control channel
    // DDC output. Produces its own dibit stream via `lsm_dibit_dma`
    // (parallel ring at 0x1A000000) and exposes BCH-decoded NID events
    // via the `lsm_*` register bank. See doc/P25_ADDRESS_MAP.md for
    // the full layout.

    /// Master enable for the LSM chain (decimator + FIRs + LsmDemod).
    /// Gates the strobe at the front so all downstream blocks go
    /// quiescent when false.
    pub fn set_lsm_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_enable().bit(enable));
    }

    /// Enables or disables the LSM dibit ring DMA. Level-triggered;
    /// mirrors the C4FM `dibit_dma` enable convention.
    pub fn set_lsm_dibit_dma_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_dibit_dma_enable().bit(enable));
    }

    /// Enables or disables the front-end LSM DC blocker (Phase 6G.1).
    ///
    /// When `true`, a one-pole leaky-integrator DC blocker runs on
    /// both I and Q at the input to `LsmDemod` -- this removes the
    /// slow IQ DC bias from the AD9361 that otherwise gives the
    /// slicer a 60/40 inner/outer dibit ratio for the first 2-3
    /// minutes after PLL start. Production code should always set
    /// this to `true`. The runtime knob exists so we can A/B the
    /// blocker on-target during bring-up. See doc/changes/031.
    pub fn set_lsm_dc_block_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_dc_block_enable().bit(enable));
    }

    /// Enables or disables the Phase 10-prep per-symbol LSM AGC.
    ///
    /// When `true`, `LsmAgc` runs inside `LsmDemodLoop` between
    /// `LsmTimingInterp` and `LsmDiffDemodSlicer`, normalising the
    /// four interpolated samples' L2 magnitude to 1.0 via a
    /// SDRTrunk-faithful fixed-point AGC loop (sqrt + division +
    /// 0.05 IIR lerp + asymmetric clamp at 500). When `false`,
    /// the AGC is bypassed and samples pass through unchanged.
    ///
    /// Production code should always set this to `true` after
    /// boot. The runtime knob exists so we can A/B the AGC
    /// on-target against the pre-AGC dibit stream.
    ///
    /// Gain state is debug-readable via the `lsm_agc_debug`
    /// register (`agc_gain_dbg` + `agc_mag_dbg`).
    pub fn set_lsm_agc_enable(&self, enable: bool) {
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_agc_enable().bit(enable));
    }

    /// Phase 8A: pulse the control-side LSM chain runtime reset.
    ///
    /// Writes `1` to the W1P `lsm_reset` field in `lsm_control`,
    /// which the Register framework turns into a 1-sync-cycle pulse
    /// on `LsmDemod.reset_in`. Clears the PLL accumulator + timing
    /// state + diff slicer history + sync register + BCH sweep
    /// state back to their init values. Self-clearing -- the PAC
    /// write-pulse semantics guarantee the field reads back as 0
    /// on the next cycle.
    ///
    /// Use this in conjunction with `set_lsm_enable(false)`
    /// before/after for the clean freeze+reset+thaw sequence. The
    /// control-side chain is currently never retuned, so in
    /// practice this is only called once at boot if at all; the
    /// helper exists for symmetry with the traffic side and for
    /// future channel-hopping work (Phase 7G).
    pub fn pulse_lsm_reset(&self) {
        // The PAC models Wpulse as a `write_with_zero` register
        // field -- we need to write ONLY the reset bit, without
        // clobbering the other RW fields in the same word. Use
        // `modify()` so the surrounding bits (lsm_enable,
        // lsm_dibit_dma_enable, lsm_dc_block_enable) are read and
        // written back unchanged.
        self.registers
            .lsm_control()
            .modify(|_, w| w.lsm_reset().bit(true));
    }

    /// Reads back the `lsm_control` register as `(lsm_enable,
    /// lsm_dibit_dma_enable, lsm_dc_block_enable)`. Used at startup
    /// to confirm the bits we wrote actually stuck in the register
    /// bank.
    pub fn lsm_control_readback(&self) -> (bool, bool, bool) {
        let c = self.registers.lsm_control().read();
        (
            c.lsm_enable().bit(),
            c.lsm_dibit_dma_enable().bit(),
            c.lsm_dc_block_enable().bit(),
        )
    }

    // ── DELETED: reset_and_reinit() (Phase 6E.6 watchdog, doc 023) ──
    //
    // The PS-side `sdr_reset` watchdog from commit 0ef0d09 was found
    // to be FUNDAMENTALLY UNSAFE during the 2026-04-10 CORDIC bake
    // diagnostic session. Pulsing `sdr_reset` mid-operation causes a
    // hard kernel panic reboot. Mechanism:
    //
    //   1. Watchdog sets sdr_reset bit via AXI-Lite.
    //   2. The bit propagates through FFSynchronizer into the `sync`
    //      clock domain reset of the maia_sdr_clk core domain.
    //   3. All `m.d.sync` flops reset to init values, INCLUDING the
    //      iq_dma / lsm_dibit_dma / traffic_dma AXI master state
    //      machines that are mid-burst on AXI HP.
    //   4. The AW phase of the in-flight burst is forgotten by the
    //      FPGA but the PS DDR controller is still waiting for
    //      WLAST=1 + BVALID=1 to retire the transaction.
    //   5. AXI HP slave hangs waiting for handshake that never comes.
    //   6. Kernel watchdog detects AXI deadlock and panics.
    //   7. Hard reboot.
    //
    // Confirmed empirically by direct devmem write to the sdr_reset
    // bit on a stuck system: the board immediately rebooted on the
    // assertion edge.
    //
    // The watchdog as previously deployed in commit 0ef0d09 was
    // ALSO silently broken in a separate way: in the chain's
    // degraded state, the lsm_registers AXI CDC returns shifted /
    // wrong data on reads (we don't yet know why -- the same bug
    // we're now hunting via the NID-event ring buffer dump in
    // main.rs). The PAC `modify()` does a read-modify-write, which
    // reads garbage from the broken CDC and writes garbage back.
    // The actual sdr_reset bit never toggled, the system never
    // rebooted, and the heartbeat just incremented its `recoveries:`
    // counter while the chain stayed stuck. From the user's
    // perspective the watchdog was firing but doing nothing -- the
    // worst possible failure mode.
    //
    // Removing `reset_and_reinit()` entirely is the right move:
    //   - No caller can accidentally invoke it.
    //   - The dangerous mid-operation use of sdr_reset is gone.
    //   - Recovery from the degraded state is now a power cycle,
    //     which is honest about what's actually possible.
    //
    // A future safer recovery path would need to:
    //   - Quiesce the AXI HP DMA masters (writes drain to completion)
    //   - Then assert reset only on the LSM datapath (not the DMA
    //     master state machines)
    //   - Then de-assert and re-arm the masters
    // That's substantially more complex than a single bit pulse and
    // requires HDL gateware changes (separate reset domains for the
    // demod chain vs the AXI master state). Out of scope for now;
    // see doc/changes/024 for the full analysis.
    //
    // If you find yourself wanting to reintroduce a watchdog, FIRST
    // read doc/changes/024 and the on-target devmem evidence in the
    // commit message of the diagnostic-instrumentation commit.

    /// Reads the `lsm_status` register and returns a coherent snapshot.
    ///
    /// **Important:** the `nid_event` bit is Rsticky -- a single read
    /// clears it. Callers that need to inspect multiple fields of the
    /// same NID event must rely on the returned snapshot (not
    /// re-read the register) because `n_errors`, `sync_distance`, and
    /// `nid_valid` are latched into Signal()s on each
    /// `nid_event_strobe` pulse and will not update again until the
    /// next NID arrives -- so the snapshot + a follow-up `lsm_nid()`
    /// + `lsm_drop_count()` read together form a coherent per-event
    /// picture.
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

    /// Reads the saturating NID drop counter. Should always be 0 in
    /// normal operation (BCH decode is ~656 us, NIDs are ~14 ms apart).
    pub fn lsm_drop_count(&self) -> u16 {
        self.registers.lsm_drop_count().read().drop_count().bits()
    }

    /// Returns the index of the most recently completed LSM dibit
    /// sub-buffer. Mirrors the C4FM `dibit_last_buffer` semantics.
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

    /// Reads new LSM dibit DMA buffers since the last call. Same
    /// format as `read_dibit_buffers()` -- packed 64-bit dibit words.
    pub fn read_lsm_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::LsmDibit)
    }

    // ── Traffic-side LSM chain (Phase 7A.2) ──────────────────────
    //
    // Mirrors the control-side LSM helpers above (lines 600-768) but
    // against the new `traffic_lsm` register bank (offset 0xC0).
    // Same field semantics throughout -- the PS-side dispatcher polls
    // `traffic_lsm_status()` the same way the control-side
    // `lsm_status()` is polled.

    /// Master enable for the traffic-side LSM chain. Mirrors
    /// `set_lsm_enable` for the control side.
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
    /// Same one-pole leaky-integrator design as the control side.
    /// Production code should always set this to `true`.
    pub fn set_traffic_lsm_dc_block_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_dc_block_enable().bit(enable));
    }

    /// Enables or disables the Phase 10-prep per-symbol LSM AGC
    /// on the traffic chain. Mirrors `set_lsm_agc_enable` on the
    /// control side. Production code should always set this to
    /// `true` after boot.
    pub fn set_traffic_lsm_agc_enable(&self, enable: bool) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_agc_enable().bit(enable));
    }

    /// Phase 8A: pulse the traffic-side LSM chain runtime reset.
    ///
    /// Writes `1` to the W1P `traffic_lsm_reset` field in
    /// `traffic_lsm_control`, which the Register framework turns
    /// into a 1-sync-cycle pulse on `LsmDemod.reset_in` for the
    /// traffic chain. Clears the PLL accumulator + timing state +
    /// diff slicer history + sync register + BCH sweep state back
    /// to init. Self-clearing.
    ///
    /// This is the PRIMARY user of the reset plumbing -- the
    /// Phase 8B retune path calls this between the DDC frequency
    /// write and the LSM re-enable so the post-retune PLL
    /// acquisition starts from cold-boot semantics (pll_reg = 0)
    /// instead of inheriting the stale phase error from the old
    /// carrier, which was the root cause of the Phase 7 traffic
    /// audio quality problem (see doc/changes/037).
    pub fn pulse_traffic_lsm_reset(&self) {
        self.registers
            .traffic_lsm_control()
            .modify(|_, w| w.traffic_lsm_reset().bit(true));
    }

    /// Phase 8B + 8C: freeze-reset-thaw the traffic LSM chain
    /// across a DDC retune. Recommended entry point for the
    /// follower task whenever it retunes the traffic channel.
    ///
    /// Sequence:
    ///   1. `traffic_lsm_enable = 0`  — Phase 8C's
    ///      `lsm_traffic_dom` clock domain drops into synchronous
    ///      reset, clearing all non-`reset_less` state inside
    ///      LsmDemod in a single sync cycle (FSM state in the
    ///      sync / BCH / CORDIC submodules, pipeline stage
    ///      strobes, output latches). The C4FM chain is also
    ///      gated at `traffic_demod_enable = 0`.
    ///   2. Write the new DDC NCO frequency.
    ///   3. **Wait for the DDC FIR pipeline to flush** (2026-04-15
    ///      fix). The 3-stage cascaded FIR in maia_hdl.ddc.DDC has
    ///      total tap depth of ~600 µs after the P25DDC v2 fork
    ///      (176/128/256 taps across /4/4/8 decim stages). When
    ///      the NCO register is written, the mixer output
    ///      instantly uses the new frequency, but the
    ///      downstream FIR tap registers still contain convolution
    ///      history from samples mixed with the OLD NCO.
    ///      Convolving new samples with stale tap state produces a
    ///      transient that looks like a high-frequency chirp to
    ///      the LSM demod. The PLL immediately chases this phantom
    ///      signal, saturates its ±π/3 accumulator clamp
    ///      (pll_reg = ±8580 in Q2.13), and locks there — unable
    ///      to track the real post-flush signal.
    ///
    ///      Observed pre-fix on Duval County NAC 0x3BA, 2026-04-15:
    ///      control chain `pll_dbg=154` (healthy), traffic chain
    ///      `pll_dbg=8579` (exactly the ±π/3 Q2.13 saturation
    ///      limit — PLL stuck at clamp on every retune). Audio
    ///      was "robotic half the time" because the NID BCH
    ///      decoder corrected half the LDUs into TDU_LC
    ///      (all-ones DUID pattern, closest codeword to random
    ///      noise in the PLL-chase transient).
    ///
    ///      Wait duration: 2 ms. At the `rxiq_cdc` 8 MSPS input
    ///      rate, the FIR cascade pipeline budget is roughly:
    ///        stage 1 (176 taps @ 8 MSPS) = 22 µs
    ///        stage 2 (128 taps @ 2 MSPS) = 64 µs
    ///        stage 3 (256 taps @ 0.5 MSPS) = 512 µs
    ///        cascade total ≈ 600 µs
    ///      2 ms = ~3× the flush time, giving generous margin.
    ///      This is a `std::thread::sleep` because
    ///      `retune_traffic_chain` is a sync function and 2 ms
    ///      of tokio-runtime blocking is acceptable for a retune
    ///      event that happens at most ~1/second during normal
    ///      grant-follow operation.
    ///   4. `traffic_lsm_enable = 1`  — Phase 8C domain reset
    ///      deasserts. The chain is live again, but the
    ///      `reset_less=True` accumulators (PLL `pll_reg`,
    ///      timing `sample_point`, diff slicer `prev_*`, sync
    ///      shift register, BCH sweep counter, etc.) still hold
    ///      their pre-disable values.
    ///   5. Pulse `traffic_lsm_reset` — the Phase 8A explicit
    ///      `reset_in` path clears ALL of those `reset_less`
    ///      registers to init inside one sync cycle. This MUST
    ///      come after the domain is re-enabled because
    ///      `m.d.<domain>` assignments only fire when the domain
    ///      is not in reset.
    ///   6. `traffic_demod_enable = 1` — C4FM chain too (shares
    ///      the same upstream DDC).
    ///
    /// Without steps 1+3+5 the carryover of PLL state from the
    /// previous carrier produces corrupted dibits for hundreds
    /// of milliseconds, which is the Phase 7 "1 in 20 calls
    /// intelligible" symptom documented in doc/changes/037.
    pub fn retune_traffic_chain(
        &self,
        frequency_hz: f64,
        sample_rate_hz: f64,
    ) -> Result<()> {
        self.set_traffic_lsm_enable(false);
        self.set_traffic_demod_enable(false);
        self.set_traffic_ddc_frequency(frequency_hz, sample_rate_hz)?;
        // Phase 10D fix: let the DDC FIR cascade flush before
        // un-freezing the LSM chain. See step 3 in the docstring
        // above for the measurement that motivated this wait.
        std::thread::sleep(std::time::Duration::from_millis(2));
        self.set_traffic_lsm_enable(true);
        self.pulse_traffic_lsm_reset();
        self.set_traffic_demod_enable(true);
        Ok(())
    }

    /// Phase 8B: quiesce the traffic LSM + C4FM chains between
    /// calls. Counterpart to `retune_traffic_chain` -- used by
    /// the follower task on Idle→timeout and on encryption
    /// tear-down so the LSM chain stops producing phantom NID
    /// events during the gap between calls (which would otherwise
    /// still be accumulating spurious dibit noise into the BCH
    /// input, flooding the sync detector with flat-distribution
    /// noise syncs; see doc/changes/037).
    pub fn pause_traffic_chain(&self) {
        self.set_traffic_lsm_enable(false);
        self.set_traffic_demod_enable(false);
    }

    /// Reads back the `traffic_lsm_control` register as
    /// `(traffic_lsm_enable, traffic_lsm_dibit_dma_enable,
    /// traffic_lsm_dc_block_enable)`.
    pub fn traffic_lsm_control_readback(&self) -> (bool, bool, bool) {
        let c = self.registers.traffic_lsm_control().read();
        (
            c.traffic_lsm_enable().bit(),
            c.traffic_lsm_dibit_dma_enable().bit(),
            c.traffic_lsm_dc_block_enable().bit(),
        )
    }

    /// Reads the `traffic_lsm_status` register and returns a coherent
    /// snapshot. Same Rsticky semantics as `lsm_status()` -- the
    /// `nid_event` bit is cleared on read.
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

    /// Reads the traffic LSM debug taps: `pll_dbg` (signed Q2.13)
    /// and `sample_point_dbg` (signed Q4.10).
    pub fn traffic_lsm_debug(&self) -> (i16, i16) {
        let d = self.registers.traffic_lsm_debug().read();
        (d.pll_dbg().bits() as i16, d.sample_point_dbg().bits() as i16)
    }

    /// Reads new traffic LSM dibit DMA buffers since the last call.
    /// Same format as `read_dibit_buffers()` -- packed 64-bit dibit
    /// words.
    pub fn read_traffic_lsm_dibit_buffers(&mut self) -> Vec<&[u8]> {
        self.read_dma_buffers(DmaChannel::TrafficLsmDibit)
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
    Iq,
    LsmDibit,
    TrafficLsmDibit,  // Phase 7A.2
}

/// Snapshot of the `lsm_status` register read in a single bus access.
///
/// See `IpCore::lsm_status` for the per-NID coherency protocol.
#[derive(Debug, Clone, Copy)]
pub struct LsmStatusSnapshot {
    /// High while `LsmNidBchFec` is sweeping a candidate NID (~656 us).
    pub bch_busy: bool,
    /// High while `LsmSyncNidExtract` is collecting the 33-dibit NID
    /// payload after a sync hit (useful as a "have lock" indicator).
    pub in_nid_window: bool,
    /// Rsticky -- latches on each `nid_event_strobe`, cleared by this
    /// very read. True means a new NID event is described by the
    /// other fields in this snapshot + a follow-up `lsm_nid()` read.
    pub nid_event: bool,
    /// Latched copy of `LsmDemod.valid_out` for the most recent NID
    /// event: true when BCH Hamming distance <= 11.
    pub nid_valid: bool,
    /// Latched BCH Hamming distance (0..63) for the most recent NID.
    pub n_errors: u8,
    /// Latched 48-bit sync hit Hamming distance (0..47).
    pub sync_distance: u8,
    /// Rsticky -- latches when the LSM DibitPacker back-pressured the
    /// LSM dibit DMA ring.
    pub dibit_overflow: bool,
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
// 3-stage FIR decimation: 8 MSPS -> 62.5 kSPS (/128 = /4 /4 /8)
// P25 Phase 1 channel: 12.5 kHz (±6.25 kHz passband)
// Output: 62.5 kSPS = 13 samples/symbol @ 4800 baud
//
// Phase 10-prep redesign (see tools/p25_ddc_filter_design.py and
// doc/changes/040_ddc_filter_redesign.txt). Previous /16 /4 /2 split
// with 48 Kaiser beta=6 taps on stage 1 had a transition band wide
// enough that P25 adjacent-site emitters at ±500 kHz to ±2 MHz
// only saw 30-50 dB of rejection before being mixed into the
// control-channel output band, collapsing CRC pass rates at
// rf_bandwidth >= 5 MHz.
//
// New plan: Parks-McClellan equiripple filters on a /4 /4 /8 split
// so each stage's transition band fits comfortably in the tap
// budget while anchoring the stopband at the per-stage output
// Nyquist. Verified cascaded response meets -90+ dB across the
// full 500 kHz - 4 MHz adjacent range at 0.05 dB passband ripple.
//
// Stage 1 (FIR4DSP):  48 taps, pb=300 kHz,  sb=1000 kHz, -93 dB
// Stage 2 (FIR2DSP):  56 taps, pb=100 kHz,  sb=250  kHz, -93 dB
// Stage 3 (FIR4DSP): 104 taps, pb=10  kHz,  sb=31   kHz, -90 dB
//
// All three tap arrays fit cleanly: stage 1 and stage 3 FIR4DSPs
// have 256-slot coefficient RAMs each; stage 2 FIR2DSP has 128
// slots. The operations_minus_one / odd_operations fields are
// computed at runtime by load_fir1/2/3 from coefficients.len() /
// decimation, so no other code in this file needs to change when
// the tap arrays are edited.
//
// Close-in adjacents at +/-12.5 / +/-25 kHz intentionally land in
// stage 3's transition band at -0.6 / -25 dB -- they are finished
// off by the downstream LsmFir LPF (83 taps, passband 7250 Hz,
// stopband 8000 Hz, >100 dB) at 31.25 kSPS in the HDL LSM chain.
// Splitting sharp close-in filtering between the DDC and the
// LsmFir LPF keeps the stage-3 tap count manageable.
//
// Peak-scaling convention
// -----------------------
// Each stage's coefficients are scaled so the maximum
// |quantised| tap lands exactly at Q1.17 max (131071). This
// matches the original Kaiser-filter convention in this file and
// gives the downstream Maia DDC MAC + macc_trunc chain the
// non-unit per-stage DC gains it was tuned for (~7x / ~6x / ~14x,
// cascaded ~600x). The FIRST Phase 10-prep flash used unit-DC-gain
// remez outputs and starved the demod chain by ~700x, producing
// 0 % LSM NIDs + 55 % C4FM CRC on-target; the peak-scale rescale
// fixes that without touching the filter *shape* (it's a pure
// scalar multiplication). See doc/changes/040 for the full
// debrief.
//
// To regenerate: `python tools/p25_ddc_filter_design.py`.

// P25DDC fork v2: SDRTrunk-faithful, unit-DC-gain coefficient
// convention. Replaces the Phase 10-prep peak-rescaled tables. See
// doc/changes/041_p25ddc_fork.md for the design rationale and
// doc/changes/041_p25ddc_fork.txt for the raw design output.
//
// Key differences from the Phase 10-prep (040) tables:
//
//   * Each stage is unit DC gain (sum of coefficients ~= 131072 =
//     1 << 17). No peak-rescaling. This decouples coefficient
//     design from output scale -- the explicit per-stage
//     amplification now lives in P25DDC's macc_trunc=[14, 17, 17]
//     default (see maia-hdl/p25_hdl/p25ddc.py).
//
//   * Stage 3 uses the full FIR4DSP 256-tap budget (vs 104 in
//     Phase 10-prep). Passband tightened to 7.25 kHz (SDRTrunk's
//     baseband LPF passband edge). This is what fixes the 25 kHz
//     LsmDecimator2 fold-back: rejection at that offset goes from
//     -25 dB (Phase 10-prep) to -71 dB (v2).
//
//   * Stage 2 uses the full FIR2DSP 128-tap budget.
//
//   * Stage 1 stops at 176 taps because scipy's remez is
//     numerically unstable at higher counts for the specific
//     200 kHz / 1 MHz band configuration. -109 dB stopband is
//     already 35+ dB below the 12-bit ADC noise floor, so growing
//     further would be pure RAM waste.
//
// To regenerate: `python tools/p25_ddc_filter_design.py`.

const P25_DEC1: usize = 4;
const P25_DEC2: usize = 4;
const P25_DEC3: usize = 8;

// Stage 1 (FIR4DSP): 176 taps, pb=200 kHz, sb=1000 kHz,
// PM equiripple, unit DC gain. fs = 8 MSPS, /4 -> 2 MSPS.
#[rustfmt::skip]
const P25_FIR1_COEFFS: &[i32] = &[
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       1,       1,       1,       0,       0,
         -1,      -3,      -4,      -5,      -5,      -3,       0,       6,
         12,      19,      23,      23,      17,       3,     -18,     -42,
        -66,     -82,     -83,     -62,     -18,      48,     124,     196,
        243,     244,     186,      63,    -113,    -316,    -501,    -618,
       -620,    -474,    -174,     249,     726,    1157,    1428,    1433,
       1102,     423,    -535,   -1627,   -2636,   -3308,   -3393,   -2692,
      -1099,    1360,    4521,    8090,   11681,   14869,   17255,   18532,
      18532,   17255,   14869,   11681,    8090,    4521,    1360,   -1099,
      -2692,   -3393,   -3308,   -2636,   -1627,    -535,     423,    1102,
       1433,    1428,    1157,     726,     249,    -174,    -474,    -620,
       -618,    -501,    -316,    -113,      63,     186,     244,     243,
        196,     124,      48,     -18,     -62,     -83,     -82,     -66,
        -42,     -18,       3,      17,      23,      23,      19,      12,
          6,       0,      -3,      -5,      -5,      -4,      -3,      -1,
          0,       0,       1,       1,       1,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
];

// Stage 2 (FIR2DSP): 128 taps, pb=60 kHz, sb=250 kHz,
// PM equiripple, unit DC gain. fs = 2 MSPS, /4 -> 500 kSPS.
#[rustfmt::skip]
const P25_FIR2_COEFFS: &[i32] = &[
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,      -1,      -1,      -1,       0,
          1,       2,       4,       6,       7,       6,       2,      -5,
        -15,     -27,     -36,     -40,     -32,     -10,      26,      72,
        119,     153,     158,     118,      28,    -106,    -264,    -409,
       -496,    -478,    -325,     -32,     368,     802,    1164,    1335,
       1210,     731,     -86,   -1136,   -2221,   -3076,   -3412,   -2970,
      -1581,     784,    3988,    7732,   11588,   15068,   17703,   19122,
      19122,   17703,   15068,   11588,    7732,    3988,     784,   -1581,
      -2970,   -3412,   -3076,   -2221,   -1136,     -86,     731,    1210,
       1335,    1164,     802,     368,     -32,    -325,    -478,    -496,
       -409,    -264,    -106,      28,     118,     158,     153,     119,
         72,      26,     -10,     -32,     -40,     -36,     -27,     -15,
         -5,       2,       6,       7,       6,       4,       2,       1,
          0,      -1,      -1,      -1,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
];

// Stage 3 (FIR4DSP): 256 taps, pb=7.25 kHz, sb=31.25 kHz,
// PM equiripple, unit DC gain. fs = 500 kSPS, /8 -> 62.5 kSPS.
// This is the critical one: fills the full FIR4DSP 256-tap budget
// for a very steep equiripple transition, which is what pushes
// 25 kHz fold-back rejection from -25 dB (Phase 10-prep) to
// -71 dB (v2). See doc/changes/041_p25ddc_fork.md.
#[rustfmt::skip]
const P25_FIR3_COEFFS: &[i32] = &[
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,      -1,      -1,      -1,      -1,
         -1,      -1,      -2,      -2,      -2,      -2,      -2,      -2,
         -2,      -1,      -1,       0,       1,       2,       4,       6,
          8,      10,      13,      15,      18,      20,      21,      23,
         23,      22,      20,      17,      12,       5,      -3,     -13,
        -25,     -38,     -52,     -67,     -81,     -94,    -106,    -115,
       -120,    -121,    -116,    -105,     -87,     -62,     -30,       9,
         55,     106,     160,     217,     272,     324,     370,     406,
        430,     437,     427,     395,     340,     262,     161,      38,
       -105,    -265,    -435,    -611,    -786,    -950,   -1096,   -1214,
      -1295,   -1330,   -1311,   -1230,   -1081,    -860,    -564,    -193,
        250,     760,    1332,    1954,    2616,    3303,    4002,    4694,
       5365,    5996,    6573,    7079,    7502,    7830,    8053,    8166,
       8166,    8053,    7830,    7502,    7079,    6573,    5996,    5365,
       4694,    4002,    3303,    2616,    1954,    1332,     760,     250,
       -193,    -564,    -860,   -1081,   -1230,   -1311,   -1330,   -1295,
      -1214,   -1096,    -950,    -786,    -611,    -435,    -265,    -105,
         38,     161,     262,     340,     395,     427,     437,     430,
        406,     370,     324,     272,     217,     160,     106,      55,
          9,     -30,     -62,     -87,    -105,    -116,    -121,    -120,
       -115,    -106,     -94,     -81,     -67,     -52,     -38,     -25,
        -13,      -3,       5,      12,      17,      20,      22,      23,
         23,      21,      20,      18,      15,      13,      10,       8,
          6,       4,       2,       1,       0,      -1,      -1,      -2,
         -2,      -2,      -2,      -2,      -2,      -2,      -1,      -1,
         -1,      -1,      -1,      -1,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
          0,       0,       0,       0,       0,       0,       0,       0,
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
    notify_iq_dma: Arc<Notify>,
    notify_lsm_dibit_dma: Arc<Notify>,
    notify_traffic_lsm_dibit_dma: Arc<Notify>,  // Phase 7A.2
}

impl InterruptHandler {
    fn new(uio: Uio, registers: Registers) -> Self {
        InterruptHandler {
            uio,
            registers,
            notify_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_dma: Arc::new(Notify::new()),
            notify_iq_dma: Arc::new(Notify::new()),
            notify_lsm_dibit_dma: Arc::new(Notify::new()),
            notify_traffic_lsm_dibit_dma: Arc::new(Notify::new()),
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

    /// Returns a waiter for post-DDC IQ ring DMA completion interrupts
    /// (Phase 6C/6D).
    pub fn waiter_iq_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_iq_dma.clone(),
        }
    }

    /// Returns a waiter for LSM control-channel dibit DMA completion
    /// interrupts (Phase 6E.9). NID events themselves are PS-polled via
    /// `IpCore::lsm_status()` rather than IRQ-driven.
    pub fn waiter_lsm_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_lsm_dibit_dma.clone(),
        }
    }

    /// Returns a waiter for traffic-side LSM dibit DMA completion
    /// interrupts (Phase 7A.2). Same convention as the control side:
    /// NID events for HDU/TDU/LDU dispatch are PS-polled via
    /// `IpCore::traffic_lsm_status()`, IRQ-driven only for the dibit
    /// DMA ring drain.
    pub fn waiter_traffic_lsm_dibit_dma(&self) -> InterruptWaiter {
        InterruptWaiter {
            notify: self.notify_traffic_lsm_dibit_dma.clone(),
        }
    }

    /// Runs the interrupt handler loop.
    ///
    /// `irq_stats` is the shared `Arc<Mutex<IrqStats>>` from the
    /// dashboard wiring; the handler updates it on every IRQ so the
    /// `/api/irq_stats` endpoint can return live counters without
    /// having to grep the log.
    ///
    /// This should be spawned as a background tokio task.
    pub async fn run(
        mut self,
        irq_stats: std::sync::Arc<tokio::sync::Mutex<crate::IrqStats>>,
    ) -> Result<()> {
        let mut total_irqs: u64 = 0;
        let mut dibit_irqs: u64 = 0;
        let mut traffic_irqs: u64 = 0;
        let mut iq_irqs: u64 = 0;
        let mut lsm_dibit_irqs: u64 = 0;
        let mut traffic_lsm_dibit_irqs: u64 = 0;  // Phase 7A.2
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            let dibit = interrupts.dibit_dma().bit();
            let traffic = interrupts.traffic_dma().bit();
            let iq = interrupts.iq_dma().bit();
            let lsm_dibit = interrupts.lsm_dibit_dma().bit();
            let traffic_lsm_dibit = interrupts.traffic_lsm_dibit_dma().bit();
            total_irqs += 1;
            if dibit {
                dibit_irqs += 1;
                self.notify_dibit_dma.notify_waiters();
            }
            if traffic {
                traffic_irqs += 1;
                self.notify_traffic_dma.notify_waiters();
            }
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
            // Update shared stats. Cheap async lock, no contention
            // because nothing else writes this struct.
            {
                let now = std::time::Instant::now();
                let mut s = irq_stats.lock().await;
                if s.started_at.is_none() {
                    s.started_at = Some(now);
                }
                s.total = total_irqs;
                s.dibit = dibit_irqs;
                s.traffic = traffic_irqs;
                s.iq = iq_irqs;
                s.lsm_dibit = lsm_dibit_irqs;
                s.traffic_lsm_dibit = traffic_lsm_dibit_irqs;
                s.last_at = Some(now);
            }
            // Log first 10 then every 64th to avoid flooding
            if total_irqs <= 10 || total_irqs % 64 == 0 {
                tracing::info!(
                    target: "p25_irq",
                    "IRQ #{total_irqs}: dibit={dibit} traffic={traffic} iq={iq} \
                     lsm_dibit={lsm_dibit} traffic_lsm_dibit={traffic_lsm_dibit} \
                     (totals dibit={dibit_irqs} traffic={traffic_irqs} \
                     iq={iq_irqs} lsm_dibit={lsm_dibit_irqs} \
                     traffic_lsm_dibit={traffic_lsm_dibit_irqs})"
                );
            }
        }
    }
}

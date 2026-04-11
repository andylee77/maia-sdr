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
    dibit_last_addr: Option<u32>,
    traffic_last_addr: Option<u32>,
    iq_last_addr: Option<u32>,
    lsm_dibit_last_addr: Option<u32>,
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

        let ip_core = IpCore {
            registers,
            dibit_dma,
            traffic_dma,
            iq_dma,
            lsm_dibit_dma,
            dibit_last_addr: None,
            traffic_last_addr: None,
            iq_last_addr: None,
            lsm_dibit_last_addr: None,
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

    /// Reads back the `lsm_control` register as `(lsm_enable,
    /// lsm_dibit_dma_enable)`. Used at startup to confirm the bits we
    /// wrote actually stuck in the register bank.
    pub fn lsm_control_readback(&self) -> (bool, bool) {
        let c = self.registers.lsm_control().read();
        (c.lsm_enable().bit(), c.lsm_dibit_dma_enable().bit())
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
    notify_iq_dma: Arc<Notify>,
    notify_lsm_dibit_dma: Arc<Notify>,
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
        loop {
            self.uio.irq_enable().await?;
            self.uio.irq_wait().await?;

            let interrupts = self.registers.interrupts().read();
            let dibit = interrupts.dibit_dma().bit();
            let traffic = interrupts.traffic_dma().bit();
            let iq = interrupts.iq_dma().bit();
            let lsm_dibit = interrupts.lsm_dibit_dma().bit();
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
                s.last_at = Some(now);
            }
            // Log first 10 then every 64th to avoid flooding
            if total_irqs <= 10 || total_irqs % 64 == 0 {
                tracing::info!(
                    target: "p25_irq",
                    "IRQ #{total_irqs}: dibit={dibit} traffic={traffic} iq={iq} \
                     lsm_dibit={lsm_dibit} (totals dibit={dibit_irqs} \
                     traffic={traffic_irqs} iq={iq_irqs} \
                     lsm_dibit={lsm_dibit_irqs})"
                );
            }
        }
    }
}

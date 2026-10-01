//! One receive chain of the core: a DDC (FIR decimator and NCO) and the LSM demodulator behind
//! it. The control chain and both traffic chains share this code; only the register bank
//! differs.
//!
//! A traffic chain reports every hardware action (NCO write, LSM reset, enable change) to its
//! epoch sink with the dibit ring position read right after it, so the stream reader can cut
//! the dibits at the moment of the action. The caller holds the core's lock, which orders these
//! reports with the reader's snapshots.

use anyhow::{bail, Result};

use super::regs::{freq_to_nco, Bank, LsmControl, LsmStatus, RegisterBlock};
use super::rings::{mono_us, EpochSink, HwAction};
use crate::hardware::presets::{DdcPreset, FirLoad, FIR_BASE};

pub struct Chain<'a> {
    pub(super) regs: &'a RegisterBlock,
    pub(super) bank: Bank,
    pub(super) sink: Option<&'a dyn EpochSink>,
}

impl Chain<'_> {
    pub fn bank(&self) -> Bank {
        self.bank
    }

    /// Load `preset`'s FIR stages and set the NCO to `nco_hz` (an offset from the LO).
    pub fn configure_ddc(&self, nco_hz: f64, preset: &DdcPreset) -> Result<()> {
        let load = FirLoad::of(preset)?;
        for (stage, ram) in load.ram.iter().enumerate() {
            for (i, &coeff) in ram.iter().enumerate() {
                self.bank.write_coeff(self.regs, (FIR_BASE[stage] + i) as u16, coeff);
            }
        }
        self.bank.set_stages(self.regs, &load.stages);
        self.set_nco(nco_hz, preset.sample_rate_hz as f64)?;
        tracing::info!(
            "{:?} DDC: preset {} ({}x decimation), NCO {:+.0} Hz",
            self.bank,
            preset.name,
            preset.total_decim(),
            nco_hz
        );
        Ok(())
    }

    /// Set the NCO to an offset within ±sample_rate/2 of the LO.
    pub fn set_nco(&self, nco_hz: f64, sample_rate_hz: f64) -> Result<()> {
        let before = self.lsm_enabled();
        self.write_nco(nco_hz, sample_rate_hz)?;
        self.epoch(HwAction::NcoWrite, before);
        Ok(())
    }

    fn write_nco(&self, nco_hz: f64, sample_rate_hz: f64) -> Result<()> {
        let half = sample_rate_hz / 2.0;
        if !(-half..=half).contains(&nco_hz) {
            bail!("{:?} NCO {nco_hz} Hz is outside ±{half} Hz", self.bank);
        }
        self.bank.set_nco(self.regs, freq_to_nco(nco_hz, sample_rate_hz));
        Ok(())
    }

    pub fn nco_word(&self) -> u32 {
        self.bank.nco(self.regs)
    }

    /// Feed the DDC with samples (or stop it).
    pub fn set_input(&self, on: bool) {
        self.bank.set_input(self.regs, on);
    }

    /// Move a traffic chain to a channel: NCO, optionally an LSM reset, and the chain on, as one
    /// action. Without the reset the AGC, PLL and timing carry over from the previous call.
    pub fn retune(&self, nco_hz: f64, sample_rate_hz: f64, reset: bool) -> Result<()> {
        let before = self.lsm_enabled();
        self.write_nco(nco_hz, sample_rate_hz)?;
        if reset {
            self.bank.pulse_reset(self.regs);
        }
        self.bank.set_lsm_enable(self.regs, true);
        self.epoch(HwAction::Retune { lsm_reset: reset }, before);
        Ok(())
    }

    pub fn lsm_enabled(&self) -> bool {
        self.bank.lsm_control(self.regs).enable
    }

    pub fn set_lsm_enable(&self, on: bool) {
        let before = self.lsm_enabled();
        self.bank.set_lsm_enable(self.regs, on);
        if before != on {
            self.epoch(HwAction::Enable(on), before);
        }
    }

    pub fn pulse_reset(&self) {
        let before = self.lsm_enabled();
        self.bank.pulse_reset(self.regs);
        self.epoch(HwAction::LsmReset, before);
    }

    pub fn set_dibit_dma(&self, on: bool) {
        self.bank.set_dibit_dma(self.regs, on);
    }

    pub fn set_dc_block(&self, on: bool) {
        self.bank.set_dc_block(self.regs, on);
    }

    pub fn set_agc(&self, on: bool) {
        self.bank.set_agc(self.regs, on);
    }

    pub fn lsm_control(&self) -> LsmControl {
        self.bank.lsm_control(self.regs)
    }

    pub fn status(&self) -> LsmStatus {
        self.bank.status(self.regs)
    }

    pub fn nid(&self) -> (u16, u8) {
        self.bank.nid(self.regs)
    }

    pub fn drop_count(&self) -> u16 {
        self.bank.drop_count(self.regs)
    }

    pub fn dibit_last_buffer(&self) -> u8 {
        self.bank.dibit_last_buffer(self.regs)
    }

    pub fn dibit_next(&self) -> u32 {
        self.bank.dibit_next(self.regs)
    }

    pub fn debug(&self) -> (i16, i16) {
        self.bank.debug(self.regs)
    }

    pub fn agc_debug(&self) -> (u16, u16) {
        self.bank.agc_debug(self.regs)
    }

    pub fn agc_threshold(&self) -> u16 {
        self.bank.agc_threshold(self.regs)
    }

    pub fn set_agc_threshold(&self, v: u16) {
        self.bank.set_agc_threshold(self.regs, v);
    }

    fn epoch(&self, action: HwAction, enabled_before: bool) {
        if let Some(sink) = self.sink {
            sink.record_hw(action, mono_us(), self.dibit_next(), enabled_before);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::super::regs::{nco_to_freq, Registers};
    use super::*;
    use crate::hardware::presets::find_preset;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<(HwAction, bool)>>);

    impl EpochSink for Recorder {
        fn record_hw(&self, action: HwAction, _t_us: u64, _next_address: u32, enabled_before: bool) {
            self.0.lock().unwrap().push((action, enabled_before));
        }
    }

    #[test]
    fn a_preset_sets_the_stages_and_the_nco() {
        let regs = Registers::in_memory();
        let chain = Chain { regs: &regs, bank: Bank::Traffic2, sink: None };
        let p = find_preset("12M").unwrap();
        chain.configure_ddc(-1_500_000.0, p).unwrap();
        let f = FirLoad::of(p).unwrap().stages;
        let d = regs.traffic2_ddc_decimation().read();
        assert_eq!(
            (d.traffic2_decimation1().bits(), d.traffic2_decimation2().bits(), d.traffic2_decimation3().bits()),
            (f[0].decimation, f[1].decimation, f[2].decimation)
        );
        assert!((nco_to_freq(chain.nco_word(), p.sample_rate_hz as f64) + 1_500_000.0).abs() < 0.1);
        assert!(chain.set_nco(7_000_000.0, p.sample_rate_hz as f64).is_err(), "outside the window");
    }

    #[test]
    fn a_traffic_retune_is_one_epoch_and_enables_the_chain() {
        let regs = Registers::in_memory();
        let rec = Recorder::default();
        let chain = Chain { regs: &regs, bank: Bank::Traffic, sink: Some(&rec) };
        chain.retune(250_000.0, 8e6, true).unwrap();
        assert!(chain.lsm_enabled());
        chain.set_lsm_enable(true); // no change, no epoch
        chain.set_lsm_enable(false);
        let got = rec.0.lock().unwrap().clone();
        assert_eq!(got, vec![(HwAction::Retune { lsm_reset: true }, false), (HwAction::Enable(false), true)]);
    }
}

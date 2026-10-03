//! One lane of the core: a DDC (FIR decimator and NCO) and the lane's packets. Every lane is the
//! same; only the register bank differs.

use anyhow::{bail, Result};

use super::regs::{freq_to_nco, LaneBank, RegisterBlock};
use crate::hardware::presets::{DdcPreset, FirLoad, FIR_BASE};

pub struct LaneDdc<'a> {
    pub(super) regs: &'a RegisterBlock,
    pub(super) bank: LaneBank,
}

impl LaneDdc<'_> {
    pub fn number(&self) -> usize {
        self.bank.0
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
            "lane {} DDC: preset {} ({}x decimation), NCO {:+.0} Hz",
            self.number(),
            preset.name,
            preset.total_decim(),
            nco_hz
        );
        Ok(())
    }

    /// Set the NCO to an offset within ±sample_rate/2 of the LO.
    pub fn set_nco(&self, nco_hz: f64, sample_rate_hz: f64) -> Result<()> {
        let half = sample_rate_hz / 2.0;
        if !(-half..=half).contains(&nco_hz) {
            bail!("lane {} NCO {nco_hz} Hz is outside ±{half} Hz", self.number());
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

    /// The lane's packets on or off, tagged `tag`. Written after the NCO, so every sample of a
    /// tag comes from that tuning (less the DDC's settling, which the reader drops).
    pub fn set_packets(&self, on: bool, tag: u16) {
        self.bank.set_control(self.regs, on, tag);
    }

    /// Packets on, and their tag.
    pub fn packets(&self) -> (bool, u16) {
        self.bank.control(self.regs)
    }
}

#[cfg(test)]
mod tests {
    use super::super::regs::{nco_to_freq, Registers};
    use super::*;
    use crate::hardware::presets::find_preset;

    #[test]
    fn a_preset_sets_the_stages_and_the_nco() {
        let regs = Registers::in_memory();
        let lane = LaneDdc { regs: &regs, bank: LaneBank(2) };
        let p = find_preset("12M").unwrap();
        lane.configure_ddc(-1_500_000.0, p).unwrap();
        let f = FirLoad::of(p).unwrap().stages;
        let d = regs.lane2_ddc_decimation().read();
        assert_eq!(
            (d.decimation1().bits(), d.decimation2().bits(), d.decimation3().bits()),
            (f[0].decimation, f[1].decimation, f[2].decimation)
        );
        assert!((nco_to_freq(lane.nco_word(), p.sample_rate_hz as f64) + 1_500_000.0).abs() < 0.1);
        assert!(lane.set_nco(7_000_000.0, p.sample_rate_hz as f64).is_err(), "outside the window");
    }

    #[test]
    fn packets_carry_the_tag_written_with_them() {
        let regs = Registers::in_memory();
        let lane = LaneDdc { regs: &regs, bank: LaneBank(1) };
        lane.set_packets(true, 7);
        assert_eq!(lane.packets(), (true, 7));
        lane.set_packets(false, 7);
        assert_eq!(lane.packets(), (false, 7));
    }
}

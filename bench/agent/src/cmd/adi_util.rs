//! Shared helpers for the axi_ad9361 interface tests (PN monitors, IDELAY,
//! AD9361 delay registers).

use super::{open_core, Ctx};
use crate::access::{RegAccess, WriteOpts};
use crate::err::{AResult, AgentError, Code};
use crate::iio::Ad9361;
use crate::regio::PhysMap;
use crate::safety;
use serde_json::{json, Value};

pub const DATA_LANES: usize = 6;
pub const FRAME_LANE: usize = 6;
pub const PN_SEL_AD9361: u32 = 0;
pub const PN_SEL_PN9: u32 = 9;
/// AD9361 SPI: RX clock/data delay, TX clock/data delay.
pub const SPI_RX_DELAY: u32 = 0x006;
pub const SPI_TX_DELAY: u32 = 0x007;

pub struct Adi<'m> {
    pub adc: RegAccess<'m, PhysMap>,
    pub phy: Ad9361,
}

#[derive(Debug, Clone, Copy)]
pub struct PnCheck {
    pub pass: bool,
    pub status_ok: bool,
    pub ch: [u32; 2],
}

impl PnCheck {
    pub fn to_json(self) -> Value {
        json!({"pass": self.pass, "if_status": self.status_ok,
               "ch0": {"pn_oos": (self.ch[0] >> 1) & 1, "pn_err": (self.ch[0] >> 2) & 1},
               "ch1": {"pn_oos": (self.ch[1] >> 1) & 1, "pn_err": (self.ch[1] >> 2) & 1}})
    }
}

impl<'m> Adi<'m> {
    pub fn open(ctx: &'m Ctx) -> AResult<Adi<'m>> {
        let (core, pm) = open_core(ctx, "adi_adc", true)?;
        let phy = Ad9361::open()?;
        Ok(Adi {
            adc: RegAccess::new(core, pm),
            phy,
        })
    }

    pub fn cntrl3(&mut self, ch: usize) -> AResult<u32> {
        self.adc.read(&format!("CHAN{ch}_CNTRL_3"))
    }

    pub fn set_cntrl3(&mut self, ch: usize, v: u32) -> AResult<()> {
        self.adc.write(&format!("CHAN{ch}_CNTRL_3"), v)
    }

    /// Sets PN_SEL ([19:16]) keeping DATA_SEL; returns the old register.
    pub fn set_pn_sel(&mut self, ch: usize, sel: u32) -> AResult<u32> {
        let old = self.cntrl3(ch)?;
        self.set_cntrl3(ch, (old & !(0xF << 16)) | ((sel & 0xF) << 16))?;
        Ok(old)
    }

    /// Sets DATA_SEL ([3:0]) keeping PN_SEL; returns the old register.
    pub fn set_data_sel(&mut self, ch: usize, sel: u32) -> AResult<u32> {
        let old = self.cntrl3(ch)?;
        self.set_cntrl3(ch, (old & !0xF) | (sel & 0xF))?;
        Ok(old)
    }

    pub fn idelay(&mut self, lane: usize) -> AResult<u32> {
        let v = self.adc.read(&format!("IDELAY_{lane}"))?;
        if v == 0xFFFF_FFFF {
            return Err(AgentError::new(
                Code::Precondition,
                "IDELAY readback 0xFFFFFFFF: IDELAYCTRL not locked (no 200 MHz reference?)",
            ));
        }
        Ok(v & 0x1F)
    }

    pub fn set_idelay(&mut self, lane: usize, tap: u32) -> AResult<()> {
        self.adc.write(&format!("IDELAY_{lane}"), tap & 0x1F)
    }

    pub fn idelays(&mut self) -> AResult<Vec<u32>> {
        (0..=FRAME_LANE).map(|l| self.idelay(l)).collect()
    }

    /// Clears the sticky PN status, waits `dwell_ms`, then samples it.
    pub fn check(&mut self, dwell_ms: u64) -> AResult<PnCheck> {
        for ch in 0..2 {
            self.adc.write(&format!("CHAN{ch}_STATUS"), 0x7)?;
        }
        safety::sleep_ms(dwell_ms.max(1))?;
        let st = self.adc.read("STATUS")?;
        let c0 = self.adc.read("CHAN0_STATUS")?;
        let c1 = self.adc.read("CHAN1_STATUS")?;
        let status_ok = st & 1 == 1;
        let bad = |v: u32| v & 0x6 != 0;
        Ok(PnCheck {
            pass: status_ok && !bad(c0) && !bad(c1),
            status_ok,
            ch: [c0, c1],
        })
    }

    pub fn clk_freq_hz(&mut self) -> Option<f64> {
        self.adc.read("CLK_FREQ").ok().map(|n| (n as f64 * 100e6 / 65536.0).round())
    }
}

/// DAC side (TX-affecting writes are only issued behind a TxGuard).
pub struct Dac<'m> {
    pub dac: RegAccess<'m, PhysMap>,
}

pub const TX_OK: WriteOpts = WriteOpts {
    force_dangerous: false,
    tx_ok: true,
};

impl<'m> Dac<'m> {
    pub fn open(ctx: &'m Ctx) -> AResult<Dac<'m>> {
        let (core, pm) = open_core(ctx, "adi_dac", true)?;
        Ok(Dac {
            dac: RegAccess::new(core, pm),
        })
    }

    pub fn data_sel(&mut self, ch: usize, sel: u32) -> AResult<()> {
        self.dac.write_opts(&format!("DAC_CHAN{ch}_CNTRL_7"), sel & 0xF, TX_OK)
    }

    pub fn sync(&mut self) -> AResult<()> {
        self.dac.write("DAC_CNTRL_1", 1)
    }

    pub fn clksel(&mut self) -> AResult<u32> {
        Ok(self.dac.read("DAC_CLKSEL")? & 1)
    }

    pub fn set_clksel(&mut self, v: u32) -> AResult<()> {
        self.dac.write("DAC_CLKSEL", v & 1)
    }
}

/// Current `in_voltage_sampling_frequency`, optionally switching to `rate`.
/// Returns (original, active).
pub fn apply_rate(phy: &Ad9361, rate: Option<u64>) -> AResult<(Option<u64>, Option<u64>)> {
    let orig = phy.sample_rate();
    if let Some(r) = rate {
        if Some(r) != orig {
            phy.set_attr("in_voltage_sampling_frequency", &r.to_string())?;
            safety::sleep_ms(200)?;
        }
    }
    Ok((orig, phy.sample_rate()))
}

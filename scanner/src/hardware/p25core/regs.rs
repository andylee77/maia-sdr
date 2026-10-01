//! Register access to the P25 core, and the three register banks that each drive one DDC and one
//! LSM chain: the control chain (`ddc_*`, `lsm_*`) and traffic chains one (`traffic_*`) and two
//! (`traffic2_*`). The banks have the same fields under different names; `bank!` generates one
//! accessor module per bank from its prefixes, and `Bank` dispatches to them.

use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

pub use p25_pac::fishball_p25::RegisterBlock;

use crate::hardware::presets::StageFields;

/// The register block, kept mapped while any holder lives.
#[derive(Clone)]
pub struct Registers {
    block: *const RegisterBlock,
    _owner: Arc<dyn Any + Send + Sync>,
}

// Registers are volatile cells in device memory; every access is a single bus read or write.
unsafe impl Send for Registers {}
unsafe impl Sync for Registers {}

impl Registers {
    #[cfg(target_os = "linux")]
    pub fn mapped(mapping: Arc<crate::hardware::mmio::Mapping>) -> Registers {
        let block = mapping.addr() as *const RegisterBlock;
        Registers { block, _owner: mapping }
    }

    /// A register block in plain memory, all zero (host tests).
    #[cfg(test)]
    pub fn in_memory() -> Registers {
        let words = std::mem::size_of::<RegisterBlock>().div_ceil(8);
        let memory: Arc<Vec<u64>> = Arc::new(vec![0u64; words]);
        let block = memory.as_ptr() as *const RegisterBlock;
        Registers { block, _owner: memory }
    }
}

impl Deref for Registers {
    type Target = RegisterBlock;

    fn deref(&self) -> &RegisterBlock {
        unsafe { &*self.block }
    }
}

/// `lsm_status`: one coherent read.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct LsmStatus {
    pub bch_busy: bool,
    pub in_nid_window: bool,
    pub nid_event: bool,
    pub nid_valid: bool,
    pub n_errors: u8,
    pub sync_distance: u8,
    pub dibit_overflow: bool,
}

/// `lsm_control` read back.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct LsmControl {
    pub enable: bool,
    pub dibit_dma: bool,
    pub dc_block: bool,
    pub agc: bool,
}

macro_rules! bank {
    ($name:ident, ddc = $dr:ident, ddc_fields = [$($df:ident)?], lsm = $lp:ident) => { paste::paste! {
        pub mod $name {
            use super::{LsmControl, LsmStatus, RegisterBlock, StageFields};

            pub fn write_coeff(r: &RegisterBlock, addr: u16, data: i32) {
                r.[<$dr coeff_addr>]().modify(|_, w| unsafe { w.[<$($df)? coeff_waddr>]().bits(addr) });
                r.[<$dr coeff>]().modify(|_, w| unsafe {
                    w.[<$($df)? coeff_wren>]().bit(true).[<$($df)? coeff_wdata>]().bits(data as u32)
                });
            }

            /// Decimation and operation counts of the three stages, every stage enabled.
            pub fn set_stages(r: &RegisterBlock, s: &[StageFields; 3]) {
                r.[<$dr decimation>]().modify(|_, w| unsafe {
                    w.[<$($df)? decimation1>]().bits(s[0].decimation)
                        .[<$($df)? decimation2>]().bits(s[1].decimation)
                        .[<$($df)? decimation3>]().bits(s[2].decimation)
                });
                r.[<$dr control>]().modify(|_, w| unsafe {
                    w.[<$($df)? operations_minus_one1>]().bits(s[0].operations_minus_one)
                        .[<$($df)? odd_operations1>]().bit(s[0].odd_operations)
                        .[<$($df)? operations_minus_one2>]().bits(s[1].operations_minus_one)
                        .[<$($df)? operations_minus_one3>]().bits(s[2].operations_minus_one)
                        .[<$($df)? odd_operations3>]().bit(s[2].odd_operations)
                        .[<$($df)? bypass2>]().clear_bit()
                        .[<$($df)? bypass3>]().clear_bit()
                });
            }

            pub fn set_nco(r: &RegisterBlock, word: u32) {
                r.[<$dr frequency>]().modify(|_, w| unsafe { w.[<$($df)? frequency>]().bits(word) });
            }

            pub fn nco(r: &RegisterBlock) -> u32 {
                r.[<$dr frequency>]().read().[<$($df)? frequency>]().bits()
            }

            pub fn set_input(r: &RegisterBlock, on: bool) {
                r.[<$dr control>]().modify(|_, w| w.[<$($df)? enable_input>]().bit(on));
            }

            pub fn lsm_control(r: &RegisterBlock) -> LsmControl {
                let c = r.[<$lp control>]().read();
                LsmControl {
                    enable: c.[<$lp enable>]().bit(),
                    dibit_dma: c.[<$lp dibit_dma_enable>]().bit(),
                    dc_block: c.[<$lp dc_block_enable>]().bit(),
                    agc: c.[<$lp agc_enable>]().bit(),
                }
            }

            pub fn set_lsm_enable(r: &RegisterBlock, on: bool) {
                r.[<$lp control>]().modify(|_, w| w.[<$lp enable>]().bit(on));
            }

            pub fn set_dibit_dma(r: &RegisterBlock, on: bool) {
                r.[<$lp control>]().modify(|_, w| w.[<$lp dibit_dma_enable>]().bit(on));
            }

            pub fn set_dc_block(r: &RegisterBlock, on: bool) {
                r.[<$lp control>]().modify(|_, w| w.[<$lp dc_block_enable>]().bit(on));
            }

            pub fn set_agc(r: &RegisterBlock, on: bool) {
                r.[<$lp control>]().modify(|_, w| w.[<$lp agc_enable>]().bit(on));
            }

            /// Clears the AGC, PLL and timing accumulators (a one-cycle pulse).
            pub fn pulse_reset(r: &RegisterBlock) {
                r.[<$lp control>]().modify(|_, w| w.[<$lp reset>]().bit(true));
            }

            pub fn status(r: &RegisterBlock) -> LsmStatus {
                let s = r.[<$lp status>]().read();
                LsmStatus {
                    bch_busy: s.bch_busy().bit(),
                    in_nid_window: s.in_nid_window().bit(),
                    nid_event: s.nid_event().bit(),
                    nid_valid: s.nid_valid().bit(),
                    n_errors: s.n_errors().bits(),
                    sync_distance: s.sync_distance().bits(),
                    dibit_overflow: s.[<$lp dibit_overflow>]().bit(),
                }
            }

            /// NAC and DUID of the latest NID.
            pub fn nid(r: &RegisterBlock) -> (u16, u8) {
                let n = r.[<$lp nid>]().read();
                (n.nac().bits(), n.duid().bits())
            }

            pub fn dibit_last_buffer(r: &RegisterBlock) -> u8 {
                r.[<$lp drop_count>]().read().[<$lp dibit_last_buffer>]().bits()
            }

            /// Address of the next dibit DMA burst (plain read, no side effect).
            pub fn dibit_next(r: &RegisterBlock) -> u32 {
                r.[<$lp dibit_next>]().read().next_address().bits()
            }

            /// PLL accumulator (Q2.13) and Gardner sample point (Q4.10).
            pub fn debug(r: &RegisterBlock) -> (i16, i16) {
                let d = r.[<$lp debug>]().read();
                (d.pll_dbg().bits() as i16, d.sample_point_dbg().bits() as i16)
            }

            /// AGC gain (Q9.7) and input magnitude (Q1.15).
            pub fn agc_debug(r: &RegisterBlock) -> (u16, u16) {
                let d = r.[<$lp agc_debug>]().read();
                (d.agc_gain_dbg().bits(), d.agc_mag_dbg().bits())
            }
        }
    }};
}

bank!(control, ddc = ddc_, ddc_fields = [], lsm = lsm_);
bank!(traffic, ddc = traffic_ddc_, ddc_fields = [traffic_], lsm = traffic_lsm_);
bank!(traffic2, ddc = traffic2_ddc_, ddc_fields = [traffic2_], lsm = traffic2_lsm_);

/// One of the three DDC + LSM register banks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bank {
    Control,
    Traffic,
    Traffic2,
}

macro_rules! dispatch {
    ($bank:expr, $f:ident($($arg:expr),*)) => {
        match $bank {
            Bank::Control => control::$f($($arg),*),
            Bank::Traffic => traffic::$f($($arg),*),
            Bank::Traffic2 => traffic2::$f($($arg),*),
        }
    };
}

impl Bank {
    pub fn write_coeff(self, r: &RegisterBlock, addr: u16, data: i32) {
        dispatch!(self, write_coeff(r, addr, data))
    }
    pub fn set_stages(self, r: &RegisterBlock, s: &[StageFields; 3]) {
        dispatch!(self, set_stages(r, s))
    }
    pub fn set_nco(self, r: &RegisterBlock, word: u32) {
        dispatch!(self, set_nco(r, word))
    }
    pub fn nco(self, r: &RegisterBlock) -> u32 {
        dispatch!(self, nco(r))
    }
    pub fn set_input(self, r: &RegisterBlock, on: bool) {
        dispatch!(self, set_input(r, on))
    }
    pub fn lsm_control(self, r: &RegisterBlock) -> LsmControl {
        dispatch!(self, lsm_control(r))
    }
    pub fn set_lsm_enable(self, r: &RegisterBlock, on: bool) {
        dispatch!(self, set_lsm_enable(r, on))
    }
    pub fn set_dibit_dma(self, r: &RegisterBlock, on: bool) {
        dispatch!(self, set_dibit_dma(r, on))
    }
    pub fn set_dc_block(self, r: &RegisterBlock, on: bool) {
        dispatch!(self, set_dc_block(r, on))
    }
    pub fn set_agc(self, r: &RegisterBlock, on: bool) {
        dispatch!(self, set_agc(r, on))
    }
    pub fn pulse_reset(self, r: &RegisterBlock) {
        dispatch!(self, pulse_reset(r))
    }
    pub fn status(self, r: &RegisterBlock) -> LsmStatus {
        dispatch!(self, status(r))
    }
    pub fn nid(self, r: &RegisterBlock) -> (u16, u8) {
        dispatch!(self, nid(r))
    }
    pub fn dibit_last_buffer(self, r: &RegisterBlock) -> u8 {
        dispatch!(self, dibit_last_buffer(r))
    }
    pub fn dibit_next(self, r: &RegisterBlock) -> u32 {
        dispatch!(self, dibit_next(r))
    }
    pub fn debug(self, r: &RegisterBlock) -> (i16, i16) {
        dispatch!(self, debug(r))
    }
    pub fn agc_debug(self, r: &RegisterBlock) -> (u16, u16) {
        dispatch!(self, agc_debug(r))
    }
}

/// NCO width of every DDC.
const NCO_BITS: u32 = 28;

/// The 28-bit phase increment of an offset from the LO.
pub fn freq_to_nco(offset_hz: f64, sample_rate_hz: f64) -> u32 {
    let scale = (1u64 << NCO_BITS) as f64;
    ((offset_hz / sample_rate_hz) * scale).round() as i32 as u32
}

/// The offset an NCO word stands for (28-bit two's complement).
pub fn nco_to_freq(word: u32, sample_rate_hz: f64) -> f64 {
    let signed = ((word << 4) as i32) >> 4;
    signed as f64 * sample_rate_hz / (1u64 << NCO_BITS) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nco_words_round_trip() {
        for (hz, rate) in [(2_862_500.0, 8e6), (-2_862_500.0, 8e6), (0.0, 12e6), (5_900_000.0, 12e6)] {
            let word = freq_to_nco(hz, rate);
            assert!((nco_to_freq(word & 0x0FFF_FFFF, rate) - hz).abs() < 0.05, "{hz} at {rate}");
        }
        // p25-httpd's traffic word for 860.9625 MHz with the LO at 858.1 MHz, 8 MSPS.
        let word = freq_to_nco(2_862_500.0, 8e6);
        assert!(word > 0x05B0_0000 && word < 0x05C0_0000);
    }

    #[test]
    fn each_bank_writes_its_own_registers() {
        let regs = Registers::in_memory();
        Bank::Traffic.set_nco(&regs, 0x0123_4567);
        assert_eq!(Bank::Traffic.nco(&regs), 0x0123_4567);
        assert_eq!(Bank::Control.nco(&regs), 0);
        assert_eq!(Bank::Traffic2.nco(&regs), 0);
        Bank::Traffic2.set_lsm_enable(&regs, true);
        assert!(Bank::Traffic2.lsm_control(&regs).enable);
        assert!(!Bank::Traffic.lsm_control(&regs).enable);
    }
}

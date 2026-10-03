//! Register access to the radio core, and its lane banks: each drives one DDC and the lane's
//! packets (enable, tag). The banks have the same fields under `laneN_` register names;
//! `lane_bank!` generates one accessor module per bank and `LaneBank` dispatches to them.

use std::any::Any;
use std::ops::Deref;
use std::sync::Arc;

pub use core_pac::radio_core::RegisterBlock;

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

macro_rules! lane_bank {
    ($name:ident) => { paste::paste! {
        pub mod $name {
            use super::{RegisterBlock, StageFields};

            pub fn write_coeff(r: &RegisterBlock, addr: u16, data: i32) {
                r.[<$name _ddc_coeff_addr>]().modify(|_, w| unsafe { w.coeff_waddr().bits(addr) });
                r.[<$name _ddc_coeff>]().modify(|_, w| unsafe {
                    w.coeff_wren().bit(true).coeff_wdata().bits(data as u32)
                });
            }

            /// Decimation and operation counts of the three stages, every stage enabled.
            pub fn set_stages(r: &RegisterBlock, s: &[StageFields; 3]) {
                r.[<$name _ddc_decimation>]().modify(|_, w| unsafe {
                    w.decimation1().bits(s[0].decimation)
                        .decimation2().bits(s[1].decimation)
                        .decimation3().bits(s[2].decimation)
                });
                r.[<$name _ddc_control>]().modify(|_, w| unsafe {
                    w.operations_minus_one1().bits(s[0].operations_minus_one)
                        .odd_operations1().bit(s[0].odd_operations)
                        .operations_minus_one2().bits(s[1].operations_minus_one)
                        .operations_minus_one3().bits(s[2].operations_minus_one)
                        .odd_operations3().bit(s[2].odd_operations)
                        .bypass2().clear_bit()
                        .bypass3().clear_bit()
                });
            }

            pub fn set_nco(r: &RegisterBlock, word: u32) {
                r.[<$name _ddc_frequency>]().modify(|_, w| unsafe { w.frequency().bits(word) });
            }

            pub fn nco(r: &RegisterBlock) -> u32 {
                r.[<$name _ddc_frequency>]().read().frequency().bits()
            }

            pub fn set_input(r: &RegisterBlock, on: bool) {
                r.[<$name _ddc_control>]().modify(|_, w| w.enable_input().bit(on));
            }

            /// The lane's packets on or off, and the tag they carry, in one write.
            pub fn set_control(r: &RegisterBlock, enable: bool, tag: u16) {
                r.[<$name _control>]().modify(|_, w| unsafe { w.enable().bit(enable).tag().bits(tag) });
            }

            pub fn control(r: &RegisterBlock) -> (bool, u16) {
                let c = r.[<$name _control>]().read();
                (c.enable().bit(), c.tag().bits())
            }
        }
    }};
}

lane_bank!(lane0);
lane_bank!(lane1);
lane_bank!(lane2);

/// The lane banks the register map has.
pub const LANE_BANKS: usize = 3;

/// One lane's register bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneBank(pub usize);

macro_rules! dispatch {
    ($bank:expr, $f:ident($($arg:expr),*)) => {
        match $bank.0 {
            0 => lane0::$f($($arg),*),
            1 => lane1::$f($($arg),*),
            2 => lane2::$f($($arg),*),
            n => unreachable!("lane bank {n}"),
        }
    };
}

impl LaneBank {
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
    pub fn set_control(self, r: &RegisterBlock, enable: bool, tag: u16) {
        dispatch!(self, set_control(r, enable, tag))
    }
    pub fn control(self, r: &RegisterBlock) -> (bool, u16) {
        dispatch!(self, control(r))
    }
}

/// NCO width of every DDC.
const NCO_BITS: u32 = 28;

/// The 28-bit phase increment of an offset from the LO.
pub fn freq_to_nco(offset_hz: f64, sample_rate_hz: f64) -> u32 {
    let scale = (1u64 << NCO_BITS) as f64;
    ((offset_hz / sample_rate_hz) * scale).round() as i32 as u32 & 0x0FFF_FFFF
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
            assert!((nco_to_freq(word, rate) - hz).abs() < 0.05, "{hz} at {rate}");
        }
        // The traffic word for 860.9625 MHz with the LO at 858.1 MHz, 8 MSPS.
        let word = freq_to_nco(2_862_500.0, 8e6);
        assert!(word > 0x05B0_0000 && word < 0x05C0_0000);
    }

    #[test]
    fn each_bank_writes_its_own_registers() {
        let regs = Registers::in_memory();
        LaneBank(1).set_nco(&regs, 0x0123_4567);
        assert_eq!(LaneBank(1).nco(&regs), 0x0123_4567);
        assert_eq!(LaneBank(0).nco(&regs), 0);
        assert_eq!(LaneBank(2).nco(&regs), 0);
        LaneBank(2).set_control(&regs, true, 0xBEEF);
        assert_eq!(LaneBank(2).control(&regs), (true, 0xBEEF));
        assert_eq!(LaneBank(1).control(&regs), (false, 0));
    }
}

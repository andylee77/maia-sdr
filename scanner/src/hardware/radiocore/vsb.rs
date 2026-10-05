//! The radio core's 8-VSB block (maia-sdr doc/changes/085, "Registers"): its table, step and
//! carrier written before it starts, its taps loaded into the idle set and armed for the next
//! field, a snapshot asked for, its status. Its packets come through the lane ring.

use serde::Serialize;

use super::RadioCore;
use crate::protocol::atsc::live::MF_DSPS;

/// The block's registers in one reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct VsbStatus {
    pub enabled: bool,
    /// The segment syncs are found and counted.
    pub segments: bool,
    /// The field syncs are counted.
    pub fields: bool,
    /// The timing loop is past its acquisition gear.
    pub tracking: bool,
    /// A tap set is armed and not yet loaded.
    pub taps_armed: bool,
    pub snapshot_pending: bool,
    /// The matched filter fell behind its input (it never should): restart the block.
    pub overrun: bool,
    pub taps_loaded: u16,
    /// Fields counted since the enable.
    pub fields_counted: u32,
    /// Codewords and snapshot values the packet buffers could not take.
    pub lost: u32,
}

impl RadioCore {
    /// The core has the 8-VSB block.
    pub fn vsb_present(&self) -> bool {
        self.identity.vsb
    }

    /// Hold the block in reset.
    pub fn vsb_disable(&self) {
        self.regs.vsb_control().modify(|_, w| w.enable().clear_bit());
    }

    /// Start the block afresh: the matched filter's table (`live::table_words`), the nominal
    /// symbol step and the carrier loop's starting frequency, then the enable.
    pub fn vsb_start(&self, table: &[[u16; MF_DSPS]], step: u32, carrier: i32) {
        self.vsb_disable();
        for (addr, w) in table.iter().enumerate() {
            self.regs.vsb_mf_taps01().modify(|_, r| unsafe { r.tap0().bits(w[0]).tap1().bits(w[1]) });
            self.regs.vsb_mf_taps23().modify(|_, r| unsafe { r.tap2().bits(w[2]).tap3().bits(w[3]) });
            self.regs.vsb_mf_tap4().modify(|_, r| unsafe { r.tap4().bits(w[4]).addr().bits(addr as u16).write().set_bit() });
        }
        self.regs.vsb_step().modify(|_, w| unsafe { w.step().bits(step) });
        self.regs.vsb_carrier().modify(|_, w| unsafe { w.frequency().bits(carrier as u32) });
        self.regs.vsb_control().modify(|_, w| w.enable().set_bit());
    }

    /// Write a tap set into the idle set and arm it: it takes effect at the next field's first
    /// symbol. Refused while a set is armed.
    pub fn vsb_load_taps(&self, taps: &[i32]) -> bool {
        if self.regs.vsb_status().read().taps_armed().bit() {
            return false;
        }
        for (i, &t) in taps.iter().enumerate().take(128) {
            self.regs.vsb_eq_tap().modify(|_, w| unsafe { w.tap().bits(t as u16).index().bits(i as u8).write().set_bit() });
        }
        self.regs.vsb_control().modify(|_, w| w.taps_arm().set_bit());
        true
    }

    /// Take the next field's snapshot.
    pub fn vsb_snapshot(&self) {
        self.regs.vsb_control().modify(|_, w| w.snapshot().set_bit());
    }

    pub fn vsb_status(&self) -> VsbStatus {
        let s = self.regs.vsb_status().read();
        VsbStatus {
            enabled: self.regs.vsb_control().read().enable().bit(),
            segments: s.segments().bit(),
            fields: s.fields().bit(),
            tracking: s.tracking().bit(),
            taps_armed: s.taps_armed().bit(),
            snapshot_pending: s.snapshot_pending().bit(),
            overrun: s.overrun().bit(),
            taps_loaded: self.regs.vsb_taps_loaded().read().count().bits(),
            fields_counted: self.regs.vsb_fields().read().count().bits(),
            lost: self.regs.vsb_lost().read().count().bits(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(not(target_os = "linux"))]
    fn a_start_writes_the_table_step_and_carrier_then_enables() {
        let core = RadioCore::in_memory(3);
        let table = vec![[0x123, 0x456, 0x789, 0xABC, 0xDEF]; 512];
        core.vsb_start(&table, 3_990_772_585, -1000);
        let s = core.vsb_status();
        assert!(s.enabled);
        assert_eq!(core.regs.vsb_step().read().step().bits(), 3_990_772_585);
        assert_eq!(core.regs.vsb_carrier().read().frequency().bits(), (-1000i32) as u32);
        // The last word written stays in the staging registers (memory holds no pulses).
        let t4 = core.regs.vsb_mf_tap4().read();
        assert_eq!((t4.tap4().bits(), t4.addr().bits()), (0xDEF, 511));
        assert!(core.vsb_load_taps(&[7; 128]));
        assert_eq!(core.regs.vsb_eq_tap().read().index().bits(), 127);
        core.vsb_disable();
        assert!(!core.vsb_status().enabled);
    }
}

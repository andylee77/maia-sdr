//! Allow-listed register access with the design-doc safety rules:
//!
//! * only registers in the core's map (rule 5);
//! * never vacant / aliased offsets (F15);
//! * read-to-clear registers only when explicitly allowed;
//! * reset-gated banks (P25 `sync` banks while `control.sdr_reset = 1`)
//!   are refused instead of hanging the AXI bus;
//! * hwval snapshot protocol (section 6.3): write the domain mask to
//!   SNAP_REQ, poll SNAP_ACK == mask for up to 10 ms, report dead domains.

use crate::err::{AResult, AgentError, Code};
use crate::regio::RegIo;
use crate::regmap::{Core, RegDef};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub const SNAP_TIMEOUT: Duration = Duration::from_millis(10);

#[derive(Debug, Clone)]
pub struct SnapResult {
    pub mask: u32,
    pub ack: u32,
    pub dead: Vec<String>,
    pub elapsed_us: u64,
}

impl SnapResult {
    pub fn to_json(&self) -> Value {
        json!({
            "mask": self.mask,
            "ack": self.ack,
            "ok": self.dead.is_empty(),
            "dead_domains": self.dead,
            "elapsed_us": self.elapsed_us,
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOpts {
    /// Allow registers marked `dangerous` (core resets).
    pub force_dangerous: bool,
    /// Caller has passed the TX interlock (`--tx-ok`) or is a TX-safing path.
    pub tx_ok: bool,
}

pub struct RegAccess<'c, IO: RegIo> {
    pub core: &'c Core,
    pub io: IO,
    snap_valid: u32,
    pub snaps: Vec<SnapResult>,
    pub dead_mask: u32,
    gate_cache: Option<bool>,
}

impl<'c, IO: RegIo> RegAccess<'c, IO> {
    pub fn new(core: &'c Core, io: IO) -> Self {
        RegAccess {
            core,
            io,
            snap_valid: 0,
            snaps: Vec::new(),
            dead_mask: 0,
            gate_cache: None,
        }
    }

    /// Forget cached snapshot/gate state (call once per poll in loops).
    pub fn refresh(&mut self) {
        self.snap_valid = 0;
        self.gate_cache = None;
    }

    pub fn domain_bit(&self, domain: &str) -> Option<u32> {
        self.core.snapshot_domains.get(domain).copied()
    }

    fn mask_names(&self, mask: u32) -> Vec<String> {
        self.core
            .snapshot_domains
            .iter()
            .filter(|(_, b)| mask & **b != 0)
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Reads the reset-gate bit (P25 `control.sdr_reset`). The gate
    /// register must itself live in an ungated domain.
    pub fn gate_asserted(&mut self) -> AResult<bool> {
        let gate = match &self.core.reset_gate {
            None => return Ok(false),
            Some(g) => g.clone(),
        };
        if let Some(v) = self.gate_cache {
            return Ok(v);
        }
        let reg = self.core.get(&gate.reg)?.clone();
        if let Some(d) = &reg.domain {
            if gate.domains.contains(d) {
                return Err(AgentError::new(
                    Code::Error,
                    "map error: reset-gate register is in a gated domain",
                ));
            }
        }
        if let Some(why) = self.core.offset_forbidden(reg.offset) {
            return Err(AgentError::new(Code::Safety, why));
        }
        let v = self.io.read32(reg.offset);
        let asserted = (v >> gate.bit) & 1 == 1;
        self.gate_cache = Some(asserted);
        Ok(asserted)
    }

    fn check_gate(&mut self, reg: &RegDef) -> AResult<()> {
        let gate = match &self.core.reset_gate {
            None => return Ok(()),
            Some(g) => g.clone(),
        };
        let dom = match &reg.domain {
            Some(d) => d.clone(),
            None => return Ok(()),
        };
        if gate.domains.contains(&dom) && self.gate_asserted()? {
            return Err(AgentError::new(
                Code::Safety,
                format!(
                    "{}.{} is in the '{}' domain, which is held in reset ({}.bit{} = 1); accessing it would hang the AXI bus. Release the reset first (p25-httpd does this at start, or `ring ... --release-reset`).",
                    self.core.name, reg.name, dom, gate.reg, gate.bit
                ),
            ));
        }
        Ok(())
    }

    /// Runs the snapshot protocol for `mask`.
    pub fn snapshot(&mut self, mask: u32) -> AResult<SnapResult> {
        let req = self.core.get("SNAP_REQ")?.offset;
        let ackr = self.core.get("SNAP_ACK")?.offset;
        let t0 = Instant::now();
        self.io.write32(req, mask);
        let mut ack;
        loop {
            ack = self.io.read32(ackr);
            if ack == mask || t0.elapsed() >= SNAP_TIMEOUT {
                break;
            }
            std::hint::spin_loop();
        }
        let dead_bits = mask & !ack;
        let res = SnapResult {
            mask,
            ack,
            dead: self.mask_names(dead_bits),
            elapsed_us: t0.elapsed().as_micros() as u64,
        };
        self.dead_mask |= dead_bits;
        self.snap_valid |= mask;
        self.snaps.push(res.clone());
        Ok(res)
    }

    /// Snapshots every domain needed by `regs` that is not yet valid.
    pub fn snapshot_for<'a>(&mut self, regs: impl IntoIterator<Item = &'a RegDef>) -> AResult<Option<SnapResult>> {
        let mut mask = 0;
        for r in regs {
            if let Some(d) = &r.snapshot {
                if let Some(b) = self.domain_bit(d) {
                    mask |= b;
                }
            }
        }
        let need = mask & !self.snap_valid;
        if need == 0 {
            return Ok(None);
        }
        self.snapshot(need).map(Some)
    }

    pub fn check_read(&mut self, reg: &RegDef, allow_side_effect: bool) -> AResult<()> {
        if let Some(why) = self.core.offset_forbidden(reg.offset) {
            return Err(AgentError::new(Code::Safety, why));
        }
        if !reg.access.readable() {
            return Err(AgentError::new(
                Code::Safety,
                format!("{}.{} is write-only", self.core.name, reg.name),
            ));
        }
        if reg.read_side_effect && !allow_side_effect {
            return Err(AgentError::new(
                Code::Safety,
                format!(
                    "{}.{} (0x{:03X}) is read-to-clear; reading it would clear sticky status other software relies on. Pass --force-side-effects to read it anyway.",
                    self.core.name, reg.name, reg.offset
                ),
            ));
        }
        self.check_gate(reg)
    }

    pub fn read_reg(&mut self, reg: &RegDef, allow_side_effect: bool) -> AResult<u32> {
        self.check_read(reg, allow_side_effect)?;
        self.snapshot_for(std::iter::once(reg))?;
        Ok(self.io.read32(reg.offset))
    }

    /// User-level read by name/offset (read-to-clear refused).
    pub fn read(&mut self, name: &str) -> AResult<u32> {
        let reg = self.core.get(name)?.clone();
        self.read_reg(&reg, false)
    }

    /// Trusted internal read that may have side effects (ring readers own
    /// the sticky status they consume).
    pub fn read_se(&mut self, name: &str) -> AResult<u32> {
        let reg = self.core.get(name)?.clone();
        self.read_reg(&reg, true)
    }

    pub fn try_read(&mut self, name: &str) -> Option<u32> {
        let reg = self.core.find(name)?.clone();
        self.read_reg(&reg, false).ok()
    }

    pub fn check_write(&mut self, reg: &RegDef, value: u64, opts: WriteOpts) -> AResult<u32> {
        if let Some(why) = self.core.offset_forbidden(reg.offset) {
            return Err(AgentError::new(Code::Safety, why));
        }
        self.core
            .write_allowed(reg)
            .map_err(|m| AgentError::new(Code::Safety, m))?;
        if reg.dangerous && !opts.force_dangerous {
            return Err(AgentError::new(
                Code::Safety,
                format!("{}.{} is marked dangerous (core reset); pass --force to write it", self.core.name, reg.name),
            ));
        }
        if reg.tx_affecting && !opts.tx_ok {
            return Err(AgentError::new(
                Code::Safety,
                format!(
                    "{}.{} changes the TX output; the host must pass the TX interlock and add --tx-ok (or use `tx off`)",
                    self.core.name, reg.name
                ),
            ));
        }
        if value > u32::MAX as u64 {
            return Err(AgentError::new(Code::Usage, format!("value 0x{value:X} does not fit 32 bits")));
        }
        if reg.width < 32 && value >> reg.width != 0 {
            return Err(AgentError::new(
                Code::Usage,
                format!("value 0x{value:X} does not fit the {}-bit register {}", reg.width, reg.name),
            ));
        }
        self.check_gate(reg)?;
        Ok(value as u32)
    }

    pub fn write_reg(&mut self, reg: &RegDef, value: u64, opts: WriteOpts) -> AResult<()> {
        let v = self.check_write(reg, value, opts)?;
        self.io.write32(reg.offset, v);
        Ok(())
    }

    /// Internal write by name (TX-safe callers only pass tx_ok when they
    /// are the TX-safing path or behind the TX guard).
    pub fn write_opts(&mut self, name: &str, value: u32, opts: WriteOpts) -> AResult<()> {
        let reg = self.core.get(name)?.clone();
        self.write_reg(&reg, value as u64, opts)
    }

    pub fn write(&mut self, name: &str, value: u32) -> AResult<()> {
        self.write_opts(name, value, WriteOpts::default())
    }

    /// Read-modify-write of a readable, writable register. Returns the old value.
    pub fn modify(&mut self, name: &str, opts: WriteOpts, f: impl FnOnce(u32) -> u32) -> AResult<u32> {
        let reg = self.core.get(name)?.clone();
        let old = self.read_reg(&reg, false)?;
        self.write_reg(&reg, f(old) as u64, opts)?;
        Ok(old)
    }

    pub fn snapshot_json(&self) -> Value {
        Value::Array(self.snaps.iter().map(|s| s.to_json()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regio::MockIo;
    use crate::regmap::{cores_from_file_value, MapSet};

    fn hwval() -> Core {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/hwval_regs.json")).unwrap();
        cores_from_file_value(&v, "fixture").0.remove(0)
    }

    #[test]
    fn p25_rules() {
        let set = MapSet::builtin();
        let p25 = set.core("p25").unwrap();
        let mut io = MockIo::new();
        io.regs.insert(0x0, 0x7032_3566);
        io.regs.insert(0x8, 1); // sdr_reset = 1
        io.regs.insert(0xE0, 0x1F);
        let mut a = RegAccess::new(p25, &mut io);
        assert_eq!(a.read("product_id").unwrap(), 0x7032_3566);
        // Read-to-clear refused by default.
        let e = a.read("wideband_iq_dma_status").unwrap_err();
        assert_eq!(e.code, Code::Safety);
        // Sync bank refused while in reset, even for trusted reads.
        let e = a.read_se("wideband_iq_dma_status").unwrap_err();
        assert!(e.msg.contains("reset"), "{}", e.msg);
        let e = a.read("wideband_iq_next_address").unwrap_err();
        assert_eq!(e.code, Code::Safety);
        // Vacant offsets are not even resolvable.
        assert!(a.core.find("0x120").is_none());
        // Release the reset -> allowed.
        a.write("control", 0).unwrap();
        a.refresh();
        assert_eq!(a.read_se("wideband_iq_dma_status").unwrap(), 0x1F);
        assert!(a.read("wideband_iq_next_address").is_ok());
        // Read-only register writes are refused.
        assert_eq!(a.write("product_id", 1).unwrap_err().code, Code::Safety);
        drop(a);
        // The mock never saw an access in a vacant range.
        assert!(io.reads.iter().all(|o| p25.offset_forbidden(*o).is_none()));
    }

    #[test]
    fn snapshot_protocol_alive_and_dead() {
        let core = hwval();
        let req = core.get("SNAP_REQ").unwrap().offset;
        let ack = core.get("SNAP_ACK").unwrap().offset;
        let ts = core.get("TS_LO").unwrap().offset;
        // Hardware model: sampling domain (bit 2) is dead.
        let mut io = MockIo::new();
        io.regs.insert(ts, 1234);
        io.on_write = Some(Box::new(move |regs, off, v| {
            if off == req {
                regs.insert(ack, v & 0b011);
            } else {
                regs.insert(off, v);
            }
        }));
        let mut a = RegAccess::new(&core, &mut io);
        assert_eq!(a.read("TS_LO").unwrap(), 1234);
        assert!(a.snaps[0].dead.is_empty());
        assert_eq!(a.snaps[0].mask, 1);
        // Second read of the same domain reuses the snapshot.
        a.read("TS_HI").unwrap();
        assert_eq!(a.snaps.len(), 1);
        let r = a.snapshot(0b111).unwrap();
        assert_eq!(r.dead, vec!["sampling".to_string()]);
        assert!(r.elapsed_us >= 9_000, "timeout honoured: {}", r.elapsed_us);
        assert_eq!(a.dead_mask, 0b100);
    }

    #[test]
    fn tx_affecting_and_dangerous() {
        let set = MapSet::builtin();
        let dac = set.core("adi_dac").unwrap();
        let mut io = MockIo::new();
        let mut a = RegAccess::new(dac, &mut io);
        assert_eq!(a.write("DAC_CHAN0_CNTRL_7", 9).unwrap_err().code, Code::Safety);
        a.write_opts("DAC_CHAN0_CNTRL_7", 3, WriteOpts { tx_ok: true, ..Default::default() })
            .unwrap();
        assert_eq!(a.write("DAC_RSTN", 0).unwrap_err().code, Code::Safety);
        let slcr = set.core("slcr").unwrap();
        let mut io2 = MockIo::new();
        let mut b = RegAccess::new(slcr, &mut io2);
        assert_eq!(b.write("DDR_PLL_CTRL", 0).unwrap_err().code, Code::Safety);
        assert_eq!(io2.writes.len(), 0);
    }
}

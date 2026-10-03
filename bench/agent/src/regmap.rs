//! Register allow-lists (`fbench.regmap/1`).
//!
//! Built-in maps are embedded at compile time (`maps/*.json` plus the P25
//! map generated from the SVD by build.rs). Files in the share directory
//! (`/mnt/sd/bench/share/*.json`) override a built-in core of the same name,
//! but never remove built-in safety annotations (read side effects, clock
//! domains, reset gate, vacant ranges, read-only cores): those are merged
//! back in as a floor.
//!
//! Accepted file shapes: one map object, an array of map objects, or an
//! object with a `cores` / `maps` array. Offsets may be JSON numbers or hex
//! strings. Register offsets are absolute within the core; an offset
//! smaller than its block offset is taken as block-relative.

use crate::err::{AResult, AgentError, Code};
use crate::util::parse_u64;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Ro,
    Rw,
    Wo,
    W1c,
}

impl Access {
    pub fn readable(self) -> bool {
        !matches!(self, Access::Wo)
    }
    pub fn writable(self) -> bool {
        !matches!(self, Access::Ro)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Ro => "ro",
            Access::Rw => "rw",
            Access::Wo => "wo",
            Access::W1c => "w1c",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub lsb: u32,
    pub width: u32,
}

impl Field {
    pub fn extract(&self, v: u32) -> u32 {
        let w = self.width.min(32);
        let mask = if w >= 32 { u32::MAX } else { (1u32 << w) - 1 };
        (v >> self.lsb.min(31)) & mask
    }
}

#[derive(Debug, Clone)]
pub struct RegDef {
    pub name: String,
    pub block: String,
    pub offset: u32,
    pub access: Access,
    pub width: u32,
    pub reset: Option<u64>,
    pub snapshot: Option<String>,
    pub desc: String,
    pub fields: Vec<Field>,
    pub read_side_effect: bool,
    pub domain: Option<String>,
    pub dangerous: bool,
    pub tx_affecting: bool,
}

impl RegDef {
    pub fn decode(&self, v: u32) -> Value {
        let mut m = serde_json::Map::new();
        for f in &self.fields {
            m.insert(f.name.clone(), json!(f.extract(v)));
        }
        Value::Object(m)
    }

    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }
}

#[derive(Debug, Clone)]
pub struct ResetGate {
    pub reg: String,
    pub bit: u32,
    pub domains: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Core {
    pub name: String,
    pub base: u64,
    pub size: u64,
    pub id_reg: Option<String>,
    pub id_value: Option<u64>,
    pub version: Option<String>,
    pub snapshot_domains: BTreeMap<String, u32>,
    pub regs: Vec<RegDef>,
    pub requires_uio: Option<String>,
    pub requires_iio: Option<String>,
    pub reset_gate: Option<ResetGate>,
    pub vacant: Vec<(u32, u32)>,
    pub decode_limit: Option<u32>,
    pub readonly: bool,
    pub writable: Option<Vec<String>>,
    pub source: String,
}

impl Core {
    /// Looks a register up by name (case-insensitive) or by offset
    /// (`0x0E0`); an offset only matches if a register is mapped there.
    pub fn find(&self, spec: &str) -> Option<&RegDef> {
        if let Some(r) = self.regs.iter().find(|r| r.name.eq_ignore_ascii_case(spec)) {
            return Some(r);
        }
        let s = spec.trim();
        if s.starts_with("0x") || s.starts_with("0X") || s.chars().all(|c| c.is_ascii_digit()) {
            if let Some(off) = parse_u64(s) {
                return self.regs.iter().find(|r| r.offset as u64 == off);
            }
        }
        None
    }

    pub fn get(&self, spec: &str) -> AResult<&RegDef> {
        self.find(spec).ok_or_else(|| {
            AgentError::new(
                Code::Safety,
                format!(
                    "register '{spec}' is not in the '{}' allow-list (safety rule 5)",
                    self.name
                ),
            )
        })
    }

    pub fn has(&self, name: &str) -> bool {
        self.find(name).is_some()
    }

    /// Returns why an offset must never be touched, if any.
    pub fn offset_forbidden(&self, off: u32) -> Option<String> {
        if off % 4 != 0 {
            return Some(format!("offset 0x{off:X} is not word aligned"));
        }
        if off as u64 + 4 > self.size {
            return Some(format!("offset 0x{off:X} is outside the core window (size 0x{:X})", self.size));
        }
        if let Some(lim) = self.decode_limit {
            if off >= lim {
                return Some(format!(
                    "offset 0x{off:X} aliases above the decoded range 0x{lim:X} of '{}'",
                    self.name
                ));
            }
        }
        for (lo, hi) in &self.vacant {
            if off >= *lo && off < *hi {
                return Some(format!(
                    "offset 0x{off:X} is in a vacant bank [0x{lo:X}, 0x{hi:X}) of '{}' (reading it hangs the AXI bus)",
                    self.name
                ));
            }
        }
        None
    }

    pub fn write_allowed(&self, r: &RegDef) -> Result<(), String> {
        if let Some(list) = &self.writable {
            if !list.iter().any(|n| n.eq_ignore_ascii_case(&r.name)) {
                return Err(format!(
                    "core '{}' only allows writes to {:?}",
                    self.name, list
                ));
            }
            return Ok(());
        }
        if self.readonly {
            return Err(format!("core '{}' is read-only for the agent", self.name));
        }
        if !r.access.writable() {
            return Err(format!("register {} is read-only", r.name));
        }
        Ok(())
    }

    pub fn summary(&self) -> Value {
        json!({
            "core": self.name,
            "base": format!("0x{:08X}", self.base),
            "size": self.size,
            "regs": self.regs.len(),
            "source": self.source,
            "readonly": self.readonly,
            "snapshot_domains": self.snapshot_domains,
            "requires": {"uio": self.requires_uio, "iio": self.requires_iio},
        })
    }
}

// ── parsing ──────────────────────────────────────────────────────────

fn hexval(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => parse_u64(s),
        _ => None,
    }
}

fn get_hex(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(hexval)
}

fn get_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn get_bool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn parse_access(s: &str) -> (Access, bool) {
    match s.trim().to_ascii_lowercase().as_str() {
        "ro" | "r" | "read-only" | "readonly" => (Access::Ro, false),
        "rc" | "r2c" | "rsticky" | "read-clear" | "roc" => (Access::Ro, true),
        "wo" | "w" | "write-only" | "wpulse" | "pulse" => (Access::Wo, false),
        "w1c" | "rw1c" | "write-1-to-clear" => (Access::W1c, false),
        _ => (Access::Rw, false),
    }
}

fn parse_fields(v: Option<&Value>) -> Vec<Field> {
    let mut out = Vec::new();
    if let Some(Value::Array(fs)) = v {
        for f in fs {
            let name = match get_str(f, "name") {
                Some(n) => n,
                None => continue,
            };
            let (lsb, width) = if let Some(br) = get_str(f, "bitRange") {
                let inner = br.trim_start_matches('[').trim_end_matches(']').to_string();
                let mut it = inner.split(':');
                let msb = it.next().and_then(parse_u64).unwrap_or(0) as u32;
                let lsb = it.next().and_then(parse_u64).unwrap_or(msb as u64) as u32;
                (lsb, msb.saturating_sub(lsb) + 1)
            } else {
                (
                    get_hex(f, "lsb").unwrap_or(0) as u32,
                    get_hex(f, "width").unwrap_or(1) as u32,
                )
            };
            out.push(Field { name, lsb, width });
        }
    }
    out
}

fn parse_reg(r: &Value, block: &str, block_off: u32, block_domain: Option<&str>) -> Result<RegDef, String> {
    let name = get_str(r, "name").ok_or("register without name")?;
    let mut off = get_hex(r, "offset")
        .or_else(|| get_hex(r, "addressOffset"))
        .ok_or(format!("register {name} without offset"))? as u32;
    if block_off > 0 && off < block_off {
        off += block_off;
    }
    let (access, rse_from_access) = parse_access(&get_str(r, "access").unwrap_or_else(|| "ro".into()));
    Ok(RegDef {
        desc: get_str(r, "desc")
            .or_else(|| get_str(r, "description"))
            .unwrap_or_else(|| name.clone()),
        name,
        block: block.to_string(),
        offset: off,
        access,
        width: get_hex(r, "width").unwrap_or(32) as u32,
        reset: get_hex(r, "reset"),
        snapshot: get_str(r, "snapshot").filter(|s| !s.is_empty()),
        fields: parse_fields(r.get("fields")),
        read_side_effect: rse_from_access || get_bool(r, "read_side_effect"),
        domain: get_str(r, "domain").or_else(|| block_domain.map(|s| s.to_string())),
        dangerous: get_bool(r, "dangerous"),
        tx_affecting: get_bool(r, "tx_affecting"),
    })
}

pub fn core_from_value(v: &Value, source: &str) -> Result<Core, String> {
    let name = get_str(v, "core").ok_or("map without 'core'")?;
    let base = get_hex(v, "base").ok_or(format!("map {name} without 'base'"))?;
    let size = get_hex(v, "size").unwrap_or(4096);
    let mut regs = Vec::new();
    if let Some(Value::Array(blocks)) = v.get("blocks") {
        for b in blocks {
            let bname = get_str(b, "name").unwrap_or_default();
            let boff = get_hex(b, "offset").unwrap_or(0) as u32;
            let bdom = get_str(b, "domain");
            if let Some(Value::Array(rs)) = b.get("regs") {
                for r in rs {
                    regs.push(parse_reg(r, &bname, boff, bdom.as_deref())?);
                }
            }
        }
    }
    if let Some(Value::Array(rs)) = v.get("regs") {
        for r in rs {
            regs.push(parse_reg(r, "", 0, None)?);
        }
    }
    let mut snapshot_domains = BTreeMap::new();
    if let Some(Value::Object(m)) = v.get("snapshot_domains") {
        for (k, x) in m {
            if let Some(n) = hexval(x) {
                snapshot_domains.insert(k.clone(), n as u32);
            }
        }
    }
    let reset_gate = v.get("reset_gate").and_then(|g| {
        Some(ResetGate {
            reg: get_str(g, "reg")?,
            bit: get_hex(g, "bit").unwrap_or(0) as u32,
            domains: g
                .get("domains")
                .and_then(|d| d.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
        })
    });
    let mut vacant = Vec::new();
    if let Some(Value::Array(vs)) = v.get("vacant") {
        for pair in vs {
            if let Some(a) = pair.as_array() {
                if a.len() == 2 {
                    if let (Some(lo), Some(hi)) = (hexval(&a[0]), hexval(&a[1])) {
                        vacant.push((lo as u32, hi as u32));
                    }
                }
            }
        }
    }
    let requires = v.get("requires");
    Ok(Core {
        name,
        base,
        size,
        id_reg: get_str(v, "id_reg"),
        id_value: get_hex(v, "id_value"),
        version: get_str(v, "version"),
        snapshot_domains,
        regs,
        requires_uio: requires.and_then(|r| get_str(r, "uio")),
        requires_iio: requires.and_then(|r| get_str(r, "iio")),
        reset_gate,
        vacant,
        decode_limit: get_hex(v, "decode_limit").map(|x| x as u32),
        readonly: get_bool(v, "readonly"),
        writable: v.get("writable").and_then(|w| w.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        }),
        source: source.to_string(),
    })
}

/// Extracts every map object from a parsed file.
pub fn cores_from_file_value(v: &Value, source: &str) -> (Vec<Core>, Vec<String>) {
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    let items: Vec<&Value> = match v {
        Value::Array(a) => a.iter().collect(),
        Value::Object(_) => {
            if let Some(Value::Array(a)) = v.get("cores").or_else(|| v.get("maps")) {
                a.iter().collect()
            } else {
                vec![v]
            }
        }
        _ => vec![],
    };
    for it in items {
        if it.get("core").is_none() {
            continue;
        }
        match core_from_value(it, source) {
            Ok(c) => out.push(c),
            Err(e) => warnings.push(format!("{source}: {e}")),
        }
    }
    (out, warnings)
}

/// Applies the built-in safety floor to a share override.
fn merge_floor(mut share: Core, builtin: &Core, warnings: &mut Vec<String>) -> Core {
    for (lo, hi) in &builtin.vacant {
        if !share.vacant.contains(&(*lo, *hi)) {
            share.vacant.push((*lo, *hi));
        }
    }
    if share.decode_limit.is_none() {
        share.decode_limit = builtin.decode_limit;
    }
    if share.reset_gate.is_none() {
        share.reset_gate = builtin.reset_gate.clone();
    }
    if share.requires_uio.is_none() {
        share.requires_uio = builtin.requires_uio.clone();
    }
    if share.requires_iio.is_none() {
        share.requires_iio = builtin.requires_iio.clone();
    }
    if builtin.readonly {
        share.readonly = true;
    }
    if share.writable.is_none() {
        share.writable = builtin.writable.clone();
    }
    for r in share.regs.iter_mut() {
        if let Some(b) = builtin.regs.iter().find(|b| b.offset == r.offset) {
            r.read_side_effect |= b.read_side_effect;
            r.dangerous |= b.dangerous;
            r.tx_affecting |= b.tx_affecting;
            if r.domain.is_none() {
                r.domain = b.domain.clone();
            }
        }
    }
    let before = share.regs.len();
    let probe = share.clone();
    share.regs.retain(|r| probe.offset_forbidden(r.offset).is_none());
    if share.regs.len() != before {
        warnings.push(format!(
            "{}: dropped {} register(s) at forbidden offsets from the share map",
            share.name,
            before - share.regs.len()
        ));
    }
    share
}

pub const BUILTIN_SOURCES: &[(&str, &str)] = &[
    ("adi_adc", include_str!("../maps/adi_adc.json")),
    ("adi_dac", include_str!("../maps/adi_dac.json")),
    ("rx_dmac", include_str!("../maps/rx_dmac.json")),
    ("tx_dmac", include_str!("../maps/tx_dmac.json")),
    ("slcr", include_str!("../maps/slcr.json")),
    ("ddrc", include_str!("../maps/ddrc.json")),
    ("l2c", include_str!("../maps/l2c.json")),
    ("afi", include_str!("../maps/afi.json")),
    ("p25", include_str!(concat!(env!("OUT_DIR"), "/p25_regs.json"))),
    ("hwval", include_str!("../maps/hwval_min.json")),
];

pub struct MapSet {
    pub cores: BTreeMap<String, Core>,
    pub warnings: Vec<String>,
    pub share_files: Vec<String>,
}

impl MapSet {
    pub fn builtin() -> MapSet {
        let mut cores = BTreeMap::new();
        let mut warnings = Vec::new();
        for (name, text) in BUILTIN_SOURCES {
            match serde_json::from_str::<Value>(text) {
                Ok(v) => match core_from_value(&v, &format!("builtin:{name}")) {
                    Ok(c) => {
                        cores.insert(c.name.clone(), c);
                    }
                    Err(e) => warnings.push(format!("builtin {name}: {e}")),
                },
                Err(e) => warnings.push(format!("builtin {name}: {e}")),
            }
        }
        MapSet {
            cores,
            warnings,
            share_files: Vec::new(),
        }
    }

    /// Built-ins, overridden by every `*.json` map in `share_dir`.
    pub fn load(share_dir: &Path) -> MapSet {
        let mut set = MapSet::builtin();
        let mut files: Vec<PathBuf> = match std::fs::read_dir(share_dir) {
            Ok(rd) => rd
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort();
        for f in files {
            set.load_file(&f);
        }
        set
    }

    pub fn load_file(&mut self, f: &Path) {
        let text = match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                self.warnings.push(format!("{}: {e}", f.display()));
                return;
            }
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                self.warnings.push(format!("{}: {e}", f.display()));
                return;
            }
        };
        let source = format!("share:{}", f.display());
        let (cores, mut w) = cores_from_file_value(&v, &source);
        self.warnings.append(&mut w);
        if !cores.is_empty() {
            self.share_files.push(f.display().to_string());
        }
        for c in cores {
            let merged = match self.cores.get(&c.name) {
                Some(b) if b.source.starts_with("builtin:") => {
                    let b = b.clone();
                    merge_floor(c, &b, &mut self.warnings)
                }
                _ => c,
            };
            self.cores.insert(merged.name.clone(), merged);
        }
    }

    pub fn core(&self, name: &str) -> AResult<&Core> {
        self.cores.get(name).ok_or_else(|| {
            AgentError::new(
                Code::NotFound,
                format!(
                    "no register map for core '{name}' (known: {}); hwval needs share/hwval_regs.json",
                    self.cores.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_parse() {
        let set = MapSet::builtin();
        assert!(set.warnings.is_empty(), "{:?}", set.warnings);
        for n in ["adi_adc", "adi_dac", "rx_dmac", "tx_dmac", "slcr", "ddrc", "l2c", "afi", "p25"] {
            assert!(set.cores.contains_key(n), "missing {n}");
        }
        let p25 = set.core("p25").unwrap();
        assert_eq!(p25.base, 0x7C46_0000);
        assert_eq!(p25.id_value, Some(0x7032_3566));
        let st = p25.get("wideband_iq_dma_status").unwrap();
        assert_eq!(st.offset, 0xE0);
        assert!(st.read_side_effect);
        assert_eq!(st.domain.as_deref(), Some("sync"));
        let ctl = p25.get("control").unwrap();
        assert_eq!(ctl.domain.as_deref(), Some("axi_lite"));
        assert!(!ctl.read_side_effect);
        assert_eq!(ctl.field("sdr_reset").unwrap().lsb, 0);
        for rtc in [0x0Cu32, 0x60, 0x80, 0xA4, 0xC4, 0xE0, 0x144, 0x184, 0x1A0, 0x1C0] {
            let r = p25.regs.iter().find(|r| r.offset == rtc).unwrap();
            assert!(r.read_side_effect, "0x{rtc:X}");
        }
        assert!(p25.offset_forbidden(0x1E0).is_some());
        assert!(p25.offset_forbidden(0x1FC).is_some());
        assert!(p25.offset_forbidden(0x200).is_some());
        assert!(p25.offset_forbidden(0x2E0).is_some());
        assert!(p25.offset_forbidden(0x120).is_none());
        assert!(p25.offset_forbidden(0x180).is_none());
        // No mapped register sits in a forbidden range.
        for r in &p25.regs {
            assert!(p25.offset_forbidden(r.offset).is_none(), "{}", r.name);
        }
        assert!(p25.find("0x1E0").is_none());
        assert_eq!(p25.find("0x120").unwrap().name, "traffic2_ddc_coeff_addr");
        let adc = set.core("adi_adc").unwrap();
        assert_eq!(adc.get("CHAN1_CNTRL_3").unwrap().offset, 0x458);
        assert_eq!(adc.get("IDELAY_6").unwrap().offset, 0x818);
        let dac = set.core("adi_dac").unwrap();
        assert_eq!(dac.get("DAC_CHAN1_CNTRL_7").unwrap().offset, 0x4458);
        assert!(dac.get("DAC_CHAN0_CNTRL_7").unwrap().tx_affecting);
        let slcr = set.core("slcr").unwrap();
        assert!(slcr.readonly);
        assert_eq!(slcr.get("DDRIOB_DCI_STATUS").unwrap().offset, 0xB74);
        let dmac = set.core("rx_dmac").unwrap();
        assert!(dmac.write_allowed(dmac.get("SCRATCH").unwrap()).is_ok());
        assert!(dmac.write_allowed(dmac.get("CONTROL").unwrap()).is_err());
        assert!(dmac.get("PARTIAL_TRANSFER_ID").unwrap().read_side_effect);
    }

    #[test]
    fn hwval_fixture_parses() {
        let text = include_str!("../tests/fixtures/hwval_regs.json");
        let v: Value = serde_json::from_str(text).unwrap();
        let (cores, w) = cores_from_file_value(&v, "fixture");
        assert!(w.is_empty(), "{w:?}");
        let c = &cores[0];
        assert_eq!(c.name, "hwval");
        assert_eq!(c.snapshot_domains.get("mem"), Some(&2));
        let ts = c.get("TS_LO").unwrap();
        assert_eq!(ts.snapshot.as_deref(), Some("sync"));
        assert!(c.get("SNAP_REQ").unwrap().access == Access::Wo);
        assert!(c.get("MT1_CTRL").unwrap().offset >= 0x700);
        assert!(c.get("RINGV2_COMMITTED_BURSTS").unwrap().snapshot.is_none());
    }

    #[test]
    fn share_override_keeps_safety_floor() {
        let mut set = MapSet::builtin();
        let share = json!({
            "schema": "fbench.regmap/1", "core": "p25", "base": "0x7C460000", "size": 4096,
            "regs": [
                {"name": "WB_STATUS", "offset": "0xE0", "access": "ro"},
                {"name": "PRODUCT", "offset": 0, "access": "ro"},
                {"name": "BAD", "offset": "0x1E4", "access": "ro"}
            ]
        });
        let (cores, _) = cores_from_file_value(&share, "test");
        let b = set.cores.get("p25").unwrap().clone();
        let mut w = Vec::new();
        let merged = merge_floor(cores.into_iter().next().unwrap(), &b, &mut w);
        set.cores.insert("p25".into(), merged);
        let p25 = set.core("p25").unwrap();
        let st = p25.get("WB_STATUS").unwrap();
        assert!(st.read_side_effect);
        assert_eq!(st.domain.as_deref(), Some("sync"));
        assert!(p25.find("BAD").is_none());
        assert!(p25.reset_gate.is_some());
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn relative_offsets_in_blocks() {
        let v = json!({"core": "x", "base": 0x1000, "blocks": [
            {"name": "a", "offset": "0x100", "regs": [{"name": "R0", "offset": "0x004"}, {"name": "R1", "offset": "0x108"}]}
        ]});
        let c = core_from_value(&v, "t").unwrap();
        assert_eq!(c.get("R0").unwrap().offset, 0x104);
        assert_eq!(c.get("R1").unwrap().offset, 0x108);
    }
}

//! IIO sysfs / debugfs access and AD9361 helpers.

use crate::err::{AResult, AgentError, Code, Context};
use crate::sys::{self, IioDev};
use crate::util::{parse_u64, read_trim};
use serde_json::{json, Value};
use std::path::PathBuf;

pub const PHY: &str = "ad9361-phy";
pub const ADC: &str = "cf-ad9361-lpc";
pub const DAC: &str = "cf-ad9361-dds-core-lpc";
pub const XADC: &str = "xadc";
/// TX attenuation restored by every safety path (AD9361 maximum).
pub const TX_ATTEN_MAX_DB: f64 = -89.75;

/// Finds an IIO device by name (`ad9361-phy`) or id (`iio:device1`).
pub fn find(dev: &str) -> AResult<IioDev> {
    let devs = sys::iio_devices();
    devs.iter()
        .find(|d| d.name == dev || d.id == dev)
        .cloned()
        .ok_or_else(|| {
            AgentError::new(
                Code::NoDevice,
                format!(
                    "IIO device '{dev}' not found (present: {})",
                    devs.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")
                ),
            )
        })
}

/// Sysfs attribute file name for an optional channel.
pub fn attr_file(chan: Option<&str>, output: bool, attr: &str) -> String {
    match chan {
        None => attr.to_string(),
        Some(c) => format!("{}_{}_{}", if output { "out" } else { "in" }, c, attr),
    }
}

/// Candidate sysfs names for a channel attribute, most specific first. IIO
/// attributes can be shared by all channels of a type (`in_voltage_<attr>`)
/// or by direction (`in_<attr>`); libiio resolves these transparently (e.g.
/// ad9361-phy `voltage0 sampling_frequency` is `in_voltage_sampling_frequency`).
pub fn attr_candidates(chan: Option<&str>, output: bool, attr: &str) -> Vec<String> {
    let dir = if output { "out" } else { "in" };
    let mut names = vec![attr_file(chan, output, attr)];
    if let Some(c) = chan {
        let kind = c.trim_end_matches(|ch: char| ch.is_ascii_digit());
        if kind != c && !kind.is_empty() {
            names.push(format!("{dir}_{kind}_{attr}"));
        }
        names.push(format!("{dir}_{attr}"));
    }
    names
}

pub fn attr_path(dev: &IioDev, chan: Option<&str>, output: bool, attr: &str) -> AResult<PathBuf> {
    let names = attr_candidates(chan, output, attr);
    if names.iter().any(|n| n.contains('/') || n.contains("..")) {
        return Err(AgentError::new(Code::Usage, "attribute names cannot contain '/'"));
    }
    // First existing candidate; then a channel with an extended name
    // (`out_altvoltage0_RX_LO_frequency` for channel altvoltage0), which
    // libiio also resolves; if nothing exists keep the channel-specific name
    // so the error message names what was asked for.
    if let Some(p) = names.iter().map(|n| dev.path.join(n)).find(|p| p.exists()) {
        return Ok(p);
    }
    if let Some(c) = chan {
        let dir = if output { "out" } else { "in" };
        let prefix = format!("{dir}_{c}_");
        let suffix = format!("_{attr}");
        if let Ok(rd) = std::fs::read_dir(&dev.path) {
            let hits: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| extended_name_match(n, &prefix, &suffix))
                .collect();
            if hits.len() == 1 {
                return Ok(dev.path.join(&hits[0]));
            }
        }
    }
    Ok(dev.path.join(&names[0]))
}

/// `out_altvoltage0_RX_LO_frequency` matches channel `altvoltage0`, attr
/// `frequency`: prefix + non-empty label + suffix, and the label must not
/// itself contain the attribute boundary (keeps `..._frequency_available`
/// from matching `frequency`).
pub fn extended_name_match(name: &str, prefix: &str, suffix: &str) -> bool {
    name.len() > prefix.len() + suffix.len()
        && name.starts_with(prefix)
        && name.ends_with(suffix)
}

pub fn get(dev: &IioDev, file: &str) -> AResult<String> {
    let p = dev.path.join(file);
    std::fs::read_to_string(&p)
        .map(|s| s.trim_end().to_string())
        .ctx(format!("read {}", p.display()))
}

pub fn set(dev: &IioDev, file: &str, value: &str) -> AResult<()> {
    let p = dev.path.join(file);
    std::fs::write(&p, value.as_bytes()).ctx(format!("write {} = {value}", p.display()))
}

pub fn debug_dir(dev: &IioDev) -> AResult<PathBuf> {
    let _ = sys::ensure_debugfs();
    let d = PathBuf::from("/sys/kernel/debug/iio").join(&dev.id);
    if !d.is_dir() {
        return Err(AgentError::new(
            Code::Precondition,
            format!("{} does not exist (debugfs not mounted or no debug attrs)", d.display()),
        ));
    }
    Ok(d)
}

pub fn debug_get(dev: &IioDev, attr: &str) -> AResult<String> {
    if attr.contains('/') {
        return Err(AgentError::new(Code::Usage, "attribute names cannot contain '/'"));
    }
    let p = debug_dir(dev)?.join(attr);
    std::fs::read_to_string(&p)
        .map(|s| s.trim_end().to_string())
        .ctx(format!("read {}", p.display()))
}

pub fn debug_set(dev: &IioDev, attr: &str, value: &str) -> AResult<()> {
    if attr.contains('/') {
        return Err(AgentError::new(Code::Usage, "attribute names cannot contain '/'"));
    }
    let p = debug_dir(dev)?.join(attr);
    std::fs::write(&p, value.as_bytes()).ctx(format!("write {} = {value}", p.display()))
}

// ── AD9361 ────────────────────────────────────────────────────────────

pub struct Ad9361 {
    pub dev: IioDev,
}

impl Ad9361 {
    pub fn open() -> AResult<Ad9361> {
        Ok(Ad9361 { dev: find(PHY)? })
    }

    pub fn spi_read(&self, addr: u32) -> AResult<u32> {
        debug_set(&self.dev, "direct_reg_access", &format!("0x{addr:X}"))?;
        let s = debug_get(&self.dev, "direct_reg_access")?;
        parse_u64(s.trim())
            .map(|v| v as u32)
            .ok_or_else(|| AgentError::new(Code::Error, format!("bad direct_reg_access reply '{s}'")))
    }

    pub fn spi_write(&self, addr: u32, value: u32) -> AResult<()> {
        debug_set(&self.dev, "direct_reg_access", &format!("0x{addr:X} 0x{value:X}"))
    }

    pub fn bist_prbs(&self, mode: u32) -> AResult<()> {
        debug_set(&self.dev, "bist_prbs", &mode.to_string())
    }

    pub fn loopback(&self, mode: u32) -> AResult<()> {
        debug_set(&self.dev, "loopback", &mode.to_string())
    }

    pub fn bist_tone(&self, spec: &str) -> AResult<()> {
        debug_set(&self.dev, "bist_tone", spec)
    }

    pub fn attr(&self, file: &str) -> AResult<String> {
        get(&self.dev, file)
    }

    pub fn set_attr(&self, file: &str, v: &str) -> AResult<()> {
        set(&self.dev, file, v)
    }

    pub fn sample_rate(&self) -> Option<u64> {
        self.attr("in_voltage_sampling_frequency").ok()?.trim().parse().ok()
    }

    pub fn rx_lo(&self) -> Option<u64> {
        self.attr("out_altvoltage0_RX_LO_frequency").ok()?.trim().parse().ok()
    }

    pub fn temp_c(&self) -> Option<f64> {
        let v: f64 = self.attr("in_temp0_input").ok()?.trim().parse().ok()?;
        Some(v / 1000.0)
    }

    pub fn tx_atten_db(&self) -> Option<f64> {
        parse_db(&self.attr("out_voltage0_hardwaregain").ok()?)
    }

    pub fn set_tx_atten_db(&self, db: f64) -> AResult<()> {
        self.set_attr("out_voltage0_hardwaregain", &format!("{db:.2}"))
    }

    /// `compatible` of the phy's DT node (adi,ad9361 / adi,ad9363a / ...).
    pub fn compatible(&self) -> Option<String> {
        std::fs::read(self.dev.path.join("of_node/compatible"))
            .ok()
            .map(|b| {
                String::from_utf8_lossy(&b)
                    .split('\0')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(",")
            })
    }
}

/// Parses "-89.750000 dB" / "71 dB" / "-10".
pub fn parse_db(s: &str) -> Option<f64> {
    s.trim().trim_end_matches("dB").trim().parse().ok()
}

// ── XADC ──────────────────────────────────────────────────────────────

pub const XADC_DEFAULT_LABELS: &[&str] = &[
    "vccint", "vccaux", "vccbram", "vccpint", "vccpaux", "vccoddr", "vrefp", "vrefn",
];

/// Nominal rail voltages (Fishball: DDR3L -> vccoddr 1.35 V).
pub fn rail_nominal(label: &str) -> Option<f64> {
    Some(match label {
        "vccint" => 1.0,
        "vccaux" => 1.8,
        "vccbram" => 1.0,
        "vccpint" => 1.0,
        "vccpaux" => 1.8,
        "vccoddr" => 1.35,
        "vrefp" => 1.25,
        _ => return None,
    })
}

/// Reads the XADC: {"temp_c": .., "<label>": volts, ...}.
pub fn xadc_read() -> Option<Value> {
    let dev = find(XADC).ok()?;
    let mut m = serde_json::Map::new();
    let rd = |f: &str| -> Option<f64> { read_trim(dev.path.join(f))?.trim().parse().ok() };
    if let (Some(raw), Some(scale)) = (rd("in_temp0_raw"), rd("in_temp0_scale")) {
        let off = rd("in_temp0_offset").unwrap_or(0.0);
        m.insert("temp_c".into(), json!(crate::util::round3((raw + off) * scale / 1000.0)));
    }
    for n in 0..16 {
        let raw = match rd(&format!("in_voltage{n}_raw")) {
            Some(r) => r,
            None => continue,
        };
        let scale = rd(&format!("in_voltage{n}_scale")).unwrap_or(0.0);
        let label = read_trim(dev.path.join(format!("in_voltage{n}_label")))
            .or_else(|| XADC_DEFAULT_LABELS.get(n).map(|s| s.to_string()))
            .unwrap_or_else(|| format!("voltage{n}"));
        m.insert(label, json!(crate::util::round3(raw * scale / 1000.0)));
    }
    Some(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(attr_file(Some("voltage0"), true, "hardwaregain"), "out_voltage0_hardwaregain");
        assert_eq!(attr_file(Some("voltage0"), false, "rssi"), "in_voltage0_rssi");
        assert_eq!(attr_file(None, false, "name"), "name");
        assert_eq!(parse_db("-89.750000 dB"), Some(-89.75));
        assert_eq!(parse_db("71 dB"), Some(71.0));
        assert_eq!(rail_nominal("vccoddr"), Some(1.35));
    }
}

#[cfg(test)]
mod attr_name_tests {
    use super::attr_candidates;

    #[test]
    fn shared_attribute_fallbacks() {
        assert_eq!(
            attr_candidates(Some("voltage0"), false, "sampling_frequency"),
            vec!["in_voltage0_sampling_frequency", "in_voltage_sampling_frequency", "in_sampling_frequency"]
        );
        assert_eq!(attr_candidates(Some("altvoltage0"), true, "frequency")[1], "out_altvoltage_frequency");
        assert_eq!(attr_candidates(None, false, "name"), vec!["name"]);
    }
}

#[cfg(test)]
mod extended_name_tests {
    use super::extended_name_match;

    #[test]
    fn extended_channel_names() {
        let (p, s) = ("out_altvoltage0_", "_frequency");
        assert!(extended_name_match("out_altvoltage0_RX_LO_frequency", p, s));
        assert!(!extended_name_match("out_altvoltage0_RX_LO_frequency_available", p, s));
        assert!(!extended_name_match("out_altvoltage1_TX_LO_frequency", p, s));
        assert!(!extended_name_match("out_altvoltage0_frequency", p, s));
    }
}

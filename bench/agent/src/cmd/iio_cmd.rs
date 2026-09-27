use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::iio;
use crate::safety;
use serde_json::{json, Value};

/// Debugfs attributes that corrupt the RX stream (maintenance required to
/// set them to anything but "off").
const RX_CORRUPTING: &[&str] = &["bist_prbs", "bist_tone", "loopback", "bist_timing_analysis", "digital_tune"];

/// Is this a TX-enabling write that must pass the host interlock?
fn tx_enabling(file: &str, value: &str) -> bool {
    let f = file.to_ascii_lowercase();
    // Any TX gain file, per-channel or shared (attribute names now resolve
    // to the shared form when the channel-specific one does not exist).
    if f.starts_with("out_voltage") && f.ends_with("_hardwaregain") {
        return iio::parse_db(value).map(|v| v > iio::TX_ATTEN_MAX_DB + 0.001).unwrap_or(true);
    }
    if f.starts_with("out_altvoltage") && (f.ends_with("_scale") || f.ends_with("_raw")) {
        return value.trim().parse::<f64>().map(|v| v != 0.0).unwrap_or(true);
    }
    if f.ends_with("_tx_lo_powerdown") {
        return value.trim() == "0";
    }
    false
}

fn debug_tx_enabling(attr: &str, value: &str) -> bool {
    let first = value.split_whitespace().next().unwrap_or("0");
    match attr {
        // mode 1 = inject TX
        "bist_prbs" | "bist_tone" => first == "1",
        _ => false,
    }
}

fn debug_is_off(attr: &str, value: &str) -> bool {
    let first = value.split_whitespace().next().unwrap_or("0");
    match attr {
        "bist_prbs" | "loopback" | "bist_tone" => first == "0",
        _ => false,
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let kind = sub(args, 1, &["attr", "debug"])?;
    let op = sub(args, 2, &["get", "set"])?;
    let dev_name = args.req("dev")?;
    let attr = args.req("attr")?;
    let chan = args.opt("chan")?;
    let output = args.flag("out");
    let value = if op == "set" { Some(args.req("value")?) } else { None };
    let tx_ok = args.flag("tx-ok");
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    args.finish()?;
    let dev = iio::find(&dev_name)?;
    match kind {
        "attr" => {
            // Resolve shared attributes (e.g. in_voltage_sampling_frequency
            // for channel voltage0) the way libiio does.
            let path = iio::attr_path(&dev, chan.as_deref(), output, &attr)?;
            let file = path
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_else(|| iio::attr_file(chan.as_deref(), output, &attr));
            if let Some(v) = &value {
                if tx_enabling(&file, v) && !tx_ok {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!("{file} = {v} enables TX output; the host must pass the TX interlock and add --tx-ok"),
                    ));
                }
                iio::set(&dev, &file, v)?;
            }
            let rb = iio::get(&dev, &file)?;
            Ok(json!({"dev": dev.name, "id": dev.id, "attr": file, "value": rb, "written": value}))
        }
        _ => {
            if let Some(v) = &value {
                if debug_tx_enabling(&attr, v) && !tx_ok {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!("{attr} = {v} injects into TX; the host must pass the TX interlock and add --tx-ok"),
                    ));
                }
                let mut w = Vec::new();
                let _m = if RX_CORRUPTING.contains(&attr.as_str()) && !debug_is_off(&attr, v) {
                    safety::require_maintenance(auto, ignore, &mut w)?
                } else {
                    None
                };
                for x in w {
                    ctx.warn(x);
                }
                iio::debug_set(&dev, &attr, v)?;
            }
            let rb = if attr == "direct_reg_access" && value.is_some() && value.as_deref().unwrap_or("").contains(' ') {
                None
            } else {
                Some(iio::debug_get(&dev, &attr)?)
            };
            Ok(json!({"dev": dev.name, "id": dev.id, "attr": attr, "value": rb, "written": value}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tx_rules() {
        assert!(!tx_enabling("out_voltage0_hardwaregain", "-89.75"));
        assert!(tx_enabling("out_voltage0_hardwaregain", "-30"));
        assert!(tx_enabling("out_voltage0_hardwaregain", "junk"));
        assert!(!tx_enabling("in_voltage0_hardwaregain", "70"));
        assert!(tx_enabling("out_altvoltage0_TX1_I_F1_scale", "0.5"));
        assert!(!tx_enabling("out_altvoltage0_TX1_I_F1_scale", "0"));
        assert!(debug_tx_enabling("bist_prbs", "1"));
        assert!(!debug_tx_enabling("bist_prbs", "2"));
        assert!(debug_tx_enabling("bist_tone", "1 1000000 0 0"));
        assert!(debug_is_off("bist_tone", "0 0 0 0"));
    }
}

use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::iio::Ad9361;
use crate::safety;
use serde_json::{json, Value};

/// `ad9361 spi read|write --addr A [--value V]` via debugfs direct_reg_access.
/// Writes change the transceiver configuration: maintenance mode is
/// required while p25-httpd runs (or --auto-maint / --ignore-maint).
pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    sub(args, 1, &["spi"])?;
    let op = sub(args, 2, &["read", "write"])?;
    let addr = args.u64_req("addr")?;
    let value = if op == "write" { Some(args.u64_req("value")?) } else { None };
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    args.finish()?;
    if addr > 0x3FF {
        return Err(AgentError::new(Code::Usage, "AD9361 SPI addresses are 10 bits (0x000-0x3FF)"));
    }
    let phy = Ad9361::open()?;
    let mut warnings = Vec::new();
    let _m = if let Some(v) = value {
        if v > 0xFF {
            return Err(AgentError::new(Code::Usage, "AD9361 SPI registers are 8 bits"));
        }
        let g = safety::require_maintenance(auto, ignore, &mut warnings)?;
        phy.spi_write(addr as u32, v as u32)?;
        g
    } else {
        None
    };
    for w in warnings {
        ctx.warn(w);
    }
    let rb = phy.spi_read(addr as u32)?;
    Ok(json!({
        "addr": format!("0x{addr:03X}"),
        "value": rb,
        "hex": format!("0x{rb:02X}"),
        "written": value.map(|v| format!("0x{v:02X}")),
    }))
}

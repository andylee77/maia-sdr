use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::safety;
use serde_json::Value;

/// `tx off [--lo-powerdown]`: max attenuation, DAC DATA_SEL zero on
/// channels 0-1, DDS scale 0, AD9361 loopback / BIST PRBS / BIST tone off.
pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    sub(args, 1, &["off"])?;
    let lo = args.flag("lo-powerdown");
    args.finish()?;
    let v = safety::tx_off(ctx.maps(), lo);
    if v["ok"] != serde_json::json!(true) {
        return Err(AgentError::new(Code::Error, "tx off incomplete (see detail.actions)").with_detail(v));
    }
    Ok(v)
}

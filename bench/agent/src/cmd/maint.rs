use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::AResult;
use crate::safety;
use serde_json::Value;

/// `maint enter|exit|status` (safety rule 4).
pub fn run(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 1, &["enter", "exit", "status"])?;
    args.finish()?;
    match op {
        "enter" => safety::maint_enter(),
        "exit" => safety::maint_exit(),
        _ => Ok(safety::maint_status_json()),
    }
}

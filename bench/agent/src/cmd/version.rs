use super::{build_info, Ctx};
use crate::cli::Args;
use crate::err::AResult;
use serde_json::{json, Value};

pub fn run(_ctx: &Ctx, args: &Args) -> AResult<Value> {
    args.finish()?;
    let b = build_info();
    Ok(json!({
        "agent": "fbench-agent",
        "version": env!("CARGO_PKG_VERSION"),
        "git": b["git"],
        "build": b,
        "schema": "fbench.agent/1",
        "patterns": crate::checker::PATTERNS,
        "memtest_patterns": crate::memtest::PATTERNS,
        "builtin_maps": crate::regmap::BUILTIN_SOURCES.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
    }))
}

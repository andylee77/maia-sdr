//! fbench-agent: on-board test agent for the Fishball Z7020 hardware
//! validation suite (doc/HW_VALIDATION_SUITE.md section 5.3).
//!
//! Contract: every invocation prints exactly one JSON object to stdout
//! (`{"ok": true, ...}` or `{"ok": false, "code": ..., "error": ...}`)
//! unless JSONL streaming to stdout is requested (`--jsonl -`), logs go to
//! stderr, and the exit code is 0 iff `ok` is true (2 error/usage,
//! 3 precondition/unsupported/wrong image, 4 safety refusal).

// Several helpers exist for tests / the host contract only.
#![allow(dead_code)]

mod access;
mod checker;
mod cli;
mod cmd;
mod err;
mod eye;
mod hist;
mod iio;
mod memtest;
mod regio;
mod regmap;
mod rings;
mod safety;
mod sha256;
mod sigmf;
mod synth;
mod sys;
mod util;

use serde_json::{json, Value};
use std::io::Write;

fn emit(v: &Value, pretty: bool) {
    let s = if pretty {
        serde_json::to_string_pretty(v).unwrap_or_default()
    } else {
        v.to_string()
    };
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{s}");
    let _ = out.flush();
}

fn main() {
    let _ = util::since_start(); // start the elapsed_s clock
    safety::install_handlers();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let pretty = argv.iter().any(|a| a == "--pretty");
    let code = run(&argv, pretty);
    std::process::exit(code);
}

fn run(argv: &[String], pretty: bool) -> i32 {
    let args = match cli::Args::parse(argv) {
        Ok(a) => a,
        Err(e) => {
            emit(&error_json("", &e), pretty);
            return e.code.exit_code();
        }
    };
    let ctx = match cmd::Ctx::from_args(&args) {
        Ok(c) => c,
        Err(e) => {
            emit(&error_json("", &e), pretty);
            return e.code.exit_code();
        }
    };
    let cmd_name = cmd::command_name(&args);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cmd::dispatch(&ctx, &args)));
    match result {
        Ok(Ok(v)) => {
            let mut out = serde_json::Map::new();
            out.insert("ok".into(), json!(true));
            out.insert("cmd".into(), json!(cmd_name));
            if let Value::Object(m) = v {
                for (k, val) in m {
                    if k != "ok" {
                        out.insert(k, val);
                    }
                }
            } else {
                out.insert("result".into(), v);
            }
            let w = ctx.take_warnings();
            if !w.is_empty() {
                let mut all: Vec<Value> = match out.remove("warnings") {
                    Some(Value::Array(a)) => a,
                    _ => Vec::new(),
                };
                all.extend(w.into_iter().map(Value::String));
                out.insert("warnings".into(), Value::Array(all));
            }
            out.insert("elapsed_s".into(), json!(util::round3(util::since_start().as_secs_f64())));
            emit(&Value::Object(out), pretty);
            0
        }
        Ok(Err(e)) => {
            let mut v = error_json(&cmd_name, &e);
            let w = ctx.take_warnings();
            if !w.is_empty() {
                v["warnings"] = json!(w);
            }
            emit(&v, pretty);
            e.code.exit_code()
        }
        Err(p) => {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "panic".into());
            emit(
                &json!({"ok": false, "cmd": cmd_name, "code": "error", "error": format!("internal error (panic): {msg}")}),
                pretty,
            );
            2
        }
    }
}

fn error_json(cmd: &str, e: &err::AgentError) -> Value {
    let mut v = json!({"ok": false, "cmd": cmd, "code": e.code.as_str(), "error": e.msg});
    if let Some(d) = &e.detail {
        v["detail"] = d.clone();
    }
    if e.code == err::Code::Interrupted {
        v["interrupted"] = json!(true);
    }
    v
}

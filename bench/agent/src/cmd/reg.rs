use super::{open_core, sub, Ctx};
use crate::access::{RegAccess, WriteOpts};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::regmap::RegDef;
use crate::util::hex32;
use serde_json::{json, Value};

fn reg_json(core: &str, r: &RegDef, v: u32) -> Value {
    json!({
        "core": core,
        "reg": r.name,
        "offset": format!("0x{:03X}", r.offset),
        "value": v,
        "hex": hex32(v),
        "fields": r.decode(v),
    })
}

/// `reg read|write|dump|list --core C [--reg R] [--value V]`
pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let op = sub(args, 1, &["read", "write", "dump", "list"])?;
    let core_name = if op == "list" { args.opt("core")? } else { Some(args.req("core")?) };
    let reg_spec = if matches!(op, "read" | "write") { Some(args.req("reg")?) } else { None };
    let value = if op == "write" { Some(args.u64_req("value")?) } else { None };
    let force_se = args.flag("force-side-effects");
    let force = args.flag("force");
    let tx_ok = args.flag("tx-ok");
    let no_init = args.flag("no-init");
    args.finish()?;

    if op == "list" {
        let maps = ctx.maps();
        return Ok(match core_name {
            None => json!({
                "cores": maps.cores.values().map(|c| c.summary()).collect::<Vec<_>>(),
                "share_dir": ctx.share_dir.display().to_string(),
                "share_files": maps.share_files,
            }),
            Some(n) => {
                let c = maps.core(&n)?;
                json!({
                    "core": c.summary(),
                    "regs": c.regs.iter().map(|r| json!({
                        "name": r.name, "offset": format!("0x{:03X}", r.offset), "access": r.access.as_str(),
                        "snapshot": r.snapshot, "domain": r.domain, "read_side_effect": r.read_side_effect,
                        "dangerous": r.dangerous, "tx_affecting": r.tx_affecting,
                    })).collect::<Vec<_>>(),
                })
            }
        });
    }

    let core_name = core_name.unwrap();
    // Allow-list checks come first: an unmapped/vacant register is a
    // safety refusal whatever image is running.
    {
        let core = ctx.maps().core(&core_name)?;
        if let Some(spec) = &reg_spec {
            let r = core.get(spec)?;
            if let Some(why) = core.offset_forbidden(r.offset) {
                return Err(AgentError::new(Code::Safety, why));
            }
            if op == "read" && r.read_side_effect && !force_se {
                return Err(AgentError::new(
                    Code::Safety,
                    format!("{core_name}.{} is read-to-clear; pass --force-side-effects to read it anyway", r.name),
                ));
            }
            if op == "write" {
                core.write_allowed(r).map_err(|m| AgentError::new(Code::Safety, m))?;
                if r.dangerous && !force {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!("{core_name}.{} is marked dangerous (core reset); pass --force to write it", r.name),
                    ));
                }
                if r.tx_affecting && !tx_ok {
                    return Err(AgentError::new(
                        Code::Safety,
                        format!("{core_name}.{} changes the TX output; the host must pass the TX interlock and add --tx-ok", r.name),
                    ));
                }
            }
        }
    }
    // hwval: make sure the core was initialised this boot (FIFO resets).
    if core_name == "hwval" {
        super::hwval::implicit_init(ctx, no_init)?;
    }
    let (core, mut pm) = open_core(ctx, &core_name, op == "write")?;
    let mut a = RegAccess::new(core, &mut pm);
    match op {
        "read" => {
            let spec = reg_spec.unwrap();
            let r = core.get(&spec)?.clone();
            let v = a.read_reg(&r, force_se)?;
            let mut out = reg_json(&core.name, &r, v);
            if !a.snaps.is_empty() {
                out["snapshot"] = a.snapshot_json();
                out["stale"] = json!(a.dead_mask != 0);
            }
            if r.read_side_effect {
                out["side_effect"] = json!("read-to-clear bits were cleared by this read");
            }
            Ok(out)
        }
        "write" => {
            let spec = reg_spec.unwrap();
            let r = core.get(&spec)?.clone();
            let v = value.unwrap();
            a.write_reg(
                &r,
                v,
                WriteOpts {
                    force_dangerous: force,
                    tx_ok,
                },
            )?;
            let mut out = json!({
                "core": core.name, "reg": r.name, "offset": format!("0x{:03X}", r.offset),
                "written": format!("0x{v:X}"),
            });
            if r.access.readable() && !r.read_side_effect && r.snapshot.is_none() {
                let rb = a.read_reg(&r, false)?;
                out["value"] = json!(rb);
                out["hex"] = json!(hex32(rb));
                out["fields"] = r.decode(rb);
            }
            Ok(out)
        }
        _ => {
            // dump
            let gate = a.gate_asserted()?;
            let regs: Vec<RegDef> = core.regs.clone();
            let mut skipped = Vec::new();
            let mut readable = Vec::new();
            for r in &regs {
                match a.check_read(r, force_se) {
                    Ok(()) => readable.push(r.clone()),
                    Err(e) => skipped.push(json!({"reg": r.name, "reason": e.msg, "code": e.code.as_str()})),
                }
            }
            if !core.snapshot_domains.is_empty() {
                let mask: u32 = core.snapshot_domains.values().fold(0, |m, b| m | b);
                if core.has("SNAP_REQ") {
                    a.snapshot(mask)?;
                }
            }
            let mut vals = serde_json::Map::new();
            let mut hexes = serde_json::Map::new();
            let mut fields = serde_json::Map::new();
            for r in &readable {
                let v = a.read_reg(r, force_se)?;
                vals.insert(r.name.clone(), json!(v));
                hexes.insert(r.name.clone(), json!(hex32(v)));
                if !r.fields.is_empty() {
                    fields.insert(r.name.clone(), r.decode(v));
                }
            }
            if vals.is_empty() && !skipped.is_empty() {
                return Err(AgentError::new(Code::Safety, "no register of this core may be read now")
                    .with_detail(json!({"skipped": skipped})));
            }
            Ok(json!({
                "core": core.name,
                "base": format!("0x{:08X}", core.base),
                "source": core.source,
                "reset_gate_asserted": gate,
                "regs": vals,
                "hex": hexes,
                "fields": fields,
                "skipped": skipped,
                "snapshot": a.snapshot_json(),
            }))
        }
    }
}

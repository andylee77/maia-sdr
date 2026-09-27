//! `telemetry --seconds N --interval-ms M [--jsonl FILE|-]`

use super::{jsonl_target, open_core, Ctx, Jsonl};
use crate::access::RegAccess;
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::iio;
use crate::safety;
use crate::sys;
use crate::util::{round3, unix_now_f64};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

#[derive(Default)]
struct Agg {
    min: BTreeMap<String, f64>,
    max: BTreeMap<String, f64>,
    sum: BTreeMap<String, f64>,
    n: BTreeMap<String, u64>,
}

impl Agg {
    fn add(&mut self, k: &str, v: f64) {
        let mn = self.min.entry(k.into()).or_insert(v);
        *mn = mn.min(v);
        let mx = self.max.entry(k.into()).or_insert(v);
        *mx = mx.max(v);
        *self.sum.entry(k.into()).or_insert(0.0) += v;
        *self.n.entry(k.into()).or_insert(0) += 1;
    }
    fn json(&self) -> Value {
        let mut m = serde_json::Map::new();
        for (k, n) in &self.n {
            m.insert(
                k.clone(),
                json!({"min": round3(self.min[k]), "max": round3(self.max[k]), "mean": round3(self.sum[k] / *n as f64), "n": n}),
            );
        }
        Value::Object(m)
    }
}

fn deltas(now: &BTreeMap<String, u64>, prev: &BTreeMap<String, u64>) -> Value {
    let mut m = serde_json::Map::new();
    for (k, v) in now {
        let d = v.saturating_sub(*prev.get(k).unwrap_or(v));
        if d > 0 {
            m.insert(k.clone(), json!(d));
        }
    }
    Value::Object(m)
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let seconds = args.f64_or("seconds", 10.0)?;
    let interval_ms = args.u64_or("interval-ms", 1000)?.max(10);
    let target = jsonl_target(args)?;
    args.finish()?;
    if !(0.0..=7.0 * 86400.0).contains(&seconds) {
        return Err(AgentError::new(Code::Usage, "--seconds out of range"));
    }
    let mut sink = Jsonl::open(target.as_deref())?;
    let phy = iio::Ad9361::open().ok();
    let mut adc = match open_core(ctx, "adi_adc", false) {
        Ok(x) => Some(x),
        Err(e) => {
            ctx.warn(format!("CLK_FREQ unavailable: {}", e.msg));
            None
        }
    };
    let n = ((seconds * 1000.0) / interval_ms as f64).floor().max(1.0) as u64;
    let t0 = Instant::now();
    let mut prev_irq = sys::parse_interrupts(&std::fs::read_to_string("/proc/interrupts").unwrap_or_default());
    let mut prev_soft = sys::parse_softirqs(&std::fs::read_to_string("/proc/softirqs").unwrap_or_default());
    let mut prev_stat = sys::parse_stat_cpus(&std::fs::read_to_string("/proc/stat").unwrap_or_default());
    let mut samples = Vec::new();
    let mut events = Vec::new();
    let mut agg = Agg::default();
    let mut last_clk: Option<f64> = None;
    for k in 0..n {
        let tick = t0 + Duration::from_millis(interval_ms * (k + 1));
        let now = Instant::now();
        if tick > now {
            safety::sleep_ms((tick - now).as_millis() as u64)?;
        }
        let t = round3(t0.elapsed().as_secs_f64());
        let xadc = iio::xadc_read();
        let ad_temp = phy.as_ref().and_then(|p| p.temp_c());
        let clk = adc.as_mut().and_then(|(core, pm)| {
            let mut a = RegAccess::new(core, pm);
            a.read("CLK_FREQ").ok()
        });
        let clk_hz = clk.map(|c| (c as f64 * 100e6 / 65536.0).round());
        let irq = sys::parse_interrupts(&std::fs::read_to_string("/proc/interrupts").unwrap_or_default());
        let soft = sys::parse_softirqs(&std::fs::read_to_string("/proc/softirqs").unwrap_or_default());
        let stat = sys::parse_stat_cpus(&std::fs::read_to_string("/proc/stat").unwrap_or_default());
        let mut cpu = serde_json::Map::new();
        for (name, v) in &stat {
            if let Some(p) = prev_stat.get(name) {
                let total = v[3].saturating_sub(p[3]).max(1) as f64;
                cpu.insert(
                    name.clone(),
                    json!({"irq": v[0].saturating_sub(p[0]), "softirq": v[1].saturating_sub(p[1]),
                           "busy_pct": round3(100.0 * v[2].saturating_sub(p[2]) as f64 / total)}),
                );
            }
        }
        let mem = sys::meminfo();
        let sample = json!({
            "t": t,
            "ts": round3(unix_now_f64()),
            "xadc": xadc,
            "ad9361_temp_c": ad_temp,
            "clk_freq_hz": clk_hz,
            "clk_freq_raw": clk,
            "loadavg": sys::loadavg(),
            "irq_deltas": deltas(&irq, &prev_irq),
            "softirq_deltas": deltas(&soft, &prev_soft),
            "cpu": Value::Object(cpu),
            "mem_available_kb": mem.get("MemAvailable"),
        });
        prev_irq = irq;
        prev_soft = soft;
        prev_stat = stat;
        // Aggregates and threshold events.
        if let Some(Value::Object(x)) = &xadc {
            for (kname, v) in x {
                if let Some(f) = v.as_f64() {
                    agg.add(&format!("xadc.{kname}"), f);
                    if kname == "temp_c" && f >= 85.0 {
                        events.push(json!({"t": t, "kind": "over_temp", "detail": format!("XADC {f:.1} C")}));
                    }
                    if let Some(nom) = iio::rail_nominal(kname) {
                        if (f - nom).abs() > nom * 0.05 {
                            events.push(json!({"t": t, "kind": "rail_out_of_range", "detail": format!("{kname} {f:.3} V (nominal {nom} V +- 5 %)")}));
                        }
                    }
                }
            }
        }
        if let Some(tc) = ad_temp {
            agg.add("ad9361_temp_c", tc);
            if tc >= 85.0 {
                events.push(json!({"t": t, "kind": "over_temp", "detail": format!("AD9361 {tc:.1} C")}));
            }
        }
        if let Some(c) = clk_hz {
            agg.add("clk_freq_hz", c);
            if let Some(l) = last_clk {
                if (c - l).abs() > l.max(1.0) * 0.01 {
                    events.push(json!({"t": t, "kind": "clk_freq_change", "detail": format!("{l} -> {c} Hz")}));
                }
            }
            last_clk = Some(c);
        }
        if let Some(la) = sys::loadavg() {
            agg.add("loadavg1", la[0]);
        }
        sink.write(&sample);
        samples.push(sample);
    }
    sink.finish();
    let total = samples.len();
    let inline = !sink.active() && total <= 3600;
    let kept: Vec<Value> = if inline {
        samples
    } else {
        samples.into_iter().skip(total.saturating_sub(10)).collect()
    };
    Ok(json!({
        "seconds": round3(t0.elapsed().as_secs_f64()),
        "interval_ms": interval_ms,
        "count": total,
        "samples": kept,
        "samples_truncated": !inline,
        "summary": agg.json(),
        "events": events,
        "jsonl": sink.path,
        "jsonl_lines": sink.lines,
    }))
}

//! `prbs soak --seconds N --poll-ms P [--rate HZ]`: AD9361 BIST PRBS
//! through the LVDS interface, error-interval counting with the ADI PN
//! monitors (sticky CHAN_STATUS polled and cleared every interval).

use super::adi_util::Adi;
use super::eyescan::{arm_rx_restore, start_rx_prbs};
use super::{sub, Ctx};
use crate::cli::Args;
use crate::err::AResult;
use crate::safety::{self, Cleanup};
use crate::util::round3;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    sub(args, 1, &["soak"])?;
    let seconds = args.f64_or("seconds", 60.0)?;
    let poll_ms = args.u64_or("poll-ms", 100)?.max(2);
    let rate = args.u64_opt("rate")?;
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    args.finish()?;
    let mut warnings = Vec::new();
    let _maint = safety::require_maintenance(auto, ignore, &mut warnings)?;
    for w in warnings {
        ctx.warn(w);
    }
    let mut cleanup = Cleanup::new();
    let mut adi = Adi::open(ctx)?;
    let saved = arm_rx_restore(ctx, &mut adi, &mut cleanup, rate)?;
    start_rx_prbs(&mut adi)?;
    // Initial lock: clear and require one clean interval before counting.
    let lock = adi.check(poll_ms)?;
    let t0 = Instant::now();
    let end = t0 + Duration::from_secs_f64(seconds);
    let mut polls = 0u64;
    let mut err_intervals = 0u64;
    let mut oos_events = 0u64;
    let mut if_status_bad = 0u64;
    let mut ovf = 0u64;
    let mut first_error: Option<f64> = None;
    let mut last_error: Option<f64> = None;
    let mut times = Vec::new();
    let mut per_ch = [[0u64; 2]; 2];
    while Instant::now() < end {
        let c = adi.check(poll_ms)?;
        polls += 1;
        let t = t0.elapsed().as_secs_f64();
        if !c.status_ok {
            if_status_bad += 1;
        }
        let mut bad = !c.status_ok;
        for ch in 0..2 {
            if c.ch[ch] & 0x2 != 0 {
                per_ch[ch][0] += 1;
                oos_events += 1;
                bad = true;
            }
            if c.ch[ch] & 0x4 != 0 {
                per_ch[ch][1] += 1;
                bad = true;
            }
        }
        if let Ok(u) = adi.adc.read("UP_STATUS") {
            if u & 0x4 != 0 {
                ovf += 1;
                let _ = adi.adc.write("UP_STATUS", 0x4);
            }
        }
        if bad {
            err_intervals += 1;
            first_error.get_or_insert(t);
            last_error = Some(t);
            if times.len() < 200 {
                times.push(round3(t));
            }
        }
    }
    let clk = adi.clk_freq_hz();
    drop(adi);
    let restored = cleanup.run();
    Ok(json!({
        "seconds": round3(t0.elapsed().as_secs_f64()),
        "poll_ms": poll_ms,
        "polls": polls,
        "error_intervals": err_intervals,
        "oos_events": oos_events,
        "if_status_bad": if_status_bad,
        "adc_overflow_intervals": ovf,
        "per_channel": {"ch0": {"oos": per_ch[0][0], "err": per_ch[0][1]}, "ch1": {"oos": per_ch[1][0], "err": per_ch[1][1]}},
        "first_error_s": first_error.map(round3),
        "last_error_s": last_error.map(round3),
        "error_times_s": times,
        "initial_lock": lock.to_json(),
        "clk_freq_hz": clk,
        "pass": err_intervals == 0 && lock.pass,
        "saved": saved,
        "restored": restored,
    }))
}

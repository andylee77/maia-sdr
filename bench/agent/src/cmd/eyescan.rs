//! `eyescan --mode idelay|ad9361|2d|ad9361-driver --rate HZ --dwell-ms D [--lanes ..]`
//!
//! All sweep loops run on the board. The AD9361 injects its BIST PRBS into
//! the RX path (bist_prbs = 2) and the axi_ad9361 PN monitors (PN_SEL = 0,
//! the device-specific pn0fn sequence) judge each delay setting: clear the
//! sticky CHAN_STATUS, dwell, read. Every changed setting is restored on
//! exit (also on error/signal) and maintenance mode is required.

use super::adi_util::{apply_rate, Adi, DATA_LANES, FRAME_LANE, PN_SEL_AD9361, SPI_RX_DELAY};
use super::{open_core, Ctx};
use crate::access::RegAccess;
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::eye;
use crate::iio::{self, Ad9361};
use crate::safety::{self, Cleanup};
use crate::util::round3;
use serde_json::{json, Value};
use std::time::Instant;

pub const SETTLE_MS: u64 = 1;

/// Saves and restores the RX interface state around a sweep.
pub fn arm_rx_restore<'m>(ctx: &'m Ctx, adi: &mut Adi, cleanup: &mut Cleanup<'m>, rate: Option<u64>) -> AResult<Value> {
    let (orig_rate, active_rate) = apply_rate(&adi.phy, None)?;
    // Restore order (reverse of push): BIST off, PN_SEL, IDELAYs, 0x006, rate.
    if let (Some(r), Some(o)) = (rate, orig_rate) {
        if r != o {
            cleanup.push("restore sample rate", move || {
                Ad9361::open()?.set_attr("in_voltage_sampling_frequency", &o.to_string())?;
                Ok(json!(o))
            });
        }
    }
    let (_, active) = apply_rate(&adi.phy, rate)?;
    let reg6 = adi.phy.spi_read(SPI_RX_DELAY)?;
    cleanup.push("restore AD9361 0x006", move || {
        Ad9361::open()?.spi_write(SPI_RX_DELAY, reg6)?;
        Ok(json!(format!("0x{reg6:02X}")))
    });
    let taps = adi.idelays()?;
    let taps_c = taps.clone();
    cleanup.push("restore IDELAY taps", move || {
        let (core, pm) = open_core(ctx, "adi_adc", true)?;
        let mut a = RegAccess::new(core, pm);
        for (l, t) in taps_c.iter().enumerate() {
            a.write(&format!("IDELAY_{l}"), *t)?;
        }
        Ok(json!(taps_c))
    });
    let c3 = [adi.cntrl3(0)?, adi.cntrl3(1)?];
    cleanup.push("restore CHAN_CNTRL_3 (PN_SEL/DATA_SEL)", move || {
        let (core, pm) = open_core(ctx, "adi_adc", true)?;
        let mut a = RegAccess::new(core, pm);
        a.write("CHAN0_CNTRL_3", c3[0])?;
        a.write("CHAN1_CNTRL_3", c3[1])?;
        Ok(json!([c3[0], c3[1]]))
    });
    cleanup.push("bist_prbs=0", || {
        Ad9361::open()?.bist_prbs(0)?;
        Ok(json!(0))
    });
    let _ = active_rate;
    Ok(json!({"rate_hz": active, "original_rate_hz": orig_rate, "reg_0x006": format!("0x{reg6:02X}"),
              "clk_delay": reg6 >> 4, "data_delay": reg6 & 0xF, "idelay_taps": taps}))
}

/// BIST PRBS on, PN monitors on the AD9361 sequence.
pub fn start_rx_prbs(adi: &mut Adi) -> AResult<()> {
    adi.phy.bist_prbs(2)?;
    adi.set_pn_sel(0, PN_SEL_AD9361)?;
    adi.set_pn_sel(1, PN_SEL_AD9361)?;
    safety::sleep_ms(10)?;
    Ok(())
}

fn parse_lanes(spec: Option<String>, default_all_together: bool) -> AResult<Vec<Option<usize>>> {
    match spec.as_deref() {
        None if default_all_together => Ok(vec![None]),
        None => Ok((0..DATA_LANES).map(Some).collect()),
        Some("all") => Ok(vec![None]),
        Some("each") => Ok((0..DATA_LANES).map(Some).collect()),
        Some(s) => s
            .split(',')
            .map(|x| {
                let l: usize = x.trim().parse().map_err(|_| AgentError::new(Code::Usage, format!("bad lane '{x}'")))?;
                if l > FRAME_LANE {
                    return Err(AgentError::new(Code::Usage, "lanes are 0-5 (data) and 6 (frame)"));
                }
                Ok(Some(l))
            })
            .collect(),
    }
}

fn set_lane(adi: &mut Adi, lane: Option<usize>, tap: u32) -> AResult<()> {
    match lane {
        Some(l) => adi.set_idelay(l, tap),
        None => {
            for l in 0..DATA_LANES {
                adi.set_idelay(l, tap)?;
            }
            Ok(())
        }
    }
}

fn restore_lane(adi: &mut Adi, lane: Option<usize>, taps: &[u32]) -> AResult<()> {
    match lane {
        Some(l) => adi.set_idelay(l, taps[l]),
        None => {
            for (l, t) in taps.iter().enumerate().take(DATA_LANES) {
                adi.set_idelay(l, *t)?;
            }
            Ok(())
        }
    }
}

fn lane_json(l: Option<usize>) -> Value {
    match l {
        Some(x) => json!(x),
        None => json!("all"),
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let mode = args.opt_or("mode", "idelay")?;
    let rate = args.u64_opt("rate")?;
    let dwell = args.u64_or("dwell-ms", 10)?;
    let lanes_spec = args.opt("lanes")?;
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    args.finish()?;
    if !["idelay", "ad9361", "2d", "ad9361-driver"].contains(&mode.as_str()) {
        return Err(AgentError::new(Code::Usage, "--mode idelay|ad9361|2d|ad9361-driver"));
    }
    let mut warnings = Vec::new();
    let _maint = safety::require_maintenance(auto, ignore, &mut warnings)?;
    for w in warnings {
        ctx.warn(w);
    }
    let t0 = Instant::now();
    let mut cleanup = Cleanup::new();
    let mut adi = Adi::open(ctx)?;
    let saved = arm_rx_restore(ctx, &mut adi, &mut cleanup, rate)?;
    let taps: Vec<u32> = saved["idelay_taps"]
        .as_array()
        .map(|a| a.iter().map(|x| x.as_u64().unwrap_or(0) as u32).collect())
        .unwrap_or_default();
    let reg6 = u32::from_str_radix(saved["reg_0x006"].as_str().unwrap_or("0x0").trim_start_matches("0x"), 16).unwrap_or(0);
    let (clk_c, data_c) = ((reg6 >> 4) & 0xF, reg6 & 0xF);
    start_rx_prbs(&mut adi)?;
    let baseline = adi.check(dwell)?;
    let clk_freq = adi.clk_freq_hz();

    let mut out = json!({
        "mode": mode,
        "dwell_ms": dwell,
        "chosen": {"clk": clk_c, "data": data_c},
        "current_taps": taps,
        "saved": saved,
        "baseline": baseline.to_json(),
        "clk_freq_hz": clk_freq,
    });
    let mut points = 0u64;
    match mode.as_str() {
        "idelay" => {
            let lanes = parse_lanes(lanes_spec, false)?;
            let mut res = Vec::new();
            for lane in lanes {
                let mut pass = Vec::with_capacity(32);
                for tap in 0..32u32 {
                    set_lane(&mut adi, lane, tap)?;
                    safety::sleep_ms(SETTLE_MS)?;
                    pass.push(adi.check(dwell)?.pass);
                    points += 1;
                }
                restore_lane(&mut adi, lane, &taps)?;
                let chosen = lane.map(|l| taps[l] as usize).or_else(|| taps.first().map(|t| *t as usize));
                let mut v = eye::summary_1d(&pass, chosen);
                v["lane"] = lane_json(lane);
                res.push(v);
            }
            out["lanes"] = json!(res);
            let min_w = res.iter().filter_map(|l| l["window"]["len"].as_u64()).min();
            out["window_taps_min"] = json!(min_w);
        }
        "ad9361" => {
            let mut grid = vec![vec![false; 16]; 16];
            for clk in 0..16u32 {
                for data in 0..16u32 {
                    adi.phy.spi_write(SPI_RX_DELAY, (clk << 4) | data)?;
                    safety::sleep_ms(SETTLE_MS)?;
                    grid[clk as usize][data as usize] = adi.check(dwell)?.pass;
                    points += 1;
                }
            }
            adi.phy.spi_write(SPI_RX_DELAY, reg6)?;
            let s = eye::summary_2d(&grid, Some((clk_c as usize, data_c as usize)));
            out["grid"] = s["grid"].clone();
            out["summary"] = s;
            out["axes"] = json!({"rows": "AD9361 0x006[7:4] clock delay", "cols": "AD9361 0x006[3:0] data delay"});
        }
        "2d" => {
            let lanes = parse_lanes(lanes_spec, false)?;
            let mut res = Vec::new();
            for lane in lanes {
                let mut grid = vec![vec![false; 32]; 16];
                for d in 0..16u32 {
                    adi.phy.spi_write(SPI_RX_DELAY, (clk_c << 4) | d)?;
                    for tap in 0..32u32 {
                        set_lane(&mut adi, lane, tap)?;
                        safety::sleep_ms(SETTLE_MS)?;
                        grid[d as usize][tap as usize] = adi.check(dwell)?.pass;
                        points += 1;
                    }
                    restore_lane(&mut adi, lane, &taps)?;
                }
                adi.phy.spi_write(SPI_RX_DELAY, reg6)?;
                let chosen_tap = lane.map(|l| taps[l] as usize).unwrap_or(taps[0] as usize);
                let s = eye::summary_2d(&grid, Some((data_c as usize, chosen_tap)));
                res.push(json!({"lane": lane_json(lane), "grid": s["grid"], "summary": s}));
            }
            out["lanes"] = json!(res);
            out["axes"] = json!({"rows": "AD9361 0x006[3:0] data delay (clock delay held)", "cols": "FPGA IDELAY tap"});
        }
        _ => {
            // Driver's own analysis (cross-check; dwell is the driver's).
            iio::debug_set(&adi.phy.dev, "bist_timing_analysis", "1")?;
            let text = iio::debug_get(&adi.phy.dev, "bist_timing_analysis")?;
            let grid = eye::parse_timing_grid(&text);
            let s = eye::summary_2d(&grid, Some((clk_c as usize, data_c as usize)));
            out["grid"] = s["grid"].clone();
            out["summary"] = s;
            out["raw"] = json!(text);
            adi.phy.spi_write(SPI_RX_DELAY, reg6)?;
        }
    }
    let after = adi.check(dwell)?;
    drop(adi);
    let restored = cleanup.run();
    out["points"] = json!(points);
    out["final_check"] = after.to_json();
    out["restored"] = json!(restored);
    out["seconds"] = json!(round3(t0.elapsed().as_secs_f64()));
    Ok(out)
}

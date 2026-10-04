//! `txlink --mode ad9361-loopback|fpga-loopback [--sweep] [--dwell-ms D] [--intervals N]`
//!
//! ad9361-loopback: DAC PN9/PN11 (DATA_SEL 9) -> LVDS -> AD9361 digital
//! TX->RX loopback -> LVDS -> ADC PN monitors (PN_SEL 9). `--sweep` scans
//! the AD9361 TX clock/data delays (SPI 0x007) for both DAC clock
//! polarities (DAC_CLKSEL).
//!
//! fpga-loopback: the ADC path takes the DAC data internally (ADC
//! CHAN_CNTRL_3.DATA_SEL = 1). The ADI PN monitor taps the raw ADC input
//! *before* that mux, so the looped data is checked in software on a
//! capture of the P25 wideband ring (p25 image) or with the hwval ingest
//! PRBS checker in PN9/PN11 mode (hwval image).
//!
//! TX safety: TX attenuation is set to maximum before any DAC source is
//! enabled (TxGuard) and `tx off` runs on every exit path.

use super::adi_util::{Adi, Dac, PN_SEL_PN9, SPI_TX_DELAY};
use super::eyescan::arm_rx_restore;
use super::{open_core, Ctx};
use crate::access::RegAccess;
use crate::checker::pn1::{self, Prbs};
use crate::cli::Args;
use crate::err::{AResult, AgentError, Code};
use crate::eye;
use crate::iio::Ad9361;
use crate::regio::PhysMap;
use crate::rings::{self, LegacyTracker, Mapping, RingKind};
use crate::safety::{self, Cleanup, TxGuard};
use crate::sys;
use crate::util::round3;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

pub fn run(ctx: &Ctx, args: &Args) -> AResult<Value> {
    let mode = args.req("mode")?;
    let sweep = args.flag("sweep");
    let dwell = args.u64_or("dwell-ms", 10)?;
    let intervals = args.u64_or("intervals", 20)?.max(1);
    let auto = args.flag("auto-maint");
    let ignore = args.flag("ignore-maint");
    args.finish()?;
    if mode != "ad9361-loopback" && mode != "fpga-loopback" {
        return Err(AgentError::new(Code::Usage, "--mode ad9361-loopback|fpga-loopback"));
    }
    let mut warnings = Vec::new();
    let _maint = safety::require_maintenance(auto, ignore, &mut warnings)?;
    for w in warnings {
        ctx.warn(w);
    }
    let t0 = Instant::now();
    // Order matters: the TX guard is created first so it is dropped last
    // (tx_off after every other restore).
    let _tx = TxGuard::arm(ctx.maps())?;
    let mut cleanup = Cleanup::new();
    let mut adi = Adi::open(ctx)?;
    let saved = arm_rx_restore(ctx, &mut adi, &mut cleanup, None)?;
    let mut dac = Dac::open(ctx)?;
    let clksel0 = dac.clksel()?;
    cleanup.push("restore DAC_CLKSEL", move || {
        let (core, pm) = open_core(ctx, "adi_dac", true)?;
        let mut a = RegAccess::new(core, pm);
        a.write("DAC_CLKSEL", clksel0)?;
        Ok(json!(clksel0))
    });
    let mut out = json!({"mode": mode, "dwell_ms": dwell, "saved": saved, "dac_clksel": clksel0,
                         "tx_atten_db": adi.phy.tx_atten_db()});
    if mode == "ad9361-loopback" {
        let reg7 = adi.phy.spi_read(SPI_TX_DELAY)?;
        cleanup.push("restore AD9361 0x007", move || {
            Ad9361::open()?.spi_write(SPI_TX_DELAY, reg7)?;
            Ok(json!(format!("0x{reg7:02X}")))
        });
        cleanup.push("loopback=0", || {
            Ad9361::open()?.loopback(0)?;
            Ok(json!(0))
        });
        adi.phy.loopback(1)?;
        for ch in 0..2 {
            dac.data_sel(ch, 9)?;
            adi.set_pn_sel(ch, PN_SEL_PN9)?;
        }
        dac.sync()?;
        safety::sleep_ms(10)?;
        // Error intervals at the chosen (current) delay.
        let mut errs = 0u64;
        let mut first = None;
        for _ in 0..intervals {
            let c = adi.check(dwell)?;
            if !c.pass {
                errs += 1;
                first.get_or_insert(c.to_json());
            }
        }
        out["chosen_delay"] = json!(format!("0x{reg7:02X}"));
        out["errors_at_chosen"] = json!(errs);
        out["errors"] = json!(errs);
        out["intervals"] = json!(intervals);
        out["first_error"] = json!(first);
        if sweep {
            let mut list = Vec::new();
            let mut grids = serde_json::Map::new();
            for cs in [0u32, 1] {
                dac.set_clksel(cs)?;
                let mut grid = vec![vec![false; 16]; 16];
                for clk in 0..16u32 {
                    for data in 0..16u32 {
                        adi.phy.spi_write(SPI_TX_DELAY, (clk << 4) | data)?;
                        safety::sleep_ms(1)?;
                        let c = adi.check(dwell)?;
                        grid[clk as usize][data as usize] = c.pass;
                        list.push(json!({"delay": format!("0x{:02X}", (clk << 4) | data), "clk": clk,
                                         "data": data, "clksel": cs, "errors": (!c.pass) as u32}));
                    }
                }
                let chosen = if cs == clksel0 { Some(((reg7 >> 4) as usize, (reg7 & 0xF) as usize)) } else { None };
                grids.insert(format!("clksel{cs}"), eye::summary_2d(&grid, chosen));
            }
            dac.set_clksel(clksel0)?;
            adi.phy.spi_write(SPI_TX_DELAY, reg7)?;
            out["sweep"] = json!(list);
            out["grids"] = Value::Object(grids);
            out["axes"] = json!({"rows": "AD9361 0x007[7:4] TX clock delay", "cols": "AD9361 0x007[3:0] TX data delay"});
        }
    } else {
        // FPGA-internal loopback: ADC DATA_SEL = 1 (DAC data), DAC PN.
        for ch in 0..2 {
            adi.set_data_sel(ch, 1)?;
            dac.data_sel(ch, 9)?;
        }
        dac.sync()?;
        safety::sleep_ms(10)?;
        let uios = sys::uio_names();
        if uios.iter().any(|u| u == "p25-core") {
            out["verify"] = verify_p25_ring(ctx)?;
        } else if uios.iter().any(|u| u == "hwval-core") {
            out["verify"] = verify_hwval_ingest(ctx, dwell * intervals)?;
        } else {
            return Err(AgentError::new(
                Code::Unsupported,
                "fpga-loopback needs the P25 wideband ring or the hwval ingest checker (ADI PN monitor taps pre-loopback data)",
            ));
        }
        out["errors"] = out["verify"]["errors"].clone();
        out["samples_checked"] = out["verify"]["samples_checked"].clone();
    }
    drop(adi);
    drop(dac);
    let restored = cleanup.run();
    out["restored"] = json!(restored);
    out["pass"] = json!(out["errors"].as_u64() == Some(0));
    out["seconds"] = json!(round3(t0.elapsed().as_secs_f64()));
    Ok(out)
}

/// Captures one fresh wideband sub-buffer (uncached) and checks the PN9 (I)
/// and PN11 (Q) streams in software.
fn verify_p25_ring(ctx: &Ctx) -> AResult<Value> {
    let (core, pm) = open_core(ctx, "p25", true)?;
    let mut a = RegAccess::new(core, pm);
    if a.gate_asserted()? {
        return Err(AgentError::new(Code::Precondition, "p25 sdr_reset = 1: start the scanner once or use `ring check --release-reset`"));
    }
    let prev = a.read("wideband_iq_dma_control")? & 1;
    if prev == 0 {
        a.write("wideband_iq_dma_control", 1)?;
    }
    let def = rings::ring_def(RingKind::P25Wideband, None, None);
    let map = Mapping::open(&def, "uncached")?;
    let mut tr = LegacyTracker::new(def.num_subbufs as u32);
    let poll = |a: &mut RegAccess<PhysMap>| -> AResult<u32> { Ok((a.read_se("wideband_iq_dma_status")? >> 1) & 0xF) };
    tr.advance(poll(&mut a)?);
    let mut got = 0;
    let mut idx = 0;
    let end = Instant::now() + Duration::from_secs(3);
    while got < 2 && Instant::now() < end {
        let r = tr.advance(poll(&mut a)?);
        if let Some(last) = r.last() {
            idx = last.0;
            got += r.len();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let res = if got >= 2 {
        let mut buf = vec![0u8; def.subbuf_bytes];
        map.copy_subbuf(idx as usize, def.subbuf_bytes, &mut buf)?;
        let n = buf.len() / 4;
        let mut i_s = Vec::with_capacity(n);
        let mut q_s = Vec::with_capacity(n);
        for c in buf.chunks_exact(4) {
            let v = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            i_s.push(v & 0xFFF);
            q_s.push((v >> 16) & 0xFFF);
        }
        let ri = pn1::check_stream(&i_s, Prbs::P09);
        let rq = pn1::check_stream(&q_s, Prbs::P11);
        json!({"method": "p25 wideband ring, software pn1fn check", "subbuf": idx,
               "samples_checked": n, "errors": ri.errors + rq.errors,
               "i": {"pairs": ri.pairs_checked, "errors": ri.errors, "alignment": ri.alignment},
               "q": {"pairs": rq.pairs_checked, "errors": rq.errors, "alignment": rq.alignment}})
    } else {
        json!({"method": "p25 wideband ring", "errors": null, "error": "no sub-buffer completed within 3 s"})
    };
    if prev == 0 {
        a.write("wideband_iq_dma_control", 0)?;
    }
    Ok(res)
}

/// hwval ingest PRBS checker in PN9/PN11 mode.
fn verify_hwval_ingest(ctx: &Ctx, ms: u64) -> AResult<Value> {
    super::hwval::implicit_init(ctx, false)?;
    let (core, pm) = open_core(ctx, "hwval", true)?;
    let mut a = RegAccess::new(core, pm);
    let old = a.read("INGEST_CTRL")?;
    a.write("INGEST_CTRL", (old & !0b110) | 0b111)?;
    a.write("INGEST_CMD", 1)?;
    safety::sleep_ms(ms.max(10))?;
    a.refresh();
    let checked_lo = a.read("PRBS_CHECKED_LO")? as u64;
    let checked_hi = a.read("PRBS_CHECKED_HI")? as u64;
    let errors = a.read("PRBS_ERRORS")?;
    let oos = a.read("PRBS_OOS_EVENTS")?;
    let st = a.read("PRBS_STATUS")?;
    a.write("INGEST_CTRL", old)?;
    Ok(json!({"method": "hwval ingest PRBS (prbs_mode = 1, PN9/PN11)",
              "samples_checked": (checked_hi << 32) | checked_lo, "errors": errors,
              "oos_events": oos, "in_sync": st & 1, "snapshot": a.snapshot_json()}))
}

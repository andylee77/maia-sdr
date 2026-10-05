//! The model's tests: its stages against their definitions or 081's code, the whole model through
//! 081's test modulator, and, offline, the gate on unit A's captures against 081's receiver.

use rustfft::num_complex::Complex32;

use super::*;
use crate::protocol::atsc::demod::{rrc, SYMBOL_RATE};
use crate::protocol::atsc::vsb::FIELD_SEGMENTS;
use crate::protocol::atsc::fec::trellis;
use crate::protocol::atsc::receiver::tests::station_packets;
use crate::protocol::atsc::receiver::{identify, modulate};

const CONFIG: Config = Config { widths: Widths::WIDE, loops: Loops::DEFAULT, schedule: Schedule::DEFAULT };

/// Test IQ (in levels) at the AD9361's scale: a level is `scale` steps of its 12 bits.
fn as_adc(iq: &[Complex32], scale: f32) -> Vec<[i16; 2]> {
    iq.iter().map(|z| [(z.re * scale).round().clamp(-2048.0, 2047.0) as i16, (z.im * scale).round().clamp(-2048.0, 2047.0) as i16]).collect()
}

fn names(p: &crate::protocol::atsc::psip::Psip) -> Vec<String> {
    p.channels.iter().map(|c| format!("{}.{} {}", c.major, c.minor, c.short_name)).collect()
}

#[test]
fn requantize_rounds_half_up_and_saturates() {
    assert_eq!(requantize(5, 1, 8), 3);
    assert_eq!(requantize(-5, 1, 8), -2);
    assert_eq!(requantize(1000, 2, 8), 127);
    assert_eq!(requantize(-1000, 2, 8), -128);
    assert_eq!(requantize(-7, 0, 4), -7);
}

#[test]
fn the_matched_filter_is_its_definition() {
    let (fs, w) = (10e6, Widths::WIDE);
    let mf = front::MatchedFilter::new(fs, &w);
    assert_eq!(mf.taps, 40);
    let x: Vec<[i16; 2]> = (0..400).map(|i| [((i * 7919) % 2001) as i16 - 1000, ((i * 104_729) % 1999) as i16 - 999]).collect();
    let rb = SYMBOL_RATE / 2.0;
    let scale = f64::from(1u32 << mf.frac);
    for k in [0u64, 1, 31, 32, 33, 63] {
        let pos = (150u64 << 32) + (k << (32 - front::PHASE_BITS));
        let tau = pos as f64 / 4_294_967_296.0;
        let i = tau as usize;
        let (mut re, mut im) = (0f64, 0f64);
        for n in i + 1 - mf.half..=i + mf.half {
            let h = rrc((tau - n as f64) / fs * rb) * rb / fs;
            re += h * f64::from(x[n][0]);
            im += h * f64::from(x[n][1]);
        }
        let z = mf.at(&x, pos).unwrap();
        assert!((f64::from(z[0]) / scale - re).abs() < 0.01 && (f64::from(z[1]) / scale - im).abs() < 0.01, "{k}: {z:?} {re} {im}");
    }
    // Rounding to the next sample's row 0.
    let next = (150u64 << 32) + (63 << (32 - front::PHASE_BITS)) + (1 << (31 - front::PHASE_BITS));
    assert_eq!(mf.at(&x, next), mf.at(&x, 151 << 32));
    assert_eq!(mf.at(&x, ((mf.half as u64 - 2) << 32) + (1 << 31)), None);
}

#[test]
fn the_turn_takes_quarter_turns_exactly() {
    let t = front::Turn::new(&Widths::WIDE);
    let z = [1 << 20, 3 << 18];
    let u = t.apply(z, 0);
    assert!((u[0] - z[0]).abs() <= 1 && (u[1] - z[1]).abs() <= 1, "{u:?}");
    let q = t.apply(z, 1 << 30);
    assert!((q[0] + z[1]).abs() <= 1 && (q[1] - z[0]).abs() <= 1, "{q:?}");
}

#[test]
fn the_viterbi_registers_decide_as_081s_traceback() {
    let mut x = 7u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let pairs: Vec<u8> = (0..20_000).map(|_| (next() & 3) as u8).collect();
    let mut enc = trellis::Encoder::default();
    let levels: Vec<f32> = pairs.iter().map(|&p| crate::protocol::atsc::vsb::level(enc.symbol(p >> 1, p & 1))).collect();
    for noise in [0.0f32, 0.5] {
        let soft: Vec<i32> = levels.iter().map(|&l| trellis::soft(l + noise * (((next() >> 11) as f64 / (1u64 << 53) as f64) as f32 * 2.0 - 1.0) * 1.7)).collect();
        let whole = trellis::decode(soft.len(), soft.iter().copied());
        let depth = 32;
        let mut v = decoder::Viterbi::default();
        let regs: Vec<u8> = soft.iter().filter_map(|&r| v.step(i64::from(r), 16, depth).0).collect();
        assert_eq!(regs.len(), soft.len() + 1 - depth);
        let differ = regs.iter().zip(&whole).filter(|(a, b)| a != b).count();
        if noise == 0.0 {
            assert_eq!(&regs[..], &whole[..regs.len()]);
            assert_eq!(&regs[..], &pairs[..regs.len()]);
        } else {
            assert!(differ * 1000 < regs.len(), "{differ} of {} differ from 081's", regs.len());
        }
    }
}

#[test]
fn the_deinterleaver_fifos_are_081s_index_map() {
    let n = 40 * decoder::DEINTERLEAVER_DELAY;
    let input: Vec<u8> = (0..n).map(|t| (t * 31 + t / 7) as u8).collect();
    let mut d = decoder::Deinterleaver::default();
    let out: Vec<u8> = input.iter().filter_map(|&b| d.push(b)).collect();
    assert_eq!(out.len(), n - decoder::DEINTERLEAVER_DELAY);
    let branches = crate::protocol::atsc::fec::BRANCHES;
    for (m, &b) in out.iter().enumerate() {
        assert_eq!(b, input[m + 4 * branches * (m % branches)], "{m}");
    }
}

#[test]
fn packets_carry_their_codewords() {
    let cws: Vec<decoder::Codeword> = (0..40)
        .map(|i| decoder::Codeword { field: (305 + i) / 312, index: (305 + i) % 312, bytes: [i as u8; 207], flag: i % 3 == 0 })
        .collect();
    let fields: Vec<front::Field> = (0..2).map(|f| front::Field { start: f * FIELD, found: true, inverted: f == 1 }).collect();
    let p = packets::pack(&cws, &fields, &front::Output::default(), &[], &Widths::WIDE, FIELD / 2);
    assert_eq!(p.len(), 3);
    let read: Vec<packets::Read> = p.iter().flat_map(|p| packets::unpack(p).unwrap().1).collect();
    // The timing loop tracks from half a field in: the first field's packets say not yet.
    let flags: Vec<u8> = p.iter().map(|p| packets::unpack(p).unwrap().0.flags).collect();
    assert_eq!(flags, [0b0110, 0b1110, 0b1110]);
    for (r, c) in read.iter().zip(&cws) {
        assert_eq!((r.0 as usize, usize::from(r.1), r.2, r.3), (c.field, c.index, c.bytes, c.flag));
    }
    let mut bad = p[1];
    bad[100] ^= 1;
    assert!(packets::unpack(&bad).is_none());
}

#[test]
fn a_modulated_station_names_itself_through_the_model() {
    let iq = as_adc(&modulate(&station_packets(6), 10e6, 0.6), 80.0);
    let run = run(&iq, 10e6, 0.0, &CONFIG).expect("field syncs");
    let st = ps::station(&run.packets);
    assert!(run.mer_db() > 20.0, "MER {}", run.mer_db());
    assert_eq!((st.failed, st.psip.tsid), (0, Some(601)));
    assert_eq!(names(&st.psip), ["4.1 WJXT-HD"]);
}

#[test]
fn a_station_off_centre_and_off_clock_still_decodes_through_the_model() {
    let fs = 10e6;
    let iq: Vec<Complex32> = modulate(&station_packets(6), fs, 0.6)
        .into_iter()
        .enumerate()
        .map(|(k, s)| s * Complex32::from_polar(1.0, (2.0 * std::f64::consts::PI * 40e3 * k as f64 / fs) as f32))
        .collect();
    let run = run(&as_adc(&iq, 80.0), fs * (1.0 + 2e-6), 40e3, &CONFIG).expect("field syncs");
    let st = ps::station(&run.packets);
    assert_eq!((st.failed, st.psip.tsid), (0, Some(601)), "MER {}", run.mer_db());
}

/// The HDHomeRun's programmes: (TSID, "major.minor name").
fn lineup(path: &std::path::Path) -> Vec<(u16, String)> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|p| Some((p.get("TransportStreamID")?.as_u64()? as u16, format!("{} {}", p.get("GuideNumber")?.as_str()?, p.get("GuideName")?.as_str()?))))
                .collect()
        })
        .unwrap_or_default()
}

/// The gate (079, bake D): on unit A's captures (`ATSC_CAPTURE_DIR`, 081's), the model names
/// every station 081's receiver names, and names nothing the HDHomeRun's lineup (beside the
/// captures, `../hdhomerun_lineup_tuning_2026-10-03.json`) does not have on that TSID. Prints
/// both side by side. `ATSC_MODEL_WIDTHS=chosen` runs the chosen widths, else the wide ones.
#[test]
fn the_model_names_every_station_081_names() {
    let Ok(dir) = std::env::var("ATSC_CAPTURE_DIR") else {
        eprintln!("ATSC_CAPTURE_DIR not set: skipped");
        return;
    };
    let dir = std::path::Path::new(&dir);
    let cfg = Config { widths: widths_from_env(), ..CONFIG };
    let lineup = lineup(&dir.join("../hdhomerun_lineup_tuning_2026-10-03.json"));
    let fs = 10e6 * (1.0 - 0.6967e-6);
    let mut files: Vec<_> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "cs16")).collect();
    files.sort();
    let results: Vec<(String, bool, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = files
            .iter()
            .map(|path| {
                let (lineup, cfg) = (&lineup, &cfg);
                s.spawn(move || {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    let iq = study::read_cs16(path, 8_000_000);
                    let ref_ = identify(&iq, fs, 0.0);
                    let x: Vec<[i16; 2]> = iq.iter().map(|z| [z.re as i16, z.im as i16]).collect();
                    drop(iq);
                    let model = run(&x, fs, 0.0, cfg);
                    let st = model.as_ref().map(|r| ps::station(&r.packets));
                    let f32_names = ref_.as_ref().map(|r| names(&r.psip)).unwrap_or_default();
                    let model_names = st.as_ref().map(|s| names(&s.psip)).unwrap_or_default();
                    let model_tsid = st.as_ref().and_then(|s| s.psip.tsid);
                    let lost: Vec<&String> = f32_names.iter().filter(|n| !model_names.contains(n)).collect();
                    let false_names: Vec<&String> = model_names
                        .iter()
                        .filter(|n| !f32_names.contains(n) && !(model_tsid.is_some_and(|t| lineup.iter().any(|(lt, ln)| *lt == t && ln == *n))))
                        .collect();
                    let (with_taps, failed_taps) = model.as_ref().map_or((0, 0), failed_with_taps);
                    let line = format!(
                        "{name:14} f32: MER {:5.1} dB, {:5} packets, {:5} corrected, {:5} failed, TSID {:?} {:?}\n{:14} model: MER {:5.1} dB, {:5} packets, {:5} corrected, {:5} failed ({failed_taps} of {with_taps} after the first taps), TSID {:?} {:?}; solves {}, fields {}, max tap {:.2}, max metric {}",
                        ref_.as_ref().map(|r| r.mer_db).unwrap_or(f32::NAN),
                        ref_.as_ref().map(|r| r.packets).unwrap_or(0),
                        ref_.as_ref().map(|r| r.corrected).unwrap_or(0),
                        ref_.as_ref().map(|r| r.failed).unwrap_or(0),
                        ref_.as_ref().and_then(|r| r.psip.tsid),
                        f32_names,
                        "",
                        model.as_ref().map(|r| r.mer_db()).unwrap_or(f64::NAN),
                        st.as_ref().map(|s| s.packets).unwrap_or(0),
                        st.as_ref().map(|s| s.corrected).unwrap_or(0),
                        st.as_ref().map(|s| s.failed).unwrap_or(0),
                        model_tsid,
                        model_names,
                        model.as_ref().map(|r| r.solves.len()).unwrap_or(0),
                        model.as_ref().map(|r| r.fields.len()).unwrap_or(0),
                        model.as_ref().map(|r| r.solves.iter().fold(0f32, |m, s| m.max(s.max_tap))).unwrap_or(0.0),
                        model.as_ref().map(|r| r.max_metric).unwrap_or(0),
                    );
                    (line, lost.is_empty() && false_names.is_empty(), format!("{name}: lost {lost:?}, false {false_names:?}"))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(!results.is_empty(), "no captures in {dir:?}");
    for (line, _, _) in &results {
        eprintln!("{line}");
    }
    let failed: Vec<&String> = results.iter().filter(|r| !r.1).map(|r| &r.2).collect();
    assert!(failed.is_empty(), "{failed:#?}");
}

/// The front end's output from 081's receiver: its symbols before the equalizer at the model's
/// scale, and its field syncs.
fn front_from_081(iq: &[Complex32], fs: f64, w: &Widths) -> Option<front::Output> {
    let u = crate::protocol::atsc::demod::unequalized(iq, fs, 0.0)?;
    let scale = f64::from(1u32 << (w.y - 5));
    let max = f64::from((1u32 << (w.y - 1)) - 1);
    let y = u.symbols.iter().map(|&v| (f64::from(v) * scale).round().clamp(-max - 1.0, max) as i32).collect();
    let fields = u.inverted.iter().enumerate().map(|(f, &inverted)| front::Field { start: (u.first_sync + f * FIELD_SEGMENTS) * crate::protocol::atsc::vsb::SEGMENT, found: true, inverted }).collect();
    Some(front::Output { y, fields, ..Default::default() })
}

/// Offline: one capture (`ATSC_DIAG_FILE`) through the model, field by field, with the loops'
/// states; and the model's equalizer and decoder on 081's front end.
#[test]
#[ignore]
fn diagnose() {
    let Ok(path) = std::env::var("ATSC_DIAG_FILE") else { return };
    let fs = 10e6 * (1.0 - 0.6967e-6);
    let iq = study::read_cs16(std::path::Path::new(&path), 8_000_000);
    let x: Vec<[i16; 2]> = iq.iter().map(|z| [z.re as i16, z.im as i16]).collect();
    let cfg = Config { loops: loops_from_env(), ..CONFIG };
    eprintln!("{:?}", cfg.loops);
    let f = front::run(&x, fs, 0.0, &cfg, 0..0);
    let r = back(&f, &cfg);
    match &r {
        None => {
            eprintln!("model: no fields; segments counted from {:?}, {} segments", f.first_segment, f.segments.len());
            let mut pn: Vec<i64> = f.segments.iter().map(|s| s.pn511).collect();
            let syncs: Vec<i64> = f.segments.iter().map(|s| s.sync).collect();
            eprintln!("  sync correlation mean {:.0} (perfect {})", syncs.iter().sum::<i64>() as f64 / syncs.len().max(1) as f64, 4 * 5 * 128);
            pn.sort_unstable();
            eprintln!("  PN511 correlation, highest: {:?} (threshold {})", &pn[pn.len().saturating_sub(12)..], 511 * 5 * 128 / 2);
            for s in f.segments.iter().step_by(256).take(12) {
                eprintln!("  segment at {}: freq {:+.1} Hz, rate {:+.3} ppm, gain {}, dc {}, sync {}, timing {}", s.start, s.freq as f64 / 4_294_967_296.0 * SYMBOL_RATE, s.rate as f64 / 4_294_967_296.0 / (fs / SYMBOL_RATE) * 1e6, s.gain, s.dc, s.sync, s.timing);
            }
        }
        Some(r) => {
            eprintln!("model: MER {:.1} dB, first segment {:?}, {} segments, fields at {:?}", r.mer_db(), f.first_segment, f.segments.len(), f.fields.iter().take(3).map(|f| (f.start, f.found)).collect::<Vec<_>>());
            for (i, f) in r.fields.iter().enumerate() {
                eprintln!("  field {i}: MER {:.1} dB, taps {}", f.mer_db(), f.taps);
            }
            let sym = SYMBOL_RATE;
            // The segments' instants against the straight line through them (tracking only), in
            // symbols, and the carrier loop's frequency spread.
            let from = cfg.loops.tracking_from();
            let segs = &f.segments[from.min(f.segments.len())..];
            if segs.len() > 2 {
                let pts: Vec<(f64, f64)> = segs.iter().enumerate().map(|(i, s)| (i as f64, s.pos as f64 / 4_294_967_296.0)).collect();
                let n = pts.len() as f64;
                let (mx, my) = (pts.iter().map(|p| p.0).sum::<f64>() / n, pts.iter().map(|p| p.1).sum::<f64>() / n);
                let b = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum::<f64>() / pts.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
                let per_symbol = fs / sym;
                let dev: Vec<f64> = pts.iter().map(|p| (p.1 - (my + b * (p.0 - mx))) / per_symbol).collect();
                let rms = (dev.iter().map(|d| d * d).sum::<f64>() / n).sqrt();
                let max = dev.iter().fold(0f64, |m, d| m.max(d.abs()));
                let step: Vec<f64> = dev.windows(2).map(|w| w[1] - w[0]).collect();
                let step_rms = (step.iter().map(|d| d * d).sum::<f64>() / step.len() as f64).sqrt();
                let f: Vec<f64> = segs.iter().map(|s| s.freq as f64 / 4_294_967_296.0 * sym).collect();
                let fm = f.iter().sum::<f64>() / f.len() as f64;
                let fsd = (f.iter().map(|v| (v - fm).powi(2)).sum::<f64>() / f.len() as f64).sqrt();
                eprintln!("timing against a line: rms {rms:.4} symbols, max {max:.4}, segment to segment {step_rms:.5}; clock {:+.3} ppm; carrier {fm:+.1} Hz ± {fsd:.1}", (b / 832.0 / per_symbol - 1.0) * 1e6);
                for (i, chunk) in dev.chunks(313).enumerate() {
                    let r = (chunk.iter().map(|d| d * d).sum::<f64>() / chunk.len() as f64).sqrt();
                    eprint!("{r:.4} ");
                    if i % 10 == 9 {
                        eprintln!();
                    }
                }
                eprintln!();
            }
            for s in f.segments.iter().step_by(64) {
                eprintln!(
                    "  segment at {}: freq {:+.1} Hz, rate {:+.3} ppm, gain {}, dc {}, sync {}, timing {}, pn511 {}",
                    s.start,
                    s.freq as f64 / 4_294_967_296.0 * sym,
                    s.rate as f64 / 4_294_967_296.0 / (fs / sym) * 1e6,
                    s.gain,
                    s.dc,
                    s.sync,
                    s.timing,
                    s.pn511
                );
            }
        }
    }
    if let Some(f) = front_from_081(&iq, fs, &CONFIG.widths) {
        eprintln!("081's fields at {:?}", f.fields.iter().take(2).map(|f| f.start).collect::<Vec<_>>());
        let b = back(&f, &cfg).unwrap();
        eprintln!("081's front, the model's equalizer and decoder: MER {:.1} dB, {} solves", b.mer_db(), b.solves.len());
        for (i, f) in b.fields.iter().enumerate() {
            eprintln!("  field {i}: MER {:.1} dB, taps {}", f.mer_db(), f.taps);
        }
    }
}

/// `ATSC_LOOPS=ckp,cki,tkp,tki,gain_k,dc_k`: other loop gains (the gears as by default).
fn loops_from_env() -> Loops {
    let Ok(s) = std::env::var("ATSC_LOOPS") else { return Loops::DEFAULT };
    let v: Vec<u32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    let d = Loops::DEFAULT;
    Loops { carrier_kp: v[0], carrier_ki: v[1], timing_kp: v[2], timing_ki: v[3], gain_k: v[4], dc_k: v[5], ..d }
}

/// `ATSC_MODEL_WIDTHS=chosen`: the chosen widths; else the wide ones.
fn widths_from_env() -> Widths {
    match std::env::var("ATSC_MODEL_WIDTHS").as_deref() {
        Ok("chosen") => Widths::CHOSEN,
        _ => Widths::WIDE,
    }
}

/// The captures the widths are measured on: 081's that it decodes or nearly does, Andy's main
/// stations among them (RF 9, 13, 14, 19, 20, 21).
const SWEEP_CAPTURES: [&str; 10] =
    ["atsc_rf9_dir", "atsc_rf13_dir", "atsc_rf14_dir", "atsc_rf17_dir", "atsc_rf23_dir", "atsc_rf20_dir", "atsc_rf11_dir", "atsc_rf19", "atsc_rf20", "atsc_rf21"];

/// The narrower widths tried for each stage, the others wide.
fn sweep_variants() -> Vec<(&'static str, i64, Widths)> {
    let w = Widths::WIDE;
    let mut v: Vec<(&'static str, i64, Widths)> = Vec::new();
    for b in [10, 12, 14, 16, 18] {
        v.push(("mf_taps", b.into(), Widths { mf_taps: b, ..w }));
    }
    for b in [10, 12, 13, 14, 16] {
        v.push(("mf_out", b.into(), Widths { mf_out: b, ..w }));
    }
    for b in [10, 12, 14, 16, 18] {
        v.push(("gain", b.into(), Widths { gain: b, ..w }));
    }
    for b in [6, 8, 10, 11, 12] {
        v.push(("nco_addr", b.into(), Widths { nco_addr: b, ..w }));
    }
    for b in [8, 10, 12, 14, 16] {
        v.push(("nco_amp", b.into(), Widths { nco_amp: b, ..w }));
    }
    for b in [8, 10, 12, 14, 16] {
        v.push(("y", b.into(), Widths { y: b, ..w }));
    }
    for b in [10, 12, 14, 15, 16, 18] {
        v.push(("eq_taps", b.into(), Widths { eq_taps: b, ..w }));
    }
    for b in [8, 9, 10, 11, 12, 14] {
        v.push(("eq_out", b.into(), Widths { eq_out: b, ..w }));
    }
    for b in [1, 2, 3, 4, 5, 6] {
        v.push(("soft_frac", b.into(), Widths { soft_frac: b, ..w }));
    }
    for b in [4, 6, 8, 12, 16] {
        v.push(("soft_limit", b.into(), Widths { soft_limit: b, ..w }));
    }
    for b in [8, 12, 16, 20, 24, 28, 40, 48, 64] {
        v.push(("traceback", b as i64, Widths { traceback: b, ..w }));
    }
    v
}

/// MER, packets corrected and failed.
fn measure(r: Option<Run>) -> (f64, usize, usize) {
    r.map(|r| {
        let st = ps::station(&r.packets);
        (r.mer_db(), st.corrected, st.failed)
    })
    .unwrap_or((f64::NAN, 0, 0))
}

/// Offline: the widths, stage by stage, on 081's captures (`ATSC_CAPTURE_DIR`): each stage
/// narrowed alone from the wide widths, its MER and failed packets against the wide model's on
/// each capture. `ATSC_SWEEP=stage,...` limits it to those stages; `ATSC_SWEEP=chosen` runs the
/// chosen widths together.
#[test]
#[ignore]
fn widths_stage_by_stage() {
    let Ok(dir) = std::env::var("ATSC_CAPTURE_DIR") else { return };
    let only: Vec<String> = std::env::var("ATSC_SWEEP").map(|s| s.split(',').map(str::to_string).collect()).unwrap_or_default();
    let fs = 10e6 * (1.0 - 0.6967e-6);
    let xs: Vec<Vec<[i16; 2]>> = SWEEP_CAPTURES
        .iter()
        .map(|n| study::read_cs16(&std::path::Path::new(&dir).join(format!("{n}.cs16")), 8_000_000).iter().map(|z| [z.re as i16, z.im as i16]).collect())
        .collect();
    let wide_cfg = CONFIG;
    let fronts: Vec<front::Output> =
        std::thread::scope(|s| xs.iter().map(|x| s.spawn(|| front::run(x, fs, 0.0, &wide_cfg, 0..0))).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect());
    let wide: Vec<(f64, usize, usize)> =
        std::thread::scope(|s| fronts.iter().map(|f| s.spawn(|| measure(back(f, &wide_cfg)))).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect());
    eprintln!("captures: {SWEEP_CAPTURES:?}");
    eprintln!("wide: MER {:?}", wide.iter().map(|m| format!("{:.2}", m.0)).collect::<Vec<_>>());
    eprintln!("wide: failed {:?}", wide.iter().map(|m| m.2).collect::<Vec<_>>());
    let mut variants = sweep_variants();
    if only.iter().any(|s| s == "chosen") {
        variants = vec![("chosen", 0, Widths::CHOSEN)];
    } else if !only.is_empty() {
        variants.retain(|v| only.iter().any(|o| o == v.0));
    }
    for (stage, bits, w) in variants {
        let cfg = Config { widths: w, ..CONFIG };
        let front_same = (w.mf_taps, w.mf_out, w.gain, w.nco_addr, w.nco_amp, w.y) == (Widths::WIDE.mf_taps, Widths::WIDE.mf_out, Widths::WIDE.gain, Widths::WIDE.nco_addr, Widths::WIDE.nco_amp, Widths::WIDE.y);
        let got: Vec<(f64, usize, usize)> = std::thread::scope(|s| {
            xs.iter()
                .zip(&fronts)
                .map(|(x, f)| s.spawn(move || if front_same { measure(back(f, &cfg)) } else { measure(run(x, fs, 0.0, &cfg)) }))
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        let d: Vec<f64> = got.iter().zip(&wide).map(|(g, b)| g.0 - b.0).collect();
        let worst = d.iter().fold(f64::INFINITY, |m, &v| m.min(v));
        let mean = d.iter().sum::<f64>() / d.len() as f64;
        let failed: Vec<i64> = got.iter().zip(&wide).map(|(g, b)| g.2 as i64 - b.2 as i64).collect();
        eprintln!(
            "{stage:10} {bits:2}: MER worst {worst:+.2} mean {mean:+.2} dB {:?}; failed {:?}",
            d.iter().map(|v| format!("{v:+.2}")).collect::<Vec<_>>(),
            failed
        );
    }
}

/// Codewords from fields equalized with solved taps, and those Reed-Solomon could not correct.
fn failed_with_taps(r: &Run) -> (usize, usize) {
    let mut cw = [0u8; crate::protocol::atsc::fec::rs::N];
    r.codewords.iter().filter(|c| r.fields.get(c.field).is_some_and(|f| f.taps > 0)).fold((0, 0), |(n, failed), c| {
        cw.copy_from_slice(&c.bytes);
        let bad = c.flag && crate::protocol::atsc::fec::rs::decode(&mut cw) == crate::protocol::atsc::fec::rs::Outcome::Failed;
        (n + 1, failed + usize::from(bad))
    })
}

/// Offline: how often the equalizer's taps must be solved (079, item 11), on the long captures
/// (`ATSC_LONG_CAPTURE_DIR`, 8 s each, the chosen widths). The front end runs once; then the
/// equalizer and decoder with taps solved from snapshots 20 fields in (0.5 s, the loops long
/// settled), each from the last taps unless said: once; every 20, 10 (0.25 s), 5 and 2 fields;
/// every field (each loaded at the next field); on each field's own snapshot (a bound, not a
/// receiver); and at 0.25 s, other snapshots and solves, and solves from the field sync. Each schedule's MER, its 5th percentile and worst field over fields 21 on, and the
/// codewords Reed-Solomon could not correct there. Separately, the start: every 0.25 s from the
/// first field the timing loop tracks through. `ATSC_TAP_CSV` (a folder): each capture's MER per
/// field for each schedule.
#[test]
#[ignore]
fn how_often_the_taps_must_be_solved() {
    let Ok(dir) = std::env::var("ATSC_LONG_CAPTURE_DIR") else { return };
    let csv = std::env::var("ATSC_TAP_CSV").ok();
    let fs = 10e6 * (1.0 - 0.688e-6);
    const FROM: usize = 20;
    // The study's base: each solve from the last taps, 20 data segments, two rounds (the
    // default became 0.25 s from the field sync, 10 segments, one round, from these results).
    let d = Schedule { every: Some(10), snapshot_segments: 20, delay: 1, rounds: 2, track: true, start: FROM };
    let schedules: Vec<(&str, Schedule)> = vec![
        ("once", Schedule { every: None, ..d }),
        ("every 20 (0.5 s)", Schedule { every: Some(20), ..d }),
        ("every 10 (0.25 s)", d),
        ("every 5", Schedule { every: Some(5), ..d }),
        ("every 2", Schedule { every: Some(2), ..d }),
        ("every field", Schedule { every: Some(1), ..d }),
        ("own field (bound)", Schedule { every: Some(1), delay: 0, ..d }),
        ("0.25 s; 10 segments", Schedule { snapshot_segments: 10, ..d }),
        ("0.25 s; 40 segments", Schedule { snapshot_segments: 40, ..d }),
        ("0.25 s; 1 round", Schedule { rounds: 1, ..d }),
        ("0.25 s; from the field sync", Schedule { track: false, ..d }),
        ("0.25 s; from the field sync; 1 round", Schedule { track: false, rounds: 1, ..d }),
        ("0.25 s; from the field sync; 10 segments", Schedule { track: false, snapshot_segments: 10, ..d }),
        ("0.25 s; from the field sync; 10 segments; 1 round", Schedule { track: false, snapshot_segments: 10, rounds: 1, ..d }),
        ("0.5 s; from the field sync", Schedule { track: false, every: Some(20), ..d }),
        ("1 s; from the field sync", Schedule { track: false, every: Some(40), ..d }),
        ("start: 0.25 s from tracking", Schedule { start: 0, ..d }),
        ("start: the default from tracking", Schedule::DEFAULT),
    ];
    // `ATSC_TAP_ONLY`: the schedules whose labels start with one of these, separated by `|`.
    let schedules: Vec<(&str, Schedule)> = match std::env::var("ATSC_TAP_ONLY") {
        Ok(only) => schedules.into_iter().filter(|s| only.split('|').any(|o| s.0.starts_with(o))).collect(),
        Err(_) => schedules,
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "cs16")).collect();
    files.sort();
    for path in &files {
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let front = {
            let x = study::read_adc(path, usize::MAX);
            front::run(&x, fs, 0.0, &Config { widths: Widths::CHOSEN, ..CONFIG }, 0..0)
        };
        let runs: Vec<Option<Run>> = std::thread::scope(|s| {
            schedules
                .iter()
                .map(|(_, sch)| {
                    let front = &front;
                    s.spawn(move || back(front, &Config { widths: Widths::CHOSEN, loops: Loops::DEFAULT, schedule: *sch }))
                })
                .collect::<Vec<_>>()
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect()
        });
        eprintln!("{name}: {} fields", front.fields.len());
        let mut table: Vec<Vec<String>> = Vec::new();
        for ((label, _), r) in schedules.iter().zip(&runs) {
            let Some(r) = r else {
                eprintln!("  {label:30} no fields");
                continue;
            };
            table.push(r.fields.iter().map(|f| if f.taps > 0 { format!("{:.2}", f.mer_db()) } else { String::new() }).collect());
            let after: Vec<&FieldResult> = r.fields.iter().skip(FROM + 1).collect();
            let (s, e) = after.iter().fold((0.0, 0.0), |(s, e), f| (s + f.signal, e + f.error));
            let mut mers: Vec<f64> = after.iter().map(|f| f.mer_db()).collect();
            mers.sort_by(f64::total_cmp);
            let mut cw = [0u8; crate::protocol::atsc::fec::rs::N];
            let (n, failed) = r.codewords.iter().filter(|c| c.field > FROM).fold((0, 0), |(n, failed), c| {
                cw.copy_from_slice(&c.bytes);
                let bad = c.flag && crate::protocol::atsc::fec::rs::decode(&mut cw) == crate::protocol::atsc::fec::rs::Outcome::Failed;
                (n + 1, failed + usize::from(bad))
            });
            let mut line = format!(
                "  {label:30} MER {:5.2} dB, 5th percentile {:5.2}, worst field {:5.2}; {} solves; {failed} of {n} codewords failed",
                study::mer_db(s, e),
                mers.get(mers.len() / 20).copied().unwrap_or(f64::NAN),
                mers.first().copied().unwrap_or(f64::NAN),
                r.solves.len()
            );
            if label.starts_with("start") {
                let first = r.fields.iter().position(|f| f.taps > 0).unwrap_or(0);
                let early: Vec<String> = r.fields[first..FROM.min(r.fields.len())].iter().map(|f| format!("{:.1}", f.mer_db())).collect();
                line += &format!("; taps from field {first}, fields {first}-{}: {}", FROM - 1, early.join(" "));
            }
            eprintln!("{line}");
        }
        if let Some(dir) = &csv {
            let mut text = format!("field,{}
", schedules.iter().map(|s| s.0).collect::<Vec<_>>().join(","));
            let rows = table.iter().map(Vec::len).max().unwrap_or(0);
            for i in 0..rows {
                text += &format!("{i},{}
", table.iter().map(|t| t.get(i).cloned().unwrap_or_default()).collect::<Vec<_>>().join(","));
            }
            std::fs::write(std::path::Path::new(dir).join(format!("{name}_mer_per_field.csv")), text).unwrap();
        }
    }
}

/// A snapshot with the shape the PS takes from the PL: the segment before a field sync, the field
/// sync, `segments` data segments; random levels through a channel with echoes, and noise.
fn synthetic_snapshot(segments: usize) -> ps::Snapshot {
    use crate::protocol::atsc::vsb::{field_sync, SEGMENT_SYNC};
    let seg = crate::protocol::atsc::vsb::SEGMENT;
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let n = (segments + 2) * seg;
    let mut a: Vec<f32> = (0..n).map(|_| (2 * (next() % 8) as i32 - 7) as f32).collect();
    for s in 0..segments + 2 {
        a[s * seg..s * seg + 4].copy_from_slice(&SEGMENT_SYNC);
    }
    let known = field_sync(true);
    a[seg..seg + known.len()].copy_from_slice(&known);
    let echoes = [(0usize, 1.0f32), (7, 0.3), (19, -0.15), (60, 0.05)];
    let y = (0..n)
        .map(|k| echoes.iter().filter(|e| k >= e.0).map(|e| e.1 * a[k - e.0]).sum::<f32>() + ((next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 0.4)
        .collect();
    ps::Snapshot { y, sync: seg, inverted: true }
}

/// The tap solve's cost, timed (run on the A9 for 079's question: `--ignored`). A solve's work
/// does not depend on the data: its rows, rounds and Cholesky are fixed.
#[test]
#[ignore]
fn the_tap_solve_timed() {
    let s = synthetic_snapshot(Schedule::DEFAULT.snapshot_segments);
    let first = ps::solve(&s, None, 2).expect("taps");
    assert!((first[equalizer::PRE] - 1.0).abs() < 0.2, "{}", first[equalizer::PRE]);
    let reps = 20;
    let short = synthetic_snapshot(10);
    for (label, snap, start, rounds) in [
        ("from the field sync, 20 segments, 2 rounds", &s, None, 2),
        ("from the field sync, 20 segments, 1 round", &s, None, 1),
        ("from the field sync, 10 segments, 2 rounds", &short, None, 2),
        ("from the field sync, 10 segments, 1 round", &short, None, 1),
        ("from the last taps, 20 segments, 2 rounds", &s, Some(&first), 2),
        ("from the last taps, 20 segments, 1 round", &s, Some(&first), 1),
    ] {
        let t = std::time::Instant::now();
        for _ in 0..reps {
            std::hint::black_box(ps::solve(snap, start.cloned(), rounds));
        }
        let ms = t.elapsed().as_secs_f64() * 1e3 / f64::from(reps);
        eprintln!("{label}: {ms:.1} ms a solve; at 4 a second {:.1} % of a core's time (both cores share each solve)", ms * 4.0 / 10.0);
    }
}

/// Offline: the loop registers' and accumulators' widths at the chosen widths, the largest each
/// reached on 081's captures (`ATSC_CAPTURE_DIR`), in signed bits.
#[test]
#[ignore]
fn register_widths() {
    let Ok(dir) = std::env::var("ATSC_CAPTURE_DIR") else { return };
    let fs = 10e6 * (1.0 - 0.6967e-6);
    let cfg = Config { widths: Widths::CHOSEN, ..CONFIG };
    let all: Vec<(front::Ranges, i64, i64)> = std::thread::scope(|s| {
        SWEEP_CAPTURES
            .iter()
            .map(|n| {
                let path = std::path::Path::new(&dir).join(format!("{n}.cs16"));
                s.spawn(move || {
                    let f = front::run(&study::read_adc(&path, 8_000_000), fs, 0.0, &cfg, 0..0);
                    let r = back(&f, &cfg);
                    (f.ranges, r.as_ref().map_or(0, |r| r.max_metric), r.as_ref().map_or(0, |r| r.max_eq_acc))
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect()
    });
    let max = |f: &dyn Fn(&(front::Ranges, i64, i64)) -> i64| all.iter().map(f).max().unwrap_or(0);
    let b = front::Ranges::bits;
    eprintln!(
        "carrier frequency sum {} bits, timing rate sum {}, gain sum {}, DC sum {}, sync correlation {}, early/late {}, PN511 {}, segment search {}, equalizer accumulator {}, path metric {} (unsigned)",
        b(max(&|r| r.0.freq_sum)),
        b(max(&|r| r.0.rate_sum)),
        b(max(&|r| r.0.gain_sum)),
        b(max(&|r| r.0.dc_sum)),
        b(max(&|r| r.0.sync)),
        b(max(&|r| r.0.early_late)),
        b(max(&|r| r.0.pn511)),
        b(max(&|r| r.0.search)),
        b(max(&|r| r.2)),
        b(max(&|r| r.1)) - 1,
    );
}

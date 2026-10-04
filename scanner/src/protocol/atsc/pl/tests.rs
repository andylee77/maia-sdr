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
    let p = packets::pack(&cws, &fields, &front::Output::default(), &[], &Widths::WIDE);
    assert_eq!(p.len(), 3);
    let read: Vec<packets::Read> = p.iter().flat_map(|p| packets::unpack(p).unwrap().1).collect();
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
                    let line = format!(
                        "{name:14} f32: MER {:5.1} dB, {:5} packets, {:5} corrected, {:5} failed, TSID {:?} {:?}\n{:14} model: MER {:5.1} dB, {:5} packets, {:5} corrected, {:5} failed, TSID {:?} {:?}; solves {}, fields {}, max tap {:.2}, max metric {}",
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
    let r = run(&x, fs, 0.0, &cfg);
    match &r {
        None => {
            let f = front::run(&x, fs, 0.0, &cfg, 0..0);
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
            eprintln!("model: MER {:.1} dB, first segment {:?}, {} segments, fields at {:?}", r.mer_db(), r.front.first_segment, r.front.segments.len(), r.front.fields.iter().take(3).map(|f| (f.start, f.found)).collect::<Vec<_>>());
            for (i, f) in r.fields.iter().enumerate() {
                eprintln!("  field {i}: MER {:.1} dB, taps {}", f.mer_db(), f.taps);
            }
            let sym = SYMBOL_RATE;
            // The segments' instants against the straight line through them (tracking only), in
            // symbols, and the carrier loop's frequency spread.
            let from = cfg.loops.tracking_from();
            let segs = &r.front.segments[from.min(r.front.segments.len())..];
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
            for s in r.front.segments.iter().step_by(64) {
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
        let b = back(f, &cfg).unwrap();
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
        Ok("chosen") => Widths::WIDE,
        _ => Widths::WIDE,
    }
}

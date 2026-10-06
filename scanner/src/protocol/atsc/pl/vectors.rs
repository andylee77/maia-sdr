//! Vectors for the gateware's tests (085): the model on a capture at the chosen widths, each
//! stage's inputs and outputs written as little-endian binary files with an index (`meta.json`),
//! which `scanner-hdl/test` reads.
//!
//! | File | Record |
//! |---|---|
//! | `x.bin` | the AD9361's I, Q (i16, i16) per input sample |
//! | `mf_table.bin` | the matched filter's taps (i16), 64 rows of 40 |
//! | `sine_table.bin` | the turn's cos, sin (i16, i16), 2048 entries |
//! | `trace.bin` | per symbol: instant u64, filtered I, Q i32, gain word u32, gained I, Q i32, θ u32, turned re, im i32, DC i32, y i32 |
//! | `sides.bin` | per early or late evaluation: symbol u32, late u32, instant u64, filtered I, Q i32, θ u32, value i32 |
//! | `y.bin` | the equalizer's input (i16) per symbol |
//! | `segments.bin` | per segment counted: start u64, instant u64, carrier frequency i64, timing rate i64, gain u32, DC i32, sync i64, timing error i64, PN511 i64 |
//! | `fields.bin` | per field counted: start u64, found u32, inverted u32 |
//! | `loads.bin` | per tap set loaded: field u32, its first symbol u32, 128 taps (i16) |
//! | `eq_out.bin` | the equalizer's output (i16) per symbol from the first field's start |
//! | `decisions.bin` | per data symbol: trellis decoder u8, decision u8 (255: none yet), soft symbol i16 |
//! | `codewords.bin` | per codeword: field u32, place u32, 207 bytes, flag u8 |
//! | `results.bin` | per field: start u64, signal energy u64, error energy u64, tap sets loaded u64 |
//! | `packets.bin` | the lane ring's packets, 4096 bytes each |

use super::*;
use crate::protocol::atsc::demod::SYMBOL_RATE;
use crate::protocol::atsc::live;

fn write(dir: &std::path::Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).unwrap();
}

/// Offline: `ATSC_VECTOR_CAPTURE` (a `.cs16` capture) through the model into `ATSC_VECTOR_OUT`.
/// `ATSC_VECTOR_FIELDS`: fields to decode (default 6); `ATSC_VECTOR_PPM`: the crystal's error
/// (default −0.688, unit A on 083's captures); `ATSC_VECTOR_TRACE`: the symbols whose front-end
/// stages to keep, from the first (default all); `ATSC_VECTOR_SHIFT`: Hz to move the channel off
/// the LO, the carrier loop told so (`meta.json`'s `carrier0`, default 0).
#[test]
#[ignore]
fn vectors_for_the_hdl() {
    let (Ok(capture), Ok(out)) = (std::env::var("ATSC_VECTOR_CAPTURE"), std::env::var("ATSC_VECTOR_OUT")) else { return };
    let fields: usize = std::env::var("ATSC_VECTOR_FIELDS").ok().and_then(|v| v.parse().ok()).unwrap_or(6);
    let ppm: f64 = std::env::var("ATSC_VECTOR_PPM").ok().and_then(|v| v.parse().ok()).unwrap_or(-0.688);
    let traced: usize = std::env::var("ATSC_VECTOR_TRACE").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
    let shift: f64 = std::env::var("ATSC_VECTOR_SHIFT").ok().and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let dir = std::path::Path::new(&out);
    std::fs::create_dir_all(dir).unwrap();
    let fs = 10e6 * (1.0 + ppm * 1e-6);
    let cfg = Config { widths: Widths::CHOSEN, loops: Loops::DEFAULT, schedule: Schedule::DEFAULT };
    // The loops settle, the segment syncs are searched, a field sync is searched for and the next
    // one counted, then the fields asked for and one more for the decoder's delay.
    let symbols = front::SETTLE + 64 * crate::protocol::atsc::vsb::SEGMENT + (fields + 3) * FIELD;
    let samples = (symbols as f64 * fs / SYMBOL_RATE) as usize + 1000;
    let mut x = study::read_adc(std::path::Path::new(&capture), samples);
    if shift != 0.0 {
        // The channel moved off the LO, and the carrier loop told so (the block's `carrier0`).
        let w = 2.0 * std::f64::consts::PI * shift / 10e6;
        for (k, s) in x.iter_mut().enumerate() {
            let (sin, cos) = (w * k as f64).sin_cos();
            let (re, im) = (f64::from(s[0]), f64::from(s[1]));
            *s = [(re * cos - im * sin).round() as i16, (re * sin + im * cos).round() as i16];
        }
    }
    let f = front::run(&x, fs, shift, &cfg, 0..traced);
    let mut trace = BackTrace::default();
    let run = back_traced(&f, &cfg, &mut trace).expect("field syncs");

    write(dir, "x.bin", &x.iter().flat_map(|s| [s[0].to_le_bytes(), s[1].to_le_bytes()]).flatten().collect::<Vec<u8>>());
    let mf = front::MatchedFilter::new(fs, &cfg.widths);
    write(dir, "mf_table.bin", &mf.table.iter().flat_map(|&t| (t as i16).to_le_bytes()).collect::<Vec<u8>>());
    let turn = front::Turn::new(&cfg.widths);
    write(dir, "sine_table.bin", &turn.table.iter().flat_map(|cs| [(cs[0] as i16).to_le_bytes(), (cs[1] as i16).to_le_bytes()]).flatten().collect::<Vec<u8>>());
    let mut b = Vec::new();
    for s in &f.trace {
        b.extend_from_slice(&s.pos.to_le_bytes());
        for v in [s.filtered[0], s.filtered[1]] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&s.gain.to_le_bytes());
        for v in [s.gained[0], s.gained[1]] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&s.theta.to_le_bytes());
        for v in [s.turned[0], s.turned[1], s.dc, s.y] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    write(dir, "trace.bin", &b);
    let mut b = Vec::new();
    for s in &f.sides {
        b.extend_from_slice(&(s.n as u32).to_le_bytes());
        b.extend_from_slice(&u32::from(s.late).to_le_bytes());
        b.extend_from_slice(&s.pos.to_le_bytes());
        for v in [s.filtered[0], s.filtered[1]] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&s.theta.to_le_bytes());
        b.extend_from_slice(&(s.value as i32).to_le_bytes());
    }
    write(dir, "sides.bin", &b);
    write(dir, "y.bin", &f.y.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect::<Vec<u8>>());
    let mut b = Vec::new();
    for s in &f.segments {
        for v in [s.start as u64, s.pos] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [s.freq, s.rate] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&s.gain.to_le_bytes());
        b.extend_from_slice(&s.dc.to_le_bytes());
        for v in [s.sync, s.timing, s.pn511] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    write(dir, "segments.bin", &b);
    let mut b = Vec::new();
    for fl in &f.fields {
        b.extend_from_slice(&(fl.start as u64).to_le_bytes());
        b.extend_from_slice(&u32::from(fl.found).to_le_bytes());
        b.extend_from_slice(&u32::from(fl.inverted).to_le_bytes());
    }
    write(dir, "fields.bin", &b);
    let mut b = Vec::new();
    for (field, start, taps) in &trace.loads {
        b.extend_from_slice(&(*field as u32).to_le_bytes());
        b.extend_from_slice(&(*start as u32).to_le_bytes());
        for &t in taps {
            b.extend_from_slice(&(t as i16).to_le_bytes());
        }
    }
    write(dir, "loads.bin", &b);
    write(dir, "eq_out.bin", &trace.eq_out.iter().flat_map(|&v| (v as i16).to_le_bytes()).collect::<Vec<u8>>());
    let mut b = Vec::new();
    for &(enc, r, pair) in &trace.decisions {
        b.push(enc);
        b.push(pair.unwrap_or(255));
        b.extend_from_slice(&r.to_le_bytes());
    }
    write(dir, "decisions.bin", &b);
    let mut b = Vec::new();
    for c in &run.codewords {
        b.extend_from_slice(&(c.field as u32).to_le_bytes());
        b.extend_from_slice(&(c.index as u32).to_le_bytes());
        b.extend_from_slice(&c.bytes);
        b.push(u8::from(c.flag));
    }
    write(dir, "codewords.bin", &b);
    let mut b = Vec::new();
    for r in &run.fields {
        for v in [r.start as u64, r.signal as u64, r.error as u64, r.taps as u64] {
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    write(dir, "results.bin", &b);
    write(dir, "packets.bin", &run.packets.concat());

    let tracking = f.segments.get(cfg.loops.tracking_from()).map_or(usize::MAX, |s| s.start);
    let st = ps::station(&run.packets);
    let meta = serde_json::json!({
        "capture": capture,
        "fs": fs,
        "step0": (fs / SYMBOL_RATE * 4_294_967_296.0).round() as u64,
        "shift_hz": shift,
        "carrier0": live::carrier_word(shift),
        "settle": front::SETTLE,
        "inputs": x.len(),
        "symbols": f.y.len(),
        "first_segment": f.first_segment,
        "first_field": trace.first,
        "tracking": tracking,
        "segments": f.segments.len(),
        "fields": run.fields.len(),
        "codewords": run.codewords.len(),
        "packets": run.packets.len(),
        "solves": run.solves.iter().map(|s| [s.field, s.applied]).collect::<Vec<_>>(),
        "mer_db": run.mer_db(),
        "corrected": st.corrected,
        "failed": st.failed,
        "tsid": st.psip.tsid,
    });
    write(dir, "meta.json", serde_json::to_string_pretty(&meta).unwrap().as_bytes());
    eprintln!("{meta}");
}

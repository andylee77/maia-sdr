//! Host tests for `protocol::p25::c4fm`.

use super::*;

/// The decoder's input: the DDC output.
const INPUT_RATE_HZ: f64 = 50_000.0;

#[test]
fn symbol_slicer_and_sync_pattern() {
    assert_eq!(to_symbol(3.0 * PI / 4.0), 1);
    assert_eq!(to_symbol(PI / 4.0), 0);
    assert_eq!(to_symbol(-PI / 4.0), 2);
    assert_eq!(to_symbol(-3.0 * PI / 4.0), 3);
    let d = sync_dibits();
    // 0x5575F5FF77FF: 01 01 01 01 01 11 01 01 11 11 01 01 11 11 11 11 01 11 01 11 11 11 11 11
    assert_eq!(&d[..6], &[1, 1, 1, 1, 1, 3]);
    assert_eq!(d.iter().filter(|x| **x == 1).count(), 11);
}

/// Synthesise ideal C4FM (a sequence of dibits as 4-level FM at 4800
/// baud, raised-cosine frequency shaping) at 50 kSPS, run the decoder and
/// check that the framer finds the syncs and reads the symbols back.
#[test]
fn decodes_synthetic_c4fm() {
    // A P25 TSDU-like frame repeated: sync + NID for NAC 0x3BA / DUID 7,
    // then pseudo-random payload.
    let sync = sync_dibits();
    let nid = crate::protocol::p25::fec::bch::encode_nid(0x3BA, 0x7);
    let mut frame: Vec<u8> = sync.to_vec();
    let mut nid_dibits: Vec<u8> = (0..32).map(|k| ((nid >> (62 - 2 * k)) & 3) as u8).collect();
    nid_dibits.insert(11, 2); // status dibit
    frame.extend(&nid_dibits);
    let mut lfsr: u32 = 0xACE1;
    for _ in 0..300 {
        lfsr = (lfsr >> 1) ^ ((lfsr & 1).wrapping_neg() & 0xB400);
        frame.push((lfsr & 3) as u8);
    }
    let mut dibits = Vec::new();
    for _ in 0..40 {
        dibits.extend(&frame);
    }
    // Frequency per symbol: +1 -> +600 Hz, +3 -> +1800, -1 -> -600, -3 -> -1800.
    let dev = |d: u8| match d {
        0 => 600.0,
        1 => 1800.0,
        2 => -600.0,
        _ => -1800.0,
    };
    let fs = INPUT_RATE_HZ;
    let sps = fs / SYMBOL_RATE;
    let n = (dibits.len() as f64 * sps) as usize;
    // Raised-cosine (alpha 0.2) frequency pulses, 8 symbols long.
    let rc = |t: f64| -> f64 {
        let a = 0.2;
        let x = t;
        let sinc = if x.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
        let den = 1.0 - (2.0 * a * x).powi(2);
        let cosp = if den.abs() < 1e-9 { std::f64::consts::PI / 4.0 } else { (std::f64::consts::PI * a * x).cos() / den };
        sinc * cosp
    };
    let mut freq = vec![0.0f64; n];
    for (k, d) in dibits.iter().enumerate() {
        let center = k as f64 * sps;
        let lo = (center - 8.0 * sps).max(0.0) as usize;
        let hi = ((center + 8.0 * sps) as usize).min(n);
        for (s, f) in freq.iter_mut().enumerate().take(hi).skip(lo) {
            *f += dev(*d) * rc((s as f64 - center) / sps);
        }
    }
    let mut phase = 0.0f64;
    let mut i = Vec::with_capacity(n);
    let mut q = Vec::with_capacity(n);
    for f in &freq {
        phase += 2.0 * std::f64::consts::PI * f / fs;
        i.push((phase.cos() * 8000.0) as f32);
        q.push((phase.sin() * 8000.0) as f32);
    }
    let mut dec = C4fmDecoder::new();
    let mut framer = Framer::default();
    for (ci, cq) in i.chunks(4096).zip(q.chunks(4096)) {
        dec.process_iq(ci, cq, &mut framer);
    }
    assert!(dec.demod.stats.syncs_valid >= 30, "{:?}", dec.demod.stats);
    assert!(framer.stats.nid_ok >= 30, "nid ok {}", framer.stats.nid_ok);
}

/// Decode a recorded control channel (`P25_C4FM_WAV` = a WAV from
/// `/api/control_iq_dump`: 50 kSPS, I left, Q right) and print the TSBK
/// CRC pass rate. `cargo test c4fm_wav -- --ignored --nocapture`.
#[test]
#[ignore]
fn c4fm_wav() {
    let Ok(path) = std::env::var("P25_C4FM_WAV") else {
        eprintln!("set P25_C4FM_WAV");
        return;
    };
    let bytes = std::fs::read(&path).unwrap();
    // 44-byte canonical header, then interleaved i16 I/Q.
    let pcm: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let mut dec = C4fmDecoder::new();
    let mut framer = Framer::default();
    for chunk in pcm.chunks(8192 * 2) {
        dec.process_iq_i16(chunk, &mut framer);
    }
    let ok = framer.stats.tsbk_ok();
    let fail = framer.stats.tsbk_crc_failures;
    eprintln!(
        "{path}: symbols {} sync candidates {} valid {} nid_fail {} | framer NIDs ok {} TSBK ok {} fail {} ({:.1} %) pll {:.3}",
        dec.demod.stats.symbols, dec.demod.stats.sync_candidates, dec.demod.stats.syncs_valid, dec.demod.stats.nid_fail,
        framer.stats.nid_ok, ok, fail,
        100.0 * ok as f64 / (ok + fail).max(1) as f64, dec.demod.eq_pll,
    );
}

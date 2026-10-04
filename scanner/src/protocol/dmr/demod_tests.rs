use super::*;

/// The demodulator's input: the DDC output.
const INPUT_RATE_HZ: f64 = 50_000.0;

/// Records what the demodulator hands the framer: every dibit, and the
/// position in that stream of each sync detection.
#[derive(Default)]
struct Recorder {
    dibits: Vec<u8>,
    syncs: Vec<(usize, DmrSyncPattern)>,
}

impl DmrSymbolSink for Recorder {
    fn receive(&mut self, dibit: u8) {
        self.dibits.push(dibit);
    }
    fn sync_detected(&mut self, pattern: DmrSyncPattern) {
        self.syncs.push((self.dibits.len(), pattern));
    }
    fn is_voice_super_frame(&self) -> bool {
        false
    }
}

/// Small deterministic generator (no rand dependency).
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
}

/// Deviation of each dibit value (0 = +1, 1 = +3, 2 = -1, 3 = -3).
fn deviation_hz(d: u8) -> f64 {
    match d {
        0 => 648.0,
        1 => 1944.0,
        2 => -648.0,
        _ => -1944.0,
    }
}

/// Continuous base-station bursts: CACH(12) + payload(54) + BS data sync(24)
/// + payload(54) dibits each, random CACH and payloads. Returns the dibits.
fn bursts(count: usize, seed: u64) -> Vec<u8> {
    let mut rng = Lcg(seed);
    let sync = DmrSyncPattern::BaseStationData.to_dibits();
    let mut out = Vec::with_capacity(count * 144);
    for _ in 0..count {
        for _ in 0..66 {
            out.push((rng.next() & 3) as u8);
        }
        out.extend_from_slice(&sync);
        for _ in 0..54 {
            out.push((rng.next() & 3) as u8);
        }
    }
    out
}

/// 4FSK at 50 kSPS: each symbol's deviation held for the symbol (smoothed
/// over half a symbol), integrated to phase, plus a carrier offset.
fn modulate(dibits: &[u8], offset_hz: f64) -> Vec<i16> {
    let fs = INPUT_RATE_HZ;
    let sps = fs / SYMBOL_RATE;
    let n = (dibits.len() as f64 * sps) as usize;
    let nrz: Vec<f64> = (0..n).map(|i| deviation_hz(dibits[(i as f64 / sps) as usize])).collect();
    let k = (sps / 2.0).round() as usize;
    let mut phase = 0.0f64;
    let mut iq = Vec::with_capacity(2 * n);
    for i in 0..n {
        let lo = i.saturating_sub(k / 2);
        let hi = (i + k / 2 + 1).min(n);
        let f = nrz[lo..hi].iter().sum::<f64>() / (hi - lo) as f64;
        phase += 2.0 * std::f64::consts::PI * (f + offset_hz) / fs;
        iq.push((8000.0 * phase.cos()) as i16);
        iq.push((8000.0 * phase.sin()) as i16);
    }
    iq
}

/// Runs IQ through the demodulator in DDC-sized chunks.
fn demodulate(iq: &[i16]) -> (Recorder, DmrDemodStats) {
    let mut demod = DmrDemodulator::new();
    let mut rec = Recorder::default();
    for chunk in iq.chunks(2 * 1250) {
        demod.process_iq_i16(chunk, &mut rec);
    }
    (rec, demod.symbols.stats)
}

#[test]
fn synthetic_bursts_are_recovered() {
    let sent = bursts(200, 7);
    let (rec, stats) = demodulate(&modulate(&sent, 150.0));
    assert!(stats.coarse_syncs >= 1, "{stats:?}");
    // After the first sync every burst should be found where expected.
    assert!(rec.syncs.len() >= 195, "{} syncs, {stats:?}", rec.syncs.len());
    for w in rec.syncs.windows(2) {
        assert_eq!(w[1].0 - w[0].0, 144, "{stats:?}");
        assert_eq!(w[1].1, DmrSyncPattern::BaseStationData);
    }
    // The 144 dibits after each sync are a whole burst, CACH first: match
    // them against what was sent (find the alignment from the first one).
    let (first, _) = rec.syncs[1];
    let got = &rec.dibits[first..first + 144];
    let start = (0..sent.len() - 144).step_by(144).find(|s| sent[*s + 90..*s + 144] == got[90..144]);
    let start = start.expect("first burst not found");
    let mut errors = 0;
    let mut compared = 0;
    for (n, (pos, _)) in rec.syncs.iter().enumerate().skip(1) {
        let s = start + (n - 1) * 144;
        if *pos + 144 > rec.dibits.len() || s + 144 > sent.len() {
            break;
        }
        let got = &rec.dibits[*pos..*pos + 144];
        errors += got.iter().zip(&sent[s..s + 144]).filter(|(a, b)| a != b).count();
        compared += 144;
    }
    assert!(compared > 190 * 144);
    assert_eq!(errors, 0, "{errors} dibit errors in {compared}");
}

#[test]
fn large_carrier_offset_is_equalised() {
    // About what unit A showed at 454 MHz (0.5 kHz) plus margin.
    let sent = bursts(100, 11);
    let (rec, stats) = demodulate(&modulate(&sent, -700.0));
    assert!(rec.syncs.len() >= 95, "{} syncs, {stats:?}", rec.syncs.len());
}

// The crystal tracker reads the equaliser's offset as signal minus NCO: a carrier above the
// tuning must come out positive. On this synthetic signal it reads about 0.78 of the offset; the
// tracker still converges (each update closes most of what is left).
#[test]
fn the_learned_carrier_offset_is_signal_minus_tuning() {
    for offset in [-400.0, 400.0] {
        let mut demod = DmrDemodulator::new();
        let mut rec = Recorder::default();
        for chunk in modulate(&bursts(100, 3), offset).chunks(2 * 1250) {
            demod.process_iq_i16(chunk, &mut rec);
        }
        let learned = -f64::from(demod.symbols.equalizer_balance()) * SYMBOL_RATE / std::f64::consts::TAU;
        let share = learned / offset;
        assert!((0.6..=1.1).contains(&share), "sent {offset} Hz, learned {learned:.0} Hz");
    }
}

#[test]
fn noise_finds_no_sync() {
    let mut rng = Lcg(3);
    let iq: Vec<i16> = (0..2 * 100_000).map(|_| ((rng.next() % 4000) as i32 - 2000) as i16).collect();
    let (rec, _) = demodulate(&iq);
    assert!(rec.syncs.len() <= 1, "{} false syncs", rec.syncs.len());
}

/// Offline check against captures from unit A: `DMR_CAPTURE_DIR` holds the
/// 50 kSPS stereo i16 WAVs (`/api/v1/iq/control.wav`). Skipped without it.
#[test]
fn captured_control_channel() {
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().map_or(false, |n| n.to_string_lossy().starts_with("cc_")))
        .collect();
    files.sort();
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut demod = DmrDemodulator::new();
        let mut rec = Recorder::default();
        for chunk in iq.chunks(2 * 1250) {
            demod.process_iq_i16(chunk, &mut rec);
        }
        let s = demod.symbols.stats;
        let data = rec.syncs.iter().filter(|(_, p)| *p == DmrSyncPattern::BaseStationData).count();
        let voice = rec.syncs.iter().filter(|(_, p)| *p == DmrSyncPattern::BaseStationVoice).count();
        eprintln!(
            "{}: syncs {} (BS data {data}, BS voice {voice}), coarse {}, fine {}, losses {}, balance {:.3} rad ({:.0} Hz)",
            path.file_name().unwrap().to_string_lossy(),
            rec.syncs.len(),
            s.coarse_syncs,
            s.fine_syncs,
            s.fine_sync_losses,
            demod.symbols.equalizer_balance(),
            -demod.symbols.equalizer_balance() as f64 * SYMBOL_RATE / (2.0 * std::f64::consts::PI),
        );
        assert!(data > 1900, "{data} BS data syncs");
    }
}

/// Host time of each front-end stage over one capture (`DMR_CAPTURE_DIR`);
/// run with `--release --nocapture`.
#[test]
fn profile_stages() {
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join("cc_454368750_20260930_204706_60s.wav");
    let bytes = std::fs::read(path).unwrap();
    let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let i: Vec<f32> = iq.iter().step_by(2).map(|v| *v as f32).collect();
    let q: Vec<f32> = iq.iter().skip(1).step_by(2).map(|v| *v as f32).collect();
    let mut d = DmrDemodulator::new();
    let mut lap = std::time::Instant::now();
    let mut next = || {
        let t = lap.elapsed();
        lap = std::time::Instant::now();
        t
    };
    let (mut a, mut b, mut c, mut e) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    d.dec_i.process(&i, &mut a);
    d.dec_q.process(&q, &mut b);
    let t_dec = next();
    d.lpf_i.process(&a, &mut c);
    d.lpf_q.process(&b, &mut e);
    let t_lpf = next();
    a.clear();
    b.clear();
    d.rrc_i.process(&c, &mut a);
    d.rrc_q.process(&e, &mut b);
    let t_rrc = next();
    let mut phases = Vec::new();
    d.diff.demodulate(&a, &b, &mut phases);
    let t_diff = next();
    let mut sink = Recorder::default();
    d.symbols.receive(&phases, &mut sink);
    let t_sym = next();
    eprintln!("60 s of IQ: halfband {t_dec:?} lpf {t_lpf:?} rrc {t_rrc:?} diff {t_diff:?} symbols {t_sym:?}");
}

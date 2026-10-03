use super::*;
use crate::protocol::p25::framer::Framer;
use crate::protocol::p25::test_fixtures::{cqpsk, IQ_RATE_HZ as INPUT_RATE_HZ};

/// What the demodulator hands the framer, kept for comparison.
#[derive(Default)]
struct Recorder {
    dibits: Vec<u8>,
    syncs: Vec<usize>,
}

impl DibitSink for Recorder {
    fn push_dibit(&mut self, dibit: u8) {
        self.dibits.push(dibit);
    }
    fn sync_detected(&mut self) {
        self.syncs.push(self.dibits.len());
    }
    fn is_assembling(&self) -> bool {
        false
    }
}

/// A P25 frame (sync, NID for NAC 0x3BA / DUID 7 with its status dibit, pseudo-random payload)
/// repeated 40 times.
fn test_dibits() -> Vec<u8> {
    let sync = sync_symbols().map(|p| if p > 0.0 { 1u8 } else { 3 });
    let nid = crate::protocol::p25::fec::bch::encode_nid(0x3BA, 0x7);
    let mut frame: Vec<u8> = sync.to_vec();
    let mut nid_dibits: Vec<u8> = (0..32).map(|k| ((nid >> (62 - 2 * k)) & 3) as u8).collect();
    nid_dibits.insert(11, 2);
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
    dibits
}

#[test]
fn decodes_synthetic_cqpsk_with_carrier_offset() {
    let dibits = test_dibits();
    let (i, q) = cqpsk(&dibits, 150.0);
    let mut dec = LsmDecoder::new();
    let mut framer = Framer::default();
    for (ci, cq) in i.chunks(4096).zip(q.chunks(4096)) {
        dec.process_iq(ci, cq, &mut framer);
    }
    assert!(dec.demod.stats.sync_detections >= 30, "{:?}", dec.demod.stats);
    assert!(framer.stats.nid_ok >= 30, "nid ok {}", framer.stats.nid_ok);
    // 150 Hz is 0.196 rad per symbol; the loop holds it.
    let expected = (2.0 * std::f64::consts::PI * 150.0 / SYMBOL_RATE) as f32;
    assert!((dec.demod.pll().abs() - expected).abs() < 0.05, "pll {}", dec.demod.pll());
}

#[test]
fn block_size_does_not_change_the_output() {
    let dibits = test_dibits();
    let (i, q) = cqpsk(&dibits, -90.0);
    let run = |sizes: &[usize]| {
        let mut dec = LsmDecoder::new();
        let mut rec = Recorder::default();
        let (mut at, mut k) = (0, 0);
        while at < i.len() {
            let end = (at + sizes[k % sizes.len()]).min(i.len());
            dec.process_iq(&i[at..end], &q[at..end], &mut rec);
            at = end;
            k += 1;
        }
        rec
    };
    let whole = run(&[i.len()]);
    let pieces = run(&[4096, 777, 13, 2500, 1]);
    assert!(whole.dibits.len() > dibits.len() * 9 / 10);
    assert_eq!(whole.dibits, pieces.dibits);
    assert_eq!(whole.syncs, pieces.syncs);
}

/// Decode 50 kSPS WAVs (SDRTrunk `_baseband.wav` recordings, `/api/v1/iq/control.wav` captures)
/// for comparison with SDRTrunk's own LSM decoder (`tools/sdrtrunk_lsm_reference.py`):
/// `P25_LSM_WAVS` = a directory (every `.wav` in it), `P25_LSM_OUT` = where each file's dibits
/// go as `<stem>.bits` (SDRTrunk's packing) and one JSON line of counts per file goes to
/// `summary.jsonl`. `tools/p25_lsm_compare.py` compares them with SDRTrunk's.
/// `cargo test --release lsm_wavs -- --ignored --nocapture`
#[test]
#[ignore = "needs P25_LSM_WAVS and P25_LSM_OUT"]
fn lsm_wavs() {
    use std::io::Write;
    let (Ok(dir), Ok(out)) = (std::env::var("P25_LSM_WAVS"), std::env::var("P25_LSM_OUT")) else {
        panic!("set P25_LSM_WAVS and P25_LSM_OUT");
    };
    std::fs::create_dir_all(&out).unwrap();
    let mut summary = std::fs::File::create(format!("{out}/summary.jsonl")).unwrap();
    let mut wavs: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "wav"))
        .collect();
    wavs.sort();
    for path in wavs {
        let bytes = std::fs::read(&path).unwrap();
        let pcm: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut dec = LsmDecoder::new();
        let mut framer = Framer::default();
        let mut dibits = Vec::new();
        struct Both<'a> {
            framer: &'a mut Framer,
            dibits: &'a mut Vec<u8>,
        }
        impl DibitSink for Both<'_> {
            fn push_dibit(&mut self, dibit: u8) {
                self.dibits.push(dibit);
                self.framer.push_dibit(dibit);
            }
            fn sync_detected(&mut self) {
                self.framer.sync_detected();
            }
            fn is_assembling(&self) -> bool {
                self.framer.is_assembling()
            }
        }
        let mut sink = Both { framer: &mut framer, dibits: &mut dibits };
        for chunk in pcm.chunks(1250 * 2) {
            dec.process_iq_i16(chunk, &mut sink);
        }
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let packed: Vec<u8> =
            dibits.chunks_exact(4).map(|d| (d[0] << 6) | (d[1] << 4) | (d[2] << 2) | d[3]).collect();
        std::fs::write(format!("{out}/{stem}.bits"), packed).unwrap();
        let s = &framer.stats;
        let line = serde_json::json!({
            "file": stem,
            "seconds": pcm.len() as f64 / 2.0 / INPUT_RATE_HZ,
            "symbols": dec.demod.stats.symbols,
            "sync_detections": dec.demod.stats.sync_detections,
            "nid_ok": s.nid_ok,
            "tsbk_ok": s.tsbk_ok(),
            "tsbk_crc_failures": s.tsbk_crc_failures,
            "hdus": s.hdus,
            "ldu1s": s.ldu1s,
            "ldu2s": s.ldu2s,
            "tdus": s.tdus,
            "tdu_lcs": s.tdu_lcs,
            "pll": dec.demod.pll(),
            "sample_gain": dec.demod.sample_gain(),
        });
        writeln!(summary, "{line}").unwrap();
        eprintln!("{line}");
    }
}

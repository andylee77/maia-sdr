//! What the receivers cost on the CPU the test runs on: each FIR stage and each whole receiver,
//! in milliseconds of work per second of a lane's 50 kSPS IQ, fed in lane-ring packets (1008
//! samples). Run on the board with the image's flags:
//! `cargo test --release receiver_cost -- --ignored --nocapture`.

use std::time::{Duration, Instant};

use super::fsk4::Fir;
use super::run;
use super::taps::{HALFBAND_63, LPF_C4FM_25K, LPF_LSM_25K, RRC_TAPS_25K};
use crate::protocol::dmr::demod::{DmrDemodulator, DmrSymbolSink};
use crate::protocol::dmr::filters::{root_raised_cosine, LPF_DMR_25K};
use crate::protocol::dmr::sync::DmrSyncPattern;
use crate::protocol::p25::c4fm::{C4fmDecoder, DibitSink};
use crate::protocol::p25::lsm::LsmDecoder;

const SECONDS: usize = 20;
const RATE: usize = 50_000;
const BLOCK: usize = 1008;

struct Discard;

impl DibitSink for Discard {
    fn push_dibit(&mut self, _: u8) {}
    fn sync_detected(&mut self) {}
    fn is_assembling(&self) -> bool {
        false
    }
}

impl DmrSymbolSink for Discard {
    fn receive(&mut self, _: u8) {}
    fn sync_detected(&mut self, _: DmrSyncPattern) {}
    fn is_voice_super_frame(&self) -> bool {
        false
    }
}

/// Deterministic noise at a quarter of full scale, interleaved I, Q.
fn iq() -> Vec<i16> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    (0..2 * SECONDS * RATE)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 50) as i16 - 8192
        })
        .collect()
}

/// Milliseconds of work per second of IQ.
fn per_second(d: Duration) -> f64 {
    d.as_secs_f64() * 1e3 / SECONDS as f64
}

fn time(mut f: impl FnMut()) -> f64 {
    let t = Instant::now();
    f();
    per_second(t.elapsed())
}

#[test]
#[ignore = "timing: run on the target CPU"]
fn receiver_cost() {
    let iq = iq();
    let i: Vec<f32> = iq.iter().step_by(2).map(|&v| v as f32 / 32768.0).collect();

    // One FIR (I or Q) over a lane's samples, in packets.
    let fir = |taps: &[f32], decimate: bool, x: &[f32]| {
        let mut f = Fir::new(taps, decimate);
        let mut out = Vec::new();
        time(|| {
            for b in x.chunks(BLOCK) {
                out.clear();
                f.process(b, &mut out);
            }
        })
    };
    let half: Vec<f32> = i.iter().step_by(2).copied().collect();
    let stages = [
        ("halfband 63 /2", fir(&HALFBAND_63, true, &i)),
        ("LSM low-pass 67", fir(&LPF_LSM_25K, false, &half)),
        ("C4FM low-pass 39", fir(&LPF_C4FM_25K, false, &half)),
        ("RRC 42", fir(&RRC_TAPS_25K, false, &half)),
        ("DMR low-pass 37", fir(&LPF_DMR_25K, false, &half)),
        ("DMR RRC 57", fir(&root_raised_cosine(25_000.0 / 4800.0, 22, 5760.0 / 25_000.0), false, &half)),
    ];
    for (name, ms) in stages {
        eprintln!("{name:>18}: {ms:6.2} ms per second of IQ (one of I, Q)");
    }

    // A 128-tap filter of no symmetry (an equalizer's): the run against one sum an output.
    let taps: Vec<f32> = half[..128].to_vec();
    let mut out = vec![0.0f32; half.len() - 127];
    let run = time(|| run::plain(&taps, &half, &mut out));
    let each = time(|| out.iter_mut().enumerate().for_each(|(k, o)| *o = run::dot(&taps, &half[k..])));
    let ns = |ms: f64| ms * 1e6 * SECONDS as f64 / out.len() as f64;
    eprintln!("{:>18}: {:6.1} ns an output (one sum an output: {:.1})", "plain 128, run", ns(run), ns(each));

    let whole = |name: &str, ms: f64| eprintln!("{name:>18}: {ms:6.2} ms per second of IQ ({:.1} % of a core)", ms / 10.0);
    let mut lsm = LsmDecoder::new();
    whole("LSM receiver", time(|| iq.chunks(2 * BLOCK).for_each(|b| lsm.process_iq_i16(b, &mut Discard))));
    let mut c4fm = C4fmDecoder::new();
    whole("C4FM receiver", time(|| iq.chunks(2 * BLOCK).for_each(|b| c4fm.process_iq_i16(b, &mut Discard))));
    let mut dmr = DmrDemodulator::new();
    whole("DMR receiver", time(|| iq.chunks(2 * BLOCK).for_each(|b| dmr.process_iq_i16(b, &mut Discard))));
}

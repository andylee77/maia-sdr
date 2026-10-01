//! Unit tests for the AMBE decoder, and the reference comparison with jmbe.
//!
//! Fixtures (regenerate with the `#[ignore]` tests below and
//! `reference/AmbeReference.java`, see `reference/README.md`):
//! - `test_frames_clay_ts2.bin`: 108 frames x 9 bytes, the timeslot 2 voice
//!   of `runs/dmr/cc_454368750_20260930_205714_60s.wav` through our DMR
//!   demodulator, framer and message processor (`Voice::ambe_frames()`).
//! - `test_frames_synthetic.bin`: 54 frames for the paths the capture does
//!   not take (`synthetic_frames()`).
//! - `test_pcm_jmbe_*.f32`: jmbe 1.0.9's PCM for each, 160 f32 LE a frame.

use super::*;

const FRAMES: &[u8] = include_bytes!("test_frames_clay_ts2.bin");
const JMBE_PCM: &[u8] = include_bytes!("test_pcm_jmbe_clay_ts2.f32");
const SYNTHETIC_FRAMES: &[u8] = include_bytes!("test_frames_synthetic.bin");
const JMBE_SYNTHETIC_PCM: &[u8] = include_bytes!("test_pcm_jmbe_synthetic.f32");

/// The capture the frame fixture comes from (read-only, outside this repo).
const CAPTURE: &str =
    "C:/Users/Andy/Projects/MAIA_SDR/maia-sdr/runs/dmr/cc_454368750_20260930_205714_60s.wav";

/// `java.util.Random` seed the reference harness puts into jmbe.
const REFERENCE_NOISE_SEED: u64 = 20260930;

fn to_frames(bytes: &[u8]) -> Vec<[u8; 9]> {
    bytes
        .chunks_exact(9)
        .map(|c| c.try_into().unwrap())
        .collect()
}

fn to_pcm(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn frames() -> Vec<[u8; 9]> {
    to_frames(FRAMES)
}

/// SDRTrunk's float to PCM16 conversion, `(short)(sample * Short.MAX_VALUE)`.
fn to_i16(sample: f32) -> i16 {
    (sample * 32767.0) as i16
}

/// 8 kHz mono 16-bit WAV, samples scaled as SDRTrunk does
/// (`(short)(sample * Short.MAX_VALUE)`; NaN becomes 0).
fn write_wav(path: &std::path::Path, samples: &[f32]) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).expect("create WAV");
    let data_len = (samples.len() * 2) as u32;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&8000u32.to_le_bytes()).unwrap();
    f.write_all(&16000u32.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    for &s in samples {
        f.write_all(&to_i16(s).to_le_bytes()).unwrap();
    }
}

/// Max |error| and SNR (dB) of `ours` against `reference`, NaN == NaN.
fn compare(reference: &[f32], ours: &[f32]) -> (f32, f64, usize) {
    let (mut max, mut signal, mut noise, mut nan_mismatch) = (0.0f32, 0.0f64, 0.0f64, 0);
    for (&r, &o) in reference.iter().zip(ours) {
        if r.is_nan() || o.is_nan() {
            nan_mismatch += (r.is_nan() != o.is_nan()) as usize;
            continue;
        }
        let e = (o - r).abs();
        max = max.max(e);
        signal += (r as f64) * (r as f64);
        noise += (e as f64) * (e as f64);
    }
    let snr = if noise == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (signal / noise).log10()
    };
    (max, snr, nan_mismatch)
}

/// Decodes `frames` with a fresh decoder (NaNs kept, `decode_raw`) and
/// compares with jmbe's PCM frame by frame. Returns (max |err|, SNR dB).
fn compare_with_jmbe(name: &str, frames: &[[u8; 9]], reference: &[f32]) -> (f32, f64) {
    assert_eq!(reference.len(), frames.len() * SAMPLES_PER_FRAME);
    let mut decoder = AmbeDecoder::with_noise_seed(REFERENCE_NOISE_SEED);
    let ours: Vec<[f32; SAMPLES_PER_FRAME]> =
        frames.iter().map(|f| decoder.decode_raw(f)).collect();
    let flat: Vec<f32> = ours.iter().flatten().copied().collect();

    let (mut worst, mut exact_frames, mut i16_diffs, mut nan_total) = ((0, 0.0f32), 0, 0, 0);
    for (i, frame) in ours.iter().enumerate() {
        let r = &reference[i * SAMPLES_PER_FRAME..(i + 1) * SAMPLES_PER_FRAME];
        let (max, snr, nan_mismatch) = compare(r, frame);
        let nans = r.iter().filter(|s| s.is_nan()).count();
        let exact = r
            .iter()
            .zip(frame.iter())
            .all(|(a, b)| a.to_bits() == b.to_bits() || (a.is_nan() && b.is_nan()));
        let diffs = r
            .iter()
            .zip(frame.iter())
            .filter(|(a, b)| to_i16(**a) != to_i16(**b))
            .count();
        exact_frames += exact as usize;
        i16_diffs += diffs;
        nan_total += nans;
        eprintln!(
            "{name} frame {i:3}: max |err| {max:.3e}  SNR {snr:6.1} dB  bit-exact {exact:5}  \
             PCM16 diffs {diffs}  NaN {nans}"
        );
        assert_eq!(nan_mismatch, 0, "{name} frame {i}: NaN positions differ");
        if max > worst.1 {
            worst = (i, max);
        }
    }
    let (max, snr, _) = compare(reference, &flat);
    let rms = (reference
        .iter()
        .filter(|s| !s.is_nan())
        .map(|&s| (s as f64) * (s as f64))
        .sum::<f64>()
        / reference.len() as f64)
        .sqrt();
    eprintln!(
        "{name} overall: max |err| {max:.3e} (frame {}), SNR {snr:.1} dB, signal RMS {rms:.4}, \
         {exact_frames}/{} frames bit-exact, {i16_diffs} PCM16 samples differ, {nan_total} NaN",
        worst.0,
        frames.len()
    );
    (max, snr)
}

/// The acceptance test: jmbe 1.0.9's PCM against ours on 108 real frames.
/// Set `AMBE_WAV_DIR` to also write our decode there as a WAV.
#[test]
fn matches_jmbe_reference_pcm() {
    let (max, snr) = compare_with_jmbe("clay", &frames(), &to_pcm(JMBE_PCM));

    if let Ok(dir) = std::env::var("AMBE_WAV_DIR") {
        let dir = std::path::Path::new(&dir);
        let mut decoder = AmbeDecoder::with_noise_seed(REFERENCE_NOISE_SEED);
        let audio: Vec<f32> = frames().iter().flat_map(|f| decoder.decode(f)).collect();
        write_wav(&dir.join("ambe_clay_ts2_rust.wav"), &audio);
    }

    // Float rounding only: the unvoiced DFT (realfft vs JTransforms) and
    // libm last bits. 1e-5 is about -100 dBFS.
    assert!(max < 1e-5, "max |err| {max}");
    assert!(snr > 100.0, "SNR {snr} dB");
}

#[test]
fn matches_jmbe_on_synthetic_frames() {
    let frames = to_frames(SYNTHETIC_FRAMES);
    assert_eq!(frames, synthetic_frames());
    let (max, snr) = compare_with_jmbe("synthetic", &frames, &to_pcm(JMBE_SYNTHETIC_PCM));
    assert!(max < 1e-5, "max |err| {max}");
    assert!(snr > 100.0, "SNR {snr} dB");
}

/// Frame types, tone metadata, repeats and comfort noise on the synthetic
/// frames, as jmbe's `getAudioWithMetadata` and model parameters show them.
#[test]
fn synthetic_frame_metadata() {
    let mut decoder = AmbeDecoder::new();
    let mut infos = Vec::new();
    for f in synthetic_frames() {
        decoder.decode(&f);
        infos.push(*decoder.last_frame().unwrap());
    }
    let meta: Vec<Option<(&str, &str)>> = infos
        .iter()
        .map(|i| i.frame.tone.and_then(|t| t.metadata()))
        .collect();
    for i in 11..=13 {
        assert_eq!(meta[i], Some(("TONE", "1000.00")));
    }
    for i in 14..=16 {
        assert_eq!(meta[i], Some(("DTMF", "5")));
    }
    assert_eq!(meta[17], Some(("KNOX", "6")));
    assert_eq!(meta[19], Some(("CALL PROGRESS", "BUSY TONE")));
    assert_eq!(infos[21].frame.frame_type, FrameType::Tone);
    assert_eq!(meta[21], None);
    assert_eq!(meta[50], Some(("TONE", "156.25")));
    for i in 4..=6 {
        assert_eq!(infos[i].frame.frame_type, FrameType::Silence);
    }
    assert_eq!(
        infos[27..=31]
            .iter()
            .map(|i| i.frame.errors)
            .collect::<Vec<_>>(),
        vec![[1, 0], [2, 0], [3, 0], [1, 0], [0, 3]]
    );
    assert!(infos[33].comfort_noise && infos[34].comfort_noise);
    assert_eq!(infos[34].frame.frame_type, FrameType::Erasure);
    // 40-42 repeat, 43 is the fourth repeat (muted), 44 repeats the default.
    assert_eq!(
        infos[39..=45]
            .iter()
            .map(|i| i.repeat_count)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 1, 0]
    );
    assert!(infos[43].comfort_noise && !infos[42].comfort_noise);
}

/// Host timing: `cargo test --release jmbe::ambe::tests::decode_timing --
/// --ignored --nocapture`.
#[test]
#[ignore]
fn decode_timing() {
    let frames = frames();
    let voice = &frames[..102];
    let mut decoder = AmbeDecoder::new();
    let rounds = 200;
    let mut sink = 0.0f32;
    let mut per_frame = Vec::new();
    for _ in 0..rounds {
        let start = std::time::Instant::now();
        for f in voice {
            sink += decoder.decode(f)[80];
        }
        per_frame.push(start.elapsed().as_secs_f64() * 1e6 / voice.len() as f64);
    }
    per_frame.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let mut slowest = 0.0f64;
    for f in voice {
        let start = std::time::Instant::now();
        sink += decoder.decode(f)[80];
        slowest = slowest.max(start.elapsed().as_secs_f64() * 1e6);
    }
    eprintln!(
        "AMBE decode: median {:.1} us/frame, min {:.1}, slowest single frame {slowest:.1} us ({sink})",
        per_frame[rounds / 2],
        per_frame[0]
    );
}

/// The paths the capture does not take: tones of each group, an invalid
/// tone (repeat), silence (b0 124/125), corrected and uncorrectable errors,
/// erasure, a damaged tone b0, a run of bad frames into muting, and a
/// silence frame jmbe reads as a tone. Built from capture frames with the
/// test encoder.
fn synthetic_frames() -> Vec<[u8; 9]> {
    use super::frame::tests::{capture_frames, encode_b, encode_tone, flip};
    let cap = capture_frames();
    let bad = |i: usize| {
        let mut f = cap[i];
        for b in [0, 1, 2, 3] {
            flip(&mut f, 0, b);
            flip(&mut f, 1, b + 5);
        }
        f
    };
    let flipped = |i: usize, vector: usize, bits: &[usize]| {
        let mut f = cap[i];
        for &b in bits {
            flip(&mut f, vector, b);
        }
        f
    };
    let mut v = Vec::new();
    v.extend_from_slice(&cap[10..14]); // 0-3 voice
    v.push(encode_b(&[124, 5, 20, 100, 30, 7, 3, 9, 2])); // 4-6 silence
    v.push(encode_b(&[125, 9, 18, 300, 60, 17, 12, 4, 5]));
    v.push(encode_b(&[124, 2, 10, 200, 90, 30, 5, 12, 6]));
    v.extend_from_slice(&cap[14..18]); // 7-10 voice
    for _ in 0..3 {
        v.push(encode_tone(32, 100)); // 11-13 1000 Hz
    }
    for _ in 0..3 {
        v.push(encode_tone(133, 64)); // 14-16 DTMF 5
    }
    for _ in 0..2 {
        v.push(encode_tone(150, 90)); // 17-18 KNOX 6
    }
    for _ in 0..2 {
        v.push(encode_tone(162, 127)); // 19-20 busy
    }
    for _ in 0..2 {
        v.push(encode_tone(200, 60)); // 21-22 invalid: repeat
    }
    v.extend_from_slice(&cap[18..22]); // 23-26 voice
    v.push(flipped(22, 0, &[5])); // 27 C0 1 error
    v.push(flipped(23, 0, &[2, 17])); // 28 C0 2
    v.push(flipped(24, 0, &[1, 9, 20])); // 29 C0 3
    v.push(flipped(25, 0, &[23])); // 30 C0 parity
    v.push(flipped(26, 1, &[3, 11, 19])); // 31 C1 3
    v.push(bad(27)); // 32 repeat
    v.push(encode_b(&[122, 0, 0, 0, 0, 0, 0, 0, 0])); // 33 erasure
    v.push(encode_b(&[126, 0, 4, 0, 0, 0, 0, 0, 0])); // 34 damaged tone b0
    v.extend_from_slice(&cap[28..32]); // 35-38 voice
    for i in 0..6 {
        v.push(bad(32 + i)); // 39-44 repeats into muting
    }
    v.extend_from_slice(&cap[38..43]); // 45-49 voice
    v.push(encode_b(&[124, 24, 3, 10, 9, 3, 0, 0, 0])); // 50 silence read as tone
    v.extend_from_slice(&cap[43..46]); // 51-53 voice
    v
}

/// Regenerates `test_frames_synthetic.bin`.
#[test]
#[ignore]
fn build_synthetic_fixture() {
    let out: Vec<u8> = synthetic_frames().iter().flatten().copied().collect();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/jmbe/ambe/test_frames_synthetic.bin");
    std::fs::write(&path, &out).unwrap();
    eprintln!("{} frames -> {}", out.len() / 9, path.display());
}

/// Regenerates `test_frames_clay_ts2.bin` from the capture
/// (`AMBE_CAPTURE` overrides the path).
#[test]
#[ignore]
fn build_frame_fixture() {
    use crate::protocol::dmr::demod::DmrDemodulator;
    use crate::protocol::dmr::framer::DmrMessageFramer;
    use crate::protocol::dmr::message::processor::DmrMessageProcessor;
    use crate::protocol::dmr::message::DmrMessage;

    let capture = std::env::var("AMBE_CAPTURE").unwrap_or_else(|_| CAPTURE.to_string());
    let bytes = std::fs::read(&capture).expect("read capture");
    let iq: Vec<i16> = bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let mut demod = DmrDemodulator::new();
    let mut framer = DmrMessageFramer::default();
    let mut processor = DmrMessageProcessor::new(std::collections::HashMap::from([
        (5, 454_368_750),
        (6, 451_087_500),
    ]));
    let mut out = Vec::new();
    let mut bursts = 0;
    for chunk in iq.chunks(2 * 1250) {
        demod.process_iq_i16(chunk, &mut framer);
        let events: Vec<_> = framer.drain().collect();
        for event in events {
            for message in processor.process(event) {
                if let DmrMessage::Voice(v) = message {
                    if v.timeslot == 2 {
                        bursts += 1;
                        eprintln!("{} {} {}", v.timestamp_ms, v.class_name(), v);
                        for f in v.ambe_frames() {
                            out.extend_from_slice(&f);
                        }
                    }
                }
            }
        }
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src/jmbe/ambe/test_frames_clay_ts2.bin");
    std::fs::write(&path, &out).unwrap();
    eprintln!(
        "{bursts} bursts, {} frames -> {}",
        out.len() / 9,
        path.display()
    );
}

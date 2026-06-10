//! Offline software decode harness — feeds either a SDRTrunk `.bits`
//! symbol stream or a SDRTrunk `_baseband.wav` IQ recording through our
//! Rust LSM pipeline + framer + JMBE vocoder, writes a WAV.
//!
//! Validates the entire host-side pipeline end-to-end against SDRTrunk's
//! `.mp3` reference output for the same call, with the HDL chain
//! removed as a variable.
//!
//! Run via:
//!
//! ```sh
//! # bits-only path: SDRTrunk dibit stream -> framer -> JMBE -> WAV
//! SOFTDEC_INPUT=path/to/file.bits \
//! SOFTDEC_OUT_WAV=out.wav \
//!   cargo test --release software_decode -- --ignored --nocapture
//!
//! # full IQ path: SDRTrunk baseband WAV -> DDC -> LSM demod -> framer -> JMBE -> WAV
//! SOFTDEC_INPUT=path/to/file_baseband.wav \
//! SOFTDEC_INPUT_RATE=4000000 \
//! SOFTDEC_CENTER_HZ=860962500 \
//! SOFTDEC_TARGET_HZ=860962500 \
//! SOFTDEC_PPM=-0.4 \
//! SOFTDEC_OUT_WAV=out.wav \
//!   cargo test --release software_decode -- --ignored --nocapture
//! ```

use super::Complex32;
use super::LsmPipeline;
use super::sync::SyncEvent;
use crate::jmbe::ImbeDecoder;
use crate::protocol::p25::control_channel::{ControlChannelDecoder, VoiceHandler};
use crate::protocol::p25::voice_frame::ImbeFrameRaw;
use crate::sw_demod::{MultistageDdc, StreamingSoftwareDdc};
use crate::vocoder::SAMPLES_PER_FRAME;

/// Per-channel decode metrics carried from the IQ-path decode through
/// `write_metrics_json`. None for the .bits path (no demod ran).
struct DemodMetrics {
    n_symbols: usize,
    hard_events: Vec<SyncEvent>,
    soft_events: Vec<SyncEvent>,
    pll_trace: Vec<f32>,
    timing_trace: Vec<f32>,
    soft_symbols: Vec<Complex32>,
}

use std::sync::Arc;
use std::sync::Mutex;

// -----------------------------------------------------------------------------
// IQ source: SDRTrunk baseband WAV
// -----------------------------------------------------------------------------

/// Read a SDRTrunk-style stereo i16 WAV into interleaved Complex32 IQ.
/// Returns (samples, sample_rate).
fn read_baseband_wav(path: &str) -> (Vec<Complex32>, u32) {
    let bytes = std::fs::read(path).expect("read WAV");
    assert!(bytes.len() > 44, "WAV too short");
    assert_eq!(&bytes[0..4], b"RIFF", "not a RIFF file");
    assert_eq!(&bytes[8..12], b"WAVE", "not a WAVE file");

    // Walk chunks looking for fmt + data.
    let mut i = 12usize;
    let mut sample_rate = 0u32;
    let mut channels = 0u16;
    let mut bits = 0u16;
    let mut data_off = 0usize;
    let mut data_len = 0usize;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz = u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let payload = i + 8;
        match id {
            b"fmt " => {
                channels = u16::from_le_bytes([bytes[payload + 2], bytes[payload + 3]]);
                sample_rate = u32::from_le_bytes([
                    bytes[payload + 4], bytes[payload + 5], bytes[payload + 6], bytes[payload + 7],
                ]);
                bits = u16::from_le_bytes([bytes[payload + 14], bytes[payload + 15]]);
            }
            b"data" => {
                data_off = payload;
                data_len = sz;
            }
            _ => {}
        }
        i = payload + sz + (sz & 1); // chunks pad to even
    }
    assert_eq!(channels, 2, "expected stereo (I+Q)");
    assert_eq!(bits, 16, "expected 16-bit PCM");

    let frame_count = data_len / 4; // 2ch * 2bytes
    let mut iq = Vec::with_capacity(frame_count);
    for k in 0..frame_count {
        let p = data_off + k * 4;
        let i = i16::from_le_bytes([bytes[p], bytes[p + 1]]) as f32 / 32768.0;
        let q = i16::from_le_bytes([bytes[p + 2], bytes[p + 3]]) as f32 / 32768.0;
        iq.push(Complex32 { re: i, im: q });
    }
    (iq, sample_rate)
}

/// Read a headerless `.cs16` raw IQ capture (interleaved i16 LE I/Q,
/// the format produced by `/api/wideband_iq_capture`). Sample rate is
/// passed in because the format carries no metadata; pass 8_000_000
/// for the wideband_iq tap.
fn read_cs16_iq(path: &str, sample_rate: u32) -> (Vec<Complex32>, u32) {
    let bytes = std::fs::read(path).expect("read .cs16");
    assert_eq!(bytes.len() % 4, 0, ".cs16 size must be 4-byte multiple");
    let n = bytes.len() / 4;
    let mut iq = Vec::with_capacity(n);
    for k in 0..n {
        let i_lo = bytes[4 * k] as u16;
        let i_hi = bytes[4 * k + 1] as u16;
        let q_lo = bytes[4 * k + 2] as u16;
        let q_hi = bytes[4 * k + 3] as u16;
        let i = (((i_hi << 8) | i_lo) as i16) as f32;
        let q = (((q_hi << 8) | q_lo) as i16) as f32;
        iq.push(Complex32 { re: i, im: q });
    }
    (iq, sample_rate)
}

/// Streaming counterpart of `ddc_to_62k5` — drives `StreamingSoftwareDdc`
/// in chunks of `chunk_samples` so the offline harness exercises the
/// SAME code path the live `sw_demod_task` uses on the board. Used to
/// localise live-vs-offline differences (chunk-boundary artifacts,
/// streaming NCO drift, FIR history splices). Output is the same
/// 62.5 kSPS Complex32 stream as the batch path.
fn ddc_to_62k5_streaming(
    iq_in: &[Complex32],
    input_rate: f64,
    nco_offset_hz: f64,
    output_rate: f64,
    chunk_samples: usize,
) -> Vec<Complex32> {
    let mut ddc = StreamingSoftwareDdc::new(input_rate, output_rate, nco_offset_hz);
    let mut out: Vec<Complex32> = Vec::new();
    for chunk in iq_in.chunks(chunk_samples) {
        out.extend(ddc.process(chunk));
    }
    out
}

// MultistageDdc variant: cascade of decim-by-D stages with progressively
// sharper Kaiser LPFs. Final stage cutoff ~4.5 kHz with stopband at 12.5
// kHz, so 6.25 kHz adjacent channels (Clay/Duval band plan) are cleanly
// rejected — the single-stage 65-tap path leaks them by 1–6 dB.
fn ddc_to_62k5_multistage(
    iq_in: &[Complex32],
    input_rate: f64,
    nco_offset_hz: f64,
    output_rate: f64,
    chunk_samples: usize,
) -> Vec<Complex32> {
    let mut ddc = MultistageDdc::new(input_rate, output_rate, nco_offset_hz);
    let mut out: Vec<Complex32> = Vec::new();
    for chunk in iq_in.chunks(chunk_samples) {
        out.extend(ddc.process(chunk));
    }
    out
}

// -----------------------------------------------------------------------------
// Software DDC: NCO mixer + multi-stage decimator down to 62.5 kSPS
// -----------------------------------------------------------------------------

/// 1. Mix the input by a complex NCO so the target frequency lands at DC.
/// 2. LPF (anti-image / anti-alias) at min(input_rate, output_rate)/2.4.
/// 3. Rational rate-convert to `output_rate` using linear interpolation.
///
/// Designed for offline test use — clarity over throughput. The linear
/// interp is mild distortion at the 5/4 ratios we use here (signal is
/// <12 kHz wide vs Nyquist of 25 kHz, plenty of headroom).
fn ddc_to_62k5(
    iq_in: &[Complex32],
    input_rate: f64,
    nco_offset_hz: f64,
    output_rate: f64,
) -> Vec<Complex32> {
    // -- Stage 1: NCO mix to baseband -------------------------------------
    let two_pi = std::f64::consts::TAU;
    let phase_step = -two_pi * nco_offset_hz / input_rate;
    let mut phase = 0.0f64;
    let mut mixed: Vec<Complex32> = Vec::with_capacity(iq_in.len());
    for s in iq_in {
        let (sin_p, cos_p) = phase.sin_cos();
        let (sin_p, cos_p) = (sin_p as f32, cos_p as f32);
        mixed.push(Complex32 {
            re: s.re * cos_p - s.im * sin_p,
            im: s.re * sin_p + s.im * cos_p,
        });
        phase += phase_step;
        if phase.abs() > 1e10 {
            phase = phase.rem_euclid(two_pi);
        }
    }

    // -- Stage 2: LPF the baseband signal ---------------------------------
    // Cutoff at min(in,out)/2.4 (~10.4 kHz at 25 kHz Nyquist). Plenty of
    // margin for the ~6.25 kHz P25 channel.
    let cutoff_hz = input_rate.min(output_rate) / 2.4;
    let n_taps = 65;
    let taps = kaiser_lpf(n_taps, cutoff_hz / input_rate, 60.0);
    let half = n_taps / 2;
    let mut filtered: Vec<Complex32> = vec![Complex32 { re: 0.0, im: 0.0 }; mixed.len()];
    for k in 0..mixed.len() {
        let mut acc_re = 0.0f32;
        let mut acc_im = 0.0f32;
        for (j, &t) in taps.iter().enumerate() {
            let src = k as i64 + j as i64 - half as i64;
            if src >= 0 && (src as usize) < mixed.len() {
                let s = mixed[src as usize];
                acc_re += t * s.re;
                acc_im += t * s.im;
            }
        }
        filtered[k] = Complex32 { re: acc_re, im: acc_im };
    }

    // -- Stage 3: Rational rate convert via linear interpolation ----------
    // Output sample n maps to input position n * input_rate / output_rate.
    let ratio = input_rate / output_rate;
    let n_out = ((filtered.len() as f64) / ratio).floor() as usize;
    let mut out: Vec<Complex32> = Vec::with_capacity(n_out);
    for n in 0..n_out {
        let pos = n as f64 * ratio;
        let i0 = pos.floor() as usize;
        let frac = (pos - i0 as f64) as f32;
        let i1 = (i0 + 1).min(filtered.len() - 1);
        let a = filtered[i0];
        let b = filtered[i1];
        out.push(Complex32 {
            re: a.re * (1.0 - frac) + b.re * frac,
            im: a.im * (1.0 - frac) + b.im * frac,
        });
    }
    out
}

/// Kaiser-windowed sinc LPF, normalised cutoff = `cutoff_normalised`
/// (Hz / input_rate). `attenuation_db` shapes the Kaiser β.
fn kaiser_lpf(n_taps: usize, cutoff_normalised: f64, attenuation_db: f64) -> Vec<f32> {
    assert!(n_taps % 2 == 1);
    let m = n_taps as f64 - 1.0;
    let beta = if attenuation_db > 50.0 {
        0.1102 * (attenuation_db - 8.7)
    } else if attenuation_db >= 21.0 {
        0.5842 * (attenuation_db - 21.0).powf(0.4) + 0.07886 * (attenuation_db - 21.0)
    } else {
        0.0
    };
    let i0_beta = bessel_i0(beta);

    let mut taps = vec![0.0f64; n_taps];
    let pi = std::f64::consts::PI;
    let two_fc = 2.0 * cutoff_normalised;
    let center = m / 2.0;
    for n in 0..n_taps {
        let x = n as f64 - center;
        // Sinc
        let sinc = if x.abs() < 1e-9 {
            two_fc
        } else {
            (pi * two_fc * x).sin() / (pi * x)
        };
        // Kaiser window
        let r = (n as f64 - center) / center;
        let w = bessel_i0(beta * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
        taps[n] = sinc * w;
    }
    // Normalise to unity DC gain.
    let dc: f64 = taps.iter().sum();
    taps.iter().map(|t| (*t / dc) as f32).collect()
}

/// Modified Bessel function I0 (used by Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let half_x = x / 2.0;
    for k in 1..50 {
        term *= (half_x / k as f64) * (half_x / k as f64);
        sum += term;
        if term < sum * 1e-12 {
            break;
        }
    }
    sum
}

// -----------------------------------------------------------------------------
// .bits file reader: SDRTrunk packed-dibit format
// -----------------------------------------------------------------------------

/// Read a SDRTrunk `.bits` file. Confirmed format from
/// `DibitToByteBufferAssembler.receive()` + `BinaryRecorder` (no header):
///   - each byte holds 4 dibits, MSB-first (the assembler does
///     `mCurrentByte <<= 2; mCurrentByte |= dibit_value` per dibit, so
///     after 4 dibits the FIRST dibit ends up at bits [6:8]).
///   - dibit value mapping: +1→0, +3→1, −1→2, −3→3 (Dibit.java).
///
/// Optional `BITS_HEADER_BYTES` env var lets us skip a leading header
/// for non-SDRTrunk dumps (e.g. our own ring captures); default 0.
///
/// Bit-pack order is governed by the `BITS_ORDER` env var:
///   "msb_dibit"  (default) = each byte holds 4 dibits MSB-first within byte
///   "lsb_dibit"            = each byte holds 4 dibits LSB-first
///   "msb_bit"              = each byte holds 8 bits MSB-first; group into dibits
///   "lsb_bit"              = each byte holds 8 bits LSB-first; group into dibits
///
/// Returns flat Vec<u8> of dibit values in transmission order.
fn read_bits_file(path: &str) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("read .bits");
    let header_bytes: usize = std::env::var("BITS_HEADER_BYTES")
        .map(|s| s.parse().unwrap_or(0))
        .unwrap_or(0);
    assert!(bytes.len() > header_bytes, ".bits too short");
    let body = &bytes[header_bytes..];
    let order = std::env::var("BITS_ORDER").unwrap_or_else(|_| "msb_dibit".into());
    eprintln!("  .bits order: {}", order);
    let mut dibits = Vec::with_capacity(body.len() * 4);
    match order.as_str() {
        "msb_dibit" => {
            for &b in body {
                dibits.push((b >> 6) & 0x03);
                dibits.push((b >> 4) & 0x03);
                dibits.push((b >> 2) & 0x03);
                dibits.push(b & 0x03);
            }
        }
        "lsb_dibit" => {
            for &b in body {
                dibits.push(b & 0x03);
                dibits.push((b >> 2) & 0x03);
                dibits.push((b >> 4) & 0x03);
                dibits.push((b >> 6) & 0x03);
            }
        }
        "msb_bit" => {
            // 8 bits MSB-first per byte, then pair into dibits.
            let mut bits: Vec<u8> = Vec::with_capacity(body.len() * 8);
            for &b in body {
                for k in (0..8).rev() {
                    bits.push((b >> k) & 1);
                }
            }
            for c in bits.chunks(2) {
                if c.len() == 2 {
                    dibits.push((c[0] << 1) | c[1]);
                }
            }
        }
        "lsb_bit" => {
            let mut bits: Vec<u8> = Vec::with_capacity(body.len() * 8);
            for &b in body {
                for k in 0..8 {
                    bits.push((b >> k) & 1);
                }
            }
            for c in bits.chunks(2) {
                if c.len() == 2 {
                    dibits.push((c[0] << 1) | c[1]);
                }
            }
        }
        other => panic!("unknown BITS_ORDER {}", other),
    }
    dibits
}

// -----------------------------------------------------------------------------
// Voice handler: catch LDU1/LDU2 IMBE bytes, run JMBE, accumulate PCM.
// -----------------------------------------------------------------------------

struct CapturingVoiceHandler {
    decoder: Mutex<ImbeDecoder>,
    pcm: Mutex<Vec<i16>>,
    counters: Mutex<Counters>,
}

#[derive(Default, Debug)]
struct Counters {
    hdu: u32,
    ldu1: u32,
    ldu2: u32,
    tdu: u32,
    tdu_lc: u32,
    imbe_total: u32,
    silent_frames: u32,
}

impl VoiceHandler for CapturingVoiceHandler {
    fn on_hdu(&self, _body: &[u8]) {
        self.counters.lock().unwrap().hdu += 1;
    }
    fn on_tdu(&self) {
        self.counters.lock().unwrap().tdu += 1;
    }
    fn on_tdu_lc(&self, _body: &[u8]) {
        self.counters.lock().unwrap().tdu_lc += 1;
    }
    fn on_ldu1(&self, frames: &[ImbeFrameRaw; 9], _body: &[u8]) {
        self.counters.lock().unwrap().ldu1 += 1;
        self.run_frames(frames);
    }
    fn on_ldu2(&self, frames: &[ImbeFrameRaw; 9], _body: &[u8]) {
        self.counters.lock().unwrap().ldu2 += 1;
        self.run_frames(frames);
    }
}

impl CapturingVoiceHandler {
    fn run_frames(&self, frames: &[ImbeFrameRaw; 9]) {
        let mut decoder = self.decoder.lock().unwrap();
        let mut pcm = self.pcm.lock().unwrap();
        let mut counters = self.counters.lock().unwrap();
        for frame in frames {
            let bytes: [u8; 18] = frame.bits;
            let samples = decoder.decode_frame(&bytes);
            let mut frame_peak: i16 = 0;
            for s in samples {
                let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
                pcm.push(v);
                if v.unsigned_abs() as i16 > frame_peak {
                    frame_peak = v.unsigned_abs() as i16;
                }
            }
            counters.imbe_total += 1;
            if frame_peak < 16 {
                counters.silent_frames += 1;
            }
            let _ = SAMPLES_PER_FRAME; // keep the import live
        }
    }
}

// -----------------------------------------------------------------------------
// WAV writer (8 kHz mono 16-bit)
// -----------------------------------------------------------------------------

fn write_mono_wav_8khz(path: &str, pcm: &[i16]) {
    use std::io::Write;
    let mut f = std::fs::File::create(path).expect("create WAV");
    let data_len = (pcm.len() * 2) as u32;
    let chunk_size = 36 + data_len;
    f.write_all(b"RIFF").unwrap();
    f.write_all(&chunk_size.to_le_bytes()).unwrap();
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
    for s in pcm {
        f.write_all(&s.to_le_bytes()).unwrap();
    }
}

// -----------------------------------------------------------------------------
// The test entry-point.
// -----------------------------------------------------------------------------

#[test]
#[ignore]
fn software_decode() {
    let in_path = std::env::var("SOFTDEC_INPUT").expect("SOFTDEC_INPUT required");
    let out_wav =
        std::env::var("SOFTDEC_OUT_WAV").unwrap_or_else(|_| "softdec_out.wav".to_string());

    // Decide path by extension or env.
    let lower = in_path.to_lowercase();
    let mut demod_metrics: Option<DemodMetrics> = None;
    let dibits: Vec<u8> = if lower.ends_with(".bits") {
        eprintln!("input: SDRTrunk .bits stream");
        read_bits_file(&in_path)
    } else if lower.ends_with(".cs16") {
        // Wideband IQ tap from `/api/wideband_iq_capture`. Headerless
        // raw i16 LE interleaved I/Q. SOFTDEC_INPUT_RATE defaults to
        // 8 MSPS (the rxiq_cdc tap rate); SOFTDEC_CENTER_HZ = AD9361
        // RX LO; SOFTDEC_TARGET_HZ = the radio frequency we want to
        // tune in software.
        let input_rate: f64 = std::env::var("SOFTDEC_INPUT_RATE")
            .map(|s| s.parse().expect("SOFTDEC_INPUT_RATE int"))
            .unwrap_or(8_000_000.0);
        let center_hz: f64 = std::env::var("SOFTDEC_CENTER_HZ")
            .expect("SOFTDEC_CENTER_HZ required for .cs16 input")
            .parse()
            .expect("SOFTDEC_CENTER_HZ");
        let target_hz: f64 = std::env::var("SOFTDEC_TARGET_HZ")
            .map(|s| s.parse().expect("SOFTDEC_TARGET_HZ"))
            .unwrap_or(center_hz);
        let ppm: f64 = std::env::var("SOFTDEC_PPM")
            .map(|s| s.parse().expect("SOFTDEC_PPM"))
            .unwrap_or(0.0);

        eprintln!("input: wideband_iq .cs16");
        let (iq, sr) = read_cs16_iq(&in_path, input_rate as u32);
        let iq = maybe_slice_iq(iq, sr as f64);
        eprintln!("  IQ samples: {}  rate: {:.0} Hz", iq.len(), sr);

        let ppm_corr_hz = -ppm * 1e-6 * center_hz;
        let nco_offset = (target_hz - center_hz) + ppm_corr_hz;
        eprintln!(
            "  NCO offset = (target {:.1} - center {:.1}) + ppm-corr {:.1} = {:.1} Hz",
            target_hz, center_hz, ppm_corr_hz, nco_offset
        );

        let iq_lsm_in = if std::env::var("SOFTDEC_HALFBAND").ok().as_deref() == Some("1") {
            // SDRTrunk-faithful halfband cascade DDC. Uses the same
            // filter math as `HalfBandTunerChannelSource`. f64 NCO,
            // power-of-2 halfband cascade, then 5:4 linear interp.
            eprintln!("  HALFBAND DDC mode (SDRTrunk-faithful)");
            crate::sw_demod::halfband_ddc_to_25k(&iq, sr as f64, nco_offset)
        } else if std::env::var("SOFTDEC_MULTISTAGE").ok().as_deref() == Some("1") {
            let chunk_samples = ((sr as f64) * 0.032) as usize;
            eprintln!("  MULTISTAGE DDC mode (chunk={chunk_samples} samples = 32 ms)");
            ddc_to_62k5_multistage(&iq, sr as f64, nco_offset, 25_000.0, chunk_samples)
        } else if std::env::var("SOFTDEC_STREAMING").ok().as_deref() == Some("1") {
            // Stream the input through the live `StreamingSoftwareDdc`
            // in 32 ms chunks (matches the on-board wideband_iq DMA
            // sub-buffer cadence at 8 MSPS). Isolates streaming-vs-
            // batch DDC differences from real-time / lock / chunk-drop
            // effects on the board.
            let chunk_samples = ((sr as f64) * 0.032) as usize;
            eprintln!("  STREAMING DDC mode (chunk={chunk_samples} samples = 32 ms)");
            ddc_to_62k5_streaming(&iq, sr as f64, nco_offset, 25_000.0, chunk_samples)
        } else {
            ddc_to_62k5(&iq, sr as f64, nco_offset, 25_000.0)
        };
        eprintln!("  after DDC: {} samples @ 25 kSPS", iq_lsm_in.len());

        let mut pipeline = LsmPipeline::new();
        let batch = pipeline.process_iq(&iq_lsm_in);
        eprintln!(
            "  LSM demod: {} symbols, hard_sync={}, soft_sync={}",
            batch.demod.n_symbols(),
            batch.hard_events.len(),
            batch.soft_events.len()
        );
        // Truncate stderr per-event prints to first 10 for human use.
        // Full lists go in the metrics JSON.
        for (i, e) in batch.hard_events.iter().enumerate().take(10) {
            eprintln!(
                "    hard #{i}: pos={} dist={} nac={:#05x} duid={}",
                e.symbol_idx, e.distance, e.best_nac(), e.best_duid()
            );
        }
        for (i, e) in batch.soft_events.iter().enumerate().take(10) {
            eprintln!(
                "    soft #{i}: pos={} score={:.3} nac={:#05x} duid={}",
                e.symbol_idx, e.score, e.best_nac(), e.best_duid()
            );
        }
        let dibits_out = batch.demod.hard_dibits.clone();
        demod_metrics = Some(DemodMetrics {
            n_symbols: batch.demod.n_symbols(),
            hard_events: batch.hard_events,
            soft_events: batch.soft_events,
            pll_trace: batch.demod.pll_trace,
            timing_trace: batch.demod.timing_trace,
            soft_symbols: batch.demod.soft_symbols,
        });
        dibits_out
    } else if lower.ends_with(".wav") {
        let input_rate: f64 = std::env::var("SOFTDEC_INPUT_RATE")
            .map(|s| s.parse().expect("SOFTDEC_INPUT_RATE int"))
            .unwrap_or(0.0);
        let center_hz: f64 = std::env::var("SOFTDEC_CENTER_HZ")
            .expect("SOFTDEC_CENTER_HZ required for wav input")
            .parse()
            .expect("SOFTDEC_CENTER_HZ");
        let target_hz: f64 = std::env::var("SOFTDEC_TARGET_HZ")
            .map(|s| s.parse().expect("SOFTDEC_TARGET_HZ"))
            .unwrap_or(center_hz);
        let ppm: f64 = std::env::var("SOFTDEC_PPM")
            .map(|s| s.parse().expect("SOFTDEC_PPM"))
            .unwrap_or(0.0);

        eprintln!("input: SDRTrunk baseband WAV");
        let (iq, sr) = read_baseband_wav(&in_path);
        let sr = if input_rate > 0.0 { input_rate } else { sr as f64 };
        let iq = maybe_slice_iq(iq, sr);
        eprintln!("  IQ samples: {}  rate: {:.0} Hz", iq.len(), sr);

        // PPM correction: a positive ppm moves the local oscillator high,
        // so signal arrives at a slightly LOWER frequency than expected.
        // Equivalently, compensate by shifting NCO offset by -ppm * center.
        let ppm_corr_hz = -ppm * 1e-6 * center_hz;
        let nco_offset = (target_hz - center_hz) + ppm_corr_hz;
        eprintln!(
            "  NCO offset = (target {:.1} - center {:.1}) + ppm-corr {:.1} = {:.1} Hz",
            target_hz, center_hz, ppm_corr_hz, nco_offset
        );

        let iq_lsm_in = if std::env::var("SOFTDEC_HALFBAND").ok().as_deref() == Some("1") {
            eprintln!("  HALFBAND DDC mode (SDRTrunk-faithful)");
            crate::sw_demod::halfband_ddc_to_25k(&iq, sr, nco_offset)
        } else if std::env::var("SOFTDEC_MULTISTAGE").ok().as_deref() == Some("1") {
            let chunk_samples = (sr * 0.032) as usize;
            eprintln!("  MULTISTAGE DDC mode (chunk={chunk_samples} samples = 32 ms)");
            ddc_to_62k5_multistage(&iq, sr, nco_offset, 25_000.0, chunk_samples)
        } else if std::env::var("SOFTDEC_STREAMING").ok().as_deref() == Some("1") {
            let chunk_samples = (sr * 0.032) as usize;
            eprintln!("  STREAMING DDC mode (chunk={chunk_samples} samples = 32 ms)");
            ddc_to_62k5_streaming(&iq, sr, nco_offset, 25_000.0, chunk_samples)
        } else {
            ddc_to_62k5(&iq, sr, nco_offset, 25_000.0)
        };
        eprintln!("  after DDC: {} samples @ 25 kSPS", iq_lsm_in.len());

        // 2026-05-03: optional pre-pad samples for loop-settling.
        // Diagnostic dibit-diff vs SDRTrunk's per-call .bits showed
        // our pipeline produces dibits ~99-100% identical at the
        // symbol level after the first ~30 dibits (~6.7 ms = AGC/PLL/
        // Gardner/RRC transient). The first transient region propagates
        // bad dibits into the framer state machine and costs ~1 LDU1
        // per call. Pre-padding with `SOFTDEC_PREPAD_SAMPLES` of the
        // input's first sample (or zeros if first-sample is zero)
        // before the actual call IQ lets the loops settle. The framer
        // state is cleared after pre-pad processing so it doesn't
        // see the synthetic dibits.
        let prepad: usize = std::env::var("SOFTDEC_PREPAD_SAMPLES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let mut pipeline = LsmPipeline::new();
        if prepad > 0 && !iq_lsm_in.is_empty() {
            let pad_sample = iq_lsm_in[0];
            let pad: Vec<Complex32> = vec![pad_sample; prepad];
            let _ = pipeline.process_iq(&pad);
            eprintln!("  pre-padded with {prepad} samples to settle loops");
        }
        let batch = pipeline.process_iq(&iq_lsm_in);
        eprintln!(
            "  LSM demod: {} symbols, hard_sync={}, soft_sync={}",
            batch.demod.n_symbols(),
            batch.hard_events.len(),
            batch.soft_events.len()
        );
        for (i, e) in batch.hard_events.iter().enumerate().take(10) {
            eprintln!(
                "    hard #{i}: pos={} dist={} nac={:#05x} duid={}",
                e.symbol_idx, e.distance, e.best_nac(), e.best_duid()
            );
        }
        for (i, e) in batch.soft_events.iter().enumerate().take(10) {
            eprintln!(
                "    soft #{i}: pos={} score={:.3} nac={:#05x} duid={}",
                e.symbol_idx, e.score, e.best_nac(), e.best_duid()
            );
        }
        let dibits_out = batch.demod.hard_dibits.clone();
        demod_metrics = Some(DemodMetrics {
            n_symbols: batch.demod.n_symbols(),
            hard_events: batch.hard_events,
            soft_events: batch.soft_events,
            pll_trace: batch.demod.pll_trace,
            timing_trace: batch.demod.timing_trace,
            soft_symbols: batch.demod.soft_symbols,
        });
        dibits_out
    } else {
        panic!("SOFTDEC_INPUT must end in .bits, .wav, or .cs16 (got {})", in_path);
    };

    eprintln!("dibits to feed framer: {}", dibits.len());

    // Build the framer + voice handler.
    let handler = Arc::new(CapturingVoiceHandler {
        decoder: Mutex::new(ImbeDecoder::new()),
        pcm: Mutex::new(Vec::new()),
        counters: Mutex::new(Counters::default()),
    });
    let mut framer = ControlChannelDecoder::new();
    framer.set_voice_handler(handler.clone());

    for &d in &dibits {
        framer.process_dibit(d);
    }

    let counters = handler.counters.lock().unwrap();
    let pcm = handler.pcm.lock().unwrap();
    let peak = pcm.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    let rms = if !pcm.is_empty() {
        let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum_sq / pcm.len() as f64).sqrt()
    } else {
        0.0
    };
    eprintln!("framer counters: {:?}", *counters);
    eprintln!(
        "framer internal: hdu={} ldu1={} ldu2={} tdu={} tdu_lc={}",
        framer.hdu_count, framer.ldu1_count, framer.ldu2_count,
        framer.tdu_count, framer.tdu_lc_count,
    );
    eprintln!("PCM: {} samples ({:.2}s) peak={} rms={:.1}",
        pcm.len(), pcm.len() as f32 / 8000.0, peak, rms);

    // Dump ALL decoded TSBKs from the framer — invaluable when running
    // on a control-channel capture: the grant TSBKs tell us which
    // traffic LCNs were active and when, so we can re-target the same
    // capture at the right traffic frequency without guessing.
    eprintln!("---");
    eprintln!("framer.recent_messages: {} decoded TSBKs",
        framer.recent_messages.len());
    let mut grant_count = 0usize;
    for (_inst, block_idx, msg) in framer.recent_messages.iter() {
        let s = format!("{:?}", msg);
        let is_grant =
            s.contains("GroupVoiceChannelGrant") ||
            s.contains("GRP_VCH_GRANT") ||
            s.contains("ChannelGrant") ||
            s.contains("VCH_GRANT");
        if is_grant {
            grant_count += 1;
            eprintln!("  TSBK{} {}", block_idx, s);
        }
    }
    eprintln!("--- {} grant TSBKs total", grant_count);

    write_mono_wav_8khz(&out_wav, &pcm);
    eprintln!("wrote {}", out_wav);

    // 2026-05-03: optional dibit dump for SDRTrunk .bits cross-comparison.
    // When SOFTDEC_DIBITS_OUT is set, pack our continuous dibit stream
    // into bytes (4 dibits per byte, MSB-first — same packing as
    // SDRTrunk writes its `_9600BPS_*.bits` files) and write to disk.
    // Used by `tools/p25_dibit_diff.py` to slide-align our dibits
    // against SDRTrunk's reference and locate divergence positions.
    if let Ok(dibits_path) = std::env::var("SOFTDEC_DIBITS_OUT") {
        let mut packed: Vec<u8> = Vec::with_capacity(dibits.len() / 4 + 1);
        let mut acc: u8 = 0;
        let mut count: u8 = 0;
        for &d in &dibits {
            acc = (acc << 2) | (d & 0x3);
            count += 1;
            if count == 4 {
                packed.push(acc);
                acc = 0;
                count = 0;
            }
        }
        if count != 0 {
            acc <<= (4 - count) * 2;
            packed.push(acc);
        }
        if let Err(e) = std::fs::write(&dibits_path, &packed) {
            eprintln!("dibits dump write failed: {}", e);
        } else {
            eprintln!(
                "wrote dibits ({} dibits = {} packed bytes) -> {}",
                dibits.len(), packed.len(), dibits_path
            );
        }
    }

    // 2026-05-02: optional metrics dump for baseline analysis. When
    // SOFTDEC_METRICS_JSON is set, write a single-line JSON summary
    // of decode quality + per-symbol traces. Consumed by
    // tools/p25_baseline_analyze.py to build a report comparing the
    // SW path against SDRTrunk's same-RF decode.
    if let Ok(metrics_path) = std::env::var("SOFTDEC_METRICS_JSON") {
        let _ = write_metrics_json(
            &metrics_path,
            &counters, &pcm, peak, rms,
            &dibits,
            demod_metrics.as_ref(),
        );
        eprintln!("wrote metrics JSON: {metrics_path}");
    }
}

#[allow(clippy::too_many_arguments)]
fn write_metrics_json(
    path: &str,
    counters: &Counters,
    pcm: &[i16],
    peak: u16,
    rms: f64,
    dibits: &[u8],
    demod: Option<&DemodMetrics>,
) -> std::io::Result<()> {
    use std::io::Write;
    let mut dibit_hist = [0u64; 4];
    for &d in dibits {
        dibit_hist[(d & 0x3) as usize] += 1;
    }
    let total = dibits.len().max(1);

    let mut f = std::fs::File::create(path)?;
    writeln!(f, "{{")?;
    writeln!(f, "  \"dibits_total\": {},", dibits.len())?;
    writeln!(f, "  \"dibit_hist\": [{}, {}, {}, {}],",
        dibit_hist[0], dibit_hist[1], dibit_hist[2], dibit_hist[3])?;
    writeln!(f, "  \"dibit_pct\": [{:.2}, {:.2}, {:.2}, {:.2}],",
        100.0 * dibit_hist[0] as f64 / total as f64,
        100.0 * dibit_hist[1] as f64 / total as f64,
        100.0 * dibit_hist[2] as f64 / total as f64,
        100.0 * dibit_hist[3] as f64 / total as f64)?;
    writeln!(f, "  \"framer\": {{")?;
    writeln!(f, "    \"hdu\": {}, \"ldu1\": {}, \"ldu2\": {},",
        counters.hdu, counters.ldu1, counters.ldu2)?;
    writeln!(f, "    \"tdu\": {}, \"tdu_lc\": {},",
        counters.tdu, counters.tdu_lc)?;
    writeln!(f, "    \"imbe_total\": {}, \"silent_frames\": {}",
        counters.imbe_total, counters.silent_frames)?;
    writeln!(f, "  }},")?;
    writeln!(f, "  \"pcm\": {{")?;
    writeln!(f, "    \"samples\": {}, \"duration_s\": {:.3},",
        pcm.len(), pcm.len() as f32 / 8000.0)?;
    writeln!(f, "    \"peak\": {}, \"rms\": {:.1}", peak, rms)?;
    writeln!(f, "  }}")?;
    if let Some(dm) = demod {
        writeln!(f, "  ,")?;
        writeln!(f, "  \"n_symbols\": {},", dm.n_symbols)?;
        // PLL stats (Costas loop output, radians)
        let pll_stats = vector_stats(&dm.pll_trace);
        writeln!(f, "  \"pll_trace\": {},", pll_stats)?;
        let timing_stats = vector_stats(&dm.timing_trace);
        writeln!(f, "  \"timing_trace\": {},", timing_stats)?;
        // Per-symbol amplitude (= |soft_symbol|) — AGC stability proxy.
        let amps: Vec<f32> = dm.soft_symbols.iter()
            .map(|s| (s.re * s.re + s.im * s.im).sqrt())
            .collect();
        let amp_stats = vector_stats(&amps);
        writeln!(f, "  \"soft_symbol_amp\": {},", amp_stats)?;
        // Hard sync events
        writeln!(f, "  \"hard_events\": [")?;
        for (i, e) in dm.hard_events.iter().enumerate() {
            let comma = if i + 1 == dm.hard_events.len() { "" } else { "," };
            writeln!(f,
                "    {{\"sym\":{},\"dist\":{},\"nac\":{},\"duid\":{},\"fec\":{}}}{}",
                e.symbol_idx, e.distance, e.best_nac(), e.best_duid(),
                if e.fec.is_some() { "true" } else { "false" }, comma)?;
        }
        writeln!(f, "  ],")?;
        // Soft sync events
        writeln!(f, "  \"soft_events\": [")?;
        for (i, e) in dm.soft_events.iter().enumerate() {
            let comma = if i + 1 == dm.soft_events.len() { "" } else { "," };
            writeln!(f,
                "    {{\"sym\":{},\"score\":{:.2},\"nac\":{},\"duid\":{},\"fec\":{}}}{}",
                e.symbol_idx, e.score, e.best_nac(), e.best_duid(),
                if e.fec.is_some() { "true" } else { "false" }, comma)?;
        }
        writeln!(f, "  ]")?;
    }
    writeln!(f, "}}")?;
    Ok(())
}

// ----------------------------------------------------------------------------
// 2026-05-03 Track-2 forensics: full-chain control->grant->traffic decode in
// one process. Mirrors the live HDL flow (single AD9361 stream feeds two DDCs:
// one fixed on the control freq, one retuned per grant). Used to validate the
// SW pipeline against SDRTrunk's reference at 4 MSPS without hardcoding the
// traffic freq.
// ----------------------------------------------------------------------------

fn pack_dibits_msb(dibits: &[u8]) -> Vec<u8> {
    let mut packed = Vec::with_capacity(dibits.len() / 4 + 1);
    let mut acc: u8 = 0;
    let mut count: u8 = 0;
    for &d in dibits {
        acc = (acc << 2) | (d & 0x3);
        count += 1;
        if count == 4 {
            packed.push(acc);
            acc = 0;
            count = 0;
        }
    }
    if count != 0 {
        acc <<= (4 - count) * 2;
        packed.push(acc);
    }
    packed
}

/// DDC mode selector for the full-chain test.
#[derive(Clone, Copy)]
enum FullChainDdcMode {
    /// Single-stage Kaiser LPF (default).
    SingleStage,
    /// 3-stage Kaiser cascade with sharper LPF.
    Multistage,
    /// SDRTrunk-faithful halfband cascade port.
    Halfband,
}

/// Run DDC + LSM + framer for one center freq. Optional `voice_handler`
/// lets the traffic phase capture IMBE; control phase passes None.
fn decode_one_freq(
    iq: &[Complex32],
    sample_rate_hz: f64,
    nco_offset_hz: f64,
    voice_handler: Option<Arc<CapturingVoiceHandler>>,
    mode: FullChainDdcMode,
) -> (ControlChannelDecoder, Vec<u8>, DemodMetrics) {
    let iq_25k = match mode {
        FullChainDdcMode::Halfband => {
            crate::sw_demod::halfband_ddc_to_25k(iq, sample_rate_hz, nco_offset_hz)
        }
        FullChainDdcMode::Multistage => {
            let chunk = (sample_rate_hz * 0.032) as usize;
            ddc_to_62k5_multistage(iq, sample_rate_hz, nco_offset_hz, 25_000.0, chunk)
        }
        FullChainDdcMode::SingleStage => {
            ddc_to_62k5(iq, sample_rate_hz, nco_offset_hz, 25_000.0)
        }
    };
    let mut pipeline = LsmPipeline::new();
    let batch = pipeline.process_iq(&iq_25k);
    let dibits = batch.demod.hard_dibits.clone();
    let metrics = DemodMetrics {
        n_symbols: batch.demod.n_symbols(),
        hard_events: batch.hard_events,
        soft_events: batch.soft_events,
        pll_trace: batch.demod.pll_trace,
        timing_trace: batch.demod.timing_trace,
        soft_symbols: batch.demod.soft_symbols,
    };
    let mut framer = ControlChannelDecoder::new();
    // Hold every TSBK we decode -- offline windows are long and the
    // 1000-entry default would evict grants before the caller can
    // inspect them.
    framer.max_recent = 1_000_000;
    if let Some(h) = voice_handler {
        framer.set_voice_handler(h);
    }
    for &d in &dibits {
        framer.process_dibit(d);
    }
    (framer, dibits, metrics)
}

/// Optional slice based on `SOFTDEC_INPUT_OFFSET_S` /
/// `SOFTDEC_INPUT_DURATION_S` env vars. Applied to all IQ reads
/// (single-freq + full-chain) so any test can isolate "DDC works on
/// short windows but fails on long ones" from "fails at this NCO
/// offset regardless".
fn maybe_slice_iq(iq: Vec<Complex32>, sr: f64) -> Vec<Complex32> {
    let off_s: f64 = std::env::var("SOFTDEC_INPUT_OFFSET_S")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    let dur_s: f64 = std::env::var("SOFTDEC_INPUT_DURATION_S")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if off_s <= 0.0 && dur_s <= 0.0 {
        return iq;
    }
    let start = ((off_s * sr) as usize).min(iq.len());
    let end = if dur_s > 0.0 {
        (start + (dur_s * sr) as usize).min(iq.len())
    } else { iq.len() };
    eprintln!("  slicing IQ to [{start}, {end}) = {:.3}s..{:.3}s ({} samples)",
              start as f64 / sr, end as f64 / sr, end - start);
    iq[start..end].to_vec()
}

/// Read a wideband IQ window from .wav (SDRTrunk my_captures) or .cs16
/// (our /api/wideband_iq_capture). Honours SOFTDEC_INPUT_RATE for .cs16
/// (no header) and the WAV-header rate for .wav (overridable by env).
fn read_wideband_iq(in_path: &str) -> (Vec<Complex32>, f64) {
    let lower = in_path.to_lowercase();
    let (iq, sr) = if lower.ends_with(".wav") {
        let (iq, sr) = read_baseband_wav(in_path);
        (iq, sr as f64)
    } else if lower.ends_with(".cs16") {
        let rate: f64 = std::env::var("SOFTDEC_INPUT_RATE")
            .map(|s| s.parse().expect("SOFTDEC_INPUT_RATE int"))
            .expect("SOFTDEC_INPUT_RATE required for .cs16");
        let (iq, sr) = read_cs16_iq(in_path, rate as u32);
        (iq, sr as f64)
    } else {
        panic!("SOFTDEC_INPUT must end in .wav or .cs16 (got {in_path})");
    };
    let env_rate: f64 = std::env::var("SOFTDEC_INPUT_RATE")
        .map(|s| s.parse().unwrap_or(0.0))
        .unwrap_or(0.0);
    let sr = if env_rate > 0.0 { env_rate } else { sr };
    (maybe_slice_iq(iq, sr), sr)
}

/// Extract unique grant frequencies from a control framer's TSBK ring.
/// Returns Vec<(freq_hz, talkgroup, encrypted)> deduped by (freq, tg).
fn collect_grant_targets(
    framer: &ControlChannelDecoder,
) -> Vec<(u64, u16, bool)> {
    use crate::protocol::p25::tsbk::TsbkMessage;
    use crate::protocol::p25::tsbk::service_options::is_encrypted;
    let mut seen: std::collections::BTreeSet<(u64, u16)> = std::collections::BTreeSet::new();
    let mut out: Vec<(u64, u16, bool)> = Vec::new();
    for (_inst, _block, msg) in framer.recent_messages.iter() {
        match msg {
            TsbkMessage::GroupVoiceChannelGrant {
                channel, talkgroup, service_options, ..
            } => {
                if let Some(freq) = framer.channel_to_frequency(*channel) {
                    let tg = talkgroup.0;
                    if seen.insert((freq, tg)) {
                        out.push((freq, tg, is_encrypted(*service_options)));
                    }
                }
            }
            TsbkMessage::GroupVoiceChannelGrantUpdateExplicit {
                transmit_channel, talkgroup, service_options, ..
            } => {
                if let Some(freq) = framer.channel_to_frequency(*transmit_channel) {
                    let tg = talkgroup.0;
                    if seen.insert((freq, tg)) {
                        out.push((freq, tg, is_encrypted(*service_options)));
                    }
                }
            }
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a, talkgroup_a, channel_b, talkgroup_b,
            } => {
                if let Some(freq) = framer.channel_to_frequency(*channel_a) {
                    let tg = talkgroup_a.0;
                    if seen.insert((freq, tg)) {
                        out.push((freq, tg, false));
                    }
                }
                if talkgroup_b.0 != 0 {
                    if let Some(freq) = framer.channel_to_frequency(*channel_b) {
                        let tg = talkgroup_b.0;
                        if seen.insert((freq, tg)) {
                            out.push((freq, tg, false));
                        }
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Full-chain SW decode: wideband IQ -> control DDC -> framer -> grants ->
/// per-grant traffic DDC -> framer -> JMBE. All in one process.
///
/// Required env vars:
///   SOFTDEC_INPUT       wideband .wav (my_captures/) or .cs16
///   SOFTDEC_CENTER_HZ   AD9361 RX LO that the wideband was captured at
///   SOFTDEC_CONTROL_HZ  control channel frequency (e.g. 860962500)
///   SOFTDEC_OUT_DIR     output directory for control_*.{bits,json} +
///                       traffic_<freq>_tg<tg>_*.{bits,json,wav}
/// Optional:
///   SOFTDEC_INPUT_RATE  Hz; required for .cs16, overrides WAV header
///   SOFTDEC_PPM         crystal ppm correction (default 0)
///   SOFTDEC_SKIP_ENCRYPTED  if "1", skip encrypted grants (no audio anyway)
#[test]
#[ignore]
fn software_decode_full_chain() {
    let in_path = std::env::var("SOFTDEC_INPUT").expect("SOFTDEC_INPUT required");
    let center_hz: f64 = std::env::var("SOFTDEC_CENTER_HZ")
        .expect("SOFTDEC_CENTER_HZ required")
        .parse().expect("SOFTDEC_CENTER_HZ");
    let control_hz: f64 = std::env::var("SOFTDEC_CONTROL_HZ")
        .expect("SOFTDEC_CONTROL_HZ required")
        .parse().expect("SOFTDEC_CONTROL_HZ");
    let ppm: f64 = std::env::var("SOFTDEC_PPM")
        .map(|s| s.parse().expect("SOFTDEC_PPM"))
        .unwrap_or(0.0);
    let out_dir = std::env::var("SOFTDEC_OUT_DIR").expect("SOFTDEC_OUT_DIR required");
    std::fs::create_dir_all(&out_dir).expect("create out dir");
    let skip_encrypted = std::env::var("SOFTDEC_SKIP_ENCRYPTED").ok().as_deref() == Some("1");
    let mode = if std::env::var("SOFTDEC_HALFBAND").ok().as_deref() == Some("1") {
        FullChainDdcMode::Halfband
    } else if std::env::var("SOFTDEC_MULTISTAGE").ok().as_deref() == Some("1") {
        FullChainDdcMode::Multistage
    } else {
        FullChainDdcMode::SingleStage
    };

    eprintln!("=== full chain: wideband -> control -> grants -> traffic ===");
    match mode {
        FullChainDdcMode::Halfband =>
            eprintln!("DDC mode: SDRTrunk-faithful halfband cascade (port of HalfBandTunerChannelSource)"),
        FullChainDdcMode::Multistage =>
            eprintln!("DDC mode: multistage Kaiser cascade"),
        FullChainDdcMode::SingleStage =>
            eprintln!("DDC mode: single-stage 65-tap Kaiser"),
    }
    eprintln!("input:        {in_path}");
    eprintln!("center_hz:    {center_hz}");
    eprintln!("control_hz:   {control_hz}");
    eprintln!("ppm:          {ppm}");
    eprintln!("out_dir:      {out_dir}");

    let (iq, sr) = read_wideband_iq(&in_path);
    eprintln!("IQ samples:   {} @ {:.0} Hz ({:.2}s)",
        iq.len(), sr, iq.len() as f64 / sr);

    let ppm_corr_hz = -ppm * 1e-6 * center_hz;

    // ---- Phase 1: control ----
    let nco_ctrl = (control_hz - center_hz) + ppm_corr_hz;
    eprintln!("\n--- phase 1: control @ {} Hz (NCO {:.1} Hz) ---",
        control_hz, nco_ctrl);
    let (ctrl_framer, ctrl_dibits, ctrl_metrics) =
        decode_one_freq(&iq, sr, nco_ctrl, None, mode);
    eprintln!("control: {} dibits, {} hard_sync, {} soft_sync, framer hdu={} ldu1={} ldu2={} tdu={} tdu_lc={}",
        ctrl_dibits.len(),
        ctrl_metrics.hard_events.len(),
        ctrl_metrics.soft_events.len(),
        ctrl_framer.hdu_count, ctrl_framer.ldu1_count,
        ctrl_framer.ldu2_count, ctrl_framer.tdu_count,
        ctrl_framer.tdu_lc_count);
    eprintln!("control: {} TSBKs in recent_messages, {} bands learned",
        ctrl_framer.recent_messages.len(), ctrl_framer.bands.len());
    // TSBK variant histogram so "0 grants" is debuggable.
    {
        let mut hist: std::collections::BTreeMap<&'static str, usize> =
            std::collections::BTreeMap::new();
        for (_inst, _block, msg) in ctrl_framer.recent_messages.iter() {
            let label: &'static str = match msg {
                crate::protocol::p25::tsbk::TsbkMessage::GroupVoiceChannelGrant {..} => "GVCG",
                crate::protocol::p25::tsbk::TsbkMessage::GroupVoiceChannelGrantUpdate {..} => "GVCG_UPD",
                crate::protocol::p25::tsbk::TsbkMessage::GroupVoiceChannelGrantUpdateExplicit {..} => "GVCG_UPD_EXP",
                crate::protocol::p25::tsbk::TsbkMessage::IdentifierUpdate {..} => "IDEN_UP",
                crate::protocol::p25::tsbk::TsbkMessage::NetworkStatus {..} => "NET_STS",
                crate::protocol::p25::tsbk::TsbkMessage::RfssStatus {..} => "RFSS_STS",
                crate::protocol::p25::tsbk::TsbkMessage::SecondaryControlChannelBroadcast {..} => "SCCB",
                crate::protocol::p25::tsbk::TsbkMessage::TdmaSyncBroadcast {..} => "TDMA_SYNC",
                _ => "OTHER",
            };
            *hist.entry(label).or_insert(0) += 1;
        }
        eprintln!("TSBK variants: {hist:?}");
    }

    let ctrl_dibits_path = format!("{out_dir}/control_dibits.bits");
    let _ = std::fs::write(&ctrl_dibits_path, pack_dibits_msb(&ctrl_dibits));
    eprintln!("wrote {ctrl_dibits_path}");

    // Dump grant TSBKs for the run record.
    let grants = collect_grant_targets(&ctrl_framer);
    eprintln!("grants found: {}", grants.len());
    for (freq, tg, enc) in &grants {
        eprintln!("  TG {tg} freq {freq} Hz encrypted={enc}");
    }
    let grants_path = format!("{out_dir}/grants.json");
    let mut g_json = String::from("[\n");
    for (i, (freq, tg, enc)) in grants.iter().enumerate() {
        let comma = if i + 1 == grants.len() { "" } else { "," };
        g_json.push_str(&format!(
            "  {{\"freq_hz\":{freq},\"talkgroup\":{tg},\"encrypted\":{enc}}}{comma}\n"
        ));
    }
    g_json.push_str("]\n");
    let _ = std::fs::write(&grants_path, g_json);

    // ---- Phase 2: per-grant traffic ----
    if grants.is_empty() {
        eprintln!("\n!!! no grants decoded from control; phase 2 skipped !!!");
        eprintln!("    likely causes: wrong SOFTDEC_CONTROL_HZ, wrong PPM, ");
        eprintln!("    bad WAV format, or control chain failed at this rate.");
        return;
    }
    for (freq, tg, encrypted) in &grants {
        if *encrypted && skip_encrypted {
            eprintln!("\n--- skipping encrypted TG {tg} @ {freq} Hz ---");
            continue;
        }
        eprintln!("\n--- phase 2: traffic TG {tg} @ {freq} Hz ---");
        let nco_t = (*freq as f64 - center_hz) + ppm_corr_hz;
        let handler = Arc::new(CapturingVoiceHandler {
            decoder: Mutex::new(ImbeDecoder::new()),
            pcm: Mutex::new(Vec::new()),
            counters: Mutex::new(Counters::default()),
        });
        let (framer, dibits, metrics) =
            decode_one_freq(&iq, sr, nco_t, Some(handler.clone()), mode);
        let counters = handler.counters.lock().unwrap();
        let pcm = handler.pcm.lock().unwrap();
        eprintln!("  dibits={}  hard_sync={}  soft_sync={}",
            dibits.len(),
            metrics.hard_events.len(),
            metrics.soft_events.len());
        eprintln!("  framer: hdu={} ldu1={} ldu2={} tdu={} tdu_lc={}",
            framer.hdu_count, framer.ldu1_count, framer.ldu2_count,
            framer.tdu_count, framer.tdu_lc_count);
        eprintln!("  IMBE: total={} silent={}",
            counters.imbe_total, counters.silent_frames);
        eprintln!("  PCM: {} samples ({:.2}s)",
            pcm.len(), pcm.len() as f32 / 8000.0);

        let tag = format!("traffic_{}_tg{}", freq, tg);
        let dibits_path = format!("{out_dir}/{tag}_dibits.bits");
        let _ = std::fs::write(&dibits_path, pack_dibits_msb(&dibits));
        let wav_path = format!("{out_dir}/{tag}.wav");
        write_mono_wav_8khz(&wav_path, &pcm);
        eprintln!("  wrote {dibits_path} + {tag}.wav");
    }
    eprintln!("\n=== full_chain done ===");
}

/// Compact JSON-formatted summary stats for an f32 vector. Stable
/// schema regardless of vector length: `{n,mean,stddev,min,max,p10,p50,p90,slope_per_kS}`.
/// `slope_per_kS` is the linear-regression slope per 1000 samples —
/// useful for detecting Costas drift over time.
fn vector_stats(v: &[f32]) -> String {
    if v.is_empty() {
        return r#"{"n":0,"mean":0,"stddev":0,"min":0,"max":0,"p10":0,"p50":0,"p90":0,"slope_per_kS":0}"#.to_string();
    }
    let n = v.len();
    let mut sorted: Vec<f32> = v.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean: f64 = v.iter().map(|x| *x as f64).sum::<f64>() / n as f64;
    let var: f64 = v.iter().map(|x| { let d = *x as f64 - mean; d * d }).sum::<f64>() / n as f64;
    let stddev = var.sqrt();
    let min = sorted.first().copied().unwrap_or(0.0);
    let max = sorted.last().copied().unwrap_or(0.0);
    let p10 = sorted[(n / 10).min(n - 1)];
    let p50 = sorted[n / 2];
    let p90 = sorted[(9 * n / 10).min(n - 1)];
    // Linear regression slope (per sample). Multiply by 1000 for per-kSample.
    let mean_x = (n - 1) as f64 / 2.0;
    let mut num = 0.0_f64;
    let mut den = 0.0_f64;
    for (i, &y) in v.iter().enumerate() {
        let dx = i as f64 - mean_x;
        num += dx * (y as f64 - mean);
        den += dx * dx;
    }
    let slope = if den > 0.0 { num / den } else { 0.0 };
    let slope_per_ks = slope * 1000.0;
    format!(
        r#"{{"n":{},"mean":{:.6},"stddev":{:.6},"min":{:.6},"max":{:.6},"p10":{:.6},"p50":{:.6},"p90":{:.6},"slope_per_kS":{:.6}}}"#,
        n, mean, stddev, min, max, p10, p50, p90, slope_per_ks)
}

//! Alert tones in a call's decoded audio: a dispatch console's attention signals (a hi-lo
//! warble, a pulsed beep, a steady tone) and two-tone sequential pages. Speech is never one pure
//! tone for long, so a tone sequence is told from it by its frames alone.
//!
//! Each 20 ms frame ends a 64 ms window (Hann, real FFT). The window is a tone when one peak
//! from 280 Hz to 3.6 kHz (its bin and two either side) holds 85 % of the energy from 100 Hz to
//! 3.6 kHz, above the floor; counting from 100 Hz takes in a voice's fundamental, so a voice's
//! strong second harmonic is not a tone. Tone frames within 2 % of each other make a segment; segments less than 300 ms apart
//! make a sequence. A sequence is an alert once it shows a pattern speech does not make:
//!
//! - two different tones back to back, the second for 160 ms (a warble, or a page's tones A
//!   and B);
//! - one tone twice, each for 120 ms, 60 to 300 ms apart (a pulsed beep);
//! - one tone for 500 ms.
//!
//! The vocoder rebuilds a tone as a harmonic of its pitch, so a tone comes out within about 1 %
//! of what was sent (P25's IMBE: a multiple of 16 kHz / (b0 + 39.5)).

use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex;
use serde::Serialize;

use super::codec::SAMPLES_PER_FRAME;

const RATE: f32 = 8_000.0;
/// The analysis window: 64 ms, moved on one frame at a time.
const WIN: usize = 512;
const BIN_HZ: f32 = RATE / WIN as f32;
/// The band whose energy a tone holds, and the lowest tone (below a two-tone page's lowest,
/// 288.5 Hz; a raised voice's fundamental reaches 250 Hz).
const F_FLOOR: f32 = 100.0;
const F_HIGH: f32 = 3_600.0;
const F_LOW: f32 = 280.0;
/// Bins either side of the peak that hold a tone (Hann's main lobe).
const LOBE: usize = 2;
/// The share of the band's energy the peak holds in a tone.
const TONALITY: f32 = 0.85;
/// RMS (16-bit units) below which a window is silence: about -50 dBFS.
const FLOOR_RMS: f32 = 100.0;
/// A tone frame within this fraction of a segment's tone continues it.
const SAME: f32 = 0.02;
/// Tones further apart than this fraction are different tones.
const DIFFERENT: f32 = 0.04;
/// Frames a segment needs to count (a call's first tone is often clipped).
const MIN_SEGMENT: u32 = 3;
/// Frames with no tone that end a sequence (300 ms).
const SEQUENCE_GAP: u32 = 15;
const FRAME_MS: u32 = 20;
/// A warble or page: the second tone lasts this long (160 ms), at most this many frames after
/// the first ends.
const SWITCH_FRAMES: u32 = 8;
const SWITCH_GAP: u32 = 3;
/// A pulsed beep: each pulse this long (120 ms), this far apart (60 to 300 ms).
const PULSE_FRAMES: u32 = 6;
const PULSE_GAP: std::ops::RangeInclusive<u32> = 3..=15;
/// A steady tone: this long (500 ms).
const STEADY_FRAMES: u32 = 25;
/// A two-tone page: each tone at least this long (400 ms); a warble's are shorter.
const PAGE_FRAMES: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AlertKind {
    /// Two tones alternating (a console's hi-lo).
    Warble,
    /// One tone repeated with gaps.
    Pulsed,
    /// One long tone.
    Steady,
    /// Two long tones in turn: a two-tone sequential page (tone A, then tone B).
    TwoTone,
    /// Three tones or more.
    Tones,
}

impl AlertKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AlertKind::Warble => "warble",
            AlertKind::Pulsed => "pulsed",
            AlertKind::Steady => "steady",
            AlertKind::TwoTone => "two_tone",
            AlertKind::Tones => "tones",
        }
    }
}

/// One alert in a call, as heard so far.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ToneAlert {
    /// Numbers the lane's alerts: with the call, it names one.
    pub seq: u32,
    pub kind: AlertKind,
    /// The distinct tones in the order first heard, Hz.
    pub tones_hz: Vec<f32>,
    /// Its tones, counting each repeat.
    pub segments: u32,
    /// From the call's first audio to its first tone.
    pub offset_ms: u32,
    /// From its first tone's start to its last tone's end.
    pub duration_ms: u32,
}

#[derive(Debug, Clone, Copy)]
struct Segment {
    /// The call's frame it started in.
    start: u32,
    frames: u32,
    sum_hz: f64,
    /// Its first and last frames' tones: their windows hold the edges.
    first_hz: f32,
    last_hz: f32,
}

impl Segment {
    fn new(start: u32, hz: f32) -> Segment {
        Segment { start, frames: 1, sum_hz: f64::from(hz), first_hz: hz, last_hz: hz }
    }

    fn hz(&self) -> f32 {
        (self.sum_hz / f64::from(self.frames)) as f32
    }

    /// The sum and count of its frames' tones without the edges, when it has others.
    fn inner(&self) -> (f64, u32) {
        if self.frames > 2 {
            (self.sum_hz - f64::from(self.first_hz) - f64::from(self.last_hz), self.frames - 2)
        } else {
            (self.sum_hz, self.frames)
        }
    }

    fn end(&self) -> u32 {
        self.start + self.frames
    }
}

fn differ(a: f32, b: f32) -> bool {
    (a - b).abs() > DIFFERENT * a.max(b)
}

/// One lane's detector; `reset` for each call.
pub struct AlertDetector {
    fft: Arc<dyn RealToComplex<f32>>,
    hann: Vec<f32>,
    /// The last `WIN` samples, oldest first.
    samples: Vec<f32>,
    input: Vec<f32>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    /// Frames of this call so far.
    frame: u32,
    current: Option<Segment>,
    /// The sequence's segments before `current`.
    done: Vec<Segment>,
    seq: u32,
    /// The sequence is an alert: its number.
    alert: Option<u32>,
}

impl Default for AlertDetector {
    fn default() -> Self {
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(WIN);
        let hann = (0..WIN).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WIN as f32).cos()).collect();
        AlertDetector {
            input: fft.make_input_vec(),
            spectrum: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            fft,
            hann,
            samples: vec![0.0; WIN],
            frame: 0,
            current: None,
            done: Vec::new(),
            seq: 0,
            alert: None,
        }
    }
}

impl AlertDetector {
    /// A new call.
    pub fn reset(&mut self) {
        self.samples.fill(0.0);
        self.frame = 0;
        self.current = None;
        self.done.clear();
        self.alert = None;
    }

    /// Take the call's next frame; while its frames make an alert, the alert so far.
    pub fn frame(&mut self, pcm: &[i16; SAMPLES_PER_FRAME]) -> Option<ToneAlert> {
        self.samples.copy_within(SAMPLES_PER_FRAME.., 0);
        for (d, s) in self.samples[WIN - SAMPLES_PER_FRAME..].iter_mut().zip(pcm) {
            *d = f32::from(*s);
        }
        let i = self.frame;
        self.frame += 1;
        let tone = self.tone();
        self.track(i, tone);
        self.summary()
    }

    /// The window's tone, Hz, if it is one.
    fn tone(&mut self) -> Option<f32> {
        let mean_sq = self.samples.iter().map(|v| v * v).sum::<f32>() / WIN as f32;
        if mean_sq < FLOOR_RMS * FLOOR_RMS {
            return None;
        }
        for ((d, s), w) in self.input.iter_mut().zip(&self.samples).zip(&self.hann) {
            *d = s * w;
        }
        self.fft.process_with_scratch(&mut self.input, &mut self.spectrum, &mut self.scratch).ok()?;
        let (lo, hi) = ((F_FLOOR / BIN_HZ).ceil() as usize, (F_HIGH / BIN_HZ).floor() as usize);
        let power = |b: usize| self.spectrum[b].norm_sqr();
        let (mut total, mut k, mut best) = (0.0f32, lo, 0.0f32);
        for b in lo..=hi {
            let p = power(b);
            total += p;
            if p > best {
                best = p;
                k = b;
            }
        }
        if total <= 0.0 || k <= lo || k >= hi {
            return None;
        }
        let peak: f32 = (k - LOBE..=k + LOBE).map(power).sum();
        if peak < TONALITY * total {
            return None;
        }
        // Quadratic interpolation of the log power about the peak bin.
        let ln = |b: usize| power(b).max(1e-12).ln();
        let (a, b, c) = (ln(k - 1), ln(k), ln(k + 1));
        let den = a - 2.0 * b + c;
        let d = if den.abs() > 1e-9 { (0.5 * (a - c) / den).clamp(-0.5, 0.5) } else { 0.0 };
        let hz = (k as f32 + d) * BIN_HZ;
        (hz >= F_LOW).then_some(hz)
    }

    fn track(&mut self, i: u32, tone: Option<f32>) {
        if let (Some(hz), Some(s)) = (tone, self.current.as_mut()) {
            if (hz - s.hz()).abs() <= SAME * s.hz() {
                s.frames += 1;
                s.sum_hz += f64::from(hz);
                s.last_hz = hz;
                return;
            }
        }
        if let Some(s) = self.current.take() {
            if s.frames >= MIN_SEGMENT {
                self.done.push(s);
            }
        }
        match tone {
            Some(hz) => self.current = Some(Segment::new(i, hz)),
            None => {
                if self.done.last().is_some_and(|last| i + 1 - last.end() > SEQUENCE_GAP) {
                    self.done.clear();
                    self.alert = None;
                }
            }
        }
    }

    fn summary(&mut self) -> Option<ToneAlert> {
        let segs: Vec<Segment> = self.done.iter().copied().chain(self.current.filter(|s| s.frames >= MIN_SEGMENT)).collect();
        if segs.is_empty() {
            return None;
        }
        if self.alert.is_none() {
            if !is_alert(&segs) {
                return None;
            }
            self.seq += 1;
            self.alert = Some(self.seq);
        }
        Some(describe(self.seq, &segs))
    }
}

fn is_alert(segs: &[Segment]) -> bool {
    let switch = segs.windows(2).any(|p| {
        differ(p[0].hz(), p[1].hz()) && p[1].start - p[0].end() <= SWITCH_GAP && p[1].frames >= SWITCH_FRAMES
    });
    let pulsed = segs.windows(2).any(|p| {
        !differ(p[0].hz(), p[1].hz())
            && p[0].frames >= PULSE_FRAMES
            && p[1].frames >= PULSE_FRAMES
            && PULSE_GAP.contains(&(p[1].start - p[0].end()))
    });
    switch || pulsed || segs.iter().any(|s| s.frames >= STEADY_FRAMES)
}

fn describe(seq: u32, segs: &[Segment]) -> ToneAlert {
    // Each distinct tone's frames and their sum.
    let mut tones: Vec<(f64, u32)> = Vec::new();
    for s in segs {
        let (s_sum, s_n) = s.inner();
        match tones.iter_mut().find(|(sum, n)| !differ((*sum / f64::from(*n)) as f32, s.hz())) {
            Some((sum, n)) => {
                *sum += s_sum;
                *n += s_n;
            }
            None => tones.push((s_sum, s_n)),
        }
    }
    let kind = match tones.len() {
        1 if segs.len() == 1 => AlertKind::Steady,
        1 => AlertKind::Pulsed,
        2 if segs.len() == 2 && segs.iter().all(|s| s.frames >= PAGE_FRAMES) => AlertKind::TwoTone,
        2 => AlertKind::Warble,
        _ => AlertKind::Tones,
    };
    let (first, last) = (segs[0], segs[segs.len() - 1]);
    ToneAlert {
        seq,
        kind,
        tones_hz: tones.iter().map(|(sum, n)| ((sum / f64::from(*n)) * 10.0).round() as f32 / 10.0).collect(),
        segments: segs.len() as u32,
        // A frame's window is centred 32 ms before the frame's end.
        offset_ms: ((first.start + 1) * FRAME_MS).saturating_sub(32),
        duration_ms: (last.end() - first.start) * FRAME_MS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 8 kHz audio from (Hz or 0 for silence, ms) pieces, at `amp`.
    fn tones(pieces: &[(f32, u32)], amp: f32) -> Vec<i16> {
        let mut out = Vec::new();
        let mut phase = 0.0f32;
        for &(hz, ms) in pieces {
            for _ in 0..ms * 8 {
                out.push(if hz > 0.0 { (amp * phase.sin()) as i16 } else { 0 });
                phase = (phase + std::f32::consts::TAU * hz / RATE) % std::f32::consts::TAU;
            }
        }
        out
    }

    /// Voiced speech-like audio: a 110-170 Hz pitch gliding, its harmonics shaped by moving
    /// formants, syllables of 150-300 ms.
    fn speech(ms: u32) -> Vec<i16> {
        let mut out = Vec::new();
        let mut phase = 0.0f32;
        for n in 0..ms * 8 {
            let t = n as f32 / RATE;
            let f0 = 140.0 + 30.0 * (t * 2.7).sin();
            phase += std::f32::consts::TAU * f0 / RATE;
            let f1 = 500.0 + 250.0 * (t * 5.1).sin();
            let f2 = 1500.0 + 500.0 * (t * 3.3).cos();
            let mut v = 0.0;
            for h in 1..25 {
                let fh = f0 * h as f32;
                let g = (-((fh - f1) / 180.0).powi(2)).exp() + 0.5 * (-((fh - f2) / 250.0).powi(2)).exp() + 0.05;
                v += g * (phase * h as f32).sin();
            }
            let syllable = (t * 4.0 * std::f32::consts::PI).sin().abs();
            out.push((v * syllable * 2500.0) as i16);
        }
        out
    }

    /// Every alert the detector reports, the last summary of each.
    fn run(pcm: &[i16]) -> Vec<ToneAlert> {
        let mut d = AlertDetector::default();
        let mut out: Vec<ToneAlert> = Vec::new();
        for f in pcm.chunks_exact(SAMPLES_PER_FRAME) {
            if let Some(a) = d.frame(f.try_into().unwrap()) {
                match out.last_mut() {
                    Some(last) if last.seq == a.seq => *last = a,
                    _ => out.push(a),
                }
            }
        }
        out
    }

    /// "warble 1505.9/806.7 Hz".
    fn label(a: &ToneAlert) -> String {
        let tones: Vec<String> = a.tones_hz.iter().map(|f| format!("{f:.1}")).collect();
        format!("{} {} Hz", a.kind.as_str().replace('_', "-"), tones.join("/"))
    }

    fn near(got: &[f32], want: &[f32]) -> bool {
        got.len() == want.len() && got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1.5)
    }

    #[test]
    fn a_console_warble_then_speech() {
        let mut pcm = tones(&[(0.0, 100), (806.7, 160), (1505.9, 240), (806.7, 240), (1505.9, 240), (806.7, 240), (0.0, 200)], 9000.0);
        pcm.extend(speech(3000));
        let got = run(&pcm);
        assert_eq!(got.len(), 1, "{got:?}");
        let a = &got[0];
        assert_eq!((a.kind, a.segments), (AlertKind::Warble, 5));
        assert!(near(&a.tones_hz, &[806.7, 1505.9]), "{:?}", a.tones_hz);
        assert!((80..=140).contains(&a.offset_ms), "offset {}", a.offset_ms);
        assert!((1060..=1180).contains(&a.duration_ms), "duration {}", a.duration_ms);
        assert_eq!(label(a), format!("warble {:.1}/{:.1} Hz", a.tones_hz[0], a.tones_hz[1]));
    }

    #[test]
    fn pulsed_and_steady_beeps() {
        let pulsed = run(&tones(&[(1010.5, 280), (0.0, 220), (1010.5, 280), (0.0, 220), (1010.5, 280), (0.0, 300)], 9000.0));
        assert_eq!(pulsed.len(), 1, "{pulsed:?}");
        assert_eq!((pulsed[0].kind, pulsed[0].segments), (AlertKind::Pulsed, 3));
        assert!(near(&pulsed[0].tones_hz, &[1010.5]), "{:?}", pulsed[0].tones_hz);
        let steady = run(&tones(&[(1010.5, 1060), (0.0, 400)], 9000.0));
        assert_eq!(steady.len(), 1);
        assert_eq!(steady[0].kind, AlertKind::Steady);
    }

    #[test]
    fn a_two_tone_page_and_its_tones() {
        let got = run(&tones(&[(0.0, 200), (349.0, 1000), (600.9, 3000), (0.0, 500)], 9000.0));
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].kind, AlertKind::TwoTone);
        assert!(near(&got[0].tones_hz, &[349.0, 600.9]), "{:?}", got[0].tones_hz);
        assert!((3900..=4100).contains(&got[0].duration_ms), "{}", got[0].duration_ms);
    }

    #[test]
    fn speech_noise_and_short_tones_are_not_alerts() {
        assert!(run(&speech(20_000)).is_empty());
        let mut seed = 12345u32;
        let noise: Vec<i16> = (0..80_000)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((seed >> 16) as i16) / 4
            })
            .collect();
        assert!(run(&noise).is_empty());
        // A 300 ms beep (a radio's talk-permit) and a quiet steady tone under the floor.
        assert!(run(&tones(&[(1000.0, 300), (0.0, 500)], 9000.0)).is_empty());
        assert!(run(&tones(&[(1000.0, 1000)], 60.0)).is_empty());
    }

    /// The alerts in every recording in `ALERT_WAV_DIR` (a unit's recordings), a line each.
    #[test]
    #[ignore]
    fn recordings_in_a_directory() {
        let Ok(dir) = std::env::var("ALERT_WAV_DIR") else { return };
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "wav"))
            .collect();
        paths.sort();
        let mut n = 0;
        for p in &paths {
            let bytes = std::fs::read(p).unwrap();
            let pcm: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
            for a in run(&pcm) {
                n += 1;
                let name = p.file_name().unwrap().to_string_lossy();
                println!("{name} {} at {} ms for {} ms, {} tones", label(&a), a.offset_ms, a.duration_ms, a.segments);
            }
        }
        println!("{n} alerts in {} recordings", paths.len());
    }

    #[test]
    fn two_alerts_in_one_call_are_two() {
        let mut pcm = tones(&[(806.7, 240), (1505.9, 240), (806.7, 240)], 9000.0);
        pcm.extend(speech(2000));
        pcm.extend(tones(&[(1010.5, 280), (0.0, 220), (1010.5, 280)], 9000.0));
        let got = run(&pcm);
        assert_eq!(got.iter().map(|a| (a.seq, a.kind)).collect::<Vec<_>>(), [(1, AlertKind::Warble), (2, AlertKind::Pulsed)]);
    }
}

//! A channel cut from the radio's window in software: the window's raw IQ mixed down to a
//! frequency in it, low-pass filtered and decimated to 250 kSPS. That is the rate the cheap
//! transmitters of the data bands are listened to at: they sit up to 100 kHz off their nominal
//! frequency, and a burst's IQ is kept at it. The channel is flat to about 90 kHz either side
//! and free of aliases there.

use anyhow::{ensure, Result};
use rustfft::num_complex::Complex32;

use crate::dsp::run::dot;

/// The channel's sample rate.
pub const RATE_HZ: u32 = 250_000;
/// Flat and alias-free to this far either side of the centre.
pub const PASS_HZ: f64 = 90_000.0;
/// Filter taps for each window sample dropped: with a Hamming window the transition from pass
/// to stop is 3.3 / 12 of the channel's rate, 69 kHz, centred on its Nyquist frequency.
const TAPS_PER_DECIMATION: usize = 12;
/// The most samples the mixer's turns may take to repeat: a channel a round number of hertz
/// from the LO repeats in a handful.
const TURNS_MOST: u64 = 1 << 16;
/// One count of the ADC's 12 bits in the channel's 16-bit samples.
pub const ADC_TO_I16: f32 = 16.0;

pub struct Channel {
    decimation: usize,
    taps: Vec<f32>,
    /// The mixer's phasor at each sample of its period (six samples for a channel 500 kHz from
    /// the LO of a 3 MSPS window), and the one the next sample takes: exact for as long as it
    /// runs, with no phasor to carry and keep at length one.
    turns: Vec<Complex32>,
    turn: usize,
    /// The mixed samples the filter has not passed yet: its history, then the new ones.
    re: Vec<f32>,
    im: Vec<f32>,
}

impl Channel {
    /// A channel centred `offset_hz` from the LO of a window sampled at `window_rate_hz`.
    pub fn new(window_rate_hz: u32, offset_hz: f64) -> Result<Channel> {
        ensure!(window_rate_hz % RATE_HZ == 0 && window_rate_hz >= 2 * RATE_HZ, "a {window_rate_hz} SPS window does not divide down to {RATE_HZ} SPS");
        ensure!(offset_hz.abs() + f64::from(RATE_HZ) / 2.0 <= f64::from(window_rate_hz) / 2.0, "a channel {offset_hz} Hz from the LO is outside the window");
        ensure!(offset_hz.fract() == 0.0, "a channel {offset_hz} Hz from the LO: a whole number of hertz");
        let decimation = (window_rate_hz / RATE_HZ) as usize;
        let period = u64::from(window_rate_hz) / gcd(u64::from(window_rate_hz), offset_hz.abs() as u64);
        ensure!(period <= TURNS_MOST, "a channel {offset_hz} Hz from the LO of a {window_rate_hz} SPS window: its mixer repeats only every {period} samples");
        let angle = -2.0 * std::f64::consts::PI * offset_hz / f64::from(window_rate_hz);
        // A quarter turn is exactly one or nought.
        let snap = |v: f64| if v.abs() < 1e-9 { 0.0 } else { v as f32 };
        Ok(Channel {
            decimation,
            taps: low_pass(TAPS_PER_DECIMATION * decimation + 1, 0.5 / decimation as f64),
            turns: (0..period).map(|k| Complex32::new(snap((angle * k as f64).cos()), snap((angle * k as f64).sin()))).collect(),
            turn: 0,
            re: Vec::new(),
            im: Vec::new(),
        })
    }

    /// Window samples for each channel sample.
    pub fn decimation(&self) -> usize {
        self.decimation
    }

    /// The filter's delay in window samples: channel sample k is centred on window sample
    /// k × decimation + this, counted from the first one pushed.
    pub fn delay(&self) -> usize {
        (self.taps.len() - 1) / 2
    }

    /// Forget what was pushed: the next sample is the first again (after a gap in the window).
    pub fn reset(&mut self) {
        self.turn = 0;
        self.re.clear();
        self.im.clear();
    }

    /// More of the window: interleaved little-endian i16 I, Q as the ADC gave them. The channel's
    /// samples they complete are added to `out`, in ADC counts.
    pub fn push(&mut self, window: &[u8], out: &mut Vec<Complex32>) {
        let held = self.re.len();
        self.re.resize(held + window.len() / 4, 0.0);
        self.im.resize(held + window.len() / 4, 0.0);
        let mut turn = self.turn;
        for ((s, re), im) in window.chunks_exact(4).zip(&mut self.re[held..]).zip(&mut self.im[held..]) {
            let i = f32::from(i16::from_le_bytes([s[0], s[1]]));
            let q = f32::from(i16::from_le_bytes([s[2], s[3]]));
            let p = self.turns[turn];
            *re = i * p.re - q * p.im;
            *im = i * p.im + q * p.re;
            turn += 1;
            if turn == self.turns.len() {
                turn = 0;
            }
        }
        self.turn = turn;
        let n = self.taps.len();
        let mut at = 0;
        while at + n <= self.re.len() {
            out.push(Complex32::new(dot(&self.taps, &self.re[at..at + n]), dot(&self.taps, &self.im[at..at + n])));
            at += self.decimation;
        }
        self.re.drain(..at);
        self.im.drain(..at);
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// A Hamming-windowed low-pass of `n` taps, its cut-off (half its gain) at `cutoff` of the
/// sample rate, with a gain of one at 0 Hz.
pub(crate) fn low_pass(n: usize, cutoff: f64) -> Vec<f32> {
    let mid = (n - 1) as f64 / 2.0;
    let taps: Vec<f64> = (0..n)
        .map(|i| {
            let x = i as f64 - mid;
            let sinc = if x == 0.0 { 2.0 * cutoff } else { (2.0 * std::f64::consts::PI * cutoff * x).sin() / (std::f64::consts::PI * x) };
            sinc * (0.54 - 0.46 * (2.0 * std::f64::consts::PI * i as f64 / (n - 1) as f64).cos())
        })
        .collect();
    let sum: f64 = taps.iter().sum();
    taps.iter().map(|t| (t / sum) as f32).collect()
}

/// A channel sample as the 16-bit pair a capture keeps.
pub fn to_i16(s: Complex32) -> [i16; 2] {
    let c = |v: f32| (v * ADC_TO_I16).round().clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16;
    [c(s.re), c(s.im)]
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A window of a tone `hz` from the LO, `amplitude` ADC counts, as the ADC's bytes.
    pub fn tone(rate: u32, hz: f64, amplitude: f64, samples: usize) -> Vec<u8> {
        (0..samples)
            .flat_map(|n| {
                let a = 2.0 * std::f64::consts::PI * hz * n as f64 / f64::from(rate);
                let (i, q) = ((amplitude * a.cos()).round() as i16, (amplitude * a.sin()).round() as i16);
                [i.to_le_bytes(), q.to_le_bytes()].concat()
            })
            .collect()
    }

    fn level_db(rate: u32, offset: f64, hz: f64) -> f64 {
        let mut ch = Channel::new(rate, offset).unwrap();
        let mut out = Vec::new();
        ch.push(&tone(rate, hz, 1000.0, 200_000), &mut out);
        let settled = &out[out.len() / 2..];
        let power: f64 = settled.iter().map(|s| f64::from(s.norm_sqr())).sum::<f64>() / settled.len() as f64;
        10.0 * (power / 1e6).log10()
    }

    #[test]
    fn the_channel_is_flat_in_its_pass_band_and_shuts_out_what_would_alias_into_it() {
        // The spot 500 kHz under the LO of a 3 MSPS window, as data mode's 433 window has it.
        let (rate, offset) = (3_000_000, -500_000.0);
        for off in [0.0, 40_000.0, -40_000.0, 90_000.0, -90_000.0] {
            let db = level_db(rate, offset, offset + off);
            assert!(db.abs() < 0.5, "{off} Hz from the centre: {db:.2} dB");
        }
        // What folds onto the pass band at 250 kSPS: 160 to 340 kHz from the centre, either side.
        for off in [160_000.0, 250_000.0, 340_000.0, -160_000.0, -250_000.0, 500_000.0] {
            let db = level_db(rate, offset, offset + off);
            assert!(db < -45.0, "{off} Hz from the centre: {db:.1} dB");
        }
    }

    #[test]
    fn a_tone_comes_out_at_its_offset_from_the_centre() {
        let (rate, offset) = (2_500_000, -500_000.0);
        let mut ch = Channel::new(rate, offset).unwrap();
        assert_eq!((ch.decimation(), ch.delay()), (10, 60));
        let mut out = Vec::new();
        ch.push(&tone(rate, offset - 40_000.0, 1000.0, 100_000), &mut out);
        assert_eq!(out.len(), (100_000 - 121) / 10 + 1);
        // The phase steps by the tone's offset at the channel's rate.
        let step: f64 = out[5000..6000].windows(2).map(|w| f64::from((w[1] * w[0].conj()).arg())).sum::<f64>() / 999.0;
        let hz = step * f64::from(RATE_HZ) / (2.0 * std::f64::consts::PI);
        assert!((hz + 40_000.0).abs() < 5.0, "{hz} Hz");
    }

    #[test]
    fn blocks_of_any_size_give_the_same_samples() {
        let rate = 2_000_000;
        let window = tone(rate, -460_000.0, 800.0, 50_000);
        let mut whole = Vec::new();
        Channel::new(rate, -500_000.0).unwrap().push(&window, &mut whole);
        let mut ch = Channel::new(rate, -500_000.0).unwrap();
        let mut pieces = Vec::new();
        for piece in window.chunks(4 * 777) {
            ch.push(piece, &mut pieces);
        }
        assert_eq!(whole, pieces);
        // After a reset the channel starts over.
        ch.reset();
        let mut again = Vec::new();
        ch.push(&window, &mut again);
        assert_eq!(whole, again);
    }

    #[test]
    fn a_window_that_does_not_divide_or_a_channel_outside_it_is_refused() {
        assert!(Channel::new(2_100_000, 0.0).is_err());
        assert!(Channel::new(250_000, 0.0).is_err());
        assert!(Channel::new(2_000_000, 900_000.0).is_err());
        assert!(Channel::new(2_000_000, -875_000.0).is_ok());
        // The mixer's turns repeat: in six samples for the 433 window, in one at the LO; a
        // channel a hertz off a round offset would take the whole second.
        assert_eq!(Channel::new(3_000_000, -500_000.0).unwrap().turns.len(), 6);
        assert_eq!(Channel::new(2_500_000, 0.0).unwrap().turns.len(), 1);
        assert!(Channel::new(3_000_000, -500_001.0).is_err() && Channel::new(3_000_000, -500_000.5).is_err());
    }

    #[test]
    fn a_capture_s_samples_use_the_sixteen_bits() {
        assert_eq!(to_i16(Complex32::new(2047.0, -2048.0)), [32752, -32768]);
        assert_eq!(to_i16(Complex32::new(0.03, 4000.0)), [0, 32767]);
    }
}

//! Analog receivers: what the analog module makes of a 250 kSPS channel cut around a carrier
//! (`ism::channel`, the cut data mode makes too). Wide FM (broadcast), narrow FM (two-way radio,
//! weather radio) and AM (aircraft), each to audio at 31.25 kHz. Narrow FM and AM are filtered
//! to their own width first; a meter on the channel's spectrum tells how far the carrier is over
//! the floor beside it, which opens and shuts their squelch.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};
use serde::{Deserialize, Serialize};

use super::ism::channel::{low_pass, RATE_HZ as CHANNEL_HZ};
use crate::dsp::run::dot;

/// Channel samples for each sample of audio.
const DECIMATION: usize = 8;
/// The audio's sample rate: 31.25 kHz.
pub const AUDIO_HZ: u32 = CHANNEL_HZ / DECIMATION as u32;
/// Full scale of the channel's samples (the ADC's 12 bits).
const FULL_SCALE: f32 = 2048.0;

/// Wide FM: the deviation that is full scale of the audio, its de-emphasis (the Americas'), and
/// the audio filter (at the channel's rate: half its gain at 13.5 kHz, shut by the stereo
/// pilot at 19 kHz, which would otherwise fold down to 12 kHz).
const WFM_DEVIATION_HZ: f32 = 75_000.0;
const WFM_DEEMPHASIS_S: f32 = 75e-6;
const WFM_TAPS: usize = 128;
const WFM_AUDIO_HZ: f64 = 13_500.0;
const WFM_GAIN: f32 = 0.7;

/// Narrow FM and AM: the filter to the signal's own width (at the channel's rate), then at the
/// audio's rate the voice band: over 250 Hz (under it are sub-audible tones and a carrier's
/// offset) and under 3.4 kHz.
const IF_TAPS: usize = 256;
const VOICE_TAPS: usize = 64;
const VOICE_HZ: f64 = 3_400.0;
const VOICE_HIGH_PASS_HZ: f32 = 250.0;
/// Narrow FM: the deviation that is full scale before de-emphasis, a de-emphasis of 6 dB an
/// octave from 300 Hz as two-way radio has it, and the gain that brings a voice back up.
const NFM_DEVIATION_HZ: f32 = 5_000.0;
const NFM_DEEMPHASIS_S: f32 = 530e-6;
const NFM_GAIN: f32 = 2.0;
/// AM: the carrier's level is followed over a tenth of a second; full modulation is this much of
/// full scale.
const AM_CARRIER_S: f32 = 0.1;
const AM_GAIN: f32 = 0.8;

/// The meter: a spectrum of this many points (bins of 977 Hz) taken this often and averaged
/// over `METER_FRAMES` (33 ms). The floor is the median bin from `FLOOR_FROM_HZ` out to where the
/// channel is still flat.
const METER_POINTS: usize = 256;
const METER_EVERY: usize = 2048;
const METER_FRAMES: usize = 4;
const FLOOR_FROM_HZ: f32 = 20_000.0;
const FLOOR_TO_HZ: f32 = 85_000.0;
/// The median of bins that are each the mean of four noise powers is this much of their mean.
const MEDIAN_OF_MEAN: f32 = 0.918;
/// The squelch shuts once the carrier has been this far under its opening for this many
/// readings of the meter (a third of a second).
const SQUELCH_SLACK_DB: f32 = 2.0;
const SQUELCH_HANG: u32 = 10;

/// How a signal is demodulated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Demod {
    /// Wide FM: broadcast radio.
    Wfm,
    /// Narrow FM: two-way radio, amateur repeaters, weather radio.
    Nfm,
    /// AM: aircraft.
    Am,
}

impl Demod {
    /// Half the width the receiver passes, Hz. Wide FM takes the channel whole.
    pub fn half_hz(self) -> f64 {
        match self {
            Demod::Wfm => 100_000.0,
            Demod::Nfm => 7_500.0,
            Demod::Am => 5_000.0,
        }
    }

    /// Narrow FM and AM have a squelch; wide FM fills its channel, with no floor beside it to
    /// measure.
    pub fn has_squelch(self) -> bool {
        self != Demod::Wfm
    }
}

/// What the meter last read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Level {
    /// The power within the receiver's width, dB of full scale.
    pub level_dbfs: f32,
    /// That power over the floor's in the same width; none for wide FM.
    pub over_db: Option<f32>,
    /// Audio is passed: the squelch is open, or there is none.
    pub open: bool,
}

/// A one-pole low-pass's step for a time constant at a sample rate.
fn step(seconds: f32, rate_hz: f32) -> f32 {
    1.0 - (-1.0 / (seconds * rate_hz)).exp()
}

/// A filter that takes a run of samples and gives one out for every `every` in.
struct Fir {
    taps: Vec<f32>,
    every: usize,
    held: Vec<f32>,
}

impl Fir {
    fn new(taps: Vec<f32>, every: usize) -> Fir {
        Fir { held: Vec::with_capacity(4 * taps.len()), taps, every }
    }

    fn push(&mut self, sample: f32) {
        self.held.push(sample);
    }

    /// Each output the held samples give, in order; what is still needed is kept.
    fn drain(&mut self, mut out: impl FnMut(f32)) {
        let n = self.taps.len();
        let mut at = 0;
        while at + n <= self.held.len() {
            out(dot(&self.taps, &self.held[at..at + n]));
            at += self.every;
        }
        self.held.drain(..at.min(self.held.len()));
    }
}

/// The channel's spectrum, read for the level within the receiver's width and the floor beside
/// it.
struct Meter {
    fft: Arc<dyn Fft<f32>>,
    window: Vec<f32>,
    /// Samples since the last frame was taken.
    since: usize,
    frame: Vec<Complex32>,
    power: Vec<f32>,
    frames: usize,
    /// Bins within the receiver's width, and the bins the floor is read from.
    inside: Vec<usize>,
    beside: Vec<usize>,
}

impl Meter {
    fn new(half_hz: f64) -> Meter {
        let bin_hz = CHANNEL_HZ as f32 / METER_POINTS as f32;
        let hz = |k: usize| (if k < METER_POINTS / 2 { k as f32 } else { k as f32 - METER_POINTS as f32 }) * bin_hz;
        Meter {
            fft: FftPlanner::new().plan_fft_forward(METER_POINTS),
            window: (0..METER_POINTS).map(|n| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * n as f32 / METER_POINTS as f32).cos()).collect(),
            since: METER_EVERY,
            frame: Vec::with_capacity(METER_POINTS),
            power: vec![0.0; METER_POINTS],
            frames: 0,
            inside: (0..METER_POINTS).filter(|&k| f64::from(hz(k).abs()) <= half_hz).collect(),
            beside: (0..METER_POINTS).filter(|&k| (FLOOR_FROM_HZ..=FLOOR_TO_HZ).contains(&hz(k).abs())).collect(),
        }
    }

    /// Take a channel sample; a reading (the power inside the width, and the floor's in the
    /// same width, both as mean squares of a sample) each time `METER_FRAMES` spectra are in.
    fn push(&mut self, sample: Complex32) -> Option<(f32, f32)> {
        self.since += 1;
        if self.since <= METER_EVERY && self.frame.is_empty() {
            return None;
        }
        self.frame.push(sample * self.window[self.frame.len()]);
        if self.frame.len() < METER_POINTS {
            return None;
        }
        self.since = METER_POINTS;
        self.fft.process(&mut self.frame);
        for (p, x) in self.power.iter_mut().zip(&self.frame) {
            *p += x.norm_sqr();
        }
        self.frame.clear();
        self.frames += 1;
        if self.frames < METER_FRAMES {
            return None;
        }
        // A bin's power as a mean square of the channel's samples: over the points, the frames
        // and the window's own power (3/8 of a rectangle's).
        let scale = 1.0 / (METER_POINTS as f32 * METER_POINTS as f32 * 0.375 * METER_FRAMES as f32);
        let inside: f32 = self.inside.iter().map(|&k| self.power[k]).sum::<f32>() * scale;
        let mut beside: Vec<f32> = self.beside.iter().map(|&k| self.power[k]).collect();
        let (_, median, _) = beside.select_nth_unstable_by(self.beside.len() / 2, f32::total_cmp);
        let floor = *median / MEDIAN_OF_MEAN * scale * self.inside.len() as f32;
        self.power.fill(0.0);
        self.frames = 0;
        Some((inside, floor))
    }
}

/// A receiver of one kind: channel samples in, audio out.
pub struct Receiver {
    demod: Demod,
    /// The carrier must be this far over the floor for audio to pass; none: it always passes.
    squelch_db: Option<f32>,
    meter: Meter,
    level: Level,
    /// Readings of the meter since the carrier was last over the squelch.
    under: u32,
    /// The channel sample before, for the discriminator at the channel's rate.
    before: Complex32,
    deemphasis: f32,
    /// Wide FM: the discriminator's output through the audio filter.
    wide: Fir,
    /// Narrow FM and AM: the channel through the filter to their width, a part each.
    re: Fir,
    im: Fir,
    /// The filtered sample before, for the discriminator at the audio's rate.
    narrow_before: Complex32,
    carrier: f32,
    high_pass: (f32, f32),
    voice: Fir,
}

impl Receiver {
    pub fn new(demod: Demod, squelch_db: Option<f32>) -> Receiver {
        let channel = f64::from(CHANNEL_HZ);
        let narrow = low_pass(IF_TAPS, demod.half_hz().min(20_000.0) / channel);
        Receiver {
            demod,
            squelch_db: squelch_db.filter(|_| demod.has_squelch()),
            meter: Meter::new(demod.half_hz()),
            level: Level { level_dbfs: -100.0, over_db: None, open: !demod.has_squelch() || squelch_db.is_none() },
            under: SQUELCH_HANG,
            before: Complex32::new(1.0, 0.0),
            deemphasis: 0.0,
            wide: Fir::new(low_pass(WFM_TAPS, WFM_AUDIO_HZ / channel), DECIMATION),
            re: Fir::new(narrow.clone(), DECIMATION),
            im: Fir::new(narrow, DECIMATION),
            narrow_before: Complex32::new(1.0, 0.0),
            carrier: 0.0,
            high_pass: (0.0, 0.0),
            voice: Fir::new(low_pass(VOICE_TAPS, VOICE_HZ / f64::from(AUDIO_HZ)), 1),
        }
    }

    /// Change the squelch without disturbing the audio.
    pub fn set_squelch(&mut self, squelch_db: Option<f32>) {
        self.squelch_db = squelch_db.filter(|_| self.demod.has_squelch());
        if self.squelch_db.is_none() {
            self.level.open = true;
        }
    }

    /// What the meter last read.
    pub fn level(&self) -> Level {
        self.level
    }

    /// Take channel samples; the audio they give is added to `audio` (none while the squelch
    /// is shut).
    pub fn push(&mut self, channel: &[Complex32], audio: &mut Vec<i16>) {
        let wide_step = 2.0 * std::f32::consts::PI * WFM_DEVIATION_HZ / CHANNEL_HZ as f32;
        let wide_deemphasis = step(WFM_DEEMPHASIS_S, CHANNEL_HZ as f32);
        for &sample in channel {
            if let Some((inside, floor)) = self.meter.push(sample) {
                self.read(inside, floor);
            }
            match self.demod {
                Demod::Wfm => {
                    let turn = sample * self.before.conj();
                    self.before = sample;
                    self.deemphasis += wide_deemphasis * (turn.im.atan2(turn.re) / wide_step - self.deemphasis);
                    self.wide.push(self.deemphasis);
                }
                Demod::Nfm | Demod::Am => {
                    self.re.push(sample.re);
                    self.im.push(sample.im);
                }
            }
        }
        let open = self.level.open;
        let mut put = |v: f32| {
            if open {
                audio.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
        };
        match self.demod {
            Demod::Wfm => self.wide.drain(|v| put(v * WFM_GAIN)),
            Demod::Nfm | Demod::Am => {
                let mut parts = Vec::new();
                self.re.drain(|v| parts.push(Complex32::new(v, 0.0)));
                let mut k = 0;
                self.im.drain(|v| {
                    parts[k].im = v;
                    k += 1;
                });
                for narrow in parts {
                    let heard = self.heard(narrow);
                    // Over the voice band's lower edge, then under its upper.
                    let pole = (-2.0 * std::f32::consts::PI * VOICE_HIGH_PASS_HZ / AUDIO_HZ as f32).exp();
                    let passed = pole * (self.high_pass.1 + heard - self.high_pass.0);
                    self.high_pass = (heard, passed);
                    self.voice.push(passed);
                }
                self.voice.drain(&mut put);
            }
        }
    }

    /// One filtered sample at the audio's rate, demodulated.
    fn heard(&mut self, narrow: Complex32) -> f32 {
        let rate = AUDIO_HZ as f32;
        match self.demod {
            Demod::Am => {
                let level = narrow.norm();
                if self.carrier == 0.0 {
                    self.carrier = level;
                }
                self.carrier += step(AM_CARRIER_S, rate) * (level - self.carrier);
                AM_GAIN * (level - self.carrier) / self.carrier.max(1e-3)
            }
            _ => {
                let turn = narrow * self.narrow_before.conj();
                self.narrow_before = narrow;
                let deviation = turn.im.atan2(turn.re) / (2.0 * std::f32::consts::PI * NFM_DEVIATION_HZ / rate);
                self.deemphasis += step(NFM_DEEMPHASIS_S, rate) * (deviation - self.deemphasis);
                NFM_GAIN * self.deemphasis
            }
        }
    }

    /// A reading of the meter: the level, and with a squelch whether it is open.
    fn read(&mut self, inside: f32, floor: f32) {
        let db = |power: f32| 10.0 * power.max(1e-12).log10();
        self.level.level_dbfs = db(inside / (FULL_SCALE * FULL_SCALE));
        if !self.demod.has_squelch() {
            self.level.over_db = None;
            self.level.open = true;
            return;
        }
        let over = db(inside / floor.max(1e-12));
        self.level.over_db = Some(over);
        let Some(squelch) = self.squelch_db else {
            self.level.open = true;
            return;
        };
        if over >= squelch {
            self.under = 0;
            self.level.open = true;
        } else if over < squelch - SQUELCH_SLACK_DB {
            self.under = self.under.saturating_add(1);
            if self.under >= SQUELCH_HANG {
                self.level.open = false;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::ism::pulses::tests::Noise;
    use super::*;

    /// What a made signal carries.
    pub enum Sent {
        /// A tone (Hz) keyed in frequency by this deviation (Hz).
        Fm(f32, f32),
        /// A tone (Hz) keyed in amplitude to this depth (0 to 1).
        Am(f32, f32),
        /// Nothing: a bare carrier.
        Carrier,
    }

    /// The channel's samples of a carrier of `amplitude`, `offset_hz` from the channel's centre,
    /// for `seconds`, over noise of `noise` counts a part.
    pub fn signal(sent: Sent, amplitude: f32, offset_hz: f32, seconds: f32, noise: f32) -> Vec<Complex32> {
        let rate = CHANNEL_HZ as f64;
        let mut hiss = Noise(7);
        let mut phase = 0.0f64;
        (0..(seconds * CHANNEL_HZ as f32) as usize)
            .map(|n| {
                let t = n as f64 / rate;
                let (level, hz) = match sent {
                    Sent::Fm(tone, deviation) => (1.0, f64::from(offset_hz) + f64::from(deviation) * (2.0 * std::f64::consts::PI * f64::from(tone) * t).sin()),
                    Sent::Am(tone, depth) => (1.0 + f64::from(depth) * (2.0 * std::f64::consts::PI * f64::from(tone) * t).sin(), f64::from(offset_hz)),
                    Sent::Carrier => (1.0, f64::from(offset_hz)),
                };
                phase += 2.0 * std::f64::consts::PI * hz / rate;
                Complex32::new(phase.cos() as f32, phase.sin() as f32) * (amplitude * level as f32) + hiss.sample(noise)
            })
            .collect()
    }

    /// The audio a receiver makes of a signal.
    fn audio_of(receiver: &mut Receiver, channel: &[Complex32]) -> Vec<i16> {
        let mut audio = Vec::new();
        for block in channel.chunks(5000) {
            receiver.push(block, &mut audio);
        }
        audio
    }

    /// A tone's amplitude in audio (of full scale), and the power of everything else over the
    /// tone's, after the first 100 ms.
    fn tone(audio: &[i16], hz: f32) -> (f32, f32) {
        let audio = &audio[(AUDIO_HZ / 10) as usize..];
        let (mut i, mut q, mut total) = (0.0f64, 0.0f64, 0.0f64);
        for (n, &s) in audio.iter().enumerate() {
            let v = f64::from(s) / 32767.0;
            let a = 2.0 * std::f64::consts::PI * f64::from(hz) * n as f64 / f64::from(AUDIO_HZ);
            i += v * a.cos();
            q += v * a.sin();
            total += v * v;
        }
        let n = audio.len() as f64;
        let amplitude = 2.0 * (i * i + q * q).sqrt() / n;
        let rest = (total / n - amplitude * amplitude / 2.0).max(0.0);
        (amplitude as f32, (rest / (amplitude * amplitude / 2.0)) as f32)
    }

    #[test]
    fn wide_fm_gives_its_tone_and_shuts_out_the_pilot() {
        // 1 kHz at 50 kHz of deviation: two thirds of full deviation, less the de-emphasis at
        // 1 kHz (0.9 dB) and the gain.
        let mut receiver = Receiver::new(Demod::Wfm, None);
        let audio = audio_of(&mut receiver, &signal(Sent::Fm(1000.0, 50_000.0), 300.0, 0.0, 0.5, 2.0));
        assert!((audio.len() as i64 - i64::from(AUDIO_HZ / 2)).abs() <= WFM_TAPS as i64, "{}", audio.len());
        let (amplitude, rest) = tone(&audio, 1000.0);
        assert!((amplitude - 0.7 * 50.0 / 75.0 * 0.904).abs() < 0.02, "{amplitude}");
        assert!(rest < 0.01, "{rest}");
        let level = receiver.level();
        assert!(level.open && level.over_db.is_none() && (level.level_dbfs - 20.0 * (300.0f32 / 2048.0).log10()).abs() < 1.5, "{level:?}");
        // A stereo pilot's tone, 19 kHz at 7.5 kHz of deviation, is 60 dB down or more: it does
        // not fold into the audio.
        let mut receiver = Receiver::new(Demod::Wfm, None);
        let audio = audio_of(&mut receiver, &signal(Sent::Fm(19_000.0, 7_500.0), 300.0, 0.0, 0.5, 0.0));
        let power = audio[3125..].iter().map(|&s| (f64::from(s) / 32767.0).powi(2)).sum::<f64>() / (audio.len() - 3125) as f64;
        assert!(power.sqrt() < 0.07 * 10f64.powf(-60.0 / 20.0), "{}", power.sqrt());
    }

    #[test]
    fn narrow_fm_gives_a_voice_tone_and_keeps_shut_on_noise() {
        // 1 kHz at 3 kHz of deviation, 1.5 kHz off the channel's centre, 30 dB over the noise
        // in its width: the tone at a third of full scale, the squelch open.
        let mut receiver = Receiver::new(Demod::Nfm, Some(8.0));
        let audio = audio_of(&mut receiver, &signal(Sent::Fm(1000.0, 3000.0), 100.0, 1500.0, 1.0, 13.0));
        let (amplitude, rest) = tone(&audio, 1000.0);
        assert!((amplitude - 0.335).abs() < 0.05 && rest < 0.05, "{amplitude} {rest}");
        let level = receiver.level();
        assert!(level.open && (level.over_db.unwrap() - 30.0).abs() < 3.0, "{level:?}");
        assert!((level.level_dbfs - 20.0 * (100.0f32 / 2048.0).log10()).abs() < 1.5, "{level:?}");
        // Noise alone: no audio at all, and the meter says there is nothing over the floor.
        let mut receiver = Receiver::new(Demod::Nfm, Some(8.0));
        let audio = audio_of(&mut receiver, &signal(Sent::Carrier, 0.0, 0.0, 1.0, 13.0));
        assert!(audio.is_empty() && !receiver.level().open && receiver.level().over_db.unwrap().abs() < 3.0, "{} {:?}", audio.len(), receiver.level());
        // With no squelch the noise is heard.
        let mut receiver = Receiver::new(Demod::Nfm, None);
        assert!(audio_of(&mut receiver, &signal(Sent::Carrier, 0.0, 0.0, 0.3, 13.0)).len() > 9000);
        // A carrier 25 kHz away, the next channel, is not this one's: shut.
        let mut receiver = Receiver::new(Demod::Nfm, Some(8.0));
        let audio = audio_of(&mut receiver, &signal(Sent::Fm(1000.0, 3000.0), 100.0, 25_000.0, 1.0, 13.0));
        assert!(audio.is_empty() && receiver.level().over_db.unwrap() < 5.0, "{:?}", receiver.level());
    }

    #[test]
    fn the_squelch_opens_with_a_carrier_and_shuts_a_third_of_a_second_after_it() {
        let mut receiver = Receiver::new(Demod::Nfm, Some(8.0));
        let mut channel = signal(Sent::Carrier, 0.0, 0.0, 0.5, 13.0);
        channel.extend(signal(Sent::Fm(1000.0, 3000.0), 100.0, 0.0, 1.0, 13.0));
        channel.extend(signal(Sent::Carrier, 0.0, 0.0, 1.0, 13.0));
        let audio = audio_of(&mut receiver, &channel);
        // The second of the carrier, and what the hang lets through after it.
        let seconds = audio.len() as f32 / AUDIO_HZ as f32;
        assert!((1.0..1.6).contains(&seconds), "{seconds}");
        assert!(!receiver.level().open);
        // Opened wide, it passes everything from then on.
        receiver.set_squelch(None);
        assert!(receiver.level().open);
    }

    #[test]
    fn am_gives_its_tone_whatever_the_carrier_s_level_or_offset() {
        // 1 kHz at half depth: 0.4 of full scale, for a strong carrier and a weak one 2 kHz off.
        for (amplitude, offset) in [(400.0, 0.0), (60.0, 2000.0)] {
            let mut receiver = Receiver::new(Demod::Am, Some(8.0));
            let audio = audio_of(&mut receiver, &signal(Sent::Am(1000.0, 0.5), amplitude, offset, 1.0, 3.0));
            let (heard, rest) = tone(&audio[9000..], 1000.0);
            assert!((heard - 0.4).abs() < 0.05 && rest < 0.05, "{amplitude}: {heard} {rest}");
            assert!(receiver.level().open && receiver.level().over_db.unwrap() > 20.0);
        }
        // A bare carrier is silence.
        let mut receiver = Receiver::new(Demod::Am, Some(8.0));
        let audio = audio_of(&mut receiver, &signal(Sent::Carrier, 400.0, 0.0, 1.0, 3.0));
        assert!(audio[9000..].iter().all(|s| s.unsigned_abs() < 1500), "{:?}", audio[9000..].iter().map(|s| s.unsigned_abs()).max());
    }

    /// The audio a receiver makes of a station in a raw capture of a window (its file named
    /// as the bands' tools name them), and the receiver after it. With `ANALOG_AUDIO_OUT` the
    /// audio is also written there as a WAV file, to listen to.
    fn station(file: &str, hz: u64, demod: Demod, squelch_db: Option<f32>) -> Option<(Vec<i16>, Receiver, Vec<Complex32>)> {
        use std::io::Read;

        use super::super::ism::channel::Channel;
        use super::super::ism::named_window;
        let dir = std::env::var("ANALOG_CAPTURE_DIR").ok()?;
        let path = std::path::Path::new(&dir).join(file);
        let (lo, rate) = named_window(file).expect("the capture's name gives its LO and rate");
        let mut channel = Channel::new(rate, hz as f64 - lo as f64).unwrap();
        let mut receiver = Receiver::new(demod, squelch_db);
        let mut source = std::fs::File::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let mut block = vec![0u8; 1 << 20];
        let (mut samples, mut all, mut audio) = (Vec::new(), Vec::new(), Vec::new());
        loop {
            let n = source.read(&mut block).unwrap();
            if n == 0 {
                break;
            }
            samples.clear();
            channel.push(&block[..n / 4 * 4], &mut samples);
            receiver.push(&samples, &mut audio);
            all.extend_from_slice(&samples);
        }
        if let Ok(out) = std::env::var("ANALOG_AUDIO_OUT") {
            let name = format!("{}_{:.4}M_{}.wav", file.split('_').next().unwrap(), hz as f64 / 1e6, serde_json::to_string(&demod).unwrap().trim_matches('"'));
            let mut wav = Vec::with_capacity(44 + 2 * audio.len());
            let bytes = 2 * audio.len() as u32;
            wav.extend_from_slice(b"RIFF");
            wav.extend_from_slice(&(36 + bytes).to_le_bytes());
            wav.extend_from_slice(b"WAVEfmt ");
            for field in [16u32, 0x0001_0001, AUDIO_HZ, 2 * AUDIO_HZ, 0x0010_0002] {
                wav.extend_from_slice(&field.to_le_bytes());
            }
            wav.extend_from_slice(b"data");
            wav.extend_from_slice(&bytes.to_le_bytes());
            wav.extend(audio.iter().flat_map(|s| s.to_le_bytes()));
            std::fs::write(std::path::Path::new(&out).join(name), wav).unwrap();
        }
        Some((audio, receiver, all))
    }

    fn rms(audio: &[i16]) -> f32 {
        (audio.iter().map(|&s| (f64::from(s) / 32767.0).powi(2)).sum::<f64>() / audio.len().max(1) as f64).sqrt() as f32
    }

    /// Unit A's captures of what is on the air (`ANALOG_CAPTURE_DIR`, doc/changes/112): twelve
    /// seconds of a 3 MSPS window with the weather radio's channel in it, and a second of the
    /// FM broadcast band at 16 MSPS. Printed with `--nocapture`.
    #[test]
    fn the_weather_channel_is_heard_and_the_channels_beside_it_are_shut() {
        let Some((audio, receiver, _)) = station("wx_163.05M_3000k.cs16", 162_550_000, Demod::Nfm, Some(8.0)) else { return };
        let level = receiver.level();
        println!("162.550 MHz: {} samples of audio ({:.1} s), rms {:.3}, {level:?}", audio.len(), audio.len() as f32 / AUDIO_HZ as f32, rms(&audio));
        // The whole twelve seconds, a voice's level, the carrier well over the floor.
        assert!(audio.len() as f32 / AUDIO_HZ as f32 > 11.8 && level.open && level.over_db.unwrap() > 15.0, "{level:?}");
        assert!((0.03..0.5).contains(&rms(&audio)), "{}", rms(&audio));
        // The other six weather channels hold nothing here: not a sample.
        for hz in [162_400_000, 162_425_000, 162_450_000, 162_475_000, 162_500_000, 162_525_000] {
            let (audio, receiver, _) = station("wx_163.05M_3000k.cs16", hz, Demod::Nfm, Some(8.0)).unwrap();
            println!("{:.3} MHz: {} samples of audio, {:?}", hz as f64 / 1e6, audio.len(), receiver.level());
            assert!(audio.is_empty() && !receiver.level().open, "{hz}: {:?}", receiver.level());
        }
    }

    #[test]
    fn a_broadcast_station_is_heard_with_its_stereo_pilot_where_it_belongs() {
        let Some((audio, receiver, channel)) = station("fm_98M_16000k.cs16", 102_900_000, Demod::Wfm, None) else { return };
        println!("102.9 MHz: {} samples of audio, rms {:.3}, {:?}", audio.len(), rms(&audio), receiver.level());
        assert!(audio.len() as f32 / AUDIO_HZ as f32 > 0.95 && (0.02..0.6).contains(&rms(&audio)), "{} {}", audio.len(), rms(&audio));
        // The discriminator's output before the audio filter is the station's multiplex: its
        // 19 kHz pilot stands well over what is 1 kHz either side of it.
        let turns: Vec<f32> = channel.windows(2).map(|w| (w[1] * w[0].conj()).arg()).collect();
        let at = |hz: f32| {
            let (mut i, mut q) = (0.0f64, 0.0f64);
            for (n, &v) in turns.iter().enumerate() {
                let a = 2.0 * std::f64::consts::PI * f64::from(hz) * n as f64 / f64::from(CHANNEL_HZ);
                i += f64::from(v) * a.cos();
                q += f64::from(v) * a.sin();
            }
            (i * i + q * q).sqrt() / turns.len() as f64
        };
        let (pilot, beside) = (at(19_000.0), at(18_000.0).max(at(20_000.0)));
        println!("the pilot at 19 kHz: {:.1} dB over what is 1 kHz from it", 20.0 * (pilot / beside).log10());
        assert!(pilot > 10.0 * beside, "{pilot} {beside}");
    }

    #[test]
    fn a_kind_goes_by_its_name_and_only_the_narrow_ones_have_a_squelch() {
        for (demod, name) in [(Demod::Wfm, "\"wfm\""), (Demod::Nfm, "\"nfm\""), (Demod::Am, "\"am\"")] {
            assert_eq!(serde_json::to_string(&demod).unwrap(), name);
            assert_eq!(serde_json::from_str::<Demod>(name).unwrap(), demod);
        }
        assert!(!Demod::Wfm.has_squelch() && Demod::Nfm.has_squelch() && Demod::Am.has_squelch());
        // A squelch asked of wide FM is none.
        assert!(Receiver::new(Demod::Wfm, Some(10.0)).level().open);
    }
}

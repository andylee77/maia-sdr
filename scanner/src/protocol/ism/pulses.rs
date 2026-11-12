//! A burst's pulses: its IQ brought to its own centre, filtered to 15 kHz either side and read
//! as an envelope that is on or off, 20 µs a sample. Every decoder of an on-off keyed device
//! works from this, so a burst is sliced once. A device 12 dB or more under another that sends at
//! the same time in the burst falls under the slice and is not seen.

use rustfft::num_complex::Complex32;

use super::channel::{low_pass, RATE_HZ as CHANNEL_RATE_HZ};
use crate::dsp::run::dot;

/// Channel samples for each sample of the envelope.
const DECIMATION: usize = 5;
/// The envelope's sample rate: 50 kSPS.
pub const RATE_HZ: u32 = CHANNEL_RATE_HZ / DECIMATION as u32;
/// Microseconds a sample of the envelope.
pub const SAMPLE_US: f32 = 1e6 / RATE_HZ as f32;
/// The filter: 121 taps at the channel's rate, half its gain 15 kHz from the burst's centre.
const TAPS: usize = 121;
const CUTOFF_HZ: f64 = 15_000.0;
/// The envelope is smoothed over this many samples: 60 µs.
const SMOOTH: usize = 3;
/// On is this share of the way from the noise (the envelope's median) to the peak (all but its
/// highest 0.2 %): the slice unit A's first captures were read with.
const SLICE: f32 = 0.35;
const PEAK_SHARE: f32 = 0.998;
/// The mixer's phasor is brought back to length one this often.
const RENORMALISE: usize = 256;

/// The burst in `iq` (the channel's samples, a burst with some of what is before and after it),
/// `offset_hz` from the channel's centre: whether it is on at each sample of the envelope. Empty
/// when there is too little of it to read.
pub fn sliced(iq: &[Complex32], offset_hz: f32) -> Vec<bool> {
    if iq.len() < 2 * TAPS {
        return Vec::new();
    }
    // To the burst's centre.
    let angle = -2.0 * std::f64::consts::PI * f64::from(offset_hz) / f64::from(CHANNEL_RATE_HZ);
    let turn = Complex32::new(angle.cos() as f32, angle.sin() as f32);
    let mut phasor = Complex32::new(1.0, 0.0);
    let (mut re, mut im) = (Vec::with_capacity(iq.len()), Vec::with_capacity(iq.len()));
    for (n, s) in iq.iter().enumerate() {
        let m = s * phasor;
        re.push(m.re);
        im.push(m.im);
        phasor *= turn;
        if n % RENORMALISE == RENORMALISE - 1 {
            // One Newton step to length one: the length never strays far.
            phasor *= 1.5 - 0.5 * phasor.norm_sqr();
        }
    }
    let taps = low_pass(TAPS, CUTOFF_HZ / f64::from(CHANNEL_RATE_HZ));
    let raw: Vec<f32> = (0..=(iq.len() - TAPS) / DECIMATION)
        .map(|k| {
            let at = k * DECIMATION;
            let (i, q) = (dot(&taps, &re[at..at + TAPS]), dot(&taps, &im[at..at + TAPS]));
            (i * i + q * q).sqrt()
        })
        .collect();
    let envelope: Vec<f32> = raw.windows(SMOOTH).map(|w| w.iter().sum::<f32>() / SMOOTH as f32).collect();
    if envelope.is_empty() {
        return Vec::new();
    }
    let mut sorted = envelope.clone();
    let (_, noise, _) = sorted.select_nth_unstable_by(envelope.len() / 2, f32::total_cmp);
    let noise = *noise;
    let (_, peak, _) = sorted.select_nth_unstable_by(((envelope.len() - 1) as f32 * PEAK_SHARE) as usize, f32::total_cmp);
    let slice = noise + SLICE * (*peak - noise);
    envelope.iter().map(|&v| v > slice).collect()
}

/// The runs of an envelope: each whether on, where it starts and how long it is, in samples.
pub fn runs(on: &[bool]) -> Vec<(bool, usize, usize)> {
    let mut out: Vec<(bool, usize, usize)> = Vec::new();
    for (n, &state) in on.iter().enumerate() {
        match out.last_mut() {
            Some(last) if last.0 == state => last.2 += 1,
            _ => out.push((state, n, 1)),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A deterministic noise source of unit variance.
    pub struct Noise(pub u64);

    impl Noise {
        fn uniform(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32 + 0.5) / (1u64 << 24) as f32
        }

        pub fn sample(&mut self, amplitude: f32) -> Complex32 {
            let (r, a) = ((-self.uniform().ln()).sqrt(), 2.0 * std::f32::consts::PI * self.uniform());
            Complex32::new(r * a.cos(), r * a.sin()) * amplitude
        }
    }

    /// The channel's samples of a carrier `offset_hz` from its centre, keyed on and off by
    /// `keyed` (µs on, µs off, ...) after `lead_us` of silence, over noise of 10 counts.
    pub fn keyed_burst(offset_hz: f32, amplitude: f32, lead_us: u32, keyed: &[u32]) -> Vec<Complex32> {
        let total: u32 = lead_us + keyed.iter().sum::<u32>() + lead_us;
        let mut edges = vec![lead_us];
        for k in keyed {
            edges.push(edges.last().unwrap() + k);
        }
        let mut noise = Noise(11);
        (0..(u64::from(total) * u64::from(CHANNEL_RATE_HZ) / 1_000_000) as usize)
            .map(|n| {
                let us = (n as f64 * 1e6 / f64::from(CHANNEL_RATE_HZ)) as u32;
                // On in the even intervals after the lead.
                let on = edges.windows(2).enumerate().any(|(k, w)| k % 2 == 0 && us >= w[0] && us < w[1]);
                let mut s = noise.sample(10.0);
                if on {
                    let a = 2.0 * std::f64::consts::PI * f64::from(offset_hz) * n as f64 / f64::from(CHANNEL_RATE_HZ);
                    s += Complex32::new(a.cos() as f32, a.sin() as f32) * amplitude;
                }
                s
            })
            .collect()
    }

    #[test]
    fn a_keyed_burst_s_pulses_and_gaps_come_out_at_their_lengths() {
        // 1.5 ms on, 1.4 off, 3.0 on, 2.8 off, 5.6 on: 40 kHz under the channel's centre.
        let iq = keyed_burst(-40_000.0, 100.0, 20_000, &[1500, 1400, 3000, 2800, 5600]);
        let on = sliced(&iq, -40_000.0);
        let lengths: Vec<(bool, f32)> = runs(&on).iter().map(|r| (r.0, r.2 as f32 * SAMPLE_US)).collect();
        let pulses: Vec<f32> = lengths.iter().filter(|r| r.0).map(|r| r.1).collect();
        let gaps: Vec<f32> = lengths[1..lengths.len() - 1].iter().filter(|r| !r.0).map(|r| r.1).collect();
        assert_eq!((pulses.len(), gaps.len()), (3, 2), "{lengths:?}");
        for (got, want) in pulses.iter().zip([1500.0, 3000.0, 5600.0]).chain(gaps.iter().zip([1400.0, 2800.0])) {
            assert!((got - want).abs() <= 100.0, "{got} for {want}: {lengths:?}");
        }
    }

    #[test]
    fn a_burst_read_off_its_centre_or_another_device_s_is_not_this_one() {
        // Read 40 kHz from where it is, the burst is outside the filter: nothing of its keying.
        let iq = keyed_burst(-40_000.0, 100.0, 20_000, &[1500, 1400, 3000, 2800, 5600]);
        let on = sliced(&iq, 0.0);
        assert!(runs(&on).iter().filter(|r| r.0).all(|r| (r.2 as f32 * SAMPLE_US) < 1000.0), "{:?}", runs(&on).len());
        // Too little to read.
        assert!(sliced(&iq[..100], 0.0).is_empty());
    }

    #[test]
    fn runs_are_each_state_s_start_and_length() {
        assert_eq!(runs(&[false, true, true, false, false, false, true]), vec![(false, 0, 1), (true, 1, 2), (false, 3, 3), (true, 6, 1)]);
        assert!(runs(&[]).is_empty());
    }
}

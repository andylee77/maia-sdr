//! A burst's pulses: its IQ brought to its own centre and filtered as narrow as a keying's
//! shortest pulse allows, its envelope taken 20 µs a sample (`envelope`), then read as on or off
//! (`slice`) and as runs of each (`runs`, and `joined` to leave glitches out). Every decoder of
//! an on-off keyed device works from this. The band's clicks are as wide as the channel, so a
//! narrower filter takes more of them away than of the burst. A device 12 dB or more under
//! another that sends at the same time, within the filter, falls under the slice and is not
//! seen.

use rustfft::num_complex::Complex32;

use super::channel::{low_pass, RATE_HZ as CHANNEL_RATE_HZ};
use crate::dsp::run::dot;

/// Channel samples for each sample of the envelope.
const DECIMATION: usize = 5;
/// The envelope's sample rate: 50 kSPS.
pub const RATE_HZ: u32 = CHANNEL_RATE_HZ / DECIMATION as u32;
/// Microseconds a sample of the envelope.
pub const SAMPLE_US: f32 = 1e6 / RATE_HZ as f32;
/// The filter's taps, at the channel's rate.
const TAPS: usize = 121;
/// What every keying read so far allows, its pulses being half a millisecond or longer: a filter
/// of 4 kHz either side, and the envelope smoothed over 120 µs (in samples).
pub const HALF_HZ: f64 = 4_000.0;
pub const SMOOTH: usize = 6;
/// On is this share of the way from the noise (the envelope's median) to the peak (all but its
/// highest 0.2 %): the slice unit A's first captures were read with.
const SLICE: f32 = 0.35;
const PEAK_SHARE: f32 = 0.998;
/// The mixer's phasor is brought back to length one this often.
const RENORMALISE: usize = 256;

/// The envelope of the burst in `iq` (the channel's samples, a burst with some of what is before
/// and after it), `offset_hz` from the channel's centre, through a filter with half its gain
/// `half_hz` either side of the burst's centre. Empty when there is too little of it to read.
pub fn envelope(iq: &[Complex32], offset_hz: f32, half_hz: f64) -> Vec<f32> {
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
    let taps = low_pass(TAPS, half_hz / f64::from(CHANNEL_RATE_HZ));
    (0..=(iq.len() - TAPS) / DECIMATION)
        .map(|k| {
            let at = k * DECIMATION;
            let (i, q) = (dot(&taps, &re[at..at + TAPS]), dot(&taps, &im[at..at + TAPS]));
            (i * i + q * q).sqrt()
        })
        .collect()
}

/// Whether a burst is on at each sample of its `envelope`, smoothed over `smooth` samples (a
/// keying's shortest pulse is the most that may be). Empty when the envelope is shorter.
pub fn slice(envelope: &[f32], smooth: usize) -> Vec<bool> {
    let smoothed: Vec<f32> = envelope.windows(smooth.max(1)).map(|w| w.iter().sum::<f32>() / w.len() as f32).collect();
    if smoothed.is_empty() {
        return Vec::new();
    }
    let mut sorted = smoothed.clone();
    let (_, noise, _) = sorted.select_nth_unstable_by(smoothed.len() / 2, f32::total_cmp);
    let noise = *noise;
    let (_, peak, _) = sorted.select_nth_unstable_by(((smoothed.len() - 1) as f32 * PEAK_SHARE) as usize, f32::total_cmp);
    let level = noise + SLICE * (*peak - noise);
    smoothed.iter().map(|&v| v > level).collect()
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

/// Runs with the glitches left out: a run shorter than `least` samples is given to the run
/// before it, so that noise in a pulse or in a gap is not two edges more.
pub fn joined(runs: &[(bool, usize, usize)], least: usize) -> Vec<(bool, usize, usize)> {
    let mut out: Vec<(bool, usize, usize)> = Vec::new();
    for &(state, start, length) in runs {
        match out.last_mut() {
            Some(last) if length < least || last.0 == state => last.2 += length,
            _ => out.push((state, start, length)),
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

    /// An amplitude a keyed burst is weak at.
    pub const WEAK: f32 = 10.0;

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

    /// A burst read as the decoders have it.
    pub fn sliced(iq: &[Complex32], offset_hz: f32) -> Vec<bool> {
        slice(&envelope(iq, offset_hz, HALF_HZ), SMOOTH)
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
    fn a_weak_burst_s_short_pulses_read_through_a_narrow_filter_with_the_glitches_left_out() {
        // Pulses of 500 µs with gaps of 2 and 4 ms, weak: through 15 kHz either side noise
        // makes pulses of its own; through 4 kHz, with what is shorter than 160 µs left out,
        // the 40 pulses and their gaps are what is read.
        let keyed: Vec<u32> = (0..40).flat_map(|k| [500, if k % 3 == 0 { 4000 } else { 2000 }]).collect();
        let iq = keyed_burst(10_000.0, WEAK, 20_000, &keyed);
        let wide = runs(&slice(&envelope(&iq, 10_000.0, 15_000.0), 3));
        assert!(wide.iter().filter(|r| r.0).count() > 44, "{}", wide.len());
        let narrow = joined(&runs(&sliced(&iq, 10_000.0)), 8);
        let pulses: Vec<f32> = narrow.iter().filter(|r| r.0).map(|r| r.2 as f32 * SAMPLE_US).collect();
        let gaps: Vec<f32> = narrow[1..narrow.len() - 1].iter().filter(|r| !r.0).map(|r| r.2 as f32 * SAMPLE_US).collect();
        assert_eq!((pulses.len(), gaps.len()), (40, 39), "{narrow:?}");
        assert!(pulses.iter().all(|p| (250.0..=900.0).contains(p)), "{pulses:?}");
        assert!(gaps.iter().enumerate().all(|(k, g)| (g - if k % 3 == 0 { 4000.0 } else { 2000.0 }).abs() < 400.0), "{gaps:?}");
    }

    #[test]
    fn a_glitch_is_given_to_the_run_before_it() {
        // A gap with a glitch in it is one gap; a pulse with a dropout in it one pulse.
        let on = [vec![true; 25], vec![false; 40], vec![true; 3], vec![false; 55], vec![true; 12], vec![false; 2], vec![true; 13], vec![false; 30]].concat();
        assert_eq!(joined(&runs(&on), 8), vec![(true, 0, 25), (false, 25, 98), (true, 123, 27), (false, 150, 30)]);
        assert_eq!(joined(&runs(&on), 1), runs(&on));
        assert!(joined(&[], 8).is_empty());
    }

    #[test]
    fn runs_are_each_state_s_start_and_length() {
        assert_eq!(runs(&[false, true, true, false, false, false, true]), vec![(false, 0, 1), (true, 1, 2), (false, 3, 3), (true, 6, 1)]);
        assert!(runs(&[]).is_empty());
    }
}

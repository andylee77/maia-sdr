//! Four-level FSK building blocks shared by the P25 C4FM and DMR receivers, ported from SDRTrunk:
//! the dibit phase constellation, a streaming FIR, the linear interpolator and the differential
//! demodulator. Names follow the Java so the two read side by side.
//!
//! Dibit values: 0 = +1, 1 = +3, 2 = −1, 3 = −3 (SDRTrunk `Dibit`).

use super::taps::MMSE_TAPS;

const PI: f32 = std::f32::consts::PI;

/// The phase step a dibit makes (SDRTrunk `Dibit.getIdealPhase`).
pub fn ideal_phase(dibit: u8) -> f32 {
    match dibit & 3 {
        0 => PI / 4.0,
        1 => 3.0 * PI / 4.0,
        2 => -PI / 4.0,
        _ => -3.0 * PI / 4.0,
    }
}

/// Hard decision on a soft symbol (SDRTrunk `toSymbol`).
pub fn to_symbol(sample: f32) -> u8 {
    const BOUNDARY: f32 = PI / 2.0;
    if sample > 0.0 {
        if sample > BOUNDARY { 1 } else { 0 }
    } else if sample < -BOUNDARY {
        3
    } else {
        2
    }
}

/// Linear interpolation between two samples at `mu` in [0, 1].
pub fn linear(x1: f32, x2: f32, mu: f64) -> f32 {
    if mu < 0.0 {
        x1
    } else if mu > 1.0 {
        x2
    } else {
        x1 + (x2 - x1) * mu as f32
    }
}

/// Streaming real FIR, optionally decimating by two. The last `taps − 1` inputs carry over to
/// the next block; zero taps (every other one of a half-band) are skipped.
pub struct Fir {
    /// (index into the window, tap) of the non-zero taps.
    taps: Vec<(usize, f32)>,
    len: usize,
    work: Vec<f32>,
    decimate: bool,
    /// The next input sample produces an output (decimation phase).
    emit: bool,
}

impl Fir {
    pub fn new(taps: &[f32], decimate: bool) -> Self {
        let len = taps.len();
        // y[n] = Σ t[k]·x[n−k]; over the window w = x[n−len+1 ..= n], x[n−k] = w[len−1−k].
        let nz = taps.iter().enumerate().filter(|(_, t)| **t != 0.0).map(|(k, t)| (len - 1 - k, *t)).collect();
        Fir { taps: nz, len, work: vec![0.0; len - 1], decimate, emit: true }
    }

    pub fn process(&mut self, x: &[f32], out: &mut Vec<f32>) {
        let hist = self.len - 1;
        self.work.truncate(hist);
        self.work.extend_from_slice(x);
        for n in 0..x.len() {
            let produce = !self.decimate || self.emit;
            self.emit = !self.emit;
            if !produce {
                continue;
            }
            let w = &self.work[n..n + self.len];
            out.push(self.taps.iter().map(|&(i, t)| t * w[i]).sum());
        }
        let used = self.work.len() - hist;
        self.work.drain(..used);
    }
}

/// The phase of each sample against the one a symbol earlier, the current sample MMSE
/// interpolated (SDRTrunk `DifferentialDemodulatorFloatScalar`).
pub struct DifferentialDemod {
    i_buf: Vec<f32>,
    q_buf: Vec<f32>,
    overlap: usize,
    interp_offset: usize,
    mu: f32,
}

impl DifferentialDemod {
    pub fn new(samples_per_symbol: f64) -> Self {
        let mu = (samples_per_symbol % 1.0) as f32;
        let mut interp_offset = samples_per_symbol.floor() as i64 - 4;
        let mut overlap = samples_per_symbol.floor() as usize + 4;
        while interp_offset < 0 {
            interp_offset += 1;
            overlap += 1;
        }
        DifferentialDemod {
            i_buf: vec![0.0; overlap],
            q_buf: vec![0.0; overlap],
            overlap,
            interp_offset: interp_offset as usize,
            mu,
        }
    }

    fn mmse(samples: &[f32], offset: usize, mu: f32) -> f32 {
        let t = &MMSE_TAPS[((128.0 * mu) as usize).min(128)];
        let s = &samples[offset..offset + 8];
        t[7] * s[0] + t[6] * s[1] + t[5] * s[2] + t[4] * s[3] + t[3] * s[4] + t[2] * s[5] + t[1] * s[6] + t[0] * s[7]
    }

    pub fn demodulate(&mut self, i: &[f32], q: &[f32], out: &mut Vec<f32>) {
        let consumed = self.i_buf.len() - self.overlap;
        self.i_buf.drain(..consumed);
        self.q_buf.drain(..consumed);
        self.i_buf.extend_from_slice(i);
        self.q_buf.extend_from_slice(q);
        for x in 0..i.len() {
            let i_prev = self.i_buf[x];
            let q_prev_conj = -self.q_buf[x];
            let off = self.interp_offset + x;
            let i_cur = Self::mmse(&self.i_buf, off, self.mu);
            let q_cur = Self::mmse(&self.q_buf, off, self.mu);
            let di = i_prev * i_cur - q_prev_conj * q_cur;
            let dq = i_prev * q_cur + i_cur * q_prev_conj;
            out.push(dq.atan2(di));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_round_trip_through_their_ideal_phases() {
        for d in 0..4u8 {
            assert_eq!(to_symbol(ideal_phase(d)), d);
        }
        assert_eq!(linear(1.0, 3.0, 0.25), 1.5);
        assert_eq!((linear(1.0, 3.0, -1.0), linear(1.0, 3.0, 2.0)), (1.0, 3.0));
    }

    #[test]
    fn fir_streams_and_decimates() {
        // An impulse through [1, 2, 3] gives the taps back, across block boundaries.
        let mut fir = Fir::new(&[1.0, 2.0, 3.0], false);
        let mut out = Vec::new();
        fir.process(&[1.0, 0.0], &mut out);
        fir.process(&[0.0, 0.0], &mut out);
        assert_eq!(out, vec![1.0, 2.0, 3.0, 0.0]);
        let mut half = Fir::new(&[1.0, 0.0, 1.0], true);
        let mut out = Vec::new();
        half.process(&[1.0, 2.0, 3.0, 4.0, 5.0], &mut out);
        assert_eq!(out, vec![1.0, 4.0, 8.0], "x[n] + x[n−2] at every other n");
    }

    #[test]
    fn a_tone_demodulates_to_its_phase_over_the_lag() {
        // At 25 kSPS and 4800 symbols/s the current sample is compared with the one
        // floor(sps) − 1 + frac(sps) = 4.21 samples earlier (about 0.8 symbol, as in SDRTrunk).
        let sps: f64 = 25_000.0 / 4800.0;
        let lag = sps.floor() - 1.0 + sps % 1.0;
        let step = (PI / 4.0) / sps as f32;
        let (i, q): (Vec<f32>, Vec<f32>) = (0..400).map(|n| ((n as f32 * step).cos(), (n as f32 * step).sin())).unzip();
        let mut demod = DifferentialDemod::new(sps);
        let mut out = Vec::new();
        demod.demodulate(&i[..123], &q[..123], &mut out);
        demod.demodulate(&i[123..], &q[123..], &mut out);
        let want = step * lag as f32;
        assert!(out[50..].iter().all(|p| (p - want).abs() < 0.002), "{:?} vs {want}", &out[50..54]);
    }
}

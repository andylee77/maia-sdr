//! The equalizer applied: out[k] = Σ w[i] y[k + 32 − i] over 128 taps, each output 32 symbols
//! after its input. The PS solves the taps (`ps`) and loads them whole; they take effect together
//! at a field's first symbol. Until the first load the equalizer passes its input.

use super::{requantize, Widths};
use crate::protocol::atsc::demod::{EQ_PRE, EQ_TAPS};

pub const TAPS: usize = EQ_TAPS;
/// Taps ahead of the symbol: the output's delay.
pub const PRE: usize = EQ_PRE;

pub struct Equalizer {
    /// The taps, oldest input's first: w[TAPS − 1 − j] weighs the window's j-th input.
    reversed: [i64; TAPS],
    /// The last TAPS inputs, twice over, so that the window is always one run.
    hist: [i64; 2 * TAPS],
    at: usize,
    shift: i32,
    width: u32,
    /// The accumulator's largest magnitude, for its width.
    pub max_acc: i64,
}

/// A tap of 1, in the taps' integers.
pub fn one(w: &Widths) -> i64 {
    1 << (w.eq_taps - 4)
}

/// Taps solved on the PS (in levels) as the PL holds them; a tap saturates at ±8.
pub fn quantize(taps: &[f32], w: &Widths) -> Vec<i32> {
    let one = one(w) as f64;
    let max = (1i64 << (w.eq_taps - 1)) - 1;
    taps.iter().map(|&t| ((f64::from(t) * one).round() as i64).clamp(-max - 1, max) as i32).collect()
}

impl Equalizer {
    pub fn new(w: &Widths) -> Equalizer {
        let shift = w.eq_taps as i32 + w.y as i32 - w.eq_out as i32 - 3;
        let mut e = Equalizer { reversed: [0; TAPS], hist: [0; 2 * TAPS], at: 0, shift, width: w.eq_out, max_acc: 0 };
        e.reversed[TAPS - 1 - PRE] = one(w);
        e
    }

    pub fn load(&mut self, taps: &[i32]) {
        for (i, &t) in taps.iter().enumerate().take(TAPS) {
            self.reversed[TAPS - 1 - i] = i64::from(t);
        }
    }

    /// Takes y[n]; gives the output for symbol n − PRE.
    pub fn step(&mut self, y: i32) -> i32 {
        self.hist[self.at] = i64::from(y);
        self.hist[self.at + TAPS] = i64::from(y);
        // Oldest (y[n − 127]) to newest (y[n]).
        let window = &self.hist[self.at + 1..self.at + 1 + TAPS];
        let acc: i64 = window.iter().zip(&self.reversed).map(|(a, b)| a * b).sum();
        self.at = (self.at + 1) % TAPS;
        self.max_acc = self.max_acc.max(acc.abs());
        requantize(acc, self.shift, self.width)
    }
}

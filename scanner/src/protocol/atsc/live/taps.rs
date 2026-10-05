//! The equalizer's taps, solved on the PS from snapshots of its input (083: every 0.25 s, each
//! from its own snapshot's field sync, 10 data segments, one round on the taps' decisions), and
//! quantized as the block holds them.
//!
//! A solve is 081's least squares: trained on the field sync's known symbols, then rounds on the
//! taps' own decisions over runs of the snapshot's data.

use std::ops::Range;

use super::packets::Gathered;
use super::{EQ_TAP_BITS, Y_BITS};
use crate::protocol::atsc::demod::{least_squares, output, rows_inside, slice};
use crate::protocol::atsc::vsb::{field_sync, SEGMENT};

/// Decision runs a round within a snapshot, and their length.
const RUNS: usize = 16;
const RUN: usize = 1024;
/// Rounds on the taps' own decisions (083's answer).
pub const ROUNDS: usize = 1;

/// The equalizer's input around a field sync, in levels.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub y: Vec<f32>,
    /// Where the field sync segment starts in `y`.
    pub sync: usize,
    pub inverted: bool,
}

impl Snapshot {
    /// A snapshot as the block sends it: the segment before the field sync first.
    pub fn from_block(g: &Gathered) -> Snapshot {
        let scale = 1.0 / f64::from(1u32 << (Y_BITS - 5));
        Snapshot {
            y: g.values.iter().map(|&v| (f64::from(v) * scale) as f32).collect(),
            sync: SEGMENT,
            inverted: g.flags & super::packets::FLAG_INVERTED != 0,
        }
    }
}

/// The equalizer's taps solved on the field whose sync segment starts at symbol `s` of `y`: its
/// field sync's known symbols (or `start`), then `rounds` rounds on its own decisions over runs
/// spread across `rows` (rows of y, inside it).
pub fn solve_on(y: &[f32], s: usize, inverted: bool, rows: Range<usize>, rounds: usize, start: Option<Vec<f32>>) -> Option<Vec<f32>> {
    let known = (s, field_sync(inverted));
    let mut w = match start {
        Some(w) => w,
        None => least_squares(y, std::slice::from_ref(&known))?,
    };
    let inside = rows_inside(y.len());
    let (lo, hi) = (rows.start.max(inside.start), rows.end.min(inside.end));
    let n = (hi.saturating_sub(lo) / RUN).min(RUNS);
    if n == 0 {
        return Some(w);
    }
    let spacing = (hi - lo - RUN) / n.max(1);
    for round in 0..rounds {
        let mut runs = vec![known.clone()];
        runs.extend((0..n).map(|r| lo + r * spacing + round * spacing / rounds.max(1)).filter(|&k0| k0 + RUN <= hi).map(|k0| (k0, (k0..k0 + RUN).map(|k| slice(output(y, &w, k))).collect())));
        w = least_squares(y, &runs)?;
    }
    Some(w)
}

/// The taps from a snapshot; `start`: the last taps, else the field sync's known symbols first.
pub fn solve(s: &Snapshot, start: Option<Vec<f32>>, rounds: usize) -> Option<Vec<f32>> {
    solve_on(&s.y, s.sync, s.inverted, s.sync + SEGMENT..s.y.len(), rounds, start)
}

/// A tap of 1, at `bits` a tap.
pub fn one(bits: u32) -> i64 {
    1 << (bits - 4)
}

/// Taps solved in levels as the block holds them (`bits` a tap; a tap saturates at ±8).
pub fn quantize(taps: &[f32], bits: u32) -> Vec<i32> {
    let one = one(bits) as f64;
    let max = (1i64 << (bits - 1)) - 1;
    taps.iter().map(|&t| ((f64::from(t) * one).round() as i64).clamp(-max - 1, max) as i32).collect()
}

/// The block's taps from a snapshot: one solve from its field sync, quantized.
pub fn for_block(g: &Gathered) -> Option<Vec<i32>> {
    solve(&Snapshot::from_block(g), None, ROUNDS).map(|t| quantize(&t, EQ_TAP_BITS))
}

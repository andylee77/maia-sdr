//! Studies on unit A's captures for 083: the channel's response (its echoes and how far they
//! reach) and how much it moves from field to field. The equalizer is solved on each field's own
//! symbols here (its field sync, then two rounds on its decisions), which no causal schedule can
//! beat; its decisions give the response, as the symbols before the equalizer correlated with
//! them (the data is white).

use std::ops::Range;

use realfft::RealFftPlanner;
use rustfft::num_complex::Complex32;

use crate::protocol::atsc::demod::{least_squares, output, rows_inside, slice, unequalized, EQ_PRE, EQ_TAPS, SYMBOL_RATE};
use crate::protocol::atsc::vsb::{field_sync, FIELD_SEGMENTS, SEGMENT, SEGMENT_SYNC, SYNC_SYMBOLS};

/// Symbols a field.
pub(crate) const FIELD: usize = FIELD_SEGMENTS * SEGMENT;
/// The response's delays, in symbols: pre-echoes to −PRE_LAGS, echoes to POST_LAGS (6 µs and 47.6 µs).
const PRE_LAGS: usize = 64;
const POST_LAGS: usize = 512;
/// Delays within this of the main path count as the main path (the pulse's own spread).
const MAIN: usize = 5;
/// Decision runs a round within a field, and their length.
const RUNS: usize = 16;
const RUN: usize = 1024;

/// Interleaved little-endian i16 I, Q, at most `max` samples.
pub(crate) fn read_cs16(path: &std::path::Path, max: usize) -> Vec<Complex32> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut b = Vec::new();
    f.by_ref().take(4 * max as u64).read_to_end(&mut b).unwrap();
    b.chunks_exact(4)
        .map(|c| Complex32::new(f32::from(i16::from_le_bytes([c[0], c[1]])), f32::from(i16::from_le_bytes([c[2], c[3]]))))
        .collect()
}

/// Interleaved little-endian i16 I, Q as the PL takes them, at most `max` samples.
pub(crate) fn read_adc(path: &std::path::Path, max: usize) -> Vec<[i16; 2]> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).unwrap();
    let mut b = Vec::new();
    f.by_ref().take(4 * max as u64).read_to_end(&mut b).unwrap();
    b.chunks_exact(4).map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])]).collect()
}

/// The equalizer's taps solved on the field whose sync segment starts at symbol `s` of `y`: its
/// field sync's known symbols, then `rounds` rounds on its own decisions over runs spread across
/// `rows` (rows of y, inside it).
pub(crate) fn solve_on(y: &[f32], s: usize, inverted: bool, rows: Range<usize>, rounds: usize, start: Option<Vec<f32>>) -> Option<Vec<f32>> {
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

/// MER (signal and error energy) of equalized data symbols.
pub(crate) fn mer_db(sig: f64, err: f64) -> f64 {
    if err > 0.0 { 10.0 * (sig / err).log10() } else { 99.0 }
}

/// One field's equalized symbols with taps `w`, their decisions (the syncs as sent), and the
/// data symbols' signal and error energies.
fn equalize_field(y: &[f32], w: &[f32], s: usize, inverted: bool) -> (Vec<f32>, f64, f64) {
    let inside = rows_inside(y.len());
    let known = field_sync(inverted);
    let (mut sig, mut err) = (0f64, 0f64);
    let mut a = vec![0f32; FIELD];
    for (i, d) in a.iter_mut().enumerate() {
        let k = s + i;
        let (seg, pos) = (i / SEGMENT, i % SEGMENT);
        let e = if inside.contains(&k) { output(y, w, k) } else { 0.0 };
        *d = if pos < SYNC_SYMBOLS {
            SEGMENT_SYNC[pos]
        } else if seg == 0 && pos < known.len() {
            known[pos]
        } else {
            slice(e)
        };
        if seg > 0 && pos >= SYNC_SYMBOLS && inside.contains(&k) {
            sig += f64::from(*d * *d);
            err += f64::from((e - *d) * (e - *d));
        }
    }
    (a, sig, err)
}

/// h[d] = Σ y[s + i] a[i − d] / Σ a² for d = −PRE_LAGS..=POST_LAGS (index d + PRE_LAGS), by FFT.
fn response(y: &[f32], s: usize, a: &[f32]) -> Vec<f32> {
    let n = (a.len() + PRE_LAGS + POST_LAGS).next_power_of_two();
    let mut planner = RealFftPlanner::<f32>::new();
    let (fwd, inv) = (planner.plan_fft_forward(n), planner.plan_fft_inverse(n));
    // y from PRE_LAGS before the field, so that every delay reads inside the block.
    let mut yb = vec![0f32; n];
    let from = s.saturating_sub(PRE_LAGS);
    let take = (a.len() + PRE_LAGS + POST_LAGS).min(y.len() - from);
    yb[..take].copy_from_slice(&y[from..from + take]);
    let mut ab = vec![0f32; n];
    ab[..a.len()].copy_from_slice(a);
    let (mut ys, mut as_) = (fwd.make_output_vec(), fwd.make_output_vec());
    let _ = fwd.process(&mut yb, &mut ys);
    let _ = fwd.process(&mut ab, &mut as_);
    for (p, q) in ys.iter_mut().zip(&as_) {
        *p *= q.conj();
    }
    let mut r = vec![0f32; n];
    let _ = inv.process(&mut ys, &mut r);
    let norm = a.iter().map(|v| v * v).sum::<f32>() * n as f32;
    // r[m] = Σ_i yb[i + m] a[i]: delay d is m = d + (s − from).
    let off = s - from;
    (0..=PRE_LAGS + POST_LAGS).map(|j| r[j + off - PRE_LAGS] / norm).collect()
}

/// Per field of a capture: the MER with taps solved on that field, and the channel's response.
pub(crate) struct Fields {
    pub mer: Vec<f64>,
    pub responses: Vec<Vec<f32>>,
    pub mer_whole: f32,
}

pub(crate) fn per_field(iq: &[Complex32], fs: f64, max_fields: usize) -> Option<Fields> {
    let u = unequalized(iq, fs, 0.0)?;
    let y = &u.symbols;
    let mer_whole = crate::protocol::atsc::demod::equalize(y, u.first_sync, &u.inverted).map(|e| crate::protocol::atsc::demod::mer(&e)).unwrap_or(0.0);
    let (mut mer, mut responses) = (Vec::new(), Vec::new());
    for (f, &inv) in u.inverted.iter().enumerate().take(max_fields) {
        let s = (u.first_sync + f * FIELD_SEGMENTS) * SEGMENT;
        if s + FIELD + POST_LAGS > y.len() {
            break;
        }
        let w = solve_on(y, s, inv, s + SEGMENT..s + FIELD, 2, None)?;
        let (a, sig, err) = equalize_field(y, &w, s, inv);
        mer.push(mer_db(sig, err));
        responses.push(response(y, s, &a));
    }
    Some(Fields { mer, responses, mer_whole })
}

fn db(v: f64) -> f64 {
    10.0 * v.max(1e-12).log10()
}

/// A capture's echoes: energy in 1 µs bins of delay against the main path's, from the mean
/// response; and how the response moves between consecutive fields.
pub(crate) fn summary(name: &str, f: &Fields) -> String {
    let lags = PRE_LAGS + POST_LAGS + 1;
    let nf = f.responses.len().max(1);
    let mean: Vec<f64> = (0..lags).map(|j| f.responses.iter().map(|h| f64::from(h[j])).sum::<f64>() / nf as f64).collect();
    let energy = |h: &[f64], r: Range<isize>| r.map(|d| h[(d + PRE_LAGS as isize) as usize].powi(2)).sum::<f64>();
    let main = energy(&mean, -(MAIN as isize)..MAIN as isize + 1);
    let outside = energy(&mean, -(PRE_LAGS as isize)..-(MAIN as isize)) + energy(&mean, MAIN as isize + 1..POST_LAGS as isize + 1);
    let beyond = energy(&mean, -(PRE_LAGS as isize)..-(EQ_PRE as isize)) + energy(&mean, (EQ_TAPS - EQ_PRE) as isize..POST_LAGS as isize + 1);
    // 1 µs bins outside the main path, strongest first.
    let per_us = SYMBOL_RATE / 1e6;
    let mut bins: Vec<(f64, f64)> = Vec::new();
    let mut d0 = -(PRE_LAGS as f64);
    while d0 < POST_LAGS as f64 {
        let (a, b) = (d0.ceil() as isize, (d0 + per_us).ceil() as isize);
        let e: f64 = (a..b.min(POST_LAGS as isize + 1)).filter(|d| d.unsigned_abs() > MAIN).map(|d| mean[(d + PRE_LAGS as isize) as usize].powi(2)).sum();
        bins.push(((d0 + per_us / 2.0) / per_us, e));
        d0 += per_us;
    }
    bins.sort_by(|x, y| y.1.total_cmp(&x.1));
    let echoes: Vec<String> = bins.iter().take(4).map(|(us, e)| format!("{us:+.1} µs {:.1}", db(e / main))).collect();
    // Field to field: the change in the response against its energy.
    let mut moves: Vec<f64> = f
        .responses
        .windows(2)
        .map(|p| {
            let (dd, ee): (f64, f64) = p[0].iter().zip(&p[1]).fold((0.0, 0.0), |(dd, ee), (a, b)| (dd + f64::from(a - b).powi(2), ee + f64::from(*b).powi(2)));
            db(dd / ee)
        })
        .collect();
    moves.sort_by(f64::total_cmp);
    let med = moves.get(moves.len() / 2).copied().unwrap_or(f64::NAN);
    let mains: Vec<f64> = f.responses.iter().map(|h| db((0..2 * MAIN + 1).map(|j| f64::from(h[PRE_LAGS - MAIN + j]).powi(2)).sum())).collect();
    let (lo, hi) = mains.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let (mlo, mhi) = f.mer.iter().fold((f64::MAX, f64::MIN), |(a, b), &v| (a.min(v), b.max(v)));
    let mmean = db(f.mer.iter().map(|m| 10f64.powf(-m / 10.0)).sum::<f64>().recip() * f.mer.len() as f64);
    format!(
        "{name}: {} fields; MER {:.1} dB one solve, per-field solve {:.1} ({:.1}..{:.1}); echoes {:.1} dB of the main path, {:.1} dB beyond the equalizer's span; strongest {}; field to field {:.1} dB (median), main path {:.2} dB p-p",
        f.mer.len(),
        f.mer_whole,
        mmean,
        mlo,
        mhi,
        db(outside / main),
        db(beyond / main),
        echoes.join(", "),
        med,
        hi - lo
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Offline: `ATSC_STUDY_FILES` (paths separated by `;`), 10 MSPS with unit A's crystal; at most
    /// `ATSC_STUDY_SECONDS` (default 0.8) of each.
    #[test]
    #[ignore]
    fn echoes_and_their_movement() {
        let Ok(files) = std::env::var("ATSC_STUDY_FILES") else {
            eprintln!("ATSC_STUDY_FILES not set: skipped");
            return;
        };
        let seconds: f64 = std::env::var("ATSC_STUDY_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(0.8);
        let fs = 10e6 * (1.0 - 0.688e-6);
        for path in files.split(';').filter(|p| !p.is_empty()) {
            let path = std::path::Path::new(path);
            let iq = read_cs16(path, (seconds * 10e6) as usize);
            let name = path.file_stem().unwrap().to_string_lossy();
            match per_field(&iq, fs, usize::MAX) {
                Some(f) => eprintln!("{}", summary(&name, &f)),
                None => eprintln!("{name}: no syncs"),
            }
        }
    }
}

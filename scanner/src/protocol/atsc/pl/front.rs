//! The front end: the AD9361's samples (12 bits) to the equalizer's input, one a symbol.
//!
//! - **Timing NCO:** each symbol's instant in input samples, 32.32 fixed point. A symbol's step
//!   is the nominal one (from the true sample rate) plus the timing loop's rate.
//! - **Matched filter at any instant:** 081's table, 64 offsets between input samples × 40 taps
//!   (±10 symbols of RS/2 at 10 MSPS), the row the instant's fraction rounded to 1/64; real taps
//!   on I and Q.
//! - **Gain:** I and Q times the gain word. The gain loop holds the mean |y| at 4 levels.
//! - **Turn:** times e^{jθ} from a sine table. θ steps a quarter turn a symbol (taking the pilot,
//!   RS/4 below the centre, to DC) plus the carrier loop's frequency. The real part is the
//!   8-level signal; the imaginary part, the pilot's phase error, drives the carrier loop.
//! - **DC:** the pilot's own level, followed and taken out.
//! - **Segment syncs:** found by summing the sync's correlation at each of a segment's 832
//!   places over 64 segments, then counted. At each one the timing loop corrects the instant by
//!   the sync's correlation half a symbol late less half a symbol early.
//! - **Field syncs:** the segment whose PN511 correlation is the highest of a field's 313,
//!   if it passes a quarter of a perfect one's (multipath takes the unequalized correlation to
//!   half and below); then counted every 313 segments, a quarter of a perfect correlation where
//!   one is due counting as found, the middle PN63's sign giving each field's parity. Four
//!   missed in a row start the search again.

use std::ops::Range;

use super::{requantize, Config, Loops, Widths};
use crate::protocol::atsc::demod::{rrc, SYMBOL_RATE};
use crate::protocol::atsc::vsb::{pn511, pn63, FIELD_SEGMENTS, SEGMENT};

/// Offsets between input samples the matched filter is tabulated at.
pub const PHASE_BITS: u32 = 6;
/// The matched filter's half-span, in symbols of RS/2 (081's).
const FILTER_HALF_SPAN: f64 = 10.0;
/// The loops take their errors at 128 a level (the equalizer's input at 12 bits).
const LOOP_BITS: u32 = 12;
const LEVEL: i64 = 1 << (LOOP_BITS - 5);
/// θ's step a symbol: a quarter turn.
const QUARTER: i64 = 1 << 30;
/// The gain loop's sum, unity at 2^(24 + gain_k).
const GAIN_ONE_BITS: u32 = 24;
/// The timing loop's rate: its sum's bits below 2^-32 samples a symbol.
const RATE_FRAC: u32 = 16;
/// The gain loop's target: the mean |y|, 4 levels.
const GAIN_TARGET: i64 = 4 * LEVEL;
/// Symbols for the gain, DC and carrier loops to settle before the segment syncs are searched.
pub const SETTLE: usize = 1 << 16;
/// Segments summed to find the segment syncs.
const SEARCH_SEGMENTS: usize = 64;
/// A field sync: a quarter of a perfect PN511 correlation (511 symbols of 5 levels), six times
/// the data's own spread there.
const FIELD_SYNC_THRESHOLD: i64 = 511 * 5 * LEVEL / 4;
/// Field syncs missed in a row before the fields are searched again.
const FIELD_MISSES: u32 = 4;
/// The segment sync, as signs.
const SYNC: [i64; 4] = [1, -1, -1, 1];
/// Where the middle PN63 starts in a field sync segment.
const MIDDLE_PN63: usize = 4 + 511 + 63;

/// The matched filter at any instant.
pub struct MatchedFilter {
    pub half: usize,
    pub taps: usize,
    /// Row p (an instant p/64 of a sample after input i): `taps` weights for inputs
    /// i + 1 − half ..= i + half.
    pub table: Vec<i32>,
    /// Bits of the output below the input's.
    pub frac: i32,
    shift: i32,
    width: u32,
}

impl MatchedFilter {
    pub fn new(fs: f64, w: &Widths) -> MatchedFilter {
        let rb = SYMBOL_RATE / 2.0;
        let half = ((FILTER_HALF_SPAN / rb * fs).ceil() as usize + 1).div_ceil(4) * 4;
        let taps = 2 * half;
        let one = f64::from(1u32 << (w.mf_taps - 1));
        let rows = 1usize << PHASE_BITS;
        let table = (0..rows)
            .flat_map(|p| {
                let f = p as f64 / rows as f64;
                (0..taps).map(move |j| (rrc((f + half as f64 - 1.0 - j as f64) / fs * rb) * rb / fs * one).round() as i32)
            })
            .collect();
        let frac = w.mf_out as i32 - 13;
        let shift = w.mf_taps as i32 - 1 - frac;
        MatchedFilter { half, taps, table, frac, shift, width: w.mf_out }
    }

    /// The filtered I and Q at instant `pos` (input samples, 32.32); `None` where its inputs leave
    /// `x`.
    pub fn at(&self, x: &[[i16; 2]], pos: u64) -> Option<[i32; 2]> {
        let below = 32 - PHASE_BITS;
        let rounded = pos.checked_add(1 << (below - 1))?;
        let i = (rounded >> 32) as usize;
        let p = ((rounded & 0xFFFF_FFFF) >> below) as usize;
        let start = (i + 1).checked_sub(self.half)?;
        let w = x.get(start..start + self.taps)?;
        let row = &self.table[p * self.taps..(p + 1) * self.taps];
        let (mut re, mut im) = (0i64, 0i64);
        for (&t, s) in row.iter().zip(w) {
            re += i64::from(t) * i64::from(s[0]);
            im += i64::from(t) * i64::from(s[1]);
        }
        Some([requantize(re, self.shift, self.width), requantize(im, self.shift, self.width)])
    }
}

/// The turn: a sine table and the complex multiply.
pub struct Turn {
    addr: u32,
    /// cos, sin of 2π k / 2^addr.
    pub table: Vec<[i32; 2]>,
    shift: i32,
    width: u32,
}

impl Turn {
    pub fn new(w: &Widths) -> Turn {
        let n = 1usize << w.nco_addr;
        let one = f64::from((1u32 << (w.nco_amp - 1)) - 1);
        let table = (0..n)
            .map(|k| {
                let a = 2.0 * std::f64::consts::PI * k as f64 / n as f64;
                [(a.cos() * one).round() as i32, (a.sin() * one).round() as i32]
            })
            .collect();
        Turn { addr: w.nco_addr, table, shift: w.nco_amp as i32 - 1, width: w.y }
    }

    /// z turned by θ (2^32 a turn): its real and imaginary parts.
    pub fn apply(&self, z: [i32; 2], theta: u32) -> [i32; 2] {
        let k = (theta.wrapping_add(1 << (31 - self.addr)) >> (32 - self.addr)) as usize;
        let [c, s] = self.table[k];
        let (zr, zi, c, s) = (i64::from(z[0]), i64::from(z[1]), i64::from(c), i64::from(s));
        [requantize(zr * c - zi * s, self.shift, self.width), requantize(zr * s + zi * c, self.shift, self.width)]
    }
}

/// One symbol through the front end: each stage's output, for the HDL to match.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Symbol {
    /// The instant, input samples (32.32).
    pub pos: u64,
    pub filtered: [i32; 2],
    pub gain: u32,
    pub gained: [i32; 2],
    pub theta: u32,
    /// The real part (the levels and the pilot's DC) and the imaginary part.
    pub turned: [i32; 2],
    pub dc: i32,
    /// The equalizer's input.
    pub y: i32,
}

/// A field sync segment, found or counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// Its first symbol.
    pub start: usize,
    /// Its PN511 passed the threshold (else only counted).
    pub found: bool,
    /// Its middle PN63 inverted.
    pub inverted: bool,
}

/// The loops at a segment's end (once the segments are counted).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Segment {
    /// Its first symbol.
    pub start: usize,
    /// Its first symbol's instant, input samples (32.32).
    pub pos: u64,
    /// The carrier loop's frequency (θ a symbol, beyond the quarter turn).
    pub freq: i64,
    /// The timing loop's rate (2^-32 samples a symbol, beyond the nominal step).
    pub rate: i64,
    pub gain: u32,
    pub dc: i32,
    /// Its sync's correlation at the symbols (128 a level, four syncs of 5 levels at best).
    pub sync: i64,
    /// The timing error it gave (late less early).
    pub timing: i64,
    /// Its PN511 correlation.
    pub pn511: i64,
}

/// The front end's output.
#[derive(Debug, Clone, Default)]
pub struct Output {
    /// The equalizer's input, one a symbol.
    pub y: Vec<i32>,
    /// Where the segment counting started.
    pub first_segment: Option<usize>,
    pub segments: Vec<Segment>,
    pub fields: Vec<Field>,
    /// The stages' outputs for the symbols asked for.
    pub trace: Vec<Symbol>,
    /// The loops' registers' largest magnitudes, for their widths.
    pub ranges: Ranges,
}

/// The largest magnitude each loop register reached once the segments were counted.
#[derive(Debug, Clone, Copy, Default)]
pub struct Ranges {
    pub freq_sum: i64,
    pub rate_sum: i64,
    pub gain_sum: i64,
    pub dc_sum: i64,
    pub sync: i64,
    pub early_late: i64,
    pub pn511: i64,
    pub search: i64,
}

impl Ranges {
    /// Signed bits to hold `v`.
    pub fn bits(v: i64) -> u32 {
        65 - v.unsigned_abs().leading_zeros()
    }
}

enum Segments {
    Settle,
    Search { acc: Vec<i64>, count: usize, last: [i64; 3] },
    Counting { k: usize },
}

enum Fields {
    /// Over `seen` segments, the best PN511 correlation and how many segments ago it was.
    Search { seen: usize, best: i64, ago: usize, inverted: bool },
    Counting { seg: usize, misses: u32, inverted: bool },
}

/// `v` at 128 a level.
fn at_loop_scale(v: i32, width: u32) -> i64 {
    if width >= LOOP_BITS { i64::from(v) >> (width - LOOP_BITS) } else { i64::from(v) << (LOOP_BITS - width) }
}

/// The front end over `x` at `fs` (the true rate), the channel's centre `centre_offset_hz` from
/// DC (the carrier loop starts there). `trace`: the symbols whose stages to keep.
pub fn run(x: &[[i16; 2]], fs: f64, centre_offset_hz: f64, cfg: &Config, trace: Range<usize>) -> Output {
    let w = &cfg.widths;
    let l: &Loops = &cfg.loops;
    let mf = MatchedFilter::new(fs, w);
    let turn = Turn::new(w);
    let gain_shift = w.gain as i32 - 4 + mf.frac - (w.y as i32 - LOOP_BITS as i32);
    let gain_max = (1u64 << w.gain) - 1;
    let gain_word = |acc: i64| -> u32 {
        let g = acc >> (GAIN_ONE_BITS + l.gain_k - (w.gain - 4));
        g.clamp(1, gain_max as i64) as u32
    };
    let gained = |z: [i32; 2], g: u32| [requantize(i64::from(z[0]) * i64::from(g), gain_shift, w.y), requantize(i64::from(z[1]) * i64::from(g), gain_shift, w.y)];
    let pn: Vec<i64> = pn511().iter().map(|&v| if v > 0.0 { 1 } else { -1 }).collect();
    let pn_mid: Vec<i64> = pn63().iter().map(|&v| if v > 0.0 { 1 } else { -1 }).collect();
    let y_max = (1i64 << (w.y - 1)) - 1;

    let step0 = (fs / SYMBOL_RATE * 4_294_967_296.0).round() as u64;
    // Start where the matched filter's inputs begin.
    let mut pos: u64 = ((mf.half as u64) << 32) + (1 << 31);
    let mut rate_acc: i64 = 0;
    let mut theta: u32 = 0;
    let mut freq_acc: i64 = ((-centre_offset_hz / SYMBOL_RATE * 4_294_967_296.0).round() as i64) << l.carrier_ki;
    let mut gain_acc: i64 = 1 << (GAIN_ONE_BITS + l.gain_k);
    let mut dc_acc: i64 = 0;
    let mut segments = Segments::Settle;
    let mut fields = Fields::Search { seen: 0, best: 0, ago: 0, inverted: false };
    let (mut early, mut late, mut sync, mut pn_acc, mut mid_acc) = (0i64, 0i64, 0i64, 0i64, 0i64);
    let mut out = Output::default();
    let mut seg_pos = 0u64;
    let mut timing = 0i64;
    loop {
        let n = out.y.len();
        let Some(filtered) = mf.at(x, pos) else { break };
        let g = gain_word(gain_acc);
        let gz = gained(filtered, g);
        let turned = turn.apply(gz, theta);
        let dc = (dc_acc >> l.dc_k) as i32;
        let diff = i64::from(turned[0]) - i64::from(dc);
        let y = diff.clamp(-y_max - 1, y_max) as i32;
        out.y.push(y);
        if trace.contains(&n) {
            out.trace.push(Symbol { pos, filtered, gain: g, gained: gz, theta, turned, dc, y });
        }
        let freq = freq_acc >> l.carrier_ki;
        let (tkp, tki) = l.timing_gains(out.segments.len());
        let rate = rate_acc >> RATE_FRAC;
        let step = step0 as i64 + rate;
        let yl = at_loop_scale(y, w.y);
        let mut nudge = 0i64;

        match &mut segments {
            Segments::Settle => {
                if n + 1 >= SETTLE {
                    segments = Segments::Search { acc: vec![0; SEGMENT], count: 0, last: [0; 3] };
                }
            }
            Segments::Search { acc, count, last } => {
                // The sync +5 −5 −5 +5 ending at this symbol.
                acc[*count % SEGMENT] += last[0] - last[1] - last[2] + yl;
                *last = [last[1], last[2], yl];
                *count += 1;
                if *count == SEARCH_SEGMENTS * SEGMENT {
                    let best = (0..SEGMENT).max_by_key(|&b| acc[b]).unwrap_or(0);
                    out.ranges.search = acc.iter().fold(0, |m, v| m.max(v.abs()));
                    // Bin `best` holds the sync's last symbol (place 3 in its segment); this
                    // symbol's bin is 831, so the next one's place is:
                    let next = (2 * SEGMENT + 3 - best) % SEGMENT;
                    out.first_segment = Some(n + 1 + (SEGMENT - next) % SEGMENT);
                    segments = Segments::Counting { k: next };
                }
            }
            Segments::Counting { k } => {
                let kk = *k;
                if kk == 0 {
                    seg_pos = pos;
                }
                if kk < 4 {
                    // Half a symbol either side of this sync symbol, the turn half a step either side.
                    let half_step = (step / 2) as u64;
                    let half_turn = (QUARTER + freq) / 2;
                    let side = |p: u64, t: u32| -> Option<i64> {
                        let z = gained(mf.at(x, p)?, g);
                        let v = i64::from(turn.apply(z, t)[0]) - i64::from(dc);
                        Some(at_loop_scale(v.clamp(-y_max - 1, y_max) as i32, w.y))
                    };
                    let (Some(e), Some(lt)) = (side(pos - half_step, theta.wrapping_sub(half_turn as u32)), side(pos + half_step, theta.wrapping_add(half_turn as u32))) else { break };
                    early += SYNC[kk] * e;
                    late += SYNC[kk] * lt;
                    sync += SYNC[kk] * yl;
                    if kk == 3 {
                        timing = late - early;
                        nudge = timing << tkp;
                        rate_acc += timing << (RATE_FRAC - tki);
                    }
                } else if kk < 4 + 511 {
                    pn_acc += pn[kk - 4] * yl;
                } else if (MIDDLE_PN63..MIDDLE_PN63 + 63).contains(&kk) {
                    mid_acc += pn_mid[kk - MIDDLE_PN63] * yl;
                }
                if kk == SEGMENT - 1 {
                    let start = n + 1 - SEGMENT;
                    let found = pn_acc > FIELD_SYNC_THRESHOLD;
                    match &mut fields {
                        Fields::Search { seen, best, ago, inverted } => {
                            *ago += 1;
                            if pn_acc > *best {
                                (*best, *ago, *inverted) = (pn_acc, 0, mid_acc < 0);
                            }
                            *seen += 1;
                            if *seen == FIELD_SEGMENTS {
                                // The best was a field sync `ago` segments back: count on from it.
                                fields = if *best > FIELD_SYNC_THRESHOLD {
                                    Fields::Counting { seg: *ago, misses: 0, inverted: *inverted }
                                } else {
                                    Fields::Search { seen: 0, best: 0, ago: 0, inverted: false }
                                };
                            }
                        }
                        Fields::Counting { seg, misses, inverted } => {
                            *seg = (*seg + 1) % FIELD_SEGMENTS;
                            if *seg == 0 {
                                *misses = if found { 0 } else { *misses + 1 };
                                *inverted = if found { mid_acc < 0 } else { !*inverted };
                                if *misses >= FIELD_MISSES {
                                    fields = Fields::Search { seen: 0, best: 0, ago: 0, inverted: false };
                                } else {
                                    out.fields.push(Field { start, found, inverted: *inverted });
                                }
                            }
                        }
                    }
                    out.segments.push(Segment { start, pos: seg_pos, freq, rate, gain: g, dc, sync, timing, pn511: pn_acc });
                    (early, late, sync, pn_acc, mid_acc) = (0, 0, 0, 0, 0);
                }
                *k = (kk + 1) % SEGMENT;
            }
        }

        // The loops.
        let ui = at_loop_scale(turned[1], w.y);
        freq_acc -= ui;
        theta = theta.wrapping_add((QUARTER + freq - (ui << l.carrier_kp)) as u32);
        dc_acc += diff;
        gain_acc = (gain_acc + (GAIN_TARGET - yl.abs())).max(1);
        pos = (pos as i64 + step + nudge) as u64;
        if out.first_segment.is_some_and(|f| n >= f) {
            let r = &mut out.ranges;
            r.freq_sum = r.freq_sum.max(freq_acc.abs());
            r.rate_sum = r.rate_sum.max(rate_acc.abs());
            r.gain_sum = r.gain_sum.max(gain_acc.abs());
            r.dc_sum = r.dc_sum.max(dc_acc.abs());
            r.sync = r.sync.max(sync.abs());
            r.early_late = r.early_late.max(early.abs()).max(late.abs());
            r.pn511 = r.pn511.max(pn_acc.abs());
        }
    }
    out
}

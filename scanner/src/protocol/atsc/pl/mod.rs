//! A fixed-point model of the PL half of the 8-VSB receiver (bake D, change 083): the stages 079
//! puts in the PL, in integers of a stated width, each a struct that a later HDL test can drive
//! and match bit for bit. The stages form three groups with nothing fed back between them, so the
//! model runs them one after the other over a capture, as the PL would run them together:
//!
//! - `front`: the matched filter at any instant, the gain, the carrier's turn and its loop on the
//!   pilot, the pilot's DC, the timing loop on the segment syncs, the segment and field syncs.
//!   Its output is the equalizer's input.
//! - `equalizer`: 128 taps (32 ahead) applied; taps the PS loads take effect at a field's start.
//! - `decoder`: MER per field, the soft symbols, the 12 trellis decoders, the bytes in field
//!   order, the deinterleaver, Reed-Solomon's syndromes.
//! - `packets`: the codewords with their syndrome flag in 4 KB packets for the lane ring.
//!
//! The PS half (`ps`) solves the equalizer's taps from snapshots of its input (081's least
//! squares), and corrects the flagged codewords and reads the transport stream as 081 does.
//! `study` holds the studies on unit A's captures that size the model; `vectors` writes each
//! stage's inputs and outputs for the gateware's tests (085).

pub mod decoder;
pub mod equalizer;
pub mod front;
pub mod packets;
pub mod ps;
pub mod study;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod vectors;

use crate::protocol::atsc::vsb::{FIELD_SEGMENTS, SEGMENT};

/// Symbols a field.
pub const FIELD: usize = FIELD_SEGMENTS * SEGMENT;

/// `v` rounded to `shift` fewer bits (half up, as a DSP48's rounding constant does; a negative
/// shift adds bits) and saturated to `width` signed bits.
pub fn requantize(v: i64, shift: i32, width: u32) -> i32 {
    let r = match shift {
        0 => v,
        s if s < 0 => v << -s,
        s => (v + (1 << (s - 1))) >> s,
    };
    let max = (1i64 << (width - 1)) - 1;
    r.clamp(-max - 1, max) as i32
}

/// Each stage's width in bits (signed unless said).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Widths {
    /// The matched filter's coefficients.
    pub mf_taps: u32,
    /// Its output: the input's scale (12 bits and one of headroom) with `mf_out − 13` bits below.
    pub mf_out: u32,
    /// The gain word (unsigned; unity at 2^(gain − 4)).
    pub gain: u32,
    /// The turn's sine table: address bits, and its values' bits.
    pub nco_addr: u32,
    pub nco_amp: u32,
    /// The gained and turned signal, the equalizer's input: a level is 2^(y − 5).
    pub y: u32,
    /// The equalizer's taps (a tap of 1 is 2^(eq_taps − 4)).
    pub eq_taps: u32,
    /// The equalizer's output: a level is 2^(eq_out − 6).
    pub eq_out: u32,
    /// The trellis decoders' soft symbols: bits below a level, and the limit in levels.
    pub soft_frac: u32,
    pub soft_limit: i32,
    /// Each trellis decoder's survivors, in its own symbols.
    pub traceback: usize,
}

impl Widths {
    /// Wide enough everywhere that the arithmetic costs nothing measurable: the reference the
    /// chosen widths are measured against.
    pub const WIDE: Widths = Widths {
        mf_taps: 24,
        mf_out: 24,
        gain: 24,
        nco_addr: 16,
        nco_amp: 24,
        y: 24,
        eq_taps: 24,
        eq_out: 24,
        soft_frac: 8,
        soft_limit: 32,
        traceback: 64,
    };

    /// Chosen by measurement (083): each stage the narrowest that, alone, loses at most 0.03 dB of
    /// MER on the cleanest capture (36 dB) or no packet on the weakest decoded.
    pub const CHOSEN: Widths = Widths {
        mf_taps: 12,
        mf_out: 13,
        gain: 12,
        nco_addr: 11,
        nco_amp: 10,
        y: 12,
        eq_taps: 16,
        eq_out: 12,
        soft_frac: 5,
        soft_limit: 12,
        traceback: 40,
    };
}

/// The loops' gains, as shifts of their errors (at 128 a level).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Loops {
    /// Carrier: θ (2^32 a turn) less the pilot's phase error shifted up this far, each symbol...
    pub carrier_kp: u32,
    /// ...and its frequency the error's running sum shifted down this far.
    pub carrier_ki: u32,
    /// Timing, at each segment sync: the instant (2^-32 samples) moved by the early-late error
    /// shifted up this far, and the rate (2^-32 samples a symbol) by the error shifted down this
    /// far, once tracking...
    pub timing_kp: u32,
    pub timing_ki: u32,
    /// ...after an acquisition gear that catches a symbol clock some ppm off before its drift
    /// builds: the segments it lasts, and its gains as above.
    pub timing_acquire: (usize, u32, u32),
    /// Gain: the word is the running sum of the level's error a symbol, shifted down
    /// 24 + gain_k − (gain − 4) bits (unity at 2^(24 + gain_k) in the sum).
    pub gain_k: u32,
    /// DC: followed over 2^dc_k symbols.
    pub dc_k: u32,
}

impl Loops {
    pub const DEFAULT: Loops = Loops { carrier_kp: 9, carrier_ki: 5, timing_kp: 10, timing_ki: 11, timing_acquire: (512, 14, 3), gain_k: 0, dc_k: 16 };

    /// The timing loop's gains at segment `n` (counted from the first).
    pub fn timing_gains(&self, n: usize) -> (u32, u32) {
        if n < self.timing_acquire.0 { (self.timing_acquire.1, self.timing_acquire.2) } else { (self.timing_kp, self.timing_ki) }
    }

    /// The segment the timing loop's tracking gains start.
    pub fn tracking_from(&self) -> usize {
        self.timing_acquire.0
    }
}

/// When the PS solves the taps, and from what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Schedule {
    /// Fields between solves; `None`: one solve, from the first field.
    pub every: Option<usize>,
    /// Data segments in a snapshot after its field sync (the segment before it comes too).
    pub snapshot_segments: usize,
    /// Fields from a snapshot's field to the field its taps start: 1, the next field, is the
    /// soonest a receiver can load them; 0 (the snapshot's own field) is a bound, not a receiver.
    pub delay: usize,
    /// Rounds on the taps' own decisions in a solve.
    pub rounds: usize,
    /// A solve after the first starts from the last taps rather than the field sync alone.
    pub track: bool,
    /// The first field a snapshot may come from (counted from the first field sync), once the
    /// timing loop also tracks.
    pub start: usize,
}

impl Schedule {
    /// 083's answer: every 0.25 s (10 fields), each solve from its own snapshot's field sync (from
    /// the last taps, a marginal channel's decisions carry the old taps' errors), 10 data
    /// segments and one round on the decisions; the taps from the next field.
    pub const DEFAULT: Schedule = Schedule { every: Some(10), snapshot_segments: 10, delay: 1, rounds: 1, track: false, start: 0 };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub widths: Widths,
    pub loops: Loops,
    pub schedule: Schedule,
}

/// One field through the equalizer and the decoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct FieldResult {
    /// Its sync segment's first symbol.
    pub start: usize,
    /// The data symbols' energy, and their error's, at the equalizer's output.
    pub signal: f64,
    pub error: f64,
    /// Tap sets loaded before it (0: the equalizer still passes its input).
    pub taps: usize,
}

impl FieldResult {
    pub fn mer_db(&self) -> f64 {
        study::mer_db(self.signal, self.error)
    }
}

/// A tap solve: the snapshot's field, the field its taps start, and their largest magnitude.
#[derive(Debug, Clone, Copy)]
pub struct Solve {
    pub field: usize,
    pub applied: usize,
    pub max_tap: f32,
}

/// The model over a capture.
pub struct Run {
    pub fields: Vec<FieldResult>,
    pub codewords: Vec<decoder::Codeword>,
    pub packets: Vec<[u8; packets::PACKET]>,
    pub solves: Vec<Solve>,
    /// The trellis decoders' largest path metric, after each step's normalization.
    pub max_metric: i64,
    /// The equalizer's accumulator's largest magnitude.
    pub max_eq_acc: i64,
}

impl Run {
    /// MER over the fields equalized with solved taps: their energies summed.
    pub fn mer_db(&self) -> f64 {
        let (s, e) = self.fields.iter().filter(|f| f.taps > 0).fold((0.0, 0.0), |(s, e), f| (s + f.signal, e + f.error));
        study::mer_db(s, e)
    }
}

/// The model over `x` (the AD9361's I, Q) at `fs` (the true rate), the channel's centre
/// `centre_offset_hz` from DC. `None` when no field sync is found.
pub fn run(x: &[[i16; 2]], fs: f64, centre_offset_hz: f64, cfg: &Config) -> Option<Run> {
    let front = front::run(x, fs, centre_offset_hz, cfg, 0..0);
    back(&front, cfg)
}

/// The back half's stage outputs, for the HDL to match.
#[derive(Debug, Clone, Default)]
pub struct BackTrace {
    /// The first field's first symbol.
    pub first: usize,
    /// The equalizer's output for each symbol from `first` to the last field's end.
    pub eq_out: Vec<i32>,
    /// The tap sets as loaded: the field (counted from the first) and the symbol they start at,
    /// and the taps.
    pub loads: Vec<(usize, usize, Vec<i32>)>,
    /// Each data symbol the decoder took: its trellis decoder, its soft symbol, and the decision
    /// the decoder gave (none while its survivors fill).
    pub decisions: Vec<(u8, i16, Option<u8>)>,
}

/// The equalizer, the decoder and the packets on the front end's output, the PS solving the
/// taps from snapshots as the schedule says.
pub fn back(front: &front::Output, cfg: &Config) -> Option<Run> {
    back_with(front, cfg, None)
}

/// `back`, its stage outputs kept in `trace`.
pub fn back_traced(front: &front::Output, cfg: &Config, trace: &mut BackTrace) -> Option<Run> {
    back_with(front, cfg, Some(trace))
}

fn back_with(front: &front::Output, cfg: &Config, mut trace: Option<&mut BackTrace>) -> Option<Run> {
    let w = &cfg.widths;
    let s = &cfg.schedule;
    // The fields counted without a break from the first field sync found.
    let fields: Vec<front::Field> = {
        let f = &front.fields;
        let n = f.windows(2).take_while(|p| p[1].start == p[0].start + FIELD).count() + usize::from(!f.is_empty());
        f[..n].iter().copied().filter(|f| f.start + FIELD <= front.y.len()).collect()
    };
    if fields.is_empty() {
        return None;
    }
    // The PS's solves, from the first field the timing loop tracks through (the headers say
    // which gear it is in), and the taps each field starts with.
    let tracking = match front.segments.is_empty() {
        true => 0,
        false => front.segments.get(cfg.loops.tracking_from()).map_or(usize::MAX, |s| s.start),
    };
    let mut solves = Vec::new();
    let mut loads: Vec<Option<Vec<i32>>> = vec![None; fields.len()];
    let mut last: Option<Vec<f32>> = None;
    let mut at = fields.iter().position(|f| f.start > tracking).unwrap_or(fields.len()).max(s.start);
    while at < fields.len() {
        let applied = at + s.delay;
        if applied >= fields.len() {
            break;
        }
        let f = fields[at];
        if let Some(snap) = ps::snapshot(&front.y, f.start, f.inverted, s.snapshot_segments, w) {
            let start = if s.track { last.clone() } else { None };
            if let Some(taps) = ps::solve(&snap, start, s.rounds) {
                let max_tap = taps.iter().fold(0f32, |m, &v| m.max(v.abs()));
                loads[applied] = Some(equalizer::quantize(&taps, w));
                solves.push(Solve { field: at, applied, max_tap });
                last = Some(taps);
            }
        }
        match s.every {
            Some(k) => at += k.max(1),
            None => break,
        }
    }
    let mut eq = equalizer::Equalizer::new(w);
    let mut dec = decoder::Decoder::new(w);
    let mut results: Vec<FieldResult> = fields.iter().map(|f| FieldResult { start: f.start, ..Default::default() }).collect();
    let mut loaded = 0;
    let first = fields[0].start;
    let end = fields[fields.len() - 1].start + FIELD;
    if let Some(t) = trace.as_deref_mut() {
        t.first = first;
        t.loads = loads.iter().enumerate().filter_map(|(f, l)| Some((f, fields[f].start, l.clone()?))).collect();
        dec.decisions = Some(Vec::new());
    }
    for n in first.saturating_sub(equalizer::TAPS)..(end + equalizer::PRE).min(front.y.len()) {
        // Output m is the symbol PRE before the input n.
        let m = n.wrapping_sub(equalizer::PRE);
        let in_fields = (first..end).contains(&m);
        if in_fields && (m - first) % FIELD == 0 {
            if let Some(t) = &loads[(m - first) / FIELD] {
                eq.load(t);
                loaded += 1;
            }
        }
        let e = eq.step(front.y[n]);
        if in_fields {
            if let Some(t) = trace.as_deref_mut() {
                t.eq_out.push(e);
            }
            let i = (m - first) / FIELD;
            let within = (m - first) % FIELD;
            let (seg, k) = (within / SEGMENT, within % SEGMENT);
            results[i].taps = loaded;
            if let Some((sig, err)) = dec.push(seg, k, e) {
                results[i].signal += sig;
                results[i].error += err;
            }
        }
    }
    let codewords = dec.finish();
    if let Some(t) = trace {
        t.decisions = dec.decisions.take().unwrap_or_default();
    }
    let packets = packets::pack(&codewords, &fields, front, &results, w, tracking);
    Some(Run { fields: results, codewords, packets, solves, max_metric: dec.max_metric, max_eq_acc: eq.max_acc })
}

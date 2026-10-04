//! From the equalizer's output to codewords:
//!
//! - **MER:** each field's data symbols against the nearest level: their energies.
//! - **Soft symbols:** the output rounded to `soft_frac` bits below a level, limited to
//!   ±`soft_limit` levels.
//! - **The 12 trellis decoders**, time-shared: symbol k of data segment s goes to decoder
//!   (k + 4 s) mod 12. Each is a 4-state soft Viterbi (081's branch metrics: the squared distance
//!   to the nearer level of each coset, its Z2 kept) whose survivors are registers `traceback`
//!   symbols deep. The decision leaves the best state's register at that depth; the precoder is
//!   undone on it.
//! - **Bytes in field order:** four decisions a byte, each decoder's bytes placed as the
//!   transmitter's commutator took them (`fec::byte_map`).
//! - **The deinterleaver:** 52 branches, branch j delaying its bytes by 4 (51 − j) of the
//!   commutator's turns, so every byte comes out 52 × 4 × 51 bytes after it went in.
//! - **Reed-Solomon's syndromes** of each 207-byte codeword; the flag is their OR.

use std::collections::VecDeque;

use super::{requantize, Widths};
use crate::protocol::atsc::fec::rs;
use crate::protocol::atsc::fec::trellis::{ENCODERS, INTO};
use crate::protocol::atsc::fec::{byte_map, BRANCHES, FIELD_BYTES};
use crate::protocol::atsc::vsb::{DATA_SEGMENTS, DATA_SYMBOLS, SYNC_SYMBOLS};

/// Each decoder's symbols in a field.
pub const PER_FIELD: usize = DATA_SEGMENTS * DATA_SYMBOLS / ENCODERS;

/// One trellis decoder.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viterbi {
    pub metric: [i64; 4],
    /// Each state's survivor: its last decisions (Z2 X1), two bits each, the newest lowest.
    pub survivor: [u64; 4],
    seen: usize,
    last_z2: u8,
}

impl Viterbi {
    /// One soft symbol `r` (`unit` a level) in; the pair (X2 X1) decided `depth` symbols before
    /// it, once there is one; and the largest path metric now.
    pub fn step(&mut self, r: i64, unit: i64, depth: usize) -> (Option<u8>, i64) {
        let (mut cost, mut z2) = ([0i64; 4], [0u8; 4]);
        for c in 0..4 {
            let lo = (2 * c as i64 - 7) * unit;
            let high = r > lo + 4 * unit;
            let d = r - lo - if high { 8 * unit } else { 0 };
            cost[c] = d * d;
            z2[c] = u8::from(high);
        }
        let (mut next, mut surv) = ([0i64; 4], [0u64; 4]);
        for (ns, from) in INTO.iter().enumerate() {
            let ((s0, c0), (s1, c1)) = (from[0], from[1]);
            let (m0, m1) = (self.metric[s0] + cost[c0], self.metric[s1] + cost[c1]);
            // A tie keeps the first way in, as 081's decoder does.
            let second = m1 < m0;
            let (prev, c) = if second { (s1, c1) } else { (s0, c0) };
            next[ns] = m0.min(m1);
            let x1 = ((ns >> 1) as u8) ^ u8::from(second);
            surv[ns] = (self.survivor[prev] << 2) | u64::from((z2[c] << 1) | x1);
        }
        let low = *next.iter().min().unwrap_or(&0);
        for m in next.iter_mut() {
            *m -= low;
        }
        self.metric = next;
        self.survivor = surv;
        self.seen += 1;
        let top = *next.iter().max().unwrap_or(&0);
        if self.seen < depth {
            return (None, top);
        }
        let best = (0..4).fold(0, |b, s| if next[s] < next[b] { s } else { b });
        let pair = ((surv[best] >> (2 * (depth - 1))) & 3) as u8;
        let (z, x1) = (pair >> 1, pair & 1);
        let x2 = z ^ self.last_z2;
        self.last_z2 = z;
        (Some((x2 << 1) | x1), top)
    }
}

/// The convolutional deinterleaver, as FIFOs.
pub struct Deinterleaver {
    branches: Vec<VecDeque<u8>>,
    t: usize,
}

/// Bytes in before the first byte out.
pub const DEINTERLEAVER_DELAY: usize = 4 * BRANCHES * (BRANCHES - 1);

impl Default for Deinterleaver {
    fn default() -> Deinterleaver {
        Deinterleaver { branches: vec![VecDeque::new(); BRANCHES], t: 0 }
    }
}

impl Deinterleaver {
    /// A byte in (the first being a field's first); the byte out, once the delay has passed.
    pub fn push(&mut self, b: u8) -> Option<u8> {
        let j = self.t % BRANCHES;
        let fifo = &mut self.branches[j];
        fifo.push_back(b);
        let out = if fifo.len() > 4 * (BRANCHES - 1 - j) { fifo.pop_front() } else { None };
        self.t += 1;
        if self.t > DEINTERLEAVER_DELAY { out } else { None }
    }
}

/// A codeword out of the deinterleaver.
#[derive(Debug, Clone)]
pub struct Codeword {
    /// The field (from the first decoded) and its place in the field (0..312).
    pub field: usize,
    pub index: usize,
    pub bytes: [u8; rs::N],
    /// A syndrome is not zero.
    pub flag: bool,
}

/// The decoder from the equalizer's output on.
pub struct Decoder {
    unit_e: i64,
    soft_shift: i32,
    soft_limit: i64,
    unit_s: i64,
    depth: usize,
    viterbi: [Viterbi; ENCODERS],
    /// Decisions each decoder has given.
    given: [usize; ENCODERS],
    /// Per decoder, its n-th byte of a field's place in the field.
    place: Vec<Vec<u32>>,
    /// Fields being put back in order: their first field, and each one's bytes and decisions.
    first_open: usize,
    open: VecDeque<(Vec<u8>, usize)>,
    deint: Deinterleaver,
    out_bytes: usize,
    cw: [u8; rs::N],
    pub codewords: Vec<Codeword>,
    pub max_metric: i64,
}

impl Decoder {
    pub fn new(w: &Widths) -> Decoder {
        let (enc, nth) = byte_map();
        let mut place = vec![vec![0u32; FIELD_BYTES / ENCODERS]; ENCODERS];
        for b in 0..FIELD_BYTES {
            place[enc[b] as usize][nth[b] as usize] = b as u32;
        }
        let fe = w.eq_out as i32 - 6;
        Decoder {
            unit_e: 1 << fe,
            soft_shift: fe - w.soft_frac as i32,
            soft_limit: i64::from(w.soft_limit) << w.soft_frac,
            unit_s: 1 << w.soft_frac,
            depth: w.traceback,
            viterbi: [Viterbi::default(); ENCODERS],
            given: [0; ENCODERS],
            place,
            first_open: 0,
            open: VecDeque::new(),
            deint: Deinterleaver::default(),
            out_bytes: 0,
            cw: [0; rs::N],
            codewords: Vec::new(),
            max_metric: 0,
        }
    }

    /// The nearest level to `e` (in the equalizer's output units).
    pub fn slice(&self, e: i64) -> i64 {
        let u = self.unit_e;
        let c = ((e + 8 * u) / (2 * u)).clamp(0, 7);
        (2 * c - 7) * u
    }

    /// The soft symbol for `e`.
    pub fn soft(&self, e: i64) -> i64 {
        let s = if self.soft_shift >= 0 { i64::from(requantize(e, self.soft_shift as u32, 32)) } else { e << -self.soft_shift };
        s.clamp(-self.soft_limit, self.soft_limit)
    }

    /// Symbol `k` of segment `seg` of the field at the equalizer's output (the fields in order,
    /// from the first decoded); for a data symbol, its energy and its error's.
    pub fn push(&mut self, seg: usize, k: usize, e: i32) -> Option<(f64, f64)> {
        if seg == 0 || k < SYNC_SYMBOLS {
            return None;
        }
        let e = i64::from(e);
        let d = self.slice(e);
        let r = self.soft(e);
        let dseg = seg - 1;
        let enc = (k - SYNC_SYMBOLS + 4 * dseg) % ENCODERS;
        let (pair, top) = self.viterbi[enc].step(r, self.unit_s, self.depth);
        self.max_metric = self.max_metric.max(top);
        if let Some(p) = pair {
            self.place_pair(enc, p);
        }
        Some(((d * d) as f64, ((e - d) * (e - d)) as f64))
    }

    /// Decoder `enc`'s next decision into its field's bytes; whole fields on to the deinterleaver.
    fn place_pair(&mut self, enc: usize, pair: u8) {
        let p = self.given[enc];
        self.given[enc] += 1;
        let (f, q) = (p / PER_FIELD, p % PER_FIELD);
        while self.first_open + self.open.len() <= f {
            self.open.push_back((vec![0u8; FIELD_BYTES], 0));
        }
        let b = self.place[enc][q / 4] as usize;
        let slot = &mut self.open[f - self.first_open];
        slot.0[b] |= pair << (6 - 2 * (q % 4));
        slot.1 += 1;
        while self.open.front().is_some_and(|o| o.1 == FIELD_BYTES * 4) {
            let (bytes, _) = self.open.pop_front().unwrap_or_default();
            self.first_open += 1;
            for b in bytes {
                if let Some(out) = self.deint.push(b) {
                    self.codeword_byte(out);
                }
            }
        }
    }

    fn codeword_byte(&mut self, b: u8) {
        let m = self.out_bytes;
        self.out_bytes += 1;
        self.cw[m % rs::N] = b;
        if m % rs::N == rs::N - 1 {
            let s = m / rs::N;
            let flag = rs::syndromes(&self.cw).iter().any(|&v| v != 0);
            self.codewords.push(Codeword { field: s / DATA_SEGMENTS, index: s % DATA_SEGMENTS, bytes: self.cw, flag });
        }
    }

    pub fn finish(&mut self) -> Vec<Codeword> {
        std::mem::take(&mut self.codewords)
    }
}

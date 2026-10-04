//! The 12 interleaved trellis codes. Symbol k of data segment s comes from encoder
//! (k + 4 s) mod 12; an encoder takes whole bytes, two bits a symbol, most significant first. Of
//! each pair the first bit (X2) is precoded (Y2 = X2 ⊕ the encoder's previous Y2) and sent as
//! Z2; the second (X1) goes through a 4-state rate-1/2 code: Z1 = X1, Z0 = D1, then D1 = X1 ⊕ D2,
//! D2 = D1. The symbol is the level of Z2 Z1 Z0.
//!
//! The decoder is a soft-decision Viterbi on the 4 states: each branch's metric is the squared
//! distance to the nearer of its coset's two levels (Z2 is uncoded), whose Z2 is kept with it.

use crate::protocol::atsc::vsb::level;

pub const ENCODERS: usize = 12;

/// The encoder of data symbol `symbol` (0..828) of data segment `segment` (0..312).
pub fn encoder(segment: usize, symbol: usize) -> usize {
    (symbol + 4 * segment) % ENCODERS
}

/// One encoder's state, for building test signals.
#[derive(Debug, Clone, Copy, Default)]
pub struct Encoder {
    y2: u8,
    d1: u8,
    d2: u8,
}

impl Encoder {
    /// The 3-bit symbol value for the bit pair (X2, X1).
    pub fn symbol(&mut self, x2: u8, x1: u8) -> u8 {
        self.y2 ^= x2;
        let z0 = self.d1;
        self.d1 = x1 ^ self.d2;
        self.d2 = z0;
        (self.y2 << 2) | (x1 << 1) | z0
    }
}

/// State (D1, D2) as 2 D1 + D2; from state `s` with input `x1`: the next state and the coset
/// (Z1 Z0) of the symbol sent.
fn branch(s: usize, x1: usize) -> (usize, usize) {
    let (d1, d2) = (s >> 1, s & 1);
    (((x1 ^ d2) << 1) | d1, (x1 << 1) | d1)
}

/// Viterbi decoding of one encoder's symbols (in the ±1..±7 units, pilot removed): its bit pairs
/// X2 X1 as `(x2 << 1) | x1`, one a symbol. The precoder is undone along the way; the first
/// symbol's X2 assumes a previous Y2 of 0.
pub fn decode(symbols: &[f32]) -> Vec<u8> {
    let n = symbols.len();
    // Per step and next state: the previous state (2 bits) and the decided Z2 (bit 2).
    let mut back = vec![[0u8; 4]; n];
    let mut metric = [0f32; 4];
    for (t, &r) in symbols.iter().enumerate() {
        // The nearer level of each coset, and its Z2.
        let mut cost = [0f32; 4];
        let mut z2 = [0u8; 4];
        for c in 0..4 {
            let lo = level(c as u8);
            let hi = lo + 8.0;
            let (a, b) = ((r - lo) * (r - lo), (r - hi) * (r - hi));
            if b < a {
                cost[c] = b;
                z2[c] = 1;
            } else {
                cost[c] = a;
            }
        }
        let mut next = [f32::INFINITY; 4];
        for s in 0..4 {
            for x1 in 0..2 {
                let (ns, c) = branch(s, x1);
                let m = metric[s] + cost[c];
                if m < next[ns] {
                    next[ns] = m;
                    back[t][ns] = s as u8 | (z2[c] << 2);
                }
            }
        }
        let low = next.iter().copied().fold(f32::INFINITY, f32::min);
        for (m, x) in metric.iter_mut().zip(next) {
            *m = x - low;
        }
    }
    let mut out = vec![0u8; n];
    let mut s = (0..4).min_by(|&a, &b| metric[a].total_cmp(&metric[b])).unwrap_or(0);
    let mut y2 = vec![0u8; n];
    for t in (0..n).rev() {
        let b = back[t][s];
        let prev = (b & 3) as usize;
        // x1 is the input that took `prev` to `s`.
        let x1 = (s >> 1) ^ (prev & 1);
        y2[t] = b >> 2;
        out[t] = x1 as u8;
        s = prev;
    }
    let mut last = 0u8;
    for (o, &y) in out.iter_mut().zip(&y2) {
        *o |= (y ^ last) << 1;
        last = y;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random_pairs(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x & 3) as u8
            })
            .collect()
    }

    #[test]
    fn noiseless_symbols_decode_to_their_bits() {
        let pairs = random_pairs(5_000, 7);
        let mut e = Encoder::default();
        let syms: Vec<f32> = pairs.iter().map(|&p| level(e.symbol(p >> 1, p & 1))).collect();
        assert_eq!(decode(&syms), pairs);
    }

    #[test]
    fn the_code_corrects_noise_a_slicer_would_not() {
        let pairs = random_pairs(20_000, 11);
        let mut e = Encoder::default();
        let mut x = 99u64;
        let mut gauss = || {
            // Box-Muller from a small LCG.
            let mut u = || {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((x >> 11) as f64 + 0.5) / (1u64 << 53) as f64
            };
            ((-2.0 * u().ln()).sqrt() * (2.0 * std::f64::consts::PI * u()).cos()) as f32
        };
        // Noise of 0.55 a symbol: a slicer errs on about 7 % of symbols.
        let syms: Vec<f32> = pairs.iter().map(|&p| level(e.symbol(p >> 1, p & 1)) + 0.55 * gauss()).collect();
        let got = decode(&syms);
        let x1_errors = got.iter().zip(&pairs).filter(|(a, b)| (*a & 1) != (*b & 1)).count();
        assert!(x1_errors < pairs.len() / 200, "{x1_errors} X1 errors in {}", pairs.len());
    }

    #[test]
    fn segments_rotate_the_encoders_by_four() {
        assert_eq!((encoder(0, 0), encoder(1, 0), encoder(2, 0), encoder(3, 0)), (0, 4, 8, 0));
        assert_eq!(encoder(1, 13), 5);
    }
}

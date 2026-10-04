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
#[cfg(test)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Encoder {
    y2: u8,
    d1: u8,
    d2: u8,
}

#[cfg(test)]
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

/// State (D1, D2) as 2 D1 + D2. Next state 2a + b comes from the two states 2b + D2 (D2 = 0, 1)
/// on input X1 = a ⊕ D2, sending coset (Z1 Z0) 2 X1 + b. Per next state, per D2: the previous
/// state and the coset.
const INTO: [[(usize, usize); 2]; 4] = [[(0, 0), (1, 2)], [(2, 1), (3, 3)], [(0, 2), (1, 0)], [(2, 3), (3, 1)]];

/// Viterbi decoding of one encoder's symbols (in the ±1..±7 units, pilot removed): its bit pairs
/// X2 X1 as `(x2 << 1) | x1`, one a symbol. The precoder is undone along the way; the first
/// symbol's X2 assumes a previous Y2 of 0. Only comparisons and arithmetic in the loop (min and
/// round are library calls on the A9).
pub fn decode(symbols: &[f32]) -> Vec<u8> {
    let n = symbols.len();
    // Per step, two bits a next state (at 2 × state): the D2 of the state it came from, and the
    // decided Z2 above it.
    let mut back = vec![0u8; n];
    let mut metric = [0f32; 4];
    for (t, &r) in symbols.iter().enumerate() {
        // Coset c's levels are 2c − 7 and 2c + 1; the nearer one, its distance and its Z2.
        let mut cost = [0f32; 4];
        let mut z2 = [0u8; 4];
        for c in 0..4 {
            let lo = level(c as u8);
            let high = r > lo + 4.0;
            let d = if high { r - lo - 8.0 } else { r - lo };
            cost[c] = d * d;
            z2[c] = u8::from(high);
        }
        let mut next = [0f32; 4];
        let mut bits = 0u8;
        for (ns, from) in INTO.iter().enumerate() {
            let (s0, c0) = from[0];
            let (s1, c1) = from[1];
            let (m0, m1) = (metric[s0] + cost[c0], metric[s1] + cost[c1]);
            let (m, d2, c) = if m1 < m0 { (m1, 1, c1) } else { (m0, 0, c0) };
            next[ns] = m;
            bits |= (d2 | (z2[c] << 1)) << (2 * ns);
        }
        back[t] = bits;
        // Only the differences between states matter.
        let base = next[0];
        for (m, x) in metric.iter_mut().zip(next) {
            *m = x - base;
        }
    }
    let mut s = 0;
    for k in 1..4 {
        if metric[k] < metric[s] {
            s = k;
        }
    }
    let mut out = vec![0u8; n];
    for t in (0..n).rev() {
        let b = (back[t] >> (2 * s)) & 3;
        let d2 = usize::from(b & 1);
        // x1 = a ⊕ D2 for next state 2a + b; Z2 kept above it for the precoder.
        out[t] = ((s >> 1) ^ d2) as u8 | ((b >> 1) << 1);
        s = ((s & 1) << 1) | d2;
    }
    // X2 = Y2 ⊕ the previous Y2.
    let mut last = 0u8;
    for o in out.iter_mut() {
        let y = *o >> 1;
        *o = (*o & 1) | ((y ^ last) << 1);
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
    fn the_transitions_are_the_encoders() {
        // Every (state, input) of the encoder lands where INTO says it comes from.
        for d1 in 0..2u8 {
            for d2 in 0..2u8 {
                for x1 in 0..2u8 {
                    let mut e = Encoder { y2: 0, d1, d2 };
                    let coset = e.symbol(0, x1) & 3;
                    let (s, ns) = (usize::from(2 * d1 + d2), usize::from(2 * e.d1 + e.d2));
                    assert_eq!(INTO[ns][usize::from(d2)], (s, usize::from(coset)), "from {s} on {x1}");
                }
            }
        }
    }

    #[test]
    fn segments_rotate_the_encoders_by_four() {
        assert_eq!((encoder(0, 0), encoder(1, 0), encoder(2, 0), encoder(3, 0)), (0, 4, 8, 0));
        assert_eq!(encoder(1, 13), 5);
    }
}

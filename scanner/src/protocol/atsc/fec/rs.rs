//! Reed-Solomon (207,187): 187 data bytes and 20 parity bytes over GF(256) (x^8 + x^4 + x^3 +
//! x^2 + 1), the generator's roots α^0..α^19; up to 10 byte errors are corrected. Byte 0 is the
//! highest power.

use std::sync::OnceLock;

pub const N: usize = 207;
pub const K: usize = 187;
const PARITY: usize = N - K;
const T: usize = PARITY / 2;

struct Gf {
    exp: [u8; 512],
    log: [u16; 256],
}

fn gf() -> &'static Gf {
    static GF: OnceLock<Gf> = OnceLock::new();
    GF.get_or_init(|| {
        let mut g = Gf { exp: [0; 512], log: [0; 256] };
        let mut v: u16 = 1;
        for i in 0..255 {
            g.exp[i] = v as u8;
            g.log[v as usize] = i as u16;
            v <<= 1;
            if v & 0x100 != 0 {
                v ^= 0x11D;
            }
        }
        for i in 255..512 {
            g.exp[i] = g.exp[i - 255];
        }
        g
    })
}

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let g = gf();
    g.exp[g.log[a as usize] as usize + g.log[b as usize] as usize]
}

fn div(a: u8, b: u8) -> u8 {
    if a == 0 {
        return 0;
    }
    let g = gf();
    g.exp[(g.log[a as usize] as usize + 255 - g.log[b as usize] as usize) % 255]
}

fn pow(i: usize) -> u8 {
    gf().exp[i % 255]
}

/// p(x) at `x`, coefficients lowest power first.
fn eval(p: &[u8], x: u8) -> u8 {
    p.iter().rev().fold(0, |acc, &c| mul(acc, x) ^ c)
}

/// What decoding did to a codeword.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Clean,
    /// This many byte errors corrected.
    Corrected(usize),
    /// More errors than the code corrects.
    Failed,
}

/// Correct `cw` (207 bytes) in place.
pub fn decode(cw: &mut [u8]) -> Outcome {
    debug_assert_eq!(cw.len(), N);
    let mut syn = [0u8; PARITY];
    for (j, s) in syn.iter_mut().enumerate() {
        let a = pow(j);
        *s = cw.iter().fold(0, |acc, &c| mul(acc, a) ^ c);
    }
    if syn.iter().all(|&s| s == 0) {
        return Outcome::Clean;
    }
    // Berlekamp-Massey: the error locator Λ(x), lowest power first.
    let mut lambda = vec![1u8];
    let mut prev = vec![1u8];
    let (mut l, mut m, mut b) = (0usize, 1usize, 1u8);
    for n in 0..PARITY {
        let mut d = syn[n];
        for i in 1..=l.min(lambda.len() - 1) {
            d ^= mul(lambda[i], syn[n - i]);
        }
        if d == 0 {
            m += 1;
            continue;
        }
        let coef = div(d, b);
        let mut next = lambda.clone();
        if next.len() < prev.len() + m {
            next.resize(prev.len() + m, 0);
        }
        for (i, &p) in prev.iter().enumerate() {
            next[i + m] ^= mul(coef, p);
        }
        if 2 * l <= n {
            prev = lambda;
            l = n + 1 - l;
            b = d;
            m = 1;
        } else {
            m += 1;
        }
        lambda = next;
    }
    while lambda.len() > 1 && *lambda.last().unwrap_or(&0) == 0 {
        lambda.pop();
    }
    if l > T || lambda.len() - 1 != l {
        return Outcome::Failed;
    }
    // Ω(x) = S(x) Λ(x) mod x^20.
    let mut omega = [0u8; PARITY];
    for (i, &li) in lambda.iter().enumerate() {
        for j in 0..PARITY - i {
            omega[i + j] ^= mul(li, syn[j]);
        }
    }
    // Λ'(x): the odd terms, one power down.
    let deriv: Vec<u8> = (0..lambda.len()).map(|i| if i % 2 == 1 { lambda[i] } else { 0 }).skip(1).collect();
    let mut found = 0;
    for (i, byte) in cw.iter_mut().enumerate() {
        let power = N - 1 - i;
        let x = pow(power);
        let xinv = pow(255 - power % 255);
        if eval(&lambda, xinv) != 0 {
            continue;
        }
        let den = eval(&deriv, xinv);
        if den == 0 {
            return Outcome::Failed;
        }
        // Forney with the first root α^0: e = X Ω(X^-1) / Λ'(X^-1).
        *byte ^= mul(x, div(eval(&omega, xinv), den));
        found += 1;
    }
    if found == l { Outcome::Corrected(found) } else { Outcome::Failed }
}

/// The 20 parity bytes of 187 data bytes (for test signals).
#[cfg(test)]
pub fn parity(data: &[u8]) -> [u8; PARITY] {
    static GEN: OnceLock<[u8; PARITY + 1]> = OnceLock::new();
    // g(x) = Π (x + α^j), highest power first.
    let g = GEN.get_or_init(|| {
        let mut g = vec![1u8];
        for j in 0..PARITY {
            let mut next = vec![0u8; g.len() + 1];
            for (i, &c) in g.iter().enumerate() {
                next[i] ^= c;
                next[i + 1] ^= mul(c, pow(j));
            }
            g = next;
        }
        g.try_into().unwrap_or([0; PARITY + 1])
    });
    let mut reg = [0u8; PARITY];
    for &d in data {
        let f = d ^ reg[0];
        reg.copy_within(1.., 0);
        reg[PARITY - 1] = 0;
        for (r, &gi) in reg.iter_mut().zip(&g[1..]) {
            *r ^= mul(f, gi);
        }
    }
    reg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codeword(seed: u8) -> Vec<u8> {
        let data: Vec<u8> = (0..K).map(|i| (i as u8).wrapping_mul(37).wrapping_add(seed)).collect();
        let mut cw = data.clone();
        cw.extend_from_slice(&parity(&data));
        cw
    }

    #[test]
    fn a_codeword_is_clean() {
        let mut cw = codeword(3);
        assert_eq!(decode(&mut cw), Outcome::Clean);
    }

    #[test]
    fn up_to_ten_byte_errors_are_corrected() {
        for errors in [1, 4, 10] {
            let good = codeword(errors as u8);
            let mut cw = good.clone();
            for k in 0..errors {
                cw[(k * 19 + 5) % N] ^= 0x5A ^ k as u8;
            }
            assert_eq!(decode(&mut cw), Outcome::Corrected(errors), "{errors} errors");
            assert_eq!(cw, good);
        }
    }

    #[test]
    fn eleven_errors_are_reported_not_miscorrected_into_silence() {
        let good = codeword(9);
        let mut cw = good.clone();
        for k in 0..11 {
            cw[k * 17] ^= 0xFF;
        }
        let out = decode(&mut cw);
        assert!(out == Outcome::Failed || cw != good, "{out:?}");
        assert_ne!(out, Outcome::Clean);
    }
}

//! The Reed-Solomon codes a UAT message carries: over GF(256) by x^8 + x^7 + x^2 + x + 1, a
//! code's generator having as its roots the powers of alpha from the 120th on: twelve of them
//! for a short message (30 bytes, 18 of them the message) and fourteen for a long one (48 and
//! 34). `correct` puts right as many wrong bytes as half the roots, wherever they are.

/// The field's polynomial, and the first root of every generator.
const POLY: u16 = 0x187;
const FIRST_ROOT: usize = 120;
/// The most roots a code here has, and the room its polynomials need.
pub const MOST_ROOTS: usize = 14;
const ROOM: usize = MOST_ROOTS + 2;

struct Field {
    exp: [u8; 510],
    log: [u8; 256],
}

const FIELD: Field = field();

const fn field() -> Field {
    let mut f = Field { exp: [0; 510], log: [0; 256] };
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        f.exp[i] = x as u8;
        f.exp[i + 255] = x as u8;
        f.log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 {
            x ^= POLY;
        }
        i += 1;
    }
    f
}

fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        0
    } else {
        FIELD.exp[usize::from(FIELD.log[usize::from(a)]) + usize::from(FIELD.log[usize::from(b)])]
    }
}

fn inv(a: u8) -> u8 {
    FIELD.exp[255 - usize::from(FIELD.log[usize::from(a)])]
}

/// Alpha to a power.
fn alpha(power: usize) -> u8 {
    FIELD.exp[power % 255]
}

/// A polynomial at `x`, its coefficients the lowest power first.
fn at(poly: &[u8], x: u8) -> u8 {
    poly.iter().rev().fold(0, |y, &c| mul(y, x) ^ c)
}

/// What `block` leaves at each of its code's roots; true when any is left.
fn syndromes(block: &[u8], roots: usize, out: &mut [u8; ROOM]) -> bool {
    let mut any = false;
    for (i, s) in out.iter_mut().enumerate().take(roots) {
        let a = alpha(FIRST_ROOT + i);
        *s = block.iter().fold(0, |s, &byte| mul(s, a) ^ byte);
        any |= *s != 0;
    }
    any
}

/// `block` (the message's bytes, then its parity's) with its wrong bytes put right, and how
/// many were wrong; none, and the block as it was, when it is further than half its code's
/// roots from any word of the code.
pub fn correct(block: &mut [u8], roots: usize) -> Option<usize> {
    debug_assert!(roots <= MOST_ROOTS && block.len() <= 255);
    let n = block.len();
    let mut left = [0u8; ROOM];
    if !syndromes(block, roots, &mut left) {
        return Some(0);
    }
    // The shortest locator that makes the syndromes (Berlekamp and Massey): its roots are the
    // inverses of where the wrong bytes are.
    let (mut sigma, mut old) = ([0u8; ROOM], [0u8; ROOM]);
    (sigma[0], old[0]) = (1, 1);
    let (mut length, mut shift, mut last) = (0usize, 1usize, 1u8);
    for r in 0..roots {
        let mut delta = left[r];
        for i in 1..=length {
            delta ^= mul(sigma[i], left[r - i]);
        }
        if delta == 0 {
            shift += 1;
            continue;
        }
        let scale = mul(delta, inv(last));
        let mut new = sigma;
        for i in 0..ROOM - shift {
            new[i + shift] ^= mul(old[i], scale);
        }
        if 2 * length <= r {
            (old, last, length, shift) = (sigma, delta, r + 1 - length, 1);
        } else {
            shift += 1;
        }
        sigma = new;
    }
    let degree = sigma.iter().rposition(|&c| c != 0).unwrap_or(0);
    if degree != length || length > roots / 2 {
        return None;
    }
    let sigma = &sigma[..=degree];
    // What the locator and the syndromes give of each wrong byte's value (Forney).
    let mut omega = [0u8; ROOM];
    for i in 0..roots {
        for k in 0..=i.min(degree) {
            omega[i] ^= mul(sigma[k], left[i - k]);
        }
    }
    let mut slope = [0u8; ROOM];
    for i in (1..=degree).step_by(2) {
        slope[i - 1] = sigma[i];
    }
    // The byte at `j` is the coefficient of x^(n-1-j).
    let mut fixed = [0u8; 255];
    let fixed = &mut fixed[..n];
    fixed.copy_from_slice(block);
    let mut found = 0;
    for (j, byte) in fixed.iter_mut().enumerate() {
        let power = n - 1 - j;
        let x_inv = alpha(255 - power % 255);
        if at(sigma, x_inv) != 0 {
            continue;
        }
        let below = at(&slope[..degree], x_inv);
        if below == 0 {
            return None;
        }
        // The place to the power of one less the first root, times omega over the slope.
        *byte ^= mul(alpha(power * (256 - FIRST_ROOT)), mul(at(&omega[..roots], x_inv), inv(below)));
        found += 1;
    }
    if found != length || syndromes(fixed, roots, &mut left) {
        return None;
    }
    block.copy_from_slice(fixed);
    Some(length)
}

/// The parity's bytes for a message's, for the tests that make a block.
#[cfg(test)]
pub(crate) fn parity(message: &[u8], roots: usize) -> Vec<u8> {
    // The generator, its highest power first, by its roots one at a time.
    let mut generator = vec![1u8];
    for i in 0..roots {
        let root = alpha(FIRST_ROOT + i);
        generator.push(0);
        for k in (1..generator.len()).rev() {
            generator[k] ^= mul(generator[k - 1], root);
        }
    }
    // What the message, moved up by the parity's length, leaves of the generator.
    let mut rest = vec![0u8; roots];
    for &byte in message {
        let lead = byte ^ rest[0];
        rest.rotate_left(1);
        rest[roots - 1] = 0;
        for (r, &g) in rest.iter_mut().zip(&generator[1..]) {
            *r ^= mul(g, lead);
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(len: usize, roots: usize, seed: u32) -> Vec<u8> {
        let mut x = seed;
        let mut message: Vec<u8> = (0..len - roots)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (x >> 24) as u8
            })
            .collect();
        let parity = parity(&message, roots);
        message.extend(parity);
        message
    }

    #[test]
    fn a_block_as_sent_leaves_nothing_and_wrong_bytes_are_put_right() {
        for (len, roots) in [(30, 12), (48, 14)] {
            let sent = block(len, roots, 7 + len as u32);
            let mut got = sent.clone();
            assert_eq!(correct(&mut got, roots), Some(0), "as sent");
            // As many wrong bytes as the code mends, at both ends and in the middle.
            for wrong in 1..=roots / 2 {
                let mut got = sent.clone();
                for k in 0..wrong {
                    let j = [0, len - 1, len / 2, 3, len - 4, 9, 17][k];
                    got[j] ^= (0x41 + 37 * k) as u8;
                }
                assert_eq!((correct(&mut got, roots), got == sent), (Some(wrong), true), "{wrong} wrong of {len}");
            }
        }
    }

    #[test]
    fn a_block_with_too_many_wrong_is_left_as_it_was() {
        for (len, roots) in [(30, 12), (48, 14)] {
            let sent = block(len, roots, 99);
            let mut failed = 0;
            for trial in 0..40usize {
                let mut got = sent.clone();
                for k in 0..=roots / 2 {
                    got[(trial * 5 + k * 4) % len] ^= 1 + ((trial + 3 * k) % 255) as u8;
                }
                let before = got.clone();
                match correct(&mut got, roots) {
                    None => {
                        assert_eq!(got, before);
                        failed += 1;
                    }
                    // One more wrong than it mends is, once in a great while, within reach of
                    // another word of the code: then it is that word, and not the one sent.
                    Some(_) => assert_ne!(got, sent),
                }
            }
            assert!(failed >= 38, "{failed} of 40 with {} wrong were refused", roots / 2 + 1);
        }
    }
}

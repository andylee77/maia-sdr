//! Generic Reed-Solomon decoder over GF(2^6) for P25 Phase 1
//! shortened codes.
//!
//! The underlying code is always RS(63, k, t) over GF(2^6) with
//! generator polynomial `x^6 + x + 1` (primitive α = 2). The three P25
//! shortenings we need all share this backbone:
//!
//! | Shortened code | k  | t | Protects                                       |
//! |----------------|----|---|------------------------------------------------|
//! | RS(24,12,13)   | 51 | 6 | TDULC LC, LDU1 LC (72-bit LCW)                 |
//! | RS(24,16,9)    | 55 | 4 | LDU2 ESS (96-bit encryption sync signature)    |
//! | RS(63,47,17)   | 47 | 8 | HDU body (120-bit header carrying alg/key/MI)  |
//!
//! Shortening is applied by the caller: populate `input[0..L]` with
//! the received hexbits in the order the relevant SDRTrunk decoder
//! uses, and zero-fill `input[L..63]`. The caller is also responsible
//! for packing/unpacking the post-decode hexbits back into the data
//! unit's LC / ESS / HDU layout.
//!
//! Implementation is a direct port of SDRTrunk's
//! `edac/BerlekempMassey.java` + `edac/ReedSolomon_63_P25.java`, lifted
//! out of our original `rs_24_12_13.rs` and parameterised on `kk`
//! (number of information symbols). `tt = (NN - kk) / 2` is derived.
//!
//! Reference: TIA 102-BAAA §4.9 (Reed-Solomon Code Generator Matrices)
//! and Simon Rockliff's 1991 reference C implementation.

pub const MM: usize = 6;
pub const NN: usize = 63;

/// Build α-power ↔ polynomial-form lookup tables for GF(2^6). See the
/// rs_24_12_13 module-level comment for the full derivation — this is
/// the same function, factored out because all three P25 RS variants
/// share the same field.
fn generate_gf() -> ([u32; NN + 1], [i32; NN + 1]) {
    let pp: [u32; MM + 1] = [1, 1, 0, 0, 0, 0, 1];
    let mut alpha_to = [0u32; NN + 1];
    let mut index_of = [-1i32; NN + 1];

    let mut mask: u32 = 1;
    alpha_to[MM] = 0;
    for i in 0..MM {
        alpha_to[i] = mask;
        index_of[alpha_to[i] as usize] = i as i32;
        if pp[i] != 0 {
            alpha_to[MM] ^= mask;
        }
        mask <<= 1;
    }
    index_of[alpha_to[MM] as usize] = MM as i32;
    let mut mask = mask >> 1;
    for i in (MM + 1)..NN {
        if alpha_to[i - 1] >= mask {
            alpha_to[i] = alpha_to[MM] ^ ((alpha_to[i - 1] ^ mask) << 1);
        } else {
            alpha_to[i] = alpha_to[i - 1] << 1;
        }
        index_of[alpha_to[i] as usize] = i as i32;
    }
    let _ = mask;
    index_of[0] = -1;
    (alpha_to, index_of)
}

/// Decode one 63-symbol RS(63,kk,2t+1) codeword in place.
///
/// * `input` — 63 6-bit symbols (polynomial form 0..=63). For a
///   shortened code populate `input[0..L]` with the received hexbits
///   in SDRTrunk's RS-input order and leave `input[L..63]` zero-filled.
/// * `kk` — number of information symbols. `t = (NN - kk) / 2` is
///   derived.
///
/// Returns `Ok(corrected)` with the full 63-symbol codeword on
/// success, `Err(uncorrected)` when the decoder cannot solve.
pub fn decode(input: &[u32; NN], kk: usize) -> Result<[u32; NN], [u32; NN]> {
    assert!(kk < NN, "kk must be less than NN={}", NN);
    let tt = (NN - kk) / 2;
    assert!((NN - kk) % 2 == 0, "NN-kk must be even (2t)");
    let (alpha_to, index_of) = generate_gf();

    // Convert received symbols to α-exponent form.
    let mut output_idx = [0i32; NN];
    for i in 0..NN {
        output_idx[i] = index_of[input[i] as usize];
    }

    // Syndromes s[1..=(NN-KK)] via Horner-style evaluation.
    let n_minus_k = NN - kk;
    let mut s = vec![0i64; n_minus_k + 1];
    let mut syn_error = false;
    for i in 1..=n_minus_k {
        let mut acc = 0u32;
        for j in 0..NN {
            if output_idx[j] != -1 {
                acc ^= alpha_to[((output_idx[j] as i64
                    + (i as i64) * (j as i64))
                    .rem_euclid(NN as i64)) as usize];
            }
        }
        if acc != 0 {
            syn_error = true;
        }
        s[i] = index_of[acc as usize] as i64;
    }

    if !syn_error {
        return Ok(*input);
    }

    // Berlekamp iteration for the error-locator polynomial.
    let rows = n_minus_k + 2;
    let cols = n_minus_k;
    let mut elp = vec![vec![0i64; cols]; rows];
    let mut d = vec![-1i64; rows];
    let mut l = vec![0i64; rows];
    let mut u_lu = vec![0i64; rows];

    d[0] = 0;
    d[1] = s[1];
    elp[0][0] = 0;
    elp[1][0] = 1;
    for i in 1..cols {
        elp[0][i] = -1;
        elp[1][i] = 0;
    }
    l[0] = 0;
    l[1] = 0;
    u_lu[0] = -1;
    u_lu[1] = 0;

    let mut u: usize = 0;
    loop {
        u += 1;
        if d[u] == -1 {
            l[u + 1] = l[u];
            for i in 0..=(l[u] as usize) {
                elp[u + 1][i] = elp[u][i];
                elp[u][i] = if elp[u][i] == 0 {
                    -1
                } else {
                    index_of[elp[u][i] as usize] as i64
                };
            }
        } else {
            let mut q: i64 = u as i64 - 1;
            while q > 0 && d[q as usize] == -1 {
                q -= 1;
            }
            if q > 0 {
                let mut j = q;
                loop {
                    j -= 1;
                    if j < 0 {
                        break;
                    }
                    if d[j as usize] != -1
                        && u_lu[q as usize] < u_lu[j as usize]
                    {
                        q = j;
                    }
                    if j == 0 {
                        break;
                    }
                }
            }
            l[u + 1] = l[u].max(l[q as usize] + u as i64 - q);
            for i in 0..cols {
                elp[u + 1][i] = 0;
            }
            for i in 0..=(l[q as usize] as usize) {
                if elp[q as usize][i] != -1 {
                    let e = elp[q as usize][i];
                    let idx = (d[u] + NN as i64 - d[q as usize] + e)
                        .rem_euclid(NN as i64) as usize;
                    elp[u + 1][i + u - q as usize] = alpha_to[idx] as i64;
                }
            }
            for i in 0..=(l[u] as usize) {
                elp[u + 1][i] ^= elp[u][i];
                elp[u][i] = if elp[u][i] == 0 {
                    -1
                } else {
                    index_of[elp[u][i] as usize] as i64
                };
            }
        }
        u_lu[u + 1] = u as i64 - l[u + 1];

        if u < cols {
            d[u + 1] = if s[u + 1] != -1 {
                alpha_to[s[u + 1] as usize] as i64
            } else {
                0
            };
            for i in 1..=(l[u + 1] as usize) {
                if s[u + 1 - i] != -1 && elp[u + 1][i] != 0 {
                    let idx = (s[u + 1 - i]
                        + index_of[elp[u + 1][i] as usize] as i64)
                        .rem_euclid(NN as i64) as usize;
                    d[u + 1] ^= alpha_to[idx] as i64;
                }
            }
            d[u + 1] = if d[u + 1] == 0 {
                -1
            } else {
                index_of[d[u + 1] as usize] as i64
            };
        }
        if !(u < cols && l[u + 1] <= tt as i64) {
            break;
        }
    }
    u += 1;

    let mut output = [0u32; NN];
    let mut irrecoverable = false;
    if l[u] <= tt as i64 {
        for i in 0..=(l[u] as usize) {
            elp[u][i] = if elp[u][i] == 0 {
                -1
            } else {
                index_of[elp[u][i] as usize] as i64
            };
        }
        let mut reg = vec![0i64; tt + 1];
        if l[u] >= 0 {
            for i in 1..=(l[u] as usize) {
                reg[i] = elp[u][i];
            }
        }
        let mut root = vec![0i64; tt];
        let mut loc = vec![0i64; tt];
        let mut count: usize = 0;
        for i in 1..=NN {
            let mut q_val: u32 = 1;
            for j in 1..=(l[u] as usize) {
                if reg[j] != -1 {
                    reg[j] = (reg[j] + j as i64).rem_euclid(NN as i64);
                    q_val ^= alpha_to[reg[j] as usize];
                }
            }
            if q_val == 0 && count < tt {
                root[count] = i as i64;
                loc[count] = NN as i64 - i as i64;
                count += 1;
            }
        }
        if count == l[u] as usize {
            let mut z = vec![0i64; tt + 1];
            for i in 1..=(l[u] as usize) {
                z[i] = if s[i] != -1 && elp[u][i] != -1 {
                    (alpha_to[s[i] as usize]
                        ^ alpha_to[elp[u][i] as usize])
                        as i64
                } else if s[i] != -1 && elp[u][i] == -1 {
                    alpha_to[s[i] as usize] as i64
                } else if s[i] == -1 && elp[u][i] != -1 {
                    alpha_to[elp[u][i] as usize] as i64
                } else {
                    0
                };
                for j in 1..i {
                    if s[j] != -1 && elp[u][i - j] != -1 {
                        let idx = (elp[u][i - j] + s[j])
                            .rem_euclid(NN as i64)
                            as usize;
                        z[i] ^= alpha_to[idx] as i64;
                    }
                }
                z[i] = if z[i] == 0 {
                    -1
                } else {
                    index_of[z[i] as usize] as i64
                };
            }
            let mut err = [0u32; NN];
            for i in 0..NN {
                err[i] = 0;
                output[i] = if output_idx[i] != -1 {
                    alpha_to[output_idx[i] as usize]
                } else {
                    0
                };
            }
            for i in 0..(l[u] as usize) {
                err[loc[i] as usize] = 1;
                for j in 1..=(l[u] as usize) {
                    if z[j] != -1 {
                        let idx = (z[j]
                            + (j as i64) * root[i])
                            .rem_euclid(NN as i64)
                            as usize;
                        err[loc[i] as usize] ^= alpha_to[idx];
                    }
                }
                if err[loc[i] as usize] != 0 {
                    let e = index_of[err[loc[i] as usize] as usize];
                    let mut q_acc: i64 = 0;
                    for j in 0..(l[u] as usize) {
                        if j != i {
                            let v = (1 ^ alpha_to[(loc[j] + root[i])
                                .rem_euclid(NN as i64)
                                as usize])
                                as usize;
                            q_acc += index_of[v] as i64;
                        }
                    }
                    q_acc = q_acc.rem_euclid(NN as i64);
                    let idx = (e as i64 - q_acc + NN as i64)
                        .rem_euclid(NN as i64)
                        as usize;
                    err[loc[i] as usize] = alpha_to[idx];
                    output[loc[i] as usize] ^= err[loc[i] as usize];
                }
            }
        } else {
            irrecoverable = true;
        }
    } else {
        irrecoverable = true;
    }

    if irrecoverable {
        for i in 0..NN {
            output[i] = if output_idx[i] != -1 {
                alpha_to[output_idx[i] as usize]
            } else {
                0
            };
        }
        return Err(output);
    }
    Ok(output)
}
#[cfg(test)]
#[path = "rs_p25_tests.rs"]
mod tests;

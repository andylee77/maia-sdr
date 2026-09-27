//! PS memory test patterns (design doc section 8, layer 2).
//!
//! All tests work on a `Region` of 32-bit words accessed with volatile
//! loads/stores, so the same code runs on mlock'd anonymous memory and on a
//! `/dev/mem` window of a reserved carve-out.

use serde_json::{json, Value};
use std::time::Instant;

pub const PATTERNS: &[&str] = &[
    "walking1",
    "walking0",
    "address",
    "inverse-address",
    "marchc",
    "prbs",
    "checkerboard",
];

pub trait Region {
    fn words(&self) -> usize;
    fn rd(&self, i: usize) -> u32;
    fn wr(&mut self, i: usize, v: u32);
    /// Address reported for word `i` (virtual offset or physical address).
    fn addr(&self, i: usize) -> u64;
}

/// A raw pointer region (anon mmap, /dev/mem, or a test Vec).
pub struct PtrRegion {
    pub ptr: *mut u32,
    pub words: usize,
    pub base_addr: u64,
}

impl Region for PtrRegion {
    #[inline]
    fn words(&self) -> usize {
        self.words
    }
    #[inline]
    fn rd(&self, i: usize) -> u32 {
        debug_assert!(i < self.words);
        // SAFETY: i < words; the pointer is valid for the region's lifetime.
        unsafe { std::ptr::read_volatile(self.ptr.add(i)) }
    }
    #[inline]
    fn wr(&mut self, i: usize, v: u32) {
        debug_assert!(i < self.words);
        // SAFETY: as above.
        unsafe { std::ptr::write_volatile(self.ptr.add(i), v) }
    }
    fn addr(&self, i: usize) -> u64 {
        self.base_addr + 4 * i as u64
    }
}

#[derive(Debug, Clone, Default)]
pub struct PatResult {
    pub name: String,
    pub errors: u64,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub seconds: f64,
    pub error_bits: u32,
    pub first: Vec<(u64, u32, u32)>,
}

impl PatResult {
    pub fn to_json(&self) -> Value {
        let mb = (self.bytes_written + self.bytes_read) as f64 / 1e6;
        json!({
            "name": self.name,
            "errors": self.errors,
            "bytes_written": self.bytes_written,
            "bytes_read": self.bytes_read,
            "seconds": crate::util::round3(self.seconds),
            "mbs": if self.seconds > 0.0 { crate::util::round1(mb / self.seconds) } else { 0.0 },
            "error_bits": format!("0x{:08X}", self.error_bits),
            "error_byte_lanes": (0..4).filter(|b| self.error_bits >> (8 * b) & 0xFF != 0).collect::<Vec<_>>(),
            "first_errors": self.first.iter().map(|(a, e, g)| json!({
                "addr": format!("0x{a:08X}"), "expected": format!("0x{e:08X}"),
                "actual": format!("0x{g:08X}"), "xor": format!("0x{:08X}", e ^ g)})).collect::<Vec<_>>(),
        })
    }
}

const MAX_FIRST: usize = 16;

struct Acc<'a, R: Region> {
    r: &'a mut R,
    res: PatResult,
}

impl<'a, R: Region> Acc<'a, R> {
    #[inline]
    fn fill(&mut self, f: impl Fn(usize) -> u32) {
        let n = self.r.words();
        for i in 0..n {
            self.r.wr(i, f(i));
        }
        self.res.bytes_written += 4 * n as u64;
    }

    #[inline]
    fn check_word(&mut self, i: usize, want: u32) {
        let got = self.r.rd(i);
        if got != want {
            self.res.errors += 1;
            self.res.error_bits |= got ^ want;
            if self.res.first.len() < MAX_FIRST {
                self.res.first.push((self.r.addr(i), want, got));
            }
        }
    }

    #[inline]
    fn verify(&mut self, f: impl Fn(usize) -> u32) {
        let n = self.r.words();
        for i in 0..n {
            self.check_word(i, f(i));
        }
        self.res.bytes_read += 4 * n as u64;
    }
}

#[inline]
pub fn xorshift32(mut x: u32) -> u32 {
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    x
}

/// Runs one pattern for `pass` (pass index varies seeds/rotations).
/// `stop` is polled between sweeps; returning true aborts the pattern.
pub fn run_pattern<R: Region>(r: &mut R, name: &str, pass: u32, stop: &dyn Fn() -> bool) -> Option<PatResult> {
    let t0 = Instant::now();
    let mut acc = Acc {
        r,
        res: PatResult {
            name: name.to_string(),
            ..Default::default()
        },
    };
    let n = acc.r.words();
    match name {
        "walking1" | "walking0" => {
            let inv = name == "walking0";
            for b in 0..32u32 {
                if stop() {
                    break;
                }
                let f = move |i: usize| {
                    let v = 1u32.rotate_left((i as u32).wrapping_add(b).wrapping_add(pass) % 32);
                    if inv {
                        !v
                    } else {
                        v
                    }
                };
                acc.fill(f);
                acc.verify(f);
            }
        }
        "address" | "inverse-address" => {
            let inv = name == "inverse-address";
            let b0 = acc.r.addr(0);
            let f = move |i: usize| {
                let a = (b0 + 4 * i as u64) as u32;
                if inv {
                    !a
                } else {
                    a
                }
            };
            acc.fill(f);
            acc.verify(f);
        }
        "checkerboard" => {
            for inv in [false, true] {
                if stop() {
                    break;
                }
                let f = move |i: usize| {
                    let v = if i & 1 == 0 { 0x5555_5555 } else { 0xAAAA_AAAA };
                    if inv {
                        !v
                    } else {
                        v
                    }
                };
                acc.fill(f);
                acc.verify(f);
            }
        }
        "prbs" => {
            let seed = 0x9E37_79B9u32 ^ pass.wrapping_mul(0x85EB_CA6B) | 1;
            let mut s = seed;
            for i in 0..n {
                s = xorshift32(s);
                acc.r.wr(i, s);
            }
            acc.res.bytes_written += 4 * n as u64;
            let mut s = seed;
            for i in 0..n {
                s = xorshift32(s);
                acc.check_word(i, s);
            }
            acc.res.bytes_read += 4 * n as u64;
        }
        "marchc" => {
            // March C-: up(w0); up(r0,w1); up(r1,w0); down(r0,w1); down(r1,w0); up(r0)
            let (z, o) = (0u32, u32::MAX);
            for i in 0..n {
                acc.r.wr(i, z);
            }
            let elems: [(bool, u32, u32); 4] = [(true, z, o), (true, o, z), (false, z, o), (false, o, z)];
            for (up, rv, wv) in elems {
                if stop() {
                    break;
                }
                if up {
                    for i in 0..n {
                        acc.check_word(i, rv);
                        acc.r.wr(i, wv);
                    }
                } else {
                    for i in (0..n).rev() {
                        acc.check_word(i, rv);
                        acc.r.wr(i, wv);
                    }
                }
                acc.res.bytes_read += 4 * n as u64;
                acc.res.bytes_written += 4 * n as u64;
            }
            for i in 0..n {
                acc.check_word(i, z);
            }
            acc.res.bytes_written += 4 * n as u64;
            acc.res.bytes_read += 4 * n as u64;
        }
        _ => return None,
    }
    acc.res.seconds = t0.elapsed().as_secs_f64();
    Some(acc.res)
}

/// Canary pattern for carve-out foreign-writer detection.
#[inline]
pub fn canary_word(seed: u64, addr: u64) -> u32 {
    let mut x = addr ^ seed;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    (x ^ (x >> 31)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Faulty {
        inner: PtrRegion,
        stuck_word: usize,
        stuck_bit: u32,
        stuck_one: bool,
    }

    impl Region for Faulty {
        fn words(&self) -> usize {
            self.inner.words()
        }
        fn rd(&self, i: usize) -> u32 {
            let v = self.inner.rd(i);
            if i == self.stuck_word {
                if self.stuck_one {
                    v | (1 << self.stuck_bit)
                } else {
                    v & !(1 << self.stuck_bit)
                }
            } else {
                v
            }
        }
        fn wr(&mut self, i: usize, v: u32) {
            self.inner.wr(i, v)
        }
        fn addr(&self, i: usize) -> u64 {
            self.inner.addr(i)
        }
    }

    #[test]
    fn clean_memory_passes_all_patterns() {
        let mut buf = vec![0u32; 4096];
        let mut r = PtrRegion {
            ptr: buf.as_mut_ptr(),
            words: buf.len(),
            base_addr: 0x1000_0000,
        };
        for p in PATTERNS {
            let res = run_pattern(&mut r, p, 0, &|| false).unwrap();
            assert_eq!(res.errors, 0, "{p}");
            assert!(res.bytes_read > 0 && res.bytes_written > 0);
        }
        assert!(run_pattern(&mut r, "nope", 0, &|| false).is_none());
    }

    #[test]
    fn stuck_bits_are_found() {
        let mut buf = vec![0u32; 4096];
        for p in PATTERNS {
            let mut total = 0;
            for stuck_one in [false, true] {
                let mut r = Faulty {
                    inner: PtrRegion {
                        ptr: buf.as_mut_ptr(),
                        words: buf.len(),
                        base_addr: 0,
                    },
                    stuck_word: 1234,
                    stuck_bit: 17,
                    stuck_one,
                };
                let res = run_pattern(&mut r, p, 1, &|| false).unwrap();
                if res.errors > 0 {
                    assert_eq!(res.error_bits, 1 << 17, "{p}");
                    assert_eq!(res.first[0].0, 1234 * 4, "{p}");
                } else {
                    // Only the single-polarity patterns may miss one polarity.
                    assert!(
                        matches!(*p, "address" | "inverse-address" | "prbs"),
                        "{p} missed stuck-at-{}",
                        stuck_one as u8
                    );
                }
                total += res.errors;
            }
            assert!(total > 0, "{p} missed both polarities");
        }
    }

    #[test]
    fn canary_is_deterministic_and_varies() {
        assert_eq!(canary_word(1, 0x100), canary_word(1, 0x100));
        assert_ne!(canary_word(1, 0x100), canary_word(1, 0x104));
        assert_ne!(canary_word(1, 0x100), canary_word(2, 0x100));
    }
}

//! Shared helpers for the `dmr::fec` unit tests.

/// Deterministic xorshift64 generator (no `rand` dependency).
pub struct XorShift(u64);

impl XorShift {
    pub fn new(seed: u64) -> Self {
        XorShift(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Uniform-ish value in 0..n.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }

    /// Fills `out` with random bits.
    pub fn fill_bits(&mut self, out: &mut [u8]) {
        for b in out.iter_mut() {
            *b = (self.next_u64() >> 33) as u8 & 1;
        }
    }

    /// `k` distinct positions in 0..n.
    pub fn positions(&mut self, n: usize, k: usize) -> Vec<usize> {
        let mut v: Vec<usize> = Vec::with_capacity(k);
        while v.len() < k {
            let p = self.below(n);
            if !v.contains(&p) {
                v.push(p);
            }
        }
        v
    }
}

/// Hex string to bits, MSB first (4 bits per digit).
pub fn hex_to_bits(hex: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(hex.len() * 4);
    for c in hex.chars() {
        let v = c.to_digit(16).expect("hex digit");
        for i in (0..4).rev() {
            out.push(((v >> i) & 1) as u8);
        }
    }
    out
}

/// Bits from a '0'/'1' string (the SDRTrunk `BinaryMessage.load` form).
pub fn str_to_bits(s: &str) -> Vec<u8> {
    s.bytes().map(|c| c - b'0').collect()
}

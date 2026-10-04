//! Pattern models for the ring checker. Each model maps a stream unit
//! (64-bit word or 32-bit IQ sample) to a sequence position.

use crate::util::rev12;
use serde_json::{json, Value};

pub const P64: u128 = 1u128 << 64;

pub trait Model {
    fn name(&self) -> &'static str;
    /// Bytes per unit (8 for 64-bit word patterns, 4 for IQ samples).
    fn unit_bytes(&self) -> usize;
    /// Positions live modulo this period.
    fn period(&self) -> u128;
    /// Sequence position of a unit, or None if the unit is not a valid member.
    fn position(&self, u: u64) -> Option<u64>;
    /// Expected unit value at `pos` given the current continuity state.
    fn value(&self, pos: u64) -> u64;
    /// Fast-path check that `u` is the unit at `pos`; updates state on success.
    fn matches(&mut self, u: u64, pos: u64) -> bool;
    /// Accept `u` at `pos` as the new continuity reference.
    fn resync(&mut self, _u: u64, _pos: u64) {}
    /// The unit at `pos` was corrupt; continue as if value(pos) had arrived.
    fn skip(&mut self, _pos: u64) {}
    /// Continuity lost (declared gap / skipped units).
    fn desync(&mut self) {}
    /// Session identifier carried in the data (tagged pattern).
    fn session(&self, _u: u64) -> Option<u32> {
        None
    }
    /// Look at the first data to choose variants (I/Q orientation).
    fn prime(&mut self, _data: &[u8]) {}
    fn info(&self) -> Value {
        Value::Null
    }
}

// ── ramp64 ────────────────────────────────────────────────────────────

/// Ring v2 mode 1: `data = seq` (64-bit word counter).
pub struct Ramp64;

impl Model for Ramp64 {
    fn name(&self) -> &'static str {
        "ramp64"
    }
    fn unit_bytes(&self) -> usize {
        8
    }
    fn period(&self) -> u128 {
        P64
    }
    fn position(&self, u: u64) -> Option<u64> {
        Some(u)
    }
    fn value(&self, pos: u64) -> u64 {
        pos
    }
    #[inline]
    fn matches(&mut self, u: u64, pos: u64) -> bool {
        u == pos
    }
}

// ── tagged ────────────────────────────────────────────────────────────

pub const SEQ56: u64 = (1u64 << 56) - 1;

/// Ring v2 mode 2: `{tag[3:0], 4'b0000, seq[55:0]}`.
#[derive(Default)]
pub struct Tagged {
    pub tag: Option<u64>,
}

impl Model for Tagged {
    fn name(&self) -> &'static str {
        "tagged"
    }
    fn unit_bytes(&self) -> usize {
        8
    }
    fn period(&self) -> u128 {
        1u128 << 56
    }
    fn position(&self, u: u64) -> Option<u64> {
        if (u >> 56) & 0xF != 0 {
            None
        } else {
            Some(u & SEQ56)
        }
    }
    fn value(&self, pos: u64) -> u64 {
        (self.tag.unwrap_or(0) << 60) | (pos & SEQ56)
    }
    #[inline]
    fn matches(&mut self, u: u64, pos: u64) -> bool {
        u == self.value(pos)
    }
    fn resync(&mut self, u: u64, _pos: u64) {
        self.tag = Some(u >> 60);
    }
    fn session(&self, u: u64) -> Option<u32> {
        if (u >> 56) & 0xF != 0 {
            None
        } else {
            Some((u >> 60) as u32)
        }
    }
    fn info(&self) -> Value {
        json!({"tag": self.tag})
    }
}

// ── prbs31 ────────────────────────────────────────────────────────────

pub const PRBS31_SEED: u32 = 0x7FFF_FFFF;

/// Bit-serial reference (mirrors `hwval_hdl.pattern.prbs31_step32`).
pub fn prbs31_step32_ref(mut state: u32) -> (u32, u32) {
    let mut word = 0u32;
    for _ in 0..32 {
        let bit = ((state >> 30) ^ (state >> 27)) & 1;
        state = ((state << 1) | bit) & 0x7FFF_FFFF;
        word = (word << 1) | bit;
    }
    (word, state)
}

/// Word-parallel next word: `next = step32(prev & 0x7FFFFFFF).word`.
///
/// With P = prev & 0x7FFFFFFF (bit j = bit generated j steps ago), the
/// output bit k (MSB first) is W[k] = P[k-1] ^ P[k-4] for k >= 4, then
/// W[3..1] = P[2..0] ^ W[31..29] and W[0] = W[31] ^ W[28].
#[inline]
pub fn prbs31_next(prev: u32) -> u32 {
    let p = prev & 0x7FFF_FFFF;
    let hi = ((p << 1) ^ (p << 4)) & 0xFFFF_FFF0;
    let lo3 = ((p << 1) & 0xE) ^ ((hi >> 28) & 0xE);
    let b0 = ((hi >> 31) ^ (hi >> 28)) & 1;
    hi | lo3 | b0
}

/// First `n` reference words from the seed (`prbs31_words`).
pub fn prbs31_words(n: usize) -> Vec<u32> {
    let mut st = PRBS31_SEED;
    (0..n)
        .map(|_| {
            let (w, s) = prbs31_step32_ref(st);
            st = s;
            w
        })
        .collect()
}

/// Ring v2 mode 3: `{seq[31:0], prbs[31:0]}` with the PRBS31 word stream of
/// `hwval_hdl/pattern.py` (x^31 + x^28 + 1, MSB first, seed 0x7FFFFFFF). The
/// PRBS half is checked self-synchronously: each word must be
/// `prbs31_next(previous word)`.
#[derive(Default)]
pub struct Prbs31 {
    last: Option<u32>,
    pub low_checked: u64,
    pub low_errors: u64,
}

impl Model for Prbs31 {
    fn name(&self) -> &'static str {
        "prbs31"
    }
    fn unit_bytes(&self) -> usize {
        8
    }
    fn period(&self) -> u128 {
        1u128 << 32
    }
    fn position(&self, u: u64) -> Option<u64> {
        Some(u >> 32)
    }
    fn value(&self, pos: u64) -> u64 {
        ((pos & 0xFFFF_FFFF) << 32) | self.last.map(prbs31_next).unwrap_or(0) as u64
    }
    #[inline]
    fn matches(&mut self, u: u64, pos: u64) -> bool {
        if (u >> 32) != (pos & 0xFFFF_FFFF) {
            return false;
        }
        let lo = u as u32;
        if let Some(l) = self.last {
            self.low_checked += 1;
            if lo != prbs31_next(l) {
                self.low_errors += 1;
                return false;
            }
        }
        self.last = Some(lo);
        true
    }
    fn resync(&mut self, u: u64, _pos: u64) {
        self.last = Some(u as u32);
    }
    fn skip(&mut self, _pos: u64) {
        self.last = self.last.map(prbs31_next);
    }
    fn desync(&mut self) {
        self.last = None;
    }
    fn info(&self) -> Value {
        json!({"low_half_checked": self.low_checked, "reference": "scanner-hdl/hwval_hdl/pattern.py prbs31_words"})
    }
}

// ── iqramp ────────────────────────────────────────────────────────────

/// Legacy ring sample ramp: 32-bit counter c, re = c[15:0], im = c[31:16];
/// each 32-bit sample unit equals c.
pub struct IqRamp;

impl Model for IqRamp {
    fn name(&self) -> &'static str {
        "iqramp"
    }
    fn unit_bytes(&self) -> usize {
        4
    }
    fn period(&self) -> u128 {
        1u128 << 32
    }
    fn position(&self, u: u64) -> Option<u64> {
        Some(u & 0xFFFF_FFFF)
    }
    fn value(&self, pos: u64) -> u64 {
        pos & 0xFFFF_FFFF
    }
    #[inline]
    fn matches(&mut self, u: u64, pos: u64) -> bool {
        u == (pos & 0xFFFF_FFFF)
    }
}

// ── pn0fn (AD9361 BIST PRBS) ──────────────────────────────────────────

pub const PN0_PERIOD: usize = 65535;
pub const IQ12_MASK: u64 = 0x0FFF_0FFF;

#[inline]
fn parity(x: u32) -> u32 {
    x.count_ones() & 1
}

/// `pn0fn` of axi_ad9361_rx_pnmon.v:
/// `{din[14:0], ^din[15:4] ^ ^din[2:1]}`.
#[inline]
pub fn pn0fn(s: u32) -> u32 {
    ((s << 1) & 0xFFFF) | (parity((s >> 4) & 0xFFF) ^ parity((s >> 1) & 0x3))
}

/// The 16-bit monitor word is `{I[11:0], brfn(Q)[3:0]}` with the
/// constraint `I[7:0] == brfn(Q)[11:4]`, i.e. I = S[15:4] and
/// Q = bitreverse12(S[11:0]). Returns the sample as `I | Q << 16` (12 bits each).
#[inline]
pub fn pn0_sample(s: u32) -> u32 {
    let i = (s >> 4) & 0xFFF;
    let q = rev12(s & 0xFFF);
    i | (q << 16)
}

/// Inverse of `pn0_sample` (None if the I/Q halves are inconsistent,
/// exactly the `adc_pn0_iq_match_s` check of the monitor).
#[inline]
pub fn pn0_state(i: u32, q: u32) -> Option<u32> {
    let qr = rev12(q & 0xFFF);
    let i = i & 0xFFF;
    if (i & 0xFF) != (qr >> 4) {
        return None;
    }
    Some((i << 4) | (qr & 0xF))
}

fn sext_pair(masked: u32) -> u64 {
    let i = crate::util::sext12(masked & 0xFFF) as u32 & 0xFFFF;
    let q = crate::util::sext12((masked >> 16) & 0xFFF) as u32 & 0xFFFF;
    (i | (q << 16)) as u64
}

pub struct Pn0 {
    pos_of: Vec<u16>,
    seq: Vec<u32>,
    pub swap: bool,
    pub swap_auto: bool,
}

impl Default for Pn0 {
    fn default() -> Self {
        Self::new()
    }
}

impl Pn0 {
    pub fn new() -> Pn0 {
        let mut pos_of = vec![u16::MAX; 65536];
        let mut seq = vec![0u32; PN0_PERIOD];
        let mut s = 1u32;
        for (pos, slot) in seq.iter_mut().enumerate() {
            pos_of[s as usize] = pos as u16;
            *slot = pn0_sample(s);
            s = pn0fn(s);
        }
        debug_assert_eq!(s, 1, "pn0fn is maximal length");
        Pn0 {
            pos_of,
            seq,
            swap: false,
            swap_auto: false,
        }
    }

    /// Masked sample (I | Q<<16) at a position (for synthesis).
    pub fn sample_at(&self, pos: u64) -> u32 {
        self.seq[(pos % PN0_PERIOD as u64) as usize]
    }

    #[inline]
    fn orient(&self, u: u64) -> u64 {
        if self.swap {
            ((u << 16) | (u >> 16)) & 0xFFFF_FFFF
        } else {
            u
        }
    }

    fn decodable(&self, data: &[u8], swap: bool) -> usize {
        data.chunks_exact(4)
            .take(2048)
            .filter(|c| {
                let mut u = u32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u64;
                if swap {
                    u = ((u << 16) | (u >> 16)) & 0xFFFF_FFFF;
                }
                pn0_state((u & 0xFFF) as u32, ((u >> 16) & 0xFFF) as u32)
                    .map(|s| s != 0)
                    .unwrap_or(false)
            })
            .count()
    }
}

impl Model for Pn0 {
    fn name(&self) -> &'static str {
        "pn0fn"
    }
    fn unit_bytes(&self) -> usize {
        4
    }
    fn period(&self) -> u128 {
        PN0_PERIOD as u128
    }
    fn position(&self, u: u64) -> Option<u64> {
        let u = self.orient(u);
        let s = pn0_state((u & 0xFFF) as u32, ((u >> 16) & 0xFFF) as u32)?;
        let p = self.pos_of[s as usize];
        if p == u16::MAX {
            None
        } else {
            Some(p as u64)
        }
    }
    fn value(&self, pos: u64) -> u64 {
        let v = sext_pair(self.seq[(pos % PN0_PERIOD as u64) as usize]);
        self.orient(v)
    }
    #[inline]
    fn matches(&mut self, u: u64, pos: u64) -> bool {
        (self.orient(u) & IQ12_MASK) == self.seq[pos as usize] as u64
    }
    fn prime(&mut self, data: &[u8]) {
        let a = self.decodable(data, false);
        let b = self.decodable(data, true);
        if b > a {
            self.swap = true;
            self.swap_auto = true;
        }
    }
    fn info(&self) -> Value {
        json!({"iq_swapped": self.swap, "period_samples": PN0_PERIOD,
               "reference": "adi-hdl axi_ad9361_rx_pnmon.v pn0fn (Q_OR_I_N=0)"})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prbs31_matches_python_reference() {
        // python: from hwval_hdl.pattern import prbs31_words; prbs31_words(6)
        let w = prbs31_words(6);
        assert_eq!(w, vec![0xe, 0xfc, 0xe38, 0xfff0, 0xe00e0, 0xfc0fc0]);
        for i in 1..w.len() {
            assert_eq!(prbs31_next(w[i - 1]), w[i], "word {i}");
        }
        // Word-parallel == bit-serial for arbitrary states.
        let mut x = 0x1234_5678u32;
        for _ in 0..10_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let st = x & 0x7FFF_FFFF;
            if st == 0 {
                continue;
            }
            assert_eq!(prbs31_next(st), prbs31_step32_ref(st).0);
            // After a word the state equals word & 0x7FFFFFFF.
            let (wd, ns) = prbs31_step32_ref(st);
            assert_eq!(ns, wd & 0x7FFF_FFFF);
        }
    }

    #[test]
    fn pn0_is_maximal_and_invertible() {
        let m = Pn0::new();
        let mut seen = std::collections::HashSet::new();
        for p in 0..PN0_PERIOD as u64 {
            let s = m.sample_at(p);
            assert!(seen.insert(s));
            let pos = m.position(s as u64).unwrap();
            assert_eq!(pos, p);
        }
        // I/Q mismatch is undecodable.
        let s = m.sample_at(10) ^ 0x0000_0010; // flip I bit 4 (inside I[7:0])
        assert!(m.position(s as u64).is_none());
    }

    #[test]
    fn pn0_monitor_equivalence() {
        // Re-derive with the monitor's own formula: data_s = {I, brfn(Q)[3:0]}
        // must equal pn0fn(previous data_s).
        let m = Pn0::new();
        let mut prev: Option<u32> = None;
        for p in 0..1000u64 {
            let smp = m.sample_at(p);
            let i = smp & 0xFFF;
            let q = (smp >> 16) & 0xFFF;
            let qr = rev12(q);
            assert_eq!(i & 0xFF, qr >> 4, "iq_match");
            let data_s = (i << 4) | (qr & 0xF);
            if let Some(pv) = prev {
                assert_eq!(pn0fn(pv), data_s);
            }
            prev = Some(data_s);
        }
    }
}

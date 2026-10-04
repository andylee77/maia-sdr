//! 8-VSB framing (A/53 Part 2): segments of 832 symbols (a 4-symbol segment sync, then 828 data
//! symbols), fields of 313 segments (a field sync segment, then 312 data segments), the eight
//! levels ±1, ±3, ±5, ±7 with the pilot's +1.25 on every symbol, and the field sync's known
//! symbols.

use std::sync::OnceLock;

pub const SEGMENT: usize = 832;
pub const SYNC_SYMBOLS: usize = 4;
pub const DATA_SYMBOLS: usize = SEGMENT - SYNC_SYMBOLS;
pub const FIELD_SEGMENTS: usize = 313;
pub const DATA_SEGMENTS: usize = FIELD_SEGMENTS - 1;
/// The segment sync, in the symbols' units.
pub const SEGMENT_SYNC: [f32; 4] = [5.0, -5.0, -5.0, 5.0];
/// The pilot: a constant added to every symbol.
pub const PILOT: f32 = 1.25;
/// Symbols at the start of a field sync segment whose values are known: the segment sync,
/// PN511, three PN63 and the 24 symbols of the VSB mode.
pub const FIELD_SYNC_KNOWN: usize = 4 + 511 + 3 * 63 + 24;

/// The symbol a 3-bit value (Z2 Z1 Z0) maps to, without the pilot.
pub fn level(v: u8) -> f32 {
    2.0 * f32::from(v) - 7.0
}

/// A sequence of `n` bits from the shift register `state`, output from bit `out`, the parity of
/// the bits in `mask` shifted in at the bottom (A/53's PN generators).
fn lfsr(mut state: u32, out: u32, mask: u32, n: usize) -> Vec<f32> {
    (0..n)
        .map(|_| {
            let bit = (state >> out) & 1;
            state = (state << 1) | ((state & mask).count_ones() & 1);
            if bit == 1 { 5.0 } else { -5.0 }
        })
        .collect()
}

pub fn pn511() -> &'static [f32] {
    static PN: OnceLock<Vec<f32>> = OnceLock::new();
    PN.get_or_init(|| lfsr(0b10, 8, 0b1_1011_0110, 511))
}

pub fn pn63() -> &'static [f32] {
    static PN: OnceLock<Vec<f32>> = OnceLock::new();
    PN.get_or_init(|| lfsr(0b11_1001, 5, 0b11_0000, 63))
}

const VSB_MODE: [u8; 24] = [0, 0, 0, 0, 1, 0, 1, 0, 0, 1, 0, 1, 1, 1, 1, 1, 0, 1, 0, 1, 1, 0, 1, 0];

/// The known symbols of a field sync segment; `inverted`: its middle PN63 inverted, as on every
/// other field.
pub fn field_sync(inverted: bool) -> Vec<f32> {
    let mut out = SEGMENT_SYNC.to_vec();
    out.extend_from_slice(pn511());
    out.extend_from_slice(pn63());
    out.extend(pn63().iter().map(|&s| if inverted { -s } else { s }));
    out.extend_from_slice(pn63());
    out.extend(VSB_MODE.iter().map(|&b| if b == 1 { 5.0 } else { -5.0 }));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_known_sequences_are_balanced_and_long_enough() {
        // A maximal sequence of length 2^n - 1 has one more 1 than 0s.
        assert_eq!(pn511().iter().filter(|&&s| s > 0.0).count(), 256);
        assert_eq!(pn63().iter().filter(|&&s| s > 0.0).count(), 32);
        assert_eq!(field_sync(false).len(), FIELD_SYNC_KNOWN);
        let (a, b) = (field_sync(false), field_sync(true));
        assert_eq!(a.iter().zip(&b).filter(|(x, y)| x != y).count(), 63, "only the middle PN63 differs");
        assert_eq!((level(0), level(7)), (-7.0, 7.0));
    }
}

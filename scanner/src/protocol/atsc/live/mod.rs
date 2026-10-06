//! The PS's half of the 8-VSB receiver whose other half is the radio core's 8-VSB block
//! (maia-sdr doc/changes/085): what the block is given (the matched filter's table, the nominal
//! symbol step, the equalizer's taps) and what the PS makes of its packets (`packets`): each
//! codeword corrected where its flag says so and derandomized into a transport stream packet, and
//! each snapshot solved for taps (`taps`). The fixed-point model (`pl`) uses this same code, so
//! the PS cannot drift from the model the gateware matches bit for bit.

pub mod packets;
pub mod taps;

use super::demod::{rrc, SYMBOL_RATE};
use super::fec::{self, randomizer, rs};

/// The gateware's widths (the model's `Widths::CHOSEN`): the matched filter's taps, the
/// equalizer's taps, and y (a level is 2^(Y_BITS - 5)).
pub const MF_TAP_BITS: u32 = 12;
pub const EQ_TAP_BITS: u32 = 16;
pub const Y_BITS: u32 = 12;
/// Offsets between input samples the matched filter is tabulated at.
pub const MF_PHASE_BITS: u32 = 6;
/// The matched filter's half-span, in symbols of RS/2 (081's).
const MF_HALF_SPAN: f64 = 10.0;
/// The block's matched filter: five DSPs of eight taps (40 taps at 10 MSPS).
pub const MF_DSPS: usize = 5;
pub const MF_TAPS_PER_DSP: usize = 8;

/// The matched filter at `fs` (the true sample rate), `tap_bits` a tap (1.0 at 2^(bits - 1)):
/// its half-span in input samples, its taps, and the table, 64 rows of `taps` (row p for an
/// instant p/64 of a sample after input i weighs inputs i + 1 - half ..= i + half).
pub fn matched_filter(fs: f64, tap_bits: u32) -> (usize, usize, Vec<i32>) {
    let rb = SYMBOL_RATE / 2.0;
    let half = ((MF_HALF_SPAN / rb * fs).ceil() as usize + 1).div_ceil(4) * 4;
    let taps = 2 * half;
    let one = f64::from(1u32 << (tap_bits - 1));
    let rows = 1usize << MF_PHASE_BITS;
    let table = (0..rows)
        .flat_map(|p| {
            let f = p as f64 / rows as f64;
            (0..taps).map(move |j| (rrc((f + half as f64 - 1.0 - j as f64) / fs * rb) * rb / fs * one).round() as i32)
        })
        .collect();
    (half, taps, table)
}

/// The table as the block holds it: word 8p + c holds DSP d's tap 8d + c of row p in its 12-bit
/// lane d. `None` unless the table has the block's 40 taps.
pub fn table_words(taps: usize, table: &[i32]) -> Option<Vec<[u16; MF_DSPS]>> {
    if taps != MF_DSPS * MF_TAPS_PER_DSP {
        return None;
    }
    let rows = table.len() / taps;
    Some(
        (0..rows * MF_TAPS_PER_DSP)
            .map(|w| {
                let (p, c) = (w / MF_TAPS_PER_DSP, w % MF_TAPS_PER_DSP);
                std::array::from_fn(|d| (table[p * taps + MF_TAPS_PER_DSP * d + c] as u16) & 0xFFF)
            })
            .collect(),
    )
}

/// The nominal symbol step at `fs`, input samples × 2^32.
pub fn step_word(fs: f64) -> u32 {
    (fs / SYMBOL_RATE * 4_294_967_296.0).round() as u32
}

/// The carrier loop's starting frequency for a channel whose centre is `centre_offset_hz` from
/// the LO (θ a symbol beyond the quarter turn, 2^32 a turn). The loop pulls in a few hundred Hz
/// while the front end settles; a station whose pilot sits further off (RF 34's, −885 Hz) needs
/// its offset here, or the segment search runs before the loop has locked and never finds a field.
pub fn carrier_word(centre_offset_hz: f64) -> i32 {
    (-centre_offset_hz / SYMBOL_RATE * 4_294_967_296.0).round() as i32
}

/// A codeword as a transport stream packet: corrected by Reed-Solomon when its flag says a
/// syndrome is not zero, derandomized for its place in the field (0-311), the sync byte first.
pub fn transport(place: u16, bytes: &[u8; rs::N], flag: bool) -> fec::Packet {
    let mut cw = *bytes;
    let outcome = if flag { rs::decode(&mut cw) } else { rs::Outcome::Clean };
    let mut b = [0u8; 188];
    b[0] = 0x47;
    let r = &randomizer()[usize::from(place) * rs::K..];
    for (j, v) in b[1..].iter_mut().enumerate() {
        *v = cw[j] ^ r[j];
    }
    fec::Packet { bytes: b, outcome }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_fills_the_block_s_words() {
        let (half, taps, table) = matched_filter(10e6, MF_TAP_BITS);
        assert_eq!((half, taps), (20, 40));
        let words = table_words(taps, &table).unwrap();
        assert_eq!(words.len(), 512);
        // Row 3, DSP 2's step 5 is tap 21.
        assert_eq!(words[3 * 8 + 5][2], (table[3 * 40 + 21] as u16) & 0xFFF);
        assert!(table_words(56, &matched_filter(12.8e6, MF_TAP_BITS).2).is_none());
    }

    #[test]
    fn the_step_is_the_sample_rate_against_the_symbol_rate() {
        assert_eq!(step_word(10e6), 3_990_775_330);
        // The model's step on unit A's captures (its vectors' `step0`).
        assert_eq!(step_word(10e6 * (1.0 - 0.688e-6)), 3_990_772_585);
    }

    #[test]
    fn the_carrier_starts_against_the_offset() {
        assert_eq!(carrier_word(0.0), 0);
        // A pilot 885 Hz low: the loop turns on by 885 Hz a symbol's worth.
        assert_eq!(carrier_word(-885.0), (885.0 / SYMBOL_RATE * 4_294_967_296.0_f64).round() as i32);
        assert!(carrier_word(40e3) < 0);
    }
}

//! Oregon Scientific version 1: the older temperature sensors of that make, on 433.92 MHz.
//!
//! The protocol's facts: on-off keying, Manchester coded at 342 bits a second, a bit whose first
//! half is on being a one. A preamble of ones, then a sync: a long gap, a pulse of about 5.7 ms
//! and another long gap, the data beginning 5.2 ms after the pulse. 32 bits, each byte's least
//! significant bit first: the first byte has the sensor's id in its low four bits and its channel
//! in its top two; the temperature follows as three decimal digits (tenths, units, tens), then
//! a flag for under zero and one for a low battery; the fourth byte is the sum of the first
//! three with the carry added back. The sensor sends the message twice in a burst.
//!
//! Written from those facts and read from unit A's captures of one (doc/changes/110). It has
//! not been held against rtl_433.

use super::pulses::runs;
use super::Reading;

pub const MODEL: &str = "Oregon Scientific v1";
const BIT_US: f32 = 1e6 / 342.0;
/// The sync's pulse, between a preamble's (1.5 and 3 ms) and nothing longer.
const SYNC_US: std::ops::RangeInclusive<f32> = 5000.0..=6500.0;
/// The data begins this long after the sync's pulse, give or take the slice's own lean and the
/// sensor's clock.
const AFTER_SYNC_US: f32 = 5200.0;
const LEAN_US: std::ops::RangeInclusive<i32> = -800..=1600;
const LEAN_STEP_US: usize = 50;
const BITS: usize = 32;

/// The message a burst's envelope holds (`on`, `sample_us` a sample), and how many times.
pub fn read(on: &[bool], sample_us: f32) -> Option<Reading> {
    let mut found: Vec<[u8; 4]> = Vec::new();
    for (state, start, length) in runs(on) {
        if state && SYNC_US.contains(&(length as f32 * sample_us)) {
            found.extend(message(on, sample_us, (start + length) as f32 * sample_us));
        }
    }
    let first = *found.first()?;
    let repeats = found.iter().filter(|m| **m == first).count() as u8;
    let digit = |nibble: u8| f32::from(nibble);
    let temperature = digit(first[2] & 0xF) * 10.0 + digit(first[1] >> 4) + digit(first[1] & 0xF) / 10.0;
    Some(Reading {
        model: MODEL.to_string(),
        kind: "weather".to_string(),
        id: u32::from(first[0] & 0xF),
        channel: Some((first[0] >> 6) + 1),
        temperature_c: Some(if first[2] & 0x20 != 0 { -temperature } else { temperature }),
        humidity_pct: None,
        battery_low: Some(first[2] & 0x80 != 0),
        repeats,
    })
}

/// The 32 bits after a sync's pulse that ends at `sync_end_us`, when they are Manchester coded
/// all through, sum as their fourth byte says and hold a decimal temperature. The cells' start
/// is tried either side of where it should be: the best-formed reading wins.
fn message(on: &[bool], sample_us: f32, sync_end_us: f32) -> Option<[u8; 4]> {
    let mut best: Option<(f32, [u8; 4])> = None;
    for lean in LEAN_US.step_by(LEAN_STEP_US) {
        let begin = sync_end_us + AFTER_SYNC_US + lean as f32;
        let Some((bytes, margin)) = bits(on, sample_us, begin) else { continue };
        let sum = u16::from(bytes[0]) + u16::from(bytes[1]) + u16::from(bytes[2]);
        let good = ((sum & 0xFF) + (sum >> 8)) as u8 == bytes[3];
        let decimal = bytes[1] & 0xF <= 9 && bytes[1] >> 4 <= 9 && bytes[2] & 0xF <= 9;
        if good && decimal && best.is_none_or(|b| margin > b.0) {
            best = Some((margin, bytes));
        }
    }
    best.map(|b| b.1)
}

/// The bits of the cells from `begin_us` on, and how clearly their halves differ; none when a
/// cell's halves are the same (it is not Manchester coded) or the envelope ends first.
fn bits(on: &[bool], sample_us: f32, begin_us: f32) -> Option<([u8; 4], f32)> {
    // The share of a part of a cell that is on.
    let level = |cell_us: f32, from: f32, to: f32| {
        let (a, b) = (((cell_us + from * BIT_US) / sample_us) as usize, ((cell_us + to * BIT_US) / sample_us) as usize);
        (a < b && b <= on.len()).then(|| on[a..b].iter().filter(|&&v| v).count() as f32 / (b - a) as f32)
    };
    let mut bytes = [0u8; 4];
    let mut margin = 0.0;
    for k in 0..BITS {
        let cell = begin_us + k as f32 * BIT_US;
        let (first, second) = (level(cell, 0.12, 0.38)?, level(cell, 0.62, 0.88)?);
        if (first > 0.5) == (second > 0.5) {
            return None;
        }
        margin += (first - second).abs();
        if first > 0.5 {
            bytes[k / 8] |= 1 << (k % 8);
        }
    }
    Some((bytes, margin))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::pulses::{self, tests::keyed_burst};
    use super::*;

    /// A message's keying as the sensor sends it once: µs on, µs off, ... from the preamble's
    /// first pulse to the last bit.
    pub fn keying(bytes: [u8; 4]) -> Vec<u32> {
        let half = (BIT_US / 2.0) as u32;
        // Each half bit, on or off: twelve ones, the sync, the 32 bits.
        let mut halves: Vec<(bool, u32)> = Vec::new();
        for _ in 0..12 {
            halves.extend([(true, half), (false, half)]);
        }
        halves.extend([(false, 4200), (true, 5700), (false, 5200)]);
        for k in 0..32 {
            let one = bytes[k / 8] >> (k % 8) & 1 == 1;
            halves.extend([(one, half), (!one, half)]);
        }
        // Joined into pulses and gaps, a pulse first.
        let mut out: Vec<u32> = Vec::new();
        let mut state = true;
        for (on, us) in halves {
            if on == state && !out.is_empty() {
                *out.last_mut().unwrap() += us;
            } else if on == state {
                out.push(us);
            } else {
                out.push(us);
                state = on;
            }
        }
        out
    }

    /// The message with its checksum, for a temperature in tenths of a degree.
    pub fn message_of(id: u8, channel: u8, tenths: i32, battery_low: bool) -> [u8; 4] {
        let t = tenths.unsigned_abs();
        let b0 = (id & 0xF) | (channel - 1) << 6;
        let b1 = (t % 10) as u8 | ((t / 10 % 10) as u8) << 4;
        let b2 = (t / 100 % 10) as u8 | if tenths < 0 { 0x20 } else { 0 } | if battery_low { 0x80 } else { 0 };
        let sum = u16::from(b0) + u16::from(b1) + u16::from(b2);
        [b0, b1, b2, ((sum & 0xFF) + (sum >> 8)) as u8]
    }

    fn read_keyed(keyed: &[u32], amplitude: f32) -> Option<Reading> {
        let iq = keyed_burst(-40_000.0, amplitude, 20_000, keyed);
        read(&pulses::tests::sliced(&iq, -40_000.0), pulses::SAMPLE_US)
    }

    #[test]
    fn a_message_reads_as_its_sensor_channel_temperature_and_battery() {
        // What unit A's sensor sent on 2026-11-12: 03 59 01 5d.
        assert_eq!(message_of(3, 1, 159, false), [0x03, 0x59, 0x01, 0x5d]);
        let once = keying([0x03, 0x59, 0x01, 0x5d]);
        let r = read_keyed(&once, 100.0).expect("a message");
        assert_eq!((r.model.as_str(), r.kind.as_str(), r.id, r.channel, r.battery_low, r.repeats), (MODEL, "weather", 3, Some(1), Some(false), 1));
        assert!((r.temperature_c.unwrap() - 15.9).abs() < 1e-4);
        // Twice in a burst, 105 ms apart, as the sensor sends it.
        let mut twice = once.clone();
        twice.push(105_000);
        twice.extend(&once);
        assert_eq!(read_keyed(&twice, 100.0).unwrap().repeats, 2);
        // Under zero, another channel, a low battery; and weak, 14 dB over the noise.
        let r = read_keyed(&keying(message_of(9, 3, -127, true)), 50.0).expect("a weak message");
        assert_eq!((r.id, r.channel, r.battery_low), (9, Some(3), Some(true)));
        assert!((r.temperature_c.unwrap() + 12.7).abs() < 1e-4);
    }

    #[test]
    fn what_is_not_a_whole_message_reads_as_nothing() {
        // A wrong checksum.
        assert!(read_keyed(&keying([0x03, 0x59, 0x01, 0x5c]), 100.0).is_none());
        // A temperature that is not decimal, its checksum good.
        assert!(read_keyed(&keying([0x03, 0x5b, 0x01, 0x5f]), 100.0).is_none());
        // Cut short after 20 bits.
        let whole = keying([0x03, 0x59, 0x01, 0x5d]);
        assert!(read_keyed(&whole[..whole.len() - 14], 100.0).is_none());
        // Another device: half-millisecond pulses with gaps of 2 and 4 ms, and one of 9 ms.
        let other: Vec<u32> = (0..60).flat_map(|k| [500, if k % 13 == 12 { 9000 } else if k % 3 == 0 { 4000 } else { 2000 }]).collect();
        assert!(read_keyed(&other, 100.0).is_none());
        // Noise alone.
        assert!(read_keyed(&[], 0.0).is_none());
        // A long pulse and silence: a sync with nothing after it.
        assert!(read_keyed(&[5700, 60_000, 400], 100.0).is_none());
    }
}

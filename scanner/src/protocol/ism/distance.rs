//! Devices keyed by pulse distance: pulses all alike, the gap after each a bit (a short gap a
//! zero, a long one a one), and a longer gap between the repeats of the message. Many of the
//! band's sensors differ only in such facts, so each is a row of `DEVICES`: its keying, how many
//! bits, what guards its message, and where its fields are. Another such device is another row,
//! and a capture of it to test with.
//!
//! The two rows' facts:
//!
//! - **AcuRite 606TX,** a temperature sensor: 32 bits six times in a burst, about every 31 s.
//!   The first byte is the sensor's id; then a bit that is set while the battery is good, three
//!   bits not read here, and the temperature as 12 signed bits, tenths of a degree C; the fourth
//!   byte is a digest of the first three.
//! - **A Prologue sensor,** temperature and humidity (the make's protocol, sold under several
//!   names): 36 bits and a spare one, six or seven times. Four bits that are always 1001, eight
//!   of id, a bit set while the battery is good, one set when its button sent the message, the
//!   channel in two, the temperature as the 606TX has it, and the humidity in eight. It has no
//!   check of its own: what guards it is its first four bits and the message's repeats.
//!
//! Written from those facts and read from unit A's captures of the two (doc/changes/111).
//! Neither has been held against rtl_433.

use std::ops::RangeInclusive;

use super::pulses::{joined, runs};
use super::Reading;

/// These devices' pulses are half a millisecond: a run shorter than 160 µs is a glitch.
const GLITCH_US: f32 = 160.0;

/// A keying's times in µs: its pulse, a zero's gap and a one's, and the least gap between the
/// repeats of a message.
struct Keying {
    pulse: RangeInclusive<f32>,
    zero: RangeInclusive<f32>,
    one: RangeInclusive<f32>,
    repeat: f32,
}

/// Pulses of half a millisecond, gaps of 2 ms and 4 ms, 9 ms between repeats.
const HALF_2_4_9: Keying = Keying { pulse: 250.0..=900.0, zero: 1400.0..=2800.0, one: 3200.0..=5000.0, repeat: 7000.0 };

/// Some of a message's bits: the first, counted from the first sent, and how many.
#[derive(Clone, Copy)]
struct Bits(usize, usize);

/// What tells a device's message from another's bits.
enum Guard {
    /// The last byte is a digest of those before it: each bit that is set adds in a key, which
    /// is shifted right at every bit, the generator put in when a one falls out.
    Digest { generator: u8, key: u8 },
    /// These bits are always this.
    Leads(Bits, u64),
}

struct Device {
    model: &'static str,
    keying: &'static Keying,
    bits: usize,
    /// Bits a word may have after the message's own.
    spare: usize,
    /// How many times a burst must hold the same message.
    needs: u8,
    guard: Guard,
    id: Bits,
    channel: Option<Bits>,
    /// The bit that is set while the battery is good.
    battery_good: usize,
    /// Signed, in tenths of a degree C.
    temperature: Bits,
    humidity: Option<Bits>,
}

const DEVICES: [Device; 2] = [
    Device {
        model: "AcuRite 606TX",
        keying: &HALF_2_4_9,
        bits: 32,
        spare: 0,
        needs: 2,
        guard: Guard::Digest { generator: 0x98, key: 0xF1 },
        id: Bits(0, 8),
        channel: None,
        battery_good: 8,
        temperature: Bits(12, 12),
        humidity: None,
    },
    Device {
        model: "Prologue TH",
        keying: &HALF_2_4_9,
        bits: 36,
        spare: 1,
        needs: 3,
        guard: Guard::Leads(Bits(0, 4), 0b1001),
        id: Bits(4, 8),
        channel: Some(Bits(14, 2)),
        battery_good: 12,
        temperature: Bits(16, 12),
        humidity: Some(Bits(28, 8)),
    },
];

/// What a device of the table says in a burst's envelope (`on`, `sample_us` a sample): the
/// message the burst holds most often, when it holds it as often as the device's row asks.
pub fn read(on: &[bool], sample_us: f32) -> Option<Reading> {
    DEVICES.iter().find_map(|device| said(device, &words(on, sample_us, device.keying)))
}

/// The words of an envelope under a keying: the gaps after its pulses as bits, a word ending at
/// a gap as long as the one between repeats. Glitches are left out first; a pulse or a gap that
/// is then none of the keying's drops what was read of its word.
fn words(on: &[bool], sample_us: f32, keying: &Keying) -> Vec<Vec<bool>> {
    let mut words = Vec::new();
    let mut word: Vec<bool> = Vec::new();
    let mut after_pulse = false;
    for (state, _, length) in joined(&runs(on), (GLITCH_US / sample_us) as usize) {
        let us = length as f32 * sample_us;
        if state {
            after_pulse = keying.pulse.contains(&us);
            if !after_pulse {
                word.clear();
            }
        } else if !after_pulse {
            word.clear();
        } else if keying.zero.contains(&us) {
            word.push(false);
        } else if keying.one.contains(&us) {
            word.push(true);
        } else {
            if us >= keying.repeat && !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
            word.clear();
        }
    }
    words
}

/// The reading of the message most of `words` are, by a device's row; none when no message of
/// its is there as often as the row asks.
fn said(device: &Device, words: &[Vec<bool>]) -> Option<Reading> {
    let mut seen: Vec<(u64, u8)> = Vec::new();
    for word in words.iter().filter(|w| (device.bits..=device.bits + device.spare).contains(&w.len())) {
        let message = word[..device.bits].iter().fold(0u64, |m, &bit| m << 1 | u64::from(bit));
        if !guarded(device, message) {
            continue;
        }
        match seen.iter_mut().find(|s| s.0 == message) {
            Some(s) => s.1 = s.1.saturating_add(1),
            None => seen.push((message, 1)),
        }
    }
    // The first of those seen most often.
    let (message, repeats) = seen.iter().fold(None, |best: Option<(u64, u8)>, &s| if best.is_some_and(|b| b.1 >= s.1) { best } else { Some(s) })?;
    if repeats < device.needs {
        return None;
    }
    let part = |at: Bits| field(message, device.bits, at);
    let raw = part(device.temperature) as i32;
    let width = device.temperature.1;
    let tenths = if raw >> (width - 1) == 1 { raw - (1 << width) } else { raw };
    Some(Reading {
        model: device.model.to_string(),
        kind: "weather".to_string(),
        id: part(device.id) as u32,
        channel: device.channel.map(|at| part(at) as u8 + 1),
        temperature_c: Some(tenths as f32 / 10.0),
        humidity_pct: device.humidity.map(|at| part(at) as u8).filter(|h| *h <= 100),
        battery_low: Some(part(Bits(device.battery_good, 1)) == 0),
        repeats,
    })
}

/// The bits `at` of a message of `bits` bits, the first sent its highest.
fn field(message: u64, bits: usize, at: Bits) -> u64 {
    message >> (bits - at.0 - at.1) & ((1 << at.1) - 1)
}

fn guarded(device: &Device, message: u64) -> bool {
    match device.guard {
        Guard::Leads(at, value) => field(message, device.bits, at) == value,
        Guard::Digest { generator, key } => {
            let bytes = device.bits / 8;
            let byte = |n: usize| field(message, device.bits, Bits(8 * n, 8)) as u8;
            digest((0..bytes - 1).map(byte), generator, key) == byte(bytes - 1)
        }
    }
}

/// Each set bit of the bytes adds in the key, which is shifted right at every bit, the generator
/// put in when a one falls out.
fn digest(bytes: impl Iterator<Item = u8>, generator: u8, mut key: u8) -> u8 {
    let mut sum = 0;
    for byte in bytes {
        for bit in (0..8).rev() {
            if byte >> bit & 1 == 1 {
                sum ^= key;
            }
            key = if key & 1 == 1 { key >> 1 ^ generator } else { key >> 1 };
        }
    }
    sum
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::pulses::{self, tests::keyed_burst};
    use super::*;

    /// A burst's keying, µs on, µs off, ...: a message of `bits` bits sent `times` times as these
    /// devices send it (a pulse and a short gap, a pulse and the repeats' gap, then each time
    /// the bits, `spare` ones after them, a pulse and the repeats' gap).
    pub fn keying(message: u64, bits: usize, spare: usize, times: usize) -> Vec<u32> {
        let mut out = vec![530, 2030, 530, 9070];
        for _ in 0..times {
            for n in (0..bits).rev() {
                out.extend([530, if message >> n & 1 == 1 { 4070 } else { 2030 }]);
            }
            for _ in 0..spare {
                out.extend([530, 4070]);
            }
            out.extend([530, 9070]);
        }
        out
    }

    /// An AcuRite 606TX's message with its digest.
    pub fn acurite(id: u8, tenths: i16, battery_good: bool) -> u64 {
        let three = [id, u8::from(battery_good) << 7 | (tenths >> 8 & 0xF) as u8, tenths as u8];
        let last = digest(three.into_iter(), 0x98, 0xF1);
        u64::from(u32::from_be_bytes([three[0], three[1], three[2], last]))
    }

    /// A Prologue sensor's message.
    pub fn prologue(id: u8, channel: u8, tenths: i16, humidity: u8, battery_good: bool) -> u64 {
        0b1001 << 32 | u64::from(id) << 24 | u64::from(battery_good) << 23 | u64::from(channel - 1) << 20 | u64::from(tenths as u16 & 0xFFF) << 8 | u64::from(humidity)
    }

    fn read_keyed(keyed: &[u32], amplitude: f32) -> Option<Reading> {
        let iq = keyed_burst(10_000.0, amplitude, 20_000, keyed);
        read(&pulses::tests::sliced(&iq, 10_000.0), pulses::SAMPLE_US)
    }

    #[test]
    fn the_digest_is_the_one_unit_a_s_sensor_sent() {
        // What it sent on 2026-11-12: b5 80 d4 be, and a tenth of a degree lower b5 80 d3 10.
        assert_eq!(digest([0xB5, 0x80, 0xD4].into_iter(), 0x98, 0xF1), 0xBE);
        assert_eq!(digest([0xB5, 0x80, 0xD3].into_iter(), 0x98, 0xF1), 0x10);
        assert_eq!(acurite(181, 212, true), 0xB580_D4BE);
    }

    #[test]
    fn an_acurite_606tx_reads_as_its_id_temperature_and_battery() {
        let r = read_keyed(&keying(0xB580_D4BE, 32, 0, 6), 100.0).expect("a message");
        assert_eq!((r.model.as_str(), r.kind.as_str(), r.id, r.channel, r.humidity_pct, r.battery_low, r.repeats), ("AcuRite 606TX", "weather", 181, None, None, Some(false), 6));
        assert!((r.temperature_c.unwrap() - 21.2).abs() < 1e-4);
        assert_eq!(r.device_key(), "acurite_606tx.181");
        // Under zero, a low battery, another id.
        let r = read_keyed(&keying(acurite(7, -127, false), 32, 0, 6), 100.0).expect("a message");
        assert_eq!((r.id, r.battery_low), (7, Some(true)));
        assert!((r.temperature_c.unwrap() + 12.7).abs() < 1e-4);
        // Once is not enough, whatever its digest; nor is a digest that is wrong, six times.
        assert!(read_keyed(&keying(0xB580_D4BE, 32, 0, 1), 100.0).is_none());
        assert!(read_keyed(&keying(0xB580_D4BF, 32, 0, 6), 100.0).is_none());
    }

    #[test]
    fn a_prologue_sensor_reads_as_its_id_channel_temperature_and_humidity() {
        // What unit A heard on 2026-11-12: 9 56 1 0ab 4b, a spare bit after it.
        assert_eq!(prologue(0x56, 2, 171, 75, false), 0x9_5610_AB4B);
        let r = read_keyed(&keying(0x9_5610_AB4B, 36, 1, 6), 100.0).expect("a message");
        assert_eq!((r.model.as_str(), r.id, r.channel, r.humidity_pct, r.battery_low, r.repeats), ("Prologue TH", 86, Some(2), Some(75), Some(true), 6));
        assert!((r.temperature_c.unwrap() - 17.1).abs() < 1e-4);
        assert_eq!(r.device_key(), "prologue_th.86.2");
        // Without the spare bit, under zero, a good battery; a humidity over 100 is none.
        let r = read_keyed(&keying(prologue(200, 3, -55, 0xCC, true), 36, 0, 7), 100.0).expect("a message");
        assert_eq!((r.id, r.channel, r.humidity_pct, r.battery_low, r.repeats), (200, Some(3), None, Some(false), 7));
        assert!((r.temperature_c.unwrap() + 5.5).abs() < 1e-4);
        // Twice is not enough; nor are other first bits, six times.
        assert!(read_keyed(&keying(0x9_5610_AB4B, 36, 1, 2), 100.0).is_none());
        assert!(read_keyed(&keying(0x8_5610_AB4B, 36, 1, 6), 100.0).is_none());
    }

    #[test]
    fn what_is_no_device_s_message_reads_as_nothing() {
        // Words of another length, a keying of other times, noise alone.
        assert!(read_keyed(&keying(0xB580_D4, 24, 0, 6), 100.0).is_none());
        let slow: Vec<u32> = keying(0xB580_D4BE, 32, 0, 6).iter().map(|us| us * 3).collect();
        assert!(read_keyed(&slow, 100.0).is_none());
        assert!(read_keyed(&[], 0.0).is_none());
        // A repeat that noise broke is left out, the others counted.
        let mut broken = keying(0xB580_D4BE, 32, 0, 3);
        broken[4 + 20] = 3000;
        assert_eq!(read_keyed(&broken, 100.0).map(|r| r.repeats), Some(2));
    }
}

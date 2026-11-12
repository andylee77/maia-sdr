//! What a message says. An extended squitter (formats 17 and 18) has the aircraft's address in
//! bits 9 to 32 and its message in bits 33 to 88, whose first five bits are its type: 1 to 4 a
//! callsign and the aircraft's category; 9 to 18 an altitude by pressure and half a position;
//! 20 to 22 half a position with a height by satellite; 19 a velocity. An all-call reply (format
//! 11) gives the address alone.
//!
//! Read here: the callsign and category; the altitude where it is sent in steps of 25 ft (the
//! older 100 ft code is not); the halves of an airborne position; the velocity over the ground
//! with the rate of climb. Not read: positions on the ground (they need to know where the
//! receiver is), the height by satellite, airspeed and heading, the status and intent types.

use super::mode_s::Frame;

/// A callsign's characters by their six bits.
const CHARS: &[u8; 64] = b"?ABCDEFGHIJKLMNOPQRSTUVWXYZ????? ???????????????0123456789??????";

#[derive(Debug, Clone, PartialEq)]
pub struct Squitter {
    /// The aircraft's 24-bit address.
    pub icao: u32,
    pub says: Says,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Says {
    /// Nothing read: an all-call reply (type 0 here), or a squitter of this type.
    Other(u8),
    /// Its callsign, and its category (a letter for the set and a number, as `A3`) when it
    /// gives one.
    Identity { callsign: String, category: Option<String> },
    /// Half an airborne position, with the altitude by pressure when the message has it.
    Position { altitude_ft: Option<i32>, half: Half },
    /// Its speed over the ground (knots), its track (degrees from north) and its rate of climb
    /// (feet a minute, down negative).
    Velocity { speed_kt: f32, track_deg: f32, vertical_fpm: Option<i32> },
}

/// Half a position: 17 bits of latitude and of longitude on the even or the odd grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Half {
    pub odd: bool,
    pub lat: u32,
    pub lon: u32,
}

/// What a message whose parity was right says.
pub fn read(frame: &Frame) -> Squitter {
    let b = &frame.bytes;
    let icao = u32::from(b[1]) << 16 | u32::from(b[2]) << 8 | u32::from(b[3]);
    if !frame.long {
        return Squitter { icao, says: Says::Other(0) };
    }
    // The message's 56 bits, its first the most significant.
    let me = b[4..11].iter().fold(0u64, |me, &byte| me << 8 | u64::from(byte));
    let kind = (me >> 51) as u8;
    let says = match kind {
        1..=4 => identity(me, kind),
        9..=18 | 20..=22 => Says::Position {
            altitude_ft: if kind <= 18 { altitude((me >> 36) as u32 & 0xFFF) } else { None },
            half: Half { odd: me >> 34 & 1 == 1, lat: (me >> 17) as u32 & 0x1_FFFF, lon: me as u32 & 0x1_FFFF },
        },
        19 => velocity(me).unwrap_or(Says::Other(kind)),
        _ => Says::Other(kind),
    };
    Squitter { icao, says }
}

/// Eight characters of six bits, and the category: the type says which set (4 is A), three
/// bits which of it (0: none given).
fn identity(me: u64, kind: u8) -> Says {
    let text: String = (0..8).map(|k| char::from(CHARS[(me >> (42 - 6 * k)) as usize & 0x3F])).collect();
    let which = (me >> 48) as u8 & 7;
    let category = (which != 0).then(|| format!("{}{which}", char::from(b'A' + (4 - kind))));
    Says::Identity { callsign: text.trim().to_string(), category }
}

/// The altitude by pressure of a 12-bit field whose eighth bit says the steps are 25 ft: the
/// other eleven count them from 1,000 ft under zero.
fn altitude(field: u32) -> Option<i32> {
    if field & 0x10 == 0 {
        return None;
    }
    let steps = (field & 0xFE0) >> 1 | (field & 0xF);
    Some(steps as i32 * 25 - 1000)
}

/// The velocity over the ground (the type's first two kinds; the second is in steps of 4
/// knots): east-west and north-south each a direction bit and ten bits one more than the
/// knots, none when they are zero; the rate of climb a sign and nine bits one more than its
/// steps of 64 ft a minute.
fn velocity(me: u64) -> Option<Says> {
    let step = match me >> 48 & 7 {
        1 => 1.0,
        2 => 4.0,
        _ => return None,
    };
    let part = |shift: u32| (me >> shift) as u32 & 0x3FF;
    let (west_east, south_north) = (part(32), part(21));
    if west_east == 0 || south_north == 0 {
        return None;
    }
    let east = (west_east - 1) as f32 * step * if me >> 42 & 1 == 1 { -1.0 } else { 1.0 };
    let north = (south_north - 1) as f32 * step * if me >> 31 & 1 == 1 { -1.0 } else { 1.0 };
    let rate = (me >> 10) as i32 & 0x1FF;
    let vertical_fpm = (rate != 0).then(|| (rate - 1) * 64 * if me >> 19 & 1 == 1 { -1 } else { 1 });
    Some(Says::Velocity { speed_kt: east.hypot(north), track_deg: east.atan2(north).to_degrees().rem_euclid(360.0), vertical_fpm })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::protocol::adsb::cpr;
    use crate::protocol::adsb::mode_s::parity;

    /// A message as it is written: 28 or 14 hexadecimal digits.
    pub(crate) fn frame(hex: &str) -> Frame {
        let mut bytes = [0u8; 14];
        for (k, byte) in bytes.iter_mut().enumerate().take(hex.len() / 2) {
            *byte = u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).unwrap();
        }
        assert_eq!(parity(&bytes[..hex.len() / 2]), 0, "{hex}: its parity");
        Frame { at: 0, bytes, long: hex.len() == 28, level_dbfs: -20.0 }
    }

    #[test]
    fn a_callsign_and_a_category() {
        let s = read(&frame("8D4840D6202CC371C32CE0576098"));
        assert_eq!(s, Squitter { icao: 0x4840D6, says: Says::Identity { callsign: "KLM1023".into(), category: None } });
    }

    #[test]
    fn an_altitude_and_the_two_halves_of_a_place() {
        let even = read(&frame("8D40621D58C382D690C8AC2863A7"));
        let odd = read(&frame("8D40621D58C386435CC412692AD6"));
        let (Says::Position { altitude_ft, half: e }, Says::Position { half: o, .. }) = (even.says, odd.says) else { panic!("not positions") };
        assert_eq!((even.icao, altitude_ft), (0x40621D, Some(38_000)));
        assert_eq!((e, o), (Half { odd: false, lat: 93_000, lon: 51_372 }, Half { odd: true, lat: 74_158, lon: 50_194 }));
        let (lat, lon) = cpr::whole((e.lat, e.lon), (o.lat, o.lon), false).unwrap();
        assert!((lat - 52.2572).abs() < 1e-4 && (lon - 3.9194).abs() < 1e-4, "{lat} {lon}");
    }

    #[test]
    fn a_velocity_with_its_track_and_its_rate_of_climb() {
        let Says::Velocity { speed_kt, track_deg, vertical_fpm } = read(&frame("8D485020994409940838175B284F")).says else { panic!("not a velocity") };
        assert!((speed_kt - 159.2).abs() < 0.05 && (track_deg - 182.88).abs() < 0.01, "{speed_kt} {track_deg}");
        assert_eq!(vertical_fpm, Some(-832));
    }

    #[test]
    fn what_is_not_read_says_its_type() {
        // An all-call reply gives the address alone.
        assert_eq!(read(&frame("5DA1BAC2892941")), Squitter { icao: 0xA1BAC2, says: Says::Other(0) });
        // The older altitude code, and an altitude field of nothing.
        assert_eq!((altitude(0x000), altitude(0xC38), altitude(0x010)), (None, Some(38_000), Some(-1000)));
    }
}

//! What a UAT message from an aircraft says. Its first byte is its type (five bits: 0 is the
//! short message, 18 bytes; the others are long, 34) and who it is from (three bits: 0 an
//! aircraft by its own address, 1 one by an address it made up; the rest are sent by ground
//! stations of other traffic). Then 24 bits of address and the state: a latitude of 23 bits
//! and a longitude of 24 in steps of 360/2^24 degrees, an altitude of 12 bits in steps of 25 ft
//! from 1,000 ft under zero, whether it is in the air, its velocity north and east (a sign and
//! ten bits one more than the knots, each) and its rate of climb (a sign and nine bits one more
//! than its steps of 64 ft a minute). A long message of type 1 or 3 goes on with its category
//! and eight characters, three to two bytes in base 40, which are its callsign when a flag says
//! so and its squawk otherwise.
//!
//! Read here: all of that but the squawk. Not read: a message a ground station sends of other
//! traffic, what an aircraft on the ground says of its speed, and the rest of a long message.

/// The characters of a callsign by their number in base 40.
const CHARS: &[u8; 40] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ  ??";
/// A short message's bytes, and a long one's.
pub const SHORT: usize = 18;
pub const LONG: usize = 34;

#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The aircraft's 24-bit address.
    pub icao: u32,
    /// The address is one the aircraft made up, not the one it is registered by.
    pub made_up: bool,
    /// Its latitude and longitude, degrees north and east.
    pub place: Option<(f64, f64)>,
    /// Its altitude by pressure, feet.
    pub altitude_ft: Option<i32>,
    /// Its speed over the ground (knots) and its track (degrees from north), in the air.
    pub velocity: Option<(f32, f32)>,
    /// Its rate of climb, feet a minute; down is negative.
    pub vertical_fpm: Option<i32>,
    pub callsign: Option<String>,
    /// Its category, a set and a number as `A1`.
    pub category: Option<String>,
}

/// What a message's bytes say, once its code has put them right: none when it is from a ground
/// station, of other traffic.
pub fn read(payload: &[u8]) -> Option<Message> {
    let b = payload;
    if b.len() < SHORT || b[0] & 7 > 1 {
        return None;
    }
    let icao = u32::from(b[1]) << 16 | u32::from(b[2]) << 8 | u32::from(b[3]);
    let lat = u32::from(b[4]) << 15 | u32::from(b[5]) << 7 | u32::from(b[6]) >> 1;
    let lon = u32::from(b[6] & 1) << 23 | u32::from(b[7]) << 15 | u32::from(b[8]) << 7 | u32::from(b[9]) >> 1;
    let place = (lat != 0 || lon != 0).then(|| {
        let step = 360.0 / f64::from(1u32 << 24);
        let (lat, lon) = (f64::from(lat) * step, f64::from(lon) * step);
        (if lat > 90.0 { lat - 180.0 } else { lat }, if lon > 180.0 { lon - 360.0 } else { lon })
    });
    let altitude = i32::from(b[10]) << 4 | i32::from(b[11]) >> 4;
    let altitude_ft = (altitude != 0).then(|| (altitude - 1) * 25 - 1000);
    // In the air, at any speed: the second kind counts its knots in fours.
    let step = match b[12] >> 6 {
        0 => Some(1.0),
        1 => Some(4.0),
        _ => None,
    };
    let north = u32::from(b[12] & 0x1F) << 6 | u32::from(b[13]) >> 2;
    let east = u32::from(b[13] & 3) << 9 | u32::from(b[14]) << 1 | u32::from(b[15]) >> 7;
    let knots = |field: u32, step: f32| (field & 0x3FF != 0).then(|| ((field & 0x3FF) - 1) as f32 * step * if field & 0x400 != 0 { -1.0 } else { 1.0 });
    let velocity = step.and_then(|step| {
        let (north, east) = (knots(north, step)?, knots(east, step)?);
        Some((north.hypot(east), east.atan2(north).to_degrees().rem_euclid(360.0)))
    });
    let climb = i32::from(b[15] & 0x7F) << 4 | i32::from(b[16]) >> 4;
    let vertical_fpm = (step.is_some() && climb & 0x1FF != 0).then(|| ((climb & 0x1FF) - 1) * 64 * if climb & 0x200 != 0 { -1 } else { 1 });
    let (mut callsign, mut category) = (None, None);
    if b.len() >= LONG && matches!(b[0] >> 3, 1 | 3) {
        let three = |k: usize| u32::from(b[17 + 2 * k]) << 8 | u32::from(b[18 + 2 * k]);
        let which = three(0) / 1600 % 40;
        // The sets of eight, as the other link names them: 1 is A1.
        category = (which != 0 && which < 32).then(|| format!("{}{}", char::from(b'A' + (which / 8) as u8), which % 8));
        if b[26] & 2 != 0 {
            let numbers = [three(0) / 40 % 40, three(0) % 40, three(1) / 1600 % 40, three(1) / 40 % 40, three(1) % 40, three(2) / 1600 % 40, three(2) / 40 % 40, three(2) % 40];
            let text: String = numbers.iter().map(|&n| char::from(CHARS[n as usize])).collect();
            callsign = Some(text.trim().to_string()).filter(|t| !t.is_empty());
        }
    }
    Some(Message { icao, made_up: b[0] & 7 == 1, place, altitude_ft, velocity, vertical_fpm, callsign, category })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len() / 2).map(|k| u8::from_str_radix(&hex[2 * k..2 * k + 2], 16).unwrap()).collect()
    }

    /// A light aircraft's messages as unit A heard them: a short one, then two long ones a few
    /// seconds on, the first with its callsign and the second with its squawk.
    pub(crate) const SHORT_HEARD: &str = "00a820902b1d718ba2620d99104e1980e800";
    pub(crate) const LONG_CALLSIGN: &str = "08a820902b1d698ba2480d99104e1980e809de0cd444440bcea4c2a0000cf0000000";
    pub(crate) const LONG_SQUAWK: &str = "08a820902b1d438ba1d20dd9104e1900f806442cfded2d0b16a4c0a0000d10000000";

    #[test]
    fn a_short_message_says_where_how_high_and_how_fast() {
        let m = read(&bytes(SHORT_HEARD)).unwrap();
        assert_eq!((m.icao, m.made_up, m.altitude_ft, m.vertical_fpm, m.callsign, m.category), (0xA82090, false, Some(4400), Some(832), None, None));
        let (lat, lon) = m.place.unwrap();
        assert!((lat - 30.3152).abs() < 1e-4 && (lon + 81.8196).abs() < 1e-4, "{lat} {lon}");
        // 18 knots south and 50 west.
        let (speed, track) = m.velocity.unwrap();
        assert!((speed - 53.1).abs() < 0.1 && (track - 250.2).abs() < 0.1, "{speed} {track}");
    }

    #[test]
    fn a_long_message_says_its_callsign_when_its_flag_says_so() {
        let named = read(&bytes(LONG_CALLSIGN)).unwrap();
        assert_eq!((named.icao, named.callsign.as_deref(), named.category.as_deref(), named.altitude_ft), (0xA82090, Some("N6224A"), Some("A1"), Some(4400)));
        // The same eight characters are its squawk in the next: no callsign of it.
        let squawking = read(&bytes(LONG_SQUAWK)).unwrap();
        assert_eq!((squawking.callsign, squawking.category.as_deref()), (None, Some("A1")));
    }

    #[test]
    fn what_a_ground_station_sends_of_others_and_what_says_nothing_is_none() {
        let mut relayed = bytes(SHORT_HEARD);
        relayed[0] = (relayed[0] & 0xF8) | 2;
        assert_eq!(read(&relayed), None);
        let mut made_up = bytes(SHORT_HEARD);
        made_up[0] |= 1;
        assert!(read(&made_up).unwrap().made_up);
        // No place, no altitude, on the ground: nothing of each.
        let mut silent = bytes(SHORT_HEARD);
        silent[4..12].fill(0);
        silent[12] = 0x80;
        let m = read(&silent).unwrap();
        assert_eq!((m.place, m.altitude_ft, m.velocity, m.vertical_fpm), (None, None, None, None));
        assert_eq!(read(&[0u8; 10]), None);
    }
}

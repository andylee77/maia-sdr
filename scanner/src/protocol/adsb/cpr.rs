//! A whole position from two halves. An airborne position is sent as 17 bits of latitude and 17
//! of longitude within a zone, in turn on a grid of 60 zones of latitude (even) and one of 59
//! (odd); the two together say which zone, anywhere on the globe.

use std::f64::consts::PI;

/// A half's 17 bits as a fraction of its zone.
const FRACTION: f64 = 131_072.0;

/// The number of longitude zones at a latitude.
pub fn zones(lat: f64) -> u32 {
    let lat = lat.abs();
    if lat == 0.0 {
        return 59;
    }
    if lat >= 87.0 {
        return if lat > 87.0 { 1 } else { 2 };
    }
    let a = 1.0 - (PI / 30.0).cos();
    let b = lat.to_radians().cos().powi(2);
    (2.0 * PI / (1.0 - a / b).acos()).floor() as u32
}

/// The latitude and longitude (degrees, north and east) an even and an odd half give, each its
/// 17 bits of latitude and of longitude; `newest_odd` says which was heard last, whose place it
/// is. None when the two are not of one zone of latitude (the aircraft crossed between them).
pub fn whole(even: (u32, u32), odd: (u32, u32), newest_odd: bool) -> Option<(f64, f64)> {
    let (lat_e, lon_e) = (f64::from(even.0) / FRACTION, f64::from(even.1) / FRACTION);
    let (lat_o, lon_o) = (f64::from(odd.0) / FRACTION, f64::from(odd.1) / FRACTION);
    let j = (59.0 * lat_e - 60.0 * lat_o + 0.5).floor();
    let south = |lat: f64| if lat >= 270.0 { lat - 360.0 } else { lat };
    let by_even = south(6.0 * (j.rem_euclid(60.0) + lat_e));
    let by_odd = south(360.0 / 59.0 * (j.rem_euclid(59.0) + lat_o));
    if zones(by_even) != zones(by_odd) {
        return None;
    }
    let lat = if newest_odd { by_odd } else { by_even };
    let nl = f64::from(zones(lat));
    let n = (nl - if newest_odd { 1.0 } else { 0.0 }).max(1.0);
    let m = (lon_e * (nl - 1.0) - lon_o * nl + 0.5).floor();
    let lon = 360.0 / n * (m.rem_euclid(n) + if newest_odd { lon_o } else { lon_e });
    Some((lat, if lon >= 180.0 { lon - 360.0 } else { lon }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_zones_of_longitude_thin_towards_the_poles() {
        assert_eq!((zones(0.0), zones(10.0), zones(30.0), zones(52.0), zones(-52.0), zones(86.9), zones(87.0), zones(89.0)), (59, 59, 51, 36, 36, 2, 2, 1));
    }

    #[test]
    fn an_even_and_an_odd_half_give_the_place() {
        // The halves of two messages an aircraft over the North Sea sent a moment apart.
        let (even, odd) = ((93_000, 51_372), (74_158, 50_194));
        let (lat, lon) = whole(even, odd, false).unwrap();
        assert!((lat - 52.2572).abs() < 1e-4 && (lon - 3.9194).abs() < 1e-4, "{lat} {lon}");
        // With the odd one the newer, the place is the odd one's: the aircraft had moved on.
        let (lat_o, lon_o) = whole(even, odd, true).unwrap();
        assert!((lat_o - 52.2658).abs() < 1e-4 && (lon_o - 3.9389).abs() < 1e-4, "{lat_o} {lon_o}");
        // West of the meridian the longitude comes back negative.
        let (n, w) = whole((0x1_0000, 0x1_8000), (0x1_0000, 0x1_8000), false).unwrap();
        assert!((n - 3.0).abs() < 1e-9 && (w + 1.525).abs() < 0.01, "{n} {w}");
    }

    #[test]
    fn halves_of_two_zones_of_latitude_give_none() {
        // By the even half the latitude has one more zone of longitude than by the odd one:
        // the two are not of one place.
        assert_eq!(whole((0, 1_000), (22_881, 1_000), false), None);
        assert!(whole((0, 1_000), (22_000, 1_000), false).is_some());
    }
}

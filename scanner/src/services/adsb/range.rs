//! How far an aircraft is from the radio, and which way: the great circle between two places,
//! on a sphere of the Earth's mean radius. That is within half a percent of the ellipsoid's
//! answer, less than an aircraft moves between two of its messages.

use crate::services::config::radio::Position;

/// The Earth's mean radius, nautical miles.
const RADIUS_NM: f64 = 6_371.008_8 / 1.852;

/// From `from` to a place: the distance over the ground, nautical miles, and the bearing at
/// `from`, degrees clockwise from north.
pub fn to(from: Position, lat: f64, lon: f64) -> (f64, f64) {
    let (here, there) = (from.lat.to_radians(), lat.to_radians());
    let east = (lon - from.lon).to_radians();
    let half = ((there - here) / 2.0).sin().powi(2) + here.cos() * there.cos() * (east / 2.0).sin().powi(2);
    let distance = 2.0 * RADIUS_NM * half.sqrt().min(1.0).asin();
    let bearing = (east.sin() * there.cos()).atan2(here.cos() * there.sin() - here.sin() * there.cos() * east.cos());
    (distance, bearing.to_degrees().rem_euclid(360.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(got: (f64, f64), distance: f64, bearing: f64) -> bool {
        (got.0 - distance).abs() < 0.01 && (got.1 - bearing).abs() < 0.01
    }

    #[test]
    fn a_degree_of_the_sphere_is_sixty_miles_and_the_bearing_is_from_north() {
        let origin = Position { lat: 0.0, lon: 0.0 };
        assert!(near(to(origin, 1.0, 0.0), 60.04, 0.0));
        assert!(near(to(origin, 0.0, 1.0), 60.04, 90.0));
        assert!(near(to(origin, -1.0, 0.0), 60.04, 180.0));
        assert!(near(to(origin, 0.0, -1.0), 60.04, 270.0));
        // A degree of longitude is shorter away from the equator, and the great circle to a
        // place due west sets out a little north of west.
        assert!(near(to(Position { lat: 30.0, lon: -81.0 }, 30.0, -82.0), 52.00, 270.25));
        assert!(near(to(Position { lat: 30.2, lon: -81.6 }, 29.2, -82.6), 79.53, 221.23));
        // The same place: no distance, and a bearing all the same.
        assert_eq!(to(Position { lat: 30.0, lon: -81.0 }, 30.0, -81.0), (0.0, 0.0));
    }

    #[test]
    fn long_ways_are_the_great_circle_s() {
        // New York (JFK) to Los Angeles (LAX), and London (LHR) to New York: the distances and
        // first bearings the airlines' great circles have, to a few miles on a sphere.
        let jfk = Position { lat: 40.6413, lon: -73.7781 };
        let (distance, bearing) = to(jfk, 33.9416, -118.4085);
        assert!((2_140.0..2_152.0).contains(&distance) && (273.0..275.0).contains(&bearing), "{distance} {bearing}");
        let (distance, bearing) = to(Position { lat: 51.47, lon: -0.4543 }, jfk.lat, jfk.lon);
        assert!((2_985.0..2_998.0).contains(&distance) && (287.0..289.0).contains(&bearing), "{distance} {bearing}");
        // Half way round the world, across the date line.
        let (distance, _) = to(Position { lat: 10.0, lon: 20.0 }, -10.0, -160.0);
        assert!((distance - 10_807.3).abs() < 0.1, "{distance}");
    }
}

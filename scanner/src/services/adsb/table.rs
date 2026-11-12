//! The aircraft heard on either link, each with what it last said, by its address. An aircraft
//! is listed once two messages gave its address: a single Mode S one may be noise that passed
//! the parity. It goes when it has not been heard for a minute.

use std::collections::HashMap;

use serde::Serialize;

use super::{range, Link};
use crate::protocol::adsb::cpr;
use crate::protocol::adsb::squitter::{Half, Says, Squitter};
use crate::protocol::adsb::uat::message::Message;
use crate::services::config::radio::Position;

/// An aircraft not heard for this long is gone.
pub const SILENT_MS: u64 = 60_000;
/// An even and an odd half make a place when they were heard within this of each other.
const PAIR_MS: u64 = 10_000;
/// Messages before an aircraft is listed.
const LISTED: u64 = 2;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Aircraft {
    /// Its 24-bit address, six hexadecimal digits.
    pub icao: String,
    /// The link it was last heard on.
    pub link: Link,
    pub callsign: Option<String>,
    pub category: Option<String>,
    pub altitude_ft: Option<i32>,
    pub speed_kt: Option<f32>,
    pub track_deg: Option<f32>,
    pub vertical_fpm: Option<i32>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// When its place was last worked out.
    pub position_unix_ms: Option<u64>,
    /// How far its place is from the radio's, nautical miles over the ground, and which way
    /// from the radio, degrees from north: none until the radio has been told where it is.
    pub distance_nm: Option<f32>,
    pub bearing_deg: Option<f32>,
    /// Its last message's pulses against the ADC's full scale, dB.
    pub level_dbfs: f32,
    pub messages: u64,
    pub first_unix_ms: u64,
    pub last_unix_ms: u64,
}

struct Entry {
    aircraft: Aircraft,
    /// The last even and the last odd half of its place, and when each was heard.
    halves: [Option<(Half, u64)>; 2],
}

#[derive(Default)]
pub struct Table {
    by: HashMap<u32, Entry>,
    listed: u64,
}

fn tenths(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

impl Table {
    /// The aircraft a message of `link` is from, counted and brought up to `now_ms`.
    fn message(&mut self, icao: u32, link: Link, level_dbfs: f32, now_ms: u64) -> &mut Entry {
        let entry = self.by.entry(icao).or_insert_with(|| Entry {
            aircraft: Aircraft {
                icao: format!("{icao:06X}"),
                link,
                callsign: None,
                category: None,
                altitude_ft: None,
                speed_kt: None,
                track_deg: None,
                vertical_fpm: None,
                lat: None,
                lon: None,
                position_unix_ms: None,
                distance_nm: None,
                bearing_deg: None,
                level_dbfs: 0.0,
                messages: 0,
                first_unix_ms: now_ms,
                last_unix_ms: now_ms,
            },
            halves: [None; 2],
        });
        let a = &mut entry.aircraft;
        a.link = link;
        a.messages += 1;
        a.last_unix_ms = now_ms;
        a.level_dbfs = tenths(level_dbfs);
        if a.messages == LISTED {
            self.listed += 1;
        }
        entry
    }

    /// A Mode S message whose parity was right, heard at `now_ms`.
    pub fn heard(&mut self, s: &Squitter, level_dbfs: f32, now_ms: u64) {
        let entry = self.message(s.icao, Link::Es, level_dbfs, now_ms);
        let a = &mut entry.aircraft;
        match &s.says {
            Says::Other(_) => {}
            Says::Identity { callsign, category } => {
                a.callsign = Some(callsign.clone());
                a.category = category.clone().or(a.category.take());
            }
            Says::Position { altitude_ft, half } => {
                a.altitude_ft = altitude_ft.or(a.altitude_ft);
                entry.halves[usize::from(half.odd)] = Some((*half, now_ms));
                if let [Some((even, even_at)), Some((odd, odd_at))] = entry.halves {
                    if even_at.abs_diff(odd_at) <= PAIR_MS {
                        if let Some((lat, lon)) = cpr::whole((even.lat, even.lon), (odd.lat, odd.lon), half.odd) {
                            // To a metre.
                            a.lat = Some((lat * 1e5).round() / 1e5);
                            a.lon = Some((lon * 1e5).round() / 1e5);
                            a.position_unix_ms = Some(now_ms);
                        }
                    }
                }
            }
            Says::Velocity { speed_kt, track_deg, vertical_fpm } => {
                a.speed_kt = Some(tenths(*speed_kt));
                a.track_deg = Some(tenths(*track_deg));
                a.vertical_fpm = vertical_fpm.or(a.vertical_fpm);
            }
        }
    }

    /// A UAT message its code put right, heard at `now_ms`: it says its place whole.
    pub fn said(&mut self, m: &Message, level_dbfs: f32, now_ms: u64) {
        let a = &mut self.message(m.icao, Link::Uat, level_dbfs, now_ms).aircraft;
        if let Some(callsign) = &m.callsign {
            a.callsign = Some(callsign.clone());
        }
        a.category = m.category.clone().or(a.category.take());
        a.altitude_ft = m.altitude_ft.or(a.altitude_ft);
        if let Some((lat, lon)) = m.place {
            a.lat = Some((lat * 1e5).round() / 1e5);
            a.lon = Some((lon * 1e5).round() / 1e5);
            a.position_unix_ms = Some(now_ms);
        }
        if let Some((speed_kt, track_deg)) = m.velocity {
            a.speed_kt = Some(tenths(speed_kt));
            a.track_deg = Some(tenths(track_deg));
        }
        a.vertical_fpm = m.vertical_fpm.or(a.vertical_fpm);
    }

    /// Drop the aircraft not heard for a minute; true when one that was listed went.
    pub fn tidy(&mut self, now_ms: u64) -> bool {
        let listed = self.count();
        self.by.retain(|_, e| now_ms.saturating_sub(e.aircraft.last_unix_ms) < SILENT_MS);
        self.count() != listed
    }

    /// The aircraft heard twice or more, by their addresses, each that has said its place with
    /// how far and which way that is from `radio`.
    pub fn list(&self, radio: Option<Position>) -> Vec<Aircraft> {
        let mut list: Vec<Aircraft> = self.by.values().filter(|e| e.aircraft.messages >= LISTED).map(|e| e.aircraft.clone()).collect();
        for a in &mut list {
            if let (Some(radio), Some(lat), Some(lon)) = (radio, a.lat, a.lon) {
                let (distance, bearing) = range::to(radio, lat, lon);
                a.distance_nm = Some(tenths(distance as f32));
                a.bearing_deg = Some(tenths(bearing as f32));
            }
        }
        list.sort_by(|a, b| a.icao.cmp(&b.icao));
        list
    }

    pub fn count(&self) -> usize {
        self.by.values().filter(|e| e.aircraft.messages >= LISTED).count()
    }

    /// Times an aircraft came to be listed: one heard again after its minute of silence counts
    /// again.
    pub fn listed(&self) -> u64 {
        self.listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::adsb::squitter::read;
    use crate::protocol::adsb::squitter::tests::frame;

    #[test]
    fn an_aircraft_is_listed_at_its_second_message_with_what_it_said() {
        let mut table = Table::default();
        let hear = |table: &mut Table, hex: &str, at: u64| table.heard(&read(&frame(hex)), -21.24, at);
        hear(&mut table, "8D40621D58C382D690C8AC2863A7", 1_000);
        assert_eq!((table.list(None), table.count(), table.listed()), (vec![], 0, 0));
        hear(&mut table, "8D40621D58C386435CC412692AD6", 1_400);
        let list = table.list(None);
        assert_eq!((list.len(), table.count(), table.listed()), (1, 1, 1));
        let a = &list[0];
        assert_eq!((a.icao.as_str(), a.link, a.altitude_ft, a.messages, a.first_unix_ms, a.last_unix_ms, a.level_dbfs), ("40621D", Link::Es, Some(38_000), 2, 1_000, 1_400, -21.2));
        // The odd half came last: the place is its.
        assert_eq!((a.lat, a.lon, a.position_unix_ms), (Some(52.26578), Some(3.93891), Some(1_400)));
        // Another aircraft's callsign and velocity; an all-call reply counts as a message.
        hear(&mut table, "8D4840D6202CC371C32CE0576098", 2_000);
        hear(&mut table, "8D485020994409940838175B284F", 2_100);
        hear(&mut table, "5DA1BAC2892941", 2_200);
        hear(&mut table, "5DA1BAC2892941", 2_300);
        let list = table.list(None);
        assert_eq!(list.iter().map(|a| a.icao.as_str()).collect::<Vec<_>>(), ["40621D", "A1BAC2"]);
        hear(&mut table, "8D4840D6202CC371C32CE0576098", 2_400);
        let klm = table.list(None).into_iter().find(|a| a.icao == "4840D6").unwrap();
        assert_eq!((klm.callsign.as_deref(), klm.category, klm.speed_kt, klm.lat), (Some("KLM1023"), None, None, None));
        hear(&mut table, "8D485020994409940838175B284F", 2_500);
        let other = table.list(None).into_iter().find(|a| a.icao == "485020").unwrap();
        assert_eq!((other.speed_kt, other.track_deg, other.vertical_fpm), (Some(159.2), Some(182.9), Some(-832)));
    }

    #[test]
    fn what_the_other_link_says_is_the_aircraft_s_too() {
        use crate::protocol::adsb::uat::message::tests::{bytes, LONG_CALLSIGN, LONG_SQUAWK, SHORT_HEARD};
        use crate::protocol::adsb::uat::message::read as read_uat;
        let mut table = Table::default();
        table.said(&read_uat(&bytes(SHORT_HEARD)).unwrap(), -37.66, 5_000);
        assert_eq!(table.count(), 0);
        table.said(&read_uat(&bytes(LONG_CALLSIGN)).unwrap(), -37.66, 6_000);
        let a = table.list(None).remove(0);
        assert_eq!((a.icao.as_str(), a.link, a.callsign.as_deref(), a.category.as_deref(), a.altitude_ft, a.messages), ("A82090", Link::Uat, Some("N6224A"), Some("A1"), Some(4_400), 2));
        // Its place whole from one message, its velocity and its climb.
        assert_eq!((a.lat, a.lon, a.position_unix_ms, a.speed_kt, a.track_deg, a.vertical_fpm, a.level_dbfs), (Some(30.31514), Some(-81.81991), Some(6_000), Some(53.1), Some(250.2), Some(832), -37.7));
        // A message with its squawk in the callsign's place leaves the callsign as it was.
        table.said(&read_uat(&bytes(LONG_SQUAWK)).unwrap(), -37.0, 7_000);
        assert_eq!(table.list(None)[0].callsign.as_deref(), Some("N6224A"));
        // Heard on the other link, it is the one aircraft, last heard there.
        table.heard(&read(&frame("5DA820909C9F8F")), -20.0, 8_000);
        let a = table.list(None).remove(0);
        assert_eq!((table.count(), a.link, a.messages, a.callsign.as_deref()), (1, Link::Es, 4, Some("N6224A")));
    }

    #[test]
    fn an_aircraft_s_distance_and_bearing_are_from_the_radio_s_place_once_it_has_one() {
        use crate::protocol::adsb::uat::message::read as read_uat;
        use crate::protocol::adsb::uat::message::tests::{bytes, LONG_CALLSIGN, SHORT_HEARD};
        let mut table = Table::default();
        // One that says where it is (30.31514, -81.81991), and one that does not.
        table.said(&read_uat(&bytes(SHORT_HEARD)).unwrap(), -30.0, 5_000);
        table.said(&read_uat(&bytes(LONG_CALLSIGN)).unwrap(), -30.0, 6_000);
        table.heard(&read(&frame("5DA1BAC2892941")), -20.0, 6_100);
        table.heard(&read(&frame("5DA1BAC2892941")), -20.0, 6_200);
        // The radio has not been told where it is: no distances.
        assert!(table.list(None).iter().all(|a| a.distance_nm.is_none() && a.bearing_deg.is_none()));
        // A degree of latitude south of the aircraft, on its meridian: 60 miles due north.
        let list = table.list(Some(Position { lat: 29.31514, lon: -81.81991 }));
        assert_eq!(list.iter().map(|a| (a.icao.as_str(), a.distance_nm, a.bearing_deg)).collect::<Vec<_>>(), [("A1BAC2", None, None), ("A82090", Some(60.0), Some(0.0))]);
        // East of it at its latitude: a little north of west, and nearer than a degree at the equator.
        let a = table.list(Some(Position { lat: 30.31514, lon: -80.81991 })).remove(1);
        assert_eq!((a.distance_nm, a.bearing_deg), (Some(51.8), Some(270.3)));
        // At the aircraft's own place.
        let a = table.list(Some(Position { lat: 30.31514, lon: -81.81991 })).remove(1);
        assert_eq!((a.distance_nm, a.bearing_deg), (Some(0.0), Some(0.0)));
    }

    #[test]
    fn halves_heard_far_apart_make_no_place_and_a_silent_aircraft_goes() {
        let mut table = Table::default();
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 1_000);
        table.heard(&read(&frame("8D40621D58C386435CC412692AD6")), -20.0, 12_000);
        assert_eq!(table.list(None)[0].lat, None);
        // The even half again, within ten seconds of the odd one: its place.
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 13_000);
        assert_eq!((table.list(None)[0].lat, table.list(None)[0].lon), (Some(52.2572), Some(3.91937)));
        table.tidy(72_999);
        assert_eq!(table.count(), 1);
        table.tidy(73_000);
        assert_eq!((table.count(), table.listed()), (0, 1));
        // Heard again, it is listed again at its second message.
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 80_000);
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 80_500);
        assert_eq!((table.count(), table.listed()), (1, 2));
    }
}

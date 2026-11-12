//! The aircraft heard, each with what it last said, by its address. An aircraft is listed once
//! two messages gave its address: a single one may be noise that passed the parity. It goes
//! when it has not been heard for a minute.

use std::collections::HashMap;

use serde::Serialize;

use crate::protocol::adsb::cpr;
use crate::protocol::adsb::squitter::{Half, Says, Squitter};

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
    /// A message whose parity was right, heard at `now_ms`.
    pub fn heard(&mut self, s: &Squitter, level_dbfs: f32, now_ms: u64) {
        let entry = self.by.entry(s.icao).or_insert_with(|| Entry {
            aircraft: Aircraft {
                icao: format!("{:06X}", s.icao),
                callsign: None,
                category: None,
                altitude_ft: None,
                speed_kt: None,
                track_deg: None,
                vertical_fpm: None,
                lat: None,
                lon: None,
                position_unix_ms: None,
                level_dbfs: 0.0,
                messages: 0,
                first_unix_ms: now_ms,
                last_unix_ms: now_ms,
            },
            halves: [None; 2],
        });
        let a = &mut entry.aircraft;
        a.messages += 1;
        a.last_unix_ms = now_ms;
        a.level_dbfs = tenths(level_dbfs);
        if a.messages == LISTED {
            self.listed += 1;
        }
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

    /// Drop the aircraft not heard for a minute; true when one that was listed went.
    pub fn tidy(&mut self, now_ms: u64) -> bool {
        let listed = self.count();
        self.by.retain(|_, e| now_ms.saturating_sub(e.aircraft.last_unix_ms) < SILENT_MS);
        self.count() != listed
    }

    /// The aircraft heard twice or more, by their addresses.
    pub fn list(&self) -> Vec<Aircraft> {
        let mut list: Vec<Aircraft> = self.by.values().filter(|e| e.aircraft.messages >= LISTED).map(|e| e.aircraft.clone()).collect();
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
        assert_eq!((table.list(), table.count(), table.listed()), (vec![], 0, 0));
        hear(&mut table, "8D40621D58C386435CC412692AD6", 1_400);
        let list = table.list();
        assert_eq!((list.len(), table.count(), table.listed()), (1, 1, 1));
        let a = &list[0];
        assert_eq!((a.icao.as_str(), a.altitude_ft, a.messages, a.first_unix_ms, a.last_unix_ms, a.level_dbfs), ("40621D", Some(38_000), 2, 1_000, 1_400, -21.2));
        // The odd half came last: the place is its.
        assert_eq!((a.lat, a.lon, a.position_unix_ms), (Some(52.26578), Some(3.93891), Some(1_400)));
        // Another aircraft's callsign and velocity; an all-call reply counts as a message.
        hear(&mut table, "8D4840D6202CC371C32CE0576098", 2_000);
        hear(&mut table, "8D485020994409940838175B284F", 2_100);
        hear(&mut table, "5DA1BAC2892941", 2_200);
        hear(&mut table, "5DA1BAC2892941", 2_300);
        let list = table.list();
        assert_eq!(list.iter().map(|a| a.icao.as_str()).collect::<Vec<_>>(), ["40621D", "A1BAC2"]);
        hear(&mut table, "8D4840D6202CC371C32CE0576098", 2_400);
        let klm = table.list().into_iter().find(|a| a.icao == "4840D6").unwrap();
        assert_eq!((klm.callsign.as_deref(), klm.category, klm.speed_kt, klm.lat), (Some("KLM1023"), None, None, None));
        hear(&mut table, "8D485020994409940838175B284F", 2_500);
        let other = table.list().into_iter().find(|a| a.icao == "485020").unwrap();
        assert_eq!((other.speed_kt, other.track_deg, other.vertical_fpm), (Some(159.2), Some(182.9), Some(-832)));
    }

    #[test]
    fn halves_heard_far_apart_make_no_place_and_a_silent_aircraft_goes() {
        let mut table = Table::default();
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 1_000);
        table.heard(&read(&frame("8D40621D58C386435CC412692AD6")), -20.0, 12_000);
        assert_eq!(table.list()[0].lat, None);
        // The even half again, within ten seconds of the odd one: its place.
        table.heard(&read(&frame("8D40621D58C382D690C8AC2863A7")), -20.0, 13_000);
        assert_eq!((table.list()[0].lat, table.list()[0].lon), (Some(52.2572), Some(3.91937)));
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

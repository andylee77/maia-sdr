//! The event log behind the Diagnostics events box: the control channel's messages in
//! SDRTrunk's text, and what the radio and the services did. Housekeeping broadcasts (several a
//! second) are kept only briefly, in a ring of their own.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

use crate::protocol::events::LogLine;
use crate::util::time;

/// Events other than housekeeping.
const EVENTS: usize = 2000;
/// Everything, housekeeping included (about 12 s of a P25 control channel).
const ALL: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub seq: u64,
    pub unix_ms: u64,
    /// "p25", "dmr" or "system".
    pub source: &'static str,
    pub class: &'static str,
    pub text: String,
    pub routine: bool,
    pub valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tg: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<u32>,
}

#[derive(Default)]
struct Rings {
    seq: u64,
    events: VecDeque<Record>,
    all: VecDeque<Record>,
}

#[derive(Default)]
pub struct EventLog {
    rings: Mutex<Rings>,
}

fn push_capped(ring: &mut VecDeque<Record>, record: Record, cap: usize) {
    ring.push_back(record);
    while ring.len() > cap {
        ring.pop_front();
    }
}

impl EventLog {
    fn push(&self, mut record: Record) {
        let Ok(mut r) = self.rings.lock() else { return };
        r.seq += 1;
        record.seq = r.seq;
        if !record.routine {
            push_capped(&mut r.events, record.clone(), EVENTS);
        }
        push_capped(&mut r.all, record, ALL);
    }

    /// A decoded control channel message.
    pub fn message(&self, source: &'static str, unix_ms: u64, line: &LogLine) {
        self.push(Record {
            seq: 0,
            unix_ms,
            source,
            class: line.class,
            text: line.text.clone(),
            routine: line.routine,
            valid: line.valid,
            slot: line.slot,
            tg: line.tg,
            unit: line.unit,
        });
    }

    /// Something the radio or a service did (a site switch, a modulation choice).
    pub fn system(&self, class: &'static str, text: impl Into<String>) {
        self.push(Record {
            seq: 0,
            unix_ms: time::unix_ms(),
            source: "system",
            class,
            text: text.into(),
            routine: false,
            valid: true,
            slot: None,
            tg: None,
            unit: None,
        });
    }

    /// Up to `limit` records after `after` (oldest first); housekeeping too when `routine`.
    pub fn since(&self, after: u64, limit: usize, routine: bool) -> Vec<Record> {
        let Ok(r) = self.rings.lock() else { return Vec::new() };
        let ring = if routine { &r.all } else { &r.events };
        let mut out: Vec<Record> = ring.iter().rev().take_while(|e| e.seq > after).take(limit).cloned().collect();
        out.reverse();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(class: &'static str, routine: bool) -> LogLine {
        LogLine { class, text: class.to_string(), routine, valid: true, slot: None, tg: None, unit: None }
    }

    #[test]
    fn housekeeping_stays_out_of_the_events() {
        let log = EventLog::default();
        log.message("p25", 1, &line("RFSS_STS_BCAST", true));
        log.message("p25", 2, &line("GRP_VCH_GRANT", false));
        log.system("site", "site clay live");
        let events = log.since(0, 10, false);
        assert_eq!(events.iter().map(|e| e.class).collect::<Vec<_>>(), ["GRP_VCH_GRANT", "site"]);
        assert_eq!((events[0].seq, events[1].seq), (2, 3));
        assert_eq!(log.since(0, 10, true).len(), 3);
        assert_eq!(log.since(2, 10, false).len(), 1);
        assert_eq!(log.since(0, 1, false)[0].class, "site", "the newest when limited");
    }

    #[test]
    fn rings_are_capped() {
        let log = EventLog::default();
        for _ in 0..(EVENTS + 10) {
            log.message("dmr", 0, &line("Clear", false));
        }
        assert_eq!(log.since(0, usize::MAX, false).len(), EVENTS);
        assert_eq!(log.since(0, usize::MAX, true).len(), ALL);
    }
}

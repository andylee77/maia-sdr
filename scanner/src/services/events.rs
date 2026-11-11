//! The event log behind the Diagnostics pages' events boxes: the control channel's messages in
//! SDRTrunk's text, and what the radio and the services did. A line is one part's: the
//! scanner's, TV's, or the radio's own, and each part's lines are kept in a ring of their own, so
//! a busy control channel does not push out what the radio did an hour ago. Housekeeping
//! broadcasts (several a second) are kept only briefly, in a ring of their own.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::protocol::events::LogLine;
use crate::util::time;

/// The scanner's events other than housekeeping (minutes of a busy control channel).
const EVENTS: usize = 2000;
/// TV's events, and the radio's own: a few a day.
const OTHER: usize = 500;
/// Everything, housekeeping included (about 12 s of a P25 control channel).
const ALL: usize = 500;

/// Whose a line is: a module's, or the radio's own (its start, clock, crystal, mode, the
/// configuration as a whole and what is taken off the card).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Part {
    Scanner,
    Tv,
    Radio,
}

#[derive(Debug, Clone, Serialize)]
pub struct Record {
    pub seq: u64,
    pub unix_ms: u64,
    pub part: Part,
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
    scanner: VecDeque<Record>,
    tv: VecDeque<Record>,
    radio: VecDeque<Record>,
    all: VecDeque<Record>,
}

impl Rings {
    fn of(&self, part: Part) -> &VecDeque<Record> {
        match part {
            Part::Scanner => &self.scanner,
            Part::Tv => &self.tv,
            Part::Radio => &self.radio,
        }
    }
}

/// The records of `ring` after `after`, newest first.
fn after(ring: &VecDeque<Record>, after: u64) -> impl Iterator<Item = &Record> {
    ring.iter().rev().take_while(move |e| e.seq > after)
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
            match record.part {
                Part::Scanner => push_capped(&mut r.scanner, record.clone(), EVENTS),
                Part::Tv => push_capped(&mut r.tv, record.clone(), OTHER),
                Part::Radio => push_capped(&mut r.radio, record.clone(), OTHER),
            }
        }
        push_capped(&mut r.all, record, ALL);
    }

    fn did(&self, part: Part, class: &'static str, text: String) {
        self.push(Record {
            seq: 0,
            unix_ms: time::unix_ms(),
            part,
            source: "system",
            class,
            text,
            routine: false,
            valid: true,
            slot: None,
            tg: None,
            unit: None,
        });
    }

    /// A decoded control channel message.
    pub fn message(&self, source: &'static str, unix_ms: u64, line: &LogLine) {
        self.push(Record {
            seq: 0,
            unix_ms,
            part: Part::Scanner,
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

    /// Something the scanner or one of its services did (a site switch, a modulation choice, a
    /// call).
    pub fn system(&self, class: &'static str, text: impl Into<String>) {
        self.did(Part::Scanner, class, text.into());
    }

    /// Something TV did (a TV scan, live TV).
    pub fn tv(&self, class: &'static str, text: impl Into<String>) {
        self.did(Part::Tv, class, text.into());
    }

    /// Something the radio did of its own: its start, its clock, its crystal, a change of mode,
    /// the configuration replaced, a download of what is on the card.
    pub fn radio(&self, class: &'static str, text: impl Into<String>) {
        self.did(Part::Radio, class, text.into());
    }

    /// Up to `limit` records after `after` (oldest first), of one part or of all; housekeeping
    /// too when `routine` (it is the scanner's, and held only briefly).
    pub fn since(&self, after_seq: u64, limit: usize, routine: bool, part: Option<Part>) -> Vec<Record> {
        let Ok(r) = self.rings.lock() else { return Vec::new() };
        let mut out: Vec<Record> = match (part, routine) {
            (None, true) => after(&r.all, after_seq).take(limit).cloned().collect(),
            (Some(Part::Scanner), true) => after(&r.all, after_seq).filter(|e| e.part == Part::Scanner).take(limit).cloned().collect(),
            (Some(p), _) => after(r.of(p), after_seq).take(limit).cloned().collect(),
            (None, false) => {
                let mut every: Vec<&Record> = [&r.scanner, &r.tv, &r.radio].into_iter().flat_map(|ring| after(ring, after_seq)).collect();
                every.sort_unstable_by(|a, b| b.seq.cmp(&a.seq));
                every.into_iter().take(limit).cloned().collect()
            }
        };
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
        let events = log.since(0, 10, false, None);
        assert_eq!(events.iter().map(|e| e.class).collect::<Vec<_>>(), ["GRP_VCH_GRANT", "site"]);
        assert_eq!((events[0].seq, events[1].seq), (2, 3));
        assert_eq!(log.since(0, 10, true, None).len(), 3);
        assert_eq!(log.since(2, 10, false, None).len(), 1);
        assert_eq!(log.since(0, 1, false, None)[0].class, "site", "the newest when limited");
    }

    #[test]
    fn each_part_keeps_its_own_lines() {
        let log = EventLog::default();
        log.radio("start", "scanner started");
        log.tv("atsc", "TV scan 1 started");
        // A busy control channel: more than the scanner's ring holds.
        for _ in 0..(EVENTS + 10) {
            log.message("p25", 0, &line("GRP_VCH_GRANT", false));
        }
        log.system("site", "site clay live");
        log.radio("mode", "ATSC TV mode");
        let classes = |part| log.since(0, usize::MAX, false, Some(part)).iter().map(|e| e.class).collect::<Vec<_>>();
        assert_eq!(classes(Part::Radio), ["start", "mode"], "not pushed out by the control channel");
        assert_eq!(classes(Part::Tv), ["atsc"]);
        let scanner = log.since(0, usize::MAX, false, Some(Part::Scanner));
        assert_eq!(scanner.len(), EVENTS);
        assert_eq!(scanner.last().unwrap().class, "site");
        // Every part together, in the order they were written.
        let every = log.since(0, usize::MAX, false, None);
        assert_eq!(every.len(), EVENTS + 3);
        assert!(every.windows(2).all(|w| w[0].seq < w[1].seq));
        assert_eq!((every[0].class, every[1].class, every.last().unwrap().class), ("start", "atsc", "mode"));
        assert_eq!(log.since(0, 2, false, None).iter().map(|e| e.class).collect::<Vec<_>>(), ["site", "mode"], "the newest when limited");
        // Housekeeping is the scanner's: the other parts' lines are theirs with or without it.
        assert_eq!(log.since(0, usize::MAX, true, Some(Part::Tv)).len(), 1);
        assert!(log.since(0, usize::MAX, true, Some(Part::Scanner)).iter().all(|e| e.part == Part::Scanner));
        assert_eq!(serde_json::to_value(Part::Tv).unwrap(), "tv");
    }

    #[test]
    fn rings_are_capped() {
        let log = EventLog::default();
        for _ in 0..(EVENTS + 10) {
            log.message("dmr", 0, &line("Clear", false));
        }
        assert_eq!(log.since(0, usize::MAX, false, None).len(), EVENTS);
        assert_eq!(log.since(0, usize::MAX, true, None).len(), ALL);
    }
}

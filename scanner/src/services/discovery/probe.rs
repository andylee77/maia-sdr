//! What a carrier carries. Every protocol's control decoder listens to it at once, each behind
//! `Probe`: P25 through the LSM and C4FM demodulators, DMR, all on the control channel's IQ. A
//! later survey can add classifiers without changing the sweep.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::protocol::dmr::control::DmrControl;
use crate::protocol::events::{ControlEvent, SiteIdentity};
use crate::protocol::p25::c4fm::C4fmDecoder;
use crate::protocol::p25::control::P25Control;
use crate::protocol::p25::lsm::LsmDecoder;
use crate::radio::streams::Block;
use crate::services::config::systems::{Modulation, Protocol};
use crate::trunking::learned::iden_band;
use crate::trunking::receivers::c4fm_clearly_better;
use crate::util::time::Stamp;

use super::{FoundNeighbour, FoundSite};

/// What one protocol's decoder heard since the last reset.
#[derive(Debug, Clone, PartialEq)]
pub enum Heard {
    Nothing,
    /// Its frames but no control messages: a traffic channel in a long call.
    Traffic,
    /// Its control channel; the identity once heard.
    Control(Box<FoundSite>),
}

pub trait Probe: Send {
    /// The carrier changed: forget what was heard.
    fn reset(&mut self);
    fn iq(&mut self, iq: &[i16], now: Stamp);
    /// Its control channel with the site identity complete: no need to listen longer.
    fn identified(&self) -> bool;
    /// Control messages so far (a few are enough to say it is this protocol).
    fn messages(&self) -> u64;
    fn heard(&self, freq_hz: u64, level_db: f32, secs: f64) -> Heard;
}

/// P25: TSBKs from either demodulator; the one with the better CRC pass rate describes the site.
pub struct P25Probe {
    lsm: P25Control,
    c4fm: P25Control,
    lsm_demod: LsmDecoder,
    demod: C4fmDecoder,
    events: Vec<ControlEvent>,
}

impl Default for P25Probe {
    fn default() -> Self {
        P25Probe {
            lsm: P25Control::new("probe"),
            c4fm: P25Control::new("probe"),
            lsm_demod: LsmDecoder::new(),
            demod: C4fmDecoder::new(),
            events: Vec::new(),
        }
    }
}

impl P25Probe {
    /// The site's modulation, by the receivers' rule. The C4FM path starts a moment later after
    /// a retune, so raw counts would favour LSM: pass rates once both have tried enough.
    fn c4fm_better(&self) -> bool {
        let (c, l) = (self.c4fm.stats(), self.lsm.stats());
        let rate = |ok: u64, tries: u64| ok as f64 / tries.max(1) as f64;
        if c.tsbk_attempts() >= 10 && l.tsbk_attempts() >= 10 {
            c4fm_clearly_better(rate(c.tsbk_ok(), c.tsbk_attempts()), rate(l.tsbk_ok(), l.tsbk_attempts()))
        } else {
            c4fm_clearly_better(c.tsbk_ok() as f64, l.tsbk_ok() as f64)
        }
    }
}

impl Probe for P25Probe {
    fn reset(&mut self) {
        *self = P25Probe::default();
    }

    fn iq(&mut self, iq: &[i16], now: Stamp) {
        self.c4fm.push_c4fm(&mut self.demod, iq, now, &mut self.events);
        self.lsm.push_lsm(&mut self.lsm_demod, iq, now, &mut self.events);
        self.events.clear();
    }

    fn identified(&self) -> bool {
        let done = |d: &P25Control| {
            let i = &d.announced().identity;
            i.wacn.is_some() && i.system.is_some() && i.site.is_some() && i.nac.is_some()
        };
        done(&self.lsm) || done(&self.c4fm)
    }

    fn messages(&self) -> u64 {
        self.lsm.stats().tsbk_ok() + self.c4fm.stats().tsbk_ok()
    }

    fn heard(&self, freq_hz: u64, level_db: f32, secs: f64) -> Heard {
        if self.messages() < 3 {
            let nids = self.lsm.stats().nid_ok.max(self.c4fm.stats().nid_ok);
            return if nids >= 3 { Heard::Traffic } else { Heard::Nothing };
        }
        let c4fm = self.c4fm_better();
        let dec = if c4fm { &self.c4fm } else { &self.lsm };
        let a = dec.announced();
        let id = a.identity;
        // A few TSBKs but no identity (a strong neighbour's control channel leaking in): not a
        // site of its own.
        if id.wacn.is_none() || id.system.is_none() || id.site.is_none() {
            return Heard::Traffic;
        }
        let s = dec.stats();
        let mut secondary: Vec<u64> = a.control_channel.iter().chain(&a.secondary).filter_map(|c| a.frequency(*c)).collect();
        secondary.sort_unstable();
        secondary.dedup();
        // The site's own announced control channel when the spectrum estimate is that channel.
        let announced = a.control_channel.and_then(|c| a.frequency(c)).filter(|f| f.abs_diff(freq_hz) <= 5_000);
        Heard::Control(Box::new(FoundSite {
            id: String::new(),
            freq_hz: announced.unwrap_or(freq_hz),
            level_db,
            protocol: Protocol::P25,
            modulation: Some(if c4fm { Modulation::C4fm } else { Modulation::Lsm }),
            msgs_per_s: s.tsbk_ok() as f64 / secs.max(0.1),
            ok_pct: 100.0 * s.tsbk_ok() as f64 / s.tsbk_attempts().max(1) as f64,
            identity: SiteIdentity::P25(id),
            bands: a.bands.values().map(iden_band).collect(),
            neighbours: a
                .neighbours
                .values()
                .map(|n| FoundNeighbour {
                    system: n.neighbour.system,
                    rfss: n.neighbour.rfss,
                    site: n.neighbour.site,
                    freq_hz: n.neighbour.control.freq_hz,
                })
                .collect(),
            secondary_hz: secondary,
            timeslot: None,
            existing_site: None,
            via_neighbour: false,
        }))
    }
}

/// DMR Tier III: valid control messages (CSBKs) and the site's ALOHA identity.
pub struct DmrProbe {
    dmr: DmrControl,
    events: Vec<ControlEvent>,
    /// Valid messages by timeslot (1, 2).
    slots: [u64; 2],
}

impl Default for DmrProbe {
    fn default() -> Self {
        DmrProbe { dmr: DmrControl::new(Default::default()), events: Vec::new(), slots: [0; 2] }
    }
}

impl Probe for DmrProbe {
    fn reset(&mut self) {
        self.dmr.new_system();
        self.dmr.stats = Default::default();
        self.slots = [0; 2];
    }

    fn iq(&mut self, iq: &[i16], _now: Stamp) {
        self.dmr.push(iq, &mut self.events);
        for e in self.events.drain(..) {
            if let ControlEvent::Message(line) = e {
                if let (true, Some(slot @ 1..=2)) = (line.valid, line.slot) {
                    self.slots[usize::from(slot) - 1] += 1;
                }
            }
        }
    }

    fn identified(&self) -> bool {
        self.dmr.identity().is_some()
    }

    fn messages(&self) -> u64 {
        self.dmr.stats.msgs_valid
    }

    fn heard(&self, freq_hz: u64, level_db: f32, secs: f64) -> Heard {
        let s = &self.dmr.stats;
        let Some(identity) = self.dmr.identity().filter(|_| s.msgs_valid >= 3) else {
            return if s.voice_bursts >= 6 { Heard::Traffic } else { Heard::Nothing };
        };
        let timeslot = control_slot(self.slots);
        Heard::Control(Box::new(FoundSite {
            id: String::new(),
            freq_hz: on_raster(freq_hz),
            level_db,
            protocol: Protocol::DmrTier3,
            modulation: None,
            msgs_per_s: s.msgs_valid as f64 / secs.max(0.1),
            ok_pct: 100.0 * s.msgs_valid as f64 / (s.msgs_valid + s.msgs_invalid).max(1) as f64,
            identity: SiteIdentity::Dmr(identity),
            bands: Vec::new(),
            neighbours: Vec::new(),
            secondary_hz: Vec::new(),
            timeslot,
            existing_site: None,
            via_neighbour: false,
        }))
    }
}

/// The timeslot carrying the control messages, when one clearly does (a site may send them on
/// both).
pub fn control_slot([ts1, ts2]: [u64; 2]) -> Option<u8> {
    if ts1 >= 3 * ts2.max(1) {
        Some(1)
    } else if ts2 >= 3 * ts1.max(1) {
        Some(2)
    } else {
        None
    }
}

/// A spectral estimate on the channel raster: 2.5 kHz in VHF, 6.25 kHz above (a DMR site
/// announces no frequency of its own to correct it by).
pub fn on_raster(freq_hz: u64) -> u64 {
    let step = if freq_hz < 300_000_000 { 2_500 } else { 6_250 };
    (freq_hz + step / 2) / step * step
}

/// The probes and when they last started over.
struct Listening {
    probes: Vec<Box<dyn Probe>>,
    since: Instant,
}

impl Listening {
    fn reset(&mut self) {
        for p in self.probes.iter_mut() {
            p.reset();
        }
        self.since = Instant::now();
    }
}

/// The probes fed from the control channel's IQ on a thread of their own, while the sweep moves
/// the control channel from carrier to carrier. Each move's first block starts them over: what
/// was queued before it is the last carrier's.
pub struct Probes {
    listening: Arc<Mutex<Listening>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Probes {
    pub fn start(rx: Receiver<Block>, probes: Vec<Box<dyn Probe>>) -> std::io::Result<Probes> {
        let listening = Arc::new(Mutex::new(Listening { probes, since: Instant::now() }));
        let stop = Arc::new(AtomicBool::new(false));
        let (l, s) = (listening.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("probe".into()).spawn(move || feed(&l, &s, rx))?;
        Ok(Probes { listening, stop, thread: Some(thread) })
    }

    /// The control channel moved to another carrier: forget the last one's counts now (its
    /// first block resets the probes again).
    pub fn reset(&mut self) {
        lock(&self.listening).reset();
    }

    /// Some protocol heard its control messages (`min` of them).
    pub fn messages(&self, min: u64) -> bool {
        lock(&self.listening).probes.iter().any(|p| p.messages() >= min)
    }

    pub fn identified(&self) -> bool {
        lock(&self.listening).probes.iter().any(|p| p.identified())
    }

    /// The best verdict: a control channel over a traffic channel over nothing.
    pub fn heard(&self, freq_hz: u64, level_db: f32) -> Heard {
        let l = lock(&self.listening);
        let secs = l.since.elapsed().as_secs_f64();
        let mut best = Heard::Nothing;
        for p in l.probes.iter() {
            match p.heard(freq_hz, level_db, secs) {
                h @ Heard::Control(_) => return h,
                Heard::Traffic => best = Heard::Traffic,
                Heard::Nothing => {}
            }
        }
        best
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn feed(listening: &Mutex<Listening>, stop: &AtomicBool, rx: Receiver<Block>) {
    while !stop.load(Ordering::Relaxed) {
        let block = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let mut l = lock(listening);
        if block.retuned {
            l.reset();
        }
        l.probes.iter_mut().for_each(|p| p.iq(&block.iq, block.at));
    }
}

//! DMR burst framing (SDRTrunk `DMRMessageFramer`): two 144-dibit buffers
//! that take bursts in turn, one per timeslot. The CACH at the start of each
//! base-station burst says which timeslot it is; voice superframes are
//! followed through their sync-less bursts B-F with pseudo patterns.

use super::demod::DmrSymbolSink;
use super::fec::cach::{self, Cach};
use super::sync::DmrSyncPattern;

/// Dibits in a burst, CACH included.
const BURST_DIBITS: usize = 144;
/// Dibits without a sync before a sync loss is reported (one second).
const SYNC_LOSS_DIBITS: u32 = 4800;

/// One 30 ms burst as received.
#[derive(Debug, Clone)]
pub struct DmrBurst {
    /// The burst's sync, or the voice pseudo pattern (B-F).
    pub pattern: DmrSyncPattern,
    /// 1 or 2; 0 while the timeslot is not known yet.
    pub timeslot: u8,
    /// 288 bits: CACH 0-23, payload 24-131, sync / EMB 132-179, payload 180-287.
    pub bits: [u8; 288],
    /// The decoded CACH (only meaningful when `pattern.has_cach()`).
    pub cach: Cach,
    /// Dibits received before this burst's last dibit (a stream clock).
    pub dibit_index: u64,
}

#[derive(Debug, Clone)]
pub enum FramerEvent {
    Burst(DmrBurst),
    /// Bits that went by without a burst, credited to a timeslot (0 = unknown).
    SyncLoss { timeslot: u8, bits: u32 },
}

struct Buffer {
    dibits: [u8; BURST_DIBITS],
    pointer: usize,
    pattern: DmrSyncPattern,
    timeslot: u8,
}

impl Buffer {
    fn new() -> Self {
        Buffer { dibits: [0; BURST_DIBITS], pointer: 0, pattern: DmrSyncPattern::Unknown, timeslot: 0 }
    }

    fn bits(&self) -> [u8; 288] {
        let mut b = [0u8; 288];
        for (i, d) in self.dibits.iter().enumerate() {
            b[2 * i] = (d >> 1) & 1;
            b[2 * i + 1] = d & 1;
        }
        b
    }
}

/// SDRTrunk `DMRMessageFramer`. Events queue up in `events` until drained.
pub struct DmrMessageFramer {
    a: Buffer,
    b: Buffer,
    a_active: bool,
    assembling: bool,
    dibit_counter: u32,
    dibit_index: u64,
    pub events: Vec<FramerEvent>,
}

impl Default for DmrMessageFramer {
    fn default() -> Self {
        DmrMessageFramer {
            a: Buffer::new(),
            b: Buffer::new(),
            a_active: false,
            assembling: false,
            dibit_counter: 0,
            dibit_index: 0,
            events: Vec::new(),
        }
    }
}

impl DmrMessageFramer {
    /// Takes the queued events.
    pub fn drain(&mut self) -> std::vec::Drain<'_, FramerEvent> {
        self.events.drain(..)
    }

    /// Ships the active buffer's burst (SDRTrunk `dispatchBufferA` /
    /// `dispatchBufferB`, which mirror each other).
    fn dispatch(&mut self, active_is_a: bool) {
        let (cur, other) = if active_is_a { (&mut self.a, &mut self.b) } else { (&mut self.b, &mut self.a) };
        self.dibit_counter = self.dibit_counter.saturating_sub(BURST_DIBITS as u32);
        if self.dibit_counter > 0 {
            if self.dibit_counter == BURST_DIBITS as u32 {
                // The other timeslot's burst was skipped; the CACH reassigns.
                self.events.push(FramerEvent::SyncLoss { timeslot: cur.timeslot, bits: 288 });
            } else {
                self.events.push(FramerEvent::SyncLoss { timeslot: 0, bits: self.dibit_counter * 2 });
                if self.dibit_counter > 1 && !cur.pattern.is_direct() {
                    cur.timeslot = 0;
                    other.timeslot = 0;
                    other.pattern = DmrSyncPattern::Unknown;
                }
            }
        }
        self.dibit_counter = 0;

        let bits = cur.bits();
        let cach = cach::decode(&bits);
        if cur.pattern.has_cach() && cach.valid && cur.timeslot != cach.timeslot {
            cur.timeslot = cach.timeslot;
            other.timeslot = if cur.timeslot == 1 { 2 } else { 1 };
        } else if cur.pattern.is_mobile_station_sync_pattern() {
            cur.timeslot = 1;
            other.timeslot = 2;
            if other.pattern == DmrSyncPattern::Unknown {
                other.pattern = DmrSyncPattern::DirectEmptyTimeslot;
            }
        }
        self.events.push(FramerEvent::Burst(DmrBurst {
            pattern: cur.pattern,
            timeslot: cur.timeslot,
            bits,
            cach,
            dibit_index: self.dibit_index,
        }));
        // Voice bursts B-F carry no sync: follow the superframe with pseudo
        // patterns.
        if cur.pattern.is_voice_pattern() {
            cur.pattern = cur.pattern.next_voice();
        }
        // Collect the other timeslot's next burst without waiting for a sync
        // when it is in a voice superframe (or an empty direct-mode slot).
        if other.pattern.is_voice_pattern() || other.pattern == DmrSyncPattern::DirectEmptyTimeslot {
            self.assembling = true;
            self.a_active = !active_is_a;
            other.pointer = 0;
        } else {
            self.assembling = false;
        }
    }
}

impl DmrSymbolSink for DmrMessageFramer {
    fn receive(&mut self, dibit: u8) {
        self.dibit_counter += 1;
        self.dibit_index += 1;
        if !self.assembling && self.dibit_counter >= SYNC_LOSS_DIBITS {
            self.events.push(FramerEvent::SyncLoss { timeslot: 0, bits: 2 * SYNC_LOSS_DIBITS });
            self.dibit_counter -= SYNC_LOSS_DIBITS;
            self.a.pattern = DmrSyncPattern::Unknown;
            self.b.pattern = DmrSyncPattern::Unknown;
        }
        if self.assembling {
            let a_active = self.a_active;
            let buf = if a_active { &mut self.a } else { &mut self.b };
            buf.dibits[buf.pointer] = dibit;
            buf.pointer += 1;
            if buf.pointer >= BURST_DIBITS {
                self.dispatch(a_active);
            }
        }
    }

    fn sync_detected(&mut self, pattern: DmrSyncPattern) {
        if self.assembling {
            // A burst still being collected is cut short: drop it.
            let buf = if self.a_active { &mut self.a } else { &mut self.b };
            if buf.pointer < BURST_DIBITS {
                buf.pointer = BURST_DIBITS;
                buf.pattern = DmrSyncPattern::Unknown;
            }
        }
        self.assembling = true;
        // The new burst goes to the buffer that was not active.
        self.a_active = !self.a_active;
        let (cur, other) = if self.a_active { (&mut self.a, &mut self.b) } else { (&mut self.b, &mut self.a) };
        cur.pointer = 0;
        cur.pattern = pattern;
        if pattern.is_direct() {
            if pattern.is_direct_ts1() {
                cur.timeslot = 1;
                other.timeslot = 2;
            } else if pattern.is_direct_ts2() {
                cur.timeslot = 2;
                other.timeslot = 1;
            }
            if other.pattern == DmrSyncPattern::Unknown {
                other.pattern = DmrSyncPattern::DirectEmptyTimeslot;
            }
        }
    }

    fn is_voice_super_frame(&self) -> bool {
        self.assembling
            && ((self.a_active && self.a.pattern.is_voice_pattern())
                || (!self.a_active && self.b.pattern.is_voice_pattern()))
    }
}

#[cfg(test)]
#[path = "framer_tests.rs"]
mod tests;

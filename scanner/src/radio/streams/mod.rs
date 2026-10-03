//! The radio's streams: every lane's IQ from the radio core's lane ring, read by one task
//! (`reader`) and handed to the lane's subscribers as blocks with an air time.
//!
//! Each tuning of a lane has a tag, written to the core after the NCO. The streams drop packets
//! of any other tag and the first samples of a new one (the DDC's filters still hold the old
//! channel), so a receiver only ever sees IQ of the channel it asked for; the first block of a
//! tuning says so. A block's air time is its sample index on the AD9361's clock, anchored to the
//! monotonic clock by reading the core's sample count, less the DDC's delay.

#[cfg(target_os = "linux")]
pub mod reader;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;

use crate::hardware::presets::DdcPreset;
use crate::hardware::radiocore::{Header, CONTROL_LANE};
use crate::radio::lane::Lane;
use crate::util::time::Stamp;

/// A run of a lane's IQ.
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    /// 50 kSPS interleaved I, Q.
    pub iq: Vec<i16>,
    /// Air time of the first sample.
    pub at: Stamp,
    /// The first block of a new tuning (a retune, a control channel move, a new preset).
    pub retuned: bool,
    /// Samples of this tuning are missing before this block (the core's buffer or the ring was
    /// full, the receiver was behind, or the lane was paused).
    pub gap: bool,
}

impl Block {
    #[cfg(test)]
    pub fn samples(&self) -> usize {
        self.iq.len() / 2
    }
}

/// A traffic lane's block.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneBlock {
    pub lane: Lane,
    pub block: Block,
}

/// Delivery counters of a subscription, for the diagnostics pages.
#[derive(Debug, Default)]
pub struct StreamCounters {
    pub blocks: AtomicU64,
    /// Blocks dropped because the receiver was behind.
    pub dropped: AtomicU64,
    /// Blocks after missing samples.
    pub gaps: AtomicU64,
}

/// A core lane's counters since boot.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LaneCounters {
    pub packets: u64,
    /// Packets of an older tuning, dropped.
    pub stale: u64,
    /// Samples dropped while the DDC settled after a retune.
    pub settling: u64,
    /// Packets that followed lost samples (the core's buffer full).
    pub lost: u64,
    /// Packets missing between two of the lane's (the ring lapped the reader).
    pub missed: u64,
}

/// Starts the receivers' streams; each stops when `stop` is set or its receiver hangs up.
pub trait StreamSource {
    /// The control channel's IQ (the core's lane 0).
    fn control_streams(&self, tx: SyncSender<Block>, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>);

    /// Each traffic lane's IQ.
    fn lane_streams(&self, _lanes: &[Lane], _tx: tokio::sync::mpsc::Sender<LaneBlock>, _stop: Arc<AtomicBool>) {}
}

impl StreamSource for Streams {
    fn control_streams(&self, tx: SyncSender<Block>, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>) {
        self.subscribe(CONTROL_LANE, Sink::Control(tx), stop, counters);
    }

    fn lane_streams(&self, lanes: &[Lane], tx: tokio::sync::mpsc::Sender<LaneBlock>, stop: Arc<AtomicBool>) {
        for &lane in lanes {
            self.subscribe(lane.core_lane(), Sink::Lane(lane, tx.clone()), stop.clone(), Arc::default());
        }
    }
}

enum Sink {
    Control(SyncSender<Block>),
    Lane(Lane, tokio::sync::mpsc::Sender<LaneBlock>),
}

enum Sent {
    Ok,
    Full,
    Closed,
}

impl Sink {
    fn send(&self, block: Block) -> Sent {
        match self {
            Sink::Control(tx) => match tx.try_send(block) {
                Ok(()) => Sent::Ok,
                Err(TrySendError::Full(_)) => Sent::Full,
                Err(TrySendError::Disconnected(_)) => Sent::Closed,
            },
            Sink::Lane(lane, tx) => match tx.try_send(LaneBlock { lane: *lane, block }) {
                Ok(()) => Sent::Ok,
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Sent::Full,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Sent::Closed,
            },
        }
    }
}

struct Subscriber {
    sink: Sink,
    stop: Arc<AtomicBool>,
    counters: Arc<StreamCounters>,
    /// A block was dropped: the next one follows a gap.
    gap: bool,
}

/// What a lane's DDC does to the samples, from its preset.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Ddc {
    /// AD9361 samples per output sample.
    decimation: u64,
    settle: usize,
    /// AD9361 samples.
    delay: f64,
}

#[derive(Default)]
struct LaneState {
    ddc: Option<Ddc>,
    tag: u16,
    /// Samples of the current tag still to drop.
    settle_left: usize,
    /// The next block delivered is the tuning's first.
    first: bool,
    /// The sample index the next packet of this tuning starts at.
    next_index: Option<u64>,
    sequence: Option<u16>,
    subscribers: Vec<Subscriber>,
    counters: LaneCounters,
}

/// A reading of the core's sample count beside the clocks.
#[derive(Debug, Clone, Copy)]
pub struct ClockReading {
    pub sample_count: u64,
    pub at: Stamp,
}

impl ClockReading {
    /// When the AD9361 made sample `index` (`index` at most the count), at `rate_hz`.
    fn stamp_of(&self, index: f64, rate_hz: f64) -> Stamp {
        let back = Duration::from_secs_f64(((self.sample_count as f64 - index) / rate_hz).max(0.0));
        Stamp {
            mono: self.at.mono.checked_sub(back).unwrap_or(self.at.mono),
            unix_ms: self.at.unix_ms.saturating_sub(back.as_millis() as u64),
        }
    }
}

/// One packet as the reader copied it out of the ring.
#[derive(Debug, Clone)]
pub struct LanePacket {
    pub header: Header,
    pub iq: Vec<i16>,
}

struct State {
    lanes: Vec<LaneState>,
    /// The AD9361 sample rate.
    rate_hz: f64,
}

/// The hub between the ring reader, the tuning and the receivers.
pub struct Streams {
    state: Mutex<State>,
    /// Packets that failed their checks (never written, or stale in the cache).
    faults: AtomicU64,
}

impl Streams {
    pub fn new(lanes: usize) -> Arc<Streams> {
        Arc::new(Streams {
            state: Mutex::new(State { lanes: (0..lanes).map(|_| LaneState::default()).collect(), rate_hz: 0.0 }),
            faults: AtomicU64::new(0),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A lane's DDC was loaded with `preset` (which also sets the AD9361 rate).
    pub fn configure(&self, lane: usize, preset: &DdcPreset) {
        let mut s = self.state();
        s.rate_hz = preset.sample_rate_hz as f64;
        if let Some(l) = s.lanes.get_mut(lane) {
            l.ddc = Some(Ddc {
                decimation: preset.total_decim() as u64,
                settle: preset.settle_samples(),
                delay: preset.group_delay(),
            });
        }
    }

    /// A lane's next tuning: the tag to write to the core after the NCO. From now on the lane's
    /// packets of any other tag are dropped, and the first samples of this one.
    pub fn retune(&self, lane: usize) -> u16 {
        let mut s = self.state();
        let Some(l) = s.lanes.get_mut(lane) else { return 0 };
        l.tag = l.tag.wrapping_add(1);
        l.settle_left = l.ddc.map_or(0, |d| d.settle);
        l.first = true;
        l.next_index = None;
        l.tag
    }

    /// The tag a lane's packets carry now.
    pub fn tag(&self, lane: usize) -> u16 {
        self.state().lanes.get(lane).map_or(0, |l| l.tag)
    }

    fn subscribe(&self, lane: usize, sink: Sink, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>) {
        let mut s = self.state();
        match s.lanes.get_mut(lane) {
            Some(l) => l.subscribers.push(Subscriber { sink, stop, counters, gap: false }),
            None => tracing::error!("the radio core has no lane {lane}"),
        }
    }

    pub fn counters(&self) -> Vec<LaneCounters> {
        self.state().lanes.iter().map(|l| l.counters).collect()
    }

    pub fn faults(&self) -> u64 {
        self.faults.load(Ordering::Relaxed)
    }

    pub fn fault(&self) {
        self.faults.fetch_add(1, Ordering::Relaxed);
    }

    /// Hand one poll's packets, in ring order, to their lanes' subscribers. `clock` was read
    /// after the packets.
    pub fn deliver(&self, packets: Vec<LanePacket>, clock: ClockReading) {
        let mut s = self.state();
        let rate = s.rate_hz;
        for p in packets {
            let Some(l) = s.lanes.get_mut(p.header.lane as usize) else {
                self.fault();
                continue;
            };
            let Some(block) = l.accept(p, &clock, rate) else { continue };
            l.subscribers.retain_mut(|sub| {
                if sub.stop.load(Ordering::Relaxed) {
                    return false;
                }
                let mut b = block.clone();
                b.gap |= std::mem::take(&mut sub.gap) && !b.retuned;
                let gap = b.gap;
                match sub.sink.send(b) {
                    Sent::Ok => {
                        sub.counters.blocks.fetch_add(1, Ordering::Relaxed);
                        if gap {
                            sub.counters.gaps.fetch_add(1, Ordering::Relaxed);
                        }
                        true
                    }
                    Sent::Full => {
                        sub.counters.dropped.fetch_add(1, Ordering::Relaxed);
                        sub.gap = true;
                        true
                    }
                    Sent::Closed => false,
                }
            });
        }
    }
}

impl LaneState {
    /// The block a packet gives the lane's subscribers, if any of it is of the current tuning
    /// past the settling.
    fn accept(&mut self, p: LanePacket, clock: &ClockReading, rate_hz: f64) -> Option<Block> {
        let h = p.header;
        self.counters.packets += 1;
        if let Some(prev) = self.sequence.replace(h.sequence) {
            let missed = h.sequence.wrapping_sub(prev).wrapping_sub(1);
            self.counters.missed += missed as u64;
        }
        if h.flags.lost {
            self.counters.lost += 1;
        }
        let ddc = self.ddc?;
        if h.tag != self.tag {
            self.counters.stale += 1;
            return None;
        }
        let gap = self.next_index.is_some_and(|n| n != h.sample_index);
        self.next_index = Some(h.sample_index + h.count as u64 * ddc.decimation);
        let skip = self.settle_left.min(h.count as usize);
        self.settle_left -= skip;
        self.counters.settling += skip as u64;
        if skip == h.count as usize || rate_hz <= 0.0 {
            return None;
        }
        let first_index = h.sample_index + skip as u64 * ddc.decimation;
        let retuned = std::mem::take(&mut self.first);
        Some(Block {
            iq: p.iq[2 * skip..].to_vec(),
            at: clock.stamp_of(first_index as f64 - ddc.delay, rate_hz),
            retuned,
            gap: gap && !retuned,
        })
    }
}

#[cfg(test)]
mod tests;

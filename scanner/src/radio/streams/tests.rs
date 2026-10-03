use std::sync::mpsc::{sync_channel, Receiver};
use std::time::Instant;

use super::*;
use crate::hardware::presets::find_preset;
use crate::hardware::radiocore::packet::Flags;

const DECIM: u64 = 240; // 12M

fn header(lane: u8, tag: u16, sequence: u16, sample_index: u64, count: u16) -> Header {
    Header {
        lane,
        flags: Flags::default(),
        count,
        tag,
        sample_index,
        power: 0,
        peak: 0,
        nco: 0,
        sequence,
        adc_clips: 0,
    }
}

/// A packet whose samples are their own sample numbers (I) within the tuning.
fn packet(h: Header) -> LanePacket {
    let first = (h.sample_index / DECIM) as i16;
    LanePacket { header: h, iq: (0..h.count as i16).flat_map(|k| [first.wrapping_add(k), 0]).collect() }
}

fn clock(sample_count: u64) -> ClockReading {
    ClockReading { sample_count, at: Stamp { mono: Instant::now() + Duration::from_secs(100), unix_ms: 1_000_000 } }
}

struct Control {
    streams: Arc<Streams>,
    rx: Receiver<Block>,
    counters: Arc<StreamCounters>,
}

fn control(depth: usize) -> Control {
    let streams = Streams::new(3);
    streams.configure(0, find_preset("12M").unwrap());
    let (tx, rx) = sync_channel(depth);
    let counters = Arc::new(StreamCounters::default());
    streams.control_streams(tx, Arc::new(AtomicBool::new(false)), counters.clone());
    Control { streams, rx, counters }
}

fn drain(rx: &Receiver<Block>) -> Vec<Block> {
    rx.try_iter().collect()
}

#[test]
fn only_the_current_tuning_reaches_the_receiver_past_its_settling() {
    let c = control(16);
    let settle = find_preset("12M").unwrap().settle_samples();
    let tag = c.streams.retune(0);
    c.streams.deliver(
        vec![
            packet(header(0, tag.wrapping_sub(1), 0, 0, 1008)),
            packet(header(0, tag, 1, 1008 * DECIM, 1008)),
            packet(header(0, tag, 2, 2016 * DECIM, 1008)),
        ],
        clock(4000 * DECIM),
    );
    let blocks = drain(&c.rx);
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].retuned && !blocks[1].retuned);
    assert!(!blocks[0].gap && !blocks[1].gap);
    assert_eq!(blocks[0].samples(), 1008 - settle);
    assert_eq!(blocks[0].iq[0], (1008 + settle) as i16, "the settling samples are the ones dropped");
    assert_eq!(blocks[1].samples(), 1008);
    let counters = c.streams.counters()[0];
    assert_eq!((counters.packets, counters.stale, counters.settling), (3, 1, settle as u64));
}

#[test]
fn air_time_counts_back_from_the_sample_count_less_the_ddc_delay() {
    let c = control(16);
    let p = find_preset("12M").unwrap();
    let tag = c.streams.retune(0);
    let index = 1_000 * DECIM;
    let reading = clock(index + 12_000_000); // one second of AD9361 samples later
    c.streams.deliver(vec![packet(header(0, tag, 0, index, 1008))], reading);
    let b = drain(&c.rx).remove(0);
    let first = index as f64 + (p.settle_samples() as u64 * DECIM) as f64 - p.group_delay();
    let expected = (reading.sample_count as f64 - first) / 12e6;
    let back = reading.at.mono - b.at.mono;
    assert!((back.as_secs_f64() - expected).abs() < 1e-6, "{back:?} against {expected}");
    assert!(reading.at.unix_ms - b.at.unix_ms <= 1_000);
}

#[test]
fn missing_samples_mark_a_gap() {
    let c = control(16);
    let tag = c.streams.retune(0);
    let mut lost = header(0, tag, 3, 5000 * DECIM, 1008);
    lost.flags.lost = true;
    c.streams.deliver(
        vec![
            packet(header(0, tag, 0, 0, 1008)),
            packet(header(0, tag, 1, 1008 * DECIM, 1008)),
            // Sequence 2 never arrives (the ring lapped the reader).
            packet(lost),
        ],
        clock(10_000 * DECIM),
    );
    let blocks = drain(&c.rx);
    assert_eq!(blocks.iter().map(|b| b.gap).collect::<Vec<_>>(), vec![false, false, true]);
    let counters = c.streams.counters()[0];
    assert_eq!((counters.missed, counters.lost), (1, 1));
}

#[test]
fn a_receiver_behind_loses_blocks_and_sees_the_gap() {
    let c = control(1);
    let tag = c.streams.retune(0);
    let packets = (0..3).map(|k| packet(header(0, tag, k, k as u64 * 1008 * DECIM, 1008))).collect();
    c.streams.deliver(packets, clock(10_000 * DECIM));
    assert_eq!(drain(&c.rx).len(), 1);
    c.streams.deliver(vec![packet(header(0, tag, 3, 3 * 1008 * DECIM, 1008))], clock(10_000 * DECIM));
    assert!(drain(&c.rx)[0].gap);
    assert_eq!(c.counters.dropped.load(Ordering::Relaxed), 2);
    assert_eq!(c.counters.gaps.load(Ordering::Relaxed), 1);
}

#[test]
fn a_resumed_lane_keeps_its_tuning_and_reports_the_pause() {
    let c = control(16);
    let tag = c.streams.retune(0);
    c.streams.deliver(vec![packet(header(0, tag, 0, 0, 1008))], clock(10_000 * DECIM));
    // Paused, then enabled again on the same tag: no settling, no retune, a gap.
    c.streams.deliver(vec![packet(header(0, tag, 1, 50_000 * DECIM, 1008))], clock(60_000 * DECIM));
    let blocks = drain(&c.rx);
    assert_eq!(blocks[1].samples(), 1008);
    assert!(!blocks[1].retuned && blocks[1].gap);
}

#[test]
fn traffic_lanes_get_their_own_packets() {
    let streams = Streams::new(3);
    let p = find_preset("12M").unwrap();
    for lane in 0..3 {
        streams.configure(lane, p);
    }
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let stop = Arc::new(AtomicBool::new(false));
    streams.lane_streams(&Lane::ALL, tx, stop.clone());
    let tags = [streams.retune(0), streams.retune(1), streams.retune(2)];
    let packets = (0..3u8).map(|lane| packet(header(lane, tags[lane as usize], 0, 0, 1008))).collect();
    streams.deliver(packets, clock(10_000 * DECIM));
    let mut lanes = Vec::new();
    while let Ok(b) = rx.try_recv() {
        lanes.push(b.lane);
    }
    assert_eq!(lanes, vec![Lane::One, Lane::Two]);
    // A stopped subscription is dropped at the next delivery.
    stop.store(true, Ordering::Relaxed);
    streams.deliver(vec![packet(header(1, tags[1], 1, 1008 * DECIM, 1008))], clock(10_000 * DECIM));
    assert!(rx.try_recv().is_err());
}

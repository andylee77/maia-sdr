//! The board's stream readers: the control DDC's IQ ring and the dibit rings, polled every 40 ms,
//! and each lane's gateware status, polled every 16 ms (fine enough not to miss NIDs 14 ms
//! apart).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::dibit_ring::{DibitClock, RingTracker};
use super::{Input, LaneInput, StreamCounters, StreamSource, Wants};
use crate::hardware::p25core::rings::DIBITS_PER_BYTE;
use crate::hardware::p25core::{DibitRing, IqRing, Lane, P25Core};
use crate::radio::hw::Hardware;
use crate::util::time::Stamp;

const POLL: Duration = Duration::from_millis(40);
const STATUS_POLL: Duration = Duration::from_millis(16);

impl StreamSource for Hardware {
    fn control_streams(
        &self,
        wants: Wants,
        tx: SyncSender<Input>,
        stop: Arc<AtomicBool>,
        counters: Arc<StreamCounters>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();
        if wants.dibits {
            tasks.push(tokio::spawn(dibits(self.core.clone(), DibitRing::Control, tx.clone(), stop.clone(), counters.clone())));
        }
        if wants.iq {
            tasks.push(tokio::spawn(iq(self.core.clone(), IqRing::Control, tx, stop, counters)));
        }
        tasks
    }

    fn lane_streams(&self, lanes: &[Lane], tx: tokio::sync::mpsc::Sender<LaneInput>, stop: Arc<AtomicBool>) -> Vec<tokio::task::JoinHandle<()>> {
        let mut tasks = Vec::new();
        for &lane in lanes {
            tasks.push(tokio::spawn(lane_dibits(self.core.clone(), lane, tx.clone(), stop.clone())));
            tasks.push(tokio::spawn(lane_status(self.core.clone(), lane, tx.clone(), stop.clone())));
        }
        tasks
    }
}

fn ticker() -> tokio::time::Interval {
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick
}

async fn iq(core: Arc<Mutex<P25Core>>, ring: IqRing, tx: SyncSender<Input>, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>) {
    // What completed before this reader started is stale.
    let _ = core.lock().await.read_iq(ring);
    let mut tick = ticker();
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let chunks: Vec<Vec<i16>> = {
            let mut c = core.lock().await;
            c.read_iq(ring)
                .iter()
                .map(|b| b.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect())
                .collect()
        };
        for chunk in chunks {
            counters.iq_chunks.fetch_add(1, Ordering::Relaxed);
            match tx.try_send(Input::Iq(chunk)) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    counters.iq_dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
    }
}

async fn dibits(core: Arc<Mutex<P25Core>>, ring: DibitRing, tx: SyncSender<Input>, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>) {
    let geometry = core.lock().await.dibit_geometry(ring);
    let Some(geometry) = geometry.filter(|g| g.is_valid()) else {
        tracing::error!("{ring:?} dibit ring geometry {geometry:?} cannot be tracked; no dibits");
        return;
    };
    let mut tracker = RingTracker::new(geometry);
    let mut reset = true;
    let mut tick = ticker();
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let mut bytes = Vec::new();
        let (result, copied) = {
            let c = core.lock().await;
            let Some(snapshot) = c.dibit_snapshot(ring) else { return };
            let result = tracker.poll(&snapshot, true);
            let copied = match result.deliver {
                Some((start, end)) if end > start => match c.copy_dibits(ring, start, end, &mut bytes) {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!("{ring:?} dibit copy failed: {e:#}");
                        false
                    }
                },
                _ => true,
            };
            (result, copied)
        };
        if let Some(r) = result.resync {
            tracing::info!("{ring:?} dibit ring resync ({}): skipped {} bytes", r.reason.as_str(), r.to_pos.saturating_sub(r.from_pos));
            counters.dibit_resyncs.fetch_add(1, Ordering::Relaxed);
            reset = true;
        }
        if !copied {
            counters.dibit_lost.fetch_add(1, Ordering::Relaxed);
            reset = true;
            continue;
        }
        if bytes.is_empty() {
            continue;
        }
        let n = bytes.len() as u64;
        match tx.try_send(Input::Dibits { bytes, reset }) {
            Ok(()) => {
                counters.dibit_bytes.fetch_add(n, Ordering::Relaxed);
                reset = false;
            }
            Err(TrySendError::Full(_)) => {
                counters.dibit_lost.fetch_add(1, Ordering::Relaxed);
                reset = true;
            }
            Err(TrySendError::Disconnected(_)) => return,
        }
    }
}

async fn lane_dibits(core: Arc<Mutex<P25Core>>, lane: Lane, tx: tokio::sync::mpsc::Sender<LaneInput>, stop: Arc<AtomicBool>) {
    let ring = DibitRing::Lane(lane);
    let geometry = core.lock().await.dibit_geometry(ring);
    let Some(geometry) = geometry.filter(|g| g.is_valid()) else {
        tracing::error!("{lane} dibit ring geometry {geometry:?} cannot be tracked; no dibits");
        return;
    };
    let mut tracker = RingTracker::new(geometry);
    let mut clock = DibitClock::new();
    let mut reset = true;
    let mut tick = ticker();
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let mut bytes = Vec::new();
        let (result, copied, running) = {
            let c = core.lock().await;
            let Some(snapshot) = c.dibit_snapshot(ring) else { return };
            let result = tracker.poll(&snapshot, true);
            if let Some(abs) = result.abs_next {
                clock.observe_next(snapshot.t_us, abs, snapshot.chain_enabled);
            }
            let copied = match result.deliver {
                Some((start, end)) if end > start => c.copy_dibits(ring, start, end, &mut bytes).is_ok(),
                _ => true,
            };
            (result, copied, snapshot.chain_enabled)
        };
        if result.resync.is_some() || !copied {
            reset = true;
        }
        let Some((start, _)) = result.deliver.filter(|_| copied && !bytes.is_empty() && running) else {
            continue;
        };
        let input = LaneInput::Dibits { lane, bytes, first: start * DIBITS_PER_BYTE, reset, clock: clock.view() };
        match tx.try_send(input) {
            Ok(()) => reset = false,
            Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => reset = true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return,
        }
    }
}

async fn lane_status(core: Arc<Mutex<P25Core>>, lane: Lane, tx: tokio::sync::mpsc::Sender<LaneInput>, stop: Arc<AtomicBool>) {
    let mut tick = tokio::time::interval(STATUS_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    while !stop.load(Ordering::Relaxed) {
        tick.tick().await;
        let read = {
            let c = core.lock().await;
            let Some(chain) = c.lane(lane) else { return };
            let status = chain.status();
            status.nid_event.then(|| (status.nid_valid, chain.nid()))
        };
        let Some((valid, (nac, duid))) = read else { continue };
        if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) =
            tx.try_send(LaneInput::Nid { lane, duid, nac, valid, at: Stamp::now() })
        {
            return;
        }
    }
}

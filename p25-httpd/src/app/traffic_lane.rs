//! Change 066: the per-chain objects of one traffic chain ("lane"),
//! built the same way for every chain (portable).
//!
//! Each lane has its own call state machine (`TrafficChain`), framer /
//! voice decoder, IMBE forwarder (with its vocoder queue) and snapshot of
//! its open call. Site-wide state is shared through `ForwarderShared`.

use std::sync::Arc;

use tokio::sync::{Mutex, RwLock};

use crate::app::dibit_airtime::DibitDelivery;
use crate::app::grant_follower::{new_active_call_shared, ActiveCallShared};
use crate::app::imbe_forwarder::{ForwarderShared, ImbeBatch, ImbeBatchRx, ImbeForwarder};
use crate::audio::CallBoundaryTx;
use crate::hardware::traffic_lane::Lane;
use crate::protocol::p25::control_channel::ControlChannelDecoder;
use crate::protocol::p25::traffic_chain::TrafficChain;
use crate::services::event_log::EventLog;

/// Vocoder queue depth: 32 LDU batches (~5.8 s audio) absorbs jitter
/// (sized up from 16 on 2026-04-24 after ~4 % IMBE drops from overflow).
pub const IMBE_QUEUE: usize = 32;

#[derive(Clone)]
pub struct TrafficLane {
    pub lane: Lane,
    pub chain: Arc<Mutex<TrafficChain>>,
    pub decoder: Arc<RwLock<ControlChannelDecoder>>,
    pub forwarder: Arc<ImbeForwarder>,
    pub active_call: ActiveCallShared,
}

/// Everything a lane needs from the rest of the app.
pub struct LaneDeps<'a> {
    pub shared: &'a ForwarderShared,
    pub delivery: &'a DibitDelivery,
    pub boundary_tx: &'a CallBoundaryTx,
    pub event_tx: &'a tokio::sync::broadcast::Sender<String>,
    pub event_log: &'a Arc<EventLog>,
    pub rx_lo: u64,
    pub sample_rate_hz: u64,
}

/// Build a lane: its forwarder (voice handler of its decoder, fed by its
/// dibit ring's air-time epochs) and the receiver of its vocoder queue.
pub fn build_lane(lane: Lane, d: &LaneDeps<'_>) -> (TrafficLane, ImbeBatchRx) {
    let (imbe_tx, imbe_rx) = tokio::sync::mpsc::channel::<ImbeBatch>(IMBE_QUEUE);
    let forwarder = Arc::new(ImbeForwarder::with_lane(imbe_tx, lane, d.shared));
    forwarder.set_airtime(d.delivery.traffic_ring(lane).clone());
    // Lets on_tdu_lc publish Motorola TALK_COMPLETE source stamps.
    forwarder.set_boundary_tx(d.boundary_tx.clone());
    forwarder.set_ws_event_tx(d.event_tx.clone());
    // TDULC LCW parses emit Activity-feed entries.
    forwarder.set_event_log(d.event_log.clone());

    let mut decoder = ControlChannelDecoder::new();
    decoder.set_event_tx(d.event_tx.clone());
    // The forwarder as voice handler: counts events and pushes frame
    // batches to the lane's vocoder via try_send.
    decoder.set_voice_handler(forwarder.clone());
    // Every successful NID decode emits one `Duid` entry labelled with
    // the chain (`/api/log?category=duid`).
    decoder.event_log = Some(d.event_log.clone());
    decoder.chain_label = lane.label();

    let chain = TrafficChain::new(d.rx_lo, d.sample_rate_hz);
    let tl = TrafficLane {
        lane,
        chain: Arc::new(Mutex::new(chain)),
        decoder: Arc::new(RwLock::new(decoder)),
        forwarder,
        active_call: new_active_call_shared(),
    };
    (tl, imbe_rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::dibit_airtime::DeliveryMode;

    #[tokio::test]
    async fn lanes_share_site_state_and_label_their_output() {
        let shared = ForwarderShared::default();
        let delivery = DibitDelivery::new(DeliveryMode::Airtime, 40);
        let boundary_tx = crate::audio::call_boundary_channel();
        let (event_tx, _) = tokio::sync::broadcast::channel(4);
        let event_log = Arc::new(EventLog::new(16));
        let d = LaneDeps {
            shared: &shared,
            delivery: &delivery,
            boundary_tx: &boundary_tx,
            event_tx: &event_tx,
            event_log: &event_log,
            rx_lo: 858_100_000,
            sample_rate_hz: 8_000_000,
        };
        let (one, _rx1) = build_lane(Lane::One, &d);
        let (two, _rx2) = build_lane(Lane::Two, &d);
        assert_eq!((one.forwarder.lane, two.forwarder.lane), (Lane::One, Lane::Two));
        assert_eq!(one.decoder.read().await.chain_label, "traffic");
        assert_eq!(two.decoder.read().await.chain_label, "traffic2");
        // Encrypted talkgroups and per-call counters are site-wide.
        one.forwarder.encrypted_tg_history.lock().unwrap().insert(402);
        assert!(two.forwarder.encrypted_tg_history.lock().unwrap().contains(&402));
        one.forwarder.call_counts.update(7, |c| c.imbe_extracted += 9);
        assert_eq!(two.forwarder.call_counts.get(7).unwrap().imbe_extracted, 9);
        // Each lane's own state is its own.
        one.forwarder.current_talkgroup.store(300, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(two.forwarder.current_talkgroup.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}

/// Change 071a: no traffic chain is locked to a call.
pub async fn all_idle(lanes: &[TrafficLane]) -> bool {
    for lane in lanes {
        if lane.chain.lock().await.current_talkgroup().is_some() {
            return false;
        }
    }
    true
}

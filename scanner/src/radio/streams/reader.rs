//! The lane ring's reader: every 20 ms (a packet is 20.16 ms of one lane), the packets of the
//! sub-buffers completed since the last poll, then the core's sample count to place them in time.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use super::{ClockReading, LanePacket, Streams};
use crate::hardware::radiocore::RadioCore;
use crate::util::time::Stamp;

const POLL: Duration = Duration::from_millis(20);

/// Runs until the process ends.
pub async fn run(core: Arc<Mutex<RadioCore>>, streams: Arc<Streams>) {
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut faults_logged = 0u64;
    loop {
        tick.tick().await;
        let mut packets = Vec::new();
        let clock = {
            let mut c = core.lock().await;
            c.read_packets(|p| match p {
                Ok(p) => packets.push(LanePacket { header: p.header, iq: p.iq().collect() }),
                Err(fault) => {
                    streams.fault();
                    if faults_logged < 20 {
                        faults_logged += 1;
                        tracing::warn!("lane ring packet refused: {fault:?}");
                    }
                }
            });
            ClockReading { sample_count: c.sample_count(), at: Stamp::now() }
        };
        streams.deliver(packets, clock);
    }
}

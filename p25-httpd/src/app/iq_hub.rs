//! Change 071b: one reader per DDC IQ ring, fanned out.
//!
//! Reading a DMA ring advances a cursor inside `IpCore`, so two readers
//! took each other's sub-buffers (the narrowband spectrum, the IQ dumps).
//! The software C4FM demodulator needs every sample, so one task now
//! reads each ring and publishes the samples: a broadcast of chunks for
//! stream consumers and a short history for snapshot readers.
//!
//! Samples are interleaved 16-bit I, Q at the DDC output rate (50 kSPS).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast;

/// Broadcast depth in chunks (a chunk is one 32 KB DMA sub-buffer,
/// ~164 ms at 50 kSPS).
const CHUNKS: usize = 64;

pub struct IqHub {
    tx: broadcast::Sender<Arc<[i16]>>,
    history: Mutex<VecDeque<i16>>,
    history_cap: usize,
    /// Complex samples published.
    pub samples: AtomicU64,
    pub chunks: AtomicU64,
}

impl IqHub {
    /// `history_s` seconds of IQ at `rate_hz` are kept for snapshots.
    pub fn new(history_s: f64, rate_hz: f64) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHUNKS);
        Arc::new(IqHub {
            tx,
            history: Mutex::new(VecDeque::new()),
            history_cap: (history_s * rate_hz) as usize * 2,
            samples: AtomicU64::new(0),
            chunks: AtomicU64::new(0),
        })
    }

    pub fn publish(&self, iq: Vec<i16>) {
        if let Ok(mut h) = self.history.lock() {
            h.extend(iq.iter().copied());
            let excess = h.len().saturating_sub(self.history_cap);
            h.drain(..excess);
        }
        self.samples.fetch_add(iq.len() as u64 / 2, Ordering::Relaxed);
        self.chunks.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(iq.into());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<[i16]>> {
        self.tx.subscribe()
    }

    /// `n` complex samples published after this call (fewer if
    /// `deadline` passes first), interleaved I, Q.
    pub async fn collect(&self, n: usize, deadline: Duration) -> Vec<i16> {
        let mut rx = self.subscribe();
        let mut out = Vec::with_capacity(n * 2);
        let end = tokio::time::Instant::now() + deadline;
        while out.len() < n * 2 {
            match tokio::time::timeout_at(end, rx.recv()).await {
                Ok(Ok(chunk)) => out.extend_from_slice(&chunk),
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => break,
            }
        }
        out.truncate(n * 2);
        out
    }
}

/// Interleaved i16 IQ as the little-endian bytes the DMA ring holds.
pub fn to_bytes(iq: &[i16]) -> Vec<u8> {
    iq.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Which DDC IQ ring.
#[derive(Debug, Clone, Copy)]
pub enum IqRing {
    Control,
    /// Traffic chain 1 (chain 2 has no IQ tap).
    Traffic,
}

/// Read `ring` every 40 ms and publish its new sub-buffers.
#[cfg(target_os = "linux")]
pub fn spawn_iq_reader(
    core: Arc<tokio::sync::Mutex<crate::hardware::fpga::IpCore>>,
    ring: IqRing,
    hub: Arc<IqHub>,
) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(40));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let chunks: Vec<Vec<i16>> = {
                let mut core = core.lock().await;
                let bufs = match ring {
                    IqRing::Control => core.read_iq_buffers(),
                    IqRing::Traffic => core.read_traffic_iq_buffers(),
                };
                bufs.iter()
                    .map(|b| b.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect())
                    .collect()
            };
            for c in chunks {
                hub.publish(c);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn publish_collect_and_history() {
        let hub = IqHub::new(0.001, 50_000.0); // 50 samples of history
        let h2 = hub.clone();
        let t = tokio::spawn(async move { h2.collect(120, Duration::from_secs(2)).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        for k in 0..4 {
            hub.publish(vec![k as i16; 80]); // 40 complex samples each
        }
        let got = t.await.unwrap();
        assert_eq!(got.len(), 240);
        assert_eq!(&got[..2], &[0, 0]);
        assert_eq!(hub.samples.load(Ordering::Relaxed), 160);
        assert_eq!(hub.history.lock().unwrap().len(), 100);
        assert_eq!(to_bytes(&[1, -2]), vec![1, 0, 0xFE, 0xFF]);
    }
}

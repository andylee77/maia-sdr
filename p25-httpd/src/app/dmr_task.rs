//! Change 075: DMR receive on the control IQ hub, first as a monitor.
//!
//! Off by default (`PUT /api/dmr?enabled=1`). When on, a thread runs the
//! software DMR demodulator and framer (`protocol::dmr`, SDRTrunk's) on the
//! control DDC's IQ, beside the P25 decoders, and keeps the counters
//! `/api/dmr` serves: syncs, bursts per timeslot, CACH, voice, the carrier
//! offset the equaliser learned and the CPU it costs.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use crate::protocol::dmr::framer::FramerEvent;

/// Counters of the DMR path. Demodulator figures count from the last reset
/// (enable or retune); burst figures from the last enable.
#[derive(Default)]
pub struct DmrRuntime {
    pub enabled: AtomicBool,
    pub chunks: AtomicU64,
    pub lagged: AtomicU64,
    pub resets: AtomicU64,
    /// Share of one core, x100.
    pub cpu_centi_pct: AtomicU64,
    /// Equaliser balance (the carrier offset), milliradians per symbol.
    pub balance_mrad: AtomicI64,
    /// Equaliser gain, x1000.
    pub gain_milli: AtomicU64,
    pub coarse_syncs: AtomicU64,
    pub fine_syncs: AtomicU64,
    pub fine_sync_losses: AtomicU64,
    /// Bursts by timeslot: unknown, 1, 2.
    pub bursts: [AtomicU64; 3],
    pub voice_bursts: AtomicU64,
    pub cach_ok: AtomicU64,
    pub cach_bad: AtomicU64,
    pub sync_loss_bits: AtomicU64,
    pub last_burst_unix_ms: AtomicU64,
}

impl DmrRuntime {
    /// Zero the counters (a new enable).
    pub fn clear(&self) {
        for c in [
            &self.chunks,
            &self.lagged,
            &self.resets,
            &self.cpu_centi_pct,
            &self.gain_milli,
            &self.coarse_syncs,
            &self.fine_syncs,
            &self.fine_sync_losses,
            &self.voice_bursts,
            &self.cach_ok,
            &self.cach_bad,
            &self.sync_loss_bits,
            &self.last_burst_unix_ms,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        for b in &self.bursts {
            b.store(0, Ordering::Relaxed);
        }
        self.balance_mrad.store(0, Ordering::Relaxed);
    }

    /// Counts what the framer produced.
    pub fn count(&self, event: &FramerEvent) {
        match event {
            FramerEvent::Burst(b) => {
                self.bursts[(b.timeslot as usize).min(2)].fetch_add(1, Ordering::Relaxed);
                if b.pattern.is_voice_pattern() {
                    self.voice_bursts.fetch_add(1, Ordering::Relaxed);
                }
                if b.pattern.has_cach() {
                    let c = if b.cach.valid { &self.cach_ok } else { &self.cach_bad };
                    c.fetch_add(1, Ordering::Relaxed);
                }
                self.last_burst_unix_ms.store(crate::app::now_unix_ms(), Ordering::Relaxed);
            }
            FramerEvent::SyncLoss { bits, .. } => {
                self.sync_loss_bits.fetch_add(*bits as u64, Ordering::Relaxed);
            }
        }
    }

    /// The `/api/dmr` body.
    pub fn snapshot(&self) -> serde_json::Value {
        let l = |a: &AtomicU64| a.load(Ordering::Relaxed);
        let balance = self.balance_mrad.load(Ordering::Relaxed) as f64 / 1000.0;
        let (ok, bad) = (l(&self.cach_ok), l(&self.cach_bad));
        serde_json::json!({
            "enabled": self.enabled.load(Ordering::Relaxed),
            "cpu_pct": l(&self.cpu_centi_pct) as f64 / 100.0,
            "chunks": l(&self.chunks),
            "lagged": l(&self.lagged),
            "resets": l(&self.resets),
            "demod": {
                "coarse_syncs": l(&self.coarse_syncs),
                "fine_syncs": l(&self.fine_syncs),
                "fine_sync_losses": l(&self.fine_sync_losses),
                "balance_rad_per_symbol": balance,
                // The balance corrects the phase per symbol: offset = -balance x 4800 / 2 pi.
                "carrier_offset_hz": (-balance * 4800.0 / std::f64::consts::TAU).round(),
                "gain": l(&self.gain_milli) as f64 / 1000.0,
            },
            "bursts": { "ts_unknown": l(&self.bursts[0]), "ts1": l(&self.bursts[1]), "ts2": l(&self.bursts[2]) },
            "voice_bursts": l(&self.voice_bursts),
            "cach": { "ok": ok, "bad": bad, "ok_pct": if ok + bad > 0 { ok as f64 * 100.0 / (ok + bad) as f64 } else { 0.0 } },
            "sync_loss_bits": l(&self.sync_loss_bits),
            "last_burst_unix_ms": l(&self.last_burst_unix_ms),
        })
    }
}

/// The DMR thread: control IQ -> DMR demodulator -> framer -> counters.
#[cfg(target_os = "linux")]
pub fn spawn_dmr_control(
    hub: std::sync::Arc<crate::app::iq_hub::IqHub>,
    control_freq: std::sync::Arc<AtomicU64>,
    rx_lo: std::sync::Arc<AtomicI64>,
    rt: std::sync::Arc<DmrRuntime>,
) {
    use crate::protocol::dmr::demod::DmrDemodulator;
    use crate::protocol::dmr::framer::DmrMessageFramer;
    use tokio::sync::broadcast::error::RecvError;
    let spawned = std::thread::Builder::new().name("dmr-cc".into()).spawn(move || {
        let mut rx = hub.subscribe();
        let mut chain: Option<(DmrDemodulator, DmrMessageFramer)> = None;
        let mut tuned = (0u64, 0i64);
        let mut busy = std::time::Duration::ZERO;
        let mut since = std::time::Instant::now();
        loop {
            let chunk = match rx.blocking_recv() {
                Ok(c) => c,
                Err(RecvError::Lagged(_)) => {
                    if chain.is_some() {
                        rt.lagged.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            if !rt.enabled.load(Ordering::Relaxed) {
                chain = None;
                continue;
            }
            // Enabled, or retuned: start from scratch (timing and equaliser
            // belong to the old channel).
            let now_tuned = (control_freq.load(Ordering::Relaxed), rx_lo.load(Ordering::Relaxed));
            if chain.is_none() || now_tuned != tuned {
                if chain.is_some() {
                    rt.resets.fetch_add(1, Ordering::Relaxed);
                }
                tuned = now_tuned;
                chain = Some((DmrDemodulator::new(), DmrMessageFramer::default()));
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
            let Some((demod, framer)) = chain.as_mut() else { continue };
            let t0 = std::time::Instant::now();
            demod.process_iq_i16(&chunk, framer);
            for event in framer.drain() {
                rt.count(&event);
            }
            busy += t0.elapsed();
            rt.chunks.fetch_add(1, Ordering::Relaxed);
            let s = demod.symbols.stats;
            rt.coarse_syncs.store(s.coarse_syncs, Ordering::Relaxed);
            rt.fine_syncs.store(s.fine_syncs, Ordering::Relaxed);
            rt.fine_sync_losses.store(s.fine_sync_losses, Ordering::Relaxed);
            if since.elapsed() >= std::time::Duration::from_secs(5) {
                let pct = busy.as_secs_f64() / since.elapsed().as_secs_f64() * 100.0;
                rt.cpu_centi_pct.store((pct * 100.0) as u64, Ordering::Relaxed);
                rt.balance_mrad.store((demod.symbols.equalizer_balance() * 1000.0) as i64, Ordering::Relaxed);
                rt.gain_milli.store((demod.symbols.equalizer_gain() * 1000.0) as u64, Ordering::Relaxed);
                busy = std::time::Duration::ZERO;
                since = std::time::Instant::now();
            }
        }
        tracing::warn!("dmr control thread exiting (IQ hub closed)");
    });
    if let Err(e) = spawned {
        tracing::error!("dmr control thread not started: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::dmr::fec::cach::Cach;
    use crate::protocol::dmr::framer::DmrBurst;
    use crate::protocol::dmr::sync::DmrSyncPattern;

    fn burst(pattern: DmrSyncPattern, timeslot: u8, cach_valid: bool) -> FramerEvent {
        FramerEvent::Burst(DmrBurst {
            pattern,
            timeslot,
            bits: [0; 288],
            cach: Cach { valid: cach_valid, busy: false, timeslot, lcss: 0, payload: [0; 17] },
            dibit_index: 0,
        })
    }

    #[test]
    fn counts_bursts_cach_and_voice() {
        let rt = DmrRuntime::default();
        rt.count(&burst(DmrSyncPattern::BaseStationData, 1, true));
        rt.count(&burst(DmrSyncPattern::BaseStationVoice, 2, true));
        rt.count(&burst(DmrSyncPattern::BsVoiceFrameB, 2, false));
        rt.count(&FramerEvent::SyncLoss { timeslot: 0, bits: 288 });
        let s = rt.snapshot();
        assert_eq!(s["bursts"]["ts1"], 1);
        assert_eq!(s["bursts"]["ts2"], 2);
        assert_eq!(s["voice_bursts"], 2);
        assert_eq!(s["cach"]["ok"], 2);
        assert_eq!(s["cach"]["bad"], 1);
        assert_eq!(s["sync_loss_bits"], 288);
        rt.clear();
        assert_eq!(rt.snapshot()["bursts"]["ts2"], 0);
    }

    #[test]
    fn carrier_offset_from_balance() {
        let rt = DmrRuntime::default();
        // 0.49 rad/symbol (unit A at 454 MHz before 074c) is about -374 Hz.
        rt.balance_mrad.store(490, Ordering::Relaxed);
        assert_eq!(rt.snapshot()["demod"]["carrier_offset_hz"], -374.0);
    }
}

//! The tuner's hardware: the AD9361 and the radio core on the board; on a development host, a
//! stand-in with no radio so the services and the API still run.
//!
//! Every move of a lane's NCO that starts a new tuning takes a new tag from the streams and
//! writes it after the NCO, so the streams know which packets are of the new tuning.

use super::streams::StreamSource;
use super::tuner::{RadioHw, Readback};
use crate::hardware::ad9361::GainMode;
use crate::hardware::presets::DdcPreset;
use crate::radio::lane::Lane;

#[cfg(target_os = "linux")]
pub use board::Board as Hardware;
#[cfg(not(target_os = "linux"))]
pub use host::NoRadio as Hardware;

#[cfg(target_os = "linux")]
mod board {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::SyncSender;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use tokio::sync::Mutex;

    use super::*;
    use crate::hardware::ad9361::Ad9361;
    use crate::hardware::radiocore::{nco_to_freq, LaneDdc, RadioCore, CONTROL_LANE};
    use crate::radio::streams::{Block, LaneBlock, StreamCounters, Streams};
    use crate::radio::tuner::LaneReadback;

    pub struct Board {
        pub ad9361: Ad9361,
        pub core: Arc<Mutex<RadioCore>>,
        pub streams: Arc<Streams>,
    }

    fn lane_of(core: &RadioCore, n: usize) -> Result<LaneDdc<'_>> {
        core.lane(n).with_context(|| format!("the radio core has no lane {n}"))
    }

    impl Board {
        /// Start a new tuning of a core lane whose NCO was just written: its packets on, with a
        /// new tag.
        fn start_tuning(&self, lane: &LaneDdc<'_>) {
            let tag = self.streams.retune(lane.number());
            lane.set_packets(true, tag);
        }
    }

    impl RadioHw for Board {
        async fn set_lo(&self, hz: u64) -> Result<()> {
            self.ad9361.set_lo_hz(hz).await
        }

        async fn set_rate(&self, sample_rate_hz: u32, rf_bandwidth_hz: u32) -> Result<()> {
            self.ad9361.set_sample_rate_hz(sample_rate_hz).await?;
            self.ad9361.set_rf_bandwidth_hz(rf_bandwidth_hz).await
        }

        async fn set_gain(&self, mode: GainMode, db: Option<f64>) -> Result<()> {
            self.ad9361.set_gain_mode(mode).await?;
            match (mode, db) {
                (GainMode::Manual, Some(db)) => self.ad9361.set_gain_db(db).await,
                _ => Ok(()),
            }
        }

        async fn configure_control(&self, preset: &'static DdcPreset, nco_hz: f64) -> Result<()> {
            let core = self.core.lock().await;
            let c = lane_of(&core, CONTROL_LANE)?;
            c.configure_ddc(nco_hz, preset)?;
            c.set_input(true);
            self.streams.configure(CONTROL_LANE, preset);
            self.start_tuning(&c);
            Ok(())
        }

        async fn set_control_nco(&self, nco_hz: f64, sample_rate_hz: u32) -> Result<()> {
            let core = self.core.lock().await;
            let c = lane_of(&core, CONTROL_LANE)?;
            c.set_nco(nco_hz, sample_rate_hz as f64)?;
            self.start_tuning(&c);
            Ok(())
        }

        async fn configure_lanes(&self, preset: &'static DdcPreset) -> Result<()> {
            let core = self.core.lock().await;
            for lane in Lane::ALL {
                let Some(l) = core.lane(lane.core_lane()) else { continue };
                l.configure_ddc(0.0, preset)?;
                l.set_input(true);
                self.streams.configure(lane.core_lane(), preset);
                // Idle until a retune: whatever its tag held is no longer tuned.
                l.set_packets(false, self.streams.retune(lane.core_lane()));
            }
            Ok(())
        }

        async fn retune_lane(&self, lane: Lane, nco_hz: f64, sample_rate_hz: u32, reset: bool) -> Result<()> {
            let core = self.core.lock().await;
            let l = lane_of(&core, lane.core_lane())?;
            l.set_nco(nco_hz, sample_rate_hz as f64)?;
            if reset {
                self.start_tuning(&l);
            } else {
                l.set_packets(true, self.streams.tag(lane.core_lane()));
            }
            Ok(())
        }

        async fn pause_lane(&self, lane: Lane) -> Result<()> {
            let core = self.core.lock().await;
            let l = lane_of(&core, lane.core_lane())?;
            l.set_packets(false, self.streams.tag(lane.core_lane()));
            Ok(())
        }

        async fn spectrum(&self) -> Option<Vec<u8>> {
            self.core.lock().await.read_spectrum().map(<[u8]>::to_vec)
        }

        async fn readback(&self, sample_rate_hz: u32) -> Readback {
            let sr = sample_rate_hz as f64;
            let mut r = Readback {
                lo_hz: self.ad9361.lo_hz().await.ok(),
                gain_db: self.ad9361.gain_db().await.ok(),
                rssi_db: self.ad9361.rssi_db().await.ok(),
                gain_mode: self.ad9361.gain_mode().await.ok().map(GainMode::as_str),
                lane_counters: self.streams.counters(),
                packet_faults: self.streams.faults(),
                ..Default::default()
            };
            let core = self.core.lock().await;
            let lane = |n: usize| {
                core.lane(n).map(|l| {
                    let (packets, tag) = l.packets();
                    LaneReadback { nco_hz: (sr > 0.0).then(|| nco_to_freq(l.nco_word(), sr)), packets, tag }
                })
            };
            r.control = lane(CONTROL_LANE);
            r.lanes = Lane::ALL.map(|l| lane(l.core_lane()));
            r.ring = Some(core.ring_status());
            r.sample_count = Some(core.sample_count());
            r.adc_clips = Some(core.adc_clips());
            r
        }
    }

    impl StreamSource for Board {
        fn control_streams(&self, tx: SyncSender<Block>, stop: Arc<AtomicBool>, counters: Arc<StreamCounters>) {
            self.streams.control_streams(tx, stop, counters);
        }

        fn lane_streams(&self, lanes: &[Lane], tx: tokio::sync::mpsc::Sender<LaneBlock>, stop: Arc<AtomicBool>) {
            self.streams.lane_streams(lanes, tx, stop);
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod host {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::SyncSender;
    use std::sync::Arc;

    use anyhow::Result;

    use super::*;
    use crate::radio::streams::{Block, StreamCounters};

    /// No radio on a development host: every operation succeeds and does nothing.
    pub struct NoRadio;

    impl RadioHw for NoRadio {
        async fn set_lo(&self, _: u64) -> Result<()> {
            Ok(())
        }
        async fn set_rate(&self, _: u32, _: u32) -> Result<()> {
            Ok(())
        }
        async fn set_gain(&self, _: GainMode, _: Option<f64>) -> Result<()> {
            Ok(())
        }
        async fn configure_control(&self, _: &'static DdcPreset, _: f64) -> Result<()> {
            Ok(())
        }
        async fn set_control_nco(&self, _: f64, _: u32) -> Result<()> {
            Ok(())
        }
        async fn configure_lanes(&self, _: &'static DdcPreset) -> Result<()> {
            Ok(())
        }
        async fn retune_lane(&self, _: Lane, _: f64, _: u32, _: bool) -> Result<()> {
            Ok(())
        }
        async fn pause_lane(&self, _: Lane) -> Result<()> {
            Ok(())
        }
        async fn readback(&self, _: u32) -> Readback {
            Readback::default()
        }
    }

    impl StreamSource for NoRadio {
        fn control_streams(&self, _: SyncSender<Block>, _: Arc<AtomicBool>, _: Arc<StreamCounters>) {}
    }
}

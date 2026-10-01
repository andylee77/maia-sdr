//! The tuner's hardware: the AD9361 and the P25 core on the board; on a development host, a
//! stand-in with no radio so the services and the API still run.

use super::tuner::{ControlLoop, LanePll, RadioHw, Readback};
use crate::hardware::ad9361::GainMode;
use crate::hardware::p25core::Lane;
use crate::hardware::presets::DdcPreset;

#[cfg(target_os = "linux")]
pub use board::Board as Hardware;
#[cfg(not(target_os = "linux"))]
pub use host::NoRadio as Hardware;

#[cfg(target_os = "linux")]
mod board {
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use tokio::sync::Mutex;

    use super::*;
    use crate::hardware::ad9361::Ad9361;
    use crate::hardware::p25core::{nco_to_freq, P25Core};

    pub struct Board {
        pub ad9361: Ad9361,
        pub core: Arc<Mutex<P25Core>>,
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
            let c = core.control();
            c.configure_ddc(nco_hz, preset)?;
            c.set_input(true);
            Ok(())
        }

        async fn set_control_nco(&self, nco_hz: f64, sample_rate_hz: u32) -> Result<()> {
            self.core.lock().await.control().set_nco(nco_hz, sample_rate_hz as f64)
        }

        async fn configure_lanes(&self, preset: &'static DdcPreset) -> Result<()> {
            let core = self.core.lock().await;
            for lane in core.lanes().collect::<Vec<_>>() {
                let chain = core.lane(lane).context("lane listed but absent")?;
                chain.configure_ddc(0.0, preset)?;
                chain.set_input(true);
            }
            Ok(())
        }

        async fn retune_lane(&self, lane: Lane, nco_hz: f64, sample_rate_hz: u32, reset: bool) -> Result<()> {
            let core = self.core.lock().await;
            core.lane(lane).with_context(|| format!("{lane} is not present"))?.retune(nco_hz, sample_rate_hz as f64, reset)
        }

        async fn lane_pll(&self, lane: Lane) -> Option<LanePll> {
            let core = self.core.lock().await;
            let clamp_q213 = core.version().pll_clamp_q213();
            core.lane(lane).map(|c| LanePll { pll_q213: c.debug().0, clamp_q213 })
        }

        async fn control_loop(&self) -> Option<ControlLoop> {
            let core = self.core.lock().await;
            let c = core.control();
            let (agc_gain, agc_mag) = c.agc_debug();
            Some(ControlLoop { pll_q213: c.debug().0, agc_gain, agc_mag })
        }

        async fn pause_lane(&self, lane: Lane) -> Result<()> {
            let core = self.core.lock().await;
            core.lane(lane).with_context(|| format!("{lane} is not present"))?.set_lsm_enable(false);
            Ok(())
        }

        async fn spectrum(&self) -> Option<Vec<u8>> {
            self.core.lock().await.read_spectrum().map(<[u8]>::to_vec)
        }

        async fn readback(&self, sample_rate_hz: u32) -> Readback {
            let sr = sample_rate_hz as f64;
            let nco = |word: u32| (sr > 0.0).then(|| nco_to_freq(word & 0x0FFF_FFFF, sr));
            let mut r = Readback {
                lo_hz: self.ad9361.lo_hz().await.ok(),
                gain_db: self.ad9361.gain_db().await.ok(),
                rssi_db: self.ad9361.rssi_db().await.ok(),
                gain_mode: self.ad9361.gain_mode().await.ok().map(GainMode::as_str),
                ..Default::default()
            };
            let core = self.core.lock().await;
            r.control_nco_hz = nco(core.control().nco_word());
            r.control_lsm = Some(core.control().lsm_control());
            r.control_nid = Some(core.control().nid());
            r.control_status = Some(core.control().status());
            for lane in core.lanes().collect::<Vec<_>>() {
                if let Some(c) = core.lane(lane) {
                    r.lane_nco_hz[lane.index()] = nco(c.nco_word());
                    r.lane_lsm[lane.index()] = Some(c.lsm_control());
                }
            }
            r
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod host {
    use anyhow::Result;

    use super::*;

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
}

//! The tuner's hardware: the AD9361 and the P25 core on the board; on a development host, a
//! stand-in with no radio so the services and the API still run.

use anyhow::Result;

use super::tuner::RadioHw;
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
    use crate::hardware::p25core::P25Core;

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

        async fn pause_lane(&self, lane: Lane) -> Result<()> {
            let core = self.core.lock().await;
            core.lane(lane).with_context(|| format!("{lane} is not present"))?.set_lsm_enable(false);
            Ok(())
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod host {
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
    }
}

//! Bring up the radio: open the AD9361 and the radio core, start the lane ring and its reader,
//! the spectrometer and the interrupt task, and build the tuner with the stored crystal
//! correction and gain.

use std::sync::Arc;

use anyhow::Result;

use crate::hardware::ad9361::GainMode;
use crate::radio::hw::Hardware;
use crate::radio::tuner::Tuner;
use crate::services::config::radio::{self, RadioConfig};

/// What boot learned about the hardware.
#[derive(Debug, Clone, serde::Serialize)]
pub struct HardwareInfo {
    pub core_version: Option<String>,
    /// Traffic lanes in use.
    pub lanes: usize,
}

pub type RadioTuner = Tuner<Hardware>;

pub fn gain_mode(mode: radio::GainMode) -> GainMode {
    match mode {
        radio::GainMode::Manual => GainMode::Manual,
        radio::GainMode::SlowAttack => GainMode::SlowAttack,
        radio::GainMode::FastAttack => GainMode::FastAttack,
        radio::GainMode::Hybrid => GainMode::Hybrid,
    }
}

pub async fn open(config: &RadioConfig, crystal_ppm: f64) -> Result<(Arc<RadioTuner>, HardwareInfo)> {
    let (hw, info) = hardware(config).await?;
    let tuner = Arc::new(Tuner::new(hw, crystal_ppm));
    let g = &config.gain;
    if let Err(e) = tuner.set_gain(gain_mode(g.mode), g.manual_db.map(f64::from)).await {
        tracing::error!("gain not set: {e:#}");
    }
    Ok((tuner, info))
}

#[cfg(target_os = "linux")]
async fn hardware(config: &RadioConfig) -> Result<(Hardware, HardwareInfo)> {
    use crate::hardware::ad9361::Ad9361;
    use crate::hardware::radiocore::RadioCore;
    use crate::radio::streams::{reader, Streams};

    let ad9361 = Ad9361::open().await?;
    let (core, interrupts) = RadioCore::take().await?;
    let identity = core.identity();
    // Every lane idle until the tuner loads a preset; the control lane's packets start then.
    for n in 0..identity.lanes {
        if let Some(l) = core.lane(n) {
            l.set_packets(false, 0);
        }
    }
    core.set_ring(true);
    core.set_spectrometer_integrations(256);
    core.set_spectrometer_peak(false);
    core.set_spectrometer(true);
    let wanted = config.traffic_chains.map_or(2, usize::from).clamp(1, 2);
    let info = HardwareInfo { core_version: Some(identity.version.to_string()), lanes: wanted.min(identity.lanes - 1) };
    tokio::spawn(async move {
        if let Err(e) = interrupts.run().await {
            tracing::error!("interrupt task ended: {e:#}");
        }
    });
    let core = Arc::new(tokio::sync::Mutex::new(core));
    let streams = Streams::new(identity.lanes);
    tokio::spawn(reader::run(core.clone(), streams.clone()));
    Ok((Hardware { ad9361, core, streams }, info))
}

#[cfg(not(target_os = "linux"))]
async fn hardware(_: &RadioConfig) -> Result<(Hardware, HardwareInfo)> {
    tracing::warn!("no radio on this host; tuning is simulated");
    Ok((Hardware, HardwareInfo { core_version: None, lanes: 0 }))
}

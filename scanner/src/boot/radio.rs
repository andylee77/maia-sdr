//! Bring up the radio: open the AD9361 and the P25 core, arm every chain idle, start the
//! interrupt task, and build the tuner with the stored crystal correction and gain.

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
    use crate::hardware::p25core::{IqRing, Lane, P25Core};

    let ad9361 = Ad9361::open().await?;
    let (core, interrupts) = P25Core::take().await?;
    let control = core.control();
    control.set_lsm_enable(true);
    control.set_dibit_dma(true);
    // Without the DC blocker the slicer sees a lopsided inner/outer ratio for minutes after
    // the PLL starts.
    control.set_dc_block(true);
    control.set_agc(true);
    core.set_iq_enable(IqRing::Control, true);
    core.set_iq_enable(IqRing::PreDiff, true);
    let wanted = config.traffic_chains.map_or(2, usize::from).clamp(1, 2);
    let lanes: Vec<Lane> = core.lanes().take(wanted).collect();
    for lane in &lanes {
        if let Some(c) = core.lane(*lane) {
            // Armed but off: a retune switches the chain on for a call.
            c.set_lsm_enable(false);
            c.set_dibit_dma(true);
            c.set_dc_block(true);
            c.set_agc(true);
            let rb = c.lsm_control();
            if rb.enable || !rb.dibit_dma {
                tracing::error!("{lane} control readback {rb:?}: expected off with its dibit DMA on");
            }
        }
    }
    core.set_iq_enable(IqRing::Traffic, true);
    core.set_spectrometer_integrations(256);
    core.set_spectrometer_peak(false);
    core.set_spectrometer(true);
    let rb = core.control().lsm_control();
    if !(rb.enable && rb.dibit_dma && rb.dc_block && rb.agc) {
        tracing::error!("control chain readback {rb:?}: expected every bit on");
    }
    let info = HardwareInfo { core_version: Some(core.version().to_string()), lanes: lanes.len() };
    tokio::spawn(async move {
        if let Err(e) = interrupts.run().await {
            tracing::error!("interrupt task ended: {e:#}");
        }
    });
    let core = Arc::new(tokio::sync::Mutex::new(core));
    Ok((Hardware { ad9361, core }, info))
}

#[cfg(not(target_os = "linux"))]
async fn hardware(_: &RadioConfig) -> Result<(Hardware, HardwareInfo)> {
    tracing::warn!("no radio on this host; tuning is simulated");
    Ok((Hardware, HardwareInfo { core_version: None, lanes: 0 }))
}

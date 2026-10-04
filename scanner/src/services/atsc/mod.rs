//! ATSC TV mode's channel finder: it steps the LO across the TV channels at 16 MSPS, two channels
//! a window, reads the wideband spectrometer there and says what each channel holds
//! (`protocol::atsc`): an 8-VSB station by its pilot, a signal without one (ATSC 3.0 or another)
//! or nothing, with the pilot's offset, the carrier to noise and the power.

pub mod sweep;

use serde::{Deserialize, Serialize};

use crate::protocol::atsc::spectrum::Kind;
use crate::protocol::atsc::{Channel, FIRST, LAST};
use crate::radio::plan::usable_half_hz;
use crate::services::discovery::SWEEP_RATE_HZ;

/// Half the part of a window clear of the decimator's edges.
pub fn usable_half() -> f64 {
    usable_half_hz(SWEEP_RATE_HZ) as f64
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AtscRequest {
    /// RF channels; empty = every channel the radio reaches.
    pub channels: Vec<u8>,
    /// Spectrometer frames per window.
    pub frames: usize,
    /// Manual receiver gain for the scan, dB; none = the AGC (slow attack).
    pub gain_db: Option<i32>,
}

impl Default for AtscRequest {
    fn default() -> Self {
        AtscRequest { channels: Vec::new(), frames: 8, gain_db: None }
    }
}

impl AtscRequest {
    pub fn channels(&self) -> Vec<u8> {
        if self.channels.is_empty() {
            Channel::all().filter(|c| c.reachable(usable_half())).map(|c| c.number).collect()
        } else {
            self.channels.clone()
        }
    }

    /// Every channel in the plan and reachable, the rest in range.
    pub fn check(&self) -> Result<(), String> {
        if let Some(n) = self.channels.iter().find(|&&n| !Channel::get(n).is_some_and(|c| c.reachable(usable_half()))) {
            return Err(format!("channel {n}: RF channels {FIRST} to {LAST} the radio reaches (not 2 or 3: below the AD9361)"));
        }
        if !(1..=64).contains(&self.frames) {
            return Err("frames 1..=64".into());
        }
        if self.gain_db.is_some_and(|g| !crate::services::config::radio::GAIN_DB_RANGE.contains(&g)) {
            return Err("gain_db -3..=76, or none for the AGC".into());
        }
        Ok(())
    }
}

/// What a scan offers: the channel plan, the default settings and the window it reads at once.
#[derive(Debug, Clone, Serialize)]
pub struct AtscOptions {
    pub channels: Vec<ChannelOption>,
    pub defaults: AtscRequest,
    pub window_hz: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChannelOption {
    pub number: u8,
    pub band: &'static str,
    pub low_hz: u64,
    pub center_hz: u64,
    /// The AD9361 tunes it (not channels 2 and 3).
    pub reachable: bool,
}

pub fn options() -> AtscOptions {
    AtscOptions {
        channels: Channel::all()
            .map(|c| ChannelOption {
                number: c.number,
                band: c.band,
                low_hz: c.low_hz,
                center_hz: c.center_hz(),
                reachable: c.reachable(usable_half()),
            })
            .collect(),
        defaults: AtscRequest::default(),
        window_hz: SWEEP_RATE_HZ,
    }
}

/// One channel as the scan read it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FoundChannel {
    pub number: u8,
    pub band: &'static str,
    pub center_hz: u64,
    pub kind: Kind,
    pub pilot_hz: Option<u64>,
    /// The pilot from where the plan puts it.
    pub pilot_offset_hz: Option<i64>,
    /// The pilot over the plateau, dB: 20.1 for a clean 8-VSB signal, less with noise or a
    /// multipath notch on it.
    pub pilot_db: Option<f32>,
    /// The plateau over the noise floor at the channel's edges: its carrier to noise, dB.
    pub level_db: f32,
    /// The channel's power, about dBm (the spectrum's scale, taken back to 60 dB of gain).
    pub power_dbm: Option<f32>,
    /// The receiver gain while it was read.
    pub gain_db: Option<f64>,
    /// The ADC clipped while it was read: the radio was overloaded and the numbers are not to be
    /// trusted (a lower manual gain helps).
    pub clipped: bool,
}

/// One channel's stretch of the spectrum its window read: the channel and 0.5 MHz either side.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ChannelSpectrum {
    pub number: u8,
    pub low_hz: u64,
    pub high_hz: u64,
    /// Where the plan puts the 8-VSB pilot.
    pub pilot_hz: u64,
    /// The window's LO (its DC spur).
    pub lo_hz: u64,
    /// The first bin's frequency.
    pub start_hz: f64,
    pub bin_hz: f64,
    /// Power per bin, about dBm (the spectrum's scale taken back to 60 dB of gain; as read when
    /// the gain is unknown).
    pub db: Vec<f32>,
}

/// A scan's progress and results.
#[derive(Debug, Clone, Serialize)]
pub struct AtscScan {
    pub id: u64,
    /// idle, sweeping, done, cancelled, error
    pub state: &'static str,
    pub started_unix_ms: u64,
    pub finished_unix_ms: u64,
    /// The channels asked for.
    pub channels: Vec<u8>,
    /// Manual gain, dB; none: the AGC.
    pub gain_db: Option<i32>,
    pub step: usize,
    pub steps: usize,
    /// The window's LO now.
    pub lo_hz: Option<u64>,
    /// Every channel read so far, lowest first.
    pub found: Vec<FoundChannel>,
    pub error: Option<String>,
    #[serde(skip)]
    pub cancel: bool,
}

impl Default for AtscScan {
    fn default() -> Self {
        AtscScan {
            id: 0,
            state: "idle",
            started_unix_ms: 0,
            finished_unix_ms: 0,
            channels: Vec::new(),
            gain_db: None,
            step: 0,
            steps: 0,
            lo_hz: None,
            found: Vec::new(),
            error: None,
            cancel: false,
        }
    }
}

impl AtscScan {
    pub fn running(&self) -> bool {
        self.state == "sweeping"
    }

    pub fn count(&self, kind: Kind) -> usize {
        self.found.iter().filter(|c| c.kind == kind).count()
    }
}

#[cfg(test)]
mod tests;

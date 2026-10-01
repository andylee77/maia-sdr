//! The AD9361 receiver through the Linux IIO sysfs interface (`ad9361-phy`).
//!
//! Receive only. Only `radio::tuner` calls the setters.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tokio::fs;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GainMode {
    Manual,
    FastAttack,
    SlowAttack,
    Hybrid,
}

impl GainMode {
    pub fn as_str(self) -> &'static str {
        match self {
            GainMode::Manual => "manual",
            GainMode::FastAttack => "fast_attack",
            GainMode::SlowAttack => "slow_attack",
            GainMode::Hybrid => "hybrid",
        }
    }

    pub fn parse(s: &str) -> Option<GainMode> {
        [GainMode::Manual, GainMode::FastAttack, GainMode::SlowAttack, GainMode::Hybrid]
            .into_iter()
            .find(|m| m.as_str() == s.trim())
    }
}

#[derive(Debug)]
pub struct Ad9361 {
    dir: PathBuf,
}

const LO: &str = "out_altvoltage0_RX_LO_frequency";
const SAMPLE_RATE: &str = "in_voltage_sampling_frequency";
const RF_BANDWIDTH: &str = "in_voltage_rf_bandwidth";
const GAIN_MODE: &str = "in_voltage0_gain_control_mode";
const GAIN: &str = "in_voltage0_hardwaregain";
const RSSI: &str = "in_voltage0_rssi";

impl Ad9361 {
    /// The first IIO device named `ad9361-phy`.
    pub async fn open() -> Result<Ad9361> {
        let mut entries = fs::read_dir("/sys/bus/iio/devices").await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = fs::read_to_string(entry.path().join("name")).await.unwrap_or_default();
            if name.trim_end() == "ad9361-phy" {
                return Ok(Ad9361::at(&entry.path()));
            }
        }
        anyhow::bail!("ad9361-phy IIO device not found")
    }

    /// A device at `dir` (tests use a directory of plain files).
    pub fn at(dir: &Path) -> Ad9361 {
        Ad9361 { dir: dir.to_path_buf() }
    }

    async fn write(&self, attr: &str, value: impl ToString) -> Result<()> {
        fs::write(self.dir.join(attr), value.to_string())
            .await
            .with_context(|| format!("AD9361 {attr}"))
    }

    async fn read(&self, attr: &str) -> Result<String> {
        Ok(fs::read_to_string(self.dir.join(attr)).await.with_context(|| format!("AD9361 {attr}"))?.trim().to_string())
    }

    pub async fn set_lo_hz(&self, hz: u64) -> Result<()> {
        self.write(LO, hz).await
    }

    pub async fn set_sample_rate_hz(&self, hz: u32) -> Result<()> {
        self.write(SAMPLE_RATE, hz).await
    }

    pub async fn set_rf_bandwidth_hz(&self, hz: u32) -> Result<()> {
        self.write(RF_BANDWIDTH, hz).await
    }

    /// Set the mode before a manual gain: changing the mode resets the gain.
    pub async fn set_gain_mode(&self, mode: GainMode) -> Result<()> {
        self.write(GAIN_MODE, mode.as_str()).await
    }

    pub async fn set_gain_db(&self, db: f64) -> Result<()> {
        self.write(GAIN, db).await
    }

    pub async fn lo_hz(&self) -> Result<u64> {
        Ok(self.read(LO).await?.parse()?)
    }

    /// The gain now applied (the AGC's choice outside manual mode).
    pub async fn gain_db(&self) -> Result<f64> {
        parse_db(&self.read(GAIN).await?)
    }

    pub async fn rssi_db(&self) -> Result<f64> {
        parse_db(&self.read(RSSI).await?)
    }

    pub async fn gain_mode(&self) -> Result<GainMode> {
        let s = self.read(GAIN_MODE).await?;
        GainMode::parse(&s).with_context(|| format!("AD9361 gain mode {s:?}"))
    }
}

/// `"71.000000 dB"` (or `"71 dB"`) to 71.0.
fn parse_db(s: &str) -> Result<f64> {
    let v = s.strip_suffix("dB").map(str::trim).with_context(|| format!("{s:?} is not in dB"))?;
    Ok(v.parse()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_reach_sysfs() {
        let dir = tempfile::tempdir().unwrap();
        let dev = Ad9361::at(dir.path());
        dev.set_lo_hz(858_100_000).await.unwrap();
        dev.set_gain_mode(GainMode::SlowAttack).await.unwrap();
        std::fs::write(dir.path().join(GAIN), "71.000000 dB\n").unwrap();
        assert_eq!(std::fs::read_to_string(dir.path().join(LO)).unwrap(), "858100000");
        assert_eq!(dev.lo_hz().await.unwrap(), 858_100_000);
        assert_eq!(dev.gain_mode().await.unwrap(), GainMode::SlowAttack);
        assert_eq!(dev.gain_db().await.unwrap(), 71.0);
    }

    #[test]
    fn decibels_parse_with_or_without_a_space() {
        assert_eq!(parse_db("91.75 dB").unwrap(), 91.75);
        assert_eq!(parse_db("-3dB").unwrap(), -3.0);
        assert!(parse_db("12").is_err());
    }
}

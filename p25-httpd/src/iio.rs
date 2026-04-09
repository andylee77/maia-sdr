//! AD9361 IIO device driver.
//!
//! Controls the AD9361 transceiver via the Linux IIO subsystem sysfs interface.
//! P25 is receive-only, so TX attributes are omitted.
//!
//! Adapted from maia-httpd/src/iio.rs.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;

/// AD9361 IIO device.
#[derive(Debug)]
pub struct Ad9361 {
    iio_device_path: PathBuf,
}

macro_rules! iio_getset {
    ($attribute:ident, $filename:expr, $ty_internal:ty, $ty_external:ty) => {
        paste::paste! {
            #[doc = concat!("Returns the value of the `", stringify!($attribute),
                            "` IIO attribute.")]
            pub async fn [<get_ $attribute>](&self) -> Result<$ty_external> {
                fs::read_to_string(self.iio_device_path.join($filename))
                    .await?
                    .trim_end()
                    .parse::<$ty_internal>()
                    .map_err(|_| anyhow::anyhow!(concat!(
                        "failed to parse IIO attribute ", stringify!($attribute))))
                    .map(|x| x.into())
            }

            #[doc = concat!("Sets the value of the `", stringify!($attribute),
                            "` IIO attribute.")]
            pub async fn [<set_ $attribute>](&self, value: $ty_external) -> Result<()> {
                fs::write(
                    self.iio_device_path.join($filename),
                    Into::<$ty_internal>::into(value).to_string().as_bytes(),
                ).await.context(concat!("failed to set IIO attribute ",
                                        stringify!($attribute)))?;
                Ok(())
            }
        }
    };
}

impl Ad9361 {
    /// Opens the first AD9361 IIO device (`ad9361-phy`) found in the system.
    pub async fn new() -> Result<Ad9361> {
        let iio_device_path = Self::find_iio_device()
            .await?
            .ok_or_else(|| anyhow::anyhow!("ad9361-phy IIO device not found"))?;
        Ok(Ad9361 { iio_device_path })
    }

    async fn find_iio_device() -> Result<Option<PathBuf>> {
        let mut entries = fs::read_dir(Path::new("/sys/bus/iio/devices")).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry
                .file_name()
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("file name is not valid UTF8"))?
                .starts_with("iio:device")
            {
                let mut path = entry.path();
                path.push("name");
                let this_name = fs::read_to_string(path).await?;
                if this_name == "ad9361-phy\n" {
                    return Ok(Some(entry.path()));
                }
            }
        }
        Ok(None)
    }

    // RX attributes
    iio_getset!(
        sampling_frequency,
        "in_voltage_sampling_frequency",
        u32,
        u32
    );
    iio_getset!(rx_rf_bandwidth, "in_voltage_rf_bandwidth", u32, u32);
    iio_getset!(rx_lo_frequency, "out_altvoltage0_RX_LO_frequency", u64, u64);
    iio_getset!(rx_gain, "in_voltage0_hardwaregain", Dbf64, f64);
    iio_getset!(
        rx_gain_mode,
        "in_voltage0_gain_control_mode",
        GainMode,
        GainMode
    );
}

/// AD9361 gain control modes.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum GainMode {
    Manual,
    FastAttack,
    SlowAttack,
    Hybrid,
}

impl std::str::FromStr for GainMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "manual" => Ok(GainMode::Manual),
            "fast_attack" => Ok(GainMode::FastAttack),
            "slow_attack" => Ok(GainMode::SlowAttack),
            "hybrid" => Ok(GainMode::Hybrid),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for GainMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        write!(
            f,
            "{}",
            match self {
                GainMode::Manual => "manual",
                GainMode::FastAttack => "fast_attack",
                GainMode::SlowAttack => "slow_attack",
                GainMode::Hybrid => "hybrid",
            }
        )
    }
}

/// Wrapper for parsing gain values formatted as `"12.345 dB"`.
#[derive(Debug, Clone, Copy)]
struct Dbf64(f64);

impl From<f64> for Dbf64 {
    fn from(value: f64) -> Dbf64 {
        Dbf64(value)
    }
}

impl From<Dbf64> for f64 {
    fn from(value: Dbf64) -> f64 {
        value.0
    }
}

impl std::str::FromStr for Dbf64 {
    type Err = <f64 as std::str::FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let db = " dB";
        if !s.ends_with(db) {
            return Err(format!("{s} does not end with 'dB'")
                .parse::<f64>()
                .err()
                .unwrap());
        }
        s[..s.len() - db.len()].parse().map(Dbf64)
    }
}

impl std::fmt::Display for Dbf64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

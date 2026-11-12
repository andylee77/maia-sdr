//! Command line. The radio's settings live in the configuration files; the init script's
//! tuning options only apply while no site is live.

use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, clap::Parser)]
#[command(name = "scanner", about = "Fishball scanner: P25 and DMR trunking")]
pub struct Args {
    /// HTTP listen address.
    #[arg(long, default_value = "0.0.0.0:8080")]
    pub listen: SocketAddr,

    /// HTTPS listen address, under the unit's own certificate (kept on the flash).
    #[arg(long, default_value = "0.0.0.0:8443")]
    pub listen_https: SocketAddr,

    /// Directory of the persistent flash (configuration, learned state).
    #[arg(long, default_value = "/mnt/jffs2")]
    pub flash_dir: PathBuf,

    /// The firmware's switch for libiio's daemon (`iiod`); a system without it has no switch.
    #[arg(long, default_value = "/usr/sbin/iio-switch")]
    pub iio_switch: PathBuf,

    /// Mount point of the SD card (history, recordings).
    #[arg(long, default_value = "/mnt/sd")]
    pub sd_dir: PathBuf,

    /// Directory of the recordings on the SD card (default: `p25_recordings` on the card).
    #[arg(long)]
    pub recordings_dir: Option<PathBuf>,

    /// A raw capture of a window, played in the radio's place in data mode (a development
    /// host): its name gives its LO and rate, as `look_434.42M_2500k.cs16`.
    #[arg(long)]
    pub data_replay: Option<PathBuf>,

    /// A raw capture of a window, played in the radio's place by the analog receiver (a
    /// development host): its name gives its LO and rate, as `wx_163.05M_3000k.cs16`.
    #[arg(long)]
    pub analog_replay: Option<PathBuf>,

    /// Receiver LO with no live site (Hz).
    #[arg(long)]
    pub rx_lo: Option<u64>,

    /// DDC preset with no live site.
    #[arg(long)]
    pub preset: Option<String>,

    /// Control channel with no live site (Hz).
    #[arg(long)]
    pub control_freq: Option<u64>,

    /// Crystal correction with no calibration on file (ppm).
    #[arg(long, allow_hyphen_values = true)]
    pub lo_ppm: Option<f64>,
}

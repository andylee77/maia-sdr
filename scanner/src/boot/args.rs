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

    /// HTTPS listen address, used when a certificate and key are given.
    #[arg(long, default_value = "0.0.0.0:8443")]
    pub listen_https: SocketAddr,

    #[arg(long)]
    pub ssl_cert: Option<PathBuf>,

    #[arg(long)]
    pub ssl_key: Option<PathBuf>,

    #[arg(long)]
    pub ca_cert: Option<PathBuf>,

    /// Directory of the persistent flash (configuration, learned state, legacy files).
    #[arg(long, default_value = "/mnt/jffs2")]
    pub flash_dir: PathBuf,

    /// Mount point of the SD card (history, recordings).
    #[arg(long, default_value = "/mnt/sd")]
    pub sd_dir: PathBuf,

    /// Directory of the recordings on the SD card (default: `p25_recordings` on the card).
    #[arg(long)]
    pub recordings_dir: Option<PathBuf>,

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

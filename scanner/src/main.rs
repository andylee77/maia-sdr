//! Fishball scanner: P25 and DMR trunking on the Fishball Z7020.
//!
//! `boot` brings the radio up from the persisted configuration and runs until a shutdown signal.

// Off the board the hardware layer and the stream readers compile for their tests only; the
// board build is the one that reports unused code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

mod api;
mod audio;
mod boot;
mod dsp;
mod hardware;
mod protocol;
mod radio;
mod services;
mod trunking;
mod ui;
mod util;

fn main() -> anyhow::Result<()> {
    let args = <boot::args::Args as clap::Parser>::parse();
    boot::logging::init();
    boot::run(args)
}

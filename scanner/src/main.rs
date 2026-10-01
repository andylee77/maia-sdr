//! Fishball scanner: P25 and DMR trunking on the Fishball Z7020.
//!
//! `boot` brings the radio up from the persisted configuration and runs until a shutdown signal.

mod api;
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

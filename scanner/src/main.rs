//! Fishball scanner: P25 and DMR trunking on the Fishball Z7020.
//!
//! `boot` brings the radio up from the persisted configuration and runs until a shutdown signal.

mod boot;
mod services;
mod util;

fn main() -> anyhow::Result<()> {
    let args = <boot::args::Args as clap::Parser>::parse();
    boot::logging::init();
    boot::run(args)
}

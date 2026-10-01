//! Logging to stdout (the init script appends it to /var/log) and the panic policy.
//!
//! The log sits on a RAM disk, so the default level is `warn`; `RUST_LOG` overrides it. The
//! operator-facing log is the event log, not this one.

use tracing_subscriber::{fmt, EnvFilter};

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    fmt().with_env_filter(filter).with_target(true).init();

    // After a panic in any task the shared state is suspect; exit and let the init script's
    // loop restart the daemon.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        eprintln!("scanner: panic, exiting so the init script restarts it");
        std::process::exit(70);
    }));
}

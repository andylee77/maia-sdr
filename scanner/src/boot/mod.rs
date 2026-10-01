//! Start-up and shutdown: load (and on first start migrate) the configuration, bring up the
//! radio and the services, serve until SIGTERM or Ctrl-C, then flush what must survive.

pub mod args;
pub mod logging;

use args::Args;

use crate::services::config::{self, Paths};

pub fn run(args: Args) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(serve(args))
}

async fn serve(args: Args) -> anyhow::Result<()> {
    let paths = Paths::new(&args.flash_dir, &args.sd_dir);
    let loaded = config::load_or_migrate(&paths)?;
    if let Some(report) = &loaded.migration {
        tracing::warn!(
            "migrated the p25-httpd files: {} systems, {} sites, {} profiles (see {})",
            report.systems,
            report.sites,
            report.profiles,
            paths.migration_log().display(),
        );
    }
    tracing::info!(
        "configuration: {} systems, live site {:?}",
        loaded.config.systems.value.systems.len(),
        loaded.config.state.value.live_site,
    );
    shutdown_signal().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

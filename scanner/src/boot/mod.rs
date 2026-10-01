//! Start-up and shutdown: load (and on a first start migrate) the configuration, bring up the
//! radio, make the live site live, serve the API until SIGTERM or Ctrl-C.

pub mod args;
pub mod logging;
pub mod radio;
pub mod state;
pub mod version;

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use args::Args;
use state::AppState;

use crate::audio::live::Audio;
use crate::radio::lease::RadioLease;
use crate::services::history::store::SiteInfo;
use crate::services::history::History;
use crate::services::clock::Clock;
use crate::services::notices::Notices;
use crate::services::recordings::storage::{self, StorageConfig};
use crate::services::recordings::{self, Policy, Recordings};
use crate::services::config::{self, Paths};
use crate::services::events::EventLog;
use crate::trunking::receivers::Receivers;
use crate::trunking::site::LiveSite;
use crate::trunking::trunk::Trunking;

pub fn run(args: Args) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    runtime.block_on(serve(args))
}

async fn serve(args: Args) -> anyhow::Result<()> {
    tracing::warn!("scanner {} starting", version::BUILD_TAG);
    let started = Instant::now();
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
    let config = loaded.config;
    let crystal_ppm = config.state.value.crystal.as_ref().map(|c| c.ppm).or(args.lo_ppm).unwrap_or(0.0);
    let live_site = config.state.value.live_site.clone();
    let (tuner, hardware) = radio::open(&config.radio.value, crystal_ppm).await?;

    let config = Arc::new(Mutex::new(config));
    let lease = RadioLease::default();
    let log = Arc::new(EventLog::default());
    log.system("start", format!("scanner {} started", version::BUILD_TAG));
    let receivers = Arc::new(Receivers::new(log.clone()));
    let clock = Arc::new(Clock::default());
    receivers.set_clock(clock.clone());
    clock.start(config.clone(), lease.clone(), log.clone());
    let lanes = crate::hardware::p25core::Lane::ALL[..hardware.lanes].to_vec();
    let audio = crate::audio::live::Audio::start(&lanes);
    let notices = Notices::default();
    let (history, recordings) = start_storage(&args, &paths, &config, &audio, &notices).await?;
    let first_call = recordings.next_call().max(history.store().max_call_id().unwrap_or(0) + 1);
    let trunking = Arc::new(Trunking::new(audio.clone(), recordings.sender(), history.sender(), notices.clone(), first_call));
    match history.query(|s| s.latest_calls(crate::trunking::trunk::RECENT)).await {
        Ok(rows) => trunking.seed_recent(rows),
        Err(e) => tracing::warn!("history: recent calls not read: {e:#}"),
    }
    let live = Arc::new(LiveSite::new(
        paths.clone(),
        config.clone(),
        tuner.clone(),
        lease.clone(),
        receivers.clone(),
        trunking.clone(),
        lanes,
        log.clone(),
        history.sender(),
    ));
    match &live_site {
        Some(site) => {
            if let Err(e) = live.activate(site).await {
                tracing::error!("site {site} did not go live: {e:#}");
            }
        }
        None => tracing::warn!("no site configured yet: waiting for a scan or a site to be added"),
    }

    {
        let live = live.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(crate::trunking::learned::SAVE_EVERY);
            tick.tick().await;
            loop {
                tick.tick().await;
                live.save_learned().await;
            }
        });
    }
    let discovery = Arc::default();
    let state = Arc::new(AppState { paths, config, tuner, live, lease, receivers, trunking, audio, recordings, history, discovery, notices, clock, log, hardware, started });
    let app = crate::api::router(state.clone());
    tokio::select! {
        r = crate::api::serve(app, args.listen, args.listen_https, args.ssl_cert.as_deref(), args.ssl_key.as_deref()) => r?,
        _ = shutdown_signal() => tracing::warn!("shutting down"),
    }
    state.receivers.stop().await;
    state.trunking.stop().await;
    state.recordings.flush(SHUTDOWN_FLUSH).await;
    state.history.flush(SHUTDOWN_FLUSH).await;
    state.live.save_learned().await;
    Ok(())
}

/// How long shutdown waits for the open recordings, the card writes and the history.
const SHUTDOWN_FLUSH: std::time::Duration = std::time::Duration::from_secs(10);

/// The card (waiting for its mount when it is there), the history on it (or in RAM) and the
/// recordings, listed from the card and reconciled with the history.
async fn start_storage(
    args: &Args,
    paths: &Paths,
    config: &Arc<Mutex<config::Config>>,
    audio: &Audio,
    notices: &Notices,
) -> anyhow::Result<(Arc<History>, Arc<Recordings>)> {
    let cfg = StorageConfig::board(&args.sd_dir, args.recordings_dir.clone().unwrap_or_else(|| paths.recordings()));
    let (policy, history_cfg, sites) = {
        let c = config.lock().await;
        (Policy::from(&c.radio.value.recording), c.radio.value.history.clone(), site_infos(&c.systems.value))
    };
    let sd = args.sd_dir.clone();
    let opened = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || {
            let mounted = storage::wait_for_sd(&cfg, storage::SD_MOUNT_WAIT);
            History::open(mounted.then_some(sd.as_path()), &history_cfg, &sites).or_else(|e| {
                tracing::error!("history on the card: {e:#}; keeping it in RAM");
                History::open(None, &history_cfg, &sites)
            })
        })
    };
    let history = opened.await??;
    let listing = {
        let cfg = cfg.clone();
        tokio::task::spawn_blocking(move || storage::index(&cfg))
    };
    let (mut list, note) = match tokio::time::timeout(storage::INDEX_TIMEOUT, listing).await {
        Ok(Ok(found)) => found,
        _ => (Vec::new(), format!("listing {} timed out", cfg.sd_dir.display())),
    };
    let store = history.store().clone();
    let list = tokio::task::spawn_blocking(move || {
        match recordings::reconcile(&mut list, &store) {
            Ok((added, removed)) if added + removed > 0 => {
                tracing::info!("history: {added} recordings listed, {removed} gone")
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("history: recordings not reconciled: {e}"),
        }
        list
    })
    .await?;
    let recordings = Recordings::start(cfg, policy, (list, note), audio, history.sender(), notices.clone());
    Ok((history, recordings))
}

/// The configured sites, as the history keeps them.
fn site_infos(systems: &config::systems::SystemsConfig) -> Vec<SiteInfo> {
    systems
        .systems
        .iter()
        .flat_map(|sys| {
            sys.sites.iter().map(move |site| SiteInfo {
                id: site.id.clone(),
                system: sys.id.clone(),
                protocol: sys.protocol.as_str().into(),
                label: site.label.clone(),
                system_label: sys.label.clone(),
            })
        })
        .collect()
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

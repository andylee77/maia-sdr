//! The board clock, kept by the configured source (`radio.json` `clock.source`):
//!
//! - `site`: the control channel's time (`site`). An unset clock and the first correction step;
//!   after that small differences slew, so times never jump backwards. Before the control
//!   channel is decoded, one internet attempt at start gives a time when there is internet.
//! - `ntp`: internet time at start, then hourly (every 5 minutes until one works).
//! - `manual`: nothing automatic (`POST /api/v1/clock` from a browser).
//!
//! Calls are timed on the monotonic clock, so a step never disturbs one.

pub mod board;
pub mod site;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::protocol::events::SiteSync;
use crate::radio::lease::RadioLease;
use crate::services::config::radio::ClockSource;
use crate::services::config::Config;
use crate::services::events::EventLog;
use crate::util::time::{iso_utc, unix_ms};
use site::{site_clock_action, ClockAction, SiteClock, MIN_VALID_MS};

const NTP_SERVERS: [&str; 3] = ["pool.ntp.org", "time.cloudflare.com", "time.google.com"];
const NTP_TIMEOUT: Duration = Duration::from_secs(5);
const NTP_OK_PERIOD: Duration = Duration::from_secs(3_600);
const NTP_RETRY: Duration = Duration::from_secs(300);
const TICK: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Serialize)]
pub struct ClockStatus {
    pub source: ClockSource,
    /// The board clock is set (2020 or later).
    pub valid: bool,
    /// How well the site's broadcasts give the time: minute, second or precise.
    pub site_precision: Option<&'static str>,
    /// Site time minus board time.
    pub site_offset_ms: Option<i64>,
    /// The last time the clock was set, and how.
    pub last_set: Option<String>,
}

#[derive(Default)]
pub struct Clock {
    site: Mutex<SiteClock>,
    last_set: Mutex<Option<String>>,
    source: Mutex<ClockSource>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Clock {
    /// A SYNC_BCST from the live site's control channel.
    pub fn observe(&self, s: SiteSync) {
        lock(&self.site).observe(s);
    }

    pub fn status(&self) -> ClockStatus {
        let site = lock(&self.site).now();
        let board = unix_ms();
        ClockStatus {
            source: *lock(&self.source),
            valid: board >= MIN_VALID_MS,
            site_precision: site.map(|(_, p)| p.as_str()),
            site_offset_ms: site.map(|(n, _)| n as i64 - board as i64),
            last_set: lock(&self.last_set).clone(),
        }
    }

    fn noted(&self, log: &EventLog, text: String) {
        log.system("clock", text.clone());
        *lock(&self.last_set) = Some(text);
    }

    /// Set the clock from a browser (any source; the manual one keeps it).
    pub fn set(&self, to_ms: u64, log: &EventLog) -> std::io::Result<()> {
        let was = unix_ms();
        board::step(to_ms)?;
        self.noted(log, format!("clock set from a browser: {} (was {})", iso_utc(to_ms), iso_utc(was)));
        Ok(())
    }

    /// Keep the board clock by the configured source.
    pub fn start(self: &Arc<Self>, config: Arc<tokio::sync::Mutex<Config>>, lease: RadioLease, log: Arc<EventLog>) {
        let me = self.clone();
        tokio::spawn(async move { me.run(config, lease, log).await });
    }

    async fn run(self: Arc<Self>, config: Arc<tokio::sync::Mutex<Config>>, lease: RadioLease, log: Arc<EventLog>) {
        let mut source: Option<ClockSource> = None;
        // The site time has set the clock since start or the source change.
        let mut stepped = false;
        let mut ntp_due: Option<Instant> = None;
        let mut tick = tokio::time::interval(TICK);
        loop {
            tick.tick().await;
            let now_source = config.lock().await.radio.value.clock.source;
            if source != Some(now_source) {
                if source.is_some() {
                    log.system("clock", format!("clock source: {}", source_name(now_source)));
                }
                source = Some(now_source);
                *lock(&self.source) = now_source;
                stepped = false;
                ntp_due = (now_source != ClockSource::Manual).then(Instant::now);
            }
            if ntp_due.is_some_and(|t| Instant::now() >= t) {
                let result = tokio::task::spawn_blocking(|| board::sync_ntp(&NTP_SERVERS, NTP_TIMEOUT)).await;
                let ok = match result {
                    Ok(Ok(secs)) => {
                        self.noted(&log, format!("clock set from the internet: {}", iso_utc(secs * 1_000)));
                        true
                    }
                    Ok(Err(e)) => {
                        tracing::info!("no internet time: {e}");
                        false
                    }
                    Err(e) => {
                        tracing::warn!("internet time: {e}");
                        false
                    }
                };
                // With the site as source, one attempt at start: the site time rules.
                ntp_due = (now_source == ClockSource::Ntp).then(|| Instant::now() + if ok { NTP_OK_PERIOD } else { NTP_RETRY });
            }
            // A scan hears other systems: steer by the live site only.
            if now_source != ClockSource::Site || !lease.is_normal() {
                continue;
            }
            let Some((site_ms, precision)) = lock(&self.site).now() else { continue };
            let board_ms = unix_ms();
            match site_clock_action(board_ms, site_ms, precision, stepped) {
                ClockAction::None => {}
                ClockAction::Step(ms) => match board::step(ms) {
                    Ok(()) => {
                        stepped = true;
                        self.noted(
                            &log,
                            format!("clock set from the control channel: {} (was {}, {})", iso_utc(ms), iso_utc(board_ms), precision.as_str()),
                        );
                    }
                    Err(e) => tracing::warn!("clock step failed: {e}"),
                },
                ClockAction::Slew(d) => {
                    if let Err(e) = board::slew(d) {
                        tracing::warn!("clock slew failed: {e}");
                    }
                }
            }
        }
    }
}

fn source_name(s: ClockSource) -> &'static str {
    match s {
        ClockSource::Site => "the control channel",
        ClockSource::Ntp => "the internet",
        ClockSource::Manual => "manual",
    }
}

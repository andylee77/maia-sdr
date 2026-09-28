//! Change 067: keeps the board clock set from the persisted clock source.
//!
//! - `site`: the control channel's time (SYNC_BCST, `services::site_clock`).
//!   An unset clock and the first correction step; after that small
//!   differences are slewed so call times never jump backwards. Before
//!   the control channel is decoded, one NTP attempt at start gives a
//!   time if the radio has internet.
//! - `ntp`: internet time at start, then hourly (every 5 min until one
//!   works).
//! - `manual`: nothing automatic (`POST /api/set_time` from a browser).

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::httpd::AppState;
use crate::services::event_log::LogCategory;
use crate::services::ntp;
use crate::services::site_clock::{site_clock_action, ClockAction};
use crate::services::ui_settings::ClockSource;

const NTP_SERVERS: [&str; 3] = ["pool.ntp.org", "time.cloudflare.com", "time.google.com"];
/// Change 071a: a clock step waits this long for both traffic chains to
/// be idle (open calls are timed on the board clock), then goes ahead.
const STEP_WAIT_MAX: Duration = Duration::from_secs(120);
const NTP_OK_PERIOD: Duration = Duration::from_secs(3_600);
const NTP_RETRY: Duration = Duration::from_secs(300);

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn iso(ms: u64) -> String {
    let s = ms / 1_000;
    let (d, r) = (s / 86_400, s % 86_400);
    // Civil from days (H. Hinnant), for log text only.
    let z = d as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", r / 3_600, r % 3_600 / 60, r % 60)
}

pub fn spawn_clock_task(state: Arc<AppState>) {
    tokio::spawn(async move {
        let policy = state.ui_settings.clock.clone();
        let mut source = policy.source();
        // The site time has set the clock since start / the source change.
        let mut stepped = false;
        // NTP: next attempt; `Some` = due (site mode: only the first).
        let mut ntp_due: Option<Instant> = match source {
            ClockSource::Manual => None,
            _ => Some(Instant::now()),
        };
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        // Change 071a: since when a step has been waiting for idle chains.
        let mut step_waiting: Option<Instant> = None;
        loop {
            tick.tick().await;
            // May the clock jump now? Not while a call is open (its hang
            // and end timers would fire at once or stall), unless a step
            // has already waited STEP_WAIT_MAX.
            let may_step = crate::app::traffic_lane::all_idle(&state.traffic_lanes).await
                || step_waiting.is_some_and(|t| t.elapsed() >= STEP_WAIT_MAX);
            let now_src = policy.source();
            if now_src != source {
                source = now_src;
                stepped = false;
                ntp_due = (source == ClockSource::Ntp).then(Instant::now);
                state.event_log.push(
                    LogCategory::System,
                    format!("clock source: {}", source.as_str()),
                    serde_json::json!({ "source": source.as_str() }),
                );
            }
            if ntp_due.is_some_and(|t| Instant::now() >= t) && !may_step {
                step_waiting.get_or_insert_with(Instant::now);
            } else if ntp_due.is_some_and(|t| Instant::now() >= t) {
                step_waiting = None;
                let r = tokio::task::spawn_blocking(|| {
                    ntp::sync_system_clock(&NTP_SERVERS, Duration::from_secs(5))
                }).await;
                let ok = matches!(r, Ok(Ok(_)));
                match &r {
                    Ok(Ok(epoch)) => {
                        tracing::info!("NTP sync OK — system clock set to Unix epoch {epoch}");
                        state.event_log.push(
                            LogCategory::System,
                            format!("clock set from NTP: {}", iso(epoch * 1_000)),
                            serde_json::json!({ "source": "ntp", "unix_s": epoch }),
                        );
                    }
                    Ok(Err(e)) => tracing::info!("NTP sync failed ({e})"),
                    Err(e) => tracing::warn!("NTP task panicked: {e}"),
                }
                // Site mode: one attempt at start (the site time rules).
                ntp_due = (source == ClockSource::Ntp)
                    .then(|| Instant::now() + if ok { NTP_OK_PERIOD } else { NTP_RETRY });
            }
            if source != ClockSource::Site {
                continue;
            }
            let (site_ms, precision) = {
                let dec = state.active_control_decoder().read().await;
                (dec.system.site_clock.now_ms(), dec.system.site_clock.precision())
            };
            let (Some(site_ms), Some(precision)) = (site_ms, precision) else {
                continue;
            };
            let board = now_unix_ms();
            match site_clock_action(board, site_ms, precision, stepped) {
                ClockAction::None => {}
                ClockAction::Step(_) if !may_step => {
                    step_waiting.get_or_insert_with(Instant::now);
                }
                ClockAction::Step(ms) => match ntp::step_clock_ms(ms) {
                    Ok(()) => {
                        stepped = true;
                        step_waiting = None;
                        state.event_log.push(
                            LogCategory::System,
                            format!(
                                "clock set from the control channel: {} (was {}, {})",
                                iso(ms), iso(board), precision.as_str(),
                            ),
                            serde_json::json!({
                                "source": "site",
                                "unix_ms": ms,
                                "was_unix_ms": board,
                                "precision": precision.as_str(),
                            }),
                        );
                    }
                    Err(e) => tracing::warn!("clock step failed: {e}"),
                },
                ClockAction::Slew(d) => {
                    if let Err(e) = ntp::slew_clock_ms(d) {
                        tracing::warn!("clock slew failed: {e}");
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::iso;

    #[test]
    fn iso_formats_utc() {
        assert_eq!(iso(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(iso(1_777_797_720_000), "2026-05-03 08:42:00 UTC");
    }
}

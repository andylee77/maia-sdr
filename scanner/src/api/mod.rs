//! The HTTP API and the web UI.
//!
//! One table (`routes!` below) builds the router and the route catalogue, so the catalogue
//! cannot drift from what is served. Handlers answer with typed JSON or an `ApiError`. A write
//! from another origin is refused (there is no authentication; the unit sits on a private
//! network); a write that succeeds is announced on `/ws/live` by the part it changed.

pub mod legacy;
pub mod v1;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Serialize;

use crate::boot::state::AppState;
use crate::services::notices::Notice;
use crate::ui;

/// An error as the API reports it: `{"ok": false, "error": "..."}` with a status.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn not_found(what: impl std::fmt::Display) -> ApiError {
        ApiError { status: StatusCode::NOT_FOUND, message: format!("{what} not found") }
    }

    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::BAD_REQUEST, message: message.into() }
    }

    pub fn conflict(message: impl Into<String>) -> ApiError {
        ApiError { status: StatusCode::CONFLICT, message: message.into() }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: format!("{e:#}") }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(serde_json::json!({ "ok": false, "error": self.message }))).into_response()
    }
}

pub type ApiResult<T> = Result<Json<T>, ApiError>;

/// One served route, for the catalogue.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct RouteDoc {
    pub method: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
}

macro_rules! routes {
    ($($method:ident $path:literal => $handler:path, $summary:literal;)*) => {
        pub const CATALOGUE: &[RouteDoc] = &[
            $(RouteDoc { method: stringify!($method), path: $path, summary: $summary }),*
        ];

        fn api_router() -> Router<Arc<AppState>> {
            Router::new()$(.route($path, $method($handler)))*
        }
    };
}

routes! {
    get "/api/v1/routes" => routes, "this list";
    get "/api/v1/status" => v1::status::get, "build, uptime, the live site, its control channel and the tuning";
    get "/api/v1/calls" => v1::calls::get, "the live site's open calls and its newest closed ones (from its history after a restart or switch), with names";
    get "/api/v1/hold" => v1::hold::get, "the talkgroup the live site is held on, if any, and each traffic lane's own";
    put "/api/v1/hold" => v1::hold::put, "hold the live site on one talkgroup (`tg`; null releases): only it is followed, whatever the aliases say; with `lane` (1 or 2) only that lane: it takes only that talkgroup, the other lane follows as before";
    get "/api/v1/calls/{id}" => v1::calls::one, "one call, live while recent, else from the history; the same shape either way";
    get "/ws/live" => ws::live, "the radio's state pushed as it changes: a snapshot, then status, traffic channels, calls, recordings, alert tones, the scan and configuration changes; and what a page subscribes to (the spectrum, the event log, the radio's readback, the window, the crystal)";
    get "/ws/events" => ws::events, "a text frame when a call opens or closes, a recording is saved or a call's alert tones are known";
    get "/ws/audio" => ws::audio, "live audio: with `v=2` every lane, each binary 20 ms frame tagged with its lane (text meta, alert and lag frames); without, lane one untagged";
    get "/api/v1/data" => v1::data::get, "packet data of a site (`site`, default the live one; `all`): totals, radios and recent records (`limit`)";
    get "/api/v1/activity/sites" => v1::activity::sites, "sites with history; where it is kept, its size and limits";
    get "/api/v1/activity/summary" => v1::activity::summary, "calls, voice and grant time, talkgroups, radios (`site`, `from`/`to` or `hours`)";
    get "/api/v1/activity/talkgroups" => v1::activity::talkgroups, "talkgroups by time, with names (`limit`)";
    get "/api/v1/activity/radios" => v1::activity::radios, "radios by time, with names (`limit`)";
    get "/api/v1/activity/radio/{unit}" => v1::activity::radio, "the talkgroups a radio used, its affiliations and registrations";
    get "/api/v1/activity/talkgroup/{tg}" => v1::activity::talkgroup, "a talkgroup's radios and encryption history";
    get "/api/v1/activity/series" => v1::activity::series, "calls and time per hour or day (`bucket`, `tz`, `tg`, `unit`)";
    get "/api/v1/activity/calls" => v1::activity::calls, "calls newest first in `/calls`' shape with names, recordings and alert tones (`tg`, `unit`, `limit`; `format=csv`: history rows as a file)";
    get "/api/v1/activity/alerts" => v1::activity::alerts, "the alert tones heard in followed calls (console warbles and beeps, two-tone pages): grouped by kind and tones, and the newest each with its call (`tg`, `unit`: the sending radio, `limit`)";
    get "/api/v1/survey" => v1::spectrum::survey, "the carriers heard in the live site's receive window over the last ten minutes: how often each is on, its peak above the floor, steady or not (from every spectrometer frame)";
    get "/api/v1/spectrum" => v1::spectrum::get, "the receive window from the wideband spectrometer (`bins`), with the control channel and lanes";
    get "/api/v1/events" => v1::events::list, "the event log after `after` (newest `limit`; housekeeping too with `routine=true`)";
    get "/api/v1/system" => v1::system::get, "the board's health: load, memory, CPU per core and per scanner thread, temperatures";
    get "/api/v1/iq/control.wav" => v1::iq::control, "the next `seconds` (default 10, at most 120) of the control channel's IQ as the decoder gets it: 50 kSPS stereo WAV, I left";
    get "/api/v1/receivers" => v1::receivers::get, "the control channel and each lane: status, decoder counters, carrier loop";
    get "/api/v1/config" => v1::config::export, "the whole configuration as one document: radio settings, systems with their aliases and sites, the live site (`download=true`: as a file)";
    put "/api/v1/config" => v1::config::import, "replace the configuration with an exported document (checked whole first); the scanner restarts";
    post "/api/v1/config/factory-reset" => v1::config::factory_reset, "back to a new unit: no systems, sites, aliases, recordings or history, default settings (the crystal calibration stays); the scanner restarts";
    get "/api/v1/radio" => v1::radio::get, "radio configuration, hardware and tuning";
    put "/api/v1/radio/gain" => v1::radio::put_gain, "receiver gain mode and manual gain";
    put "/api/v1/radio/settings" => v1::radio::put_settings, "presets the planner may use, traffic lanes, call timings, history limits";
    put "/api/v1/radio/clock" => v1::radio::put_clock, "where the board clock comes from: site, ntp or manual";
    get "/api/v1/radio/crystal" => v1::radio::crystal, "the crystal correction: applied, calibrated and tracked";
    put "/api/v1/radio/crystal" => v1::radio::put_crystal, "crystal tracking on or off, and its anchor (Hz from this run's calibration; 0: no limit)";
    post "/api/v1/radio/crystal/calibrate" => v1::radio::calibrate_crystal, "measure the crystal correction on the live control channel now (about 7 s)";
    post "/api/v1/clock" => v1::radio::set_time, "set the board clock (`unix_ms`, a browser's time)";
    put "/api/v1/radio/recording" => v1::radio::put_recording, "recording on/off, where new recordings go, how many each store keeps";
    get "/api/v1/recordings" => v1::recordings::list, "recordings newest first (`limit`, `site`), with the stores' state";
    delete "/api/v1/recordings" => v1::recordings::clear, "delete every recording of `store` (sd, ram or all)";
    get "/api/v1/recordings/{id}" => v1::recordings::file, "one recording's WAV (`{id}` or `{id}.wav`; byte ranges)";
    delete "/api/v1/recordings/{id}" => v1::recordings::delete, "delete one recording";
    get "/api/v1/systems" => v1::systems::list, "systems with their sites";
    get "/api/v1/systems/{id}" => v1::systems::get, "one system";
    put "/api/v1/systems/{id}" => v1::systems::put_system, "edit a system: its name, identity (P25: WACN and system ID; DMR: model and network) and details (location, county, type, voice)";
    get "/api/v1/systems/{id}/aliases" => v1::aliases::get, "a system's aliases (names, priorities, recording, speakers of its talkgroups and radios) and listening settings";
    put "/api/v1/systems/{id}/aliases" => v1::aliases::put, "replace a system's aliases (the live site follows them at once)";
    put "/api/v1/systems/{id}/listening" => v1::aliases::put_listening, "how a system treats talkgroups with no priority, and pre-emption";
    put "/api/v1/systems/{id}/talkgroups/{tg}" => v1::aliases::put_talkgroup, "one talkgroup's controls: name, group, priority, do-not-monitor, record, speaker";
    post "/api/v1/systems/{id}/radioreference" => v1::systems::import_radioreference, "import a RadioReference CSV (`csv`; talkgroups or sites, told by its header): talkgroups no alias covers become aliases (fully encrypted ones never followed unless `encrypted_do_not_monitor` is false); new sites are added (`sites`: only these rows), configured ones gain the channels they lack";
    post "/api/v1/systems/{id}/radioreference/preview" => v1::systems::preview_radioreference, "what that import would change; nothing is saved";
    put "/api/v1/systems/{system}/sites/{site}" => v1::systems::put_site, "edit a site: its name, identity (kept when absent), channels and receiver settings (the live site goes live again with the change)";
    delete "/api/v1/systems/{system}/sites/{site}" => v1::systems::delete_site, "remove a site (not the live one) and what it learned; the history keeps its calls";
    delete "/api/v1/systems/{id}" => v1::systems::delete_system, "remove a system with its sites and aliases (none of its sites live); the history keeps their calls";
    get "/api/v1/sites" => v1::sites::list, "every site, with the live one marked";
    post "/api/v1/sites/{id}/activate" => v1::sites::activate, "make a site live (returns once it is)";
    post "/api/v1/sites/{id}/stop" => v1::sites::stop, "stop the live site: no site is live until one is made live";
    get "/api/v1/sites/{id}/learned" => v1::sites::learned, "what a site taught the radio: band plan, grants, encrypted talkgroups, neighbours, its other channels";
    get "/api/v1/sites/{id}/plan" => v1::sites::plan, "the live site's receive window against its channels, and the planner's choice";
    post "/api/v1/sites/{id}/recentre" => v1::sites::recentre, "move the live site's window to the planner's choice now (both lanes idle)";
    get "/api/v1/scan" => v1::scan::get, "the scan's progress (the band and window read now) and what it found";
    get "/api/v1/scan/options" => v1::scan::options, "what a scan offers: its bands by name, the default settings, the window it reads at once and its step";
    post "/api/v1/scan" => v1::scan::start, "find the systems on the air (the live site pauses meanwhile)";
    post "/api/v1/scan/cancel" => v1::scan::cancel, "stop the scan";
    post "/api/v1/scan/add" => v1::scan::add, "add one found system from its card: a new system's name, identity and details; each ticked site's name, identity and channels (as heard when absent)";
    get "/api/v1/mode" => v1::mode::get, "the unit's mode: `scanner` (P25 and DMR trunking) or `atsc` (ATSC TV)";
    put "/api/v1/mode" => v1::mode::put, "change mode (`mode`): ATSC mode has the radio to itself with the live site paused; scanner mode brings the site back with the configured gain; kept across restarts";
    get "/api/v1/atsc/scan" => v1::atsc::get, "the TV scan's progress and each channel read: 8-VSB (its pilot found), a signal without the 8-VSB pilot (ATSC 3.0 or other) or vacant; the pilot's offset and level, the carrier to noise, the power";
    get "/api/v1/atsc/scan/channel/{n}" => v1::atsc::channel, "one RF channel's spectrum as the last TV scan read it: the channel and 0.5 MHz either side, dB per bin (about dBm)";
    get "/api/v1/atsc/scan/options" => v1::atsc::options, "the TV channel plan (RF 2-36 and which the radio reaches), the default settings and the window read at once";
    post "/api/v1/atsc/scan" => v1::atsc::start, "scan the TV channels in ATSC mode (`channels`, default every one reached; `frames` a window; `gain_db`, default the AGC)";
    post "/api/v1/atsc/scan/cancel" => v1::atsc::cancel, "stop the TV scan";
    get "/api/system" => legacy::system, "legacy, for the bench: the build";
    get "/api/ui/state" => legacy::ui_state, "legacy, for the bench: the unit's wall clock";
    get "/api/imbe_dump" => legacy::imbe_dump, "legacy, for the bench: the newest raw IMBE frames";
    get "/api/ui/calls" => legacy::ui_calls, "legacy, for the bench: the newest calls with their voice frame counts (`limit`, default 40)";
    get "/api/ui/settings" => legacy::ui_settings, "legacy, for the bench: the clock source";
    put "/api/ui/settings" => legacy::put_ui_settings, "legacy, for the bench: set the clock source";
}

async fn routes() -> Json<&'static [RouteDoc]> {
    Json(CATALOGUE)
}

/// Writes only from a page this unit served (or a client that sends no Origin, like curl).
async fn same_origin_writes(headers: HeaderMap, req: Request, next: Next) -> Response {
    if req.method() != Method::GET && req.method() != Method::HEAD {
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
        if let (Some(origin), Some(host)) = (origin, host) {
            let origin_host = origin.split_once("://").map_or(origin, |(_, h)| h);
            if origin_host != host {
                return ApiError { status: StatusCode::FORBIDDEN, message: format!("write from {origin} refused") }
                    .into_response();
            }
        }
    }
    next.run(req).await
}

pub fn router(state: Arc<AppState>) -> Router {
    api_router()
        .merge(ui::router())
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), announce_writes))
        .layer(middleware::from_fn(same_origin_writes))
        .with_state(state)
}

/// The part of the configuration a write to `path` changes, as `/ws/live` names it.
fn changed_part(path: &str) -> Option<&'static str> {
    if path.starts_with("/api/v1/radio") || path == "/api/v1/clock" {
        Some("radio")
    } else if (path.starts_with("/api/v1/systems") && !path.ends_with("/preview")) || path == "/api/v1/scan/add" {
        Some("systems")
    } else if path == "/api/v1/hold" {
        Some("hold")
    } else if path.starts_with("/api/v1/recordings") {
        Some("recordings")
    } else {
        None
    }
}

/// Announce each successful write by the part it changed, so open pages read it again.
async fn announce_writes(State(s): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let write = req.method() != Method::GET && req.method() != Method::HEAD;
    let part = changed_part(req.uri().path());
    let response = next.run(req).await;
    if let (true, Some(what)) = (write && response.status().is_success(), part) {
        s.notices.send(Notice::Changed { what });
    }
    response
}

async fn not_found(State(_): State<Arc<AppState>>) -> ApiError {
    ApiError::not_found("route")
}

/// Serve HTTP, and HTTPS too when a certificate and key are given.
pub async fn serve(
    app: Router,
    http: SocketAddr,
    https: SocketAddr,
    cert: Option<&std::path::Path>,
    key: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    use axum_server::accept::NoDelayAcceptor;
    // No Nagle: small live-audio frames must not wait for an ACK.
    let plain = axum_server::bind(http).acceptor(NoDelayAcceptor::new()).serve(app.clone().into_make_service());
    match (cert, key) {
        (Some(cert), Some(key)) => {
            let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(cert, key).await?;
            let secure = axum_server::bind_rustls(https, tls)
                .map(|r| r.acceptor(NoDelayAcceptor::new()))
                .serve(app.into_make_service());
            tracing::info!("serving http://{http} and https://{https}");
            tokio::select! {
                r = plain => r?,
                r = secure => r?,
            }
        }
        _ => {
            tracing::info!("serving http://{http}");
            plain.await?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn writes_name_the_part_they_change() {
        use super::changed_part;
        assert_eq!(changed_part("/api/v1/radio/gain"), Some("radio"));
        assert_eq!(changed_part("/api/v1/clock"), Some("radio"));
        assert_eq!(changed_part("/api/v1/systems/clay/sites/clay_1"), Some("systems"));
        assert_eq!(changed_part("/api/v1/scan/add"), Some("systems"));
        assert_eq!(changed_part("/api/v1/systems/clay/talkgroups/300"), Some("systems"));
        assert_eq!(changed_part("/api/v1/systems/clay/radioreference"), Some("systems"));
        assert_eq!(changed_part("/api/v1/systems/clay/radioreference/preview"), None, "a preview changes nothing");
        assert_eq!(changed_part("/api/v1/hold"), Some("hold"));
        assert_eq!(changed_part("/api/v1/scan"), None, "the scan's progress comes on its own");
        assert_eq!(changed_part("/api/v1/sites/clay_1/activate"), None, "the live state comes on its own");
    }

    use super::*;

    #[test]
    fn the_route_table_builds_and_lists_each_route_once() {
        let _ = api_router();
        let mut seen = std::collections::HashSet::new();
        for r in CATALOGUE {
            assert!(seen.insert((r.method, r.path)), "{} {} twice", r.method, r.path);
        }
    }

    /// `doc/API.md` is the route table rendered; `API_DOC_BLESS=1` rewrites it.
    #[test]
    fn the_api_reference_is_the_route_table() {
        let mut md = String::from(
            "# Scanner API\n\nGenerated from the route table (`src/api/mod.rs`) by the test \
             `the_api_reference_is_the_route_table`; `API_DOC_BLESS=1 cargo test` rewrites it. Writes from \
             another origin are refused; errors are `{\"ok\": false, \"error\": \"...\"}` with a status.\n\n\
             | Method | Path | What |\n|--------|------|------|\n",
        );
        for r in CATALOGUE {
            md += &format!("| {} | `{}` | {} |\n", r.method.to_uppercase(), r.path, r.summary.replace('|', "\\|"));
        }
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("doc/API.md");
        if std::env::var("API_DOC_BLESS").is_ok_and(|v| v == "1") {
            std::fs::write(&path, &md).unwrap();
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default().replace("\r\n", "\n");
        assert!(have == md, "doc/API.md is not the route table: run with API_DOC_BLESS=1");
    }
}

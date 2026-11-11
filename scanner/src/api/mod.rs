//! The HTTP API and the web UI.
//!
//! One table (`routes!` below) builds the router and the route catalogue, so the catalogue
//! cannot drift from what is served. Handlers answer with typed JSON or an `ApiError`. Every
//! request passes the guard (`access`): the name it was sent to is the unit's, and its client is
//! known at the level its route asks for. A write or a socket from a page of another origin is
//! refused; a write that succeeds is announced on `/ws/live` by the part it changed.

pub mod access;
pub mod legacy;
pub mod v1;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
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
    get "/api/v1/session" => access::session, "who the client is: whether a password is set (`protected`), its level (`admin`, `listener`, null: not signed in) and how it is known (`open`: no password is set, `session`, `token`)";
    post "/api/v1/session" => access::sign_in, "sign in with `password` (the level is whose it is) or trade a `token` for a browser's session; the answer sets the session's cookie";
    delete "/api/v1/session" => access::sign_out, "sign out: the session ends";
    get "/ca.crt" => access::ca_certificate, "the unit's CA certificate (PEM), for a PC to install so that its browser and tools trust the unit's HTTPS; open to a client that is not signed in, and served over plain HTTP too";
    get "/api/v1/certificate" => access::certificate, "the unit's HTTPS: its port, the CA's name and the SHA-256 of its certificate (to hold against what a PC shows), the names the certificate is for, those left out because the CA signs only private addresses and local names, and when it ends";
    get "/api/v1/access" => access::get, "the administrator's: whether each password is set, the browsers signed in, the tokens, the names the unit answers to";
    put "/api/v1/access/password" => access::put_password, "set, change or remove a level's password (`level`, `new`; `current` to change or remove the administrator's). The first administrator's password closes the radio and signs its setter in; removing it removes every password, session and token";
    post "/api/v1/access/tokens" => access::create_token, "make a token for a tool (`label`, `level`): the token is in the answer this once, and is sent as `Authorization: Bearer <token>`";
    delete "/api/v1/access/tokens/{id}" => access::revoke_token, "revoke a token";
    delete "/api/v1/access/sessions" => access::end_other_sessions, "end every session but the asker's";
    delete "/api/v1/access/sessions/{id}" => access::end_session, "end one session";
    get "/api/v1/access/iiod" => access::get_iiod, "the administrator's: libiio's daemon (`iiod`: port 30431 on the network, and the USB gadget's IIO function), which lets another program use the board as a tuner and so retune it under the scanner. Whether the system has its switch (`available`), the choice kept (`wanted`) and whether it runs now (`running`)";
    put "/api/v1/access/iiod" => access::put_iiod, "switch libiio's daemon (`on`). The choice is kept for every start, unless `until_restart` goes with `on`: then it runs until the unit restarts and the choice kept stays. The USB gadget leaves the bus for a moment and returns, so a host on the USB cable loses the unit's network and serial console and finds them again";
    put "/api/v1/access/names" => access::put_names, "the host names the unit answers to besides its addresses, `localhost` and its own host name (`names`): a request sent to any other name is refused";
    get "/api/v1/status" => v1::status::get, "build, uptime, the live site, its control channel and the tuning";
    get "/api/v1/calls" => v1::calls::get, "the live site's open calls and its newest closed ones (from its history after a restart or switch), with names";
    get "/api/v1/talkgroups" => v1::talkgroups::get, "the live site's talkgroups: those heard there in the last week or among the calls listed now (an alias for a talkgroup not heard lists none), each with how it is treated (followed, its traffic lane, priority, recorded) and when it was last heard; and what decides recording";
    get "/api/v1/hold" => v1::hold::get, "the talkgroup the live site is held on, if any, and each traffic lane's own";
    put "/api/v1/hold" => v1::hold::put, "hold the live site on one talkgroup (`tg`; null releases): only it is followed, whatever the aliases say; with `lane` (1 or 2) only that lane: it takes only that talkgroup, the other lane follows as before";
    get "/api/v1/calls/{id}" => v1::calls::one, "one call, live while recent, else from the history; the same shape either way";
    get "/ws/live" => ws::live, "the radio's state pushed as it changes: a snapshot, then status, traffic channels, calls, recordings, alert tones, the live site's talkgroups, the scan and configuration changes; and what a page subscribes to (the spectrum, the event log, the radio's readback, the window, the crystal, live TV, the download running)";
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
    get "/api/v1/events" => v1::events::list, "the event log after `after` (newest `limit`; housekeeping too with `routine=true`; `part`: only the lines of `scanner`, `tv` or `radio`, the radio's own)";
    get "/api/v1/system" => v1::system::get, "the board's health: load, memory, CPU per core and per scanner thread, temperatures";
    get "/api/v1/iq/control.wav" => v1::iq::control, "the next `seconds` (default 10, at most 120) of the control channel's IQ as the decoder gets it: 50 kSPS stereo WAV, I left";
    get "/api/v1/receivers" => v1::receivers::get, "the control channel and each lane: status, decoder counters, carrier loop";
    get "/api/v1/config" => v1::config::export, "the whole configuration as one document: radio settings, systems with their aliases and sites, the live site (`download=true`: as a file)";
    put "/api/v1/config" => v1::config::import, "replace the configuration with an exported document (checked whole first); the scanner restarts";
    get "/api/v1/playlist" => v1::playlist::export, "every system as an SDRTrunk playlist (XML): a channel for each site on its control channel and those that take over from it, each system's aliases as a list named after it (`download=true`: as a file)";
    post "/api/v1/playlist" => v1::playlist::put, "import an SDRTrunk playlist (`xml`): each of its systems into the configured system on its channels' frequencies or into a new one, its channels as sites, its alias lists as aliases; `systems` says what to take of each (`name`, `into`, `channels` by number, `aliases`), absent: what the preview suggests";
    post "/api/v1/playlist/preview" => v1::playlist::preview, "what that import would change; nothing is saved";
    post "/api/v1/config/factory-reset" => v1::config::factory_reset, "back to a new unit: no systems, sites, aliases, recordings, history or TV scan, default settings (the crystal calibration stays); the scanner restarts";
    get "/api/v1/radio" => v1::radio::get, "radio configuration, hardware and tuning";
    put "/api/v1/radio/gain" => v1::radio::put_gain, "receiver gain mode and manual gain";
    put "/api/v1/radio/settings" => v1::radio::put_settings, "any of: the presets the planner may use (`presets_allowed`), the traffic lanes (`traffic_chains`, null: all), the call timings (`calls`) and the history's limits (`history`); what is left out stays as it is";
    put "/api/v1/radio/clock" => v1::radio::put_clock, "where the board clock comes from: site, ntp or manual";
    put "/api/v1/radio/packet_data" => v1::radio::put_packet_data, "packet data followed or not (`follow`): followed, the last traffic lane of a site with a data channel waits there between calls; the live site follows the change at once";
    get "/api/v1/radio/crystal" => v1::radio::crystal, "the crystal correction: applied, calibrated and tracked";
    put "/api/v1/radio/crystal" => v1::radio::put_crystal, "crystal tracking on or off, and its anchor (Hz from this run's calibration; 0: no limit)";
    post "/api/v1/radio/crystal/calibrate" => v1::radio::calibrate_crystal, "measure the crystal correction on the live control channel now (about 7 s)";
    post "/api/v1/clock" => v1::radio::set_time, "set the board clock (`unix_ms`, a browser's time)";
    put "/api/v1/radio/recording" => v1::radio::put_recording, "any of: recording on/off (`enabled`), every followed call or only those of talkgroups marked record (`every_call`), where new recordings go, how long the card keeps them (locked ones longer), its share of the card, files a day folder, the count RAM keeps";
    get "/api/v1/recordings" => v1::recordings::list, "recordings newest first (`limit`: 200 when not given, 2,000 at most; `site`), with the listed site's total and the stores' state; `/api/v1/files` has them by span of time and as archives";
    delete "/api/v1/recordings" => v1::recordings::clear, "delete every recording of `store` (sd, ram or all)";
    get "/api/v1/recordings/{id}" => v1::recordings::file, "one recording's WAV (`{id}` or `{id}.wav`; byte ranges)";
    delete "/api/v1/recordings/{id}" => v1::recordings::delete, "delete one recording";
    put "/api/v1/recordings/{id}/lock" => v1::recordings::lock, "lock (`locked`: true) or unlock a card recording: kept for the locked days, in the locked tree";
    get "/api/v1/systems" => v1::systems::list, "systems with their sites";
    get "/api/v1/systems/{id}" => v1::systems::get, "one system";
    put "/api/v1/systems/{id}" => v1::systems::put_system, "edit a system: its name, identity (P25: WACN and system ID; DMR: model and network) and details (location, county, type, voice)";
    get "/api/v1/systems/{id}/aliases" => v1::aliases::get, "a system's aliases (names, priorities, recording, speakers of its talkgroups and radios) and listening settings";
    put "/api/v1/systems/{id}/aliases" => v1::aliases::put, "replace a system's aliases (the live site follows them at once); with `?rev=` (the `rev` the list was read with) only that list is replaced: 409 when it changed since. No talkgroup or radio may be named by itself in two aliases";
    put "/api/v1/systems/{id}/listening" => v1::aliases::put_listening, "how a system treats talkgroups with no priority, and pre-emption";
    put "/api/v1/systems/{id}/talkgroups/{tg}" => v1::aliases::put_talkgroup, "one talkgroup's controls: name, group, color, priority, follow (or do-not-monitor), record, speaker (its traffic lane: left is lane 1, right lane 2, both either); a talkgroup with no alias of its own gets one, and leaves an alias that named it among others";
    delete "/api/v1/systems/{id}/talkgroups/{tg}" => v1::aliases::delete_talkgroup, "remove the alias that names one talkgroup alone: it is then treated as a talkgroup with none";
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
    get "/api/v1/atsc/scan" => v1::atsc::get, "the TV scan's progress and each channel read: 8-VSB (its pilot found), a signal without the 8-VSB pilot (ATSC 3.0 or other) or vacant; the pilot's offset and level, the carrier to noise, the power; the station decoded (TSID, virtual channels and names, its clock); after a restart, the last scan that finished";
    get "/api/v1/atsc/scan/channel/{n}" => v1::atsc::channel, "one RF channel's spectrum as the last TV scan read it: the channel and 0.5 MHz either side, dB per bin (about dBm); not kept across a restart";
    get "/api/v1/atsc/scan/options" => v1::atsc::options, "the TV channel plan (RF 2-36 and which the radio reaches), the default settings and the window read at once";
    post "/api/v1/atsc/scan" => v1::atsc::start, "scan the TV channels in ATSC mode (`channels`, default every one reached; `frames` a window; `gain_db`, default the AGC), then decode each 8-VSB channel of 15 dB or more for its station's names (`identify`, default on)";
    post "/api/v1/atsc/scan/cancel" => v1::atsc::cancel, "stop the TV scan";
    get "/api/v1/files" => v1::files::list, "what is on the card as files to take away: the card's state and space, the recordings by the span of time their calls started in (`span`: 15, 30, 60 or 1440 minutes, default 60; a reader adds them up into its own zone's days), the history's database, the TV recordings, and the download running";
    get "/api/v1/files/recordings" => v1::files::count, "how many recordings a selection takes and their size (`from`, `to`: unix ms of the call's start; `site`, `tg`, `unit`; `locked=true`: only the locked ones)";
    get "/api/v1/files/recordings.zip" => v1::files::recordings_zip, "a selection's recordings as one ZIP archive (not compressed), each in the folder of its call's UTC date, with `recordings.csv` naming them; made as it is sent, one download at a time";
    get "/api/v1/files/history.sqlite" => v1::files::history, "the history's database (SQLite) as one consistent file: a copy is made on the card first, then sent and removed";
    get "/api/v1/files/tv/{file}" => v1::files::tv, "a TV recording's file, `rf<N>.ts`, as it is on the card";
    get "/api/v1/atsc/recordings" => v1::atsc::recordings, "the TV recordings a player can be served (`tv/rf<N>.ts` on the SD card): each one's RF channel, size and programmes";
    get "/api/v1/atsc/stream/{rf}" => v1::atsc::stream, "an RF channel's transport stream as it plays, for VLC and other players and the Watch page (`video/mp2t`): live from the radio core's 8-VSB block when it plays that channel, or in ATSC mode when nobody watches another (it tunes it); otherwise its recording at the pace of its clock, from the start again at its end; `program`: that programme alone, under a PAT naming only it";
    get "/api/v1/atsc/live" => v1::atsc::live, "live TV: whether the radio core has the 8-VSB block, the channel, the block's state (segments, fields, tracking, taps), the loops at the last field sync (carrier Hz, timing ppm, gain word, sync), the latest fields' MER, codewords clean, corrected and failed, tap solves and their time, PSIP's names, players";
    post "/api/v1/atsc/live" => v1::atsc::live_start, "play RF channel `rf` live in ATSC mode (the radio core's 8-VSB block; refused during a TV scan)";
    post "/api/v1/atsc/live/stop" => v1::atsc::live_stop, "stop live TV";
    get "/api/v1/diag/burst" => v1::diag::get, "the burst timing check's progress (what to do now) and result: where one burst starts, as an AD9361 sample index, in a spectrum frame's header, in the control lane's packets and in the capture, and whether they agree to within one transform";
    post "/api/v1/diag/burst" => v1::diag::start, "check the radio core's timing with a burst (a key fob, the ESP32-DIV) at `freq_hz`: a first press finds it (`find`, default on), the next is measured; `offset_hz` (300 kHz) above the LO, `span_hz` (150 kHz) searched, `wait_s` (60) for each press, `threshold_db` (15). The live site pauses until it ends";
    post "/api/v1/diag/burst/cancel" => v1::diag::cancel, "stop the burst timing check";
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

/// Writes and sockets only from a page this unit served (or a client that sends no Origin, like
/// curl): a page of another site cannot change the radio, nor open a socket and read it.
async fn own_pages_only(headers: HeaderMap, req: Request, next: Next) -> Response {
    let write = req.method() != Method::GET && req.method() != Method::HEAD;
    let socket = headers.contains_key(header::UPGRADE);
    if write || socket {
        let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
        if let Some(origin) = origin.filter(|o| !same_origin(o, served_as(&headers, req.uri()))) {
            let what = if socket { "socket" } else { "write" };
            return ApiError { status: StatusCode::FORBIDDEN, message: format!("{what} from {origin} refused") }.into_response();
        }
    }
    next.run(req).await
}

/// The host a request was sent to: its Host header, or over HTTP/2 (which has none) the
/// request's authority.
fn served_as<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers.get(header::HOST).and_then(|v| v.to_str().ok()).or_else(|| uri.authority().map(|a| a.as_str()))
}

/// The page at `origin` (`scheme://host[:port]`) is one this unit served as `host`. With no host
/// to hold it against, it is not.
fn same_origin(origin: &str, host: Option<&str>) -> bool {
    let origin_host = origin.split_once("://").map_or(origin, |(_, h)| h);
    host == Some(origin_host)
}

pub fn router(state: Arc<AppState>) -> Router {
    api_router()
        .merge(ui::router())
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), announce_writes))
        .layer(middleware::from_fn_with_state(state.clone(), access::guard))
        .layer(middleware::from_fn(own_pages_only))
        .with_state(state)
}

/// For a field of an edit that may be taken away: `null` and a missing field apart. A missing
/// field is `None` (with `#[serde(default)]`), `null` is `Some(None)`.
pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

/// The part of the configuration a write to `path` changes, as `/ws/live` names it.
fn changed_part(path: &str) -> Option<&'static str> {
    if path.starts_with("/api/v1/radio") || path == "/api/v1/clock" {
        Some("radio")
    } else if (path.starts_with("/api/v1/systems") && !path.ends_with("/preview")) || path == "/api/v1/scan/add" || path == "/api/v1/playlist" {
        Some("systems")
    } else if path == "/api/v1/hold" {
        Some("hold")
    } else if path.starts_with("/api/v1/recordings") {
        Some("recordings")
    } else if path.starts_with("/api/v1/access") {
        Some("access")
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

/// How often the certificate is held against the unit's names and the clock.
const CERTIFICATE_EVERY: std::time::Duration = std::time::Duration::from_secs(30);

/// See that the unit's certificate names what the unit is reached by now. True when it was made.
async fn certificate_current(s: &Arc<AppState>) -> Result<bool, String> {
    let state = s.clone();
    tokio::task::spawn_blocking(move || {
        let tls = state.tls.as_ref().ok_or("no certificate store")?;
        let (added, host) = state.access.names();
        let names = crate::services::tls::names(&crate::services::tls::addresses(), host.as_deref(), &added);
        tls.ensure(&names, crate::util::time::unix_ms() / 1000)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Keep the certificate current while the unit runs: its address may change with a lease, its
/// administrator may add a name, and the clock is set after the start.
async fn keep_certificate(s: Arc<AppState>, config: axum_server::tls_rustls::RustlsConfig) {
    let mut names = s.access.changes();
    loop {
        tokio::select! {
            _ = tokio::time::sleep(CERTIFICATE_EVERY) => {}
            changed = names.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        match certificate_current(&s).await {
            Ok(false) => {}
            Ok(true) => {
                let loaded = match s.tls.as_ref().map(|t| t.pems()) {
                    Some(Ok((cert, key))) => config.reload_from_pem(cert, key).await.map_err(|e| e.to_string()),
                    Some(Err(e)) => Err(e),
                    None => return,
                };
                let names = s.tls.as_ref().map(|t| t.view().names.join(", ")).unwrap_or_default();
                match loaded {
                    Ok(()) => s.log.radio("access", format!("the certificate was made again: it names {names}")),
                    Err(e) => s.log.radio("access", format!("the new certificate was not taken: {e}")),
                }
            }
            Err(e) => tracing::warn!("the certificate: {e}"),
        }
    }
}

/// Serve HTTP, and HTTPS under the unit's own certificate (`services::tls`). Once a password is
/// set the HTTP port only sends a browser to HTTPS.
pub async fn serve(app: Router, state: Arc<AppState>, http: SocketAddr, https: SocketAddr) -> anyhow::Result<()> {
    use axum_server::accept::NoDelayAcceptor;
    // No Nagle: small live-audio frames must not wait for an ACK.
    let plain_app = app.clone().layer(middleware::from_fn_with_state(state.clone(), access::https_only)).layer(axum::Extension(access::Secure(false)));
    let plain = axum_server::bind(http).acceptor(NoDelayAcceptor::new()).serve(plain_app.into_make_service());
    let tls = match certificate_current(&state).await.and_then(|_| state.tls.as_ref().ok_or("no certificate store".to_string())?.pems()) {
        Ok((cert, key)) => axum_server::tls_rustls::RustlsConfig::from_pem(cert, key).await.map_err(|e| e.to_string()),
        Err(e) => Err(e),
    };
    match tls {
        Ok(tls) => {
            tokio::spawn(keep_certificate(state.clone(), tls.clone()));
            state.https_port.store(https.port(), std::sync::atomic::Ordering::Relaxed);
            let secure = axum_server::bind_rustls(https, tls)
                .map(|r| r.acceptor(NoDelayAcceptor::new()))
                .serve(app.layer(axum::Extension(access::Secure(true))).into_make_service());
            tracing::info!("serving http://{http} and https://{https}");
            tokio::select! {
                r = plain => r?,
                r = secure => r?,
            }
        }
        Err(e) => {
            tracing::error!("HTTPS is not served: {e}");
            state.log.radio("access", format!("HTTPS is not served: {e}"));
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
        assert_eq!(changed_part("/api/v1/playlist"), Some("systems"));
        assert_eq!(changed_part("/api/v1/playlist/preview"), None, "a preview changes nothing");
        assert_eq!(changed_part("/api/v1/systems/clay/talkgroups/300"), Some("systems"));
        assert_eq!(changed_part("/api/v1/systems/clay/radioreference"), Some("systems"));
        assert_eq!(changed_part("/api/v1/systems/clay/radioreference/preview"), None, "a preview changes nothing");
        assert_eq!(changed_part("/api/v1/hold"), Some("hold"));
        assert_eq!(changed_part("/api/v1/scan"), None, "the scan's progress comes on its own");
        assert_eq!(changed_part("/api/v1/sites/clay_1/activate"), None, "the live state comes on its own");
    }

    use super::*;

    #[test]
    fn a_write_is_held_against_the_host_it_was_sent_to() {
        let unit = Some("192.168.120.50:8080");
        assert!(same_origin("http://192.168.120.50:8080", unit));
        assert!(!same_origin("http://evil.example", unit));
        assert!(!same_origin("https://192.168.120.50:8443", unit), "another port is another origin");
        assert!(!same_origin("null", unit));
        assert!(!same_origin("https://192.168.120.50:8443", None), "no host to hold the origin against");
    }

    #[test]
    fn an_http2_request_names_its_host_in_its_authority() {
        let mut headers = HeaderMap::new();
        let h2: Uri = "https://192.168.120.50:8443/api/v1/config/factory-reset".parse().unwrap();
        assert_eq!(served_as(&headers, &h2), Some("192.168.120.50:8443"));
        assert!(!same_origin("https://evil.example", served_as(&headers, &h2)));
        let h1: Uri = "/api/v1/hold".parse().unwrap();
        assert_eq!(served_as(&headers, &h1), None);
        headers.insert(header::HOST, "192.168.120.50:8080".parse().unwrap());
        assert_eq!(served_as(&headers, &h1), Some("192.168.120.50:8080"));
    }

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

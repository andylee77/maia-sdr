//! The HTTP API and the web UI.
//!
//! One table (`routes!` below) builds the router and the route catalogue, so the catalogue
//! cannot drift from what is served. Handlers answer with typed JSON or an `ApiError`. A write
//! from another origin is refused (there is no authentication; the unit sits on a private
//! network).

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
    get "/api/v1/calls" => v1::calls::get, "the open calls and the newest closed ones";
    get "/ws/audio" => ws::audio, "live audio of every lane (binary 20 ms frames, text meta and lag frames)";
    get "/api/v1/events" => v1::events::list, "the event log after `after` (newest `limit`; housekeeping too with `routine=true`)";
    get "/api/v1/radio" => v1::radio::get, "radio configuration, hardware and tuning";
    put "/api/v1/radio/gain" => v1::radio::put_gain, "receiver gain mode and manual gain";
    put "/api/v1/radio/recording" => v1::radio::put_recording, "recording on/off, where new recordings go, how many each store keeps";
    get "/api/v1/recordings" => v1::recordings::list, "recordings newest first (`limit`, `site`), with the stores' state";
    delete "/api/v1/recordings" => v1::recordings::clear, "delete every recording of `store` (sd, ram or all)";
    get "/api/v1/recordings/{id}" => v1::recordings::file, "one recording's WAV (`{id}` or `{id}.wav`; byte ranges)";
    delete "/api/v1/recordings/{id}" => v1::recordings::delete, "delete one recording";
    get "/api/v1/systems" => v1::systems::list, "systems with their sites";
    get "/api/v1/systems/{id}" => v1::systems::get, "one system";
    get "/api/v1/sites" => v1::sites::list, "every site, with the live one marked";
    post "/api/v1/sites/{id}/activate" => v1::sites::activate, "make a site live (returns once it is)";
    get "/api/v1/profiles" => v1::profiles::list, "profiles and each site's active one";
    put "/api/v1/sites/{id}/profile" => v1::profiles::select, "choose a site's active profile";
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
        .layer(middleware::from_fn(same_origin_writes))
        .with_state(state)
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
    use super::*;

    #[test]
    fn the_route_table_builds_and_lists_each_route_once() {
        let _ = api_router();
        let mut seen = std::collections::HashSet::new();
        for r in CATALOGUE {
            assert!(seen.insert((r.method, r.path)), "{} {} twice", r.method, r.path);
        }
    }
}

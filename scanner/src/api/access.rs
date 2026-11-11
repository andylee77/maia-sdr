//! The guard on every route and socket, signing in and out (`/api/v1/session`), and the
//! administrator's routes for the passwords, sessions, tokens and names (`/api/v1/access`).
//!
//! A client is known by its session's cookie (a browser) or a bearer token (a tool). A read may
//! carry its token in the address instead (`?token=`), for a player that sends no header (VLC on
//! a TV stream). With no administrator's password set the radio is open and every client is the
//! administrator.

use std::sync::Arc;

use axum::extract::{Extension, Path, Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use super::{served_as, ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::access::{self, Access, Level, TokenView, Via, View, Who};

const COOKIE: &str = "scanner_session";
/// A browser keeps its session this long; the radio keeps it until it is ended.
const COOKIE_MAX_AGE_S: u32 = 365 * 86_400;

/// Whether a request came over TLS (set on each listener's router).
#[derive(Debug, Clone, Copy)]
pub struct Secure(pub bool);

/// What a client showed to be known by; kept by a socket to see that it still is.
#[derive(Debug, Clone, Default)]
pub struct Shown {
    session: Option<String>,
    token: Option<String>,
}

impl Shown {
    pub fn of(headers: &HeaderMap) -> Shown {
        let session = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|pair| pair.trim().strip_prefix(COOKIE).and_then(|rest| rest.strip_prefix('=')))
            .map(str::to_string);
        let token = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).map(|t| t.trim().to_string());
        Shown { session, token }
    }

    pub fn who(&self, access: &Access) -> Who {
        access.who(self.session.as_deref(), self.token.as_deref())
    }

    /// Still known, at a listener's level or more: a socket goes on.
    pub fn still_known(&self, access: &Access) -> bool {
        self.who(access).level.is_some()
    }
}

/// The token in a request's address (`token=`), taken out of it: the route sees its own query.
fn take_token(req: &mut Request) -> Option<String> {
    let query = req.uri().query()?.to_string();
    let mut token = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| match pair.strip_prefix("token=") {
            Some(t) => {
                token = Some(t.to_string());
                false
            }
            None => true,
        })
        .collect();
    let token = token?;
    let path = req.uri().path();
    let path_and_query = if rest.is_empty() { path.to_string() } else { format!("{path}?{}", rest.join("&")) };
    let mut parts = req.uri().clone().into_parts();
    parts.path_and_query = path_and_query.parse().ok();
    if let Ok(uri) = Uri::from_parts(parts) {
        *req.uri_mut() = uri;
    }
    Some(token)
}

fn refuse(status: StatusCode, message: impl Into<String>) -> Response {
    ApiError { status, message: message.into() }.into_response()
}

/// Every request: the name it was sent to is one of the unit's, and the client is known at the
/// level its route asks for. The handlers find who it is, and what it showed, in the request's
/// extensions.
pub async fn guard(State(s): State<Arc<AppState>>, mut req: Request, next: Next) -> Response {
    if let Some(host) = served_as(req.headers(), req.uri()) {
        if !s.access.answers_to(host) {
            return refuse(StatusCode::MISDIRECTED_REQUEST, format!("this radio does not answer to the name {host:?}: use its address, or have its administrator add the name (Settings, Access)"));
        }
    }
    let read = req.method() == Method::GET || req.method() == Method::HEAD;
    let mut shown = Shown::of(req.headers());
    if read && shown.token.is_none() {
        shown.token = take_token(&mut req);
    }
    let who = shown.who(&s.access);
    if let Some(level) = access::needed(read, req.uri().path()) {
        match who.level {
            None => return refuse(StatusCode::UNAUTHORIZED, "sign in: this radio has a password"),
            Some(have) if have < level => return refuse(StatusCode::FORBIDDEN, format!("signed in as a {}: this is the {}'s", have.label(), level.label())),
            Some(_) => {}
        }
    }
    req.extensions_mut().insert(who);
    req.extensions_mut().insert(shown);
    next.run(req).await
}

/// Who the client is, as the page asks at its start.
#[derive(Serialize)]
pub struct Session {
    /// An administrator's password is set: clients sign in.
    pub protected: bool,
    /// `admin` or `listener`; null: not signed in.
    pub level: Option<Level>,
    /// `open` (no password is set), `session` or `token`.
    pub via: Option<Via>,
}

fn session_of(s: &AppState, who: &Who) -> Session {
    Session { protected: s.access.protected(), level: who.level, via: who.via }
}

pub async fn session(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>) -> Json<Session> {
    Json(session_of(&s, &who))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignIn {
    pub password: Option<String>,
    /// A token, traded for a browser's session at its level (a tool driving a page).
    pub token: Option<String>,
}

fn cookie(value: &str, max_age_s: u32, secure: bool) -> String {
    format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_s}{}", if secure { "; Secure" } else { "" })
}

fn agent_of(headers: &HeaderMap) -> String {
    headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("a client that gave no name").to_string()
}

fn failed(e: String) -> ApiError {
    ApiError { status: StatusCode::INTERNAL_SERVER_ERROR, message: e }
}

/// Start a session at `level` and answer with its cookie.
fn signed_in(s: &AppState, level: Level, headers: &HeaderMap, secure: bool) -> Result<Response, ApiError> {
    let value = s.access.sign_in(level, &agent_of(headers)).map_err(failed)?;
    let who = Who { level: Some(level), via: Some(Via::Session), session: None };
    Ok(([(header::SET_COOKIE, cookie(&value, COOKIE_MAX_AGE_S, secure))], Json(session_of(s, &who))).into_response())
}

/// Sign in with a password (the level is whose it is) or a token.
pub async fn sign_in(State(s): State<Arc<AppState>>, Extension(Secure(secure)): Extension<Secure>, headers: HeaderMap, Json(req): Json<SignIn>) -> Result<Response, ApiError> {
    if !s.access.protected() {
        return Err(ApiError::conflict("no password is set: there is nothing to sign in to"));
    }
    let level = match (req.password, req.token) {
        (Some(password), None) => {
            if let Some(wait) = s.access.must_wait() {
                return Err(ApiError { status: StatusCode::TOO_MANY_REQUESTS, message: format!("too many wrong passwords: try again in {} s", wait.as_secs() + 1) });
            }
            let access = s.clone();
            let level = tokio::task::spawn_blocking(move || access.access.check_password(&password)).await.map_err(|e| ApiError::from(anyhow::anyhow!(e)))?;
            if level.is_none() {
                s.log.radio("access", format!("a wrong password from {}", agent_of(&headers)));
            }
            level
        }
        (None, Some(token)) => s.access.token_level(&token),
        _ => return Err(ApiError::bad_request("give a password or a token")),
    };
    let level = level.ok_or(ApiError { status: StatusCode::UNAUTHORIZED, message: "that is not a password of this radio".into() })?;
    s.log.radio("access", format!("the {} signed in from {}", level.label(), agent_of(&headers)));
    signed_in(&s, level, &headers, secure)
}

/// Sign out: the session ends, here and on the radio.
pub async fn sign_out(State(s): State<Arc<AppState>>, Extension(Secure(secure)): Extension<Secure>, headers: HeaderMap) -> Result<Response, ApiError> {
    if let Some(value) = Shown::of(&headers).session {
        s.access.sign_out(&value).map_err(failed)?;
    }
    let who = Who { level: None, via: None, session: None };
    Ok(([(header::SET_COOKIE, cookie("", 0, secure))], Json(session_of(&s, &who))).into_response())
}

/// The passwords set, the browsers signed in, the tokens and the names.
pub async fn get(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>) -> Json<View> {
    Json(s.access.view(who.session.as_deref()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasswordEdit {
    pub level: Level,
    /// The administrator's password now, to change or remove it.
    pub current: Option<String>,
    /// The new one; null takes the level's password away (the administrator's: every password,
    /// session and token, and the radio is open again).
    pub new: Option<String>,
}

/// Set, change or remove a password. The first administrator's password signs its setter in.
pub async fn put_password(
    State(s): State<Arc<AppState>>,
    Extension(who): Extension<Who>,
    Extension(Secure(secure)): Extension<Secure>,
    headers: HeaderMap,
    Json(req): Json<PasswordEdit>,
) -> Result<Response, ApiError> {
    let first = !s.access.protected() && req.level == Level::Admin && req.new.is_some();
    let (state, keep, level, removed) = (s.clone(), who.session.clone(), req.level, req.new.is_none());
    tokio::task::spawn_blocking(move || state.access.set_password(req.level, req.current.as_deref(), req.new.as_deref(), keep.as_deref()))
        .await
        .map_err(|e| ApiError::from(anyhow::anyhow!(e)))?
        .map_err(ApiError::bad_request)?;
    s.log.radio(
        "access",
        match (level, removed, first) {
            (Level::Admin, true, _) => "the passwords were removed: the radio is open".to_string(),
            (Level::Admin, false, true) => "an administrator's password was set: clients sign in from now".to_string(),
            (level, true, _) => format!("the {}'s password was removed", level.label()),
            (level, false, _) => format!("the {}'s password was changed", level.label()),
        },
    );
    if first {
        return signed_in(&s, Level::Admin, &headers, secure);
    }
    Ok(Json(s.access.view(who.session.as_deref())).into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenRequest {
    /// What uses it.
    pub label: String,
    pub level: Level,
}

#[derive(Serialize)]
pub struct NewToken {
    #[serde(flatten)]
    pub view: TokenView,
    /// The token, given this once: `Authorization: Bearer <token>`.
    pub token: String,
}

pub async fn create_token(State(s): State<Arc<AppState>>, Json(req): Json<TokenRequest>) -> ApiResult<NewToken> {
    if !s.access.protected() {
        return Err(ApiError::conflict("no password is set: a token opens nothing that is not open"));
    }
    let (view, token) = s.access.create_token(&req.label, req.level).map_err(ApiError::bad_request)?;
    s.log.radio("access", format!("a token was made for {:?} at the {}'s level", view.label, view.level.label()));
    Ok(Json(NewToken { view, token }))
}

pub async fn revoke_token(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>, Path(id): Path<String>) -> ApiResult<View> {
    if !s.access.revoke_token(&id).map_err(failed)? {
        return Err(ApiError::not_found(format!("token {id}")));
    }
    s.log.radio("access", format!("token {id} was revoked"));
    Ok(Json(s.access.view(who.session.as_deref())))
}

/// End one session.
pub async fn end_session(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>, Path(id): Path<String>) -> ApiResult<View> {
    if s.access.end_sessions(Some(&id), None).map_err(failed)? == 0 {
        return Err(ApiError::not_found(format!("session {id}")));
    }
    Ok(Json(s.access.view(who.session.as_deref())))
}

/// End every session but the asker's.
pub async fn end_other_sessions(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>) -> ApiResult<View> {
    let ended = s.access.end_sessions(None, who.session.as_deref()).map_err(failed)?;
    s.log.radio("access", format!("{ended} other session(s) were ended"));
    Ok(Json(s.access.view(who.session.as_deref())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Names {
    pub names: Vec<String>,
}

/// The names the unit answers to besides its addresses and its own host name.
pub async fn put_names(State(s): State<Arc<AppState>>, Extension(who): Extension<Who>, Json(req): Json<Names>) -> ApiResult<View> {
    s.access.set_names(req.names).map_err(ApiError::bad_request)?;
    Ok(Json(s.access.view(who.session.as_deref())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn what_a_client_shows_is_read_from_its_headers() {
        let mut h = HeaderMap::new();
        assert!(Shown::of(&h).session.is_none() && Shown::of(&h).token.is_none());
        h.insert(header::COOKIE, "theme=dark; scanner_session=abc123; other=1".parse().unwrap());
        h.insert(header::AUTHORIZATION, "Bearer fbs_0011".parse().unwrap());
        let shown = Shown::of(&h);
        assert_eq!((shown.session.as_deref(), shown.token.as_deref()), (Some("abc123"), Some("fbs_0011")));
        // A cookie of another name that ends the same is not it, nor is another scheme a token.
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "not_scanner_session=abc123; scanner_session_old=1".parse().unwrap());
        h.insert(header::AUTHORIZATION, "Basic dXNlcjpwYXNz".parse().unwrap());
        let shown = Shown::of(&h);
        assert!(shown.session.is_none() && shown.token.is_none());
    }

    #[test]
    fn a_token_in_the_address_is_taken_out_of_it() {
        let request = |uri: &str| Request::builder().uri(uri).body(Body::empty()).unwrap();
        let mut req = request("/api/v1/events?limit=1&token=fbs_0011&part=radio");
        assert_eq!(take_token(&mut req).as_deref(), Some("fbs_0011"));
        assert_eq!(req.uri().to_string(), "/api/v1/events?limit=1&part=radio", "the route sees its own query");
        let mut req = request("/api/v1/atsc/stream/19?token=fbs_0011");
        assert_eq!(take_token(&mut req).as_deref(), Some("fbs_0011"));
        assert_eq!(req.uri().to_string(), "/api/v1/atsc/stream/19");
        let mut req = request("/api/v1/status?tokens=1");
        assert_eq!(take_token(&mut req), None);
        assert_eq!(req.uri().to_string(), "/api/v1/status?tokens=1");
        assert_eq!(take_token(&mut request("/api/v1/status")), None);
    }

    #[test]
    fn a_session_cookie_is_the_browsers_alone() {
        let c = cookie("abc", COOKIE_MAX_AGE_S, false);
        assert!(c.starts_with("scanner_session=abc; Path=/; HttpOnly; SameSite=Strict; Max-Age="));
        assert!(!c.contains("Secure"));
        assert!(cookie("abc", COOKIE_MAX_AGE_S, true).ends_with("; Secure"), "over TLS it is sent over TLS only");
        assert!(cookie("", 0, false).contains("Max-Age=0"), "signing out takes it back");
    }
}

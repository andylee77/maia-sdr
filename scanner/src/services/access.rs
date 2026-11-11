//! Who may reach the radio: the administrator's password and the listener's, the browsers
//! signed in with them, the tokens tools use, and the names the unit answers to.
//!
//! With no administrator's password the radio is open, as a new unit is: every client is the
//! administrator. Once one is set a client is whoever its session or its token says. A listener
//! sees and hears and changes nothing.
//!
//! A password is kept as a salted PBKDF2-HMAC-SHA256 hash; a session and a token are random and
//! kept as their SHA-256. Sessions end when they are signed out, when their password changes, or
//! when newer ones push them out: the board has no clock of its own to time them by. Everything
//! is one file on the flash, outside the configuration: an export does not carry it and a factory
//! reset leaves it. A file named `scanner-reset-access` on the card clears it at the next start,
//! for a password that is lost.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use aws_lc_rs::{digest, pbkdf2, rand};
use serde::{Deserialize, Serialize};

use crate::util::atomic_file;
use crate::util::time::unix_ms;

/// The file on the flash, in the scanner's directory.
pub const FILE: &str = "access.json";
/// On the card: clears the passwords, sessions and tokens at the next start.
pub const RESET_MARKER: &str = "scanner-reset-access";
/// About a second of an A9 core for each password tried.
const ITERATIONS: u32 = 100_000;
const MIN_PASSWORD: usize = 8;
const MAX_PASSWORD: usize = 256;
/// Browsers signed in at once; a new one past it pushes the oldest out.
const MAX_SESSIONS: usize = 32;
const MAX_TOKENS: usize = 32;
const MAX_NAMES: usize = 16;
/// After this many wrong passwords in a row, the next try waits.
const FREE_TRIES: u32 = 5;
const WAIT_AFTER: Duration = Duration::from_secs(30);
/// What a token starts with, so one is known when seen.
const TOKEN_PREFIX: &str = "fbs_";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Sees and hears; changes nothing.
    Listener,
    /// Everything.
    Admin,
}

impl Level {
    pub fn label(self) -> &'static str {
        match self {
            Level::Listener => "listener",
            Level::Admin => "administrator",
        }
    }
}

/// How a client is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// No password is set: everyone is the administrator.
    Open,
    Session,
    Token,
}

/// A client as the guard knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Who {
    pub level: Option<Level>,
    pub via: Option<Via>,
    /// Its session's id, when it has one.
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Secret {
    salt: String,
    hash: String,
    iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSession {
    sha256: String,
    level: Level,
    created_unix_ms: u64,
    /// The browser, as it names itself.
    agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredToken {
    id: String,
    label: String,
    level: Level,
    sha256: String,
    created_unix_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Stored {
    admin: Option<Secret>,
    listener: Option<Secret>,
    sessions: Vec<StoredSession>,
    tokens: Vec<StoredToken>,
    /// Names the unit answers to besides its own.
    names: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionView {
    pub id: String,
    pub level: Level,
    pub created_unix_ms: u64,
    pub agent: String,
    /// The one asking.
    pub current: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TokenView {
    pub id: String,
    pub label: String,
    pub level: Level,
    pub created_unix_ms: u64,
}

/// What the administrator sees of the access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct View {
    /// An administrator's password is set.
    pub protected: bool,
    /// A listener's password is set.
    pub listener: bool,
    pub sessions: Vec<SessionView>,
    pub tokens: Vec<TokenView>,
    /// Names the unit answers to besides its addresses, `localhost`, and its host name (below).
    pub names: Vec<String>,
    pub host_name: Option<String>,
}

struct Inner {
    stored: Stored,
    /// Wrong passwords in a row, and when the next try may be made.
    wrong: u32,
    wait_until: Option<Instant>,
}

pub struct Access {
    path: PathBuf,
    host_name: Option<String>,
    inner: Mutex<Inner>,
    /// Moves at every change: a socket then sees whether its client is still known.
    changes: tokio::sync::watch::Sender<u64>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn sha256(text: &str) -> String {
    hex(digest::digest(&digest::SHA256, text.as_bytes()).as_ref())
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut buf = vec![0u8; bytes];
    rand::fill(&mut buf).map_err(|_| "the system gave no random bytes".to_string())?;
    Ok(hex(&buf))
}

fn iterations(n: u32) -> std::num::NonZeroU32 {
    std::num::NonZeroU32::new(n.max(1)).unwrap_or(std::num::NonZeroU32::MIN)
}

/// Hash a password under a fresh salt. Slow: about a second on the board.
fn hash(password: &str) -> Result<Secret, String> {
    let salt = random_hex(16)?;
    let mut out = [0u8; 32];
    pbkdf2::derive(pbkdf2::PBKDF2_HMAC_SHA256, iterations(ITERATIONS), salt.as_bytes(), password.as_bytes(), &mut out);
    Ok(Secret { salt, hash: hex(&out), iterations: ITERATIONS })
}

/// Whether `password` is the secret's. As slow as `hash`.
fn matches(secret: &Secret, password: &str) -> bool {
    let Some(want) = unhex(&secret.hash) else { return false };
    pbkdf2::verify(pbkdf2::PBKDF2_HMAC_SHA256, iterations(secret.iterations), secret.salt.as_bytes(), password.as_bytes(), &want).is_ok()
}

fn check_new(password: &str) -> Result<(), String> {
    match password.chars().count() {
        n if n < MIN_PASSWORD => Err(format!("a password is at least {MIN_PASSWORD} characters")),
        n if n > MAX_PASSWORD => Err(format!("a password is at most {MAX_PASSWORD} characters")),
        _ => Ok(()),
    }
}

/// The unit's own host name.
fn host_name() -> Option<String> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname").ok().or_else(|| std::env::var("COMPUTERNAME").ok())?;
    let name = name.trim().to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// A request's `Host` without its port, in lower case: `[::1]:8080` is `::1`.
fn host_of(header: &str) -> String {
    let h = header.trim().to_ascii_lowercase();
    if let Some(rest) = h.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default().to_string();
    }
    match h.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => name.to_string(),
        _ => h,
    }
}

impl Access {
    /// Read the file in `dir`; with the reset marker on the card (`sd`), start from nothing.
    /// Returns the access and a line for the log when something is worth one.
    pub fn load(dir: &Path, sd: &Path) -> (Access, Option<String>) {
        let path = dir.join(FILE);
        let marker = sd.join(RESET_MARKER);
        let mut note = None;
        let mut stored: Stored = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                note = Some(format!("{}: {e}; the radio starts open", path.display()));
                Stored::default()
            }),
            Err(_) => Stored::default(),
        };
        if marker.exists() {
            let had = stored.admin.is_some();
            stored = Stored::default();
            let _ = atomic_file::write_json(&path, &stored);
            let _ = std::fs::remove_file(&marker);
            note = Some(format!("{RESET_MARKER} was on the card: the passwords, sessions and tokens are cleared{}", if had { "" } else { " (none was set)" }));
        }
        let changes = tokio::sync::watch::channel(0).0;
        (Access { path, host_name: host_name(), inner: Mutex::new(Inner { stored, wrong: 0, wait_until: None }), changes }, note)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn save(&self, stored: &Stored) -> Result<(), String> {
        self.changes.send_modify(|n| *n += 1);
        atomic_file::write_json(&self.path, stored).map_err(|e| format!("{}: {e}", self.path.display()))
    }

    /// Told at every change of who is known.
    pub fn changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// An administrator's password is set: clients must be known.
    pub fn protected(&self) -> bool {
        self.lock().stored.admin.is_some()
    }

    /// Whether the unit answers to the name a request was sent to (its `Host`): an address,
    /// `localhost`, its own host name (and that under `.local`), or a name added to the list.
    /// A page under any other name was pointed at the unit by someone else's DNS.
    pub fn answers_to(&self, host: &str) -> bool {
        let name = host_of(host);
        if name == "localhost" || name.parse::<std::net::IpAddr>().is_ok() {
            return true;
        }
        if self.host_name.as_ref().is_some_and(|own| &name == own || name == format!("{own}.local")) {
            return true;
        }
        self.lock().stored.names.iter().any(|n| n == &name)
    }

    /// Who a client is, by its session's cookie value or its bearer token.
    pub fn who(&self, session: Option<&str>, token: Option<&str>) -> Who {
        let inner = self.lock();
        if inner.stored.admin.is_none() {
            return Who { level: Some(Level::Admin), via: Some(Via::Open), session: None };
        }
        if let Some(t) = token {
            let sha = sha256(t);
            if let Some(found) = inner.stored.tokens.iter().find(|x| x.sha256 == sha) {
                return Who { level: Some(found.level), via: Some(Via::Token), session: None };
            }
        }
        if let Some(s) = session {
            let sha = sha256(s);
            if let Some(found) = inner.stored.sessions.iter().find(|x| x.sha256 == sha) {
                return Who { level: Some(found.level), via: Some(Via::Session), session: Some(sha[..12].to_string()) };
            }
        }
        Who { level: None, via: None, session: None }
    }

    /// How long a sign-in must wait after too many wrong passwords; None: it may be tried.
    pub fn must_wait(&self) -> Option<Duration> {
        let inner = self.lock();
        inner.wait_until.and_then(|t| t.checked_duration_since(Instant::now())).filter(|d| !d.is_zero())
    }

    /// The level `password` signs in at. Slow (it hashes once or twice): call it off the runtime.
    pub fn check_password(&self, password: &str) -> Option<Level> {
        let (admin, listener) = {
            let inner = self.lock();
            (inner.stored.admin.clone(), inner.stored.listener.clone())
        };
        let level = if admin.as_ref().is_some_and(|s| matches(s, password)) {
            Some(Level::Admin)
        } else if listener.as_ref().is_some_and(|s| matches(s, password)) {
            Some(Level::Listener)
        } else {
            None
        };
        let mut inner = self.lock();
        if level.is_some() {
            inner.wrong = 0;
            inner.wait_until = None;
        } else {
            inner.wrong += 1;
            if inner.wrong >= FREE_TRIES {
                inner.wait_until = Some(Instant::now() + WAIT_AFTER);
            }
        }
        level
    }

    /// The level a token gives, for a tool that trades it for a browser's session.
    pub fn token_level(&self, token: &str) -> Option<Level> {
        let sha = sha256(token);
        self.lock().stored.tokens.iter().find(|x| x.sha256 == sha).map(|x| x.level)
    }

    /// A new session at `level` for the browser `agent`: its cookie's value.
    pub fn sign_in(&self, level: Level, agent: &str) -> Result<String, String> {
        let value = random_hex(32)?;
        let mut inner = self.lock();
        let agent: String = agent.chars().filter(|c| !c.is_control()).take(120).collect();
        inner.stored.sessions.push(StoredSession { sha256: sha256(&value), level, created_unix_ms: unix_ms(), agent });
        let excess = inner.stored.sessions.len().saturating_sub(MAX_SESSIONS);
        inner.stored.sessions.drain(..excess);
        self.save(&inner.stored)?;
        Ok(value)
    }

    /// End the session with this cookie value.
    pub fn sign_out(&self, session: &str) -> Result<(), String> {
        let sha = sha256(session);
        let mut inner = self.lock();
        inner.stored.sessions.retain(|x| x.sha256 != sha);
        self.save(&inner.stored)
    }

    /// End the session with this id, or with none every session but `keep`. Returns how many.
    pub fn end_sessions(&self, id: Option<&str>, keep: Option<&str>) -> Result<usize, String> {
        let mut inner = self.lock();
        let before = inner.stored.sessions.len();
        inner.stored.sessions.retain(|x| match id {
            Some(id) => &x.sha256[..12] != id,
            None => keep == Some(&x.sha256[..12]),
        });
        self.save(&inner.stored)?;
        Ok(before - inner.stored.sessions.len())
    }

    /// Set a level's password, or with `None` take it away: the listener's account then, or the
    /// administrator's and with it every password, session and token (the radio is open again).
    /// Changing or removing the administrator's needs the one it has (`current`). The sessions of
    /// a changed password end, but for `keep`, the one that changed it. Slow, as `check_password`.
    pub fn set_password(&self, level: Level, current: Option<&str>, new: Option<&str>, keep: Option<&str>) -> Result<(), String> {
        if let Some(p) = new {
            check_new(p)?;
        }
        let had_admin = self.lock().stored.admin.clone();
        if let (Level::Admin, Some(old)) = (level, &had_admin) {
            if !current.is_some_and(|c| matches(old, c)) {
                return Err("the administrator's password now is not that".into());
            }
        }
        if level == Level::Listener && had_admin.is_none() {
            return Err("the administrator's password comes first".into());
        }
        let secret = new.map(hash).transpose()?;
        let mut inner = self.lock();
        match (level, secret) {
            (Level::Admin, None) => inner.stored = Stored { names: std::mem::take(&mut inner.stored.names), ..Stored::default() },
            (level, secret) => {
                match level {
                    Level::Admin => inner.stored.admin = secret,
                    Level::Listener => inner.stored.listener = secret,
                }
                inner.stored.sessions.retain(|x| x.level != level || keep == Some(&x.sha256[..12]));
            }
        }
        self.save(&inner.stored)
    }

    /// A new token at `level`: its id and the token itself, shown this once.
    pub fn create_token(&self, label: &str, level: Level) -> Result<(TokenView, String), String> {
        let label: String = label.trim().chars().filter(|c| !c.is_control()).take(60).collect();
        if label.is_empty() {
            return Err("a token needs a name: what uses it".into());
        }
        let token = format!("{TOKEN_PREFIX}{}", random_hex(32)?);
        let mut inner = self.lock();
        if inner.stored.tokens.len() >= MAX_TOKENS {
            return Err(format!("{MAX_TOKENS} tokens is the most: revoke one first"));
        }
        let id = random_hex(4)?;
        let stored = StoredToken { id: id.clone(), label: label.clone(), level, sha256: sha256(&token), created_unix_ms: unix_ms() };
        let view = TokenView { id, label, level, created_unix_ms: stored.created_unix_ms };
        inner.stored.tokens.push(stored);
        self.save(&inner.stored)?;
        Ok((view, token))
    }

    /// Revoke the token with this id. False when there is none.
    pub fn revoke_token(&self, id: &str) -> Result<bool, String> {
        let mut inner = self.lock();
        let before = inner.stored.tokens.len();
        inner.stored.tokens.retain(|x| x.id != id);
        self.save(&inner.stored)?;
        Ok(inner.stored.tokens.len() < before)
    }

    /// The names the unit answers to besides its own.
    pub fn set_names(&self, names: Vec<String>) -> Result<Vec<String>, String> {
        let mut clean = Vec::new();
        for n in names {
            let n = n.trim().to_ascii_lowercase();
            if n.is_empty() {
                continue;
            }
            if n.len() > 253 || !n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') {
                return Err(format!("{n:?} is not a host name"));
            }
            if !clean.contains(&n) {
                clean.push(n);
            }
        }
        if clean.len() > MAX_NAMES {
            return Err(format!("{MAX_NAMES} names is the most"));
        }
        let mut inner = self.lock();
        inner.stored.names = clean.clone();
        self.save(&inner.stored)?;
        Ok(clean)
    }

    /// What the administrator sees; `current` is the asker's session id.
    pub fn view(&self, current: Option<&str>) -> View {
        let inner = self.lock();
        let s = &inner.stored;
        View {
            protected: s.admin.is_some(),
            listener: s.listener.is_some(),
            sessions: s
                .sessions
                .iter()
                .map(|x| SessionView { id: x.sha256[..12].to_string(), level: x.level, created_unix_ms: x.created_unix_ms, agent: x.agent.clone(), current: current == Some(&x.sha256[..12]) })
                .collect(),
            tokens: s.tokens.iter().map(|x| TokenView { id: x.id.clone(), label: x.label.clone(), level: x.level, created_unix_ms: x.created_unix_ms }).collect(),
            names: s.names.clone(),
            host_name: self.host_name.clone(),
        }
    }

    /// A listener's password is set.
    #[cfg(test)]
    pub fn has_listener(&self) -> bool {
        self.lock().stored.listener.is_some()
    }
}

/// What a route asks of a client: nothing, a listener's level or the administrator's.
pub fn needed(method_is_read: bool, path: &str) -> Option<Level> {
    // The page itself and signing in: before anyone is known.
    if path == "/" || path == "/index.html" || path.starts_with("/ui/") || path == "/api/v1/session" {
        return None;
    }
    // The accounts are the administrator's to see too.
    if !method_is_read || path.starts_with("/api/v1/access") {
        return Some(Level::Admin);
    }
    Some(Level::Listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> (tempfile::TempDir, Access) {
        let dir = tempfile::tempdir().unwrap();
        let (access, note) = Access::load(dir.path(), dir.path());
        assert!(note.is_none());
        (dir, access)
    }

    #[test]
    fn a_new_unit_is_open_and_a_password_closes_it() {
        let (_dir, a) = fresh();
        assert!(!a.protected());
        assert_eq!(a.who(None, None), Who { level: Some(Level::Admin), via: Some(Via::Open), session: None });
        assert_eq!(a.set_password(Level::Listener, None, Some("listen-only-1"), None).unwrap_err(), "the administrator's password comes first");
        assert!(a.set_password(Level::Admin, None, Some("short"), None).is_err());
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        assert!(a.protected());
        assert_eq!(a.who(None, None), Who { level: None, via: None, session: None });
        assert_eq!(a.who(Some("made up"), Some("fbs_made_up")).level, None);
    }

    #[test]
    fn a_password_signs_in_at_its_level() {
        let (_dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        a.set_password(Level::Listener, None, Some("listen-only-1"), None).unwrap();
        assert!(a.has_listener());
        assert_eq!(a.check_password("correct horse 1"), Some(Level::Admin));
        assert_eq!(a.check_password("listen-only-1"), Some(Level::Listener));
        assert_eq!(a.check_password("neither of them"), None);
        let cookie = a.sign_in(Level::Listener, "Firefox\non a line of its own").unwrap();
        let who = a.who(Some(&cookie), None);
        assert_eq!((who.level, who.via), (Some(Level::Listener), Some(Via::Session)));
        let view = a.view(who.session.as_deref());
        assert_eq!(view.sessions.len(), 1);
        assert!(view.sessions[0].current && view.sessions[0].agent == "Firefoxon a line of its own");
        a.sign_out(&cookie).unwrap();
        assert_eq!(a.who(Some(&cookie), None).level, None);
    }

    #[test]
    fn what_is_kept_holds_no_password_session_or_token() {
        let (dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        let cookie = a.sign_in(Level::Admin, "browser").unwrap();
        let (_, token) = a.create_token("bench", Level::Admin).unwrap();
        let file = std::fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(!file.contains("correct horse 1") && !file.contains(&cookie) && !file.contains(&token), "{file}");
        // Read back by the next start, each is still good.
        let (b, _) = Access::load(dir.path(), dir.path());
        assert!(b.protected());
        assert_eq!(b.who(Some(&cookie), None).level, Some(Level::Admin));
        assert_eq!(b.who(None, Some(&token)).via, Some(Via::Token));
        assert_eq!(b.check_password("correct horse 1"), Some(Level::Admin));
    }

    #[test]
    fn a_changed_password_ends_its_sessions_but_the_changers() {
        let (_dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        a.set_password(Level::Listener, None, Some("listen-only-1"), None).unwrap();
        let mine = a.sign_in(Level::Admin, "mine").unwrap();
        let other = a.sign_in(Level::Admin, "another browser").unwrap();
        let listener = a.sign_in(Level::Listener, "kitchen").unwrap();
        let me = a.who(Some(&mine), None).session;
        assert_eq!(a.set_password(Level::Admin, Some("not it at all"), Some("battery staple 2"), me.as_deref()).unwrap_err(), "the administrator's password now is not that");
        a.set_password(Level::Admin, Some("correct horse 1"), Some("battery staple 2"), me.as_deref()).unwrap();
        assert_eq!(a.who(Some(&mine), None).level, Some(Level::Admin));
        assert_eq!(a.who(Some(&other), None).level, None, "signed in with the old password");
        assert_eq!(a.who(Some(&listener), None).level, Some(Level::Listener), "the listener's did not change");
        assert_eq!(a.check_password("correct horse 1"), None);
        // The listener's account taken away: its sessions with it.
        a.set_password(Level::Listener, None, None, None).unwrap();
        assert!(!a.has_listener());
        assert_eq!(a.who(Some(&listener), None).level, None);
        // The administrator's taken away: the radio is open again, and nothing of before is left.
        let (_, token) = a.create_token("bench", Level::Listener).unwrap();
        assert!(a.set_password(Level::Admin, None, None, None).is_err(), "it needs the password");
        a.set_password(Level::Admin, Some("battery staple 2"), None, None).unwrap();
        assert!(!a.protected());
        assert_eq!(a.who(None, None).via, Some(Via::Open));
        a.set_password(Level::Admin, None, Some("a third one, new"), None).unwrap();
        assert_eq!(a.who(Some(&mine), Some(&token)).level, None, "sessions and tokens of before are gone");
    }

    #[test]
    fn tokens_are_made_listed_and_revoked() {
        let (_dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        assert!(a.create_token("  ", Level::Admin).is_err());
        let (view, token) = a.create_token(" the bench ", Level::Listener).unwrap();
        assert!(token.starts_with(TOKEN_PREFIX) && token.len() == TOKEN_PREFIX.len() + 64);
        assert_eq!((view.label.as_str(), view.level), ("the bench", Level::Listener));
        assert_eq!(a.who(None, Some(&token)), Who { level: Some(Level::Listener), via: Some(Via::Token), session: None });
        assert_eq!(a.token_level(&token), Some(Level::Listener));
        assert_eq!(a.view(None).tokens, [view.clone()]);
        assert!(a.revoke_token(&view.id).unwrap());
        assert!(!a.revoke_token(&view.id).unwrap());
        assert_eq!(a.who(None, Some(&token)).level, None);
    }

    #[test]
    fn sessions_are_ended_one_or_all_but_the_askers() {
        let (_dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        let cookies: Vec<String> = (0..3).map(|i| a.sign_in(Level::Admin, &format!("browser {i}")).unwrap()).collect();
        let ids: Vec<String> = cookies.iter().map(|c| a.who(Some(c), None).session.unwrap()).collect();
        assert_eq!(a.end_sessions(Some(&ids[1]), None).unwrap(), 1);
        assert_eq!(a.who(Some(&cookies[1]), None).level, None);
        assert_eq!(a.end_sessions(None, Some(&ids[0])).unwrap(), 1);
        assert_eq!(a.view(Some(&ids[0])).sessions.iter().map(|s| (s.agent.as_str(), s.current)).collect::<Vec<_>>(), [("browser 0", true)]);
        // Past the most kept at once, the oldest goes.
        for i in 0..MAX_SESSIONS {
            a.sign_in(Level::Listener, &format!("more {i}")).unwrap();
        }
        assert_eq!(a.view(None).sessions.len(), MAX_SESSIONS);
        assert_eq!(a.who(Some(&cookies[0]), None).level, None);
    }

    #[test]
    fn wrong_passwords_in_a_row_make_the_next_try_wait() {
        let (_dir, a) = fresh();
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        for _ in 0..FREE_TRIES - 1 {
            assert_eq!(a.check_password("a wrong guess"), None);
            assert!(a.must_wait().is_none());
        }
        assert_eq!(a.check_password("a wrong guess"), None);
        assert!(a.must_wait().is_some_and(|d| d <= WAIT_AFTER && d > WAIT_AFTER - Duration::from_secs(5)));
        // The right one clears the count.
        assert_eq!(a.check_password("correct horse 1"), Some(Level::Admin));
        assert!(a.must_wait().is_none());
    }

    #[test]
    fn the_unit_answers_to_its_own_names() {
        let (_dir, mut a) = fresh();
        a.host_name = Some("fishball".into());
        for host in ["192.168.120.50:8080", "192.168.120.50", "[fe80::1]:8443", "localhost:8090", "LOCALHOST", "fishball", "Fishball.local:8443"] {
            assert!(a.answers_to(host), "{host}");
        }
        for host in ["attacker.example", "fishball.attacker.example", "192.168.120.50.attacker.example:8080", "fishball.lan", ""] {
            assert!(!a.answers_to(host), "{host}");
        }
        assert!(a.set_names(vec!["bad name".into()]).is_err());
        assert_eq!(a.set_names(vec![" Fishball.LAN ".into(), "fishball.lan".into(), String::new()]).unwrap(), ["fishball.lan"]);
        assert!(a.answers_to("fishball.lan:8443"));
    }

    #[test]
    fn the_marker_on_the_card_clears_it() {
        let dir = tempfile::tempdir().unwrap();
        let sd = tempfile::tempdir().unwrap();
        let (a, _) = Access::load(dir.path(), sd.path());
        a.set_password(Level::Admin, None, Some("correct horse 1"), None).unwrap();
        std::fs::write(sd.path().join(RESET_MARKER), b"").unwrap();
        let (b, note) = Access::load(dir.path(), sd.path());
        assert!(!b.protected());
        assert!(note.unwrap().starts_with("scanner-reset-access was on the card"));
        assert!(!sd.path().join(RESET_MARKER).exists(), "the marker is used once");
        let (c, note) = Access::load(dir.path(), sd.path());
        assert!(!c.protected() && note.is_none());
    }

    #[test]
    fn each_route_asks_for_its_level() {
        for path in ["/", "/index.html", "/ui/v1/js/main.js", "/api/v1/session"] {
            assert_eq!((needed(true, path), needed(false, path)), (None, None), "{path}");
        }
        assert_eq!(needed(true, "/api/v1/status"), Some(Level::Listener));
        assert_eq!(needed(true, "/ws/audio"), Some(Level::Listener));
        assert_eq!(needed(true, "/api/v1/files/recordings.zip"), Some(Level::Listener));
        assert_eq!(needed(false, "/api/v1/hold"), Some(Level::Admin));
        assert_eq!(needed(true, "/api/v1/access"), Some(Level::Admin), "the accounts are the administrator's to see too");
        assert_eq!(needed(true, "/api/system"), Some(Level::Listener), "the bench's routes as the rest");
    }
}

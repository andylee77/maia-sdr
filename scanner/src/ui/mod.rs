//! The web UI: plain HTML, CSS and ES modules under `static/`, embedded at compile time (no
//! build step, no CDN: the unit may have no internet).
//!
//! `index.html` is served at `/` and refers to every asset as `ui/{{BUILD}}/<path>`; the
//! placeholder becomes the build tag plus a hash of the assets, so a new binary always gets new
//! URLs. Versioned assets are cached as immutable; `index.html` is not cached.

use std::sync::{Arc, OnceLock};

use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::boot::state::AppState;
use crate::boot::version::BUILD_TAG;

struct Asset {
    path: &'static str,
    body: &'static str,
}

macro_rules! assets {
    ($($p:literal),* $(,)?) => {
        &[$(Asset { path: $p, body: include_str!(concat!("static/", $p)) }),*]
    };
}

const ASSETS: &[Asset] = assets![
    "index.html",
    "css/base.css",
    "css/components.css",
    "js/main.js",
    "js/api.js",
    "js/store.js",
    "js/dom.js",
    "js/format.js",
    "js/protocols.js",
    "js/views/now.js",
    "js/views/systems.js",
    "js/views/settings.js",
    "js/views/diagnostics.js",
    "js/views/activity.js",
    "js/views/packet_data.js",
    "js/views/scan.js",
    "js/views/spectrum.js",
    "js/views/board.js",
    "js/audio/player.js",
    "js/audio/ring.js",
    "js/audio/sources.js",
];

const PLACEHOLDER: &str = "{{BUILD}}";

fn find(path: &str) -> Option<&'static Asset> {
    ASSETS.iter().find(|a| a.path == path)
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        // Browsers refuse to run a module served as anything else.
        Some("js") => "text/javascript; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `<build tag>.<FNV-1a of every asset>`.
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut hash: u32 = 0x811c_9dc5;
        for a in ASSETS {
            for b in a.path.bytes().chain(a.body.bytes()) {
                hash ^= b as u32;
                hash = hash.wrapping_mul(0x0100_0193);
            }
        }
        format!("{BUILD_TAG}.{hash:08x}")
    })
}

async fn index() -> Response {
    let body = find("index.html").map(|a| a.body.replace(PLACEHOLDER, version())).unwrap_or_default();
    ([(header::CONTENT_TYPE, content_type("index.html")), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn asset(Path((v, path)): Path<(String, String)>) -> Response {
    match find(&path) {
        Some(a) if v == version() => (
            [(header::CONTENT_TYPE, content_type(&path)), (header::CACHE_CONTROL, "public, max-age=31536000, immutable")],
            a.body,
        )
            .into_response(),
        // An old page asking for old assets: reload it.
        _ => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route("/", get(index)).route("/index.html", get(index)).route("/ui/{v}/{*path}", get(asset))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Relative imports of a module, resolved against its directory.
    fn imports(path: &str, body: &str) -> Vec<String> {
        let dir: Vec<&str> = path.rsplit_once('/').map_or(vec![], |(d, _)| d.split('/').collect());
        body.lines()
            .filter(|l| l.trim_start().starts_with("import "))
            .filter_map(|l| l.split(['\'', '"']).nth(1))
            .filter(|spec| spec.starts_with('.'))
            .map(|spec| {
                let mut parts: Vec<&str> = dir.clone();
                for seg in spec.split('/') {
                    match seg {
                        "." => {}
                        ".." => {
                            parts.pop();
                        }
                        s => parts.push(s),
                    }
                }
                parts.join("/")
            })
            .collect()
    }

    #[test]
    fn every_reference_resolves_and_every_asset_is_used() {
        let index = find("index.html").unwrap().body;
        let mut reached: BTreeSet<String> = index
            .split(&format!("ui/{PLACEHOLDER}/"))
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().to_string())
            .collect();
        let mut queue: Vec<String> = reached.iter().cloned().collect();
        while let Some(path) = queue.pop() {
            let asset = find(&path).unwrap_or_else(|| panic!("{path} is referenced but not embedded"));
            for dep in imports(&path, asset.body) {
                assert!(find(&dep).is_some(), "{path} imports {dep}, which is not embedded");
                if reached.insert(dep.clone()) {
                    queue.push(dep);
                }
            }
        }
        reached.insert("index.html".into());
        let unused: Vec<_> = ASSETS.iter().map(|a| a.path).filter(|p| !reached.contains(*p)).collect();
        assert!(unused.is_empty(), "embedded but unreachable: {unused:?}");
    }

    /// Only the protocol registry knows the protocols: generic pages ask it (`protocol(...)`).
    #[test]
    fn only_the_protocol_registry_names_protocols() {
        let word = |body: &str, w: &str| {
            let lower = body.to_lowercase();
            lower.match_indices(w).any(|(i, _)| {
                let before = lower[..i].chars().next_back().is_none_or(|c| !c.is_ascii_alphanumeric());
                let after = lower[i + w.len()..].chars().next().is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_');
                before && after
            })
        };
        for a in ASSETS.iter().filter(|a| (a.path.ends_with(".js") || a.path.ends_with(".html")) && a.path != "js/protocols.js") {
            for w in ["p25", "dmr", "nac"] {
                assert!(!word(a.body, w), "{} names {w}: ask js/protocols.js", a.path);
            }
            assert!(!a.body.to_lowercase().contains("tsbk"), "{} names TSBKs: ask js/protocols.js", a.path);
        }
    }

    #[test]
    fn the_version_changes_with_the_build() {
        assert!(version().starts_with(BUILD_TAG));
        assert_eq!(content_type("js/main.js"), "text/javascript; charset=utf-8");
    }
}

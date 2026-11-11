//! The web UI: plain HTML, CSS and ES modules under `static/`, embedded at compile time (no
//! build step, no CDN: the unit may have no internet). The one built file is the ATSC player's
//! WebAssembly module (`player/atsc_player.wasm`, from `scanner/player` by
//! `tools/build_player.sh`); a test checks it was built from the crate's sources as they are.
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
    body: &'static [u8],
}

impl Asset {
    /// A text asset's text (none for the WebAssembly module).
    fn text(&self) -> &'static str {
        std::str::from_utf8(self.body).unwrap_or("")
    }
}

macro_rules! assets {
    ($($p:literal),* $(,)?) => {
        &[$(Asset { path: $p, body: include_bytes!(concat!("static/", $p)) }),*]
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
    "js/prefs.js",
    "js/modules.js",
    "js/aliases.js",
    "js/views/now.js",
    "js/views/now_calls.js",
    "js/views/talkgroups.js",
    "js/views/systems.js",
    "js/views/settings.js",
    "js/views/diagnostics.js",
    "js/views/activity.js",
    "js/views/packet_data.js",
    "js/views/scan.js",
    "js/views/channels.js",
    "js/views/viewer.js",
    "js/views/cards.js",
    "js/views/window.js",
    "js/views/spectrum.js",
    "js/views/board.js",
    "js/audio/player.js",
    "js/audio/ring.js",
    "js/audio/sources.js",
    "js/audio/gate.js",
    "js/audio/recording.js",
    "js/tv/player.js",
    "js/tv/ring.js",
    "js/tv/screen.js",
    "js/tv/sources.js",
    "player/atsc_player.wasm",
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
        // As WebAssembly.compileStreaming requires.
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// `<build tag>.<FNV-1a of every asset>`.
fn version() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        let mut hash: u32 = 0x811c_9dc5;
        for a in ASSETS {
            for &b in a.path.as_bytes().iter().chain(a.body) {
                hash ^= b as u32;
                hash = hash.wrapping_mul(0x0100_0193);
            }
        }
        format!("{BUILD_TAG}.{hash:08x}")
    })
}

async fn index() -> Response {
    let body = find("index.html").map(|a| a.text().replace(PLACEHOLDER, version())).unwrap_or_default();
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

    /// Relative imports of a module, and the files it names by `new URL(..., import.meta.url)`,
    /// resolved against its directory.
    fn imports(path: &str, body: &str) -> Vec<String> {
        let dir: Vec<&str> = path.rsplit_once('/').map_or(vec![], |(d, _)| d.split('/').collect());
        body.lines()
            .filter(|l| l.trim_start().starts_with("import ") || l.contains("import.meta.url"))
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
        let index = find("index.html").unwrap().text();
        let mut reached: BTreeSet<String> = index
            .split(&format!("ui/{PLACEHOLDER}/"))
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().to_string())
            .collect();
        let mut queue: Vec<String> = reached.iter().cloned().collect();
        while let Some(path) = queue.pop() {
            let asset = find(&path).unwrap_or_else(|| panic!("{path} is referenced but not embedded"));
            for dep in imports(&path, asset.text()) {
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
                assert!(!word(a.text(), w), "{} names {w}: ask js/protocols.js", a.path);
            }
            assert!(!a.text().to_lowercase().contains("tsbk"), "{} names TSBKs: ask js/protocols.js", a.path);
        }
    }

    #[test]
    fn the_version_changes_with_the_build() {
        assert!(version().starts_with(BUILD_TAG));
        assert_eq!(content_type("js/main.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("player/atsc_player.wasm"), "application/wasm");
    }

    /// FNV-1a (64 bits) of the player crate's sources: each file's path in the crate and its text
    /// with LF line ends, in path order.
    fn player_sources_hash() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("player");
        let mut files = vec!["Cargo.toml".to_string(), "Cargo.lock".to_string()];
        let mut dirs = vec![root.join("src")];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap().filter_map(|e| e.ok()) {
                let p = e.path();
                if p.is_dir() {
                    dirs.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    files.push(p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
        files.sort();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for f in &files {
            let text = std::fs::read_to_string(root.join(f)).unwrap().replace("\r\n", "\n");
            for b in f.bytes().chain([0]).chain(text.bytes()).chain([0]) {
                hash ^= u64::from(b);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        format!("{hash:016x}")
    }

    /// `static/player/atsc_player.source` holds the hash of the sources the committed module was
    /// built from: `tools/build_player.sh` builds it, then runs this with `PLAYER_BLESS=1`.
    #[test]
    fn the_player_module_is_built_from_its_sources() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/static/player/atsc_player.source");
        let hash = player_sources_hash();
        if std::env::var("PLAYER_BLESS").is_ok_and(|v| v == "1") {
            std::fs::write(&path, format!("{hash}\n")).unwrap();
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(have.trim(), hash, "scanner/player changed since its module was built: run tools/build_player.sh");
        assert!(find("player/atsc_player.wasm").is_some_and(|a| a.body.starts_with(b"\0asm")));
    }
}

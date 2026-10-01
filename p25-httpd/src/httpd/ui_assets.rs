//! Change 056: the web UI's static files, embedded at compile time.
//!
//! The UI is plain HTML + CSS + native ES modules under `ui/` (next
//! to this file): no bundler, no build step, no CDN — the radio often
//! has no internet. Every file is `include_str!`-ed here, so the
//! binary is still the whole deployment.
//!
//! URLs: `index.html` is served at `/` and references everything as
//! `ui/{{BUILD}}/<path>`; the placeholder is replaced by
//! [`asset_version`] (BUILD_TAG + a hash of the embedded files) when
//! the page is served, and module imports are relative, so a new
//! firmware — or even a rebuild without a BUILD_TAG bump — gets new
//! URLs and a browser never runs stale JS. Versioned assets are served
//! `immutable`; `index.html` itself `no-cache`.
//!
//! `ui_assets_tests.rs` checks that every file referenced by
//! `index.html` and every JS `import` resolves to an embedded asset,
//! so a missing module fails `cargo test` instead of the browser.

/// One embedded file, path relative to `ui/`.
pub struct UiAsset {
    pub path: &'static str,
    pub body: &'static str,
}

macro_rules! ui_assets {
    ($($p:literal),* $(,)?) => {
        &[$(UiAsset { path: $p, body: include_str!(concat!("ui/", $p)) }),*]
    };
}

/// Every file of the UI. Add new modules here (the tests fail if a
/// referenced file is missing from this table, or if a file here is
/// not reachable from `index.html`).
pub const UI_ASSETS: &[UiAsset] = ui_assets![
    "index.html",
    "css/base.css",
    "css/components.css",
    "js/main.js",
    "js/api.js",
    "js/store.js",
    "js/format.js",
    "js/dom.js",
    "js/audio/player.js",
    "js/audio/ring.js",
    "js/audio/sources.js",
    "js/views/now.js",
    "js/views/radio.js",
    "js/views/diagnostics.js",
    "js/views/settings.js",
    "js/views/systems.js",
    "js/views/activity.js",
    "js/components/site_card.js",
    // Change 075: the DMR control channel feed.
    "js/components/dmr_feed.js",
    "js/components/packet_data.js",
    "js/components/call_card.js",
    "js/components/calls_list.js",
    "js/components/log_view.js",
    "js/components/spectrum.js",
    "js/components/kv_table.js",
    "js/components/alias_editor.js",
    "js/components/monitor_picker.js",
    "js/components/speakers_panel.js",
    "js/components/ignore_list.js",
    "js/components/profile_picker.js",
    "js/components/coverage_card.js",
    "js/components/tg_groups_editor.js",
];

pub const INDEX_PATH: &str = "index.html";

/// Replaced in `index.html` by [`asset_version`].
pub const VERSION_PLACEHOLDER: &str = "{{BUILD}}";

pub fn find(path: &str) -> Option<&'static UiAsset> {
    UI_ASSETS.iter().find(|a| a.path == path)
}

/// MIME type by extension (`text/javascript` for modules: browsers
/// refuse to execute a module served as anything else).
pub fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    }
}

/// 32-bit FNV-1a over every embedded file (paths and bodies).
fn content_hash() -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for a in UI_ASSETS {
        for b in a.path.bytes().chain(a.body.bytes()) {
            h ^= b as u32;
            h = h.wrapping_mul(0x0100_0193);
        }
    }
    h
}

/// URL version segment: `<BUILD_TAG>.<hash>`. Computed once.
pub fn asset_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| format!("{}.{:08x}", crate::BUILD_TAG, content_hash()))
}

/// `index.html` with the version placeholder filled in.
pub fn render_index() -> String {
    find(INDEX_PATH)
        .map(|a| a.body.replace(VERSION_PLACEHOLDER, asset_version()))
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "ui_assets_tests.rs"]
mod tests;

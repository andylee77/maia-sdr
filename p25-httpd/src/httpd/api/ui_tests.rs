//! Host tests for the change-056 page / asset handlers in
//! `httpd::api::ui`. Attached via
//! `#[cfg(test)] #[path = "ui_tests.rs"] mod tests;`.

use super::*;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().build().unwrap()
}

fn header(r: &Response, name: header::HeaderName) -> String {
    r.headers()
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

fn asset(version: &str, path: &str, headers: HeaderMap) -> Response {
    rt().block_on(get_ui_asset(Path((version.to_string(), path.to_string())), headers))
}

#[test]
fn index_is_uncached_and_versioned() {
    let r = rt().block_on(get_ui_index());
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(header(&r, header::CACHE_CONTROL), "no-cache");
    assert!(header(&r, header::CONTENT_TYPE).starts_with("text/html"));
}

#[test]
fn current_version_assets_are_immutable_modules() {
    let r = asset(ui_assets::asset_version(), "js/main.js", HeaderMap::new());
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(header(&r, header::CONTENT_TYPE), "text/javascript; charset=utf-8");
    assert!(header(&r, header::CACHE_CONTROL).contains("immutable"));
    assert_eq!(header(&r, header::ETAG), format!("\"{}\"", ui_assets::asset_version()));
}

#[test]
fn stale_version_is_served_uncached() {
    let r = asset("2020-01-01-old.deadbeef", "css/base.css", HeaderMap::new());
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(header(&r, header::CACHE_CONTROL), "no-cache");
    assert!(header(&r, header::CONTENT_TYPE).starts_with("text/css"));
}

#[test]
fn etag_revalidation_and_missing_files() {
    let mut h = HeaderMap::new();
    h.insert(
        header::IF_NONE_MATCH,
        format!("\"{}\"", ui_assets::asset_version()).parse().unwrap(),
    );
    assert_eq!(asset(ui_assets::asset_version(), "js/api.js", h).status(), StatusCode::NOT_MODIFIED);
    assert_eq!(asset(ui_assets::asset_version(), "js/nope.js", HeaderMap::new()).status(), StatusCode::NOT_FOUND);
    assert_eq!(asset(ui_assets::asset_version(), "../main.rs", HeaderMap::new()).status(), StatusCode::NOT_FOUND);
}

//! Host tests for `httpd::ui_assets` (change 056): every file the UI
//! references is embedded, nothing embedded is dead, nothing is loaded
//! from the network, and modules stay small. Attached via
//! `#[cfg(test)] #[path = "ui_assets_tests.rs"] mod tests;`.

use super::*;
use std::collections::{BTreeSet, VecDeque};

/// Soft cap on module size (one responsibility per file).
const MAX_LINES: usize = 420;

const PREFIX: &str = "ui/{{BUILD}}/";

/// `src="..."` / `href="..."` values in `html`.
fn html_refs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for attr in ["src=\"", "href=\""] {
        let mut rest = html;
        while let Some(i) = rest.find(attr) {
            rest = &rest[i + attr.len()..];
            if let Some(end) = rest.find('"') {
                out.push(rest[..end].to_string());
                rest = &rest[end..];
            }
        }
    }
    out
}

/// Text between the first `quote` after `pat` and the next `quote`.
fn quoted_after(s: &str, pat: &str) -> Option<String> {
    let rest = &s[s.find(pat)? + pat.len()..];
    let q = rest.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = &rest[1..];
    Some(body[..body.find(q)?].to_string())
}

/// Module specifiers of static imports / re-exports (statements that
/// start a line: `import ... from '...'`, `import '...'`, `export ...
/// from '...'`) and dynamic `import('...')` anywhere. Line-based so a
/// string like `' from '` inside code is not mistaken for an import.
fn js_imports(js: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in js.lines() {
        let t = line.trim_start();
        // `} from '...'` closes a multi-line import list.
        if t.starts_with("import ") || t.starts_with("export ") || t.starts_with("} from ") {
            if let Some(spec) = quoted_after(t, " from ") {
                out.push(spec);
            } else if let Some(spec) = quoted_after(t, "import ") {
                out.push(spec);
            }
        }
        let mut rest = t;
        while let Some(i) = rest.find("import(") {
            if let Some(spec) = quoted_after(&rest[i..], "import(") {
                out.push(spec);
            }
            rest = &rest[i + 7..];
        }
    }
    out
}

/// Resolve a relative specifier against the importing module's path.
fn resolve(from: &str, spec: &str) -> Option<String> {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return None; // bare or absolute specifier: not allowed
    }
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop(); // the importing file itself
    for seg in spec.split('/') {
        match seg {
            "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

#[test]
fn asset_table_is_consistent() {
    let mut seen = BTreeSet::new();
    for a in UI_ASSETS {
        assert!(seen.insert(a.path), "duplicate asset {}", a.path);
        assert_ne!(content_type(a.path), "application/octet-stream", "{}", a.path);
        assert!(!a.body.trim().is_empty(), "{} is empty", a.path);
    }
    assert!(find(INDEX_PATH).is_some());
    assert_eq!(content_type("js/main.js"), "text/javascript; charset=utf-8");
}

#[test]
fn index_references_resolve() {
    let index = find(INDEX_PATH).unwrap().body;
    let refs = html_refs(index);
    assert!(refs.iter().any(|r| r.ends_with("js/main.js")), "{refs:?}");
    for r in refs {
        if r.starts_with('#') || r.starts_with("data:") || r == "/legacy" {
            continue;
        }
        let path = r.strip_prefix(PREFIX)
            .unwrap_or_else(|| panic!("index.html ref {r:?} must start with {PREFIX}"));
        assert!(find(path).is_some(), "index.html references missing asset {path}");
    }
}

#[test]
fn every_js_import_resolves() {
    for a in UI_ASSETS.iter().filter(|a| a.path.ends_with(".js")) {
        for spec in js_imports(a.body) {
            let path = resolve(a.path, &spec)
                .unwrap_or_else(|| panic!("{}: import {spec:?} must be relative", a.path));
            assert!(find(&path).is_some(), "{}: import {spec:?} -> missing {path}", a.path);
        }
    }
}

#[test]
fn every_asset_is_reachable_from_index() {
    let index = find(INDEX_PATH).unwrap().body;
    let mut reached: BTreeSet<String> = BTreeSet::new();
    reached.insert(INDEX_PATH.to_string());
    let mut queue: VecDeque<String> = html_refs(index)
        .into_iter()
        .filter_map(|r| r.strip_prefix(PREFIX).map(str::to_string))
        .collect();
    while let Some(p) = queue.pop_front() {
        if !reached.insert(p.clone()) {
            continue;
        }
        if let Some(a) = find(&p) {
            if p.ends_with(".js") {
                for spec in js_imports(a.body) {
                    if let Some(next) = resolve(&p, &spec) {
                        queue.push_back(next);
                    }
                }
            }
        }
    }
    for a in UI_ASSETS {
        assert!(reached.contains(a.path), "{} is embedded but never loaded", a.path);
    }
}

#[test]
fn nothing_is_loaded_from_the_network() {
    for a in UI_ASSETS {
        for bad in ["src=\"http", "href=\"http", "src=\"//", "href=\"//",
                    "from 'http", "from \"http", "url(http", "@import"] {
            assert!(!a.body.contains(bad), "{} contains {bad:?}", a.path);
        }
    }
}

#[test]
fn modules_stay_small() {
    for a in UI_ASSETS {
        let n = a.body.lines().count();
        assert!(n <= MAX_LINES, "{} has {n} lines (max {MAX_LINES})", a.path);
    }
}

#[test]
fn index_is_versioned_on_render() {
    let html = render_index();
    assert!(!html.contains(VERSION_PLACEHOLDER));
    assert!(html.contains(&format!("ui/{}/js/main.js", asset_version())));
    assert!(asset_version().starts_with(crate::BUILD_TAG));
}

#[test]
fn import_scanner_ignores_strings() {
    let js = "import { a } from './a.js';\nimport * as b from \"../b.js\";\n\
              export { c } from './c.js';\nimport './side.js';\n\
              const s = 'call from ' + x + ' from ' + y;\nconst m = await import('./lazy.js');\n";
    assert_eq!(js_imports(js), vec!["./a.js", "../b.js", "./c.js", "./side.js", "./lazy.js"]);
}

#[test]
fn resolver_handles_parent_dirs() {
    assert_eq!(resolve("js/views/now.js", "../store.js").as_deref(), Some("js/store.js"));
    assert_eq!(resolve("js/main.js", "./views/now.js").as_deref(), Some("js/views/now.js"));
    assert_eq!(resolve("js/main.js", "store.js"), None);
    assert_eq!(resolve("js/main.js", "../../../x.js"), None);
}

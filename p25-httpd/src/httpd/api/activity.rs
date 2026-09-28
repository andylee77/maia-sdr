//! Change 072: the activity history (`services::history`).
//!
//! Every endpoint takes `site` (default: the active site) and a window:
//! `from` / `to` (unix ms) or `hours` back from now (default 24). Totals
//! and series start at the hour `from` is in; call listings at `from`.
//!
//! - `GET /api/activity/sites`: sites with history; where it is kept,
//!   space used and the limit.
//! - `GET /api/activity/summary`: calls, airtime, talkgroups, radios.
//! - `GET /api/activity/talkgroups?limit=`: by airtime, with names.
//! - `GET /api/activity/radios?limit=`: by airtime, with names.
//! - `GET /api/activity/radio/{unit}`: the talkgroups a radio used and
//!   its affiliations / registrations.
//! - `GET /api/activity/talkgroup/{tg}`: its radios, encryption history.
//! - `GET /api/activity/series?bucket=hour|day&tz=<min east of UTC>&tg=&unit=`
//! - `GET /api/activity/calls?tg=&unit=&limit=&format=csv`

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};

use crate::httpd::AppState;
use crate::services::history::{HistoryStore, Range, SeriesFilter};

type Params = HashMap<String, String>;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn num<T: std::str::FromStr>(p: &Params, k: &str) -> Option<T> {
    p.get(k).and_then(|v| v.parse().ok())
}

fn range(state: &AppState, p: &Params) -> Range {
    let now = now_unix_ms();
    let to = num::<u64>(p, "to").unwrap_or(now);
    let hours = num::<u64>(p, "hours").unwrap_or(24).clamp(1, 24 * 400);
    let from = num::<u64>(p, "from").unwrap_or(to.saturating_sub(hours * 3_600_000));
    let site = p.get("site").cloned().unwrap_or_else(|| state.lo_plans.site());
    Range { site, from_ms: from, to_ms: to }
}

/// The site's talkgroup / radio names (the live site's, or the stored
/// ones of another site).
fn names(state: &AppState, site: &str) -> (BTreeMap<u16, String>, BTreeMap<u32, String>) {
    let s = state.ui_settings.snapshot();
    if s.site == site {
        return (s.tg_aliases, s.unit_aliases);
    }
    s.sites.get(site).map(|e| (e.tg_aliases.clone(), e.unit_aliases.clone())).unwrap_or_default()
}

fn no_store() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, Json(serde_json::json!({ "ok": false, "error": "no history database" }))).into_response()
}

/// Run a query on the store off the async runtime.
async fn query<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&HistoryStore) -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, Response> {
    let Some(store) = state.history.clone() else { return Err(no_store()) };
    match tokio::task::spawn_blocking(move || f(&store)).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "ok": false, "error": e.to_string() }))).into_response()),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "ok": false, "error": e.to_string() }))).into_response()),
    }
}

/// The window asked for; totals (from the hourly tables) start at
/// `first_hour_ms`.
fn window(r: &Range) -> serde_json::Value {
    serde_json::json!({ "site": r.site, "from_ms": r.from_ms, "to_ms": r.to_ms, "first_hour_ms": r.first_hour() })
}

pub async fn get_sites(State(state): State<Arc<AppState>>) -> Response {
    let (path, on_sd) = match &state.history {
        Some(s) => (Some(s.path.display().to_string()), s.on_sd),
        None => (None, false),
    };
    let (sites, used, size) = match query(&state, |s| Ok((s.sites()?, s.used_bytes()?, s.size_bytes()))).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let max = if on_sd { crate::services::history::MAX_BYTES_SD } else { crate::services::history::MAX_BYTES_RAM };
    let labels: HashMap<String, String> = crate::services::sites::list_sites()
        .into_iter()
        .filter_map(|n| crate::services::sites::load_site(&n).ok().map(|s| (n, s.label)))
        .collect();
    let sites: Vec<serde_json::Value> = sites
        .into_iter()
        .map(|s| {
            let mut v = serde_json::to_value(&s).unwrap_or_default();
            v["label"] = labels.get(&s.site).cloned().unwrap_or_else(|| s.site.clone()).into();
            v
        })
        .collect();
    Json(serde_json::json!({
        "ok": true, "active": state.lo_plans.site(), "sites": sites,
        "database": path, "on_sd": on_sd, "size_bytes": size, "used_bytes": used, "max_bytes": max,
        "retention_days": crate::services::history::RETENTION_DAYS,
    }))
    .into_response()
}

pub async fn get_summary(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let q = r.clone();
    match query(&state, move |s| s.summary(&q)).await {
        Ok(v) => Json(serde_json::json!({ "ok": true, "window": window(&r), "summary": v })).into_response(),
        Err(e) => e,
    }
}

pub async fn get_talkgroups(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let limit = num::<usize>(&p, "limit").unwrap_or(50).clamp(1, 1000);
    let q = r.clone();
    let rows = match query(&state, move |s| s.talkgroups(&q, limit)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tg_names, _) = names(&state, &r.site);
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|t| {
            let mut v = serde_json::to_value(&t).unwrap_or_default();
            v["alias"] = tg_names.get(&t.tg).cloned().into();
            v
        })
        .collect();
    Json(serde_json::json!({ "ok": true, "window": window(&r), "items": items })).into_response()
}

pub async fn get_radios(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let limit = num::<usize>(&p, "limit").unwrap_or(50).clamp(1, 1000);
    let q = r.clone();
    let rows = match query(&state, move |s| s.radios(&q, limit)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (_, unit_names) = names(&state, &r.site);
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|u| {
            let mut v = serde_json::to_value(&u).unwrap_or_default();
            v["alias"] = unit_names.get(&u.unit).cloned().into();
            v
        })
        .collect();
    Json(serde_json::json!({ "ok": true, "window": window(&r), "items": items })).into_response()
}

pub async fn get_radio(State(state): State<Arc<AppState>>, Path(unit): Path<u32>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let q = r.clone();
    let d = match query(&state, move |s| s.radio(&q, unit)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tg_names, unit_names) = names(&state, &r.site);
    let mut v = serde_json::to_value(&d).unwrap_or_default();
    v["alias"] = unit_names.get(&unit).cloned().into();
    if let Some(list) = v["talkgroups"].as_array_mut() {
        for t in list {
            let tg = t["tg"].as_u64().unwrap_or(0) as u16;
            t["alias"] = tg_names.get(&tg).cloned().into();
        }
    }
    if let Some(list) = v["events"].as_array_mut() {
        for t in list {
            let tg = t["tg"].as_u64().unwrap_or(0) as u16;
            t["alias"] = tg_names.get(&tg).cloned().into();
        }
    }
    Json(serde_json::json!({ "ok": true, "window": window(&r), "radio": v })).into_response()
}

pub async fn get_talkgroup(State(state): State<Arc<AppState>>, Path(tg): Path<u16>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let q = r.clone();
    let d = match query(&state, move |s| s.talkgroup(&q, tg)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    let (tg_names, unit_names) = names(&state, &r.site);
    let mut v = serde_json::to_value(&d).unwrap_or_default();
    v["alias"] = tg_names.get(&tg).cloned().into();
    if let Some(list) = v["radios"].as_array_mut() {
        for u in list {
            let unit = u["unit"].as_u64().unwrap_or(0) as u32;
            u["alias"] = unit_names.get(&unit).cloned().into();
        }
    }
    Json(serde_json::json!({ "ok": true, "window": window(&r), "talkgroup": v })).into_response()
}

fn filter(p: &Params) -> SeriesFilter {
    SeriesFilter { tg: num(p, "tg"), unit: num(p, "unit") }
}

pub async fn get_series(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let bucket = match p.get("bucket").map(String::as_str) {
        Some("day") => 86_400_000,
        Some("hour") | None => 3_600_000,
        Some(other) => {
            return (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "ok": false, "error": format!("bucket: hour or day, not {other}") }))).into_response();
        }
    };
    let tz = num::<i64>(&p, "tz").unwrap_or(0).clamp(-14 * 60, 14 * 60);
    let f = filter(&p);
    let q = r.clone();
    match query(&state, move |s| s.series(&q, bucket, tz, f)).await {
        Ok(v) => Json(serde_json::json!({ "ok": true, "window": window(&r), "bucket_ms": bucket, "tz": tz, "buckets": v })).into_response(),
        Err(e) => e,
    }
}

pub async fn get_calls(State(state): State<Arc<AppState>>, Query(p): Query<Params>) -> Response {
    let r = range(&state, &p);
    let csv = p.get("format").map(String::as_str) == Some("csv");
    let limit = num::<usize>(&p, "limit").unwrap_or(if csv { 100_000 } else { 200 }).clamp(1, 1_000_000);
    let f = filter(&p);
    let q = r.clone();
    let rows = match query(&state, move |s| s.calls(&q, f, limit)).await {
        Ok(v) => v,
        Err(e) => return e,
    };
    if csv {
        let name = format!("activity_{}_{}.csv", r.site, crate::services::history::iso_utc(r.from_ms).replace([' ', ':'], "-"));
        return (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            ],
            crate::services::history::to_csv(&rows),
        )
            .into_response();
    }
    Json(serde_json::json!({ "ok": true, "window": window(&r), "items": rows })).into_response()
}

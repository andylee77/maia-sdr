//! `/api/v1/activity/...`: the call history (`services::history`), with the site's system's
//! talkgroup and radio names.
//!
//! Every query takes `site` (default: the live site) or `system` (all its sites) and a window:
//! `from` / `to` (unix ms) or `hours` back from now (default 24). Totals and series start at the
//! hour `from` is in; call listings at `from`.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::v1::calls::{NamedCall, Names};
use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::aliases::AliasIndex;
use crate::trunking::trunk::view_of_row;
use crate::services::history::store::{self, Bucket, CallRow, Range, SeriesFilter, SiteStat, Summary, HOUR_MS};
use crate::trunking::site::LiveState;
use crate::util::time::{iso_utc, unix_ms};

/// Calls a JSON listing returns at most (the CSV export streams more).
const JSON_CALLS_MAX: usize = 5_000;

#[derive(Deserialize, Default)]
pub struct Window {
    pub site: Option<String>,
    pub system: Option<String>,
    pub from: Option<u64>,
    pub to: Option<u64>,
    pub hours: Option<u64>,
    pub limit: Option<usize>,
    pub tg: Option<u32>,
    pub unit: Option<u32>,
    /// `hour` or `day`.
    pub bucket: Option<String>,
    /// Minutes east of UTC that day buckets start at midnight of.
    pub tz: Option<i64>,
    /// `csv` for a call listing as a file.
    pub format: Option<String>,
}

/// The window asked for; totals (from the hourly tables) start at `first_hour_ms`.
#[derive(Serialize)]
pub struct WindowView {
    /// The site, or with `system` the system.
    pub site: String,
    pub system: bool,
    pub from_ms: u64,
    pub to_ms: u64,
    pub first_hour_ms: u64,
}

async fn range(s: &AppState, w: &Window) -> Result<Range, ApiError> {
    let to = w.to.unwrap_or_else(unix_ms);
    let hours = w.hours.unwrap_or(24).clamp(1, 24 * 400);
    let from = w.from.unwrap_or(to.saturating_sub(hours * HOUR_MS));
    if let Some(system) = &w.system {
        return Ok(Range { site: system.clone(), by_system: true, from_ms: from, to_ms: to });
    }
    let site = match &w.site {
        Some(site) => site.clone(),
        None => live_site(s).await.ok_or_else(|| ApiError::bad_request("no live site: name one with `site`"))?,
    };
    Ok(Range::site(&site, from, to))
}

async fn live_site(s: &AppState) -> Option<String> {
    match s.live.state() {
        LiveState::Live(l) => Some(l.site.id.clone()),
        _ => s.config.lock().await.state.value.live_site.clone(),
    }
}

fn window(r: &Range) -> WindowView {
    WindowView { site: r.site.clone(), system: r.by_system, from_ms: r.from_ms, to_ms: r.to_ms, first_hour_ms: r.first_hour() }
}

/// The aliases of the range's system.
async fn names(s: &AppState, r: &Range) -> AliasIndex {
    let c = s.config.lock().await;
    let sys = if r.by_system { c.systems.value.system(&r.site) } else { c.systems.value.site(&r.site).map(|(sys, _)| sys) };
    sys.map(|sys| sys.alias_index()).unwrap_or_default()
}

fn tg_name(ix: &AliasIndex, tg: u32) -> Option<String> {
    ix.talkgroup(tg).map(|a| a.name.clone())
}

fn radio_name(ix: &AliasIndex, unit: u32) -> Option<String> {
    ix.radio(unit).map(|a| a.name.clone())
}

/// A row with its name.
#[derive(Serialize)]
pub struct Named<T> {
    #[serde(flatten)]
    pub row: T,
    pub alias: Option<String>,
}

#[derive(Serialize)]
pub struct SiteView {
    #[serde(flatten)]
    pub stat: SiteStat,
    pub label: String,
    pub system: Option<String>,
}

#[derive(Serialize)]
pub struct Sites {
    pub active: Option<String>,
    pub sites: Vec<SiteView>,
    pub database: String,
    pub on_sd: bool,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub max_bytes: u64,
    pub retention_days: u32,
    /// What opening the history did (the copy of p25-httpd's).
    pub note: String,
}

pub async fn sites(State(s): State<Arc<AppState>>) -> ApiResult<Sites> {
    let h = s.history.clone();
    let (stats, used) = h.query(|st| Ok((st.sites()?, st.used_bytes()?))).await?;
    let c = s.config.lock().await;
    let sites = stats
        .into_iter()
        .map(|stat| {
            let found = c.systems.value.site(&stat.site);
            SiteView {
                label: found.map_or_else(|| stat.site.clone(), |(_, site)| site.label.clone()),
                system: found.map(|(sys, _)| sys.id.clone()),
                stat,
            }
        })
        .collect();
    drop(c);
    Ok(Json(Sites {
        active: live_site(&s).await,
        sites,
        database: h.store().path.display().to_string(),
        on_sd: h.on_sd,
        size_bytes: h.store().size_bytes(),
        used_bytes: used,
        max_bytes: h.limits().max_bytes,
        retention_days: h.limits().retention_days,
        note: h.note.clone(),
    }))
}

#[derive(Serialize)]
pub struct SummaryView {
    pub window: WindowView,
    pub summary: Summary,
}

pub async fn summary(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> ApiResult<SummaryView> {
    let r = range(&s, &w).await?;
    let q = r.clone();
    let summary = s.history.query(move |st| st.summary(&q)).await?;
    Ok(Json(SummaryView { window: window(&r), summary }))
}

#[derive(Serialize)]
pub struct Items<T> {
    pub window: WindowView,
    pub items: Vec<T>,
}

pub async fn talkgroups(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> ApiResult<Items<Named<store::TgStat>>> {
    let r = range(&s, &w).await?;
    let limit = w.limit.unwrap_or(50).clamp(1, 1000);
    let q = r.clone();
    let rows = s.history.query(move |st| st.talkgroups(&q, limit)).await?;
    let ix = names(&s, &r).await;
    let items = rows.into_iter().map(|t| Named { alias: tg_name(&ix, t.tg), row: t }).collect();
    Ok(Json(Items { window: window(&r), items }))
}

pub async fn radios(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> ApiResult<Items<Named<store::RadioStat>>> {
    let r = range(&s, &w).await?;
    let limit = w.limit.unwrap_or(50).clamp(1, 1000);
    let q = r.clone();
    let rows = s.history.query(move |st| st.radios(&q, limit)).await?;
    let ix = names(&s, &r).await;
    let items = rows.into_iter().map(|u| Named { alias: radio_name(&ix, u.unit), row: u }).collect();
    Ok(Json(Items { window: window(&r), items }))
}

#[derive(Serialize)]
pub struct RadioView {
    pub unit: u32,
    pub alias: Option<String>,
    pub talkgroups: Vec<Named<store::RadioTg>>,
    pub events: Vec<Named<store::UnitEvent>>,
}

#[derive(Serialize)]
pub struct RadioReply {
    pub window: WindowView,
    pub radio: RadioView,
}

pub async fn radio(State(s): State<Arc<AppState>>, Path(unit): Path<u32>, Query(w): Query<Window>) -> ApiResult<RadioReply> {
    let r = range(&s, &w).await?;
    let q = r.clone();
    let d = s.history.query(move |st| st.radio(&q, unit)).await?;
    let ix = names(&s, &r).await;
    let radio = RadioView {
        unit,
        alias: radio_name(&ix, unit),
        talkgroups: d.talkgroups.into_iter().map(|t| Named { alias: tg_name(&ix, t.tg), row: t }).collect(),
        events: d.events.into_iter().map(|e| Named { alias: tg_name(&ix, e.tg), row: e }).collect(),
    };
    Ok(Json(RadioReply { window: window(&r), radio }))
}

#[derive(Serialize)]
pub struct TalkgroupView {
    pub tg: u32,
    pub alias: Option<String>,
    pub radios: Vec<Named<store::TgRadio>>,
    pub calls: u64,
    pub encrypted: u64,
    pub first_encrypted_ms: Option<u64>,
    pub last_encrypted_ms: Option<u64>,
    pub last_clear_ms: Option<u64>,
    pub affiliated_radios: u64,
}

#[derive(Serialize)]
pub struct TalkgroupReply {
    pub window: WindowView,
    pub talkgroup: TalkgroupView,
}

pub async fn talkgroup(State(s): State<Arc<AppState>>, Path(tg): Path<u32>, Query(w): Query<Window>) -> ApiResult<TalkgroupReply> {
    let r = range(&s, &w).await?;
    let q = r.clone();
    let d = s.history.query(move |st| st.talkgroup(&q, tg)).await?;
    let ix = names(&s, &r).await;
    let talkgroup = TalkgroupView {
        tg: d.tg,
        alias: tg_name(&ix, tg),
        radios: d.radios.into_iter().map(|u| Named { alias: radio_name(&ix, u.unit), row: u }).collect(),
        calls: d.calls,
        encrypted: d.encrypted,
        first_encrypted_ms: d.first_encrypted_ms,
        last_encrypted_ms: d.last_encrypted_ms,
        last_clear_ms: d.last_clear_ms,
        affiliated_radios: d.affiliated_radios,
    };
    Ok(Json(TalkgroupReply { window: window(&r), talkgroup }))
}

#[derive(Serialize)]
pub struct Series {
    pub window: WindowView,
    pub bucket_ms: u64,
    pub tz: i64,
    pub buckets: Vec<Bucket>,
}

pub async fn series(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> ApiResult<Series> {
    let r = range(&s, &w).await?;
    let bucket_ms = match w.bucket.as_deref() {
        Some("day") => 24 * HOUR_MS,
        Some("hour") | None => HOUR_MS,
        Some(other) => return Err(ApiError::bad_request(format!("bucket: hour or day, not {other}"))),
    };
    let tz = w.tz.unwrap_or(0).clamp(-14 * 60, 14 * 60);
    let f = SeriesFilter { tg: w.tg, unit: w.unit };
    let q = r.clone();
    let buckets = s.history.query(move |st| st.series(&q, bucket_ms, tz, f)).await?;
    Ok(Json(Series { window: window(&r), bucket_ms, tz, buckets }))
}

/// Calls in the window, newest first, in `/calls`' shape with names; `format=csv` as a file of
/// history rows, streamed (up to a million).
pub async fn calls(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> Result<Response, ApiError> {
    let r = range(&s, &w).await?;
    let f = SeriesFilter { tg: w.tg, unit: w.unit };
    if w.format.as_deref() == Some("csv") {
        let limit = w.limit.unwrap_or(1_000_000).clamp(1, 1_000_000);
        let name = format!("activity_{}_{}.csv", r.site, iso_utc(r.from_ms).replace([' ', ':'], "-"));
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, std::io::Error>>(4);
        let store = s.history.store().clone();
        let q = r.clone();
        tokio::task::spawn_blocking(move || {
            let sent = store.export_csv(&q, f, limit, |chunk| tx.blocking_send(Ok(chunk)).is_ok());
            if let Err(e) = sent {
                let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
            }
        });
        let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|c| (c, rx)) }));
        return Ok((
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}\"")),
            ],
            body,
        )
            .into_response());
    }
    let limit = w.limit.unwrap_or(200).clamp(1, JSON_CALLS_MAX);
    let q = r.clone();
    let rows: Vec<CallRow> = s.history.query(move |st| st.calls(&q, f, limit)).await?;
    let names = Names::load(&s).await;
    let items: Vec<NamedCall> = rows.into_iter().map(|row| names.call(view_of_row(row))).collect();
    Ok(Json(Items { window: window(&r), items }).into_response())
}

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

use std::collections::HashSet;

use crate::api::v1::calls::{attach, named_alerts, stored_alerts, Dispatch, NamedAlert, NamedCall, Names};
use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config::aliases::AliasIndex;
use crate::trunking::trunk::view_of_row;
use crate::services::history::store::{self, AlertRow, Bucket, CallRow, Range, SeriesFilter, SiteStat, Summary, HOUR_MS};
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

/// Calls in the window, newest first, in `/calls`' shape with names, recordings and alerts;
/// `format=csv` as a file of history rows, streamed (up to a million).
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
    let mut items: Vec<NamedCall> = rows.into_iter().map(|row| names.call(view_of_row(row))).collect();
    let stored = stored_alerts(&s, &items).await?;
    attach(&s, &mut items, Some(stored));
    Ok(Json(Items { window: window(&r), items }).into_response())
}

/// Alerts heard alike: one kind with the same tones (each within 2 %).
#[derive(Serialize)]
pub struct AlertGroup {
    pub kind: String,
    /// Its tones, highest first, Hz: the mean of its alerts'.
    pub tones_hz: Vec<f32>,
    pub count: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    /// The radios that sent it and the talkgroups it was on, most first.
    pub sources: Vec<u32>,
    pub talkgroups: Vec<u32>,
}

#[derive(Serialize)]
pub struct AlertsReply {
    pub window: WindowView,
    /// Every alert in the window, grouped.
    pub groups: Vec<AlertGroup>,
    /// The newest (`limit`).
    pub items: Vec<NamedAlert>,
}

/// The alert tones heard in the window: grouped, and the newest each with its call and the call
/// that carried what it announced.
pub async fn alerts(State(s): State<Arc<AppState>>, Query(w): Query<Window>) -> ApiResult<AlertsReply> {
    let r = range(&s, &w).await?;
    let f = SeriesFilter { tg: w.tg, unit: w.unit };
    let limit = w.limit.unwrap_or(200).clamp(1, JSON_CALLS_MAX);
    let q = r.clone();
    let rows = s.history.query(move |st| st.alerts(&q, f, 1_000_000)).await?;
    let groups = group_alerts(&rows);
    let names = Names::load(&s).await;
    let mut items = named_alerts(&s, &names, rows.into_iter().take(limit).collect());
    let alerts: Vec<AlertRow> = items.iter().map(|a| a.alert.clone()).collect();
    let dispatches = s
        .history
        .query(move |st| {
            alerts
                .iter()
                .map(|a| {
                    let q = Range::site(&a.site, a.call_started_ms, a.call_started_ms + FOLLOW_SPAN_MS);
                    let mut calls = st.calls(&q, SeriesFilter { tg: Some(a.tg), unit: None }, 100)?;
                    calls.reverse();
                    Ok(dispatch_of(a, &calls))
                })
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
    let ids: HashSet<u64> = dispatches.iter().flatten().map(|d| d.call_id).collect();
    let recordings = s.recordings.of_calls(&ids);
    for (item, mut d) in items.iter_mut().zip(dispatches) {
        if let Some(d) = d.as_mut() {
            d.recorded = recordings.get(&d.call_id).is_some_and(|rs| {
                rs.iter().any(|r| (r.site == item.alert.site || r.site.is_empty()) && r.started_unix_ms.abs_diff(d.started_ms) < 10_000)
            });
        }
        item.dispatch = d;
    }
    Ok(Json(AlertsReply { window: window(&r), groups, items }))
}

/// How far after an alert's call its dispatch is looked for.
const FOLLOW_SPAN_MS: u64 = 120_000;
/// The sending radio's next transmission follows its last within this.
const FOLLOW_GAP_MS: u64 = 5_000;
/// The least voice a dispatch has (a shorter key-up carries no message).
const DISPATCH_MIN_MS: u64 = 4_000;

/// The call that carried what an alert announced: a console sends its alert, then keys again to
/// speak (sometimes a short, silent key-up between). The alert's own call when its voice runs on
/// 4 s past the tone; else the first of the sending radio's next transmissions on the talkgroup
/// with 4 s of voice, each within 5 s of the last, until another radio talks. `calls` are the
/// talkgroup's from the alert's call on, oldest first.
fn dispatch_of(a: &AlertRow, calls: &[CallRow]) -> Option<Dispatch> {
    let own = calls.iter().find(|c| c.call_id == a.call_id && c.started_ms == a.call_started_ms)?;
    let tone_end = a.offset_ms + a.duration_ms;
    let mut last_end = own.ended_ms;
    let mut next: Option<&CallRow> = None;
    for c in calls.iter().filter(|c| c.started_ms > own.started_ms) {
        if c.started_ms.saturating_sub(last_end) > FOLLOW_GAP_MS || c.source != a.source {
            break;
        }
        if c.voice_ms >= DISPATCH_MIN_MS {
            next = Some(c);
            break;
        }
        last_end = c.ended_ms;
    }
    match next {
        _ if own.voice_ms >= tone_end + DISPATCH_MIN_MS => Some(Dispatch {
            call_id: own.call_id,
            started_ms: own.started_ms,
            voice_ms: own.voice_ms - tone_end,
            offset_ms: tone_end,
            recorded: false,
        }),
        Some(c) => Some(Dispatch { call_id: c.call_id, started_ms: c.started_ms, voice_ms: c.voice_ms, offset_ms: 0, recorded: false }),
        None => None,
    }
}

fn group_alerts(rows: &[AlertRow]) -> Vec<AlertGroup> {
    struct Acc {
        kind: String,
        sums: Vec<f64>,
        count: u64,
        first_ms: u64,
        last_ms: u64,
        sources: Vec<(u32, u64)>,
        talkgroups: Vec<(u32, u64)>,
    }
    fn bump(list: &mut Vec<(u32, u64)>, id: u32) {
        match list.iter_mut().find(|(x, _)| *x == id) {
            Some((_, n)) => *n += 1,
            None => list.push((id, 1)),
        }
    }
    fn most_first(mut list: Vec<(u32, u64)>) -> Vec<u32> {
        list.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        list.into_iter().map(|(id, _)| id).collect()
    }
    let mut groups: Vec<Acc> = Vec::new();
    for a in rows {
        let mut tones = a.tones_hz.clone();
        tones.sort_by(|x, y| y.total_cmp(x));
        let alike = |g: &&mut Acc| {
            g.kind == a.kind
                && g.sums.len() == tones.len()
                && g.sums.iter().zip(&tones).all(|(sum, t)| ((sum / g.count as f64) as f32 - t).abs() <= 0.02 * t)
        };
        let g = match groups.iter_mut().find(alike) {
            Some(g) => g,
            None => {
                groups.push(Acc {
                    kind: a.kind.clone(),
                    sums: vec![0.0; tones.len()],
                    count: 0,
                    first_ms: a.at_ms,
                    last_ms: a.at_ms,
                    sources: Vec::new(),
                    talkgroups: Vec::new(),
                });
                groups.last_mut().expect("just pushed")
            }
        };
        for (sum, t) in g.sums.iter_mut().zip(&tones) {
            *sum += f64::from(*t);
        }
        g.count += 1;
        g.first_ms = g.first_ms.min(a.at_ms);
        g.last_ms = g.last_ms.max(a.at_ms);
        if let Some(src) = a.source {
            bump(&mut g.sources, src);
        }
        bump(&mut g.talkgroups, a.tg);
    }
    let mut out: Vec<AlertGroup> = groups
        .into_iter()
        .map(|g| AlertGroup {
            tones_hz: g.sums.iter().map(|sum| ((sum / g.count as f64) * 10.0).round() as f32 / 10.0).collect(),
            kind: g.kind,
            count: g.count,
            first_ms: g.first_ms,
            last_ms: g.last_ms,
            sources: most_first(g.sources),
            talkgroups: most_first(g.talkgroups),
        })
        .collect();
    out.sort_by(|a, b| b.count.cmp(&a.count).then(b.last_ms.cmp(&a.last_ms)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: u64, started: u64, ended: u64, source: u32, voice_ms: u64) -> CallRow {
        CallRow {
            site: "clay".into(),
            call_id: id,
            started_ms: started,
            ended_ms: ended,
            tg: 300,
            source: Some(source),
            sources: vec![source],
            freq_hz: None,
            channel: None,
            timeslot: None,
            lane: 1,
            encrypted: false,
            emergency: false,
            private: false,
            first_voice_ms: None,
            followed: true,
            not_followed: None,
            voice_ms,
            grant_ms: ended - started,
            codec: Some("imbe".into()),
            frames: voice_ms / 20,
            frame_errors: 0,
            close_reason: "tg_change".into(),
            end_kind: None,
        }
    }

    fn alert(call_id: u64, started: u64, duration_ms: u64) -> AlertRow {
        AlertRow {
            site: "clay".into(),
            call_id,
            call_started_ms: started,
            at_ms: started + 70,
            tg: 300,
            source: Some(1013),
            lane: 1,
            kind: "warble".into(),
            tones_hz: vec![806.5, 1506.2],
            segments: 5,
            offset_ms: 8,
            duration_ms,
        }
    }

    #[test]
    fn the_dispatch_is_the_consoles_longest_next_transmission() {
        // Clay, 07:48:17: the warble, a silent key-up, the dispatch, then a unit answers.
        let calls = [
            call(8225, 0, 1_401, 1013, 900),
            call(8226, 1_401, 4_040, 1013, 1_260),
            call(8227, 4_040, 9_910, 1013, 5_220),
            call(8228, 9_910, 15_277, 3_402_071, 3_060),
            call(8230, 23_361, 26_602, 1013, 900),
        ];
        let d = dispatch_of(&alert(8225, 0, 880), &calls).unwrap();
        assert_eq!((d.call_id, d.voice_ms, d.offset_ms), (8227, 5_220, 0));
        // 08:00:32: straight into the dispatch.
        let calls = [call(8280, 0, 1_800, 1013, 1_080), call(8281, 1_800, 12_040, 1013, 9_720), call(8282, 12_040, 14_726, 3_400_031, 1_440)];
        assert_eq!(dispatch_of(&alert(8280, 0, 1_000), &calls).map(|d| d.call_id), Some(8281));
        // Short key-ups are passed over; a pause over 5 s ends the console's turn.
        let calls = [call(1, 0, 1_400, 1013, 900), call(2, 2_000, 5_500, 1013, 3_000), call(3, 6_000, 13_000, 1013, 6_500), call(4, 13_100, 30_000, 1013, 16_000)];
        assert_eq!(dispatch_of(&alert(1, 0, 880), &calls).map(|d| d.call_id), Some(3));
        let late = [call(1, 0, 1_400, 1013, 900), call(2, 7_000, 15_000, 1013, 7_500)];
        assert_eq!(dispatch_of(&alert(1, 0, 880), &late), None);
    }

    #[test]
    fn a_warble_spoken_over_is_its_own_dispatch_and_a_lone_one_has_none() {
        let calls = [call(9, 0, 8_000, 1013, 7_000), call(10, 20_000, 23_000, 1013, 2_000)];
        let d = dispatch_of(&alert(9, 0, 1_100), &calls).unwrap();
        assert_eq!((d.call_id, d.offset_ms, d.voice_ms), (9, 1_108, 5_892));
        let lone = [call(11, 0, 1_400, 1013, 1_000), call(12, 30_000, 33_000, 1013, 2_000)];
        assert_eq!(dispatch_of(&alert(11, 0, 900), &lone), None);
    }
}

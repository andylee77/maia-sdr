//! `GET /api/v1/data`: packet data (`services::packet_data`) for a site (default the live one, `all`
//! for every site): totals by kind, the radios with packet data and the recent records, newest
//! first, with the radios' names; and the data channel the site announces.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::v1::activity::Named;
use crate::api::ApiResult;
use crate::boot::state::AppState;
use crate::services::config::aliases::AliasIndex;
use crate::services::packet_data::{DataRecord, RadioData, RECENT};
use crate::trunking::site::LiveState;

#[derive(Deserialize)]
pub struct Params {
    pub limit: Option<usize>,
    /// A site id, or `all`.
    pub site: Option<String>,
}

#[derive(Serialize)]
pub struct DataView {
    /// The site shown; `None` for every site.
    pub site: Option<String>,
    pub data_channel_hz: Option<u64>,
    pub pdus: u64,
    pub duplicates: u64,
    pub totals: BTreeMap<String, u64>,
    pub radios: Vec<Named<RadioData>>,
    pub recent: Vec<Named<DataRecord>>,
}

pub async fn get(State(s): State<Arc<AppState>>, Query(p): Query<Params>) -> ApiResult<DataView> {
    let limit = p.limit.unwrap_or(100).clamp(1, RECENT);
    let live = match s.live.state() {
        LiveState::Live(l) => Some(l.site.id.clone()),
        _ => None,
    };
    let site = match p.site.as_deref() {
        Some("all") => None,
        Some(id) => Some(id.to_string()),
        None => live.clone(),
    };
    // Radio names come from each record's own site's system.
    let names: HashMap<String, Arc<AliasIndex>> = {
        let c = s.config.lock().await;
        let mut by_site = HashMap::new();
        for sys in &c.systems.value.systems {
            let ix = Arc::new(sys.alias_index());
            for x in sys.sites.iter().filter(|x| site.as_deref().is_none_or(|w| w == x.id)) {
                by_site.insert(x.id.clone(), ix.clone());
            }
        }
        by_site
    };
    let data_channel_hz = match &site {
        Some(id) => s.live.learned(id).await.ok().and_then(|l| l.data_channel_hz),
        None => None,
    };
    let wants = |r: &str| site.as_deref().is_none_or(|w| w == r);
    let named = |site: &str, llid: u32| names.get(site).and_then(|ix| ix.radio(llid)).map(|a| a.name.clone());
    let view = s.packet_data.with(|d| {
        let mut radios: Vec<RadioData> = d.radios.values().filter(|r| wants(&r.site)).cloned().collect();
        radios.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
        radios.truncate(200);
        let counts = d.counts(site.as_deref());
        DataView {
            site: site.clone(),
            data_channel_hz,
            pdus: counts.pdus,
            duplicates: counts.duplicates,
            totals: counts.totals,
            radios: radios.into_iter().map(|r| Named { alias: named(&r.site, r.llid), row: r }).collect(),
            recent: d
                .recent
                .iter()
                .rev()
                .filter(|r| wants(&r.site))
                .take(limit)
                .map(|r| Named { alias: named(&r.site, r.llid), row: r.clone() })
                .collect(),
        }
    });
    Ok(Json(view))
}

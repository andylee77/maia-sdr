//! Change 074: packet data (`app::data_task`).
//!
//! `GET /api/data?limit=&site=`: the decoders' PDU counters, totals by
//! kind, radios with packet data (newest first) and the recent records
//! (newest first). `site` narrows radios and records to one site (`all`
//! for every site; default: the active site).

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::Json;

use crate::httpd::AppState;

pub async fn get_data(State(state): State<Arc<AppState>>, Query(p): Query<HashMap<String, String>>) -> Json<serde_json::Value> {
    let limit = p.get("limit").and_then(|v| v.parse::<usize>().ok()).unwrap_or(100).clamp(1, crate::app::data_task::RECENT);
    let active = state.lo_plans.site();
    let site = match p.get("site").map(String::as_str) {
        Some("all") => None,
        Some(s) => Some(s.to_string()),
        None => Some(active.clone()),
    };
    let mut decoders = Vec::new();
    for d in [&state.decoder, &state.lsm_decoder].into_iter().chain(state.data_decoders.iter()) {
        let d = d.read().await;
        decoders.push(serde_json::json!({
            "chain": d.chain_label,
            "active": d.active,
            "nids": d.nid_decoded_ok,
            "pdu_frames": d.pdu_frames,
            "pdu_header_crc_fail": d.pdu_header_crc_fail,
            "pdu_blocks": d.pdu_blocks,
        }));
    }
    // Radio names from the site's profile (the active site's for "all").
    let (_, names) = state.ui_settings.snapshot().aliases_for(site.as_deref().unwrap_or(&active));
    let named = |v: &mut serde_json::Value, llid: u32| {
        v["alias"] = names.get(&llid).cloned().into();
    };
    let data = state.data.lock().unwrap_or_else(|e| e.into_inner());
    let wants = |s: &str| site.as_deref().is_none_or(|w| w == s);
    let recent: Vec<serde_json::Value> = data
        .recent
        .iter()
        .rev()
        .filter(|r| wants(&r.site))
        .take(limit)
        .map(|r| {
            let mut v = serde_json::to_value(r).unwrap_or_default();
            named(&mut v, r.llid);
            v
        })
        .collect();
    let mut radios: Vec<_> = data.radios.values().filter(|r| wants(&r.site)).cloned().collect();
    radios.sort_by(|a, b| b.last_ms.cmp(&a.last_ms));
    radios.truncate(200);
    let radios: Vec<serde_json::Value> = radios
        .into_iter()
        .map(|r| {
            let mut v = serde_json::to_value(&r).unwrap_or_default();
            named(&mut v, r.llid);
            v
        })
        .collect();
    Json(serde_json::json!({
        "ok": true,
        "site": site,
        "active_site": active,
        "data_channel_hz": crate::app::data_task::data_channel_hz(),
        "pdus": data.pdus,
        "duplicates": data.duplicates,
        "totals": data.totals,
        "decoders": decoders,
        "radios": radios,
        "recent": recent,
    }))
}

//! Change 072: fills the activity history (`services::history`).
//!
//! Every 30 s the finished calls in the grant-summary rings (followed,
//! and encrypted / not followed) that ended 15+ s ago (their late
//! counts are in by then) are stored under the active site, with the
//! radio events (accepted affiliations, registrations) gathered
//! meanwhile. Batching keeps SD writes down (one transaction a flush).
//! Nothing is gathered during a sweep: the decoders hear other systems.
//! Pruned daily to `RETENTION_DAYS` and the size limit (`MAX_BYTES_*`).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::app::grant_stats::GrantDecodeSummary;
use crate::httpd::AppState;
use crate::protocol::p25::control_channel::UnitObservation;
use crate::services::history::{CallRow, HistoryStore, UnitEventKind, UnitNote, MAX_BYTES_RAM, MAX_BYTES_SD, RETENTION_DAYS};

const FLUSH: Duration = Duration::from_secs(30);
/// A call is stored this long after it ended.
const SETTLE_MS: u64 = 15_000;
/// Calls remembered as stored (by id and start) for this long.
const SEEN_MS: u64 = 30 * 60_000;
/// Distinct radio events kept between flushes (the rest are dropped).
const MAX_NOTES: usize = 20_000;

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A finished call as a history row.
pub fn to_row(g: &GrantDecodeSummary, site: &str) -> CallRow {
    let close_reason = serde_json::to_value(g.close_reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into());
    let source = g.source.or(g.actual_speaker).filter(|s| *s != 0);
    CallRow {
        site: site.to_string(),
        call_id: g.call_id,
        started_ms: g.started_unix_ms,
        ended_ms: g.ended_unix_ms,
        tg: g.tg,
        source,
        sources: g.sources_observed.clone(),
        freq_hz: g.freq_hz,
        chain: g.chain,
        encrypted: g.encrypted,
        followed: g.not_followed.is_none(),
        not_followed: g.not_followed.map(str::to_string),
        voice_ms: g.imbe_extracted * 20,
        // Grant to the last grant update; a call with no update was
        // shorter than their period: grant to close.
        grant_ms: g.air_duration_ms.unwrap_or_else(|| g.ended_unix_ms.saturating_sub(g.started_unix_ms)),
        imbe: g.imbe_extracted,
        vocoder_errors: g.vocoder_errors,
        close_reason,
    }
}

type NoteKey = (String, u32, u16, UnitEventKind);

/// Count a radio event into the pending notes (per site, radio,
/// talkgroup and kind).
fn gather(notes: &mut HashMap<NoteKey, UnitNote>, site: String, u: UnitObservation, now: u64) {
    let key = (site, u.unit, u.tg, u.kind);
    if let Some(n) = notes.get_mut(&key) {
        n.count += 1;
        n.last_ms = now;
    } else if notes.len() < MAX_NOTES {
        notes.insert(key, UnitNote { unit: u.unit, tg: u.tg, kind: u.kind, first_ms: now, last_ms: now, count: 1 });
    }
}

pub fn spawn_history_task(state: Arc<AppState>, store: Arc<HistoryStore>, mut units: mpsc::Receiver<UnitObservation>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(FLUSH);
        let mut seen: HashMap<(u64, u64), u64> = HashMap::new();
        let mut notes: HashMap<NoteKey, UnitNote> = HashMap::new();
        let mut last_prune = 0u64;
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                Some(u) = units.recv() => {
                    if state.radio_lease.is_normal() {
                        gather(&mut notes, state.lo_plans.site(), u, now_unix_ms());
                    }
                    continue;
                }
            }
            let now = now_unix_ms();
            let mut rows = Vec::new();
            if state.radio_lease.is_normal() {
                // (During a sweep the rings hold nothing of this site's.)
                let site = state.lo_plans.site();
                for ring in [&state.grant_decode_stats, &state.enc_grant_decode_stats] {
                    let Ok(r) = ring.lock() else { continue };
                    for g in r.iter() {
                        let key = (g.call_id, g.started_unix_ms);
                        if g.ended_unix_ms == 0 || g.ended_unix_ms + SETTLE_MS > now || seen.contains_key(&key) {
                            continue;
                        }
                        seen.insert(key, now);
                        rows.push(to_row(g, &site));
                    }
                }
            }
            seen.retain(|_, t| now.saturating_sub(*t) < SEEN_MS);
            let prune = now.saturating_sub(last_prune) > 24 * 3_600_000;
            if rows.is_empty() && notes.is_empty() && !prune {
                continue;
            }
            let mut by_site: HashMap<String, Vec<UnitNote>> = HashMap::new();
            for ((site, ..), n) in notes.drain() {
                by_site.entry(site).or_default().push(n);
            }
            let store = store.clone();
            let result = tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
                store.insert_calls(&rows)?;
                for (site, n) in &by_site {
                    store.note_units(site, n)?;
                }
                if !prune {
                    return Ok(0);
                }
                let old = store.prune(now.saturating_sub(RETENTION_DAYS * 86_400_000))?;
                let big = store.trim_to(if store.on_sd { MAX_BYTES_SD } else { MAX_BYTES_RAM })?;
                if big > 0 {
                    tracing::info!("history: {big} oldest calls removed to stay under the size limit");
                }
                Ok(old)
            })
            .await;
            match result {
                Ok(Ok(pruned)) => {
                    if prune {
                        last_prune = now;
                        if pruned > 0 {
                            tracing::info!("history: pruned {pruned} calls older than {RETENTION_DAYS} days");
                        }
                    }
                }
                Ok(Err(e)) => tracing::warn!("history: not stored: {e}"),
                Err(e) => tracing::warn!("history: store task failed: {e}"),
            }
        }
    });
}

/// Open the history database: on the SD card when it is mounted, else in RAM.
pub fn open_store() -> Option<Arc<HistoryStore>> {
    let sd_mounted = std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some(crate::audio::rec_storage::SD_MOUNT)))
        .unwrap_or(false);
    let (path, on_sd) = if sd_mounted {
        (crate::services::history::SD_PATH, true)
    } else {
        (crate::services::history::RAM_PATH, false)
    };
    let path = std::env::var("P25_HISTORY_DB").unwrap_or_else(|_| path.to_string());
    match HistoryStore::open(std::path::Path::new(&path), on_sd) {
        Ok(s) => {
            tracing::info!("history: {path} ({})", if on_sd { "SD card" } else { "RAM: lost on reboot" });
            Some(Arc::new(s))
        }
        Err(e) => {
            tracing::warn!("history: {path} not opened: {e}");
            None
        }
    }
}

//! Change 056: builders for the consolidated web-UI documents,
//! `GET /api/ui/state` and `GET /api/ui/calls` (types in
//! `p25_json::ui`).
//!
//! The HTTP handler (`httpd::api::ui`) takes short snapshots of the
//! live state (lifecycle's `ActiveCallSnapshot`, the grant-stats rings,
//! the recordings ring, the settings) and hands them to the pure
//! functions here, so the presentation rules are host-tested:
//!
//!   - call phase: `acquiring` → `voice` → `hang` (the lifecycle keeps
//!     a call open after its last voice until the end-of-transmission
//!     grace or the no-keep-alive timeout, change 057; the pre-056
//!     dashboard showed that hang time as an active call);
//!   - the recent-call list: grant summaries joined with recordings by
//!     call_id, with an explicit reason whenever there is no audio;
//!   - control-channel health from a short TSBK rate window.

use std::collections::{BTreeMap, VecDeque};

use p25_json::ui::{UiCall, UiCallSummary, UiRecordingRef};

use crate::app::grant_follower::ActiveCallSnapshot;
use crate::app::grant_stats::GrantDecodeSummary;
use crate::audio::recorder::RecordingEntry;

/// Voice within this window counts as "talking now".
pub const VOICE_HOLD_MS: u64 = 1_500;

/// A granted call with no voice yet is "acquiring" for this long,
/// then "hang".
pub const ACQUIRE_WINDOW_MS: u64 = 3_000;

/// Board clock before 2020-01-01 = never set.
pub const CLOCK_VALID_AFTER_MS: u64 = 1_577_836_800_000;

/// A closed call whose WAV is not in the ring yet is "saving" for this
/// long: the recorder finalises `CLOSING_DRAIN_MS` (2 s) after the
/// close, plus a tick and the write.
pub const SAVING_GRACE_MS: u64 = 3_500;

/// Acquired but no TSBK decoded for this long = stale control channel.
pub const CC_STALE_MS: u64 = 5_000;

pub fn clock_valid(now_unix_ms: u64) -> bool {
    now_unix_ms >= CLOCK_VALID_AFTER_MS
}

/// Talkgroup and radio-unit aliases (from `services::ui_settings`).
#[derive(Debug, Default, Clone, Copy)]
pub struct Aliases<'a> {
    pub tg: Option<&'a BTreeMap<u32, String>>,
    pub unit: Option<&'a BTreeMap<u32, String>>,
}

impl Aliases<'_> {
    pub fn tg(&self, tg: u32) -> Option<String> {
        self.tg.and_then(|m| m.get(&tg).cloned())
    }
    pub fn unit(&self, id: Option<u32>) -> Option<String> {
        id.and_then(|i| self.unit.and_then(|m| m.get(&i).cloned()))
    }
}

/// "acquiring" | "voice" | "hang".
pub fn call_phase(now: u64, started: u64, last_voice: Option<u64>) -> &'static str {
    match last_voice {
        Some(t) if now.saturating_sub(t) <= VOICE_HOLD_MS => "voice",
        None if now.saturating_sub(started) <= ACQUIRE_WINDOW_MS => "acquiring",
        _ => "hang",
    }
}

pub fn build_call(
    s: &ActiveCallSnapshot,
    now: u64,
    aliases: Aliases,
    recording: bool,
    // Change 066: traffic chain number (1 or 2).
    chain: u8,
) -> UiCall {
    // Change 057: the lifecycle publishes when (and by which rule) the
    // call will close; an end-of-transmission marker means the voice is
    // over even if the last chunks are still being played out.
    let phase = if s.close_via == "end" {
        "hang"
    } else {
        call_phase(now, s.started_unix_ms, s.last_voice_unix_ms)
    };
    UiCall {
        call_id: s.call_id,
        tg: s.tg,
        tg_alias: aliases.tg(s.tg),
        source: s.source,
        source_alias: aliases.unit(s.source),
        sources: s.sources_observed.clone(),
        freq_hz: s.freq_hz,
        channel: s.channel.clone(),
        encrypted: s.encrypted,
        started_unix_ms: s.started_unix_ms,
        elapsed_ms: now.saturating_sub(s.started_unix_ms),
        phase: phase.to_string(),
        voice_ms: s.voice_frames * 20,
        first_voice_unix_ms: s.first_voice_unix_ms,
        last_voice_unix_ms: s.last_voice_unix_ms,
        close_in_ms: s.close_at_unix_ms.saturating_sub(now),
        close_via: s.close_via.to_string(),
        close_window_ms: s.close_window_ms,
        end_lc: s.end_lc.map(str::to_string),
        recording: recording && !s.encrypted,
        chain,
    }
}

/// "ok" | "stale" | "searching".
pub fn site_health(acquired: bool, last_tsbk_age_ms: Option<u64>) -> &'static str {
    match (acquired, last_tsbk_age_ms) {
        (false, _) => "searching",
        (true, Some(a)) if a <= CC_STALE_MS => "ok",
        (true, _) => "stale",
    }
}

/// Rate of good TSBKs over the last ~10 s from cumulative decoder
/// counters (`tsbk_crc_ok`, `tsbk_crc_failures`). Samples are kept at
/// most once per second; a counter going backwards (decoder reset)
/// restarts the window.
#[derive(Debug, Default)]
pub struct RateWindow {
    samples: VecDeque<(u64, u64, u64)>,
}

impl RateWindow {
    const MIN_SPACING_MS: u64 = 1_000;
    const KEEP: usize = 11;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, t_ms: u64, ok: u64, fail: u64) {
        if let Some(&(lt, lok, lfail)) = self.samples.back() {
            if ok < lok || fail < lfail || t_ms < lt {
                self.samples.clear();
            } else if t_ms - lt < Self::MIN_SPACING_MS {
                return;
            }
        }
        self.samples.push_back((t_ms, ok, fail));
        while self.samples.len() > Self::KEEP {
            self.samples.pop_front();
        }
    }

    /// (good TSBKs per second, CRC-ok percent) over the window. `None`
    /// until two samples a second apart exist; the percentage is `None`
    /// when no block was attempted in the window.
    pub fn rates(&self) -> Option<(f64, Option<f64>)> {
        let (&(t0, ok0, f0), &(t1, ok1, f1)) = (self.samples.front()?, self.samples.back()?);
        if t1 <= t0 {
            return None;
        }
        let d_ok = (ok1 - ok0) as f64;
        let d_all = d_ok + (f1 - f0) as f64;
        let per_s = d_ok * 1000.0 / (t1 - t0) as f64;
        let pct = (d_all > 0.0).then(|| 100.0 * d_ok / d_all);
        Some((per_s, pct))
    }
}

/// Changes whenever either grant-stats ring or the recordings ring
/// changes. Inputs are newest-last slices of each ring's tail.
/// Change 057: plus `stats_rev` (a closed call's counters updated by
/// its late-decoded tail, `grant_stats::GrantStatsRev`) and
/// `rec_pending` (recordings still waiting for the SD card).
#[allow(clippy::too_many_arguments)]
pub fn calls_rev(
    clear_newest: Option<u64>,
    clear_len: usize,
    enc_newest: Option<u64>,
    enc_len: usize,
    rec_newest: Option<u64>,
    rec_len: usize,
    stats_rev: u64,
    rec_pending: usize,
) -> String {
    format!(
        "c{}.{}-e{}.{}-r{}.{}-s{}.{}",
        clear_newest.unwrap_or(0), clear_len,
        enc_newest.unwrap_or(0), enc_len,
        rec_newest.unwrap_or(0), rec_len,
        stats_rev, rec_pending,
    )
}

/// Options of `GET /api/ui/calls`.
#[derive(Debug, Clone)]
pub struct CallsQuery {
    pub limit: usize,
    /// Include encrypted and not-followed grants (zero-audio rows).
    pub include_not_followed: bool,
    /// Change 073: only this site's calls and recordings (`None`: all).
    pub site: Option<String>,
}

impl CallsQuery {
    fn wants(&self, site: &str) -> bool {
        self.site.as_deref().is_none_or(|w| w == site)
    }
}

/// Change 073: calls and recordings per site in the lists, most first.
pub fn site_counts(
    clear: &[GrantDecodeSummary],
    enc: &[GrantDecodeSummary],
    recs: &[RecordingEntry],
) -> Vec<(String, u64, u64)> {
    let mut m: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    for g in clear.iter().chain(enc) {
        m.entry(g.site.clone()).or_default().0 += 1;
    }
    for r in recs {
        m.entry(r.site.clone()).or_default().1 += 1;
    }
    let mut v: Vec<_> = m.into_iter().map(|(s, (c, r))| (s, c, r)).collect();
    v.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then(a.0.cmp(&b.0)));
    v
}

fn rec_ref(r: &RecordingEntry) -> UiRecordingRef {
    UiRecordingRef {
        id: r.id,
        url: format!("/api/recordings/{}.wav", r.id),
        duration_ms: r.duration_ms,
        size_bytes: r.size_bytes,
        filename: r.filename.clone(),
        storage: r.storage.to_string(),
    }
}

fn close_reason_str(g: &GrantDecodeSummary) -> String {
    serde_json::to_value(g.close_reason)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

/// Why a grant summary has no recording (or "recorded").
pub fn audio_status(
    g: &GrantDecodeSummary,
    rec: Option<&RecordingEntry>,
    now: u64,
    oldest_rec_id: Option<u64>,
    skipped: &dyn Fn(u64) -> bool,
) -> &'static str {
    if rec.is_some() {
        "recorded"
    } else if g.encrypted {
        "encrypted"
    } else if g.not_followed.is_some() {
        "not_followed"
    } else if g.imbe_extracted == 0 {
        "no_voice"
    } else if skipped(g.call_id) {
        "not_recorded"
    } else if now.saturating_sub(g.ended_unix_ms) < SAVING_GRACE_MS {
        "saving"
    } else if oldest_rec_id.map(|o| g.call_id < o).unwrap_or(false) {
        "evicted"
    } else {
        "missing"
    }
}

fn from_summary(
    g: &GrantDecodeSummary,
    rec: Option<&RecordingEntry>,
    aliases: Aliases,
    status: &str,
) -> UiCallSummary {
    let voice_ms = rec
        .map(|r| r.duration_ms)
        .unwrap_or(g.imbe_extracted * 20);
    let mut sources = g.sources_observed.clone();
    if sources.is_empty() {
        sources.extend(g.source);
    }
    UiCallSummary {
        call_id: g.call_id,
        tg: g.tg,
        tg_alias: aliases.tg(g.tg),
        source: g.source,
        source_alias: aliases.unit(g.source),
        sources,
        freq_hz: g.freq_hz,
        channel: g.channel.clone(),
        started_unix_ms: g.started_unix_ms,
        ended_unix_ms: g.ended_unix_ms,
        open_ms: g.duration_ms,
        voice_ms,
        air_ms: g.air_duration_ms,
        first_voice_ms: g.first_imbe_ms,
        imbe: g.imbe_extracted,
        ldu: g.ldu1_count + g.ldu2_count,
        vocoder_errors: g.vocoder_errors,
        vocoder_silent: g.vocoder_silent,
        encrypted: g.encrypted,
        not_followed: g.not_followed.map(str::to_string),
        close_reason: close_reason_str(g),
        recording: rec.map(rec_ref),
        audio_status: status.to_string(),
        chain: g.chain,
        site: g.site.clone(),
    }
}

/// A recording whose grant summary already rolled out of the ring (or
/// never existed): still listed so no audio is ever invisible.
fn from_orphan(r: &RecordingEntry, aliases: Aliases) -> UiCallSummary {
    let mut sources = r.sources_observed.clone();
    if sources.is_empty() {
        sources.extend(r.source);
    }
    UiCallSummary {
        call_id: r.id,
        tg: r.talkgroup,
        tg_alias: aliases.tg(r.talkgroup),
        source: r.source,
        source_alias: aliases.unit(r.source),
        sources,
        freq_hz: r.freq_hz,
        channel: r.channel.clone(),
        started_unix_ms: r.started_unix_ms,
        ended_unix_ms: r.started_unix_ms + r.duration_ms,
        open_ms: r.duration_ms,
        voice_ms: r.duration_ms,
        air_ms: None,
        first_voice_ms: r.first_chunk_after_open_ms,
        imbe: r.duration_ms / 20,
        ldu: r.ldu1_count.unwrap_or(0) + r.ldu2_count.unwrap_or(0),
        vocoder_errors: r.vocoder_errors.unwrap_or(0),
        vocoder_silent: r.vocoder_silent.unwrap_or(0),
        encrypted: false,
        not_followed: None,
        close_reason: "unknown".into(),
        recording: Some(rec_ref(r)),
        audio_status: "recorded".into(),
        chain: 0,
        site: r.site.clone(),
    }
}

/// Recent calls, newest first. `clear` / `enc` / `recs` are the rings
/// in storage order (oldest first).
pub fn build_calls(
    clear: &[GrantDecodeSummary],
    enc: &[GrantDecodeSummary],
    recs: &[RecordingEntry],
    aliases: Aliases,
    q: CallsQuery,
    now: u64,
    skipped: &dyn Fn(u64) -> bool,
) -> Vec<UiCallSummary> {
    let by_id: std::collections::HashMap<u64, &RecordingEntry> =
        recs.iter().map(|r| (r.id, r)).collect();
    // (Over every recording: "rolled out" is about the whole ring.)
    let oldest_rec_id = recs.iter().map(|r| r.id).min();
    let mut used = std::collections::HashSet::new();
    let mut items: Vec<UiCallSummary> = Vec::new();

    let enc_iter = enc.iter().filter(|_| q.include_not_followed);
    for g in clear.iter().chain(enc_iter).filter(|g| q.wants(&g.site)) {
        let rec = by_id.get(&g.call_id).copied();
        if rec.is_some() {
            used.insert(g.call_id);
        }
        let status = audio_status(g, rec, now, oldest_rec_id, skipped);
        items.push(from_summary(g, rec, aliases, status));
    }
    // Recordings paired only with the encrypted ring are still used
    // when that ring is hidden: never list them as orphans.
    for g in enc {
        if by_id.contains_key(&g.call_id) {
            used.insert(g.call_id);
        }
    }
    for r in recs.iter().filter(|r| q.wants(&r.site)) {
        if !used.contains(&r.id) {
            items.push(from_orphan(r, aliases));
        }
    }
    items.sort_by(|a, b| {
        b.started_unix_ms
            .cmp(&a.started_unix_ms)
            .then(b.call_id.cmp(&a.call_id))
    });
    items.truncate(q.limit);
    items
}

#[cfg(test)]
#[path = "ui_state_tests.rs"]
mod tests;

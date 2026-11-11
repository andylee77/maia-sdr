//! `GET /api/v1/talkgroups`: the live site's talkgroups as the live screen lists them: those
//! heard at the site in the last week or among the calls listed now, and those an alias sets how
//! to listen to (a name alone lists none: a system's aliases can be hundreds). Each says
//! how the follower treats it (by the follower's own rule, `trunking::follow::routing`), whether
//! its calls are recorded, and when it was last heard. `/ws/live` pushes the same view when it
//! changes.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::Json;
use serde::Serialize;

use crate::boot::state::AppState;
use crate::services::config::aliases::{Alias, AliasId, Listening, Side};
use crate::services::history::store::{Range, TgStat, HOUR_MS};
use crate::trunking::follow::routing::Routing;
use crate::trunking::site::LiveState;
use crate::trunking::trunk::CallView;
use crate::util::time::unix_ms;

/// How far back a talkgroup heard is listed.
const HEARD_HOURS: u64 = 7 * 24;
/// The history is asked for the week at most this often; the calls listed now cover the rest.
const HEARD_EVERY: Duration = Duration::from_secs(300);
/// Talkgroups read from the history, the busiest first.
const HEARD_MAX: usize = 1000;

/// One site's talkgroups of the last week, as the history last gave them.
#[derive(Clone)]
struct Week {
    site: String,
    read: Instant,
    /// When it was read, on the board's clock: calls granted after it are not in it.
    read_ms: u64,
    rows: Arc<Vec<TgStat>>,
}

/// The live site's week, kept between reads of the history.
#[derive(Default)]
pub struct HeardWeek(Mutex<Option<Week>>);

impl HeardWeek {
    async fn of(&self, s: &AppState, site: &str) -> Week {
        let kept = self.0.lock().ok().and_then(|w| w.clone());
        if let Some(w) = kept.filter(|w| w.site == site && w.read.elapsed() < HEARD_EVERY) {
            return w;
        }
        let read_ms = unix_ms();
        let q = Range::site(site, read_ms.saturating_sub(HEARD_HOURS * HOUR_MS), read_ms);
        let week = match s.history.query(move |st| st.talkgroups(&q, HEARD_MAX)).await {
            Ok(rows) => Week { site: site.to_string(), read: Instant::now(), read_ms, rows: Arc::new(rows) },
            // Not kept: the next view asks again.
            Err(e) => {
                tracing::warn!("the week's talkgroups of {site} not read: {e:#}");
                return Week { site: site.to_string(), read: Instant::now(), read_ms: 0, rows: Arc::default() };
            }
        };
        if let Ok(mut kept) = self.0.lock() {
            *kept = Some(week.clone());
        }
        week
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Talkgroup {
    pub tg: u32,
    /// Its alias's name; none while that is only its number.
    pub name: Option<String>,
    pub group: Option<String>,
    pub color: Option<String>,
    /// An alias names it alone: its controls are its own.
    pub own: bool,
    /// The follower follows its calls (encrypted ones never).
    pub followed: bool,
    /// Its traffic lane: `left` lane 1 only, `right` lane 2 only, `both` either.
    pub speaker: Side,
    pub priority: Option<u8>,
    /// Its alias says record its calls.
    pub record: bool,
    /// Its calls are recorded: it is followed, recording is on, and it is marked or every call is.
    pub recorded: bool,
    /// In the last week: its calls, how many of them encrypted, and when it was last heard.
    pub calls: u64,
    pub encrypted: u64,
    pub last_ms: Option<u64>,
}

/// What decides whether a followed call is recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RecordingPolicy {
    pub enabled: bool,
    /// Every followed call; off: those of the talkgroups marked `record`.
    pub every_call: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Talkgroups {
    /// The live site and its system; none while no site is live.
    pub site: Option<String>,
    pub system: Option<String>,
    pub listening: Option<Listening>,
    pub recording: RecordingPolicy,
    pub items: Vec<Talkgroup>,
}

/// What was heard of one talkgroup: its calls, the encrypted ones, the last one's time.
#[derive(Default, Clone, Copy)]
struct Heard {
    calls: u64,
    encrypted: u64,
    last_ms: Option<u64>,
}

/// The talkgroups of a system at one site: the week's, those of the calls listed now (`calls`,
/// counted when granted after the week was read at `read_ms`), and those an alias gives a
/// priority, a lane or a Record mark. An alias that only names a talkgroup, or switches it off,
/// lists it once it is heard.
fn list(aliases: &[Alias], listening: Listening, recording: RecordingPolicy, week: &[TgStat], read_ms: u64, calls: &[&CallView]) -> Vec<Talkgroup> {
    let mut heard: BTreeMap<u32, Heard> = BTreeMap::new();
    for a in aliases.iter().filter(|a| a.priority.is_some() || a.record || a.speaker != Side::Both) {
        for id in &a.ids {
            if let AliasId::Talkgroup { value } = id {
                heard.entry(*value).or_default();
            }
        }
    }
    for t in week {
        let h = heard.entry(t.tg).or_default();
        h.calls += t.calls;
        h.encrypted += t.encrypted;
        h.last_ms = h.last_ms.max(Some(t.last_ms));
    }
    for c in calls.iter().filter(|c| !c.private) {
        let h = heard.entry(c.tg).or_default();
        if c.started_unix_ms > read_ms {
            h.calls += 1;
            h.encrypted += u64::from(c.encrypted);
        }
        h.last_ms = h.last_ms.max(Some(c.started_unix_ms));
    }
    let routing = Routing::new(aliases, listening);
    let index = crate::services::config::aliases::AliasIndex::new(aliases);
    heard
        .into_iter()
        .map(|(tg, h)| {
            let alias = index.talkgroup(tg);
            let followed = routing.route(tg).is_some();
            let record = routing.record(tg);
            Talkgroup {
                tg,
                name: alias.map(|a| a.name.clone()).filter(|n| *n != format!("TG {tg}")),
                group: alias.and_then(|a| a.group.clone()),
                color: alias.and_then(|a| a.color.clone()),
                own: aliases.iter().any(|a| a.ids == [AliasId::Talkgroup { value: tg }]),
                followed,
                speaker: alias.map_or(listening.unmonitored_speaker, |a| a.speaker),
                priority: alias.and_then(|a| a.priority),
                record,
                recorded: followed && recording.enabled && (recording.every_call || record),
                calls: h.calls,
                encrypted: h.encrypted,
                last_ms: h.last_ms,
            }
        })
        .collect()
}

/// The live site's talkgroups now.
pub async fn view(s: &AppState) -> Talkgroups {
    let live = match s.live.state() {
        LiveState::Live(l) => Some(l),
        _ => None,
    };
    let week = match &live {
        Some(l) => Some(s.heard.of(s, &l.site.id).await),
        None => None,
    };
    let calls = s.trunking.calls();
    let c = s.config.lock().await;
    let r = &c.radio.value.recording;
    let recording = RecordingPolicy { enabled: r.enabled, every_call: r.every_call };
    let (Some(live), Some(week)) = (live, week) else {
        return Talkgroups { recording, ..Default::default() };
    };
    let Some(sys) = c.systems.value.system(&live.system.id) else {
        return Talkgroups { recording, ..Default::default() };
    };
    let here: Vec<&CallView> = calls.open.iter().chain(&calls.recent).filter(|x| x.site == live.site.id).collect();
    Talkgroups {
        site: Some(live.site.id.clone()),
        system: Some(sys.id.clone()),
        listening: Some(sys.listening),
        recording,
        items: list(&sys.aliases, sys.listening, recording, &week.rows, week.read_ms, &here),
    }
}

pub async fn get(State(s): State<Arc<AppState>>) -> Json<Talkgroups> {
    Json(view(&s).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(tg: u32, calls: u64, encrypted: u64, last_ms: u64) -> TgStat {
        TgStat { tg, calls, encrypted, voice_s: 0.0, grant_s: 0.0, radios: 1, last_ms }
    }

    fn call(tg: u32, started_unix_ms: u64, encrypted: bool) -> CallView {
        CallView {
            call: 1,
            site: "clay".into(),
            tg,
            source: None,
            speaker: None,
            freq_hz: None,
            slot: None,
            channel: None,
            encrypted,
            emergency: false,
            private: false,
            not_followed: None,
            lane: None,
            started_unix_ms,
            first_voice_unix_ms: None,
            ended_unix_ms: None,
            open_ms: None,
            grant_ms: None,
            close: None,
            end_lc: None,
            sources: Vec::new(),
            voice_frames: 0,
            codec: None,
            frame_errors: None,
        }
    }

    const EVERY: RecordingPolicy = RecordingPolicy { enabled: true, every_call: true };
    const MARKED: RecordingPolicy = RecordingPolicy { enabled: true, every_call: false };

    #[test]
    fn the_list_is_the_week_the_calls_listed_now_and_the_talkgroups_with_a_setting() {
        let mut named = Alias::talkgroup(300, "Dispatch");
        named.record = true;
        named.speaker = Side::Left;
        // Switched off: listed when heard (850), not when silent (851).
        let mut off = Alias::talkgroup(850, "TG 850");
        off.do_not_monitor = true;
        let mut silent = Alias::talkgroup(851, "TG 851");
        silent.do_not_monitor = true;
        let mut prior = Alias::talkgroup(700, "Fire");
        prior.priority = Some(1);
        let aliases = [named, off, silent, Alias::talkgroup(900, "Named, never heard"), prior];
        let week = [stat(300, 40, 0, 5_000), stat(402, 9, 9, 4_000), stat(850, 0, 0, 3_000)];
        let (old, new, private) = (call(300, 4_500, false), call(318, 7_000, false), CallView { private: true, ..call(3_409_922, 8_000, false) });
        let items = list(&aliases, Listening::default(), MARKED, &week, 6_000, &[&old, &new, &private]);
        assert_eq!(
            items.iter().map(|t| t.tg).collect::<Vec<_>>(),
            vec![300, 318, 402, 700, 850],
            "by number: the heard ones and those with a setting to listen by; not one only named or only switched off, nor a private call's radio"
        );
        let t = |tg: u32| items.iter().find(|t| t.tg == tg).unwrap();
        assert_eq!((t(700).calls, t(700).last_ms, t(700).priority), (0, None, Some(1)), "never heard, listed for its priority");
        // The week holds the call granted before it was read; the one granted after is added.
        assert_eq!((t(300).calls, t(300).last_ms), (40, Some(5_000)));
        assert_eq!((t(318).calls, t(318).last_ms, t(318).name.clone()), (1, Some(7_000), None));
        assert_eq!((t(300).name.as_deref(), t(300).speaker, t(300).own), (Some("Dispatch"), Side::Left, true));
        assert_eq!((t(850).name.clone(), t(850).followed, t(850).calls), (None, false, 0), "named only by its number, and switched off");
        // Marked: the marked one's calls are recorded, the others' are not.
        assert!(t(300).recorded && !t(318).recorded && !t(850).recorded);
        assert_eq!((t(402).calls, t(402).encrypted), (9, 9));
    }

    #[test]
    fn what_is_recorded_follows_the_policy_and_the_follower() {
        let mut off = Alias::talkgroup(850, "Schools");
        off.do_not_monitor = true;
        off.record = true;
        let week = [stat(300, 1, 0, 1), stat(850, 1, 0, 1)];
        let recorded = |policy| list(&[off.clone()], Listening::default(), policy, &week, 10, &[]).into_iter().map(|t| (t.tg, t.recorded)).collect::<Vec<_>>();
        assert_eq!(recorded(EVERY), vec![(300, true), (850, false)], "a talkgroup not followed is never recorded");
        assert_eq!(recorded(MARKED), vec![(300, false), (850, false)]);
        assert_eq!(recorded(RecordingPolicy { enabled: false, every_call: true }), vec![(300, false), (850, false)]);
        // Where only talkgroups with a priority are followed, one with none is not.
        let strict = Listening { follow_unmonitored: false, ..Listening::default() };
        let items = list(&[], strict, EVERY, &week, 10, &[]);
        assert!(items.iter().all(|t| !t.followed && !t.recorded));
    }
}

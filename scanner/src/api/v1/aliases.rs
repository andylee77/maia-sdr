//! `/api/v1/systems/{id}/aliases`, `/listening` and `/talkgroups/{tg}`: a system's aliases (what
//! the radio follows, records and plays where), its listening settings, and one talkgroup's
//! controls as the live screen sets them. The live site follows each change at once.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config;
use crate::services::config::aliases::{Alias, AliasId, Listening, LOWEST_PRIORITY};
use crate::services::config::aliases::Side;
use crate::services::config::systems::System;

#[derive(Serialize)]
pub struct Aliases {
    pub aliases: Vec<Alias>,
    pub listening: Listening,
}

pub async fn get(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Aliases> {
    let c = s.config.lock().await;
    let sys = c.systems.value.system(&id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
    Ok(Json(Aliases { aliases: sys.aliases.clone(), listening: sys.listening }))
}

/// Replace a system's alias list, after checking every alias.
pub async fn put(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(aliases): Json<Vec<Alias>>) -> ApiResult<Aliases> {
    for a in &aliases {
        a.check().map_err(ApiError::bad_request)?;
    }
    let out = edit(&s, &id, |sys| {
        sys.aliases = aliases;
        Ok(())
    })
    .await?;
    Ok(Json(out))
}

pub async fn put_listening(State(s): State<Arc<AppState>>, Path(id): Path<String>, Json(listening): Json<Listening>) -> ApiResult<Aliases> {
    listening.check().map_err(ApiError::bad_request)?;
    let out = edit(&s, &id, |sys| {
        sys.listening = listening;
        Ok(())
    })
    .await?;
    Ok(Json(out))
}

/// One talkgroup's controls; a field left out stays as it is.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkgroupEdit {
    pub name: Option<String>,
    pub group: Option<String>,
    /// 1 (highest) to 100; null takes the priority away.
    #[serde(default, with = "double_option")]
    pub priority: Option<Option<u8>>,
    pub do_not_monitor: Option<bool>,
    /// Followed or not, whatever the system asks of a followed talkgroup: where only those with
    /// a priority are followed, following one with none gives it the lowest.
    pub follow: Option<bool>,
    pub record: Option<bool>,
    pub speaker: Option<Side>,
}

/// Set one talkgroup's controls: on the alias that names it alone, else on a new alias for it
/// (named after the talkgroup, or after the range alias that held it).
pub async fn put_talkgroup(State(s): State<Arc<AppState>>, Path((id, tg)): Path<(String, u32)>, Json(req): Json<TalkgroupEdit>) -> ApiResult<Alias> {
    let mut edited = None;
    edit(&s, &id, |sys| {
        edited = Some(set_talkgroup(sys, tg, &req)?);
        Ok(())
    })
    .await?;
    Ok(Json(edited.expect("edited")))
}

/// One talkgroup's controls set in `sys`; the alias as it then is, or why it cannot be.
fn set_talkgroup(sys: &mut System, tg: u32, req: &TalkgroupEdit) -> Result<Alias, String> {
    let own = sys.aliases.iter().position(|a| a.ids == [AliasId::Talkgroup { value: tg }]);
    let i = match own {
        Some(i) => i,
        None => {
            let base = sys.alias_index().talkgroup(tg).cloned();
            let mut a = Alias::talkgroup(tg, format!("TG {tg}"));
            if let Some(b) = base {
                a = Alias { ids: vec![AliasId::Talkgroup { value: tg }], ..b };
            }
            sys.aliases.push(a);
            sys.aliases.len() - 1
        }
    };
    let needs_priority = !sys.listening.follow_unmonitored;
    let a = &mut sys.aliases[i];
    if let Some(name) = &req.name {
        a.name = name.trim().to_string();
    }
    if let Some(group) = &req.group {
        a.group = Some(group.trim().to_string()).filter(|g| !g.is_empty());
    }
    if let Some(p) = req.priority {
        a.priority = p;
    }
    if let Some(d) = req.do_not_monitor {
        a.do_not_monitor = d;
    }
    if let Some(follow) = req.follow {
        a.do_not_monitor = !follow;
        if follow && needs_priority && a.priority.is_none() {
            a.priority = Some(LOWEST_PRIORITY);
        }
    }
    if let Some(r) = req.record {
        a.record = r;
    }
    if let Some(sp) = req.speaker {
        a.speaker = sp;
    }
    a.check()?;
    Ok(a.clone())
}

/// Remove the alias that names one talkgroup alone. The talkgroup is then treated as one with no
/// alias, or as the range alias that holds it.
pub async fn delete_talkgroup(State(s): State<Arc<AppState>>, Path((id, tg)): Path<(String, u32)>) -> ApiResult<Aliases> {
    let out = edit(&s, &id, |sys| {
        let before = sys.aliases.len();
        sys.aliases.retain(|a| a.ids != [AliasId::Talkgroup { value: tg }]);
        if sys.aliases.len() == before {
            return Err(format!("no alias names talkgroup {tg} alone"));
        }
        Ok(())
    })
    .await?;
    Ok(Json(out))
}

/// `f` applied to `sys`, which is left as it was when `f` refuses. Returns the system as it was.
fn apply(sys: &mut System, f: impl FnOnce(&mut System) -> Result<(), String>) -> Result<System, String> {
    let mut changed = sys.clone();
    f(&mut changed)?;
    Ok(std::mem::replace(sys, changed))
}

/// Change a system under the config lock, save it, and have the live site follow. A change that
/// is refused or cannot be saved leaves the system as it was.
async fn edit(s: &AppState, id: &str, f: impl FnOnce(&mut System) -> Result<(), String>) -> Result<Aliases, ApiError> {
    let out = {
        let mut c = s.config.lock().await;
        let i = c.systems.value.systems.iter().position(|x| x.id == id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
        let before = apply(&mut c.systems.value.systems[i], f).map_err(ApiError::bad_request)?;
        if let Err(e) = config::save(&s.paths.systems(), &c.systems) {
            c.systems.value.systems[i] = before;
            return Err(e.into());
        }
        let sys = &c.systems.value.systems[i];
        Aliases { aliases: sys.aliases.clone(), listening: sys.listening }
    };
    s.live.aliases_changed().await;
    Ok(out)
}

/// `null` and a missing field apart: a missing field is `None`, `null` is `Some(None)`.
mod double_option {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(d).map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::config::systems::Protocol;

    fn system() -> System {
        System {
            id: "clay".into(),
            label: "Clay County".into(),
            protocol: Protocol::P25,
            identity: Default::default(),
            details: Default::default(),
            aliases: Vec::new(),
            listening: Default::default(),
            sites: Vec::new(),
        }
    }

    fn set(sys: &mut System, tg: u32, json: &str) -> Result<System, String> {
        let req: TalkgroupEdit = serde_json::from_str(json).unwrap();
        apply(sys, |s| set_talkgroup(s, tg, &req).map(|_| ()))
    }

    #[test]
    fn a_refused_talkgroup_edit_leaves_the_system_as_it_was() {
        let mut sys = system();
        // A talkgroup with no alias gets one of its own, named after it.
        set(&mut sys, 300, r#"{"priority": 5}"#).unwrap();
        assert_eq!(sys.aliases.len(), 1);
        assert_eq!((sys.aliases[0].name.as_str(), sys.aliases[0].priority), ("TG 300", Some(5)));
        // Followed or not is `follow`; where only talkgroups with a priority are followed, it
        // gives one with none the lowest, and leaves a priority there is.
        set(&mut sys, 300, r#"{"follow": false}"#).unwrap();
        assert!(sys.aliases[0].do_not_monitor);
        sys.listening.follow_unmonitored = false;
        set(&mut sys, 300, r#"{"follow": true}"#).unwrap();
        set(&mut sys, 301, r#"{"follow": true}"#).unwrap();
        assert_eq!(sys.aliases.iter().map(|a| (a.do_not_monitor, a.priority)).collect::<Vec<_>>(), vec![(false, Some(5)), (false, Some(100))]);
        sys.aliases.truncate(1);
        sys.listening.follow_unmonitored = true;
        let before = sys.clone();
        for bad in [r#"{"priority": 0}"#, r#"{"speaker": "off"}"#, r#"{"name": " "}"#] {
            assert!(set(&mut sys, 300, bad).is_err(), "{bad} on an alias there is");
            assert!(set(&mut sys, 301, bad).is_err(), "{bad} on a talkgroup with none");
            assert_eq!(sys, before, "{bad}");
        }
    }
}

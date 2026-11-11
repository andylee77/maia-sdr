//! `/api/v1/systems/{id}/aliases`, `/listening` and `/talkgroups/{tg}`: a system's aliases (what
//! the radio follows, records and plays where), its listening settings, and one talkgroup's
//! controls as the live screen sets them. The live site follows each change at once.
//!
//! The list has a revision (`rev`), so a page that replaces it whole does not write over a change
//! made since it read the list.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::api::{ApiError, ApiResult};
use crate::boot::state::AppState;
use crate::services::config;
use crate::services::config::aliases::{check_list, Alias, AliasId, Listening, Side, LOWEST_PRIORITY};
use crate::services::config::systems::System;

#[derive(Serialize)]
pub struct Aliases {
    pub aliases: Vec<Alias>,
    pub listening: Listening,
    /// The list's revision: it changes with the list.
    pub rev: String,
}

impl Aliases {
    fn of(sys: &System) -> Self {
        Aliases { aliases: sys.aliases.clone(), listening: sys.listening, rev: rev_of(&sys.aliases) }
    }
}

/// A list's revision: a hash of it as it is stored (as text: a page's numbers do not hold 64 bits).
fn rev_of(aliases: &[Alias]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    serde_json::to_string(aliases).unwrap_or_default().hash(&mut h);
    format!("{:016x}", h.finish())
}

pub async fn get(State(s): State<Arc<AppState>>, Path(id): Path<String>) -> ApiResult<Aliases> {
    let c = s.config.lock().await;
    let sys = c.systems.value.system(&id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
    Ok(Json(Aliases::of(sys)))
}

#[derive(Deserialize)]
pub struct Guard {
    /// The revision the list was read at; with it, a list changed since is not replaced.
    pub rev: Option<String>,
}

/// Replace a system's alias list, after checking every alias. With `rev`, only the list that was
/// read: 409 when it changed since.
pub async fn put(State(s): State<Arc<AppState>>, Path(id): Path<String>, Query(guard): Query<Guard>, Json(aliases): Json<Vec<Alias>>) -> ApiResult<Aliases> {
    for a in &aliases {
        a.check().map_err(ApiError::bad_request)?;
    }
    check_list(&aliases).map_err(ApiError::bad_request)?;
    let out = edit(&s, &id, |sys| {
        if guard.rev.is_some_and(|rev| rev != rev_of(&sys.aliases)) {
            return Err(ApiError::conflict("the aliases changed since they were read: read them again"));
        }
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
    /// `#rrggbb`; null takes the colour away.
    #[serde(default, with = "double_option")]
    pub color: Option<Option<String>>,
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
/// (named after the talkgroup, or after the alias that held it: a range, or one naming it among
/// others, which gives it up).
pub async fn put_talkgroup(State(s): State<Arc<AppState>>, Path((id, tg)): Path<(String, u32)>, Json(req): Json<TalkgroupEdit>) -> ApiResult<Alias> {
    let mut edited = None;
    edit(&s, &id, |sys| {
        edited = Some(set_talkgroup(sys, tg, &req).map_err(ApiError::bad_request)?);
        Ok(())
    })
    .await?;
    Ok(Json(edited.expect("edited")))
}

/// One talkgroup's controls set in `sys`; the alias as it then is, or why it cannot be.
fn set_talkgroup(sys: &mut System, tg: u32, req: &TalkgroupEdit) -> Result<Alias, String> {
    let exact = AliasId::Talkgroup { value: tg };
    let own = sys.aliases.iter().position(|a| a.ids == [exact]);
    let i = match own {
        Some(i) => i,
        None => {
            let a = match sys.alias_index().talkgroup(tg).cloned() {
                Some(held) => Alias { ids: vec![exact], ..held },
                None => Alias::talkgroup(tg, format!("TG {tg}")),
            };
            // An alias naming it among others gives it up, and its own alias takes that place:
            // the first alias to name a talkgroup is the one read for it.
            match sys.aliases.iter().position(|x| x.ids.contains(&exact)) {
                Some(j) => {
                    sys.aliases[j].ids.retain(|id| *id != exact);
                    if sys.aliases[j].ids.is_empty() {
                        sys.aliases.remove(j);
                    }
                    sys.aliases.insert(j, a);
                    j
                }
                None => {
                    sys.aliases.push(a);
                    sys.aliases.len() - 1
                }
            }
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
    if let Some(color) = &req.color {
        a.color = color.as_ref().map(|c| c.trim().to_string());
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
            return Err(ApiError::bad_request(format!("no alias names talkgroup {tg} alone")));
        }
        Ok(())
    })
    .await?;
    Ok(Json(out))
}

/// `f` applied to `sys`, which is left as it was when `f` refuses. Returns the system as it was.
fn apply<E>(sys: &mut System, f: impl FnOnce(&mut System) -> Result<(), E>) -> Result<System, E> {
    let mut changed = sys.clone();
    f(&mut changed)?;
    Ok(std::mem::replace(sys, changed))
}

/// Change a system under the config lock, save it, and have the live site follow. A change that
/// is refused or cannot be saved leaves the system as it was.
async fn edit(s: &AppState, id: &str, f: impl FnOnce(&mut System) -> Result<(), ApiError>) -> Result<Aliases, ApiError> {
    let out = {
        let mut c = s.config.lock().await;
        let i = c.systems.value.systems.iter().position(|x| x.id == id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
        let before = apply(&mut c.systems.value.systems[i], f)?;
        if let Err(e) = config::save(&s.paths.systems(), &c.systems) {
            c.systems.value.systems[i] = before;
            return Err(e.into());
        }
        Aliases::of(&c.systems.value.systems[i])
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
    fn a_talkgroup_named_among_others_gets_an_alias_of_its_own_that_is_the_one_read() {
        let mut sys = system();
        let mut fire = Alias::talkgroup(300, "Fire");
        fire.ids.push(AliasId::Talkgroup { value: 301 });
        fire.priority = Some(1);
        sys.aliases = vec![Alias::talkgroup(200, "Roads"), fire];
        set(&mut sys, 300, r##"{"record": true, "color": "#ff8800"}"##).unwrap();
        let names: Vec<(&str, usize)> = sys.aliases.iter().map(|a| (a.name.as_str(), a.ids.len())).collect();
        assert_eq!(names, vec![("Roads", 1), ("Fire", 1), ("Fire", 1)], "its own alias in the place of the one that held it");
        let index = sys.alias_index();
        let read = index.talkgroup(300).unwrap();
        assert_eq!((read.record, read.priority, read.color.as_deref()), (true, Some(1), Some("#ff8800")));
        assert!(!index.talkgroup(301).unwrap().record, "the other talkgroup keeps the alias as it was");
        // A colour is #rrggbb, or none.
        assert!(set(&mut sys, 300, r#"{"color": "orange"}"#).is_err());
        set(&mut sys, 300, r#"{"color": null}"#).unwrap();
        assert_eq!(sys.alias_index().talkgroup(300).unwrap().color, None);
    }

    #[test]
    fn the_revision_moves_with_the_list() {
        let mut sys = system();
        let empty = rev_of(&sys.aliases);
        set(&mut sys, 300, r#"{"name": "Dispatch"}"#).unwrap();
        let named = rev_of(&sys.aliases);
        assert_ne!(empty, named);
        assert_eq!(named, rev_of(&sys.aliases.clone()));
        assert_eq!(named.len(), 16);
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

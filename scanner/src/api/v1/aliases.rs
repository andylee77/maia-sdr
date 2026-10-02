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
use crate::services::config::aliases::{Alias, AliasId, Listening};
use crate::services::config::aliases::Side;

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
    pub record: Option<bool>,
    pub speaker: Option<Side>,
}

/// Set one talkgroup's controls: on the alias that names it alone, else on a new alias for it
/// (named after the talkgroup, or after the range alias that held it).
pub async fn put_talkgroup(State(s): State<Arc<AppState>>, Path((id, tg)): Path<(String, u32)>, Json(req): Json<TalkgroupEdit>) -> ApiResult<Alias> {
    let mut edited = None;
    edit(&s, &id, |sys| {
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
        if let Some(r) = req.record {
            a.record = r;
        }
        if let Some(sp) = req.speaker {
            a.speaker = sp;
        }
        a.check()?;
        edited = Some(a.clone());
        Ok(())
    })
    .await?;
    Ok(Json(edited.expect("edited")))
}

/// Change a system under the config lock, save it, and have the live site follow.
async fn edit(s: &AppState, id: &str, f: impl FnOnce(&mut config::systems::System) -> Result<(), String>) -> Result<Aliases, ApiError> {
    let out = {
        let mut c = s.config.lock().await;
        let sys = c.systems.value.systems.iter_mut().find(|x| x.id == id).ok_or_else(|| ApiError::not_found(format!("system {id}")))?;
        f(sys).map_err(ApiError::bad_request)?;
        let out = Aliases { aliases: sys.aliases.clone(), listening: sys.listening };
        config::save(&s.paths.systems(), &c.systems)?;
        out
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

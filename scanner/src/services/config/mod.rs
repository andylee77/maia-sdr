//! The configuration: three layers the user owns and the state the radio learns.
//!
//! - `radio.json`: the hardware and services (`radio`).
//! - `systems.json`: systems, their aliases (what to follow, record and where it plays) and their
//!   sites (`systems`).
//! - `state/`: the live site, the crystal calibration and per-site learned data (`state`).
//!
//! Files are versioned JSON under `<flash>/scanner/`, written atomically and only when something
//! changes. A missing file is the default, so a new unit starts with no systems. A file written
//! by a newer binary is read but never written, so a downgrade cannot drop its fields.

pub mod aliases;
pub mod ids;
pub mod radio;
pub mod state;
pub mod systems;

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::util::{atomic_file, time};

pub use radio::RadioConfig;
pub use state::{RadioState, SiteState};
pub use systems::SystemsConfig;

/// Version of the files this binary writes.
pub const VERSION: u32 = 1;

/// Where the configuration, the history and the recordings live.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    pub sd: PathBuf,
}

impl Paths {
    pub fn new(flash: &Path, sd: &Path) -> Self {
        Paths { root: flash.join("scanner"), sd: sd.to_path_buf() }
    }

    /// The recordings on the SD card.
    pub fn recordings(&self) -> PathBuf {
        self.sd.join("p25_recordings")
    }

    pub fn radio(&self) -> PathBuf {
        self.root.join("radio.json")
    }

    pub fn systems(&self) -> PathBuf {
        self.root.join("systems.json")
    }

    pub fn radio_state(&self) -> PathBuf {
        self.root.join("state").join("radio.json")
    }

    pub fn site_state(&self, site: &str) -> PathBuf {
        self.root.join("state").join("sites").join(format!("{site}.json"))
    }
}

/// A loaded file and whether this binary may write it back.
#[derive(Debug, Clone)]
pub struct Stored<T> {
    pub value: T,
    pub writable: bool,
}

/// Load a versioned file: missing gives the default; a file that does not parse is moved aside
/// (`<name>.corrupt-<unix ms>`) and gives the default; a newer version is read-only.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> anyhow::Result<Stored<T>> {
    let body = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Stored { value: T::default(), writable: true });
        }
        Err(e) => return Err(anyhow::anyhow!("{}: {e}", path.display())),
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&body)
        .and_then(|v| {
            let version = v.get("version").and_then(|x| x.as_u64()).unwrap_or(0);
            serde_json::from_value::<T>(v).map(|t| (t, version))
        });
    match parsed {
        Ok((value, version)) => {
            let writable = version <= u64::from(VERSION);
            if !writable {
                tracing::warn!("{} is version {version}, newer than {VERSION}: read only", path.display());
            }
            Ok(Stored { value, writable })
        }
        Err(e) => {
            let aside = path.with_extension(format!("json.corrupt-{}", time::unix_ms()));
            tracing::error!("{}: {e}; moved to {} and using defaults", path.display(), aside.display());
            std::fs::rename(path, &aside)?;
            Ok(Stored { value: T::default(), writable: true })
        }
    }
}

/// Write a loaded file back unless a newer binary owns its format.
pub fn save<T: Serialize>(path: &Path, stored: &Stored<T>) -> anyhow::Result<()> {
    if !stored.writable {
        anyhow::bail!("{} belongs to a newer version; not written", path.display());
    }
    atomic_file::write_json(path, &stored.value)?;
    Ok(())
}

/// What `GET /api/v1/config` exports and `PUT` imports.
pub const FORMAT: &str = "scanner-config";

/// The user's configuration as one document: the radio settings, the systems with their aliases
/// and sites, and the live site. What the radio learned on the air and the crystal calibration
/// (the board's own) are not part of it.
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDoc {
    pub format: String,
    pub version: u32,
    #[serde(default)]
    pub build: String,
    #[serde(default)]
    pub exported_unix_ms: u64,
    pub radio: RadioConfig,
    pub systems: SystemsConfig,
    #[serde(default)]
    pub live_site: Option<String>,
}

/// What a removal took out of the configuration.
#[derive(Debug, Default, PartialEq, Serialize)]
pub struct Removed {
    pub systems: Vec<String>,
    pub sites: Vec<String>,
}

/// The whole configuration as loaded at boot.
#[derive(Debug, Clone)]
pub struct Config {
    pub radio: Stored<RadioConfig>,
    pub systems: Stored<SystemsConfig>,
    pub state: Stored<RadioState>,
}

impl Config {
    pub fn load(paths: &Paths) -> anyhow::Result<Config> {
        let mut config = Config { radio: load(&paths.radio())?, systems: load(&paths.systems())?, state: load(&paths.radio_state())? };
        for problem in config.repair() {
            tracing::warn!("configuration: {problem}");
        }
        Ok(config)
    }

    /// Drop references to things that do not exist (in memory only) and report them.
    pub fn repair(&mut self) -> Vec<String> {
        let mut problems = Vec::new();
        let mut site_ids = std::collections::HashSet::new();
        for sys in &self.systems.value.systems {
            if !ids::is_valid(&sys.id) {
                problems.push(format!("system id {:?} is not a valid id", sys.id));
            }
            for site in &sys.sites {
                if !ids::is_valid(&site.id) || !site_ids.insert(site.id.clone()) {
                    problems.push(format!("site id {:?} is invalid or used twice", site.id));
                }
            }
        }
        for sys in &self.systems.value.systems {
            for a in &sys.aliases {
                if let Err(e) = a.check() {
                    problems.push(format!("system {}: {e}", sys.id));
                }
            }
            if let Err(e) = sys.listening.check() {
                problems.push(format!("system {}: {e}", sys.id));
            }
        }
        let systems = &self.systems.value;
        if let Some(live) = &self.state.value.live_site {
            if systems.site(live).is_none() {
                problems.push(format!("live site {live:?} is not configured; starting with no site"));
                self.state.value.live_site = None;
            }
        }
        problems
    }

    pub fn site_state(paths: &Paths, site: &str) -> anyhow::Result<Stored<SiteState>> {
        load(&paths.site_state(site))
    }

    pub fn export(&self, build: &str) -> ConfigDoc {
        ConfigDoc {
            format: FORMAT.to_string(),
            version: VERSION,
            build: build.to_string(),
            exported_unix_ms: time::unix_ms(),
            radio: self.radio.value.clone(),
            systems: self.systems.value.clone(),
            live_site: self.state.value.live_site.clone(),
        }
    }

    /// Take `doc` in place of the radio settings and systems, after checking it whole.
    /// The live site is the document's, else the current one if the document has it. Returns the
    /// sites that are gone.
    pub fn import(&mut self, doc: ConfigDoc) -> Result<Vec<String>, String> {
        if doc.format != FORMAT {
            return Err(format!("not a {FORMAT} document"));
        }
        if doc.version > VERSION {
            return Err(format!("version {} is newer than this scanner's {VERSION}", doc.version));
        }
        doc.radio.check().map_err(|e| format!("radio: {e}"))?;
        let systems = doc.systems;
        let live = doc.live_site.or_else(|| self.state.value.live_site.clone()).filter(|s| systems.site(s).is_some());
        let mut next = self.clone();
        next.radio.value = RadioConfig { version: VERSION, ..doc.radio };
        next.systems.value = SystemsConfig { version: VERSION, ..systems };
        next.state.value.live_site = live;
        let problems = next.repair();
        if !problems.is_empty() {
            return Err(problems.join("; "));
        }
        let gone = self.site_ids().into_iter().filter(|s| next.systems.value.site(s).is_none()).collect();
        *self = next;
        Ok(gone)
    }

    /// Back to a new unit's configuration: no systems or sites, the default radio settings. The
    /// crystal calibration stays (it is the board's). Returns the sites that are gone.
    pub fn factory(&mut self) -> Vec<String> {
        let gone = self.site_ids();
        self.radio.value = RadioConfig::default();
        self.systems.value = SystemsConfig::default();
        self.state.value.live_site = None;
        gone
    }

    fn site_ids(&self) -> Vec<String> {
        self.systems.value.systems.iter().flat_map(|s| s.sites.iter().map(|x| x.id.clone())).collect()
    }

    /// Write every file (after an import or a factory reset).
    pub fn save_all(&mut self, paths: &Paths) -> anyhow::Result<()> {
        for writable in [&mut self.radio.writable, &mut self.systems.writable, &mut self.state.writable] {
            *writable = true;
        }
        save(&paths.radio(), &self.radio)?;
        save(&paths.systems(), &self.systems)?;
        save(&paths.radio_state(), &self.state)?;
        Ok(())
    }

    /// Take a site out of its system. None if the system has no such site. Keeping the live site
    /// is the caller's.
    pub fn remove_site(&mut self, system: &str, site: &str) -> Option<Removed> {
        let sys = self.systems.value.systems.iter_mut().find(|s| s.id == system)?;
        let before = sys.sites.len();
        sys.sites.retain(|s| s.id != site);
        if sys.sites.len() == before {
            return None;
        }
        Some(Removed { sites: vec![site.to_string()], ..Default::default() })
    }

    /// Take a system out with its sites and aliases.
    pub fn remove_system(&mut self, id: &str) -> Option<Removed> {
        let i = self.systems.value.systems.iter().position(|s| s.id == id)?;
        let sys = self.systems.value.systems.remove(i);
        let sites: Vec<String> = sys.sites.into_iter().map(|s| s.id).collect();
        Some(Removed { systems: vec![id.to_string()], sites })
    }
}

/// Delete what removed sites learned (their state files).
pub fn forget_sites(paths: &Paths, sites: &[String]) {
    for site in sites {
        match std::fs::remove_file(paths.site_state(site)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("site {site}: learned state not deleted: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> Config {
        let site = |id: &str| json!({ "id": id, "label": id, "control": { "freq_hz": 851_000_000u64 } });
        let systems: SystemsConfig = serde_json::from_value(json!({
            "version": 1,
            "systems": [
                { "id": "a", "label": "A", "protocol": "p25", "sites": [site("a1"), site("a2")] },
                { "id": "b", "label": "B", "protocol": "dmr_tier3", "sites": [site("b1")] },
            ],
        }))
        .unwrap();
        fn stored<T>(value: T) -> Stored<T> {
            Stored { value, writable: true }
        }
        Config { radio: stored(RadioConfig::default()), systems: stored(systems), state: stored(RadioState::default()) }
    }

    #[test]
    fn a_site_is_removed_from_its_system() {
        let mut c = config();
        assert_eq!(c.remove_site("a", "a2"), Some(Removed { sites: vec!["a2".into()], ..Default::default() }));
        assert!(c.systems.value.site("a2").is_none());
        assert!(c.systems.value.site("a1").is_some());
        assert_eq!(c.remove_site("a", "a2"), None);
        assert_eq!(c.remove_site("b", "a1"), None, "a site of another system");
    }

    #[test]
    fn an_export_imports_back_and_a_broken_one_is_refused() {
        let mut c = config();
        c.state.value.live_site = Some("b1".into());
        let doc = c.export("test");
        let mut d = config();
        d.factory();
        assert!(d.systems.value.systems.is_empty());
        assert_eq!(d.import(doc.clone()), Ok(Vec::new()));
        assert_eq!(d.systems.value, c.systems.value);
        assert_eq!(d.state.value.live_site.as_deref(), Some("b1"));

        let mut wrong = doc.clone();
        wrong.format = "p25-httpd".into();
        assert!(d.import(wrong).is_err());
        let mut bad_alias = doc.clone();
        bad_alias.systems.systems[0].aliases.push(aliases::Alias { priority: Some(0), ..aliases::Alias::talkgroup(300, "Fire") });
        assert!(d.import(bad_alias).unwrap_err().contains("priority"));
        let mut bad_radio = doc.clone();
        bad_radio.radio.presets_allowed.clear();
        assert!(d.import(bad_radio).unwrap_err().starts_with("radio:"));
        assert_eq!(d.systems.value, c.systems.value, "a refused import changes nothing");

        // A document without system b: its site is gone and the live site with it.
        let mut smaller = doc;
        smaller.systems.systems.retain(|s| s.id != "b");
        smaller.live_site = None;
        assert_eq!(d.import(smaller), Ok(vec!["b1".to_string()]));
        assert_eq!(d.state.value.live_site, None);
    }

    #[test]
    fn a_factory_reset_keeps_only_the_crystal() {
        let mut c = config();
        c.state.value.live_site = Some("a1".into());
        c.state.value.crystal = Some(state::Crystal {
            ppm: -0.7,
            measured_at_lo_hz: 858_100_000,
            lo_shift_hz: 598,
            control_freq_hz: None,
            method: "tracker".into(),
            at_unix_ms: 1,
        });
        c.radio.value.presets_allowed = vec!["16M".into()];
        assert_eq!(c.factory(), ["a1", "a2", "b1"]);
        assert!(c.systems.value.systems.is_empty());
        assert_eq!(c.radio.value, RadioConfig::default());
        assert_eq!(c.state.value.live_site, None);
        assert!(c.state.value.crystal.is_some());
    }

    #[test]
    fn a_removed_system_takes_its_sites() {
        let mut c = config();
        let r = c.remove_system("a").unwrap();
        assert_eq!(r, Removed { systems: vec!["a".into()], sites: vec!["a1".into(), "a2".into()] });
        assert_eq!(c.systems.value.systems.len(), 1);
        assert_eq!(c.remove_system("a"), None);
    }
}

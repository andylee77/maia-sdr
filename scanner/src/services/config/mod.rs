//! The configuration: three layers the user owns and the state the radio learns.
//!
//! - `radio.json`: the hardware and services (`radio`).
//! - `systems.json`: systems, their names and their sites (`systems`).
//! - `profiles.json`: what to follow and where it plays (`profiles`).
//! - `state/`: the live site, the crystal calibration and per-site learned data (`state`).
//!
//! Files are versioned JSON under `<flash>/scanner/`, written atomically and only when something
//! changes. A file written by a newer binary is read but never written, so a downgrade cannot
//! drop its fields. On the first start of a unit that ran p25-httpd, `migrate` builds the files
//! from the old ones and leaves those untouched.

pub mod ids;
pub mod migrate;
pub mod profiles;
pub mod radio;
pub mod state;
pub mod systems;

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::util::{atomic_file, time};

pub use profiles::ProfilesConfig;
pub use radio::RadioConfig;
pub use state::{RadioState, SiteState};
pub use systems::SystemsConfig;

/// Version of the files this binary writes.
pub const VERSION: u32 = 1;

/// Where the configuration and the legacy files live.
#[derive(Debug, Clone)]
pub struct Paths {
    pub root: PathBuf,
    /// The flash directory p25-httpd kept its files in.
    pub flash: PathBuf,
    pub sd: PathBuf,
}

impl Paths {
    pub fn new(flash: &Path, sd: &Path) -> Self {
        Paths { root: flash.join("scanner"), flash: flash.to_path_buf(), sd: sd.to_path_buf() }
    }

    /// The recordings on the SD card (p25-httpd's directory: its files are listed as they are).
    pub fn recordings(&self) -> PathBuf {
        self.sd.join("p25_recordings")
    }

    pub fn radio(&self) -> PathBuf {
        self.root.join("radio.json")
    }

    pub fn systems(&self) -> PathBuf {
        self.root.join("systems.json")
    }

    pub fn profiles(&self) -> PathBuf {
        self.root.join("profiles.json")
    }

    pub fn radio_state(&self) -> PathBuf {
        self.root.join("state").join("radio.json")
    }

    pub fn site_state(&self, site: &str) -> PathBuf {
        self.root.join("state").join("sites").join(format!("{site}.json"))
    }

    pub fn migration_log(&self) -> PathBuf {
        self.root.join("migration-076.log")
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

/// The whole configuration as loaded at boot.
#[derive(Debug, Clone)]
pub struct Config {
    pub radio: Stored<RadioConfig>,
    pub systems: Stored<SystemsConfig>,
    pub profiles: Stored<ProfilesConfig>,
    pub state: Stored<RadioState>,
}

impl Config {
    pub fn load(paths: &Paths) -> anyhow::Result<Config> {
        let mut config = Config {
            radio: load(&paths.radio())?,
            systems: load(&paths.systems())?,
            profiles: load(&paths.profiles())?,
            state: load(&paths.radio_state())?,
        };
        for problem in config.repair() {
            tracing::warn!("configuration: {problem}");
        }
        Ok(config)
    }

    /// Drop references to things that do not exist (in memory only) and report them.
    fn repair(&mut self) -> Vec<String> {
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
        let systems = &self.systems.value;
        self.profiles.value.profiles.retain(|p| {
            let known = systems.system(&p.system).is_some();
            if !known {
                problems.push(format!("profile {:?} belongs to an unknown system", p.id));
            }
            known
        });
        let profiles = &self.profiles.value.profiles;
        self.profiles.value.active.retain(|site, id| {
            let ok = systems.site(site).is_some() && profiles.iter().any(|p| &p.id == id);
            if !ok {
                problems.push(format!("active profile {id:?} of site {site:?} is unknown"));
            }
            ok
        });
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
}

/// The configuration at boot and, on a first start after p25-httpd, the migration report.
pub struct Loaded {
    pub config: Config,
    pub migration: Option<migrate::Report>,
}

pub fn load_or_migrate(paths: &Paths) -> anyhow::Result<Loaded> {
    let migration = if !paths.root.exists() && migrate::legacy_present(paths) {
        Some(migrate::run(paths)?)
    } else {
        None
    };
    Ok(Loaded { config: Config::load(paths)?, migration })
}

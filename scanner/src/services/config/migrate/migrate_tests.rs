//! The migration on copies of units A's and B's files (`tests/fixtures/unit_*`).

use std::path::{Path, PathBuf};

use super::*;
use crate::services::config::{load_or_migrate, Config};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let target = to.join(e.file_name());
        if e.path().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            std::fs::copy(e.path(), target).unwrap();
        }
    }
}

/// A unit's flash and SD card rebuilt from its fixture.
fn unit(name: &str) -> (tempfile::TempDir, Paths) {
    let dir = tempfile::tempdir().unwrap();
    let src = fixtures().join(name);
    let flash = dir.path().join("flash");
    let sd = dir.path().join("sd");
    copy_dir(&src.join("jffs2"), &flash);
    std::fs::create_dir_all(sd.join("p25_recordings")).unwrap();
    let conn = rusqlite::Connection::open(sd.join("p25-history.sqlite")).unwrap();
    conn.execute_batch("PRAGMA foreign_keys=OFF;").unwrap();
    conn.execute_batch(&std::fs::read_to_string(src.join("history.sql")).unwrap()).unwrap();
    drop(conn);
    let recs: Vec<serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(src.join("recordings.json")).unwrap()).unwrap();
    for r in recs {
        std::fs::write(sd.join("p25_recordings").join(r["name"].as_str().unwrap()), b"").unwrap();
    }
    let paths = Paths::new(&flash, &sd);
    (dir, paths)
}

/// Every file under `dir`, with its bytes.
fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().is_dir() {
            out.extend(snapshot(&e.path()));
        } else {
            out.insert(e.path(), std::fs::read(e.path()).unwrap());
        }
    }
    out
}

#[test]
fn unit_a_migrates_into_systems_and_leaves_its_files_alone() {
    let (_dir, paths) = unit("unit_a");
    let before = snapshot(&paths.flash);
    let loaded = load_or_migrate(&paths).unwrap();
    let report = loaded.migration.expect("unit A has p25-httpd files");
    let after: BTreeMap<_, _> = snapshot(&paths.flash).into_iter().filter(|(p, _)| !p.starts_with(&paths.root)).collect();
    assert_eq!(before, after, "the old files are untouched");
    assert!(!paths.root.with_file_name("scanner.migrating").exists());
    assert!(paths.migration_log().exists());

    let c = &loaded.config;
    let systems = &c.systems.value;
    assert_eq!((report.systems, report.sites), (9, 21), "{:#?}", report.lines);
    assert_eq!(c.state.value.live_site.as_deref(), Some("cec_gcs"));

    let (clay_sys, clay) = systems.site("clay").unwrap();
    assert_eq!((clay_sys.id.as_str(), clay_sys.label.as_str(), clay_sys.protocol), ("clay-county", "Clay County", Protocol::P25));
    assert_eq!((clay_sys.identity.wacn, clay_sys.identity.system), (Some(0xBEE00), Some(0x8A0)));
    assert_eq!(clay.control.freq_hz, 860_962_500);
    assert_eq!(clay.window, Window { auto: true, min_preset: Some("12M".into()), cc_position: CcPosition::Top });
    assert_eq!(clay.source.as_deref().map(|s| s.contains("SDRTrunk")), Some(true));

    let (cec_sys, cec) = systems.site("cec_gcs").unwrap();
    assert_eq!((cec_sys.label.as_str(), cec_sys.protocol), ("Clay Electric", Protocol::DmrTier3));
    assert_eq!((cec_sys.identity.model, cec_sys.identity.network), (Some(DmrModel::Small), Some(0)));
    assert_eq!((cec.label.as_str(), cec.identity.site, cec.identity.colour_code), ("Green Cove Springs", Some(2), Some(0)));
    assert_eq!((cec.control.lcn, cec.control.timeslot), (Some(5), Some(1)));
    assert_eq!(cec.channel_plan.as_ref().unwrap().lcn_hz[&6], 451_087_500);

    let (duval_sys, duval) = systems.site("duval").unwrap();
    assert_eq!((duval_sys.label.as_str(), duval.label.as_str()), ("Jacksonville City - First Coast Radio", "Duval"));

    // FPL's hand-made site and its finder sites share WACN 92463 system 00A; site 40 is another
    // network with the same system id.
    let (fpl, _) = systems.site("fpl_clay").unwrap();
    assert_eq!((fpl.label.as_str(), fpl.sites.len()), ("Florida Power and Light", 13));
    assert_eq!(systems.site("system_00a_site_109_109").unwrap().1.label, "Site 109-109");
    let (other, _) = systems.site("system_00a_site_40_40").unwrap();
    assert_eq!((other.id.as_str(), other.label.as_str()), ("system-00a", "System 00A"));
    assert_eq!(other.identity.wacn, Some(0x91F82));

    // Learned state and the crystal.
    let clay_state = Config::site_state(&paths, "clay").unwrap().value;
    assert_eq!(clay_state.grants[&857_987_500], 13_506);
    let finder_state = Config::site_state(&paths, "system_4d6_site_1_2").unwrap().value;
    assert_eq!(finder_state.iden_bands.len(), 5);
    let crystal = c.state.value.crystal.as_ref().unwrap();
    assert_eq!((crystal.measured_at_lo_hz, crystal.lo_shift_hz), (858_100_000, 598));

    // Every site has an active profile of its own system; identical defaults are shared.
    let profiles = &c.profiles.value;
    for sys in &systems.systems {
        for site in &sys.sites {
            assert_eq!(profiles.active_for(&site.id).map(|p| p.system.as_str()), Some(sys.id.as_str()), "{}", site.id);
        }
    }
    assert_eq!(profiles.profiles.iter().filter(|p| p.system == fpl.id).count(), 1);

    assert_eq!(c.radio.value.gain.mode, GainMode::SlowAttack);
    assert_eq!(c.radio.value.recording.storage, Storage::Sd);
}

#[test]
fn unit_b_writes_out_the_clay_seed_it_ran_on() {
    let (_dir, paths) = unit("unit_b");
    let loaded = load_or_migrate(&paths).unwrap();
    let report = loaded.migration.unwrap();
    let c = &loaded.config;
    assert_eq!(c.state.value.live_site.as_deref(), Some("clay"));
    assert_eq!((report.systems, report.sites), (1, 1));
    assert!(report.lines.iter().any(|l| l.contains("site clay: written out from the seed")));
    assert_eq!(c.radio.value.gain, radio::Gain { mode: GainMode::Manual, manual_db: Some(60) });
    let state = Config::site_state(&paths, "clay").unwrap().value;
    assert_eq!(state.grants[&857_987_500], 3_428);
}

#[test]
fn a_second_start_loads_without_migrating() {
    let (_dir, paths) = unit("unit_b");
    load_or_migrate(&paths).unwrap();
    let again = load_or_migrate(&paths).unwrap();
    assert!(again.migration.is_none());
    assert_eq!(again.config.state.value.live_site.as_deref(), Some("clay"));
}

#[test]
fn an_interrupted_migration_is_redone() {
    let (_dir, paths) = unit("unit_b");
    let partial = paths.root.with_file_name("scanner.migrating");
    std::fs::create_dir_all(&partial).unwrap();
    std::fs::write(partial.join("radio.json"), b"{").unwrap();
    let loaded = load_or_migrate(&paths).unwrap();
    assert!(loaded.migration.is_some());
    assert!(!partial.exists());
}

#[test]
fn a_file_from_before_profiles_becomes_the_live_sites_default() {
    let settings: legacy::Settings = serde_json::from_value(serde_json::json!({
        "tg_aliases": {"300": "Dispatch"},
        "unit_aliases": {"1014": "Console 14"},
        "monitor_tgs": [301, 300, 301],
        "ignore_tgs": [700, 402],
        "tg_groups": [{"name": "Primary", "tgs": [300]}],
        "speakers": {"left": ["Primary"], "right": [], "other": "off", "preempt": true},
    }))
    .unwrap();
    // p25-httpd ran on its "clay" seed; its plan file refers to it.
    let input = Legacy {
        settings: Some(settings),
        plans: [("clay".to_string(), legacy::Plan::default())].into(),
        ..Default::default()
    };
    let (built, report) = build(&input);
    assert_eq!(report.live_site.as_deref(), Some("clay"), "{:#?}", report.lines);
    let clay = built.systems.systems.iter().find(|s| s.id == "clay-county").unwrap();
    assert_eq!(clay.talkgroups[&300], "Dispatch");
    assert_eq!(clay.radios[&1014], "Console 14");
    let p = &built.profiles.profiles[0];
    assert_eq!((p.id.as_str(), p.name.as_str()), ("clay-county/default", "Default"));
    assert_eq!((p.monitor.clone(), p.ignore.clone()), (vec![301, 300], vec![402, 700]));
    assert_eq!(p.speakers.other, Side::Off);
    assert_eq!(built.profiles.active["clay"], "clay-county/default");
}

#[test]
fn names_of_two_sites_merge_and_the_live_site_wins_a_conflict() {
    let entry = |tg: &str| legacy::SiteEntry {
        tg_aliases: [(300, tg.to_string()), (301, "TAC 1".to_string())].into(),
        ..Default::default()
    };
    let settings = legacy::Settings {
        site: "fpl_clay".into(),
        sites: [("fpl_clay".to_string(), entry("Ops")), ("other".to_string(), entry("Operations"))].into(),
        ..Default::default()
    };
    let site = |name: &str, label: &str| legacy::Site {
        name: name.into(),
        label: label.into(),
        control_freq_hz: 936_250_000,
        wacn: Some(0x92463),
        system_id: Some(0xA),
        ..Default::default()
    };
    let input = Legacy {
        settings: Some(settings),
        site_files: [
            ("fpl_clay".to_string(), site("fpl_clay", "Florida Power and Light (Clay)")),
            ("other".to_string(), site("other", "System 00A site 15-15")),
        ]
        .into(),
        active: Some("fpl_clay".into()),
        ..Default::default()
    };
    let (built, report) = build(&input);
    let sys = &built.systems.systems[0];
    assert_eq!(sys.sites.len(), 2);
    assert_eq!((sys.talkgroups[&300].as_str(), sys.talkgroups[&301].as_str()), ("Ops", "TAC 1"));
    assert!(report.lines.iter().any(|l| l.contains("talkgroup 300: \"Ops\" (fpl_clay) kept over \"Operations\" (other)")));
}

#[test]
fn names_and_profiles_kept_per_site_carry_over() {
    let settings: legacy::Settings = serde_json::from_value(serde_json::json!({
        "site": "clay",
        "sites": {"clay": {
            "tg_aliases": {"300": "Dispatch"},
            "unit_aliases": {"3406028": "Medic 1"},
            "profiles": [{"name": "Everything"}, {"name": "Fire", "monitor_tgs": [301]}],
            "active_profile": "Fire"
        }}
    }))
    .unwrap();
    let (built, _) = build(&Legacy { settings: Some(settings), ..Default::default() });
    let clay = &built.systems.systems[0];
    assert_eq!((clay.talkgroups[&300].as_str(), clay.radios[&3_406_028].as_str()), ("Dispatch", "Medic 1"));
    assert_eq!(built.profiles.profiles.len(), 2);
    assert_eq!(built.profiles.active["clay"], "clay-county/fire");
    assert_eq!(built.profiles.profile("clay-county/fire").unwrap().monitor, vec![301]);
}

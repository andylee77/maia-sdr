//! Host tests for `services::ui_settings` (change 056). Attached via
//! `#[cfg(test)] #[path = "ui_settings_tests.rs"] mod tests;`.

use super::*;

fn tmp_file(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "p25_ui_settings_test_{}_{}",
        std::process::id(),
        tag
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("p25-ui-settings.json")
}

fn patch(json: &str) -> SettingsPatch {
    serde_json::from_str(json).unwrap()
}

#[test]
fn defaults_match_pre_056_behaviour() {
    let s = UiSettings::default();
    assert!(s.recording.enabled);
    assert_eq!(s.recording.max_count, DEFAULT_MAX_RECORDINGS);
    assert!(s.tg_aliases.is_empty() && s.unit_aliases.is_empty());
    assert!(s.monitor_tgs.is_empty());
}

#[test]
fn recording_patch_validates_range() {
    let base = UiSettings::default();
    let (s, ch) = apply_patch(&base, patch(r#"{"recording":{"enabled":false}}"#)).unwrap();
    assert!(!s.recording.enabled);
    assert_eq!(s.recording.max_count, DEFAULT_MAX_RECORDINGS);
    assert!(ch.recording && !ch.tg_aliases);

    assert!(apply_patch(&base, patch(r#"{"recording":{"max_count":0}}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"recording":{"max_count":501}}"#)).is_err());
    let (s, _) = apply_patch(&base, patch(r#"{"recording":{"max_count":120}}"#)).unwrap();
    assert_eq!(s.recording.max_count, 120);

    // No-op patch reports no change.
    let (_, ch) = apply_patch(&base, patch(r#"{"recording":{"enabled":true}}"#)).unwrap();
    assert!(!ch.any());
}

#[test]
fn unknown_fields_are_rejected_in_patches() {
    assert!(serde_json::from_str::<SettingsPatch>(r#"{"recordings":{}}"#).is_err());
    assert!(serde_json::from_str::<SettingsPatch>(r#"{"recording":{"on":true}}"#).is_err());
}

#[test]
fn aliases_are_trimmed_capped_and_blank_removes() {
    let base = UiSettings::default();
    let long = "x".repeat(100);
    let body = format!(
        r#"{{"tg_aliases":{{"300":"  EMS Dispatch ","402":"   ","1":"{long}"}},
             "unit_aliases":{{"1014":"Console 14"}}}}"#
    );
    let (s, ch) = apply_patch(&base, patch(&body)).unwrap();
    assert_eq!(s.tg_aliases.get(&300).map(String::as_str), Some("EMS Dispatch"));
    assert!(!s.tg_aliases.contains_key(&402), "blank alias removes the entry");
    assert_eq!(s.tg_aliases.get(&1).unwrap().chars().count(), MAX_ALIAS_CHARS);
    assert_eq!(s.unit_aliases.get(&1014).map(String::as_str), Some("Console 14"));
    assert!(ch.tg_aliases && ch.unit_aliases && !ch.recording);

    assert!(apply_patch(&base, patch(r#"{"tg_aliases":{"0":"zero"}}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"unit_aliases":{"16777216":"too big"}}"#)).is_err());
}

#[test]
fn monitor_list_dedups_in_priority_order() {
    let base = UiSettings::default();
    let (s, ch) = apply_patch(&base, patch(r#"{"monitor_tgs":[402,300,402,301]}"#)).unwrap();
    assert_eq!(s.monitor_tgs, vec![402, 300, 301]);
    assert!(ch.monitor_tgs);
    assert!(apply_patch(&base, patch(r#"{"monitor_tgs":[0]}"#)).is_err());
}

#[test]
fn parse_tolerates_partial_unknown_and_bad_values() {
    let s = parse_settings(br#"{"recording":{"max_count":99999},"future_field":1,
                              "tg_aliases":{"0":"x","300":" EMS "},
                              "monitor_tgs":[0,300,300]}"#)
        .unwrap();
    assert!(s.recording.enabled, "missing field defaults");
    assert_eq!(s.recording.max_count, MAX_RECORDINGS_LIMIT, "clamped");
    assert_eq!(s.tg_aliases.len(), 1);
    assert_eq!(s.tg_aliases[&300], "EMS");
    assert_eq!(s.monitor_tgs, vec![300]);
    assert!(parse_settings(b"not json").is_err());
}

#[test]
fn store_persists_and_reloads() {
    let path = tmp_file("roundtrip");
    let store = SettingsStore::load(Some(path.clone()));
    assert!(store.load_note().contains("not present"));
    assert!(store.recording.enabled());
    let rev0 = store.rev();

    let out = store
        .update(patch(r#"{"recording":{"enabled":false,"max_count":7},
                          "tg_aliases":{"300":"EMS"},"monitor_tgs":[300]}"#))
        .unwrap();
    assert!(out.persisted, "{:?}", out.save_error);
    assert!(out.changed.recording && out.changed.tg_aliases && out.changed.monitor_tgs);
    assert!(store.rev() > rev0);
    // Live policy follows immediately.
    assert!(!store.recording.enabled());
    assert_eq!(store.recording.max_count(), 7);
    // No stray temp file left behind.
    assert!(!path.with_extension("json.tmp").exists());

    let again = SettingsStore::load(Some(path.clone()));
    assert!(again.load_note().starts_with("loaded"));
    let s = again.snapshot();
    assert!(!s.recording.enabled);
    assert_eq!(s.recording.max_count, 7);
    assert_eq!(s.tg_aliases[&300], "EMS");
    assert_eq!(s.monitor_tgs, vec![300]);
    assert!(!again.recording.enabled());
}

#[test]
fn invalid_update_changes_nothing() {
    let path = tmp_file("invalid");
    let store = SettingsStore::load(Some(path.clone()));
    let rev0 = store.rev();
    assert!(store.update(patch(r#"{"recording":{"max_count":0}}"#)).is_err());
    assert_eq!(store.rev(), rev0);
    assert!(!path.exists(), "nothing written on a rejected patch");
    assert_eq!(store.snapshot(), UiSettings::default());
}

#[test]
fn corrupt_file_falls_back_to_defaults_and_is_replaced_on_save() {
    let path = tmp_file("corrupt");
    std::fs::write(&path, b"{ half a json").unwrap();
    let store = SettingsStore::load(Some(path.clone()));
    assert!(store.load_note().contains("unreadable"));
    assert_eq!(store.snapshot(), UiSettings::default());
    store.update(patch(r#"{"recording":{"enabled":false}}"#)).unwrap();
    let s = parse_settings(&std::fs::read(&path).unwrap()).unwrap();
    assert!(!s.recording.enabled);
}

#[test]
fn in_memory_store_applies_but_does_not_persist() {
    let store = SettingsStore::load(None);
    let out = store.update(patch(r#"{"recording":{"enabled":false}}"#)).unwrap();
    assert!(!out.persisted);
    assert!(out.save_error.is_none());
    assert!(!store.recording.enabled());
}

#[test]
fn skipped_ids_are_bounded() {
    let p = RecordingPolicy::new(&RecordingSettings::default());
    for id in 0..(SKIPPED_IDS_KEEP as u64 + 10) {
        p.note_skipped(id);
    }
    assert!(!p.was_skipped(0), "oldest evicted");
    assert!(p.was_skipped(SKIPPED_IDS_KEEP as u64 + 9));
}

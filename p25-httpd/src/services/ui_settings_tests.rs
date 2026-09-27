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
fn call_close_and_storage_defaults() {
    let s = UiSettings::default();
    assert_eq!((s.call.hang_ms, s.call.end_grace_ms), (3_000, 2_000));
    assert_eq!(s.recording.storage, StorageKind::Ram);
    assert_eq!((s.recording.sd_max_count, s.recording.sd_max_mb), (2_000, 2_048));
    // A 056 file (no `call`, no storage fields) loads with these.
    let old = parse_settings(br#"{"recording":{"enabled":true,"max_count":40}}"#).unwrap();
    assert_eq!(old.call, CallSettings::default());
    assert_eq!(old.recording.storage, StorageKind::Ram);
}

#[test]
fn call_and_storage_patches_validate_ranges() {
    let base = UiSettings::default();
    let ok = |j: &str| apply_patch(&base, patch(j)).unwrap();
    let bad = |j: &str| assert!(apply_patch(&base, patch(j)).is_err(), "{j} should fail");
    let (s, ch) = ok(r#"{"call":{"hang_ms":1000,"end_grace_ms":0}}"#);
    assert_eq!((s.call.hang_ms, s.call.end_grace_ms), (1_000, 0));
    assert!(ch.call && !ch.recording);
    bad(r#"{"call":{"hang_ms":999}}"#);
    bad(r#"{"call":{"hang_ms":30001}}"#);
    bad(r#"{"call":{"end_grace_ms":10001}}"#);
    let (s, ch) = ok(r#"{"recording":{"storage":"sd","sd_max_count":5000,"sd_max_mb":16}}"#);
    assert_eq!(s.recording.storage, StorageKind::Sd);
    assert_eq!((s.recording.sd_max_count, s.recording.sd_max_mb), (5_000, 16));
    assert!(ch.recording && !ch.call);
    bad(r#"{"recording":{"sd_max_count":0}}"#);
    bad(r#"{"recording":{"sd_max_count":5001}}"#);
    bad(r#"{"recording":{"sd_max_mb":15}}"#);
    bad(r#"{"recording":{"sd_max_mb":32769}}"#);
    // Unknown store name / unknown call field: the patch does not parse.
    assert!(serde_json::from_str::<SettingsPatch>(r#"{"recording":{"storage":"usb"}}"#).is_err());
    assert!(serde_json::from_str::<SettingsPatch>(r#"{"call":{"idle_ms":1}}"#).is_err());
    // Hand-edited out-of-range values are clamped on load.
    let s = parse_settings(br#"{"call":{"hang_ms":5,"end_grace_ms":99999},
                               "recording":{"sd_max_mb":1,"sd_max_count":0}}"#)
        .unwrap();
    assert_eq!((s.call.hang_ms, s.call.end_grace_ms), (HANG_MS_MIN, END_GRACE_MS_MAX));
    assert_eq!((s.recording.sd_max_mb, s.recording.sd_max_count), (SD_MAX_MB_MIN, 1));
}

#[test]
fn live_policies_follow_updates() {
    let store = SettingsStore::load(None);
    assert_eq!(store.call.hang_ms(), DEFAULT_HANG_MS);
    assert_eq!(store.recording.storage(), StorageKind::Ram);
    store
        .update(patch(r#"{"call":{"hang_ms":2500,"end_grace_ms":1500},
                          "recording":{"storage":"sd","max_count":10,"sd_max_count":300,"sd_max_mb":64}}"#))
        .unwrap();
    assert_eq!((store.call.hang_ms(), store.call.end_grace_ms()), (2_500, 1_500));
    assert_eq!(store.recording.storage(), StorageKind::Sd);
    assert_eq!(
        store.recording.retention(),
        Retention { ram_max_count: 10, sd_max_count: 300, sd_max_bytes: 64 * 1024 * 1024 }
    );
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

// RX gain set from the Radio view survives a restart (bench 2026-09-27:
// the boot default, manual 60 dB, starved core 0.2.0 on the site antenna).
#[test]
fn radio_gain_is_validated_persisted_and_reloaded() {
    assert_eq!(UiSettings::default().radio, RadioSettings::default());
    let base = UiSettings::default();
    assert!(apply_patch(&base, patch(r#"{"radio":{"gain_mode":"auto"}}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"radio":{"manual_gain_db":77}}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"radio":{"manual_gain_db":-4}}"#)).is_err());
    let (s, ch) = apply_patch(&base, patch(r#"{"radio":{"gain_mode":"slow_attack"}}"#)).unwrap();
    assert!(ch.radio && ch.any());
    assert_eq!(s.radio.gain_mode.as_deref(), Some("slow_attack"));
    assert_eq!(s.radio.manual_gain_db, None);

    let path = tmp_file("radio");
    let store = SettingsStore::load(Some(path.clone()));
    let out = store
        .update(patch(r#"{"radio":{"gain_mode":"manual","manual_gain_db":66}}"#))
        .unwrap();
    assert!(out.persisted && out.changed.radio);
    let again = SettingsStore::load(Some(path)).snapshot();
    assert_eq!(again.radio.gain_mode.as_deref(), Some("manual"));
    assert_eq!(again.radio.manual_gain_db, Some(66));

    // A hand-edited bad value is dropped, not fatal.
    let s = parse_settings(br#"{"radio":{"gain_mode":"turbo","manual_gain_db":200}}"#).unwrap();
    assert_eq!(s.radio, RadioSettings::default());
}

// Change 063: talkgroup groups and speaker routing. The operator's setup:
// Primary (300) left; TAC (301-310) and Hospital right, TAC above Hospital.
fn site_groups() -> &'static str {
    r#"{"tg_groups":[{"name":"Primary","tgs":[300]},
                     {"name":"TAC","tgs":[301,302,303,304,305,306,307,308,309,310]},
                     {"name":"Hospital","tgs":[318,319]}],
        "speakers":{"left":["Primary"],"right":["tac","Hospital"],"other":"off","preempt":true}}"#
}

#[test]
fn groups_and_speakers_validate() {
    let base = UiSettings::default();
    let (s, ch) = apply_patch(&base, patch(site_groups())).unwrap();
    assert!(ch.tg_groups && ch.speakers);
    // Names resolve to the group's spelling.
    assert_eq!(s.speakers.right, vec!["TAC".to_string(), "Hospital".to_string()]);
    // Duplicate names, TG 0, a group on both sides, and a nameless group fail.
    assert!(apply_patch(&base, patch(r#"{"tg_groups":[{"name":"A","tgs":[1]},{"name":"a","tgs":[2]}]}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"tg_groups":[{"name":"A","tgs":[0]}]}"#)).is_err());
    assert!(apply_patch(&base, patch(r#"{"tg_groups":[{"name":"  ","tgs":[5]}]}"#)).is_err());
    assert!(apply_patch(&s, patch(r#"{"speakers":{"left":["TAC"],"right":["TAC"]}}"#)).is_err());
    // Deleting a group drops it from the routing.
    let (s2, _) = apply_patch(&s, patch(r#"{"tg_groups":[{"name":"Primary","tgs":[300]}]}"#)).unwrap();
    assert_eq!(s2.speakers.left, vec!["Primary".to_string()]);
    assert!(s2.speakers.right.is_empty());
    // A hand-edited file keeps its valid groups and routing.
    let p = parse_settings(br#"{"tg_groups":[{"name":"X","tgs":[7,7,0]},{"name":"x","tgs":[8]}],
        "speakers":{"left":["X","Nope"],"right":["X"]}}"#).unwrap();
    assert_eq!(p.tg_groups.len(), 1);
    assert_eq!(p.tg_groups[0].tgs, vec![7]);
    assert_eq!((p.speakers.left.len(), p.speakers.right.len()), (1, 0));
}

#[test]
fn routing_follows_sides_and_priority() {
    let (s, _) = apply_patch(&UiSettings::default(), patch(site_groups())).unwrap();
    let r = Routing::new(&s.tg_groups, &s.speakers);
    assert_eq!(r.route(300).map(|x| (x.side, x.rank)), Some((Side::Left, 0)));
    assert_eq!(r.route(305).map(|x| (x.side, x.rank)), Some((Side::Right, 1)));
    assert_eq!(r.route(318).map(|x| (x.side, x.rank)), Some((Side::Right, 2)));
    assert_eq!(r.route(402), None, "other talkgroups are off");
    // TAC takes the chain from Hospital, Primary from both, never upward.
    assert!(r.preempts(305, 318));
    assert!(r.preempts(300, 305));
    assert!(!r.preempts(318, 305));
    assert!(!r.preempts(318, 319), "same group: first come");
    assert!(!r.preempts(402, 318), "not followed never pre-empts");
    // Pre-emption off: first come for everything.
    let mut sp = s.speakers.clone();
    sp.preempt = false;
    assert!(!Routing::new(&s.tg_groups, &sp).preempts(300, 318));
    // A group on neither side is not followed.
    sp.right.retain(|n| n != "Hospital");
    assert_eq!(Routing::new(&s.tg_groups, &sp).route(318), None);
}

#[test]
fn no_groups_follows_everything_without_preemption() {
    let d = UiSettings::default();
    let r = Routing::new(&d.tg_groups, &d.speakers);
    assert_eq!(r.route(300).map(|x| (x.side, x.rank)), Some((Side::Both, OTHER_RANK)));
    assert!(!r.preempts(300, 402));
}

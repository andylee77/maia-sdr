use super::*;

#[test]
fn sync_dibits_are_plus_or_minus_three() {
    // 0xDFF57D75DF5D: D = 11 01, F = 11 11, 5 = 01 01, 7 = 01 11.
    let d = DmrSyncPattern::BaseStationData.to_dibits();
    assert_eq!(&d[..6], &[3, 1, 3, 3, 3, 3]);
    let plus = d.iter().filter(|x| **x == 1).count();
    // DMR syncs have equal numbers of +3 and -3 symbols.
    assert_eq!(plus, 12);
}

#[test]
fn every_sync_word_is_balanced() {
    for p in [
        DmrSyncPattern::BaseStationData,
        DmrSyncPattern::BaseStationVoice,
        DmrSyncPattern::MobileStationData,
        DmrSyncPattern::MobileStationVoice,
        DmrSyncPattern::DirectDataTimeslot1,
        DmrSyncPattern::DirectDataTimeslot2,
        DmrSyncPattern::DirectVoiceTimeslot1,
        DmrSyncPattern::DirectVoiceTimeslot2,
    ] {
        let s: f32 = p.to_symbols().iter().sum();
        assert!(s.abs() < 1e-4, "{:?}", p);
    }
}

#[test]
fn detector_finds_the_sent_pattern() {
    let mut det = DmrSoftSyncDetector::default();
    let mut score = 0.0;
    for s in DmrSyncPattern::BaseStationVoice.to_symbols() {
        score = det.process_and_calculate(s);
    }
    assert_eq!(det.detected_pattern(), DmrSyncPattern::BaseStationVoice);
    // 24 symbols of (3 pi / 4)^2.
    let ideal = 24.0 * (3.0 * std::f32::consts::PI / 4.0).powi(2);
    assert!((score - ideal).abs() < 1e-3);
}

#[test]
fn mode_monitor_locks_to_base_station() {
    let mut m = DmrSyncModeMonitor::default();
    let mut mode = None;
    for _ in 0..11 {
        mode = m.detected(DmrSyncPattern::BaseStationData);
    }
    assert_eq!(mode, Some(DmrSyncDetectMode::BaseOnly));
    assert_eq!(m.detected(DmrSyncPattern::MobileStationData), None);
}

#[test]
fn voice_superframe_sequence() {
    let mut p = DmrSyncPattern::BaseStationVoice;
    let mut seen = vec![p];
    for _ in 0..5 {
        p = p.next_voice();
        seen.push(p);
    }
    assert_eq!(seen.last(), Some(&DmrSyncPattern::BsVoiceFrameF));
    assert_eq!(DmrSyncPattern::BsVoiceFrameF.next_voice(), DmrSyncPattern::Unknown);
}

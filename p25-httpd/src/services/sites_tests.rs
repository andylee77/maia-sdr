use super::*;

#[test]
fn snap_lo_top_clay_8m() {
    // Clay: CC = 860.9625, traffic 852-861 MHz (all below CC).
    // 8M preset: half_sr = 4 MHz, margin 250 kHz → CC at +3.75 MHz of LO.
    // LO = 860.9625 - 3.75 = 857.2125 MHz.
    let site = Site {
        name: "clay".into(),
        label: "Clay County".into(),
        modulation: "LSM".into(),
        preset_default: "8M".into(),
        cc_position: CcPosition::Top,
        control_freq_hz: 860_962_500,
        alt_control_freqs_hz: vec![],
        traffic_freqs_hz: vec![],
        nac: None, wacn: None, system_id: None, rfss_id: None, site_id: None, lra: None,
        iden_bands: vec![],
        last_updated_unix_ms: None,
        notes: vec![], seed_source: None, runtime_overlay_path: None,
    };
    let lo = site.snap_lo_hz(8_000_000, 0.0, 250_000.0);
    assert_eq!(lo, 857_212_500);
    // Resulting NCO offset for the CC: 860.9625 - 857.2125 = 3.75 MHz,
    // well inside ±4 MHz IF window with 250 kHz margin to the edge.
}

#[test]
fn snap_lo_center_duval_8m() {
    let site = Site {
        name: "duval".into(),
        label: "Duval".into(),
        modulation: "LSM".into(),
        preset_default: "8M".into(),
        cc_position: CcPosition::Center,
        control_freq_hz: 855_487_500,
        alt_control_freqs_hz: vec![],
        traffic_freqs_hz: vec![],
        nac: None, wacn: None, system_id: None, rfss_id: None, site_id: None, lra: None,
        iden_bands: vec![],
        last_updated_unix_ms: None,
        notes: vec![], seed_source: None, runtime_overlay_path: None,
    };
    let lo = site.snap_lo_hz(8_000_000, 0.0, 250_000.0);
    assert_eq!(lo, 855_487_500);
}

#[test]
fn snap_lo_bottom_arbitrary() {
    let site = Site {
        name: "test".into(),
        label: "Test".into(),
        modulation: "LSM".into(),
        preset_default: "4M".into(),
        cc_position: CcPosition::Bottom,
        control_freq_hz: 800_000_000,
        alt_control_freqs_hz: vec![],
        traffic_freqs_hz: vec![],
        nac: None, wacn: None, system_id: None, rfss_id: None, site_id: None, lra: None,
        iden_bands: vec![],
        last_updated_unix_ms: None,
        notes: vec![], seed_source: None, runtime_overlay_path: None,
    };
    // 4 MHz preset, half_sr = 2 MHz, margin 250 kHz → CC at -1.75 MHz of LO.
    // LO = 800 + 1.75 = 801.75 MHz.
    let lo = site.snap_lo_hz(4_000_000, 0.0, 250_000.0);
    assert_eq!(lo, 801_750_000);
}

#[test]
fn snap_lo_includes_lo_shift() {
    // Crystal ppm trim adds directly to the LO command.
    let site = Site {
        name: "x".into(), label: "x".into(), modulation: "LSM".into(),
        preset_default: "8M".into(),
        cc_position: CcPosition::Center,
        control_freq_hz: 860_000_000,
        alt_control_freqs_hz: vec![], traffic_freqs_hz: vec![],
        nac: None, wacn: None, system_id: None, rfss_id: None, site_id: None, lra: None,
        iden_bands: vec![], last_updated_unix_ms: None,
        notes: vec![], seed_source: None, runtime_overlay_path: None,
    };
    let lo_no_shift = site.snap_lo_hz(8_000_000, 0.0, 250_000.0);
    let lo_with_shift = site.snap_lo_hz(8_000_000, 470.0, 250_000.0);
    assert_eq!(lo_with_shift - lo_no_shift, 470);
}

#[test]
fn parse_clay_seed() {
    // Round-trip the embedded clay seed through the loader. Force
    // the overlay dir to a tmp location so an operator's persisted
    // overlay can't bleed into this test.
    std::env::set_var(
        "P25_SITES_OVERLAY_DIR",
        std::env::temp_dir().join("p25_sites_test_empty").to_string_lossy().to_string(),
    );
    let site = load_site("clay").expect("load clay seed");
    assert_eq!(site.name, "clay");
    assert_eq!(site.control_freq_hz, 860_962_500);
    assert_eq!(site.cc_position, CcPosition::Top);
    assert_eq!(site.nac, Some(2209));
}

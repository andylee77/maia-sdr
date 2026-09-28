//! Host tests for `app::discovery` (change 071).

use super::*;

#[test]
fn steps_cover_the_bands() {
    let uh = 7_200_000.0;
    let s = plan_steps(DEFAULT_BANDS, uh);
    // 700 (12 MHz) and 900 (6 MHz) take one window each, 800 (18 MHz) two.
    assert_eq!(s.len(), 4);
    for &(lo, hi) in DEFAULT_BANDS {
        let mut f = lo;
        while f <= hi {
            assert!(s.iter().any(|&c| (f as f64 - c as f64).abs() <= uh), "{f} uncovered");
            f += 100_000;
        }
    }
    assert_eq!(ScanRequest::default().bands().len(), 3);
    assert_eq!(ScanRequest { all: true, ..Default::default() }.bands().len(), 5);
}

fn frames_with(carriers: &[(usize, f32, usize)], n_frames: usize) -> Vec<Vec<f32>> {
    // 4096 bins at -100 dB; (bin, level, frames present)
    (0..n_frames)
        .map(|k| {
            let mut f = vec![-100.0f32; 4096];
            for &(bin, lvl, present) in carriers {
                if k < present {
                    f[bin] = -100.0 + lvl;
                    f[bin + 1] = -100.0 + lvl - 6.0;
                }
            }
            f
        })
        .collect()
}

#[test]
fn continuous_carriers_found_bursty_ones_not() {
    let span = 16_000_000.0;
    let bin = span / 4096.0;
    // A control channel at +1 MHz (always there), a traffic channel at
    // -2 MHz (2 of 8 frames), the DC spur, and a carrier beyond the
    // usable window.
    let cc_bin = 2048 + (1_000_000.0 / bin) as usize;
    let tr_bin = 2048 - (2_000_000.0 / bin) as usize;
    let edge_bin = 2048 + (7_800_000.0 / bin) as usize;
    let frames = frames_with(&[(cc_bin, 30.0, 8), (tr_bin, 30.0, 2), (2048, 40.0, 8), (edge_bin, 30.0, 8)], 8);
    let c = find_carriers(&frames, 860_000_000.0, span, 7_200_000.0, 12.0, 0.8);
    assert_eq!(c.len(), 1, "{c:?}");
    assert!((c[0].freq_hz as f64 - 861_000_000.0).abs() < bin * 1.5, "{}", c[0].freq_hz);
    assert!(c[0].level_db > 29.0 && c[0].persistence == 1.0);
    assert!(find_carriers(&[], 0.0, span, 1.0, 12.0, 0.8).is_empty());
}

#[test]
fn found_site_to_site_file() {
    let f = FoundSite {
        freq_hz: 936_250_000,
        level_db: 30.0,
        modulation: "C4FM".into(),
        tsbk_per_s: 27.0,
        crc_pct: 99.0,
        nac: Some(0x015),
        wacn: Some(0x92463),
        system_id: Some(0x00A),
        rfss_id: Some(21),
        site_id: Some(21),
        lra: None,
        bands: vec![],
        neighbours: vec![],
        secondary_hz: vec![936_250_000, 937_000_000],
        existing_site: None,
        via_neighbour: false,
    };
    assert_eq!(f.key(), "92463-00A-21-21");
    assert_eq!(site_name("Florida Power & Light (Clay)"), "florida_power_light_clay");
    assert_eq!(site_name("  "), "site");
    let s = to_site(&f, "fpl_clay", "FPL Clay");
    assert_eq!(s.control_freq_hz, 936_250_000);
    assert_eq!(s.alt_control_freqs_hz, vec![937_000_000]);
    assert_eq!(s.modulation, "C4FM");
    assert_eq!((s.nac, s.system_id, s.site_id), (Some(0x015), Some(0x00A), Some(21)));
    assert!(s.traffic_freqs_hz.is_empty());
}

#[test]
fn lease_is_exclusive() {
    let l = RadioLease::default();
    assert!(l.is_normal());
    assert!(l.take_sweep());
    assert!(!l.take_sweep());
    l.release();
    assert!(l.is_normal());
}

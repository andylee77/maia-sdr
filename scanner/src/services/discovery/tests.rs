//! Step planning, matching found sites to the configuration, and adding them.

use super::*;
use crate::protocol::events::{DmrIdentity, P25Identity};
use crate::radio::plan::usable_half_hz;
use crate::services::config::systems::SystemsConfig;

fn p25(freq_hz: u64, wacn: u32, system: u16, rfss: u8, site: u8) -> FoundSite {
    FoundSite {
        id: String::new(),
        freq_hz,
        level_db: 30.0,
        protocol: Protocol::P25,
        modulation: Some("lsm"),
        msgs_per_s: 40.0,
        ok_pct: 99.5,
        identity: HeardIdentity::P25(P25Identity {
            nac: Some(0x8A1),
            wacn: Some(wacn),
            system: Some(system),
            rfss: Some(rfss),
            site: Some(site),
            lra: Some(0),
        }),
        bands: Vec::new(),
        neighbours: Vec::new(),
        secondary_hz: vec![freq_hz, freq_hz + 475_000],
        timeslot: None,
        existing_site: None,
        via_neighbour: false,
    }
}

fn dmr(freq_hz: u64) -> FoundSite {
    FoundSite {
        protocol: Protocol::DmrTier3,
        modulation: None,
        identity: HeardIdentity::Dmr(DmrIdentity { colour_code: 0, model: "SMALL", network: 0, site: 2 }),
        secondary_hz: Vec::new(),
        timeslot: Some(1),
        ..p25(freq_hz, 0, 0, 0, 0)
    }
}

/// A card that names a system and its ticked sites and leaves the rest as heard.
fn card(label: &str, sites: &[(&FoundSite, &str)]) -> AddSystem {
    AddSystem {
        label: label.into(),
        identity: None,
        details: SystemDetails::default(),
        sites: sites
            .iter()
            .map(|(f, l)| AddSite { key: f.key(), label: (*l).into(), identity: None, control: None, channels_hz: Vec::new() })
            .collect(),
    }
}

#[test]
fn the_default_bands_take_eight_windows() {
    let uh = usable_half_hz(SWEEP_RATE_HZ) as f64;
    let s = plan_steps(&default_bands(), uh);
    // 700 (12 MHz) and 900 (6 MHz) take one window each; 800 (18), UHF (20) and VHF (24) two.
    assert_eq!(s.len(), 8, "{s:?}");
    assert_eq!(options().step_hz as f64, (2.0 * uh * 0.9).floor(), "the step the page shows is the plan's");
    for (lo, hi) in default_bands() {
        let mut f = lo;
        while f <= hi {
            assert!(s.iter().any(|&c| (f as f64 - c as f64).abs() <= uh), "{f} uncovered");
            f += 100_000;
        }
    }
    assert_eq!(ScanRequest::default().bands(), default_bands());
}

#[test]
fn found_sites_are_keyed_by_identity() {
    assert_eq!(p25(860_962_500, 0xBEE00, 0x8A1, 1, 1).key(), "p25:BEE00-8A1-1-1");
    assert_eq!(dmr(454_368_750).key(), "dmr:SMALL-0-2-0");
    let mut st = ScanState::default();
    let mut weak = p25(860_962_500, 0xBEE00, 0x8A1, 1, 1);
    weak.msgs_per_s = 10.0;
    st.found(weak);
    st.found(p25(860_487_500, 0xBEE00, 0x8A1, 1, 1));
    st.found(dmr(454_368_750));
    assert_eq!(st.sites.len(), 2);
    assert_eq!(st.sites[0].freq_hz, 860_487_500, "the stronger find of one site is kept");
    assert_eq!(st.sites[0].id, "p25:BEE00-8A1-1-1");
}

#[test]
fn a_scan_adds_systems_and_sites_and_merges_into_known_ones() {
    let mut config = SystemsConfig::default();
    let clay = p25(860_962_500, 0xBEE00, 0x8A1, 1, 1);
    let clay_2 = p25(857_000_000, 0xBEE00, 0x8A1, 1, 2);
    let cec = dmr(454_368_750);
    let found = vec![clay.clone(), clay_2.clone(), cec.clone()];
    let details = SystemDetails { location: Some("Green Cove Springs, FL".into()), county: Some(" Clay ".into()), ..Default::default() };
    let added = add(&mut config, &found, &AddSystem { details, ..card("Clay County", &[(&clay, "Clay"), (&clay_2, "Clay 2")]) }).unwrap();
    assert_eq!(added.systems, vec!["clay_county"]);
    assert_eq!(added.sites, vec!["clay_county_clay", "clay_county_clay_2"]);
    let added = add(&mut config, &found, &card("Clay Electric", &[(&cec, "Green Cove Springs")])).unwrap();
    assert_eq!((added.systems, added.sites), (vec!["clay_electric".to_string()], vec!["clay_electric_green_cove_springs".to_string()]));
    assert!(add(&mut config, &found, &card("Mixed", &[(&clay, "A"), (&cec, "B")])).is_err(), "one system a card");
    let sys = &config.systems[0];
    assert_eq!((sys.protocol, sys.identity.wacn, sys.identity.system), (Protocol::P25, Some(0xBEE00), Some(0x8A1)));
    assert_eq!((sys.details.location.as_deref(), sys.details.county.as_deref()), (Some("Green Cove Springs, FL"), Some("Clay")));
    assert_eq!(sys.sites.len(), 2, "both P25 sites in one system");
    let site = &sys.sites[0];
    assert_eq!((site.control.freq_hz, site.control.alternates_hz.clone()), (860_962_500, vec![861_437_500]));
    assert_eq!((site.identity.rfss, site.identity.site, site.identity.nac), (Some(1), Some(1), Some(0x8A1)));
    let dmr_site = &config.systems[1].sites[0];
    assert_eq!((dmr_site.identity.colour_code, dmr_site.control.timeslot), (Some(0), Some(1)));
    assert_eq!(config.systems[1].identity.model, Some(DmrModel::Small));

    // A rescan: the same sites are known; a new alternate is added, labels stay.
    let mut again = p25(860_962_500, 0xBEE00, 0x8A1, 1, 1);
    again.secondary_hz = vec![860_962_500, 861_437_500, 858_000_000];
    assert_eq!(existing_site(&again, &config).as_deref(), Some("clay_county_clay"));
    let by_channel = p25(860_963_000, 0, 0, 0, 0);
    assert_eq!(existing_site(&by_channel, &config).as_deref(), Some("clay_county_clay"), "a control channel within 3 kHz");
    let added = add(&mut config, &[again.clone()], &card("Renamed system", &[(&again, "Renamed")])).unwrap();
    assert_eq!((added.systems.len(), added.sites.len(), added.updated.clone()), (0, 0, vec!["clay_county_clay".to_string()]));
    let site = &config.systems[0].sites[0];
    assert_eq!(site.label, "Clay");
    assert_eq!(site.control.alternates_hz, vec![861_437_500, 858_000_000]);
    assert_eq!(config.systems[0].label, "Clay County", "a configured system keeps its name");

    // A name is needed; an id already taken gets a suffix.
    let other = p25(770_000_000, 0xBEE00, 0x8A1, 2, 9);
    assert!(add(&mut config, &[other.clone()], &card("Clay County", &[(&other, " ")])).is_err());
    let added = add(&mut config, &[other.clone()], &card("Clay County", &[(&other, "Clay")])).unwrap();
    assert_eq!(added.sites, vec!["clay_county_clay_3"]);
    let nowhere = AddSite { key: "p25:none".into(), label: "x".into(), identity: None, control: None, channels_hz: Vec::new() };
    assert!(add(&mut config, &[], &AddSystem { sites: vec![nowhere], ..card("x", &[]) }).is_err());
    assert!(add(&mut config, &[], &card("x", &[])).is_err(), "nothing ticked");
}

#[test]
fn a_configured_site_heard_on_another_channel_moves_there() {
    let mut config = SystemsConfig::default();
    let wrong = dmr(454_118_750);
    add(&mut config, &[wrong.clone()], &card("Clay Electric", &[(&wrong, "Green Cove Springs")])).unwrap();
    let right = dmr(454_368_750);
    assert_eq!(existing_site(&right, &config).as_deref(), Some("clay_electric_green_cove_springs"), "the same site by identity");
    let added = add(&mut config, &[right.clone()], &card("Clay Electric", &[(&right, "Green Cove Springs")])).unwrap();
    assert_eq!((added.sites.len(), added.updated.clone()), (0, vec!["clay_electric_green_cove_springs".to_string()]));
    let site = &config.systems[0].sites[0];
    assert_eq!((site.label.as_str(), site.control.freq_hz, site.control.timeslot), ("Green Cove Springs", 454_368_750, Some(1)));
    assert!(site.control.alternates_hz.is_empty(), "the old channel is not kept: the site does not announce it");
}

#[test]
fn a_card_sets_what_the_user_changed() {
    let clay = p25(860_962_500, 0xBEE00, 0x8A0, 1, 1);
    let mut c = card("Clay County Public Safety", &[(&clay, "Simulcast")]);
    c.sites[0].identity = Some(SiteIdentity { rfss: Some(1), site: Some(1), nac: Some(0x8A1), ..Default::default() });
    c.sites[0].control = Some(Control { freq_hz: 860_962_500, alternates_hz: vec![858_987_500, 860_437_500], ..Default::default() });
    c.sites[0].channels_hz = vec![855_237_500, 856_437_500];
    let mut config = SystemsConfig::default();
    add(&mut config, &[clay.clone()], &c).unwrap();
    let site = &config.systems[0].sites[0];
    assert_eq!((config.systems[0].label.as_str(), site.label.as_str()), ("Clay County Public Safety", "Simulcast"));
    assert_eq!(site.control.alternates_hz, [858_987_500, 860_437_500]);
    assert_eq!(site.channels_hz, [855_237_500, 856_437_500]);

    let mut bad = c.clone();
    bad.sites[0].identity = Some(SiteIdentity { nac: Some(0x1000), ..Default::default() });
    assert!(add(&mut SystemsConfig::default(), &[clay], &bad).is_err(), "a NAC is 3 hex digits");
}

#[test]
fn a_control_timeslot_is_kept_only_when_one_clearly_carries_the_messages() {
    use super::probe::control_slot;
    assert_eq!(control_slot([120, 3]), Some(1));
    assert_eq!(control_slot([2, 90]), Some(2));
    assert_eq!(control_slot([60, 55]), None);
    assert_eq!(control_slot([0, 0]), None);
}

#[test]
fn dmr_finds_land_on_the_channel_raster() {
    use super::probe::on_raster;
    assert_eq!(on_raster(454_369_375), 454_368_750);
    assert_eq!(on_raster(451_086_000), 451_087_500);
    assert_eq!(on_raster(151_001_900), 151_002_500);
}

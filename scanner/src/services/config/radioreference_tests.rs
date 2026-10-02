//! RadioReference's talkgroups and sites files, read and imported.

use super::*;
use crate::services::config::aliases::AliasId;

const TALKGROUPS: &str = "Decimal,Hex,Alpha Tag,Mode,Description,Tag,Category\r\n\
    100,064,\"Fire Dispatch\",\"D\",\"Dispatch\",\"Fire Dispatch\",\"County Fire\"\r\n\
    101,065,\"Fire TAC 1\",\"DE\",\"Tactical 1\",\"Fire-Tac\",\"County Fire\"\r\n\
    102,066,\"\",\"De\",\"Tactical 2, North\",\"Fire-Tac\",\"County Fire\"\r\n\
    200,0c8,\"Sheriff \"\"A\"\"\",\"T\",\"Sheriff\",\"Law Dispatch\",\"\"\r\n\
    100,064,\"Again\",\"D\",\"\",\"\",\"\"\r\n";

const P25_SITES: &str = "RFSS,Site Dec,Site Hex,Site NAC,Description,County Name,Lat,Lon,Range,Frequencies\n\
    1,001,1,8A1,\"Simulcast\",\"Clay\",29.96467,-81.87462,20,855.237500,856.437500,858.987500c,860.962500c\n\
    3,003,3,9,\"North\",\"Duval\",30.1,-81.6,25,851.012500c,852.5125\n\
    5,005,5,,\"Quiet\",\"Duval\",,,,852.000000\n";

const DMR_SITES: &str = "Region,Site Dec,Site Hex,Description,County Name,Lat,Lon,Range,Frequencies\n\
    1,003,3,\"Green Cove\",\"Clay\",30.05,-81.74,25,454.368750c,454.537500\n";

fn system(protocol: Protocol, sites: Vec<Site>) -> System {
    System {
        id: "clay".into(),
        label: "Clay County".into(),
        protocol,
        identity: Default::default(),
        details: Default::default(),
        aliases: Vec::new(),
        listening: Default::default(),
        sites,
    }
}

fn scanned(id: &str, rfss: Option<u32>, site: Option<u32>, control_hz: u64) -> Site {
    Site {
        id: id.into(),
        label: "Scanned".into(),
        identity: SiteIdentity { rfss, site, ..Default::default() },
        control: Control { freq_hz: control_hz, ..Default::default() },
        modulation: Default::default(),
        channels_hz: Vec::new(),
        channel_plan: None,
        window: Default::default(),
        notes: Vec::new(),
        source: None,
    }
}

fn run(sys: &mut System, csv: &str, req: &ImportRequest) -> Result<Imported, String> {
    let file = parse(csv)?;
    let ids: Vec<String> = sys.sites.iter().map(|s| s.id.clone()).collect();
    import(sys, &file, req, &ids)
}

#[test]
fn talkgroups_become_aliases_named_and_grouped_as_sdrtrunk_makes_them() {
    let mut sys = system(Protocol::P25, Vec::new());
    let out = run(&mut sys, TALKGROUPS, &ImportRequest { encrypted_do_not_monitor: false, ..Default::default() }).unwrap();
    assert_eq!(out.file, "talkgroups");
    let names: Vec<(&str, Option<&str>, bool)> =
        sys.aliases.iter().map(|a| (a.name.as_str(), a.group.as_deref(), a.do_not_monitor)).collect();
    assert_eq!(
        names,
        [
            ("Fire Dispatch", Some("County Fire"), false),
            ("Fire TAC 1", Some("County Fire"), false),
            ("Tactical 2, North", Some("County Fire"), false),
            ("Sheriff \"A\"", None, false),
        ],
        "the alpha tag (or the description), the category; the first row of a talkgroup wins"
    );
    assert_eq!(sys.aliases[0].ids, [AliasId::Talkgroup { value: 100 }]);
    assert_eq!(sys.aliases[0].priority, None, "followed as the system's listening settings say");
}

#[test]
fn fully_encrypted_talkgroups_are_never_followed_unless_asked() {
    let mut sys = system(Protocol::P25, Vec::new());
    let req: ImportRequest = serde_json::from_str(&serde_json::json!({ "csv": TALKGROUPS }).to_string()).unwrap();
    assert!(req.encrypted_do_not_monitor, "on unless turned off, as in SDRTrunk");
    run(&mut sys, TALKGROUPS, &req).unwrap();
    let muted: Vec<&str> = sys.aliases.iter().filter(|a| a.do_not_monitor).map(|a| a.name.as_str()).collect();
    assert_eq!(muted, ["Fire TAC 1"], "DE is fully encrypted, De partly");
}

#[test]
fn talkgroups_an_alias_already_covers_are_kept() {
    let mut sys = system(Protocol::P25, Vec::new());
    let fire = Alias { ids: vec![AliasId::TalkgroupRange { min: 100, max: 101 }], priority: Some(1), ..Alias::talkgroup(0, "Fire") };
    sys.aliases.push(fire.clone());
    let out = run(&mut sys, TALKGROUPS, &ImportRequest::default()).unwrap();
    assert_eq!(out.talkgroups_kept, [100, 101]);
    assert_eq!(out.aliases_added.len(), 2);
    assert_eq!(sys.aliases[0], fire);
    let again = run(&mut sys, TALKGROUPS, &ImportRequest::default()).unwrap();
    assert!(again.aliases_added.is_empty(), "a second import adds nothing");
}

#[test]
fn p25_sites_start_on_their_first_control_channel() {
    let mut sys = system(Protocol::P25, Vec::new());
    let out = run(&mut sys, P25_SITES, &ImportRequest::default()).unwrap();
    assert_eq!(out.sites_added.iter().map(|s| s.key.as_str()).collect::<Vec<_>>(), ["1-1", "3-3"]);
    assert_eq!(out.skipped, ["5-5 (Quiet): no control channel"]);
    let s = &sys.sites[0];
    assert_eq!((s.id.as_str(), s.label.as_str()), ("clay_county_simulcast", "Simulcast"));
    assert_eq!((s.identity.rfss, s.identity.site, s.identity.nac), (Some(1), Some(1), Some(0x8A1)));
    assert_eq!((s.control.freq_hz, s.control.alternates_hz.clone()), (858_987_500, vec![860_962_500]));
    assert_eq!(s.channels_hz, [855_237_500, 856_437_500, 860_962_500]);
    assert_eq!(s.notes, ["RadioReference: Clay County; 29.96467, -81.87462; range 20 mi"]);
    assert_eq!(s.source.as_deref(), Some("RadioReference sites file (RFSS 1, site 1)"));
    assert_eq!(sys.sites[1].identity.nac, Some(0x009));
    assert_eq!(sys.sites[1].channels_hz, [852_512_500]);
}

#[test]
fn a_configured_site_gains_only_the_channels_it_lacks() {
    let mut sys = system(Protocol::P25, vec![scanned("clay_simulcast", Some(1), Some(1), 860_962_500)]);
    let only = ImportRequest { sites: Some(vec!["1-1".into()]), ..Default::default() };
    let out = run(&mut sys, P25_SITES, &only).unwrap();
    assert!(out.sites_added.is_empty() && out.skipped.is_empty(), "rows not asked for are left out");
    assert_eq!(out.sites_updated.len(), 1);
    let s = &sys.sites[0];
    assert_eq!((s.label.as_str(), s.control.freq_hz), ("Scanned", 860_962_500), "its name and control channel stay");
    assert_eq!(s.control.alternates_hz, [858_987_500]);
    assert_eq!(s.channels_hz, [855_237_500, 856_437_500, 858_987_500]);
    assert_eq!(s.identity.nac, Some(0x8A1));
    assert!(run(&mut sys, P25_SITES, &only).unwrap().sites_updated.is_empty(), "nothing left to gain");
}

#[test]
fn p25_sites_are_matched_by_rfss_and_site_not_by_a_shared_frequency() {
    let mut sys = system(Protocol::P25, vec![scanned("clay_north", Some(1), Some(2), 858_987_500)]);
    let out = run(&mut sys, P25_SITES, &ImportRequest::default()).unwrap();
    assert!(out.sites_updated.is_empty());
    assert_eq!(out.sites_added.len(), 2);
}

#[test]
fn dmr_sites_are_matched_by_control_channel() {
    let mut sys = system(Protocol::DmrTier3, Vec::new());
    let out = run(&mut sys, DMR_SITES, &ImportRequest::default()).unwrap();
    let s = &out.sites_added[0].site;
    assert_eq!((s.control.freq_hz, s.channels_hz.clone()), (454_368_750, vec![454_537_500]));
    assert_eq!(s.identity, SiteIdentity::default());
    assert_eq!(s.source.as_deref(), Some("RadioReference sites file (region 1, site 3)"));

    let mut sys = system(Protocol::DmrTier3, vec![scanned("cec_gcs", None, Some(3), 454_368_750)]);
    let out = run(&mut sys, DMR_SITES, &ImportRequest::default()).unwrap();
    assert!(out.sites_added.is_empty());
    assert_eq!(sys.sites[0].channels_hz, [454_537_500]);
}

#[test]
fn a_sites_file_belongs_to_a_system_of_its_protocol() {
    let e = run(&mut system(Protocol::P25, Vec::new()), DMR_SITES, &ImportRequest::default()).unwrap_err();
    assert!(e.contains("DMR sites file"), "{e}");
    assert!(run(&mut system(Protocol::DmrTier3, Vec::new()), P25_SITES, &ImportRequest::default()).is_err());
}

#[test]
fn a_bad_file_is_refused_whole() {
    let e = parse("Name,Value\nx,1\n").unwrap_err();
    assert!(e.contains("not a RadioReference"), "{e}");
    let e = parse("Decimal,Hex,Alpha Tag\n100,064,A\nx,0,B\n").unwrap_err();
    assert!(e.contains("line 3"), "{e}");
    let sites = |row: &str| parse(&format!("RFSS,Site Dec,Site NAC,Description,Frequencies\n{row}\n"));
    assert!(sites("1,1,8A1,A,85x.1c").is_err());
    assert!(sites("1,1,XYZ,A,851.0125c").is_err());
    assert!(sites("1,1,8A1,A,10.000000c").is_err(), "below 70 MHz");
    assert!(sites("1,1,8A1,A,851.0125c").is_ok());
}

#[test]
fn frequencies_are_read_exactly() {
    assert_eq!(frequency("454.368750c"), Ok((454_368_750, true)));
    assert_eq!(frequency("855.2375"), Ok((855_237_500, false)));
    assert_eq!(frequency("860"), Ok((860_000_000, false)));
    assert!(frequency("851.0125001").is_err());
}

#[test]
fn quotes_commas_a_byte_order_mark_and_blank_lines() {
    assert_eq!(fields(" a ,\"b, c\",\"d \"\"e\"\"\",,"), ["a", "b, c", "d \"e\"", "", ""]);
    let File::Talkgroups(rows) = parse("\u{feff}Decimal,Hex,Alpha Tag\n\n300,12c,X\n").unwrap() else { panic!("talkgroups") };
    assert_eq!(rows, [Talkgroup { id: 300, name: "X".into(), category: None, encrypted: false }]);
}

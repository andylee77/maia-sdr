//! A playlist read, taken into a configuration and written back.

use super::import::{import, ChannelState, Take};
use super::*;
use crate::services::config::aliases::Listening;
use crate::services::config::systems::{ChannelPlan, Control, Site, SystemsConfig};

/// A playlist as SDRTrunk writes one: six channels of two systems, and seven aliases in three
/// lists.
const PLAYLIST: &str = r#"<playlist version="4">
  <alias name="Fire Dispatch" color="-16711936" group="Fire" list="COUNTY" iconName="Fire Truck">
    <id type="record"/>
    <id type="talkgroup" value="300" protocol="APCO25"/>
  </alias>
  <alias name="Fire TAC" color="0" group="Fire" list="COUNTY">
    <id type="talkgroupRange" protocol="APCO25" max="316" min="301"/>
    <id type="priority" priority="12"/>
  </alias>
  <alias name="Sheriff 850" color="0" group="Sheriff" list="COUNTY">
    <id type="priority" priority="-1"/>
    <id type="talkgroup" value="850" protocol="APCO25"/>
  </alias>
  <alias name="Engine 15" color="0" list="COUNTY">
    <id type="radio" value="3400012" protocol="APCO25"/>
    <id type="radio" value="3597051" protocol="APCO25"/>
    <id type="radioRange" protocol="APCO25" min="1010" max="1020"/>
    <id type="tones"/>
  </alias>
  <alias name="Stream only" color="0" list="COUNTY">
    <id type="broadcastChannel" channel="feed"/>
  </alias>
  <alias name="Lake City" color="0" group="All" list="POWER-DMR">
    <id type="talkgroup" value="87921" protocol="DMR"/>
    <id type="talkgroup" value="300" protocol="APCO25"/>
  </alias>
  <alias name="Nobody's" color="0" list="SPARE">
    <id type="talkgroup" value="1" protocol="APCO25"/>
  </alias>
  <channel name="LCN 1" site="Main" system="County" order="1" enabled="false">
    <source_configuration type="sourceConfigTuner" frequency="855237500" source_type="TUNER"/>
    <alias_list_name>COUNTY</alias_list_name>
    <event_log_configuration/>
    <aux_decode_configuration/>
    <decode_configuration type="decodeConfigP25Phase1" modulation="CQPSK" ignore_encrypted_calls="false" ignore_data_calls="false" ignore_unmonitored_calls="false" traffic_channel_pool_size="0"/>
    <record_configuration/>
  </channel>
  <channel name="LCN 2" site="Main" system="County" order="1" enabled="true">
    <source_configuration type="sourceConfigTuner" frequency="860962500" preferred_tuner="A tuner" source_type="TUNER"/>
    <alias_list_name>COUNTY</alias_list_name>
    <decode_configuration type="decodeConfigP25Phase1" modulation="CQPSK" ignore_unmonitored_calls="true" traffic_channel_pool_size="12"/>
  </channel>
  <channel name="Control" site="North" system="County" order="0" enabled="false">
    <source_configuration type="sourceConfigTunerMultipleFrequency" frequency_rotation_delay="400" source_type="TUNER_MULTIPLE_FREQUENCIES">
      <frequency>851012500</frequency>
      <frequency>852512500</frequency>
    </source_configuration>
    <alias_list_name>COUNTY</alias_list_name>
    <decode_configuration type="decodeConfigP25Phase1" modulation="C4FM"/>
  </channel>
  <channel name="Control" site="North" system="County" order="0" enabled="false">
    <source_configuration type="sourceConfigTunerMultipleFrequency" source_type="TUNER_MULTIPLE_FREQUENCIES">
      <frequency>853000000</frequency>
      <frequency>853500000</frequency>
    </source_configuration>
    <alias_list_name>COUNTY</alias_list_name>
    <decode_configuration type="decodeConfigP25Phase1" modulation="C4FM"/>
  </channel>
  <channel name="Weather" site="Main" system="County" order="0" enabled="false">
    <source_configuration type="sourceConfigTuner" frequency="162550000" source_type="TUNER"/>
    <decode_configuration type="decodeConfigNBFM" bandwidth="BW_12_5"/>
  </channel>
  <channel name="Control" site="Plant" system="Power Co-op" order="0" enabled="false">
    <source_configuration type="sourceConfigTuner" frequency="454368750" source_type="TUNER"/>
    <alias_list_name>POWER-DMR</alias_list_name>
    <decode_configuration type="decodeConfigDMR" ignore_data_calls="false" ignore_crc="false" use_compressed_talkgroups="false" traffic_channel_pool_size="20">
      <timeslot lsn="5" downlink="454368750" uplink="0"/>
      <timeslot lsn="6" downlink="451087500" uplink="0"/>
    </decode_configuration>
  </channel>
</playlist>
"#;

/// The same kinds of element as older playlists spell them.
const OLD_SPELLING: &str = r#"<?xml version="1.0" ?>
<playlist version="4">
  <channel system="County" site="Main" name="P25-Control" enabled="true" order="1">
    <alias_list_name>NAMES</alias_list_name>
    <decode_configuration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="decodeConfigP25Phase1" modulation="C4FM" traffic_channel_pool_size="10" ignore_data_calls="true"/>
    <source_configuration xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="sourceConfigTuner" frequency="860962500" preferred_tuner="A tuner"/>
  </channel>
  <alias list="NAMES" name="Fire &amp; Rescue" color="-16711936" iconName="No Icon" recordAudio="true">
    <id xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="talkgroup" value="300" protocol="APCO25"/>
  </alias>
</playlist>
"#;

fn site(id: &str, label: &str, control: u64, alternates: &[u64]) -> Site {
    Site {
        id: id.into(),
        label: label.into(),
        identity: Default::default(),
        control: Control { freq_hz: control, alternates_hz: alternates.to_vec(), ..Default::default() },
        modulation: Modulation::Lsm,
        channels_hz: Vec::new(),
        channel_plan: None,
        window: Default::default(),
        notes: Vec::new(),
        source: None,
    }
}

/// A unit with the county's system as a scan found it, and one talkgroup switched off from the
/// live screen.
fn configured() -> SystemsConfig {
    let mut off = Alias::talkgroup(850, "TG 850");
    off.do_not_monitor = true;
    SystemsConfig {
        systems: vec![System {
            id: "county_ps".into(),
            label: "County Public Safety".into(),
            protocol: Protocol::P25,
            identity: Default::default(),
            details: Default::default(),
            aliases: vec![off],
            listening: Listening::default(),
            sites: vec![site("county_ps_simulcast", "Simulcast", 860_962_500, &[859_437_500]), site("county_ps_north", "North", 851_012_500, &[])],
        }],
        ..Default::default()
    }
}

#[test]
fn a_playlist_is_read_in_both_spellings() {
    let p = parse(PLAYLIST).unwrap();
    assert_eq!((p.channels.len(), p.aliases.len()), (6, 7));
    let dump = p.dump();
    let lines: Vec<&str> = dump.lines().collect();
    assert_eq!(lines[1], "channel|County|Main|LCN 2|enabled=true|decodeConfigP25Phase1|mod=CQPSK|ium=true|freqs=860962500|lcn=|list=COUNTY");
    assert_eq!(lines[2], "channel|County|North|Control|enabled=false|decodeConfigP25Phase1|mod=C4FM|ium=-|freqs=851012500,852512500|lcn=|list=COUNTY");
    assert_eq!(lines[4], "channel|County|Main|Weather|enabled=false|decodeConfigNBFM|mod=-|ium=-|freqs=162550000|lcn=|list=");
    assert_eq!(lines[5], "channel|Power Co-op|Plant|Control|enabled=false|decodeConfigDMR|mod=-|ium=-|freqs=454368750|lcn=5:454368750,6:451087500|list=POWER-DMR");
    assert_eq!(lines[6], "alias|COUNTY|Fire Dispatch|Fire|color=-16711936|icon=Fire Truck|record talkgroup:APCO25:300");
    assert_eq!(lines[7], "alias|COUNTY|Fire TAC|Fire|color=0|icon=|talkgroupRange:APCO25:301-316 priority:12");
    assert_eq!(lines[9], "alias|COUNTY|Engine 15||color=0|icon=|radio:APCO25:3400012 radio:APCO25:3597051 radioRange:APCO25:1010-1020 tones");
    assert_eq!(p.channels[0].protocol(), Some(Protocol::P25));
    assert_eq!(p.channels[4].protocol(), None);

    let old = parse(OLD_SPELLING).unwrap();
    assert_eq!(
        old.dump(),
        "channel|County|Main|P25-Control|enabled=true|decodeConfigP25Phase1|mod=C4FM|ium=-|freqs=860962500|lcn=|list=NAMES\n\
         alias|NAMES|Fire & Rescue||color=-16711936|icon=No Icon|talkgroup:APCO25:300\n"
    );
    assert!(parse("<systems/>").unwrap_err().contains("not an SDRTrunk playlist"));
    assert!(parse("Decimal,Hex,Alpha Tag").is_err());
}

#[test]
fn an_alias_is_taken_for_its_system_s_protocol() {
    let p = parse(PLAYLIST).unwrap();
    let of = |name: &str, protocol| p.aliases.iter().find(|a| a.name == name).unwrap().to_alias(protocol);
    let fire = of("Fire Dispatch", Protocol::P25).unwrap();
    assert_eq!((fire.color.as_deref(), fire.icon.as_deref(), fire.group.as_deref(), fire.record), (Some("#00ff00"), Some("Fire Truck"), Some("Fire"), true));
    assert_eq!(fire.ids, vec![AliasId::Talkgroup { value: 300 }]);
    let tac = of("Fire TAC", Protocol::P25).unwrap();
    assert_eq!((tac.priority, tac.do_not_monitor, tac.color.clone()), (Some(12), false, None));
    assert_eq!(tac.ids, vec![AliasId::TalkgroupRange { min: 301, max: 316 }]);
    // SDRTrunk's -1 is "never followed".
    let sheriff = of("Sheriff 850", Protocol::P25).unwrap();
    assert_eq!((sheriff.priority, sheriff.do_not_monitor), (None, true));
    // What the scanner has no use for is left; an alias with nothing else is not taken.
    assert_eq!(of("Engine 15", Protocol::P25).unwrap().ids.len(), 3);
    assert_eq!(of("Stream only", Protocol::P25), None);
    // Only the IDs of the system's protocol.
    assert_eq!(of("Lake City", Protocol::DmrTier3).unwrap().ids, vec![AliasId::Talkgroup { value: 87921 }]);
    assert_eq!(of("Lake City", Protocol::P25).unwrap().ids, vec![AliasId::Talkgroup { value: 300 }]);
    assert_eq!(of("Fire TAC", Protocol::DmrTier3), None);
    for a in p.aliases.iter().filter_map(|a| a.to_alias(Protocol::P25)) {
        assert_eq!(a.check(), Ok(()));
    }
}

#[test]
fn what_a_preview_suggests_for_a_unit_that_has_the_system() {
    let p = parse(PLAYLIST).unwrap();
    let mut config = configured();
    let out = import(&mut config, &p, None).unwrap();
    assert_eq!((out.channels, out.aliases), (6, 7));
    let county = &out.systems[0];
    assert_eq!((county.name.as_str(), county.protocol, county.matched.as_deref(), county.into.as_deref(), county.new_id.clone()), ("County", Some(Protocol::P25), Some("county_ps"), Some("county_ps"), None));
    let states: Vec<(&str, ChannelState, bool, Option<&str>)> = county.channels.iter().map(|c| (c.name.as_str(), c.state, c.picked, c.site_id.as_deref())).collect();
    assert_eq!(
        states,
        vec![
            ("LCN 1", ChannelState::New, false, None),
            ("LCN 2", ChannelState::Configured, false, Some("county_ps_simulcast")),
            ("Control", ChannelState::Gains, true, Some("county_ps_north")),
            ("Control", ChannelState::New, true, None),
            ("Weather", ChannelState::Unsupported, false, None),
        ]
    );
    assert_eq!(county.channels[0].note.as_deref(), Some("one of 2 channels at this site: the playlist does not say which is a control channel"));
    assert_eq!(county.channels[4].note.as_deref(), Some("NBFM is not a decoder the scanner follows"));
    let list = &county.lists[0];
    assert_eq!((list.name.as_str(), list.aliases, list.added, list.named, list.kept, list.skipped), ("COUNTY", 5, 3, 1, 0, 1));
    assert!(county.aliases);

    // The other system has no configured one on its frequency: one of its own, since its only
    // channel stands alone at its site.
    let power = &out.systems[1];
    assert_eq!((power.protocol, power.matched.clone(), power.into.clone(), power.new_id.as_deref()), (Some(Protocol::DmrTier3), None, None, Some("power_co_op")));
    assert_eq!((power.channels[0].state, power.channels[0].picked), (ChannelState::New, true));
    assert_eq!((power.lists[0].added, power.lists[0].skipped), (1, 0));
    assert_eq!(out.unused_lists.iter().map(|u| (u.name.as_str(), u.aliases)).collect::<Vec<_>>(), vec![("SPARE", 1)]);
    assert_eq!(out.systems_added, ["power_co_op"]);
    assert_eq!(out.sites_added, ["county_ps_north_853_mhz", "power_co_op_plant"]);
    assert_eq!(out.sites_updated, ["county_ps_north"]);
    assert_eq!((out.aliases_added, out.aliases_named), (4, 1));

    // The configuration after it.
    let sys = &config.systems[0];
    assert_eq!(sys.sites[1].control.alternates_hz, [852_512_500], "the configured site gained the frequency it lacked");
    let north = &sys.sites[2];
    assert_eq!((north.label.as_str(), north.control.freq_hz, north.control.alternates_hz.clone(), north.modulation), ("North 853 MHz", 853_000_000, vec![853_500_000], Modulation::C4fm));
    assert_eq!(north.source.as_deref(), Some("SDRTrunk playlist (County, North, Control)"));
    let names: Vec<(&str, bool)> = sys.aliases.iter().map(|a| (a.name.as_str(), a.do_not_monitor)).collect();
    assert_eq!(names, vec![("Sheriff 850", true), ("Fire Dispatch", false), ("Fire TAC", false), ("Engine 15", false)], "the talkgroup held under its number took its name");
    assert_eq!(sys.aliases[0].group.as_deref(), Some("Sheriff"));
    let dmr = &config.systems[1];
    assert_eq!((dmr.label.as_str(), dmr.protocol, dmr.listening.follow_unmonitored), ("Power Co-op", Protocol::DmrTier3, true));
    let plant = &dmr.sites[0];
    assert_eq!((plant.control.freq_hz, plant.control.lcn, plant.modulation), (454_368_750, Some(5), Modulation::Auto));
    assert_eq!(plant.channel_plan, Some(ChannelPlan { lcn_hz: [(5, 454_368_750), (6, 451_087_500)].into() }));
    assert_eq!(dmr.aliases.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Lake City"]);

    // The same playlist again adds nothing.
    let again = import(&mut config, &p, None).unwrap();
    assert_eq!((again.systems_added.len(), again.sites_added.len(), again.sites_updated.len(), again.aliases_added, again.aliases_named), (0, 0, 0, 0, 0));
    assert_eq!(again.systems[1].matched.as_deref(), Some("power_co_op"));
}

#[test]
fn a_request_says_what_is_taken_of_each_system() {
    let p = parse(PLAYLIST).unwrap();
    // A system of its own, from the channel SDRTrunk starts; the other system is not listed.
    let mut config = configured();
    let takes = [Take { name: "County".into(), into: None, channels: Some(vec![1, 4]), aliases: true }];
    let out = import(&mut config, &p, Some(&takes)).unwrap();
    assert_eq!((out.systems_added.clone(), out.sites_added.clone(), out.aliases_added, out.aliases_named), (vec!["county".to_string()], vec!["county_main".to_string()], 4, 0));
    let made = &config.systems[1];
    assert_eq!((made.id.as_str(), made.label.as_str(), made.sites[0].control.freq_hz, made.sites[0].modulation), ("county", "County", 860_962_500, Modulation::Lsm));
    assert!(!made.listening.follow_unmonitored, "its channel ignores talkgroups with no priority");
    assert!(made.aliases.iter().any(|a| a.name == "Sheriff 850" && a.do_not_monitor));
    assert_eq!(config.systems[0], configured().systems[0], "the configured system is as it was");
    assert!(!out.systems[0].channels[4].picked, "a channel the scanner cannot follow is not taken, asked or not");
    assert!(!out.systems[1].aliases && out.systems[1].channels.iter().all(|c| !c.picked));
    assert_eq!(config.systems.len(), 2);

    // With no channel of it taken, a system of its own is not made, and its aliases have no place.
    let mut config = configured();
    let out = import(&mut config, &p, Some(&[Take { name: "County".into(), into: None, channels: Some(vec![]), aliases: true }])).unwrap();
    assert_eq!((config.clone(), out.aliases_added, out.systems[0].aliases), (configured(), 0, false));
    assert_eq!(out.systems[0].lists[0].added, 4, "what it would add is still counted");

    // Only its aliases, into the system named.
    let out = import(&mut config, &p, Some(&[Take { name: "County".into(), into: Some("county_ps".into()), channels: Some(vec![]), aliases: true }])).unwrap();
    assert_eq!((out.aliases_added, out.aliases_named, out.sites_added.len(), out.sites_updated.len()), (3, 1, 0, 0));
    assert_eq!(config.systems[0].sites, configured().systems[0].sites);

    // Its channels left to the suggestion, for where it is sent: as a system of its own, the
    // channel SDRTrunk starts and the two that rotate, each a site.
    let mut config = configured();
    let out = import(&mut config, &p, Some(&[Take { name: "County".into(), into: None, channels: None, aliases: false }])).unwrap();
    assert_eq!(out.systems[0].channels.iter().filter(|c| c.picked).map(|c| c.index).collect::<Vec<_>>(), [1, 2, 3]);
    assert_eq!(out.sites_added, ["county_main", "county_north", "county_north_853_mhz"]);
    assert!(config.systems[1].aliases.is_empty());

    for (takes, why) in [
        (Take { name: "Nowhere".into(), into: None, channels: None, aliases: false }, "the playlist has no system \"Nowhere\""),
        (Take { name: "County".into(), into: Some("gone".into()), channels: None, aliases: true }, "no system gone"),
        (Take { name: "Power Co-op".into(), into: Some("county_ps".into()), channels: None, aliases: true }, "Power Co-op cannot go into County Public Safety"),
    ] {
        let mut config = configured();
        let e = import(&mut config, &p, Some(&[takes])).unwrap_err();
        assert!(e.contains(why), "{e}");
        assert_eq!(config, configured());
    }
}

#[test]
fn the_configuration_as_a_playlist_reads_back() {
    let p = parse(PLAYLIST).unwrap();
    let mut config = configured();
    import(&mut config, &p, None).unwrap();
    config.systems[0].aliases[2].speaker = Side::Left;
    config.systems[0].aliases[1].name = "Fire <Dispatch> & \"more\"".into();
    config.systems[0].listening.follow_unmonitored = false;
    let xml = export(&config.systems);
    let back = parse(&xml).unwrap();
    assert_eq!(back.channels.len(), 4, "a channel a site");
    let dump = back.dump();
    let lines: Vec<&str> = dump.lines().collect();
    assert_eq!(lines[0], "channel|County Public Safety|Simulcast|Control|enabled=false|decodeConfigP25Phase1|mod=CQPSK|ium=true|freqs=860962500,859437500|lcn=|list=County Public Safety");
    assert_eq!(lines[2], "channel|County Public Safety|North 853 MHz|Control|enabled=false|decodeConfigP25Phase1|mod=C4FM|ium=true|freqs=853000000,853500000|lcn=|list=County Public Safety");
    assert_eq!(lines[3], "channel|Power Co-op|Plant|Control|enabled=false|decodeConfigDMR|mod=-|ium=-|freqs=454368750|lcn=5:454368750,6:451087500|list=Power Co-op");
    assert_eq!(lines[4], "alias|County Public Safety|Sheriff 850|Sheriff|color=0|icon=|talkgroup:APCO25:850 priority:-1");
    assert_eq!(lines[5], "alias|County Public Safety|Fire <Dispatch> & \"more\"|Fire|color=-16711936|icon=Fire Truck|talkgroup:APCO25:300 record");
    // Each system's aliases come back as they were, but for the tuner a playlist has no place for.
    for sys in &config.systems {
        let got: Vec<Alias> = back.aliases.iter().filter(|a| a.list == sys.label).filter_map(|a| a.to_alias(sys.protocol)).collect();
        let want: Vec<Alias> = sys.aliases.iter().map(|a| Alias { speaker: Side::Both, ..a.clone() }).collect();
        assert_eq!(got, want, "{}", sys.label);
    }
    // And into a unit with nothing, the same systems and sites.
    let mut empty = SystemsConfig::default();
    let out = import(&mut empty, &back, None).unwrap();
    assert_eq!((out.systems_added.len(), out.sites_added.len()), (2, 4));
    for (made, was) in empty.systems.iter().zip(&config.systems) {
        assert_eq!((made.label.as_str(), made.protocol, made.listening.follow_unmonitored), (was.label.as_str(), was.protocol, was.listening.follow_unmonitored));
        let controls = |s: &System| s.sites.iter().map(|x| (x.label.clone(), x.control.freq_hz, x.control.alternates_hz.clone(), x.channel_plan.clone())).collect::<Vec<_>>();
        assert_eq!(controls(made), controls(was));
        assert_eq!(made.aliases.len(), was.aliases.len());
    }
}

#[test]
fn two_systems_of_one_name_get_lists_of_their_own() {
    let mut config = configured();
    let mut twin = config.systems[0].clone();
    twin.id = "county_ps_2".into();
    twin.sites.clear();
    config.systems.push(twin);
    let back = parse(&export(&config.systems)).unwrap();
    let lists: Vec<&str> = back.aliases.iter().map(|a| a.list.as_str()).collect();
    assert_eq!(lists, ["County Public Safety (county_ps)", "County Public Safety (county_ps_2)"]);
}

/// The lines of each playlist in `PLAYLIST_FILES` (separated by `;`), written beside the names
/// given in `PLAYLIST_OUT` in the same order: what `tools/sdrtrunk_playlist_reference.py` holds
/// against SDRTrunk's own reading of the files.
#[test]
#[ignore]
fn playlist_reference() {
    let files = std::env::var("PLAYLIST_FILES").expect("PLAYLIST_FILES");
    let outs = std::env::var("PLAYLIST_OUT").expect("PLAYLIST_OUT");
    for (file, out) in files.split(';').zip(outs.split(';')) {
        let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{file}: {e}"));
        let playlist = parse(&text).unwrap_or_else(|e| panic!("{file}: {e}"));
        std::fs::write(out, playlist.dump()).unwrap_or_else(|e| panic!("{out}: {e}"));
        println!("{file}: {} channels, {} aliases", playlist.channels.len(), playlist.aliases.len());
    }
}

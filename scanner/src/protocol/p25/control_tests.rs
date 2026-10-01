//! Host tests for `protocol::p25::control`.

use super::*;
use crate::protocol::p25::test_fixtures::*;
use crate::protocol::p25::types::{RadioId, Talkgroup};

fn iden(identifier: u8, spacing: u32, base: u64, slots: u8) -> TsbkMessage {
    TsbkMessage::IdentifierUpdate {
        identifier,
        bw: 100,
        transmit_offset: -45_000_000,
        channel_spacing: spacing,
        base_frequency: base,
        slots,
    }
}

fn feed(a: &mut Announced, msgs: Vec<TsbkMessage>) -> Vec<ControlEvent> {
    let mut out = Vec::new();
    for m in msgs {
        a.tsbk(0, 0, m, Now::now(), &mut out);
    }
    out.retain(|e| !matches!(e, ControlEvent::Message(_)));
    out
}

#[test]
fn identity_is_reported_when_it_changes() {
    let mut a = Announced::default();
    let net = || TsbkMessage::NetworkStatus { wacn: FLORIDA_WACN, system_id: CLAY_SYSTEM_ID, channel: Channel(0x0639) };
    let out = feed(&mut a, vec![net(), net()]);
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], ControlEvent::Identity(SiteIdentity::P25(P25Identity { wacn: Some(FLORIDA_WACN), .. }))));
    assert_eq!(a.control_channel, Some(Channel(0x0639)));
}

#[test]
fn the_channel_plan_resolves_channels() {
    let mut a = Announced::default();
    let out = feed(&mut a, vec![iden(0, 6_250, 851_006_250, 1), iden(0, 6_250, 851_006_250, 1)]);
    assert_eq!(out.len(), 1);
    assert_eq!(a.frequency(Channel(0x0639)), Some(CLAY_CONTROL_FREQ_HZ));
}

#[test]
fn grants_carry_the_channel_and_the_service_options() {
    let mut a = Announced::default();
    feed(&mut a, vec![iden(2, 12_500, 851_012_500, 2)]);
    let out = feed(
        &mut a,
        vec![
            TsbkMessage::GroupVoiceChannelGrant {
                channel: Channel(0x2000 | 228),
                talkgroup: Talkgroup(300),
                source: RadioId(1),
                service_options: 0x40,
            },
            TsbkMessage::GroupVoiceChannelGrantUpdate {
                channel_a: Channel(0x2000 | 228),
                talkgroup_a: Talkgroup(300),
                channel_b: Channel(0),
                talkgroup_b: Talkgroup(0),
            },
        ],
    );
    let grants: Vec<Grant> = out
        .into_iter()
        .filter_map(|e| match e {
            ControlEvent::Grant(g) => Some(g),
            _ => None,
        })
        .collect();
    assert_eq!(grants.len(), 2);
    let g = grants[0];
    assert_eq!((g.tg, g.source, g.encrypted, g.update), (300, Some(1), true, false));
    assert_eq!((g.channel.freq_hz, g.channel.tdma, g.channel.slot), (Some(852_437_500), true, Some(0)));
    assert!(grants[1].update && !grants[1].encrypted);
}

#[test]
fn neighbours_are_counted_and_reported_once() {
    let mut a = Announced::default();
    feed(&mut a, vec![iden(0, 6_250, 851_006_250, 1)]);
    let adj = |site: u8| TsbkMessage::AdjacentStatus {
        lra: 1,
        rfss_id: 1,
        site_id: site,
        channel: Channel(0x0639),
        system_id: 0x3BD,
        conventional: false,
        failure: false,
        valid: true,
        active: true,
        service_class: 0x70,
    };
    let out = feed(&mut a, vec![adj(2), adj(2), adj(3)]);
    assert_eq!(out.len(), 2);
    let n = &a.neighbours[&(0x3BD, 1, 2)];
    assert_eq!((n.count, n.neighbour.control.freq_hz), (2, Some(CLAY_CONTROL_FREQ_HZ)));
}

#[test]
fn accepted_unit_events_only() {
    let mut a = Announced::default();
    let out = feed(
        &mut a,
        vec![
            TsbkMessage::GroupAffiliationResponse {
                response: 0,
                announcement_group: Talkgroup(0),
                group: Talkgroup(300),
                target: RadioId(1234),
            },
            TsbkMessage::GroupAffiliationResponse {
                response: 2,
                announcement_group: Talkgroup(0),
                group: Talkgroup(301),
                target: RadioId(1234),
            },
            TsbkMessage::UnitDeRegistrationAcknowledge { wacn: 0, system_id: 0, target: RadioId(55) },
        ],
    );
    assert!(matches!(out[0], ControlEvent::Unit { unit: 1234, group: Some(300), kind: UnitKind::GroupAffiliation }));
    assert!(matches!(out[1], ControlEvent::Unit { unit: 55, group: None, kind: UnitKind::Deregistration }));
    assert_eq!(out.len(), 2);
}

#[test]
fn log_lines_use_sdrtrunk_names() {
    let a = Announced::default();
    let line = a.log_line(1, &TsbkMessage::RfssStatus { lra: 0, rfss_id: 1, site_id: 1, channel: Channel(0x0639) });
    assert_eq!(line.text, "TSBK2 RFSS_STS_BCAST RFSS:01 SITE:01");
    assert!(line.routine);
}

#[test]
fn new_system_forgets_the_site() {
    let mut c = P25Control::new("control");
    feed(&mut c.announced, vec![iden(0, 6_250, 851_006_250, 1)]);
    c.new_system();
    assert!(c.announced().bands.is_empty());
    assert_eq!(c.locked_nac(), 0);
}

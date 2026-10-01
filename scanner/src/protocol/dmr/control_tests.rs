//! Host tests for `protocol::dmr::control`.

use super::*;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::slot_type::DataType;
use crate::protocol::dmr::message::csbk;
use crate::protocol::dmr::message::data::DataBurst;
use crate::protocol::dmr::sync::DmrSyncPattern;

/// An on-air CSBK from Clay Electric.
fn on_air(hex: &str) -> DmrMessage {
    let bits = hex
        .chars()
        .flat_map(|c| {
            let v = c.to_digit(16).unwrap();
            (0..4).rev().map(move |i| ((v >> i) & 1) as u8)
        })
        .collect();
    DmrMessage::Csbk(csbk::create(DataBurst {
        pattern: DmrSyncPattern::BaseStationData,
        timeslot: 1,
        timestamp_ms: 0,
        cach: Cach { valid: true, busy: false, timeslot: 1, lcss: 0, payload: [0; 17] },
        color_code: 0,
        data_type: DataType::Csbk,
        bits,
        reserved: 0,
        valid: true,
    }))
}

#[test]
fn a_talkgroup_grant_names_the_lcn_and_timeslot() {
    let g = voice_grant(&on_air("B1000058015775014001E621")).unwrap();
    assert_eq!((g.tg, g.source, g.private), (87925, Some(81921), false));
    assert_eq!((g.channel.id, g.channel.slot), (ChannelId::DmrLcn(5), Some(2)));
    assert!(voice_grant(&on_air("99001101F6400B000000DEDA")).is_none());
}

#[test]
fn aloha_reports_the_identity_once() {
    let mut control = DmrControl::new(HashMap::new());
    let mut out = Vec::new();
    for _ in 0..3 {
        control.message(&on_air("99001101F6400B000000DEDA"), &mut out);
    }
    let identities: Vec<_> = out.iter().filter(|e| matches!(e, ControlEvent::Identity(_))).collect();
    assert_eq!(identities.len(), 1);
    let expected = DmrIdentity { colour_code: 0, model: "SMALL", network: 0, site: 2 };
    assert_eq!(control.identity(), Some(expected));
    let lines: Vec<&LogLine> = out
        .iter()
        .filter_map(|e| match e {
            ControlEvent::Message(l) => Some(l),
            _ => None,
        })
        .collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].routine && lines[0].valid);
    assert_eq!(control.stats.classes["Aloha"], (3, 0));
}

/// Offline: unit A's Clay Electric control captures (`DMR_CAPTURE_DIR` holding `cc_*.wav`).
#[test]
fn captured_control_channel_events() {
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("cc_")))
        .collect();
    files.sort();
    let (mut grants, mut lines) = (0, 0);
    for path in &files {
        let bytes = std::fs::read(path).unwrap();
        let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut control = DmrControl::new(HashMap::from([(5, 454_368_750), (6, 451_087_500)]));
        let mut out = Vec::new();
        for chunk in iq.chunks(2 * 1250) {
            control.push(chunk, &mut out);
        }
        assert_eq!(control.identity(), Some(DmrIdentity { colour_code: 0, model: "SMALL", network: 0, site: 2 }));
        for e in &out {
            match e {
                ControlEvent::Grant(g) => {
                    grants += 1;
                    let expected = match g.channel.id {
                        ChannelId::DmrLcn(5) => Some(454_368_750),
                        ChannelId::DmrLcn(6) => Some(451_087_500),
                        _ => None,
                    };
                    assert_eq!(g.channel.freq_hz, expected);
                }
                ControlEvent::Message(_) => lines += 1,
                _ => {}
            }
        }
        assert!(control.carrier_offset_hz().is_some());
        let s = &control.stats;
        assert!(s.msgs_valid * 10 > (s.msgs_valid + s.msgs_invalid) * 9, "{s:?}");
    }
    eprintln!("{} files: {lines} messages, {grants} grants", files.len());
    assert!(lines > 1000 && grants > 0);
}

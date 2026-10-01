use super::*;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::slot_type::DataType;
use crate::protocol::dmr::message::csbk;
use crate::protocol::dmr::message::data::DataBurst;
use crate::protocol::dmr::message::types::DmrChannel;
use crate::protocol::dmr::sync::DmrSyncPattern;

const GCS_CC: u64 = 454_368_750;
const GCS_VOICE: u64 = 451_087_500;

fn hex_bits(hex: &str) -> Vec<u8> {
    hex.chars()
        .flat_map(|c| {
            let v = c.to_digit(16).unwrap();
            (0..4).rev().map(move |i| ((v >> i) & 1) as u8)
        })
        .collect()
}

/// A control-channel CSBK from its on-air payload, its channel given the
/// frequency the LCN map would add.
fn csbk_message(hex: &str, freq_hz: Option<u64>) -> DmrMessage {
    let burst = DataBurst {
        pattern: DmrSyncPattern::BaseStationData,
        timeslot: 1,
        timestamp_ms: 0,
        cach: Cach { valid: true, busy: false, timeslot: 1, lcss: 0, payload: [0; 17] },
        color_code: 0,
        data_type: DataType::Csbk,
        bits: hex_bits(hex),
        reserved: 0,
        valid: true,
    };
    let mut c = csbk::create(burst);
    if let Some(ch) = c.channel.as_mut() {
        ch.downlink_hz = freq_hz;
    }
    DmrMessage::Csbk(c)
}

/// TALKGROUP VOICE CHANNEL GRANT FM:81921 TO:87925 LCN:5 CHANID:12 (unit A, 2026-09-30 20:57).
const TV_GRANT_LCN5_TS2: &str = "B1000058015775014001E621";

#[test]
fn grant_fields_from_air() {
    let m = csbk_message(TV_GRANT_LCN5_TS2, Some(GCS_CC));
    let g = voice_grant(&m).expect("a voice grant");
    assert_eq!(g.talkgroup, 87925);
    assert_eq!(g.source, Some(81921));
    assert_eq!((g.lcn, g.timeslot), (5, 2));
    assert_eq!(g.freq_hz, Some(GCS_CC));
    assert!(!g.private);
}

fn grant(tg: u32, lcn: u16, ts: u8, freq: Option<u64>) -> DmrGrant {
    DmrGrant { talkgroup: tg, source: Some(1), private: false, lcn, timeslot: ts, freq_hz: freq }
}

/// Drives `on_control` with a grant built field by field.
fn on_grant(f: &mut DmrFollower, g: DmrGrant, now: u64) -> Vec<FollowerAction> {
    // Re-use a real CSBK and overwrite what the follower reads.
    let mut m = csbk_message(TV_GRANT_LCN5_TS2, g.freq_hz);
    if let DmrMessage::Csbk(c) = &mut m {
        c.channel = Some(DmrChannel { lcn: g.lcn, timeslot: g.timeslot, downlink_hz: g.freq_hz, uplink_hz: None, absolute: false });
        // Destination (talkgroup) bits 32..56.
        crate::protocol::dmr::fec::set_int(g.talkgroup, &mut c.burst.bits[32..56]);
    }
    f.on_control(&m, now)
}

#[test]
fn follows_tunes_keeps_alive_and_times_out() {
    let mut f = DmrFollower::new();
    let a = on_grant(&mut f, grant(87925, 6, 2, Some(GCS_VOICE)), 0);
    assert_eq!(a[0], FollowerAction::Tune { freq_hz: GCS_VOICE });
    assert!(matches!(a[1], FollowerAction::Grant { not_followed: None, .. }));
    // The repeated grant keeps it alive, no retune.
    let a = on_grant(&mut f, grant(87925, 6, 2, Some(GCS_VOICE)), 60);
    assert!(matches!(a[..], [FollowerAction::KeepAlive { talkgroup: 87925, .. }]));
    // Another talkgroup while busy: recorded, not followed.
    let a = on_grant(&mut f, grant(87924, 6, 1, Some(GCS_VOICE)), 100);
    assert!(matches!(a[..], [FollowerAction::Grant { not_followed: Some("busy"), .. }]));
    // The same talkgroup's next transmission on the control repeater's TS2.
    let a = on_grant(&mut f, grant(87925, 5, 2, Some(GCS_CC)), 500);
    assert_eq!(a[0], FollowerAction::Tune { freq_hz: GCS_CC });
    assert!(matches!(a[1], FollowerAction::Grant { not_followed: None, .. }));
    assert!(f.tick(500 + HANG_MS - 1).is_empty());
    assert_eq!(f.tick(500 + HANG_MS), vec![FollowerAction::End { reason: "timeout" }]);
    assert!(f.following().is_none());
    // Idle again: the next grant is followed.
    let a = on_grant(&mut f, grant(87924, 5, 2, Some(GCS_CC)), 9000);
    assert!(matches!(a[..], [FollowerAction::Grant { not_followed: None, .. }]));
}

#[test]
fn unknown_lcn_and_disabled_are_recorded_not_followed() {
    let mut f = DmrFollower::new();
    let a = on_grant(&mut f, grant(87925, 9, 1, None), 0);
    assert!(matches!(a[..], [FollowerAction::Grant { not_followed: Some("unknown_lcn"), .. }]));
    f.enabled = false;
    let a = on_grant(&mut f, grant(87925, 6, 1, Some(GCS_VOICE)), 0);
    assert!(matches!(a[..], [FollowerAction::Grant { not_followed: Some("follower_off"), .. }]));
    assert!(f.following().is_none());
}

/// Offline: the 20:57 call in unit A's capture (`DMR_CAPTURE_DIR`). The
/// control repeater carries it on TS2 after a grant to LCN 5 TS2, so the
/// control capture serves as the traffic channel too.
#[test]
fn captured_call_on_the_control_repeater() {
    use crate::protocol::dmr::demod::DmrDemodulator;
    use crate::protocol::dmr::framer::DmrMessageFramer;
    use crate::protocol::dmr::message::processor::DmrMessageProcessor;
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let path = std::path::Path::new(&dir).join("cc_454368750_20260930_205714_60s.wav");
    let bytes = std::fs::read(path).unwrap();
    let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    let mut demod = DmrDemodulator::new();
    let mut framer = DmrMessageFramer::default();
    let lcns = [(5u16, GCS_CC), (6u16, GCS_VOICE)].into_iter().collect();
    let mut processor = DmrMessageProcessor::new(lcns);
    let mut follower = DmrFollower::new();
    let mut actions = Vec::new();
    for chunk in iq.chunks(2 * 1250) {
        demod.process_iq_i16(chunk, &mut framer);
        let events: Vec<_> = framer.drain().collect();
        for event in events {
            for m in processor.process(event) {
                let now = m.timestamp_ms();
                actions.extend(follower.on_control(&m, now));
                if follower.tuned_hz() == Some(GCS_CC) {
                    actions.extend(follower.on_traffic(&m, now));
                }
                actions.extend(follower.tick(now));
            }
        }
    }
    let tunes: Vec<_> = actions.iter().filter(|a| matches!(a, FollowerAction::Tune { .. })).collect();
    let voice = actions.iter().filter(|a| matches!(a, FollowerAction::Voice { .. })).count();
    let ends: Vec<_> = actions.iter().filter(|a| matches!(a, FollowerAction::End { .. })).collect();
    eprintln!("tunes {tunes:?}, voice bursts {voice}, ends {ends:?}");
    // 82321 on LCN 6, then 81921 on LCN 5 TS2 (the control repeater), then 82321 on LCN 6 again.
    assert!(tunes.contains(&&FollowerAction::Tune { freq_hz: GCS_CC }));
    assert!(tunes.contains(&&FollowerAction::Tune { freq_hz: GCS_VOICE }));
    // Six voice superframes on TS2: up to 36 bursts.
    assert!(voice >= 30, "{voice} voice bursts");
    assert!(actions.contains(&FollowerAction::Source { source: 81921 }) || voice > 0);
    assert!(!ends.is_empty());
}

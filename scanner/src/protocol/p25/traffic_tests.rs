//! Host tests for `protocol::p25::traffic`.

use super::*;
use crate::protocol::p25::voice_frame::{HduHeader, ImbeFrameRaw};

const FRAMES: [ImbeFrameRaw; 9] = [ImbeFrameRaw { bits: [0; 18] }; 9];

fn following(tg: u32, source: Option<u32>) -> P25Traffic {
    let mut t = P25Traffic::new("lane 1");
    t.follow(CallContext { call: 7, tg, source, encrypted: false });
    t
}

fn kinds(out: &[TrafficEvent]) -> Vec<String> {
    out.iter()
        .filter_map(|e| match e {
            TrafficEvent::Message(_) => None,
            TrafficEvent::VoiceNid { header, .. } => Some(format!("voice_nid header={header}")),
            TrafficEvent::Voice { encrypted, .. } => Some(format!("voice enc={encrypted}")),
            TrafficEvent::Source(s) => Some(format!("source {s}")),
            TrafficEvent::TalkComplete(s) => Some(format!("talk_complete {s:?}")),
            TrafficEvent::End { lc, .. } => Some(format!("end {lc}")),
            TrafficEvent::Pdu(f) => Some(format!("pdu {}", f.chain)),
        })
        .collect()
}

fn ldu1(t: &mut P25Traffic, source: u32, at: Instant, out: &mut Vec<TrafficEvent>) {
    let lcw = TdulcLcw::GroupVoiceChannelUser { talkgroup: 300, source_radio_id: source, service_options: 0 };
    t.unit(Unit::Ldu1(FRAMES, Some(source), Some(lcw)), at, at, out);
}

fn tdulc(t: &mut P25Traffic, lcw: TdulcLcw, valid: bool, at: Instant, out: &mut Vec<TrafficEvent>) {
    t.unit(Unit::TduLc(Some((lcw, valid))), at, at, out);
}

#[test]
fn three_of_four_link_controls_name_the_talker_once() {
    let mut t = following(300, Some(1014));
    let at = Instant::now();
    let mut out = Vec::new();
    ldu1(&mut t, 3406021, at, &mut out);
    ldu1(&mut t, 3406021, at, &mut out);
    ldu1(&mut t, 9, at, &mut out); // a corrupted decode
    assert!(!kinds(&out).iter().any(|k| k.starts_with("source")));
    ldu1(&mut t, 3406021, at, &mut out);
    ldu1(&mut t, 3406021, at, &mut out);
    let sources: Vec<String> = kinds(&out).into_iter().filter(|k| k.starts_with("source")).collect();
    assert_eq!(sources, ["source 3406021"]);
    // A system controller address is never a talker.
    let mut t = following(300, None);
    for _ in 0..4 {
        ldu1(&mut t, 0xFF_FFFE, at, &mut out);
    }
    assert_eq!(kinds(&out).iter().filter(|k| k.starts_with("source")).count(), 1);
}

#[test]
fn an_hdu_marks_the_call_encrypted_and_an_ldu2_cannot() {
    let mut t = following(402, None);
    let at = Instant::now();
    let mut out = Vec::new();
    let hdu = |alg| HduHeader { talkgroup: 402, key_id: 1, algorithm_id: alg, message_indicator: [0; 9] };
    t.unit(Unit::Hdu(Some(hdu(0x84))), at, at, &mut out);
    ldu1(&mut t, 1, at, &mut out);
    assert_eq!(kinds(&out), ["voice enc=true"]);
    // An unknown algorithm byte (a corrupt decode) does not.
    let mut t = following(402, None);
    out.clear();
    t.unit(Unit::Hdu(Some(hdu(0x08))), at, at, &mut out);
    ldu1(&mut t, 1, at, &mut out);
    assert_eq!(kinds(&out), ["voice enc=false"]);
}

#[test]
fn the_first_valid_terminator_after_voice_ends_the_transmission() {
    let mut t = following(300, Some(1014));
    let at = Instant::now();
    let mut out = Vec::new();
    // No voice yet: no end.
    tdulc(&mut t, TdulcLcw::GroupVoiceChannelUser { talkgroup: 300, source_radio_id: 0, service_options: 0 }, true, at, &mut out);
    assert!(kinds(&out).is_empty());
    ldu1(&mut t, 1014, at, &mut out);
    // An LC that fails its RS check does not end it; the first valid one does, once.
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 1014 }, false, at, &mut out);
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 1014 }, true, at, &mut out);
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 1014 }, true, at + Duration::from_secs(2), &mut out);
    let k = kinds(&out);
    assert_eq!(k.iter().filter(|k| k.starts_with("end")).cloned().collect::<Vec<_>>(), ["end talk_complete"]);
    // Talk complete: once per 1.5 s, the grant's radio.
    assert_eq!(k.iter().filter(|k| k.starts_with("talk_complete")).count(), 2);
    // Voice again: the next valid terminator ends that transmission.
    ldu1(&mut t, 1014, at, &mut out);
    tdulc(&mut t, TdulcLcw::CallTermination { by_radio_id: 0xFF_FFFE }, true, at + Duration::from_secs(5), &mut out);
    let k = kinds(&out);
    assert_eq!(k.iter().filter(|k| k.starts_with("end")).last().unwrap(), "end network_teardown");
    assert_eq!(k.last().unwrap(), "talk_complete Some(1014)");
}

#[test]
fn a_talk_complete_naming_another_radio_is_not_believed() {
    let mut t = following(300, Some(1014));
    let at = Instant::now();
    let mut out = Vec::new();
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 3406021 }, true, at, &mut out);
    assert!(!kinds(&out).iter().any(|k| k.starts_with("talk_complete")));
    // With no radio from the grant, a plausible one is.
    let mut t = following(300, None);
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 3406021 }, true, at, &mut out);
    assert_eq!(kinds(&out).last().unwrap(), "talk_complete Some(3406021)");
    // No call followed: terminators are ignored.
    let mut t = P25Traffic::new("lane 1");
    out.clear();
    tdulc(&mut t, TdulcLcw::MotorolaTalkComplete { by_radio_id: 3406021 }, true, at, &mut out);
    assert!(out.is_empty());
}

/// Offline: SDRTrunk's traffic recordings (`P25_SDRTRUNK_DIR`, `T-LCN` `.bits` files) decode to
/// voice, ends and talk completes.
#[test]
fn sdrtrunk_traffic_recordings() {
    let Ok(dir) = std::env::var("P25_SDRTRUNK_DIR") else { return };
    let (mut voices, mut ends, mut talk, mut files) = (0, 0, 0, 0);
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let path = e.path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.ends_with(".bits") || !name.contains("T-LCN-") {
            continue;
        }
        files += 1;
        let mut t = following(300, None);
        let at = Instant::now();
        let mut out = Vec::new();
        for byte in std::fs::read(&path).unwrap() {
            for shift in [6, 4, 2, 0] {
                t.push(byte >> shift, at, at + Duration::from_secs(files), &mut out);
            }
        }
        let s = &t.framer.stats;
        let v = out.iter().filter(|e| matches!(e, TrafficEvent::Voice { .. })).count() as u64;
        assert_eq!(v, s.ldu1s + s.ldu2s, "{name}");
        voices += v;
        ends += out.iter().filter(|e| matches!(e, TrafficEvent::End { .. })).count();
        talk += out.iter().filter(|e| matches!(e, TrafficEvent::TalkComplete(_))).count();
    }
    eprintln!("{files} files: {voices} LDUs, {ends} ends, {talk} talk completes");
    assert!(voices > 1000 && ends > 50 && talk > 50);
}

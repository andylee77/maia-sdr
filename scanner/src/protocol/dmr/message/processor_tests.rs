//! Unit tests for `processor.rs`, and the capture comparison with SDRTrunk.

use super::*;
use crate::protocol::dmr::demod::DmrDemodulator;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::emb::{EMB_INDEXES, VALID_WORDS};
use crate::protocol::dmr::fec::hamming::{Hamming16, Hamming17};
use crate::protocol::dmr::fec::set_int;
use crate::protocol::dmr::fec::slot_type::DataType;
use crate::protocol::dmr::framer::{DmrBurst, DmrMessageFramer};
use crate::protocol::dmr::message::bits::field;

fn hex_bits(hex: &str) -> Vec<u8> {
    hex.chars()
        .flat_map(|c| {
            let v = c.to_digit(16).unwrap();
            (0..4).rev().map(move |i| ((v >> i) & 1) as u8)
        })
        .collect()
}

/// Interleaved BPTC(68,36) block (4 CACH payloads) for 36 short LC bits.
fn slc_block(info: &[u8]) -> Vec<u8> {
    let mut d = [0u8; 68];
    for row in 0..3 {
        d[row * 17..row * 17 + 12].copy_from_slice(&info[row * 12..row * 12 + 12]);
        let p = Hamming17::calculate_checksum(&d, row * 17);
        set_int(p, &mut d[row * 17 + 12..row * 17 + 17]);
    }
    for column in 0..17 {
        d[51 + column] = d[column] ^ d[17 + column] ^ d[34 + column];
    }
    let mut tx = vec![0u8; 68];
    for index in 0..67 {
        tx[index] = d[index * 17 % 67];
    }
    tx[67] = d[67];
    tx
}

/// Interleaved BPTC(128,77) block (4 voice burst fragments) for 77 embedded LC bits.
fn flc_block(info: &[u8]) -> Vec<u8> {
    let mut d = [0u8; 128];
    let mut pointer = 0;
    for row in 0..7 {
        let width = if row < 2 { 11 } else { 10 };
        d[row * 16..row * 16 + width].copy_from_slice(&info[pointer..pointer + width]);
        pointer += width;
        if row >= 2 {
            d[row * 16 + 10] = info[70 + row];
        }
        let p = Hamming16::calculate_checksum(&d, row * 16);
        set_int(p, &mut d[row * 16 + 11..row * 16 + 16]);
    }
    for column in 0..16 {
        d[112 + column] = (0..7).fold(0, |acc, row| acc ^ d[row * 16 + column]);
    }
    let mut tx = vec![0u8; 128];
    for i in 0..127 {
        tx[(i * 8) % 127] = d[i];
    }
    tx[127] = d[127];
    tx
}

/// A CSBK payload: opcode, fields (start, end, value), CRC with `mask`.
fn csbk_bits(opcode: u32, fields: &[(usize, usize, u32)], mask: u16) -> Vec<u8> {
    let mut bits = vec![0u8; 96];
    set_int(opcode, &mut bits[2..8]);
    for &(start, end, value) in fields {
        set_int(value, &mut bits[start..end]);
    }
    let crc = !crc::crc_ccitt(&bits[..80]) ^ mask;
    set_int(u32::from(crc), &mut bits[80..96]);
    bits
}

fn cach(lcss: u8, payload: &[u8]) -> Cach {
    let mut p = [0u8; 17];
    p.copy_from_slice(payload);
    Cach {
        valid: true,
        busy: false,
        timeslot: 1,
        lcss,
        payload: p,
    }
}

fn data_burst(bits: Vec<u8>, data_type: DataType) -> DataBurst {
    DataBurst {
        pattern: DmrSyncPattern::BaseStationData,
        timeslot: 1,
        timestamp_ms: 0,
        cach: cach(SINGLE_FRAGMENT, &[0; 17]),
        color_code: 0,
        data_type,
        bits,
        reserved: 0,
        valid: true,
    }
}

fn voice_event(pattern: DmrSyncPattern, bits: [u8; 288], cach: Cach, timeslot: u8) -> FramerEvent {
    FramerEvent::Burst(DmrBurst {
        pattern,
        timeslot,
        bits,
        cach,
        dibit_index: 0,
    })
}

fn texts(messages: &[DmrMessage]) -> Vec<String> {
    messages
        .iter()
        .map(|m| format!("{}|{}", m.class_name(), m))
        .collect()
}

#[test]
fn short_lc_assembles_from_four_cach_fragments() {
    let block = slc_block(&hex_bits("2400AE288"));
    let mut processor = DmrMessageProcessor::new(HashMap::new());
    let mut out = Vec::new();
    let lcss = [
        FIRST_FRAGMENT,
        CONTINUATION_FRAGMENT,
        CONTINUATION_FRAGMENT,
        LAST_FRAGMENT,
    ];
    for (i, &l) in lcss.iter().enumerate() {
        let c = cach(l, &block[i * 17..i * 17 + 17]);
        out.extend(processor.process(voice_event(
            DmrSyncPattern::BaseStationVoice,
            [0; 288],
            c,
            1,
        )));
    }
    let texts = texts(&out);
    assert_eq!(texts.len(), 5, "{texts:?}");
    assert_eq!(texts[3], "VoiceAMessage|CC:- BS VOICE A");
    assert_eq!(
        texts[4],
        "ControlChannelSystemParameters|SLC TIER III CONTROL CHANNEL SMALL NET:0 SITE:2 REGISTRATION REQUIRED \
         SLOT COUNTER:226 MSG:2400AE288"
    );
    assert!(out[4].is_valid());
    assert_eq!(out[4].timeslot(), 0);
}

#[test]
fn sync_loss_drops_a_partial_short_lc() {
    let block = slc_block(&hex_bits("2400AE288"));
    let mut processor = DmrMessageProcessor::new(HashMap::new());
    let mut out = Vec::new();
    for (i, &l) in [FIRST_FRAGMENT, CONTINUATION_FRAGMENT].iter().enumerate() {
        let c = cach(l, &block[i * 17..i * 17 + 17]);
        out.extend(processor.process(voice_event(
            DmrSyncPattern::BaseStationVoice,
            [0; 288],
            c,
            1,
        )));
    }
    out.extend(processor.process(FramerEvent::SyncLoss {
        timeslot: 0,
        bits: 288,
    }));
    for (i, &l) in [CONTINUATION_FRAGMENT, LAST_FRAGMENT].iter().enumerate() {
        let c = cach(l, &block[34 + i * 17..51 + i * 17]);
        out.extend(processor.process(voice_event(
            DmrSyncPattern::BaseStationVoice,
            [0; 288],
            c,
            1,
        )));
    }
    assert!(
        out.iter().all(|m| !matches!(m, DmrMessage::ShortLc(_))),
        "{:?}",
        texts(&out)
    );
    assert_eq!(out[2].to_string(), "<-> SYNC LOSS - BITS PROCESSED [288]");
}

#[test]
fn embedded_lc_assembles_from_voice_bursts_b_to_e() {
    // Group voice channel user, TG 87925 from 81921, priority 2, + checksum 5.
    let mut lc = vec![0u8; 77];
    set_int(0x02, &mut lc[16..24]);
    set_int(87925, &mut lc[24..48]);
    set_int(81921, &mut lc[48..72]);
    let sum: u32 = (0..9).map(|i| field(&lc, i * 8, i * 8 + 8)).sum();
    set_int(sum % 31, &mut lc[72..77]);
    let block = flc_block(&lc);

    let mut processor = DmrMessageProcessor::new(HashMap::new());
    let patterns = [
        DmrSyncPattern::BsVoiceFrameB,
        DmrSyncPattern::BsVoiceFrameC,
        DmrSyncPattern::BsVoiceFrameD,
        DmrSyncPattern::BsVoiceFrameE,
        DmrSyncPattern::BsVoiceFrameF,
    ];
    let lcss = [
        FIRST_FRAGMENT,
        CONTINUATION_FRAGMENT,
        CONTINUATION_FRAGMENT,
        LAST_FRAGMENT,
        SINGLE_FRAGMENT,
    ];
    let mut out = Vec::new();
    for i in 0..5 {
        let mut bits = [0u8; 288];
        let mut word = [0u8; 16];
        set_int(u32::from(VALID_WORDS[usize::from(lcss[i])]), &mut word);
        for (x, &j) in EMB_INDEXES.iter().enumerate() {
            bits[j] = word[x];
        }
        if i < 4 {
            bits[140..172].copy_from_slice(&block[i * 32..i * 32 + 32]);
        }
        out.extend(processor.process(voice_event(
            patterns[i],
            bits,
            cach(SINGLE_FRAGMENT, &[0; 17]),
            2,
        )));
    }
    let texts = texts(&out);
    let flc: Vec<_> = texts
        .iter()
        .filter(|t| t.starts_with("GroupVoiceChannelUser"))
        .collect();
    assert_eq!(flc, ["GroupVoiceChannelUser|FLC GROUP VOICE CHANNEL USER FM:81921 TO:87925 SERVICE OPTIONS [PRIORITY-2]"]);
    // The LC follows burst E; burst F carries a null short burst.
    let e = texts
        .iter()
        .position(|t| t.ends_with("BS VOICE E"))
        .unwrap();
    assert!(
        texts[e + 1].starts_with("GroupVoiceChannelUser"),
        "{texts:?}"
    );
    assert!(
        texts.contains(&"VoiceEMBMessage|CC:0 BS VOICE F NULL SHORT BURST".to_string()),
        "{texts:?}"
    );
}

#[test]
fn grants_take_the_lcn_map_and_multi_block_csbks_assemble() {
    let mut processor = DmrMessageProcessor::new(HashMap::from([(6, 451_087_500)]));

    // Talkgroup voice grant on LCN 6 slot 2: enriched from the map.
    let bits = csbk_bits(
        49,
        &[(16, 28, 6), (28, 29, 1), (32, 56, 87925), (56, 80, 81921)],
        crc::CSBK_CRC_MASK,
    );
    let mut out = Vec::new();
    processor.receive(
        DmrMessage::Csbk(csbk::create(data_burst(bits, DataType::Csbk))),
        &mut out,
    );
    let DmrMessage::Csbk(grant) = &out[0] else {
        panic!("{out:?}")
    };
    assert_eq!(grant.channel.unwrap().downlink_hz, Some(451_087_500));
    assert_eq!(grant.channel.unwrap().channel_id(), 14);

    // MBC header + last block: a grant with absolute channel parameters.
    let header_bits = csbk_bits(
        49,
        &[(32, 56, 87925), (56, 80, 81921)],
        crc::MBC_HEADER_CRC_MASK,
    );
    let header = Csbk {
        burst: data_burst(header_bits, DataType::MbcHeader),
        kind: CsbkKind::MbcHeader,
        blocks: Vec::new(),
        absolute: None,
        channel: None,
    };
    let block = csbk_bits(
        0,
        &[(0, 1, 1), (22, 34, 77), (57, 67, 452), (67, 80, 100)],
        crc::MBC_LAST_BLOCK_CRC_MASK,
    );
    let mut out = Vec::new();
    processor.receive(DmrMessage::Csbk(header), &mut out);
    processor.receive(
        DmrMessage::MbcContinuation(data_burst(block, DataType::MbcBlock)),
        &mut out,
    );
    // Each burst's CACH (LCSS 0 here) also yields a single-fragment SLC, as in SDRTrunk.
    out.retain(|m| !matches!(m, DmrMessage::ShortLc(_)));
    let texts = texts(&out);
    assert_eq!(texts.len(), 3, "{texts:?}");
    assert!(texts[0]
        .starts_with("MBCHeader|CC:0 MULTI-BLOCK CSBK HEADER TALKGROUP VOICE CHANNEL GRANT MSG:"));
    assert!(
        texts[1].starts_with("MBCContinuationBlock|CC:0 MULTI-BLOCK CSBK CONTINUATION-FINAL MSG:")
    );
    assert!(
        texts[2].starts_with("TalkgroupVoiceChannelGrant|CC:0 TALKGROUP VOICE CHANNEL GRANT FM:81921 TO:87925 77 452.0125"),
        "{}",
        texts[2]
    );
    let DmrMessage::Csbk(multi) = &out[2] else {
        panic!()
    };
    assert!(multi.burst.valid);
    assert_eq!(multi.channel.unwrap().downlink_hz, Some(452_012_500));
}

#[test]
fn alternate_csbk_mask_is_accepted_once_seen_twice() {
    let mut processor = DmrMessageProcessor::new(HashMap::new());
    let bits = csbk_bits(25, &[(40, 56, 0b01_0000000_00010_00)], 0x1234);
    let mut valid = Vec::new();
    for _ in 0..3 {
        let mut out = Vec::new();
        processor.receive(
            DmrMessage::Csbk(csbk::create(data_burst(bits.clone(), DataType::Csbk))),
            &mut out,
        );
        valid.push(out[0].is_valid());
    }
    assert_eq!(valid, [false, true, true]);
}

/// `timeslot|valid|class|text`, the reference format minus file and timestamp.
fn line(message: &DmrMessage) -> String {
    format!(
        "{}|{}|{}|{}",
        message.timeslot(),
        message.is_valid(),
        message.class_name(),
        message
    )
}

/// Runs demodulator, framer and processor over a 50 kSPS stereo i16 WAV.
fn decode_wav(path: &std::path::Path) -> Vec<String> {
    let bytes = std::fs::read(path).unwrap();
    let iq: Vec<i16> = bytes[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect();
    let mut demod = DmrDemodulator::new();
    let mut framer = DmrMessageFramer::default();
    let mut processor =
        DmrMessageProcessor::new(HashMap::from([(5, 454_368_750), (6, 451_087_500)]));
    let mut lines = Vec::new();
    for chunk in iq.chunks(2 * 1250) {
        demod.process_iq_i16(chunk, &mut framer);
        let events: Vec<_> = framer.drain().collect();
        for event in events {
            lines.extend(processor.process(event).iter().map(line));
        }
    }
    lines
}

/// One step of an alignment of `a` (reference) with `b` (ours).
#[derive(Debug, PartialEq)]
enum Step {
    Same,
    OnlyA(usize),
    OnlyB(usize),
}

/// Longest-common-subsequence alignment.
fn align(a: &[String], b: &[String]) -> Vec<Step> {
    let (n, m) = (a.len(), b.len());
    let mut table = vec![0u16; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[at(i, j)] = if a[i] == b[j] {
                table[at(i + 1, j + 1)] + 1
            } else {
                table[at(i + 1, j)].max(table[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut steps = Vec::new();
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            steps.push(Step::Same);
            i += 1;
            j += 1;
        } else if j < m && (i == n || table[at(i, j + 1)] >= table[at(i + 1, j)]) {
            steps.push(Step::OnlyB(j));
            j += 1;
        } else {
            steps.push(Step::OnlyA(i));
            i += 1;
        }
    }
    steps
}

#[test]
fn align_finds_the_common_lines() {
    let a: Vec<String> = ["x", "a", "b", "c"].iter().map(|s| s.to_string()).collect();
    let b: Vec<String> = ["a", "b", "y", "c"].iter().map(|s| s.to_string()).collect();
    let steps = align(&a, &b);
    assert_eq!(steps.iter().filter(|s| **s == Step::Same).count(), 3);
    assert_eq!(steps[0], Step::OnlyA(0));
}

/// Offline: decodes unit A's control captures and diffs the messages against
/// SDRTrunk's on the same files (`DMR_CAPTURE_DIR` holding `cc_*.wav` and
/// `sdrtrunk_ref_cc.txt`, `file|timestamp|timeslot|valid|class|text`).
#[test]
fn captured_reference() {
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let reference = std::fs::read_to_string(dir.join("sdrtrunk_ref_cc.txt")).unwrap();
    let mut by_file: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for l in reference.lines() {
        let fields: Vec<&str> = l.splitn(6, '|').collect();
        if fields.len() == 6 {
            by_file
                .entry(fields[0].to_string())
                .or_default()
                .push(fields[2..].join("|"));
        }
    }

    let (mut total_ref, mut total_ours, mut total_same) = (0, 0, 0);
    let mut differences: Vec<String> = Vec::new();
    let mut unmatched: std::collections::BTreeMap<String, (u32, u32)> = Default::default();
    for (file, expected) in &by_file {
        let ours = decode_wav(&dir.join(file));
        let steps = align(expected, &ours);
        let same = steps.iter().filter(|s| **s == Step::Same).count();
        eprintln!(
            "{file}: SDRTrunk {} ours {} matched {same}",
            expected.len(),
            ours.len()
        );
        total_ref += expected.len();
        total_ours += ours.len();
        total_same += same;
        for step in steps {
            let (side, text) = match step {
                Step::Same => continue,
                Step::OnlyA(i) => ("-", &expected[i]),
                Step::OnlyB(j) => ("+", &ours[j]),
            };
            let class = text.split('|').nth(2).unwrap_or("").to_string();
            let counts = unmatched.entry(class).or_default();
            if side == "-" {
                counts.0 += 1;
            } else {
                counts.1 += 1;
            }
            differences.push(format!("{file} {side} {text}"));
        }
    }
    eprintln!("TOTAL: SDRTrunk {total_ref} ours {total_ours} matched {total_same}");
    eprintln!("Unmatched by class (SDRTrunk only / ours only): {unmatched:?}");
    eprintln!("First differences (- SDRTrunk only, + ours only):");
    for d in differences.iter().take(40) {
        eprintln!("  {d}");
    }
}

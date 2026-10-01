//! Unit tests for `lc.rs`.

use super::*;
use crate::protocol::dmr::fec::set_int;

fn hex_bits(hex: &str, len: usize) -> Vec<u8> {
    let mut bits: Vec<u8> = hex
        .chars()
        .flat_map(|c| {
            let v = c.to_digit(16).unwrap();
            (0..4).rev().map(move |i| ((v >> i) & 1) as u8)
        })
        .collect();
    bits.truncate(len);
    bits
}

/// 72 LC bits: opcode, service options, talkgroup / target, source.
fn lc72(opcode: u32, options: u32, target: u32, source: u32) -> Vec<u8> {
    let mut bits = vec![0u8; 72];
    set_int(opcode, &mut bits[2..8]);
    set_int(options, &mut bits[16..24]);
    set_int(target, &mut bits[24..48]);
    set_int(source, &mut bits[48..72]);
    bits
}

/// 77-bit embedded LC: 72 bits + checksum 5.
fn embedded(lc: &[u8]) -> Vec<u8> {
    let mut bits = lc.to_vec();
    let sum: u32 = (0..9).map(|i| field(lc, i * 8, i * 8 + 8)).sum();
    bits.extend_from_slice(&[0; 5]);
    set_int(sum % 31, &mut bits[72..77]);
    bits
}

/// 96-bit header / terminator LC: 72 bits + RS(12,9) parity XOR mask.
fn with_rs(lc: &[u8], mask: u8) -> Vec<u8> {
    let bytes: Vec<u8> = (0..9).map(|i| field(lc, i * 8, i * 8 + 8) as u8).collect();
    let parity = rs_12_9::calculate_checksum(&bytes);
    let mut bits = lc.to_vec();
    for p in parity {
        let mut byte = [0u8; 8];
        set_int(u32::from(p ^ mask), &mut byte);
        bits.extend_from_slice(&byte);
    }
    bits
}

#[test]
fn on_air_short_lc() {
    // Clay Electric SLCs as SDRTrunk printed them.
    let slc = create_short(hex_bits("2400AE288", 36), 0);
    assert!(slc.valid);
    assert_eq!(slc.class_name(), "ControlChannelSystemParameters");
    assert_eq!(
        slc.to_string(),
        "SLC TIER III CONTROL CHANNEL SMALL NET:0 SITE:2 REGISTRATION REQUIRED SLOT COUNTER:226 MSG:2400AE288"
    );
    let slc = create_short(hex_bits("34009681E", 36), 0);
    assert_eq!(slc.class_name(), "TrafficChannelSystemParameters");
    assert_eq!(
        slc.to_string(),
        "SLC TIER III TRAFFIC CHANNEL SMALL NET:0 SITE:2 SLOT COUNTER:360 MSG:34009681E"
    );
    let mut bad = hex_bits("2400AE288", 36);
    bad[30] ^= 1;
    assert!(create_short(bad, 0)
        .to_string()
        .starts_with("[CRC ERROR] SLC TIER III CONTROL CHANNEL"));
}

#[test]
fn single_fragment_null_and_activity() {
    let null = create_short(vec![0; 17], 0);
    assert!(null.valid);
    assert_eq!(
        (null.class_name(), null.to_string().as_str()),
        ("NullMessage", "SLC TS1:IDLE TS2:IDLE")
    );
    // Activity update: TS1 group voice, hash 0xA5; TS2 idle.
    let mut bits = vec![0u8; 36];
    set_int(1, &mut bits[0..4]);
    set_int(8, &mut bits[4..8]);
    set_int(0xA5, &mut bits[12..20]);
    let crc8 = crc::crc8(&bits[..28]);
    set_int(u32::from(crc8), &mut bits[28..36]);
    let slc = create_short(bits.clone(), 0);
    assert!(slc.valid);
    assert_eq!(
        slc.to_string(),
        format!("SLC TS1:GROUP VOICE [A5] TS2:IDLE MSG:{}", hex(&bits))
    );
}

#[test]
fn embedded_group_voice_lc() {
    let mut masks = DmrCrcMaskManager::default();
    let bits = embedded(&lc72(0, 0x02, 87925, 81921));
    let lc = create_full(bits.clone(), 0, 2, false, 0, &mut masks);
    assert!(lc.valid);
    assert_eq!(lc.class_name(), "GroupVoiceChannelUser");
    assert_eq!(
        lc.to_string(),
        "FLC GROUP VOICE CHANNEL USER FM:81921 TO:87925 SERVICE OPTIONS [PRIORITY-2]"
    );
    assert_eq!(
        lc.opcode(),
        LcOpcode::FULL_STANDARD_GROUP_VOICE_CHANNEL_USER
    );
    let options = lc.service_options().unwrap();
    assert!(!options.is_emergency() && !options.is_encrypted() && !options.is_broadcast());
    assert_eq!(options.priority(), 2);
    let broadcast = create_full(
        embedded(&lc72(0, 0x08, 87925, 82321)),
        0,
        2,
        false,
        0,
        &mut masks,
    );
    assert_eq!(
        broadcast.to_string(),
        "FLC GROUP VOICE CHANNEL USER FM:82321 TO:87925 SERVICE OPTIONS [BROADCAST]"
    );
}

#[test]
fn checksum_failures_and_alternate_masks() {
    let mut masks = DmrCrcMaskManager::default();
    let mut bits = embedded(&lc72(0, 0, 87925, 81921));
    // A different 5-bit "mask" on the checksum: bad once, accepted when it repeats.
    bits[76] ^= 1;
    let first = create_full(bits.clone(), 0, 2, false, 0, &mut masks);
    assert!(!first.valid);
    assert!(first
        .to_string()
        .starts_with("[CRC-ERROR] FLC GROUP VOICE CHANNEL USER"));
    assert!(create_full(bits, 30, 2, false, 0, &mut masks).valid);
}

#[test]
fn on_air_tait_lc_has_a_good_checksum() {
    // SDRTrunk logged this embedded LC as "[CRC-ERROR] FLC UNKNOWN OPCODE:8
    // VENDOR:TAIT MSG:085853532D3233323998": its residual is 0, which
    // LCMessageFactory.java:134 calls invalid.
    let bits = hex_bits("085853532D3233323998", 77);
    assert_eq!(crc::checksum_5(&bits), 0);
    let lc = create_full(bits, 0, 1, false, 0, &mut DmrCrcMaskManager::default());
    assert!(lc.valid);
    assert_eq!(lc.class_name(), "UnknownFullLCMessage");
    assert_eq!(
        lc.to_string(),
        "FLC UNKNOWN OPCODE:8 VENDOR:TAIT MSG:085853532D3233323998"
    );
}

#[test]
fn header_and_terminator_lc_use_their_rs_masks() {
    let lc = lc72(3, 0x80, 82321, 81921);
    let mut masks = DmrCrcMaskManager::default();
    let header = create_full(with_rs(&lc, 0x96), 0, 1, false, 0, &mut masks);
    assert!(header.valid);
    assert_eq!(
        header.to_string(),
        "FLC UNIT TO UNIT VOICE CHANNEL USER FM:81921 TO:82321 SERVICE OPTIONS [EMERGENCY]"
    );
    let terminator = create_full(with_rs(&lc, 0x99), 0, 1, true, 0, &mut masks);
    assert!(terminator.valid);
    // The voice header mask on a terminator fails (until seen twice).
    assert!(!create_full(with_rs(&lc, 0x96), 0, 1, true, 0, &mut masks).valid);
    // One bad byte is corrected.
    let mut bits = with_rs(&lc, 0x96);
    for b in &mut bits[24..32] {
        *b ^= 1;
    }
    let fixed = create_full(bits, 0, 1, false, 0, &mut masks);
    assert!(fixed.valid);
    assert_eq!(fixed.corrected, 8);
    assert_eq!(fixed.destination(), Some(Address::Radio(82321)));
}

#[test]
fn terminator_data_and_unknown_lc() {
    let mut bits = vec![0u8; 72];
    set_int(48, &mut bits[2..8]);
    set_int(87925, &mut bits[16..40]);
    set_int(81921, &mut bits[40..64]);
    set_int(1, &mut bits[64..65]);
    set_int(1, &mut bits[66..67]);
    set_int(5, &mut bits[69..72]);
    let lc = create_full(
        with_rs(&bits, 0x99),
        0,
        1,
        true,
        0,
        &mut DmrCrcMaskManager::default(),
    );
    assert_eq!(lc.class_name(), "TerminatorData");
    assert_eq!(lc.to_string(), "FM:81921 TO:87925 COMPLETE SEQUENCE:5");

    let lc = create_full(
        embedded(&lc72(4, 0, 0, 0)),
        0,
        1,
        false,
        0,
        &mut DmrCrcMaskManager::default(),
    );
    assert_eq!(lc.class_name(), "UnknownFullLCMessage");
    assert!(lc
        .to_string()
        .starts_with("FLC UNKNOWN TALKER ALIAS HEADER MSG:"));
}

#[test]
fn pi_header_encryption_parameters() {
    let mut bits = vec![0u8; 96];
    set_int(0x21, &mut bits[2..8]);
    set_int(16, &mut bits[8..16]);
    set_int(7, &mut bits[16..24]);
    set_int(0x01020304, &mut bits[24..56]);
    set_int(87925, &mut bits[56..80]);
    let crc = !crc::crc_ccitt(&bits[..80]) ^ crc::PI_HEADER_CRC_MASK;
    set_int(u32::from(crc), &mut bits[80..96]);
    let lc = create_full_encryption(bits.clone(), 0, 1);
    assert!(lc.valid);
    assert_eq!(
        lc.to_string(),
        format!(
            "FLC ENCRYPTION PARAMETERS VENDOR:MOTOROLA CAP+ ALGORITHM:DMRA RC4/EP KEY:7 IV:01020304 TALKGROUP:87925 MSG:{}",
            hex(&bits)
        )
    );
}

//! Unit tests for `csbk.rs`: on-air CSBKs from Clay Electric (SDRTrunk's text
//! for the same payloads) and CSBKs built field by field.

use super::*;
use crate::protocol::dmr::fec::cach::Cach;
use crate::protocol::dmr::fec::set_int;
use crate::protocol::dmr::fec::slot_type::DataType;
use crate::protocol::dmr::sync::DmrSyncPattern;

fn hex_bits(hex: &str) -> Vec<u8> {
    hex.chars()
        .flat_map(|c| {
            let v = c.to_digit(16).unwrap();
            (0..4).rev().map(move |i| ((v >> i) & 1) as u8)
        })
        .collect()
}

fn burst(bits: Vec<u8>, data_type: DataType) -> DataBurst {
    DataBurst {
        pattern: DmrSyncPattern::BaseStationData,
        timeslot: 1,
        timestamp_ms: 0,
        cach: Cach {
            valid: true,
            busy: false,
            timeslot: 1,
            lcss: 0,
            payload: [0; 17],
        },
        color_code: 0,
        data_type,
        bits,
        reserved: 0,
        valid: true,
    }
}

fn from_hex(hex: &str) -> Csbk {
    create(burst(hex_bits(hex), DataType::Csbk))
}

/// A CSBK payload: opcode, fields (start, end, value), CRC with `mask`.
fn build(opcode: u32, fields: &[(usize, usize, u32)], mask: u16) -> Vec<u8> {
    let mut bits = vec![0u8; 96];
    set_int(opcode, &mut bits[2..8]);
    for &(start, end, value) in fields {
        set_int(value, &mut bits[start..end]);
    }
    let crc = !crc::crc_ccitt(&bits[..80]) ^ mask;
    set_int(u32::from(crc), &mut bits[80..96]);
    bits
}

fn text_of(opcode: u32, fields: &[(usize, usize, u32)]) -> (String, &'static str) {
    let csbk = create(burst(
        build(opcode, fields, crc::CSBK_CRC_MASK),
        DataType::Csbk,
    ));
    assert!(csbk.burst.valid);
    (csbk.to_string(), csbk.class_name())
}

#[test]
fn on_air_aloha() {
    let c = from_hex("99001101F6400B000000DEDA");
    assert_eq!(c.class_name(), "Aloha");
    assert!(c.burst.valid);
    assert_eq!(
        c.to_string(),
        "CC:0 ALOHA SMALL NETWORK:0 SITE:2 NET-CONNECTED SERVICES:ALL ETSI VER:RESERVED 4 MASK:0 99001101F6400B000000DEDA"
    );
    let sic = c.system_identity_code();
    assert_eq!((sic.model, sic.network, sic.site), (1, 0, 2));
}

#[test]
fn on_air_talkgroup_voice_channel_grants() {
    for (hex, channel) in [
        ("B1000058015775014001E621", "LCN:5 CHANID:12"),
        ("B1000068015775014001BFAC", "LCN:6 CHANID:14"),
        ("B100006C0157750140017E6A", "LCN:6 CHANID:14"),
    ] {
        let c = from_hex(hex);
        assert_eq!(c.class_name(), "TalkgroupVoiceChannelGrant");
        assert!(c.burst.valid, "{hex}");
        assert_eq!(
            c.to_string(),
            format!("CC:0 TALKGROUP VOICE CHANNEL GRANT FM:81921 TO:87925  {channel} MSG:{hex}")
        );
        assert_eq!(c.source(), Some(Address::Radio(81921)));
        assert_eq!(c.destination(), Some(Address::Talkgroup(87925)));
    }
    let c = from_hex("B1000058015775014001E621");
    assert_eq!(
        (c.channel.unwrap().lcn, c.channel.unwrap().timeslot),
        (5, 2)
    );
}

#[test]
fn on_air_clear() {
    let c = from_hex("AE000000FFFED4FFFECAFF8F");
    assert_eq!(c.class_name(), "Clear");
    assert_eq!(
        c.to_string(),
        "CC:0 CLEAR - RETURN TO LCN:0 CHANID:1 FM:TRUNKING SYSTEM CONTROLLER TO:ALL RADIOS/TALKGROUPS \
         MSG:AE000000FFFED4FFFECAFF8F"
    );
}

#[test]
fn on_air_crc_failures_print_like_sdrtrunk() {
    let c = from_hex("99001100C6500D004204CED4");
    assert!(!c.burst.valid);
    assert_eq!(
        c.to_string(),
        "[CRC-ERROR] CC:0 ALOHA TO:16900 SMALL NETWORK:32 SITE:3 NET-CONNECTED SERVICES:ALL ETSI VER:RESERVED 4 \
         MASK:0 MULTIPLE CONTROL CHANNELS - CAT A SUBSCRIBERS 99001100C6500D004204CED4"
    );
    let u = from_hex("802814000600010000008680");
    assert_eq!(u.class_name(), "UnknownCSBKMessage");
    assert_eq!(
        u.to_string(),
        "[CRC-ERROR] CC:0 CSBK *UNKNOWN* VENDOR:40 UNKNOWN OPCODE:0 MSG:802814000600010000008680"
    );
}

#[test]
fn single_bit_error_is_corrected_by_the_crc() {
    let mut bits = hex_bits("B1000058015775014001E621");
    bits[40] ^= 1;
    let c = create(burst(bits, DataType::Csbk));
    assert!(c.burst.valid);
    assert_eq!(hex(c.bits()), "B1000058015775014001E621");
}

#[test]
fn reference_texts_built_from_fields() {
    // Texts SDRTrunk printed for Clay Electric (runs/dmr/sdrtrunk_ref_cc.txt).
    let small_site_2 = (40, 56, 0b01_0000000_00010_00);
    assert_eq!(
        text_of(
            47,
            &[(28, 31, 2), (31, 32, 1), (32, 56, 87925), (56, 80, 81921)]
        ),
        (
            "CC:0 PROTECT: ILLEGALLY PARKED FM:81921 TO:87925".into(),
            "Protect"
        )
    );
    assert_eq!(
        text_of(47, &[(28, 31, 3), (32, 56, 82321), (56, 80, 87925)]),
        (
            "CC:0 PROTECT: ENABLE TARGET ID PTT ONLY FM:87925 TO:82321".into(),
            "Protect"
        )
    );
    assert_eq!(
        text_of(40, &[(16, 21, 1), (21, 30, 511), (30, 35, 31), small_site_2, (56, 68, 4095), (68, 80, 4095)]),
        (
            "CC:0 CALL TIMERS EMERG:INFINITY PACKET:INFINITY MS-MS:INFINITY MS-LINE:INFINITY SMALL NETWORK:0 SITE:2"
                .into(),
            "CallTimerParameters"
        )
    );
    assert_eq!(
        text_of(
            40,
            &[
                (16, 21, 2),
                (21, 35, 0b01_0000000_00011),
                small_site_2,
                (68, 80, 7)
            ]
        ),
        (
            "CC:0 VOTED NETWORK:0 SITE:3 CHAN: LCN:7 CHANID:15 THIS SMALL NETWORK:0 SITE:2".into(),
            "VoteNowAdvice"
        )
    );
    assert_eq!(
        text_of(40, &[(16, 21, 7), small_site_2]),
        ("CC:0 ANNOUNCEMENT SITE INFORMATION".into(), "Announcement")
    );
    assert_eq!(
        text_of(50, &[(16, 28, 6), (32, 56, 87925), (56, 80, 82321)]),
        (
            "CC:0 BROADCAST TALKGROUP VOICE CHANNEL GRANT FM:82321 TO:87925  LCN:6 CHANID:13"
                .into(),
            "BroadcastTalkgroupVoiceChannelGrant"
        )
    );
}

#[test]
fn other_standard_csbks() {
    assert_eq!(
        text_of(
            32,
            &[
                (16, 17, 0),
                (23, 31, 0x62),
                (32, 56, 82321),
                (56, 80, 0xFFFEC6)
            ]
        ),
        (
            "CC:0 REGISTRATION ACCEPTED TO:82321 FM:REGISTRATION SERVICE".into(),
            "RegistrationAccepted"
        )
    );
    assert_eq!(
        text_of(32, &[(23, 31, 0x2E), (32, 56, 1234), (56, 80, 5678)]),
        (
            "CC:0 ACKNOWLEDGED REASON:CALLED PARTY BUSY FM:5678 TO:1234".into(),
            "Acknowledge"
        )
    );
    assert_eq!(
        text_of(28, &[(28, 32, 1), (32, 56, 82321), (56, 80, 1000)]),
        (
            "CC:0 VOICE SERVICE RADIO CHECK (INDIVIDUAL) TO:82321 FM:1000".into(),
            "ServiceRadioCheck"
        )
    );
    assert_eq!(
        text_of(28, &[(28, 32, 14), (32, 56, 82321), (56, 80, 0x123456)]),
        (
            "CC:0 AUTHENTICATE:82321 CHALLENGE VALUE:123456".into(),
            "AuthenticateRegisterRadioCheck"
        )
    );
    assert_eq!(
        text_of(
            48,
            &[
                (16, 28, 6),
                (28, 29, 1),
                (30, 31, 1),
                (32, 56, 82321),
                (56, 80, 81921)
            ]
        ),
        (
            "CC:0 EMERGENCY PRIVATE VOICE CHANNEL GRANT FM:81921 TO:82321  LCN:6 CHANID:14".into(),
            "PrivateVoiceChannelGrant"
        )
    );
    assert_eq!(
        text_of(57, &[(44, 56, 9)]),
        (
            "CC:0 MOVE TRUNK CONTROL CHANNEL  LCN:9 CHANID:19".into(),
            "MoveTSCC"
        )
    );
    assert_eq!(
        text_of(
            61,
            &[(17, 18, 1), (24, 32, 3), (32, 56, 87925), (56, 80, 81921)]
        )
        .1,
        "Preamble"
    );
    assert_eq!(
        text_of(
            40,
            &[
                (16, 21, 6),
                (21, 35, 0b01_0000000_00111),
                (40, 56, 0b01_0000000_00010_00),
                (68, 80, 15)
            ]
        ),
        (
            "CC:0 SMALL NEIGHBOR NETWORK:0 SITE:7  LCN:15 CHANID:31 THIS NETWORK:0 SITE:2".into(),
            "AdjacentSiteInformation"
        )
    );
    // A vendor opcode is printed by name as an unknown CSBK.
    let (text, class) = text_of(0, &[(8, 16, 16)]);
    assert_eq!(class, "UnknownCSBKMessage");
    assert!(
        text.starts_with("CC:0 CSBK *UNKNOWN* MOTOROLA CAP+ UNKNOWN OPCODE:0 MSG:"),
        "{text}"
    );
}

#[test]
fn call_timer_values() {
    assert_eq!(emergency_timer(0), "INTERNAL");
    assert_eq!(emergency_timer(29), "3.5 MINS");
    assert_eq!(emergency_timer(30), "4.0 MINS");
    assert_eq!(packet_timer(11), "45 SECS");
    assert_eq!(call_timer(108), "5.0 MINS");
    assert_eq!(call_timer(4095), "INFINITY");
}

#[test]
fn multi_block_clear_takes_the_absolute_channel() {
    // MBC header: CLEAR (opcode 46) with the header mask; block 1: absolute
    // channel parameters, last block, LCN 6, 451.0875 / 456.0875 MHz.
    let header_bits = build(
        46,
        &[(0, 1, 0), (32, 56, 0xFFFED4), (56, 80, 0xFFFECA)],
        crc::MBC_HEADER_CRC_MASK,
    );
    let header = Csbk {
        burst: burst(header_bits, DataType::MbcHeader),
        kind: CsbkKind::MbcHeader,
        blocks: Vec::new(),
        absolute: None,
        channel: None,
    };
    assert!(header
        .to_string()
        .starts_with("CC:0 MULTI-BLOCK CSBK HEADER CLEAR MSG:"));
    let block = build(
        0,
        &[
            (0, 1, 1),
            (12, 16, 3),
            (22, 34, 6),
            (34, 44, 456),
            (44, 57, 87 * 8 / 10 * 1000 / 125),
            (57, 67, 451),
            (67, 80, 700),
        ],
        crc::MBC_LAST_BLOCK_CRC_MASK,
    );
    let clear = create_multi(&header, vec![block]);
    assert_eq!(clear.kind, CsbkKind::Clear);
    assert!(clear.burst.valid);
    let channel = clear.channel.unwrap();
    assert!(channel.absolute);
    assert_eq!((channel.lcn, channel.downlink_hz), (6, Some(451_087_500)));
    assert_eq!(
        clear.to_string(),
        format!(
            "CC:0 CLEAR - RETURN TO6 451.0875 FM:TRUNKING SYSTEM CONTROLLER TO:ALL RADIOS/TALKGROUPS MSG:{}",
            hex(clear.bits())
        )
    );
}

//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.


use super::*;
use crate::protocol::p25::test_fixtures::*;

#[test]
fn test_ccitt80_crc_known_tsbk() {
    // Phase 6F.2j: known-good TSBK1 SEC_CCH_BROADCST captured from
    // SDRTrunk's .bits file via the Python replay script. Decoded
    // bytes after deinterleave + Viterbi (clean, metric=0).
    // Expected residual = 0xFFFF (Xored convention) per
    // CRCP25.correctCCITT80.
    let bytes = [0x39u8, 0x00, 0x01, 0x01, 0x04, 0xfd, 0x04, 0x05, 0xe5, 0x04, 0x1e, 0x97];
    let calc = ccitt80_crc(&bytes);
    let msg = u16::from_be_bytes([bytes[10], bytes[11]]);
    let residual = calc ^ msg;
    assert!(
        residual == 0 || residual == 0xFFFF,
        "expected residual 0 or 0xFFFF, got 0x{:04X} (calc=0x{:04X} msg=0x{:04X})",
        residual,
        calc,
        msg,
    );
}

#[test]
fn test_opcode_parsing() {
    assert_eq!(TsbkOpcode::from(0x00), TsbkOpcode::GroupVoiceChannelGrant);
    // Phase 6F.4: 0x33 = IDEN_UPDATE_TDMA, 0x34 = IDEN_UPDATE_VUHF,
    // 0x3D = IDEN_UPDATE (standard FDMA, the one Clay County actually
    // broadcasts). Until 6F.4 we mapped 0x34 to IdentifierUpdate
    // which never matched real on-air TSBKs.
    assert_eq!(TsbkOpcode::from(0x33), TsbkOpcode::IdentifierUpdateTdma);
    assert_eq!(TsbkOpcode::from(0x34), TsbkOpcode::IdentifierUpdateVuhf);
    assert_eq!(TsbkOpcode::from(0x3D), TsbkOpcode::IdentifierUpdate);
    assert_eq!(TsbkOpcode::from(0x3B), TsbkOpcode::NetworkStatusBroadcast);
    // LB and P bits should be masked
    assert_eq!(TsbkOpcode::from(0xC0), TsbkOpcode::GroupVoiceChannelGrant);
}

#[test]
fn test_grp_v_ch_grant_decode() {
    // Construct a synthetic GRP_V_CH_GRANT TSBK
    let mut data = [0u8; 12];
    data[0] = 0x80; // LB=1, P=0, opcode=0x00
    data[1] = 0x00; // standard manufacturer
    // payload: options=0, channel=0x0639 (band 0, ch 1593), talkgroup=0x012C, source=0x000001
    data[2] = 0x00; // options
    data[3] = 0x06; // channel high
    data[4] = 0x39; // channel low
    data[5] = 0x01; // talkgroup high
    data[6] = 0x2C; // talkgroup low
    data[7] = 0x00; // source byte 0
    data[8] = 0x00; // source byte 1
    data[9] = 0x01; // source byte 2
    // CRC (not checked in this test)
    data[10] = 0x00;
    data[11] = 0x00;

    let block = TsbkBlock::parse(&data);
    assert!(block.last_block);
    assert_eq!(block.manufacturer, 0x00);

    let msg = block.decode().unwrap();
    match msg {
        TsbkMessage::GroupVoiceChannelGrant {
            channel,
            talkgroup,
            source,
            service_options,
        } => {
            assert_eq!(channel.0, 0x0639);
            assert_eq!(channel.identifier(), 0);
            assert_eq!(channel.number(), 0x639); // 1593
            assert_eq!(talkgroup.0, 0x012C); // 300
            assert_eq!(source.0, 1);
            // Phase 7C: synthetic test data has options=0
            // (clear voice, no emergency, no encryption).
            assert_eq!(service_options, 0x00);
            assert!(!service_options::is_encrypted(service_options));
            assert!(!service_options::is_emergency(service_options));
        }
        _ => panic!("Expected GroupVoiceChannelGrant"),
    }
}

/// Phase 7C: synthetic GRP_V_CH_GRANT with the encryption bit
/// set in the service options byte. Verifies that the decoder
/// reads payload[0] correctly and that the helpers in
/// `service_options` mod return true for the right bit.
#[test]
fn test_grp_v_ch_grant_decode_encrypted() {
    let mut data = [0u8; 12];
    data[0] = 0x80; // LB=1, P=0, opcode=0x00
    data[1] = 0x00; // standard manufacturer
    // payload: options=0x40 (ENCRYPTED bit set)
    data[2] = 0x40;
    data[3] = 0x06;
    data[4] = 0x39;
    data[5] = 0x01;
    data[6] = 0x2C;
    data[7] = 0x00;
    data[8] = 0x00;
    data[9] = 0x01;
    data[10] = 0x00;
    data[11] = 0x00;

    let block = TsbkBlock::parse(&data);
    let msg = block.decode().unwrap();
    match msg {
        TsbkMessage::GroupVoiceChannelGrant {
            service_options,
            ..
        } => {
            assert_eq!(service_options, 0x40);
            assert!(service_options::is_encrypted(service_options));
            assert!(!service_options::is_emergency(service_options));
        }
        _ => panic!("Expected GroupVoiceChannelGrant"),
    }
}

#[test]
fn test_net_sts_bcst_decode() {
    // Synthetic NET_STS_BCST for Clay County: WACN=0xBEE00, sys=0x8A0
    let mut data = [0u8; 12];
    data[0] = 0xBB; // LB=1, P=0, opcode=0x3B
    data[1] = 0x00; // standard manufacturer
    // payload: lra=0x00, wacn=0xBEE00, system_id=0x8A0, channel=0x0639
    data[2] = 0x00; // payload[0]: LRA
    data[3] = 0xBE; // payload[1]: WACN bits 19-12
    data[4] = 0xE0; // payload[2]: WACN bits 11-4
    data[5] = 0x08; // payload[3]: WACN bits 3-0 (0x0) | system_id bits 11-8 (0x8)
    data[6] = 0xA0; // payload[4]: system_id bits 7-0
    data[7] = 0x06; // payload[5]: channel high
    data[8] = 0x39; // payload[6]: channel low
    data[9] = 0x00; // payload[7]: services

    let block = TsbkBlock::parse(&data);
    let msg = block.decode().unwrap();
    match msg {
        TsbkMessage::NetworkStatus {
            wacn,
            system_id,
            channel,
        } => {
            assert_eq!(wacn, FLORIDA_WACN);
            assert_eq!(system_id, CLAY_SYSTEM_ID);
            assert_eq!(channel.0, 0x0639);
        }
        _ => panic!("Expected NetworkStatus"),
    }
}

#[test]
fn test_frequency_band_calculation() {
    // Band 0 from Clay County: base=851006250, spacing=6250, offset=-45000000
    let band = FrequencyBand {
        identifier: 0,
        bandwidth_hz: 12500,
        transmit_offset_hz: -45_000_000,
        channel_spacing_hz: 6_250,
        base_frequency_hz: 851_006_250,
    };

    // Channel 1593 should be 860.9625 MHz (control channel)
    let freq = band.channel_frequency(1593);
    assert_eq!(freq, CLAY_CONTROL_FREQ_HZ);
}

// Change 067: SYNC_BCST micro-slots, rollover lock and local offset
// (SDRTrunk `SynchronizationBroadcast` bit numbering).
#[test]
fn test_sync_bcst_decode_time_fields() {
    fn set(d: &mut [u8; 12], start: usize, n: usize, v: u64) {
        for i in 0..n {
            let bit = (v >> (n - 1 - i)) & 1;
            let pos = start + i;
            if bit == 1 {
                d[pos / 8] |= 0x80 >> (pos % 8);
            }
        }
    }
    let mut d = [0u8; 12];
    d[0] = 0x80 | 0x30;
    set(&mut d, 29, 1, 1); // not locked to an external reference
    set(&mut d, 30, 1, 0); // micro-slots locked to the minute
    set(&mut d, 33, 1, 0); // local offset valid
    set(&mut d, 34, 1, 1); // west of UTC
    set(&mut d, 35, 4, 4); // 4 h
    set(&mut d, 40, 7, 26);
    set(&mut d, 47, 4, 5);
    set(&mut d, 51, 5, 3);
    set(&mut d, 56, 5, 12);
    set(&mut d, 61, 6, 42);
    set(&mut d, 67, 13, 2000);
    match TsbkBlock::parse(&d).decode() {
        Some(TsbkMessage::TdmaSyncBroadcast {
            time_locked, year, month, day, hours, minutes,
            microslots, microslot_locked, local_offset_min,
        }) => {
            assert!(!time_locked);
            assert_eq!((year, month, day, hours, minutes), (2026, 5, 3, 12, 42));
            assert_eq!((microslots, microslot_locked), (2000, true));
            assert_eq!(local_offset_min, Some(-240));
        }
        other => panic!("{other:?}"),
    }
    // Offset marked invalid, micro-slots free-running.
    set(&mut d, 30, 1, 1);
    set(&mut d, 33, 1, 1);
    match TsbkBlock::parse(&d).decode() {
        Some(TsbkMessage::TdmaSyncBroadcast { microslot_locked, local_offset_min, .. }) => {
            assert!(!microslot_locked);
            assert_eq!(local_offset_min, None);
        }
        other => panic!("{other:?}"),
    }
}

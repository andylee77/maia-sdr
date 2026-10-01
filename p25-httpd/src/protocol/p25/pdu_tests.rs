//! Host tests for `protocol::p25::pdu` (change 074).

use super::*;

fn set_bit(b: &mut [u8], i: usize) {
    b[i / 8] |= 0x80 >> (i % 8);
}

#[test]
fn status_layout_matches_tsdu() {
    // Header block: 98 data dibits + status at 14, 50, 86.
    assert_eq!(raw_len_for_data(98), 101);
    let raw: Vec<u8> = (0..101u32).map(|i| (i % 4) as u8).collect();
    let data = strip_status(&raw);
    assert_eq!(data.len(), 98);
    assert_eq!(&data[..14], &raw[..14]);
    assert_eq!(data[14], raw[15]);
}

#[test]
fn crcs_match_their_reference_values() {
    // CRC-32/CKSUM ("123456789" -> 0x765E7680): the packet CRC.
    assert_eq!(packet_crc32(b"123456789"), 0x765E_7680);
    // CRC-16/XMODEM ("123456789" -> 0x31C3): the header CRC's register.
    assert_eq!(crc_bits(b"123456789", 0..72, 16, 0x1021), 0x31C3);
    // SDRTrunk's table entries: a single set bit gives its entry.
    let hdr = |i| {
        let mut b = [0u8; 12];
        set_bit(&mut b, i);
        header_crc(&b)
    };
    assert_eq!((hdr(0), hdr(1), hdr(79)), (0x1BCB, 0x8DE5, 0xEFDE));
    // CRC-9 covers bits 0-6 then 16-143 (135 bits): entries 0, 1, 133, 134.
    let c9 = |i| {
        let mut b = [0u8; 18];
        set_bit(&mut b, i);
        block_crc9(&b)
    };
    assert_eq!((c9(0), c9(1), c9(142), c9(143)), (0x1E7, 0x1F3, 0x14D, 0x1A6));
}

#[test]
fn header_crc_agrees_with_the_tsbk_crc() {
    // Same CCITT CRC over 80 bits as a TSBK (either convention).
    let mut b = [0x5Au8, 0xC4, 0, 0x12, 0x34, 0x56, 0x82, 0x08, 0, 2, 0, 0];
    let tsbk = crate::protocol::p25::tsbk::ccitt80_crc(&b);
    let ours = header_crc(&b);
    assert!(ours == tsbk || ours == tsbk ^ 0xFFFF, "{ours:04X} vs {tsbk:04X}");
    b[10] = (ours >> 8) as u8;
    b[11] = ours as u8;
    assert!(PduHeader::parse(b, true, None).crc_ok);
}

#[test]
fn three_quarter_trellis_round_trip_corrects_errors() {
    let mut data = [0u8; 18];
    for (i, d) in data.iter_mut().enumerate() {
        *d = (i as u8).wrapping_mul(37) ^ 0xA5;
    }
    let mut on_air = encode_3_4(&data);
    let (clean, errors) = decode_3_4(&on_air).unwrap();
    assert_eq!((clean, errors), (data, 0));
    // Two dibit errors far apart are corrected.
    on_air[10] ^= 1;
    on_air[70] ^= 2;
    let (fixed, errors) = decode_3_4(&on_air).unwrap();
    assert_eq!(fixed, data);
    assert!(errors >= 2, "{errors}");
}

/// A header's 12 octets (with its CRC).
fn header(confirmed: bool, outbound: bool, format: u8, sap: u8, llid: u32, btf: u8, pad: u8, dho: u8) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[0] = ((confirmed as u8) << 6) | ((outbound as u8) << 5) | (format & 0x1F);
    b[1] = 0xC0 | (sap & 0x3F);
    b[3] = (llid >> 16) as u8;
    b[4] = (llid >> 8) as u8;
    b[5] = llid as u8;
    b[6] = 0x80 | (btf & 0x7F);
    b[7] = pad & 0x1F;
    b[9] = dho & 0x3F;
    let crc = header_crc(&b);
    b[10] = (crc >> 8) as u8;
    b[11] = crc as u8;
    b
}

#[test]
fn header_fields() {
    let h = PduHeader::parse(header(true, true, FORMAT_PACKET, 4, 0x12_3456, 3, 8, 2), true, None);
    assert!(h.confirmed && h.outbound && h.full_message);
    assert_eq!((h.format, h.sap, h.llid, h.blocks_to_follow, h.pad_octets, h.data_header_offset), (22, 4, 0x12_3456, 3, 8, 2));
    assert!(h.confirmed_blocks());
    assert_eq!((format_name(h.format), sap_name(h.sap)), ("packet", "packet_data"));
}

/// A UDP/IPv4 packet to `dst_port` with `payload`.
fn udp_packet(dst_port: u16, payload: &[u8]) -> Vec<u8> {
    let total = 28 + payload.len();
    let mut p = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, 64, 17, 0, 0, 12, 1, 2, 3, 10, 0, 0, 7];
    p.extend_from_slice(&4001u16.to_be_bytes());
    p.extend_from_slice(&dst_port.to_be_bytes());
    p.extend_from_slice(&((8 + payload.len()) as u16).to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p.extend_from_slice(payload);
    p
}

/// Confirmed blocks (serial, CRC-9, 16 octets each) carrying `payload`.
fn confirmed_blocks(payload: &[u8]) -> Vec<[u8; 18]> {
    payload
        .chunks(16)
        .enumerate()
        .map(|(n, c)| {
            let mut b = [0u8; 18];
            b[0] = (n as u8) << 1;
            b[2..2 + c.len()].copy_from_slice(c);
            let crc = block_crc9(&b);
            b[0] |= (crc >> 8) as u8 & 1;
            b[1] = crc as u8;
            b
        })
        .collect()
}

/// The user payload of a PDU: SNDCP header, packet, pad, CRC-32.
fn pdu_payload(packet: &[u8], octets: usize) -> (Vec<u8>, u8) {
    let mut p = vec![0x40, 0x00]; // SNDCP: PDU type 4, NSAPI 0, no compression
    p.extend_from_slice(packet);
    let pad = octets - 4 - p.len();
    p.extend(std::iter::repeat(0).take(pad));
    let crc = packet_crc32(&p);
    p.extend_from_slice(&crc.to_be_bytes());
    (p, pad as u8)
}

#[test]
fn packet_assembly_checks_crcs_and_finds_the_ip_packet() {
    let ip = udp_packet(4001, b"LRRP!!");
    let (payload, pad) = pdu_payload(&ip, 48);
    let h = PduHeader::parse(header(true, false, FORMAT_PACKET, 4, 0x12_3456, 3, pad, 2), true, None);
    let blocks: Vec<PduBlock> = confirmed_blocks(&payload)
        .iter()
        .map(|b| decode_block(&encode_3_4(b), true).unwrap())
        .collect();
    assert!(blocks.iter().all(|b| b.crc9_ok == Some(true)));
    let p = Packet::assemble(&h, &blocks).unwrap();
    assert!(p.crc32_ok);
    assert_eq!(p.sndcp_header(), Some((4, 0, 0, 0)));
    assert_eq!(p.user_data(), &ip[..]);
    let info = parse_ip(p.user_data()).unwrap();
    assert_eq!((info.protocol, info.dst_port, udp_service(4001)), (17, Some(4001), Some("lrrp")));
    assert_eq!(info.dst, std::net::Ipv4Addr::new(10, 0, 0, 7));
    assert_eq!(&p.user_data()[info.payload_at..], b"LRRP!!");
    // A corrupt payload octet fails the CRC-9 of its block.
    let mut bad = confirmed_blocks(&payload)[1];
    bad[5] ^= 0x10;
    assert_eq!(decode_block(&encode_3_4(&bad), true).unwrap().crc9_ok, Some(false));
}

#[test]
fn a_pdu_on_the_air_is_decoded_end_to_end() {
    use crate::protocol::p25::control_channel::ControlChannelDecoder;
    use crate::protocol::p25::fec::{bch, trellis_encode_bytes};
    use crate::protocol::p25::test_fixtures::CLAY_NAC;
    use crate::protocol::p25::wire::FRAME_SYNC_PATTERN;

    let unpack = |bits: u64, n: usize| -> Vec<u8> { (0..n).rev().map(|i| ((bits >> (2 * i)) & 3) as u8).collect() };
    let ip = udp_packet(4005, b"ARS reg");
    let (payload, pad) = pdu_payload(&ip, 48);
    let hdr = header(true, false, FORMAT_PACKET, 4, 0x00_1234, 3, pad, 2);
    let mut data: Vec<u8> = trellis_encode_bytes(&hdr).to_vec();
    for b in confirmed_blocks(&payload) {
        data.extend_from_slice(&encode_3_4(&b));
    }
    data.extend(std::iter::repeat(0).take(7)); // nulls to the next status (K=3)
    // Status dibits in place (14 + 36k).
    let mut body = Vec::new();
    let mut it = data.into_iter();
    let mut pos = 0;
    loop {
        if pos >= 14 && (pos - 14) % 36 == 0 {
            body.push(1);
        } else if let Some(d) = it.next() {
            body.push(d);
        } else {
            break;
        }
        pos += 1;
    }
    let nid = unpack(bch::encode_nid(CLAY_NAC, 0xC), 32);
    let mut frame = unpack(FRAME_SYNC_PATTERN, 24);
    frame.extend_from_slice(&nid[..11]);
    frame.push(0);
    frame.extend_from_slice(&nid[11..]);
    frame.extend_from_slice(&body);

    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let mut dec = ControlChannelDecoder::new();
    dec.chain_label = "data1";
    dec.pdu_tx = Some(tx);
    for d in frame {
        dec.process_dibit(d);
    }
    let f = rx.try_recv().expect("a PDU");
    assert_eq!((f.chain, f.nac, f.blocks_expected, f.blocks.len()), ("data1", CLAY_NAC, 3, 3));
    assert!(f.header.crc_ok && !f.header.outbound);
    assert_eq!(f.header.llid, 0x1234);
    assert!(f.blocks.iter().all(|b| b.crc9_ok == Some(true)));
    let p = Packet::assemble(&f.header, &f.blocks).unwrap();
    assert!(p.crc32_ok);
    let info = parse_ip(p.user_data()).unwrap();
    assert_eq!((info.dst_port, udp_service(4005)), (Some(4005), Some("ars")));
    assert_eq!((dec.pdu_frames, dec.pdu_blocks, dec.pdu_header_crc_fail), (1, 3, 0));
}

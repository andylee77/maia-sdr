//! P25 Phase 1 packet data (PDU, DUID 0xC), TIA-102.BAAA.
//!
//! Port of SDRTrunk's PDU path (`P25P1MessageFramer`, `PDUHeader`,
//! `ConfirmedDataBlock`, `ViterbiDecoder_3_4_P25`, `PacketMessage`), plus
//! the checks SDRTrunk skips: the CRC-9 of each confirmed block and the
//! packet CRC-32.
//!
//! A PDU is a header block and `blocks_to_follow` data blocks, each 196
//! trellis-coded bits (98 dibits), back to back after the NID, with the
//! body's status dibits on the frame's period-36 schedule (14 + 36k). The
//! header and unconfirmed blocks use the 1/2-rate trellis (12 octets);
//! confirmed blocks the 3/4-rate trellis (18 octets: serial, CRC-9, 16
//! payload octets). The packet's last 4 octets are its CRC-32.

use crate::protocol::p25::fec::{TrellisDecoder, DATA_DEINTERLEAVE};

/// Trellis-coded dibits in one block (196 bits).
pub const BLOCK_DIBITS: usize = 98;
/// Most data blocks read in one PDU (the header field allows 127).
pub const MAX_BLOCKS: usize = 64;

/// Header formats (octet 0, bits 3-7).
pub const FORMAT_RESPONSE: u8 = 3;
pub const FORMAT_UMBTC: u8 = 21;
pub const FORMAT_PACKET: u8 = 22;
pub const FORMAT_AMBTC: u8 = 23;

/// A body status dibit (every 36th dibit of the frame: body raw
/// positions 14 + 36k).
fn is_status(pos: usize) -> bool {
    crate::protocol::p25::types::is_body_status_dibit(pos)
}

/// Body raw dibits (status dibits in place) that hold `data` data dibits.
pub fn raw_len_for_data(data: usize) -> usize {
    let (mut raw, mut got) = (0, 0);
    while got < data {
        if !is_status(raw) {
            got += 1;
        }
        raw += 1;
    }
    raw
}

/// The data dibits of a PDU body (status dibits removed).
pub fn strip_status(raw: &[u8]) -> Vec<u8> {
    raw.iter().enumerate().filter(|(i, _)| !is_status(*i)).map(|(_, d)| d & 3).collect()
}

/// `len` bits from `bits_start` (bit 0 = MSB of octet 0), as a number.
pub fn bits(bytes: &[u8], start: usize, len: usize) -> u64 {
    let mut v = 0u64;
    for i in start..start + len {
        let bit = bytes.get(i / 8).map(|b| (b >> (7 - i % 8)) & 1).unwrap_or(0);
        v = (v << 1) | bit as u64;
    }
    v
}

fn bit_at(bytes: &[u8], i: usize) -> u8 {
    (bytes[i / 8] >> (7 - i % 8)) & 1
}

/// MSB-first CRC over `n` bits of `bytes` taken from `positions`.
fn crc_bits(bytes: &[u8], positions: impl Iterator<Item = usize>, width: u32, poly: u64) -> u64 {
    let mask = (1u64 << width) - 1;
    let mut reg = 0u64;
    for i in positions {
        let top = (reg >> (width - 1)) & 1;
        reg = (reg << 1) & mask;
        if top ^ bit_at(bytes, i) as u64 == 1 {
            reg ^= poly & mask;
        }
    }
    reg
}

/// Header CRC-16 (CCITT, init 0, final XOR 0xFFFF) over bits 0-79.
pub fn header_crc(bytes: &[u8; 12]) -> u16 {
    (crc_bits(bytes, 0..80, 16, 0x1021) ^ 0xFFFF) as u16
}

/// Confirmed-block CRC-9 (x^9+x^6+x^4+x^3+1, init 0, final XOR 0x1FF)
/// over the serial number (bits 0-6) and the payload (bits 16-143).
pub fn block_crc9(bytes: &[u8; 18]) -> u16 {
    (crc_bits(bytes, (0..7).chain(16..144), 9, 0x259) ^ 0x1FF) as u16
}

/// Packet CRC-32 (0x04C11DB7, init 0, not reflected, final XOR
/// 0xFFFFFFFF) over `bytes`.
pub fn packet_crc32(bytes: &[u8]) -> u32 {
    (crc_bits(bytes, 0..bytes.len() * 8, 32, 0x04C1_1DB7) ^ 0xFFFF_FFFF) as u32
}

/// Transmitted CRC equals the computed one, or its complement (SDRTrunk
/// accepts both conventions).
fn crc_matches(calc: u64, sent: u64, mask: u64) -> bool {
    sent == calc || sent == (calc ^ mask)
}

/// The PDU header (octets 0-11).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PduHeader {
    #[serde(skip)]
    pub bytes: [u8; 12],
    /// CRC good (after at most one corrected bit).
    pub crc_ok: bool,
    pub corrected_bit: Option<u8>,
    /// A/N: the sender wants a response (confirmed delivery).
    pub confirmed: bool,
    /// I/O: outbound (network to radio).
    pub outbound: bool,
    pub format: u8,
    /// Service access point (packet and AMBTC headers).
    pub sap: u8,
    pub mfid: u8,
    /// Logical link id: the radio (to it if outbound, from it if inbound).
    pub llid: u32,
    pub full_message: bool,
    pub blocks_to_follow: u8,
    pub pad_octets: u8,
    pub data_header_offset: u8,
    /// Response packets: the response octet (octet 1).
    pub response: u8,
    /// AMBTC: the opcode (bits 58-63).
    pub opcode: u8,
}

impl PduHeader {
    /// Decode the header block (98 trellis dibits). `None` when the
    /// trellis gives nothing; check `crc_ok`.
    pub fn decode(dibits: &[u8]) -> Option<Self> {
        let mut b = TrellisDecoder::decode(dibits)?;
        let sent = |b: &[u8; 12]| ((b[10] as u64) << 8) | b[11] as u64;
        let mut corrected = None;
        let mut ok = crc_matches(header_crc(&b) as u64, sent(&b), 0xFFFF);
        if !ok {
            // One wrong bit is corrected (as SDRTrunk does).
            for i in 0..96 {
                b[i / 8] ^= 0x80 >> (i % 8);
                if crc_matches(header_crc(&b) as u64, sent(&b), 0xFFFF) {
                    ok = true;
                    corrected = Some(i as u8);
                    break;
                }
                b[i / 8] ^= 0x80 >> (i % 8);
            }
        }
        Some(Self::parse(b, ok, corrected))
    }

    pub fn parse(b: [u8; 12], crc_ok: bool, corrected_bit: Option<u8>) -> Self {
        let f = |s, l| bits(&b, s, l);
        PduHeader {
            bytes: b,
            crc_ok,
            corrected_bit,
            confirmed: f(1, 1) == 1,
            outbound: f(2, 1) == 1,
            format: f(3, 5) as u8,
            sap: f(10, 6) as u8,
            mfid: f(16, 8) as u8,
            llid: f(24, 24) as u32,
            full_message: f(48, 1) == 1,
            blocks_to_follow: f(49, 7) as u8,
            pad_octets: f(59, 5) as u8,
            data_header_offset: f(74, 6) as u8,
            response: f(8, 8) as u8,
            opcode: f(58, 6) as u8,
        }
    }

    /// Data blocks use the 3/4-rate trellis (confirmed packet data).
    pub fn confirmed_blocks(&self) -> bool {
        self.confirmed && self.format == FORMAT_PACKET
    }
}

/// A PDU as the decoder hands it out.
#[derive(Debug, Clone)]
pub struct PduFrame {
    /// Decoder that read it ("control", "data1", ...).
    pub chain: &'static str,
    pub nac: u16,
    pub at_ms: u64,
    pub header: PduHeader,
    /// Data blocks in order (a block the trellis could not decode is
    /// missing; see `blocks_expected`).
    pub blocks: Vec<PduBlock>,
    pub blocks_expected: usize,
}

/// One decoded data block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PduBlock {
    /// Payload octets (12 unconfirmed, 16 confirmed).
    pub payload: Vec<u8>,
    /// Confirmed blocks: data block serial number.
    pub serial: Option<u8>,
    /// Confirmed blocks: CRC-9 good.
    pub crc9_ok: Option<bool>,
    /// Trellis path metric (bit errors seen).
    pub errors: u32,
}

/// Decode one data block (98 trellis dibits).
pub fn decode_block(dibits: &[u8], confirmed: bool) -> Option<PduBlock> {
    if confirmed {
        let (b, errors) = decode_3_4(dibits)?;
        let sent = bits(&b, 7, 9);
        Some(PduBlock {
            payload: b[2..18].to_vec(),
            serial: Some(bits(&b, 0, 7) as u8),
            crc9_ok: Some(crc_matches(block_crc9(&b) as u64, sent, 0x1FF)),
            errors,
        })
    } else {
        let b = TrellisDecoder::decode(dibits)?;
        Some(PduBlock { payload: b.to_vec(), serial: None, crc9_ok: None, errors: 0 })
    }
}

/// The user packet of a PDU: the blocks' payloads joined, less the data
/// header offset, pad octets and the CRC-32.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// All payload octets (including offset, pad and CRC).
    pub payload: Vec<u8>,
    pub crc32_ok: bool,
    /// Range of the user data in `payload`.
    pub data: std::ops::Range<usize>,
}

impl Packet {
    pub fn assemble(h: &PduHeader, blocks: &[PduBlock]) -> Option<Self> {
        let payload: Vec<u8> = blocks.iter().flat_map(|b| b.payload.iter().copied()).collect();
        if payload.len() < 4 {
            return None;
        }
        let body = payload.len() - 4;
        let sent = u32::from_be_bytes(payload[body..].try_into().ok()?) as u64;
        let crc32_ok = crc_matches(packet_crc32(&payload[..body]) as u64, sent, 0xFFFF_FFFF);
        let start = (h.data_header_offset as usize).min(body);
        let end = body.saturating_sub(h.pad_octets as usize).max(start);
        Some(Packet { payload, crc32_ok, data: start..end })
    }

    pub fn user_data(&self) -> &[u8] {
        &self.payload[self.data.clone()]
    }

    /// The SNDCP data header (the 2 octets before the user data when the
    /// data header offset is 2): (PDU type, NSAPI, IP header compression,
    /// UDP compression).
    pub fn sndcp_header(&self) -> Option<(u8, u8, u8, u8)> {
        (self.data.start == 2).then(|| {
            let (a, b) = (self.payload[0], self.payload[1]);
            (a >> 4, a & 0xF, b >> 4, b & 0xF)
        })
    }
}

/// An IPv4 packet's header, and its UDP ports when it is UDP.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct IpInfo {
    pub src: std::net::Ipv4Addr,
    pub dst: std::net::Ipv4Addr,
    pub protocol: u8,
    pub total_len: u16,
    pub src_port: Option<u16>,
    pub dst_port: Option<u16>,
    /// Offset of the UDP payload (or the IP payload) in the packet.
    #[serde(skip)]
    pub payload_at: usize,
}

/// Parse an IPv4 (and UDP) header. `None` if it is not IPv4.
pub fn parse_ip(p: &[u8]) -> Option<IpInfo> {
    if p.len() < 20 || p[0] >> 4 != 4 {
        return None;
    }
    let ihl = (p[0] & 0xF) as usize * 4;
    if ihl < 20 || p.len() < ihl {
        return None;
    }
    let total_len = u16::from_be_bytes([p[2], p[3]]);
    let protocol = p[9];
    let src = std::net::Ipv4Addr::new(p[12], p[13], p[14], p[15]);
    let dst = std::net::Ipv4Addr::new(p[16], p[17], p[18], p[19]);
    let (mut src_port, mut dst_port, mut payload_at) = (None, None, ihl);
    if protocol == 17 && p.len() >= ihl + 8 {
        src_port = Some(u16::from_be_bytes([p[ihl], p[ihl + 1]]));
        dst_port = Some(u16::from_be_bytes([p[ihl + 2], p[ihl + 3]]));
        payload_at = ihl + 8;
    }
    Some(IpInfo { src, dst, protocol, total_len, src_port, dst_port, payload_at })
}

/// The well-known application of a UDP port (Motorola services).
pub fn udp_service(port: u16) -> Option<&'static str> {
    Some(match port {
        231 => "cellocator",
        4001 => "lrrp",
        4004 | 64414 => "xcmp",
        4005 => "ars",
        4007 => "tms",
        4008 => "telemetry",
        4009 => "otap",
        4012 => "battery",
        4013 => "job_ticket",
        _ => return None,
    })
}

/// Name of a service access point.
pub fn sap_name(sap: u8) -> &'static str {
    match sap {
        0 => "user_data",
        1 => "encrypted_user_data",
        2 => "circuit_data",
        3 => "circuit_data_control",
        4 => "packet_data",
        5 => "arp",
        6 => "sndcp_control",
        15 => "scan_preamble",
        29 => "encryption_support",
        31 => "extended_address",
        32 => "registration",
        33 => "channel_reassignment",
        34 => "system_configuration",
        35 => "loopback",
        36 => "statistics",
        37 => "out_of_service",
        38 => "paging",
        39 => "configuration",
        40 => "kmm",
        41 => "encrypted_kmm",
        48 => "location",
        61 => "trunking_control",
        63 => "encrypted_trunking_control",
        _ => "other",
    }
}

/// Name of a response packet's response octet (class bits 7-6, type
/// bits 5-3), as SDRTrunk's `PacketResponse`.
pub fn response_name(r: u8) -> &'static str {
    match (r >> 6, (r >> 3) & 7) {
        (0, 1) => "all_blocks_received",
        (1, 0) => "illegal_format",
        (1, 1) => "packet_crc_fail",
        (1, 2) => "memory_full",
        (1, 3) => "fsn_out_of_sequence",
        (1, 4) => "undeliverable",
        (1, 5) => "msn_out_of_sequence",
        (1, 6) => "unauthorized",
        (2, _) => "selective_retry",
        _ => "other",
    }
}

/// Name of a header format.
pub fn format_name(format: u8) -> &'static str {
    match format {
        FORMAT_RESPONSE => "response",
        FORMAT_UMBTC => "umbtc",
        FORMAT_PACKET => "packet",
        FORMAT_AMBTC => "ambtc",
        _ => "other",
    }
}

// ── 3/4-rate trellis ─────────────────────────────────────────────────

/// TIA-102.BAAA 3/4-rate trellis: transmitted nibble for (previous
/// input tribit = state, current input tribit). SDRTrunk `P25_3_4_Node`.
const TRANSITION_3_4: [[u8; 8]; 8] = [
    [2, 13, 14, 1, 7, 8, 11, 4],
    [14, 1, 7, 8, 11, 4, 2, 13],
    [10, 5, 6, 9, 15, 0, 3, 12],
    [6, 9, 15, 0, 3, 12, 10, 5],
    [15, 0, 3, 12, 10, 5, 6, 9],
    [3, 12, 10, 5, 6, 9, 15, 0],
    [7, 8, 11, 4, 2, 13, 14, 1],
    [11, 4, 2, 13, 14, 1, 7, 8],
];

/// Deinterleaved trellis nibbles of a block (98 dibits).
fn nibbles(dibits: &[u8]) -> [u8; 49] {
    let mut inter = [0u8; 196];
    for n in 0..98 {
        inter[2 * n] = (dibits[n] >> 1) & 1;
        inter[2 * n + 1] = dibits[n] & 1;
    }
    let mut de = [0u8; 196];
    for i in 0..196 {
        de[DATA_DEINTERLEAVE[i]] = inter[i];
    }
    let mut out = [0u8; 49];
    for (n, o) in out.iter_mut().enumerate() {
        *o = (de[4 * n] << 3) | (de[4 * n + 1] << 2) | (de[4 * n + 2] << 1) | de[4 * n + 3];
    }
    out
}

/// Decode a 3/4-rate block (98 dibits) to 18 octets and the path metric
/// (bit errors). Viterbi over 8 states from state 0; the 49th symbol
/// flushes with input 0.
pub fn decode_3_4(dibits: &[u8]) -> Option<([u8; 18], u32)> {
    if dibits.len() < BLOCK_DIBITS {
        return None;
    }
    let recv = nibbles(dibits);
    let mut metrics = [u32::MAX; 8];
    metrics[0] = 0;
    let mut back = [[0u8; 8]; 49];
    for (t, &r) in recv.iter().enumerate() {
        let mut next = [u32::MAX; 8];
        for prev in 0..8 {
            if metrics[prev] == u32::MAX {
                continue;
            }
            for cur in 0..8 {
                if t == 48 && cur != 0 {
                    continue;
                }
                let cost = metrics[prev] + (TRANSITION_3_4[prev][cur] ^ r).count_ones();
                if cost < next[cur] {
                    next[cur] = cost;
                    back[t][cur] = prev as u8;
                }
            }
        }
        metrics = next;
    }
    let errors = metrics[0];
    let mut state = 0usize;
    let mut inputs = [0u8; 49];
    for t in (0..49).rev() {
        inputs[t] = state as u8;
        state = back[t][state] as usize;
    }
    let mut out = [0u8; 18];
    for (n, &tri) in inputs[..48].iter().enumerate() {
        for k in 0..3 {
            let bit = (tri >> (2 - k)) & 1;
            let i = n * 3 + k;
            out[i / 8] |= bit << (7 - i % 8);
        }
    }
    Some((out, errors))
}

/// 3/4-rate encoder (tests): 18 octets to 98 on-air dibits.
#[cfg(test)]
pub(crate) fn encode_3_4(data: &[u8; 18]) -> [u8; 98] {
    let mut nib = [0u8; 49];
    let mut prev = 0usize;
    for (n, slot) in nib.iter_mut().enumerate().take(48) {
        let tri = bits(data, n * 3, 3) as usize;
        *slot = TRANSITION_3_4[prev][tri];
        prev = tri;
    }
    nib[48] = TRANSITION_3_4[prev][0];
    let mut de = [0u8; 196];
    for n in 0..49 {
        for k in 0..4 {
            de[4 * n + k] = (nib[n] >> (3 - k)) & 1;
        }
    }
    let mut inter = [0u8; 196];
    for i in 0..196 {
        inter[i] = de[DATA_DEINTERLEAVE[i]];
    }
    let mut out = [0u8; 98];
    for n in 0..98 {
        out[n] = (inter[2 * n] << 1) | inter[2 * n + 1];
    }
    out
}

#[cfg(test)]
#[path = "pdu_tests.rs"]
mod tests;

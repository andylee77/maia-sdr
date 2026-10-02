//! Packet data: the PDUs the P25 decoders read, as records. Each says which radio (LLID), which
//! way, what (format, SAP, SNDCP, IP / UDP and the Motorola service by port) and whether its
//! CRCs held. Kept in memory (recent records, totals per radio) and logged. The same PDU read by
//! two decoders within two seconds is counted once.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::protocol::p25::pdu::{self, PduFrame};
use crate::services::events::EventLog;

/// Records kept.
pub const RECENT: usize = 500;
/// A PDU seen again within this long (another decoder) is a duplicate.
const DUP_MS: u64 = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct DataRecord {
    pub at_ms: u64,
    pub site: String,
    /// The decoder that read it.
    pub chain: &'static str,
    pub nac: u16,
    /// Network to radio.
    pub outbound: bool,
    pub format: &'static str,
    pub sap: &'static str,
    pub sap_id: u8,
    pub llid: u32,
    pub mfid: u8,
    pub confirmed: bool,
    pub blocks: usize,
    pub blocks_expected: usize,
    /// Confirmed blocks whose CRC-9 failed.
    pub bad_blocks: usize,
    /// Trellis bit errors per confirmed block.
    pub block_errors: Vec<u32>,
    /// Packet CRC-32 (packet data with blocks).
    pub crc_ok: Option<bool>,
    /// SNDCP data header (PDU type, NSAPI, IP and UDP compression).
    pub sndcp: Option<(u8, u8, u8, u8)>,
    pub ip: Option<pdu::IpInfo>,
    /// Application by UDP port (lrrp, ars, tms, ...).
    pub service: Option<&'static str>,
    /// User data octets.
    pub bytes: usize,
    /// AMBTC opcode; response octet.
    pub opcode: Option<u8>,
    pub response: Option<u8>,
    /// Up to 64 octets of the user data (or payload), hex.
    pub hex: String,
}

/// Totals for one radio at one site.
#[derive(Debug, Clone, Default, Serialize)]
pub struct RadioData {
    pub site: String,
    pub llid: u32,
    pub packets: u64,
    pub inbound: u64,
    pub outbound: u64,
    pub bytes: u64,
    pub first_ms: u64,
    pub last_ms: u64,
    /// Packets per service or SAP.
    pub kinds: BTreeMap<String, u64>,
    pub ip: Option<std::net::Ipv4Addr>,
}

/// What a site's decoders read.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub pdus: u64,
    /// The same PDU read by a second decoder.
    pub duplicates: u64,
    /// By kind ("packet/lrrp", "ambtc", "response/...").
    pub totals: BTreeMap<String, u64>,
}

#[derive(Debug, Default)]
pub struct DataState {
    pub recent: VecDeque<DataRecord>,
    pub radios: HashMap<(String, u32), RadioData>,
    /// Per site.
    pub counts: HashMap<String, Counts>,
}

fn hex(b: &[u8]) -> String {
    b.iter().take(64).map(|x| format!("{x:02x}")).collect()
}

/// A PDU as a record.
pub fn to_record(f: &PduFrame, site: &str) -> DataRecord {
    let h = &f.header;
    let mut r = DataRecord {
        at_ms: f.at_ms,
        site: site.to_string(),
        chain: f.chain,
        nac: f.nac,
        outbound: h.outbound,
        format: pdu::format_name(h.format),
        // A response header has its response octet where the others have the SAP.
        sap: if h.format == pdu::FORMAT_RESPONSE { pdu::response_name(h.response) } else { pdu::sap_name(h.sap) },
        sap_id: h.sap,
        llid: h.llid,
        mfid: h.mfid,
        confirmed: h.confirmed,
        blocks: f.blocks.len(),
        blocks_expected: f.blocks_expected,
        bad_blocks: f.blocks.iter().filter(|b| b.crc9_ok == Some(false)).count(),
        block_errors: f.blocks.iter().filter(|b| b.serial.is_some()).map(|b| b.errors).collect(),
        crc_ok: None,
        sndcp: None,
        ip: None,
        service: None,
        bytes: 0,
        opcode: (h.format == pdu::FORMAT_AMBTC).then_some(h.opcode),
        response: (h.format == pdu::FORMAT_RESPONSE).then_some(h.response),
        hex: String::new(),
    };
    if h.format == pdu::FORMAT_PACKET && !f.blocks.is_empty() && f.blocks.len() == f.blocks_expected {
        if let Some(p) = pdu::Packet::assemble(h, &f.blocks) {
            r.crc_ok = Some(p.crc32_ok);
            r.sndcp = p.sndcp_header();
            let data = p.user_data();
            r.bytes = data.len();
            r.hex = hex(data);
            // Plain user and packet data carry IP (SAP 0 and 4).
            if matches!(h.sap, 0 | 4) {
                if let Some(ip) = pdu::parse_ip(data) {
                    r.service = ip.dst_port.and_then(pdu::udp_service).or(ip.src_port.and_then(pdu::udp_service));
                    r.ip = Some(ip);
                }
            }
        }
    } else if !f.blocks.is_empty() {
        let all: Vec<u8> = f.blocks.iter().flat_map(|b| b.payload.iter().copied()).collect();
        r.bytes = all.len();
        r.hex = hex(&all);
    }
    r
}

/// The record's kind for the totals.
fn kind(r: &DataRecord) -> String {
    match (r.format, r.service) {
        ("packet", Some(s)) => format!("packet/{s}"),
        ("packet", None) => format!("packet/{}", r.sap),
        ("response", _) => format!("response/{}", r.sap),
        (f, _) => f.to_string(),
    }
}

fn summary(r: &DataRecord) -> String {
    let dir = if r.outbound { "to" } else { "from" };
    let mut s = format!("{} {dir} radio {} ({} blocks", kind(r), r.llid, r.blocks);
    if let Some(ok) = r.crc_ok {
        s.push_str(if ok { ", CRC ok" } else { ", CRC bad" });
    }
    s.push(')');
    if let Some(ip) = &r.ip {
        s.push_str(&format!(" {} -> {}", ip.src, ip.dst));
        if let (Some(sp), Some(dp)) = (ip.src_port, ip.dst_port) {
            s.push_str(&format!(" udp {sp}->{dp}"));
        }
    }
    s
}

impl DataState {
    /// One site's counts, or every site's together.
    pub fn counts(&self, site: Option<&str>) -> Counts {
        let mut out = Counts::default();
        for (_, c) in self.counts.iter().filter(|(s, _)| site.is_none_or(|w| w == s.as_str())) {
            out.pdus += c.pdus;
            out.duplicates += c.duplicates;
            for (k, n) in &c.totals {
                *out.totals.entry(k.clone()).or_default() += n;
            }
        }
        out
    }

    /// Add a record, unless another decoder read the same PDU just before. True if new.
    pub fn add(&mut self, r: DataRecord) -> bool {
        let dup = self.recent.iter().rev().take(16).any(|o| {
            r.at_ms.saturating_sub(o.at_ms) < DUP_MS && o.chain != r.chain && o.llid == r.llid && o.hex == r.hex && o.format == r.format
        });
        let counts = self.counts.entry(r.site.clone()).or_default();
        if dup {
            counts.duplicates += 1;
            return false;
        }
        counts.pdus += 1;
        *counts.totals.entry(kind(&r)).or_default() += 1;
        let e = self.radios.entry((r.site.clone(), r.llid)).or_insert_with(|| RadioData {
            site: r.site.clone(),
            llid: r.llid,
            first_ms: r.at_ms,
            ..Default::default()
        });
        e.packets += 1;
        if r.outbound {
            e.outbound += 1;
        } else {
            e.inbound += 1;
        }
        e.bytes += r.bytes as u64;
        e.last_ms = r.at_ms;
        *e.kinds.entry(kind(&r)).or_default() += 1;
        if let Some(ip) = &r.ip {
            // The radio's own address: the far end of what it receives, the source of what it
            // sends.
            e.ip = Some(if r.outbound { ip.dst } else { ip.src });
        }
        if self.recent.len() >= RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(r);
        true
    }
}

/// The records, shared between the decoders that add to them and the API.
pub struct PacketData {
    state: Mutex<DataState>,
    log: Arc<EventLog>,
}

impl PacketData {
    pub fn new(log: Arc<EventLog>) -> Self {
        PacketData { state: Mutex::default(), log }
    }

    /// A PDU read at `site`.
    pub fn pdu(&self, f: &PduFrame, site: &str) {
        let r = to_record(f, site);
        let line = summary(&r);
        let new = self.state.lock().map(|mut d| d.add(r)).unwrap_or(false);
        if new {
            self.log.system("data", line);
        }
    }

    /// Read the records.
    pub fn with<T>(&self, f: impl FnOnce(&DataState) -> T) -> T {
        f(&self.state.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::p25::pdu::{PduBlock, PduHeader};

    fn frame(chain: &'static str, at_ms: u64, llid: u32, outbound: bool) -> PduFrame {
        let mut b = [0u8; 12];
        b[0] = ((outbound as u8) << 5) | pdu::FORMAT_AMBTC;
        b[1] = 0xC0 | 61;
        b[5] = llid as u8;
        b[4] = (llid >> 8) as u8;
        b[6] = 0x81;
        PduFrame {
            chain,
            nac: 0x8A1,
            at_ms,
            header: PduHeader::parse(b, true, None),
            blocks: vec![PduBlock { payload: vec![1; 12], serial: None, crc9_ok: None, errors: 0 }],
            blocks_expected: 1,
        }
    }

    #[test]
    fn records_are_counted_per_radio_once() {
        let mut d = DataState::default();
        assert!(d.add(to_record(&frame("control", 1_000, 0x1234, false), "clay")));
        // Another decoder read the same PDU: a duplicate.
        assert!(!d.add(to_record(&frame("lane 2", 1_100, 0x1234, false), "clay")));
        // The same radio again later, and another radio.
        assert!(d.add(to_record(&frame("control", 9_000, 0x1234, true), "clay")));
        assert!(d.add(to_record(&frame("control", 9_100, 0x5678, false), "clay")));
        assert!(d.add(to_record(&frame("control", 9_200, 0x1234, false), "fpl_clay")));
        let clay = d.counts(Some("clay"));
        assert_eq!((clay.pdus, clay.duplicates, clay.totals["ambtc"]), (3, 1, 3));
        assert_eq!(d.counts(Some("fpl_clay")).pdus, 1);
        assert_eq!(d.counts(None).pdus, 4);
        let r = &d.radios[&("clay".to_string(), 0x1234)];
        assert_eq!((r.packets, r.inbound, r.outbound, r.first_ms, r.last_ms), (2, 1, 1, 1_000, 9_000));
        let rec = &d.recent[0];
        assert_eq!((rec.format, rec.sap, rec.opcode, rec.bytes), ("ambtc", "trunking_control", Some(0), 12));
    }
}

//! Change 074: packet data. Collects the PDUs the decoders read:
//!
//! - the data decoders of the traffic chains, fed what the call gate
//!   holds back (a chain parked on the data channel between calls);
//! - the control channel (multi-block trunking messages).
//!
//! Each PDU becomes a record: which radio (LLID), which way, what (format,
//! SAP, SNDCP, IP / UDP and the Motorola service by port) and whether its
//! CRCs held. Kept in memory (recent records, totals per radio) and
//! logged (category "data"). The same PDU read by both chains (both
//! parked on the data channel) is counted once.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc;

use crate::protocol::p25::pdu::{self, PduFrame};
use crate::services::event_log::{EventLog, LogCategory};

/// Records kept.
pub const RECENT: usize = 500;

/// The data channel the active control channel announces (SNDCP data
/// channel announcement), Hz; 0 = not known yet. Written by the control
/// decoder, read by the follower (it parks an idle chain there).
static DATA_CHANNEL_HZ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn data_channel_hz() -> u64 {
    DATA_CHANNEL_HZ.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn set_data_channel_hz(hz: u64) {
    DATA_CHANNEL_HZ.store(hz, std::sync::atomic::Ordering::Relaxed);
}
/// A PDU seen again within this long (the other chain) is a duplicate.
const DUP_MS: u64 = 2_000;

#[derive(Debug, Clone, Serialize)]
pub struct DataRecord {
    pub at_ms: u64,
    pub site: String,
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
    /// Trellis bit errors per block (confirmed blocks).
    pub block_errors: Vec<u32>,
    /// Packet CRC-32 (packet data with blocks).
    pub crc_ok: Option<bool>,
    /// SNDCP data header (PDU type, NSAPI, IP / UDP compression).
    pub sndcp: Option<(u8, u8, u8, u8)>,
    pub ip: Option<pdu::IpInfo>,
    /// Application by UDP port (lrrp, ars, tms, ...).
    pub service: Option<&'static str>,
    /// User data octets.
    pub bytes: usize,
    /// AMBTC opcode / response octet.
    pub opcode: Option<u8>,
    pub response: Option<u8>,
    /// Up to 64 octets of the user data (or payload), hex.
    pub hex: String,
}

/// Totals for one radio (per site).
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
    /// Packets per service / SAP.
    pub kinds: BTreeMap<String, u64>,
    pub ip: Option<std::net::Ipv4Addr>,
}

#[derive(Debug, Default)]
pub struct DataState {
    pub recent: VecDeque<DataRecord>,
    pub radios: HashMap<(String, u32), RadioData>,
    /// Counts by kind ("packet/lrrp", "ambtc", "response", ...).
    pub totals: BTreeMap<String, u64>,
    pub pdus: u64,
    pub duplicates: u64,
}

pub type SharedData = Arc<Mutex<DataState>>;

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
        // (A response header has its response octet where others have
        // the SAP.)
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
            // Plain user / packet data carries IP (SAP 0 and 4).
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
    let mut s = format!("{} {} radio {} ({} blocks", kind(r), dir, r.llid, r.blocks);
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
    /// Add a record (unless it repeats one within `DUP_MS`). True if new.
    pub fn add(&mut self, r: DataRecord) -> bool {
        let dup = self.recent.iter().rev().take(16).any(|o| {
            r.at_ms.saturating_sub(o.at_ms) < DUP_MS && o.chain != r.chain && o.llid == r.llid && o.hex == r.hex && o.format == r.format
        });
        if dup {
            self.duplicates += 1;
            return false;
        }
        self.pdus += 1;
        *self.totals.entry(kind(&r)).or_default() += 1;
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
            // The radio's own address: the source of what it sends.
            e.ip = Some(if r.outbound { ip.dst } else { ip.src });
        }
        if self.recent.len() >= RECENT {
            self.recent.pop_front();
        }
        self.recent.push_back(r);
        true
    }
}

pub fn spawn_data_task(data: SharedData, event_log: Arc<EventLog>, mut rx: mpsc::Receiver<PduFrame>) {
    tokio::spawn(async move {
        while let Some(f) = rx.recv().await {
            let site = crate::services::lo_plan::active_site();
            let r = to_record(&f, &site);
            let line = summary(&r);
            let fields = serde_json::to_value(&r).unwrap_or_default();
            let new = data.lock().map(|mut d| d.add(r)).unwrap_or(false);
            if new {
                event_log.push(LogCategory::Data, line, fields);
            }
        }
    });
}

#[cfg(test)]
#[path = "data_task_tests.rs"]
mod tests;

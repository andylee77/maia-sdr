//! Host tests for `app::data_task` (change 074).

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
    assert!(d.add(to_record(&frame("data1", 1_000, 0x1234, false), "clay")));
    // The other chain read the same PDU: a duplicate.
    assert!(!d.add(to_record(&frame("data2", 1_100, 0x1234, false), "clay")));
    // The same radio again later, and another radio.
    assert!(d.add(to_record(&frame("data1", 9_000, 0x1234, true), "clay")));
    assert!(d.add(to_record(&frame("data1", 9_100, 0x5678, false), "clay")));
    assert_eq!((d.pdus, d.duplicates), (3, 1));
    let r = &d.radios[&("clay".to_string(), 0x1234)];
    assert_eq!((r.packets, r.inbound, r.outbound, r.first_ms, r.last_ms), (2, 1, 1, 1_000, 9_000));
    assert_eq!(d.totals["ambtc"], 3);
    let rec = &d.recent[0];
    assert_eq!((rec.format, rec.sap, rec.opcode, rec.bytes), ("ambtc", "trunking_control", Some(0), 12));
}

//! Host tests for `protocol::p25::framer`.

use super::*;
use crate::protocol::p25::fec::trellis_encode_bytes;
use crate::protocol::p25::test_fixtures::*;
use crate::protocol::p25::tsbk::ccitt80_crc;

/// `n` dibits of `bits`, most significant first.
fn unpack(bits: u64, n: usize) -> Vec<u8> {
    (0..n).rev().map(|i| ((bits >> (i * 2)) & 0x3) as u8).collect()
}

/// Frame sync and a NID for `nac`/`duid`, with `status` in the NID's status slot.
fn sync_and_nid(nac: u16, duid: u8, status: u8) -> Vec<u8> {
    let nid = unpack(bch::encode_nid(nac, duid), 32);
    let mut out = unpack(FRAME_SYNC_PATTERN, 24);
    out.extend_from_slice(&nid[..NID_STATUS_DIBIT_INDEX]);
    out.push(status);
    out.extend_from_slice(&nid[NID_STATUS_DIBIT_INDEX..]);
    out
}

/// A TSBK with its CRC filled in.
fn tsbk(mut bytes: [u8; 12]) -> [u8; 12] {
    let crc = ccitt80_crc(&bytes);
    bytes[10] = (crc >> 8) as u8;
    bytes[11] = crc as u8;
    bytes
}

/// A TSDU body: the blocks' trellis dibits, the null padding, and the status dibits.
fn tsdu_body(blocks: &[[u8; 12]]) -> Vec<u8> {
    let mut data: Vec<u8> = blocks.iter().flat_map(trellis_encode_bytes).collect();
    let len = TsduDeinterleaver::body_dibits_for_blocks(blocks.len()).unwrap();
    let mut body = Vec::with_capacity(len);
    data.resize(len, 0);
    let mut data = data.into_iter();
    for i in 0..len {
        body.push(if crate::protocol::p25::types::is_body_status_dibit(i) { 1 } else { data.next().unwrap() });
    }
    body
}

const NET_STS: [u8; 12] = [0x3B, 0x00, 0x00, 0xBE, 0xE0, 0x08, 0xA0, 0x06, 0x39, 0x00, 0, 0];
const RFSS_STS: [u8; 12] = [0xBA, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x06, 0x39, 0x00, 0, 0];

fn run(framer: &mut Framer, dibits: &[u8]) -> Vec<String> {
    let mut seen = Vec::new();
    for &d in dibits {
        framer.push(d, &mut |f| {
            seen.push(match f {
                Framed::Nid(n) => format!("nid {:03X} {:?}", n.nac, n.duid),
                Framed::Tsbk { index, opcode, .. } => format!("tsbk{index} {opcode:02X}"),
                other => format!("{other:?}").split([' ', '(']).next().unwrap().to_string(),
            })
        });
    }
    seen
}

#[test]
fn the_nid_status_dibit_is_skipped() {
    // A wrong value in the status slot must not reach the BCH codeword.
    let mut framer = Framer::default();
    assert_eq!(run(&mut framer, &sync_and_nid(CLAY_NAC, 0x7, 0x3)), ["nid 8A1 Tsdu"]);
    assert!(framer.is_assembling());
}

#[test]
fn a_tsdu_reads_on_until_the_last_block() {
    let mut framer = Framer::default();
    let mut dibits = sync_and_nid(CLAY_NAC, 0x7, 0);
    dibits.extend(tsdu_body(&[tsbk(NET_STS), tsbk(RFSS_STS)]));
    assert_eq!(run(&mut framer, &dibits), ["nid 8A1 Tsdu", "tsbk0 3B", "tsbk1 3A"]);
    assert_eq!((framer.stats.tsdus, framer.stats.tsbk_attempts(), framer.stats.tsbk_ok()), (1, 2, 2));
    assert!(!framer.is_assembling());
}

#[test]
fn a_last_block_flag_ends_the_tsdu() {
    let mut framer = Framer::default();
    let mut single = NET_STS;
    single[0] |= 0x80;
    let mut dibits = sync_and_nid(CLAY_NAC, 0x7, 0);
    dibits.extend(tsdu_body(&[tsbk(single)]));
    assert_eq!(run(&mut framer, &dibits), ["nid 8A1 Tsdu", "tsbk0 3B"]);
    assert_eq!(framer.stats.tsbk_attempts(), 1);
    assert!(!framer.is_assembling());
}

#[test]
fn a_failed_block_does_not_end_the_tsdu() {
    let mut framer = Framer::default();
    let mut broken = tsbk(NET_STS);
    broken[4] ^= 0xFF;
    let mut dibits = sync_and_nid(CLAY_NAC, 0x7, 0);
    dibits.extend(tsdu_body(&[broken, tsbk(RFSS_STS)]));
    assert_eq!(run(&mut framer, &dibits), ["nid 8A1 Tsdu", "tsbk1 3A"]);
    assert_eq!((framer.stats.tsbk_crc_failures, framer.stats.tsbk_ok()), (1, 1));
}

#[test]
fn a_nac_lock_drops_other_nacs_until_a_run_moves_it() {
    let mut framer = Framer::default();
    // TDUs: NID plus 15 dibits.
    let tdu = |nac| {
        let mut d = sync_and_nid(nac, 0x3, 0);
        d.extend([0; 15]);
        d
    };
    for _ in 0..3 {
        run(&mut framer, &tdu(0x0C5));
    }
    assert_eq!(framer.locked_nac(), 0x0C5);
    assert_eq!(run(&mut framer, &tdu(0x8A1)), Vec::<String>::new());
    assert_eq!(framer.stats.nid_nac_mismatch, 1);
    for _ in 2..NacTracker::RELOCK_AFTER {
        run(&mut framer, &tdu(0x8A1));
    }
    assert_eq!(run(&mut framer, &tdu(0x8A1)), ["nid 8A1 Tdu", "Tdu"]);
    assert_eq!(framer.stats.nac_relocks, 1);
    // A retune forgets the lock.
    framer.reset();
    assert_eq!(framer.locked_nac(), 0);
}

#[test]
fn scattered_wrong_nacs_do_not_move_the_lock() {
    let mut t = NacTracker::default();
    for _ in 0..5 {
        t.track(0x0C5);
    }
    for _ in 0..20 {
        assert!(!t.other_nac(0x123));
        assert!(!t.other_nac(0x456));
        t.track(0x0C5);
    }
    assert_eq!(t.dominant(), 0x0C5);
    for i in 1..=NacTracker::RELOCK_AFTER {
        assert_eq!(t.other_nac(0x8A1), i == NacTracker::RELOCK_AFTER);
    }
    assert_eq!(t.dominant(), 0);
}

#[test]
fn a_degenerate_nid_is_rejected_before_bch() {
    let mut framer = Framer::default();
    let mut dibits = unpack(FRAME_SYNC_PATTERN, 24);
    dibits.extend([1; 33]);
    assert!(run(&mut framer, &dibits).is_empty());
    assert_eq!(framer.stats.nid_entropy_rejected, 1);
}

#[test]
fn soft_sync_starts_a_nid() {
    let mut framer = Framer::default();
    framer.sync_detected();
    let nid = sync_and_nid(CLAY_NAC, 0x7, 0);
    assert_eq!(run(&mut framer, &nid[24..]), ["nid 8A1 Tsdu"]);
}

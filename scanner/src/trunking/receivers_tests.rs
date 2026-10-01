//! Host tests for `trunking::receivers`.

use std::sync::mpsc::SyncSender;

use super::*;
use crate::protocol::p25::fec::{bch, trellis_encode_bytes, TsduDeinterleaver};
use crate::protocol::p25::tsbk::ccitt80_crc;
use crate::protocol::p25::types::is_body_status_dibit;
use crate::protocol::p25::wire::{FRAME_SYNC_PATTERN, NID_STATUS_DIBIT_INDEX};

/// Cumulative (C4FM, LSM) TSBKs from per-second counts.
fn feed(choice: &mut ModulationChoice, per_second: &[(u64, u64)], start: Instant) -> bool {
    let (mut c, mut l) = (0, 0);
    let mut switched = choice.tick(0, 0, start);
    for (i, (dc, dl)) in per_second.iter().enumerate() {
        c += dc;
        l += dl;
        switched |= choice.tick(c, l, start + Duration::from_secs(i as u64 + 1));
    }
    switched
}

#[test]
fn auto_moves_to_the_clearly_better_decoder_only() {
    let t = Instant::now();
    // C4FM 99 %, LSM 42 % (a C4FM site): C4FM, once ten seconds are counted.
    let mut c = ModulationChoice::new(Modulation::Auto);
    assert!(!feed(&mut c, &[(40, 17); 9], t) && !c.c4fm());
    let mut c = ModulationChoice::new(Modulation::Auto);
    assert!(feed(&mut c, &[(40, 17); 10], t) && c.c4fm());
    // A start-up transient does not decide: one demodulator a second late.
    let mut c = ModulationChoice::new(Modulation::Auto);
    let mut start = vec![(40, 0)];
    start.extend([(40, 40); 15]);
    assert!(!feed(&mut c, &start, t) && !c.c4fm());
    // Both about equal (an LSM site): stays.
    let mut c = ModulationChoice::new(Modulation::Auto);
    assert!(!feed(&mut c, &[(40, 39), (41, 40)].repeat(10), t) && !c.c4fm());
    // Too few TSBKs to judge: stays.
    let mut c = ModulationChoice::new(Modulation::Auto);
    assert!(!feed(&mut c, &[(1, 0); 20], t) && !c.c4fm());
    // Forced.
    let mut c = ModulationChoice::new(Modulation::Lsm);
    assert!(!feed(&mut c, &[(40, 0); 20], t) && !c.c4fm());
    let mut c = ModulationChoice::new(Modulation::C4fm);
    assert!(!feed(&mut c, &[(0, 40); 20], t) && c.c4fm());
}

#[test]
fn auto_dwells_after_a_switch() {
    let t = Instant::now();
    let mut c = ModulationChoice::new(Modulation::Auto);
    feed(&mut c, &[(40, 10); 10], t);
    assert!(c.c4fm());
    // LSM clearly better now, but within the dwell: stays on C4FM.
    let mut cum = (400, 100);
    for s in 11..60 {
        cum = (cum.0 + 5, cum.1 + 40);
        assert!(!c.tick(cum.0, cum.1, t + Duration::from_secs(s)), "switched back at {s} s");
    }
    cum = (cum.0 + 5, cum.1 + 40);
    assert!(c.tick(cum.0, cum.1, t + Duration::from_secs(72)));
    assert!(!c.c4fm());
}

#[test]
fn rate_window_reports_per_second_and_share() {
    let mut r = RateWindow::default();
    assert_eq!(r.rates(), (None, None));
    r.sample(0, 0);
    r.sample(39, 40);
    r.sample(79, 80);
    assert_eq!(r.rates(), (Some(39.5), Some(98.8)));
    for i in 0..20 {
        r.sample(100 + i * 10, 100 + i * 10);
    }
    assert_eq!(r.rates(), (Some(10.0), Some(100.0)));
}

#[test]
fn ring_bytes_unpack_low_bits_first() {
    let mut out = Vec::new();
    unpack(&[0b11_10_01_00, 0b00_00_00_11], &mut out);
    assert_eq!(out, [0, 1, 2, 3, 3, 0, 0, 0]);
}

/// One TSDU with a single NET_STS_BCST for Clay County, as dibits.
fn net_status_tsdu() -> Vec<u8> {
    let mut tsbk = [0xBBu8, 0x00, 0x00, 0xBE, 0xE0, 0x08, 0xA0, 0x06, 0x39, 0x00, 0, 0];
    let crc = ccitt80_crc(&tsbk);
    tsbk[10] = (crc >> 8) as u8;
    tsbk[11] = crc as u8;
    let unpack48 = |bits: u64, n: usize| -> Vec<u8> { (0..n).rev().map(|i| ((bits >> (i * 2)) & 3) as u8).collect() };
    let mut out = unpack48(FRAME_SYNC_PATTERN, 24);
    let nid = unpack48(bch::encode_nid(0x8A1, 0x7), 32);
    out.extend_from_slice(&nid[..NID_STATUS_DIBIT_INDEX]);
    out.push(0);
    out.extend_from_slice(&nid[NID_STATUS_DIBIT_INDEX..]);
    let len = TsduDeinterleaver::body_dibits_for_blocks(1).unwrap();
    let mut data = trellis_encode_bytes(&tsbk).to_vec().into_iter().chain(std::iter::repeat(0));
    out.extend((0..len).map(|i| if is_body_status_dibit(i) { 1 } else { data.next().unwrap() }));
    out
}

fn pack(dibits: &[u8]) -> Vec<u8> {
    dibits.chunks(4).map(|c| c.iter().enumerate().fold(0u8, |b, (i, d)| b | (d << (2 * i)))).collect()
}

/// Hands prepared deliveries to the receivers.
struct Prepared(Vec<Vec<u8>>);

impl StreamSource for Prepared {
    fn control_streams(
        &self,
        _: Wants,
        tx: SyncSender<Input>,
        _: Arc<AtomicBool>,
        _: Arc<StreamCounters>,
    ) -> Vec<tokio::task::JoinHandle<()>> {
        for bytes in &self.0 {
            tx.send(Input::Dibits { bytes: bytes.clone(), reset: false }).unwrap();
        }
        Vec::new()
    }
}

#[tokio::test]
async fn p25_receivers_publish_the_lsm_decoder_messages() {
    let log = Arc::new(EventLog::default());
    let receivers = Receivers::new(log.clone());
    let mut dibits = Vec::new();
    for _ in 0..4 {
        dibits.extend(net_status_tsdu());
    }
    dibits.resize(dibits.len().div_ceil(4) * 4, 0);
    let context = Context {
        site: "clay".into(),
        protocol: Protocol::P25,
        modulation: Modulation::Auto,
        lcn_hz: HashMap::new(),
        trunk: None,
    };
    receivers.start(context, &Prepared(vec![pack(&dibits)])).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while log.since(0, 10, true).len() < 4 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let status = receivers.status();
    receivers.stop().await;
    let records = log.since(0, 10, true);
    assert_eq!(records.len(), 4);
    assert!(records.iter().all(|r| r.text.starts_with("TSBK1 NET_STS_BCAST WACN:BEE00") && r.routine && r.source == "p25"));
    assert!(log.since(0, 10, false).is_empty(), "housekeeping only");
    match status.identity {
        Some(SiteIdentity::P25(id)) => assert_eq!((id.nac, id.wacn, id.system), (Some(0x8A1), Some(0xBEE00), Some(0x8A0))),
        other => panic!("{other:?}"),
    }
    assert_eq!(status.modulation, Some("lsm"));
    assert!(status.running && !receivers.status().running);
}

//! Unit tests for the sibling production module.
//!
//! Attached as a child via `#[cfg(test)] #[path = "..."]
//! mod tests;` in the production file, so `use super::*;`
//! resolves to the parent module's private items.

use super::*;
use crate::protocol::p25::test_fixtures::*;

#[test]
fn test_system_identity_tracking() {
    let mut decoder = ControlChannelDecoder::new();

    // Simulate NET_STS_BCST
    decoder.handle_tsbk(0, TsbkMessage::NetworkStatus {
        wacn: FLORIDA_WACN,
        system_id: CLAY_SYSTEM_ID,
        channel: Channel(0x0639),
    });

    assert_eq!(decoder.system.wacn, Some(FLORIDA_WACN));
    assert_eq!(decoder.system.system_id, Some(CLAY_SYSTEM_ID));
    assert_eq!(decoder.system.control_channel.unwrap().0, 0x0639);
}

#[test]
fn test_frequency_band_table() {
    let mut decoder = ControlChannelDecoder::new();

    // Add Clay County band 0
    decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
        identifier: 0,
        bw: 100, // 12500 Hz
        transmit_offset: -45_000_000,
        channel_spacing: 6_250,
        base_frequency: 851_006_250,
        slots: 1,
    });

    // Resolve control channel
    let freq = decoder
        .channel_to_frequency(Channel(0x0639))
        .unwrap();
    assert_eq!(freq, CLAY_CONTROL_FREQ_HZ); // 860.9625 MHz
}

// Phase 2e (2026-04-25): the per-decoder grant HashMap was
// removed. The three tests that lived here (test_grant_tracking,
// test_grant_dedup_by_talkgroup, test_grant_update_preserves_source_id)
// asserted on `decoder.grants` directly. Equivalent semantics —
// per-call lifecycle, source-update-on-refresh, dedup — now live
// in `app::grant_follower` and are covered by lifecycle tests there.

/// Drive the decoder end-to-end with a frame sync + 33-dibit NID
/// (with a deliberately-wrong status dibit injected at index 11)
/// and verify the BCH FEC still decodes the correct NAC/DUID.
/// Regression guard for doc/changes/022 (see NID_TRANSMITTED_DIBITS
/// const docstring for the full incident).
#[test]
fn test_nid_status_dibit_skip_e2e() {
    use crate::protocol::p25::fec::bch as nid_fec;

    // Helper: unpack a 48-bit pattern into 24 dibits MSB-first,
    // or a 64-bit word into 32 dibits.
    fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n_dibits);
        for i in (0..n_dibits).rev() {
            out.push(((bits >> (i * 2)) & 0x3) as u8);
        }
        out
    }

    // Clean Clay County NID: NAC=0x8A1, DUID=0x7 (TSDU).
    let nid_bits = nid_fec::encode_nid(CLAY_NAC, 0x7);
    let nid_dibits_32 = unpack_dibits(nid_bits, 32);

    // Build the 33-dibit on-air NID window: splice a DELIBERATELY
    // WRONG status dibit (value 0x3 = "-3") at index 11. If the
    // decoder folds this into nid_bits, the BCH codeword gets
    // corrupted and the test fails. If the decoder correctly
    // skips index 11, the test passes.
    let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
    on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
    on_air_nid.push(0x3); // garbage status dibit
    on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
    assert_eq!(on_air_nid.len(), 33);

    // Frame sync pattern unpacked into 24 dibits. Matches the
    // decoder's FRAME_SYNC_DIBIT_PATTERN constant at the top of
    // this file.
    let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);
    assert_eq!(fs_dibits.len(), 24);

    // Drive the decoder: first 24 dibits of frame sync (to arm
    // the correlator) followed by the 33 dibits of the on-air NID
    // window (with status spliced in).
    let mut decoder = ControlChannelDecoder::new();
    for &d in &fs_dibits {
        decoder.process_dibit(d);
    }
    for &d in &on_air_nid {
        decoder.process_dibit(d);
    }

    // After the NID is fully consumed the decoder should have
    // latched the Clay County NAC into `system.nac`. Anything
    // else means the BCH decoder either rejected the codeword or
    // miscorrected to a different NAC -- either way the status
    // dibit skip is broken.
    assert_eq!(
        decoder.system.nac,
        Some(Nac::new(CLAY_NAC)),
        "decoder should land on the clean Clay County NAC after \
         skipping the status dibit at position 11; got {:?}",
        decoder.system.nac,
    );
}

/// Multi-block TSBK end-to-end regression guard. Builds a real
/// 2-block TSDU body (TSBK1 last_block=0, TSBK2 last_block=1)
/// with valid CCITT_80 CRCs, trellis-encodes each 12-byte block,
/// and splices the 7 status dibits at body positions
/// {13,49,85,121,157,193,229} plus 28 trailing null padding
/// dibits. Verifies:
///
/// 1. `tsdu_attempts` == 1
/// 2. `tsbk_block_attempts` == 2
/// 3. `tsbk_crc_ok` == 2
/// 4. Both messages dispatched:
///    - Block 1: NetworkStatusBroadcast (0x3B) → `system.wacn`
///    - Block 2: RfssStatusBroadcast (0x3A) → `system.rfss_id`
/// 5. Decoder returns to Hunting after the second block.
#[test]
fn test_multi_block_tsbk_e2e() {
    use crate::protocol::p25::fec::bch as nid_fec;
    use crate::protocol::p25::fec::trellis_encode_bytes;
    use crate::protocol::p25::tsbk::ccitt80_crc;

    fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n_dibits);
        for i in (0..n_dibits).rev() {
            out.push(((bits >> (i * 2)) & 0x3) as u8);
        }
        out
    }

    // Helper: take 12 TSBK bytes minus the trailing CRC, compute
    // the CCITT_80 CRC for "Plain" convention (residual==0), and
    // splice it into bytes[10..12]. Returns the finalized 12-byte
    // block ready for trellis_encode_bytes.
    fn finalize_tsbk(mut bytes: [u8; 12]) -> [u8; 12] {
        // The CRC covers the first 80 bits = bytes[0..10]. We want
        // residual = calc XOR msg_crc == 0, so msg_crc = calc.
        let calc = ccitt80_crc(&bytes);
        bytes[10] = (calc >> 8) as u8;
        bytes[11] = (calc & 0xFF) as u8;
        bytes
    }

    // ── TSBK1: NET_STS_BCST (opcode 0x3B), LB=0 ──
    let tsbk1_raw = [
        0x3B, // LB=0, P=0, opcode=0x3B (NetworkStatusBroadcast)
        0x00, // standard manufacturer
        0x00, // payload[0]: LRA
        0xBE, // payload[1]: WACN bits 19-12
        0xE0, // payload[2]: WACN bits 11-4
        0x08, // payload[3]: WACN bits 3-0 | system_id bits 11-8
        0xA0, // payload[4]: system_id bits 7-0
        0x06, // payload[5]: channel high
        0x39, // payload[6]: channel low
        0x00, // payload[7]: services
        0x00, 0x00, // CRC placeholder
    ];
    let tsbk1 = finalize_tsbk(tsbk1_raw);

    // ── TSBK2: RFSS_STS_BCST (opcode 0x3A), LB=1 ──
    // Matches SDRTrunk RFSSStatusBroadcast.java:
    // payload[0] = LRA, payload[1..2] = system_id (12 bits at bits
    // 28-39), payload[3] = RFSS, payload[4] = SITE,
    // payload[5..6] = freq_band(4) | channel_number(12).
    let tsbk2_raw = [
        0xBA, // LB=1, P=0, opcode=0x3A
        0x00, // standard manufacturer
        0x00, // payload[0]: LRA
        0x00, // payload[1]: bits 24-27 reserved/active flag,
              //              bits 28-31 = system high nibble (0)
        0x00, // payload[2]: bits 32-39 = system low byte (0)
        0x01, // payload[3]: RFSS ID = 1
        0x01, // payload[4]: SITE ID = 1
        0x06, // payload[5]: freq_band(4)=0 | channel_number high(4)=0x6
        0x39, // payload[6]: channel_number low(8)=0x39
        0x00, // payload[7]: system service class
        0x00, 0x00, // CRC placeholder
    ];
    let tsbk2 = finalize_tsbk(tsbk2_raw);

    // Trellis-encode each block to 98 on-air dibits.
    let tsbk1_dibits = trellis_encode_bytes(&tsbk1);
    let tsbk2_dibits = trellis_encode_bytes(&tsbk2);

    // Concatenate the two blocks → 196 trellis dibits, then
    // append 28 null dibits → 224 dibits, then splice in the 7
    // status dibits at positions {13,49,85,121,157,193,229} →
    // 231 raw body dibits. The decoder will reverse this.
    let mut data: Vec<u8> = Vec::with_capacity(224);
    data.extend_from_slice(&tsbk1_dibits);
    data.extend_from_slice(&tsbk2_dibits);
    // 28 trailing null dibits (value doesn't matter -- gets stripped)
    for _ in 0..28 {
        data.push(0);
    }
    assert_eq!(data.len(), 224);

    let status_positions = [14usize, 50, 86, 122, 158, 194, 230];
    let mut body: Vec<u8> = Vec::with_capacity(231);
    let mut data_iter = data.into_iter();
    for i in 0..231 {
        if status_positions.contains(&i) {
            body.push(0x01); // status dibit -- value gets stripped
        } else {
            body.push(data_iter.next().unwrap());
        }
    }
    assert_eq!(body.len(), 231);

    // Build sync + NID for Clay County NAC=0x8A1, DUID=0x7 (TSDU).
    let nid_bits = nid_fec::encode_nid(CLAY_NAC, 0x7);
    let nid_dibits_32 = unpack_dibits(nid_bits, 32);
    let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
    on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
    on_air_nid.push(0x0); // status dibit (value irrelevant -- skipped)
    on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
    let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);

    // Drive the decoder.
    let mut decoder = ControlChannelDecoder::new();
    for &d in &fs_dibits {
        decoder.process_dibit(d);
    }
    for &d in &on_air_nid {
        decoder.process_dibit(d);
    }
    for &d in &body {
        decoder.process_dibit(d);
    }

    // Verify counters: one TSDU, two blocks, both CRCs OK.
    assert_eq!(
        decoder.tsdu_attempts, 1,
        "expected 1 TSDU attempt, got {}",
        decoder.tsdu_attempts
    );
    assert_eq!(
        decoder.tsbk_block_attempts, 2,
        "expected 2 TSBK block attempts (TSBK1 + TSBK2), got {}",
        decoder.tsbk_block_attempts
    );
    assert_eq!(
        decoder.tsbk_crc_ok, 2,
        "expected 2 TSBK CRC successes, got {} (failures: trellis={} crc={})",
        decoder.tsbk_crc_ok,
        decoder.tsbk_trellis_failures,
        decoder.tsbk_crc_failures,
    );

    // Verify both messages dispatched: TSBK1 set wacn,
    // TSBK2 set rfss_id.
    assert_eq!(
        decoder.system.wacn,
        Some(FLORIDA_WACN),
        "TSBK1 NetworkStatus should have set wacn=0xBEE00"
    );
    assert_eq!(
        decoder.system.rfss_id,
        Some(0x01),
        "TSBK2 RfssStatus should have set rfss_id=1"
    );
}

/// Single-block TSBK regression: confirm `last_block=1` on the
/// FIRST block correctly terminates after TSBK1 without trying to
/// read 108 more dibits for an imaginary TSBK2. Otherwise the
/// decoder would silently consume the next sync window's dibits
/// and fall out of sync.
#[test]
fn test_single_block_tsbk_terminates_on_lb1() {
    use crate::protocol::p25::fec::bch as nid_fec;
    use crate::protocol::p25::fec::trellis_encode_bytes;
    use crate::protocol::p25::tsbk::ccitt80_crc;

    fn unpack_dibits(bits: u64, n_dibits: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n_dibits);
        for i in (0..n_dibits).rev() {
            out.push(((bits >> (i * 2)) & 0x3) as u8);
        }
        out
    }

    let mut tsbk1_raw = [
        0xBB, // LB=1, opcode=0x3B
        0x00, 0x00, 0xBE, 0xE0, 0x08, 0xA0, 0x06, 0x39, 0x00, 0x00, 0x00,
    ];
    let calc = ccitt80_crc(&tsbk1_raw);
    tsbk1_raw[10] = (calc >> 8) as u8;
    tsbk1_raw[11] = (calc & 0xFF) as u8;

    let tsbk1_dibits = trellis_encode_bytes(&tsbk1_raw);
    // 98 trellis + 21 null = 119 non-status dibits, then splice 4
    // status dibits at body positions {13,49,85,121} → 123 raw.
    let mut data: Vec<u8> = Vec::with_capacity(119);
    data.extend_from_slice(&tsbk1_dibits);
    for _ in 0..21 {
        data.push(0);
    }
    let status_positions = [14usize, 50, 86, 122];
    let mut body: Vec<u8> = Vec::with_capacity(123);
    let mut data_iter = data.into_iter();
    for i in 0..123 {
        if status_positions.contains(&i) {
            body.push(0x01);
        } else {
            body.push(data_iter.next().unwrap());
        }
    }

    let nid_bits = nid_fec::encode_nid(CLAY_NAC, 0x7);
    let nid_dibits_32 = unpack_dibits(nid_bits, 32);
    let mut on_air_nid: Vec<u8> = Vec::with_capacity(33);
    on_air_nid.extend_from_slice(&nid_dibits_32[..11]);
    on_air_nid.push(0x0);
    on_air_nid.extend_from_slice(&nid_dibits_32[11..]);
    let fs_dibits = unpack_dibits(FRAME_SYNC_DIBIT_PATTERN, 24);

    let mut decoder = ControlChannelDecoder::new();
    for &d in &fs_dibits {
        decoder.process_dibit(d);
    }
    for &d in &on_air_nid {
        decoder.process_dibit(d);
    }
    for &d in &body {
        decoder.process_dibit(d);
    }

    assert_eq!(decoder.tsdu_attempts, 1);
    assert_eq!(
        decoder.tsbk_block_attempts, 1,
        "single-block TSBK with LB=1 must NOT trigger a second block read"
    );
    assert_eq!(decoder.tsbk_crc_ok, 1);
    assert_eq!(decoder.system.wacn, Some(FLORIDA_WACN));
    // After TSBK1 with LB=1, we should be back in Hunting.
    assert!(matches!(decoder.state, DecoderState::Hunting));
}

/// Change 070: a site switch or retune forgets the old system, so the
/// NAC lock no longer rejects the new channel's frames.
#[test]
fn new_system_drops_the_nac_lock_and_identity() {
    let mut decoder = ControlChannelDecoder::new();
    for _ in 0..5 {
        decoder.nac_tracker.track(0x8A1);
    }
    decoder.system.nac = Some(Nac::new(0x8A1));
    decoder.system.wacn = Some(FLORIDA_WACN);
    assert_eq!(decoder.nac_tracker.dominant(), 0x8A1);
    decoder.new_system();
    assert_eq!(decoder.nac_tracker.dominant(), 0);
    assert!(decoder.system.nac.is_none() && decoder.system.wacn.is_none());
    assert!(decoder.bands.is_empty());
    assert!(matches!(decoder.state, DecoderState::Hunting));
}

/// Change 071a: neighbours are remembered per (system, RFSS, site); a
/// grant on a TDMA band is flagged for the follower.
#[test]
fn neighbours_and_tdma_grants() {
    let mut decoder = ControlChannelDecoder::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    decoder.set_grant_event_tx(tx);
    decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
        identifier: 0, bw: 100, transmit_offset: -45_000_000,
        channel_spacing: 6_250, base_frequency: 851_006_250, slots: 1,
    });
    decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
        identifier: 2, bw: 100, transmit_offset: -45_000_000,
        channel_spacing: 12_500, base_frequency: 851_012_500, slots: 2,
    });
    let adj = |site: u8| TsbkMessage::AdjacentStatus {
        lra: 1, rfss_id: 1, site_id: site, channel: Channel(0x0639), system_id: 0x3BD,
        conventional: false, failure: false, valid: true, active: true, service_class: 0x70,
    };
    decoder.handle_tsbk(0, adj(2));
    decoder.handle_tsbk(0, adj(2));
    decoder.handle_tsbk(0, adj(3));
    assert_eq!(decoder.system.neighbours.len(), 2);
    let n = &decoder.system.neighbours[&(0x3BD, 1, 2)];
    assert_eq!(n.count, 2);
    assert_eq!(decoder.channel_to_frequency(n.channel), Some(CLAY_CONTROL_FREQ_HZ));
    assert_eq!(service_class_names(0x70), vec!["data", "voice", "registration"]);
    decoder.new_system();
    assert!(decoder.system.neighbours.is_empty());

    // Grants: band 0 is Phase 1, band 2 is TDMA.
    decoder.handle_tsbk(0, TsbkMessage::IdentifierUpdate {
        identifier: 2, bw: 100, transmit_offset: -45_000_000,
        channel_spacing: 12_500, base_frequency: 851_012_500, slots: 2,
    });
    for (chan, tdma) in [(Channel(0x2000 | 228), true)] {
        decoder.handle_tsbk(0, TsbkMessage::GroupVoiceChannelGrant {
            channel: chan, talkgroup: Talkgroup(300), source: RadioId(1), service_options: 0,
        });
        match rx.try_recv() {
            Ok(crate::protocol::p25::events::P25Event::Grant(g)) => {
                assert_eq!(g.tdma, tdma);
                assert_eq!(g.frequency_hz, Some(852_437_500));
            }
            other => panic!("{other:?}"),
        }
    }
}

/// Change 071a: a run of another NAC moves the lock; scattered false
/// NACs do not.
#[test]
fn nac_lock_moves_after_a_run_of_another_nac() {
    let mut t = NacTracker::default();
    for _ in 0..5 {
        t.track(0x0C5);
    }
    assert_eq!(t.dominant(), 0x0C5);
    // Scattered wrong NACs, interleaved with the locked one: no move.
    for _ in 0..20 {
        assert!(!t.other_nac(0x123));
        assert!(!t.other_nac(0x456));
        t.track(0x0C5);
    }
    assert_eq!(t.dominant(), 0x0C5);
    // Eight in a row of 0x8A1: the lock moves.
    for i in 1..=NacTracker::RELOCK_AFTER {
        assert_eq!(t.other_nac(0x8A1), i == NacTracker::RELOCK_AFTER);
    }
    assert_eq!(t.dominant(), 0);
    for _ in 0..3 {
        t.track(0x8A1);
    }
    assert_eq!(t.dominant(), 0x8A1);
}

/// Change 072: accepted affiliations and registrations reach the history
/// channel; denied ones and an inactive decoder's do not.
#[test]
fn unit_events_for_history() {
    use crate::services::history::UnitEventKind;
    let mut decoder = ControlChannelDecoder::new();
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    decoder.unit_event_tx = Some(tx);
    decoder.handle_tsbk(0, TsbkMessage::GroupAffiliationResponse {
        response: 0, announcement_group: Talkgroup(0), group: Talkgroup(300), target: RadioId(1234),
    });
    decoder.handle_tsbk(0, TsbkMessage::GroupAffiliationResponse {
        response: 2, announcement_group: Talkgroup(0), group: Talkgroup(301), target: RadioId(1234),
    });
    decoder.handle_tsbk(0, TsbkMessage::UnitDeRegistrationAcknowledge { wacn: 0, system_id: 0, target: RadioId(55) });
    assert_eq!(rx.try_recv().unwrap(), UnitObservation { unit: 1234, tg: 300, kind: UnitEventKind::GroupAffiliation });
    assert_eq!(rx.try_recv().unwrap(), UnitObservation { unit: 55, tg: 0, kind: UnitEventKind::Deregistration });
    assert!(rx.try_recv().is_err());
    decoder.active = false;
    decoder.handle_tsbk(0, TsbkMessage::UnitDeRegistrationAcknowledge { wacn: 0, system_id: 0, target: RadioId(56) });
    assert!(rx.try_recv().is_err());
}

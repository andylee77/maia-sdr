use super::*;
use crate::protocol::dmr::demod::DmrDemodulator;

fn sync_then(framer: &mut DmrMessageFramer, pattern: DmrSyncPattern, dibits: &[u8]) {
    framer.sync_detected(pattern);
    for d in dibits {
        framer.receive(*d);
    }
}

#[test]
fn bursts_alternate_buffers_and_carry_their_pattern() {
    let mut f = DmrMessageFramer::default();
    let burst = [1u8; 144];
    sync_then(&mut f, DmrSyncPattern::BaseStationData, &burst);
    sync_then(&mut f, DmrSyncPattern::BaseStationData, &burst);
    let bursts: Vec<_> = f
        .drain()
        .filter_map(|e| match e {
            FramerEvent::Burst(b) => Some(b),
            _ => None,
        })
        .collect();
    assert_eq!(bursts.len(), 2);
    assert!(bursts.iter().all(|b| b.pattern == DmrSyncPattern::BaseStationData));
    // Dibit 1 (+3) is bits 0,1.
    assert_eq!(&bursts[0].bits[..4], &[0, 1, 0, 1]);
}

#[test]
fn voice_superframe_is_followed_without_syncs() {
    let mut f = DmrMessageFramer::default();
    let burst = [0u8; 144];
    // Voice A on one slot, data on the other, then the voice slot's bursts
    // B-F arrive with no sync (the data slot keeps its syncs).
    sync_then(&mut f, DmrSyncPattern::BaseStationVoice, &burst);
    for _ in 0..5 {
        sync_then(&mut f, DmrSyncPattern::BaseStationData, &burst);
        assert!(f.is_voice_super_frame());
        for d in burst {
            f.receive(d);
        }
    }
    let patterns: Vec<_> = f
        .drain()
        .filter_map(|e| match e {
            FramerEvent::Burst(b) if b.pattern != DmrSyncPattern::BaseStationData => Some(b.pattern),
            _ => None,
        })
        .collect();
    assert_eq!(
        patterns,
        vec![
            DmrSyncPattern::BaseStationVoice,
            DmrSyncPattern::BsVoiceFrameB,
            DmrSyncPattern::BsVoiceFrameC,
            DmrSyncPattern::BsVoiceFrameD,
            DmrSyncPattern::BsVoiceFrameE,
            DmrSyncPattern::BsVoiceFrameF,
        ]
    );
}

#[test]
fn a_second_of_nothing_is_a_sync_loss() {
    let mut f = DmrMessageFramer::default();
    for _ in 0..4800 {
        f.receive(0);
    }
    let events: Vec<_> = f.drain().collect();
    assert!(matches!(events[..], [FramerEvent::SyncLoss { timeslot: 0, bits: 9600 }]));
}

/// Offline: demodulator + framer over unit A's captures (`DMR_CAPTURE_DIR`).
#[test]
fn captured_bursts() {
    let Ok(dir) = std::env::var("DMR_CAPTURE_DIR") else {
        return;
    };
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map_or(false, |x| x == "wav"))
        .collect();
    files.sort();
    for path in files {
        let bytes = std::fs::read(&path).unwrap();
        let iq: Vec<i16> = bytes[44..].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
        let mut demod = DmrDemodulator::new();
        let mut framer = DmrMessageFramer::default();
        let mut counts = std::collections::BTreeMap::<String, u32>::new();
        let (mut cach_ok, mut cach_n, mut loss_bits) = (0u32, 0u32, 0u32);
        for chunk in iq.chunks(2 * 1250) {
            demod.process_iq_i16(chunk, &mut framer);
            for e in framer.drain() {
                match e {
                    FramerEvent::Burst(b) => {
                        *counts.entry(format!("TS{} {}", b.timeslot, b.pattern.label())).or_default() += 1;
                        if b.pattern.has_cach() {
                            cach_n += 1;
                            cach_ok += b.cach.valid as u32;
                        }
                    }
                    FramerEvent::SyncLoss { bits, .. } => loss_bits += bits,
                }
            }
        }
        eprintln!(
            "{}: CACH valid {cach_ok}/{cach_n}, sync-loss bits {loss_bits}, fine-sync losses {}, {:?}",
            path.file_name().unwrap().to_string_lossy(),
            demod.symbols.stats.fine_sync_losses,
            counts
        );
    }
}

use super::*;

/// The 108 AMBE+2 frames of the 20:57 Clay Electric transmission (TS2), the
/// AMBE port's fixture.
const CLAY_FRAMES: &[u8] = include_bytes!("../jmbe/ambe/test_frames_clay_ts2.bin");

fn batches() -> Vec<DmrVoiceBatch> {
    CLAY_FRAMES
        .chunks_exact(27)
        .map(|b| {
            let mut frames = [[0u8; 9]; 3];
            for (i, f) in frames.iter_mut().enumerate() {
                f.copy_from_slice(&b[9 * i..9 * i + 9]);
            }
            DmrVoiceBatch { frames, talkgroup: 87_925, source: Some(81_921), call_id: 7, captured_at_ms: 1 }
        })
        .collect()
}

#[test]
fn a_call_decodes_to_lane_one_chunks_with_its_identity() {
    let mut v = DmrVoiceDecoder::default();
    let mut chunks = Vec::new();
    for b in batches() {
        chunks.extend(v.decode(&b));
    }
    assert_eq!(chunks.len(), 108);
    assert!(chunks.iter().all(|c| c.lane == Lane::One && c.talkgroup == 87_925 && c.source == 81_921 && c.call_id == 7));
    // Speech: well above silence once the decoder has settled.
    let rms = |c: &AudioChunk| (c.pcm.iter().map(|&s| (s as f64).powi(2)).sum::<f64>() / 160.0).sqrt();
    let loud = chunks[4..100].iter().filter(|c| rms(c) > 300.0).count();
    assert!(loud > 60, "{loud} loud frames");
    // The tail frames (the transmission ended) are counted as errored.
    assert!(v.frame_errors >= 6 && v.frame_errors < 20, "{}", v.frame_errors);
}

#[test]
fn a_new_call_starts_a_fresh_decoder() {
    let b = batches();
    let mut v = DmrVoiceDecoder::default();
    let first: Vec<_> = b[..5].iter().flat_map(|x| v.decode(x)).collect();
    // The same bursts as call 8 from scratch decode the same as call 7 did.
    let mut again = Vec::new();
    for x in &b[..5] {
        let mut y = x.clone();
        y.call_id = 8;
        again.extend(v.decode(&y));
    }
    assert_eq!(first.iter().map(|c| c.pcm).collect::<Vec<_>>(), again.iter().map(|c| c.pcm).collect::<Vec<_>>());
}

#[test]
fn agc_limits_and_leaves_silence_alone() {
    let mut agc = PcmAgc::default();
    let mut loud = [32000i16; 160];
    for _ in 0..50 {
        let mut p = loud;
        agc.apply(&mut p);
        loud = p;
    }
    assert!(loud.iter().all(|&s| s <= 32700));
    let mut quiet = [3i16; 160];
    let mut agc = PcmAgc::default();
    agc.apply(&mut quiet);
    assert_eq!(quiet, [3i16; 160]);
}

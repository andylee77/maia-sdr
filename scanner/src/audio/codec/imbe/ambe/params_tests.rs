//! Unit tests for `params.rs`.

use super::*;
use crate::audio::codec::imbe::ambe::frame::tests::{capture_frames, flip};

#[test]
fn block_lengths_sum_to_l() {
    // decodePRBAVector fills T[1..=L] from the four blocks.
    for l in 9..=56 {
        let j = LMPR_BLOCK_LENGTH[l];
        assert_eq!(j[1] + j[2] + j[3] + j[4], l, "L {l}");
    }
}

#[test]
fn voicing_band_index_stays_below_eight() {
    // jmbe throws past band 7; check the clamp in set_voicing_decisions
    // never applies for a voice fundamental.
    for (b0, &(_, l, frame_type)) in FUNDAMENTAL.iter().enumerate() {
        if frame_type == FrameType::Voice {
            let w0 = fundamental_w0(b0);
            let index = ((l as f32 * w0) * 16.0f32 / TWO_PI) as usize;
            assert!(index <= 7, "b0 {b0}: band {index}");
        }
    }
}

#[test]
fn default_frame_is_w124_with_voice_defaults() {
    let p = ModelParameters::new_default();
    assert_eq!((p.b0, p.l, p.frame_type), (124, 15, FrameType::Voice));
    // jmbe's W124: (float)(PI / 32.0 * 2.0 * PI).
    assert_eq!(
        p.w0,
        (core::f64::consts::PI / 32.0 * 2.0 * core::f64::consts::PI) as f32
    );
    assert_eq!(p.enhanced, vec![1.0; 16]);
    assert!(!p.has_voiced_bands());
    assert_eq!(p.unvoiced_band_count(), 16);
}

#[test]
fn bad_c0_repeats_the_previous_frame_sharing_its_voicing() {
    let frames = capture_frames();
    let mut previous = ModelParameters::new_default();
    for f in &frames[60..64] {
        let p = ModelParameters::from_frame(&AmbeFrame::decode(f), &mut previous);
        previous = p;
    }
    let mut bad = frames[64];
    for b in [0, 1, 2, 3] {
        flip(&mut bad, 0, b);
        flip(&mut bad, 1, b + 5);
    }
    let frame = AmbeFrame::decode(&bad);
    assert!(frame.errors[0] >= 2 && frame.errors[0] + frame.errors[1] >= 6);
    let before = previous.clone();
    let p = ModelParameters::from_frame(&frame, &mut previous);
    assert_eq!(p.repeat_count, 1);
    assert_eq!(
        (p.b0, p.l, p.w0, p.gain),
        (before.b0, before.l, before.w0, before.gain)
    );
    assert_eq!(p.spectral, before.spectral);
    assert_eq!(p.local_energy, before.local_energy);
    // Smoothing at this error rate may only set voicing bits, and jmbe's
    // shared array makes the previous frame see them too.
    assert_eq!(previous.voicing, p.voicing);
    for (now, was) in p.voicing.iter().zip(before.voicing.iter()) {
        assert!(*now || !*was);
    }
    assert!(p.error_rate > before.error_rate);
}

#[test]
fn erasure_uses_defaults_with_zero_frequency() {
    let mut previous = ModelParameters::new_default();
    let mut frame = AmbeFrame::decode(&capture_frames()[5]);
    frame.b0 = 121;
    frame.frame_type = FrameType::Erasure;
    let p = ModelParameters::from_frame(&frame, &mut previous);
    assert!(p.is_erasure());
    assert_eq!((p.l, p.w0, p.gain), (9, 0.0, 0.0));
    assert_eq!(p.spectral, vec![1.0; 10]);
}

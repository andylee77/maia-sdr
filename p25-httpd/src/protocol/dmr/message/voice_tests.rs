//! Unit tests for `voice.rs`.

use super::*;
use crate::protocol::dmr::fec::emb::{EMB_INDEXES, VALID_WORDS};
use crate::protocol::dmr::fec::set_int;

fn cach() -> Cach {
    Cach {
        valid: true,
        busy: false,
        timeslot: 2,
        lcss: 0,
        payload: [0; 17],
    }
}

/// A burst B-F with an EMB for colour code `cc`, PI and LCSS, and `fragment` at 140..172.
fn emb_burst(cc: u16, pi: u16, lcss: u16, fragment: &[u8]) -> [u8; 288] {
    let mut bits = [0u8; 288];
    let mut word = [0u8; 16];
    set_int(
        u32::from(VALID_WORDS[usize::from((cc << 3) | (pi << 2) | lcss)]),
        &mut word,
    );
    for (x, &i) in EMB_INDEXES.iter().enumerate() {
        bits[i] = word[x];
    }
    bits[140..172].copy_from_slice(fragment);
    bits
}

#[test]
fn voice_a_and_emb_text() {
    let a = Voice::new(DmrSyncPattern::BaseStationVoice, [0; 288], cach(), 0, 2);
    assert_eq!(
        (a.class_name(), a.to_string().as_str()),
        ("VoiceAMessage", "CC:- BS VOICE A")
    );

    let b = Voice::new(
        DmrSyncPattern::BsVoiceFrameB,
        emb_burst(0, 0, 1, &[0; 32]),
        cach(),
        0,
        2,
    );
    assert_eq!(
        (b.class_name(), b.to_string().as_str()),
        ("VoiceEMBMessage", "CC:0 BS VOICE B")
    );
    assert_eq!(b.emb.unwrap().lcss, 1);

    let e = Voice::new(
        DmrSyncPattern::BsVoiceFrameE,
        emb_burst(0, 1, 2, &[0; 32]),
        cach(),
        0,
        2,
    );
    assert_eq!(e.to_string(), "CC:0 BS VOICE E ENCRYPTED");

    // An EMB beyond repair: no colour code, as SDRTrunk prints it.
    let mut bad = emb_burst(0, 0, 3, &[0; 32]);
    for i in [132, 133, 134, 135] {
        bad[i] ^= 1;
    }
    assert_eq!(
        Voice::new(DmrSyncPattern::BsVoiceFrameD, bad, cach(), 0, 2).to_string(),
        "BS VOICE D"
    );
}

#[test]
fn frame_f_short_bursts() {
    // An all-zero fragment is a valid null short burst.
    assert!(matches!(
        ShortBurst::extract(&[0; 32]),
        ShortBurst::Null { valid: true, .. }
    ));
    let mut f = Voice::new(
        DmrSyncPattern::BsVoiceFrameF,
        emb_burst(0, 0, 0, &[0; 32]),
        cach(),
        0,
        2,
    );
    f.embedded = Some(ShortBurst::extract(f.flc_fragment()));
    assert_eq!(f.to_string(), "CC:0 BS VOICE F NULL SHORT BURST");

    // SDRTrunk on Clay Electric: "BS VOICE F NON-STANDARD SHORT BURST:C64BFFFF".
    let mut deinterleaved = [0u8; 32];
    set_int(0xC64BFFFF, &mut deinterleaved);
    let mut fragment = [0u8; 32];
    for x in 0..32 {
        fragment[x] = deinterleaved[bptc_16_2::DEINTERLEAVE[x]];
    }
    let burst = ShortBurst::extract(&fragment);
    assert_eq!(burst.to_string(), "NON-STANDARD SHORT BURST:C64BFFFF");

    let mut txi = [0u8; 32];
    set_int(0b000_00100_011, &mut txi[..11]);
    assert_eq!(
        ShortBurst::TransmitInterrupt(txi).to_string(),
        "TRANSMIT INTERRUPT (TXI) AT FRAME D"
    );
}

#[test]
fn ambe_frames_are_the_three_72_bit_slices() {
    let mut bits = [0u8; 288];
    bits[24] = 1; // frame 1 bit 0
    bits[131] = 1; // frame 2 bit 35
    bits[180] = 1; // frame 2 bit 36
    bits[287] = 1; // frame 3 bit 71
    let v = Voice::new(DmrSyncPattern::BaseStationVoice, bits, cach(), 0, 1);
    let frames = v.ambe_frames();
    assert_eq!(frames[0][0], 0x80);
    assert_eq!(frames[1][4], 0x18);
    assert_eq!(frames[2][8], 0x01);
    assert_eq!(
        frames.iter().flatten().map(|b| b.count_ones()).sum::<u32>(),
        4
    );
}

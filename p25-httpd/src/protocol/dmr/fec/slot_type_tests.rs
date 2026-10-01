//! Unit tests for `slot_type.rs`.

use super::*;
use crate::protocol::dmr::fec::{golay24, set_int};

/// 20 transmitted slot type bits for a colour code and data type.
fn encode(cc: u8, dt: u8) -> [u8; 20] {
    let mut w = [0u8; 24];
    set_int(u32::from(cc), &mut w[4..8]);
    set_int(u32::from(dt), &mut w[8..12]);
    let c = golay24::calculate_checksum(&w, 0);
    set_int(c, &mut w[12..23]);
    w[23] = w[..23].iter().fold(0, |acc, &b| acc ^ b);
    let mut out = [0u8; 20];
    out.copy_from_slice(&w[4..]);
    out
}

fn burst_with(bits: &[u8; 20]) -> [u8; 288] {
    let mut burst = [0u8; 288];
    for (x, &b) in bits.iter().enumerate() {
        burst[MESSAGE_INDEXES[x]] = b;
    }
    burst
}

#[test]
fn known_golay_20_8_parity() {
    // ETSI Golay(20,8) parity for the first info bytes (MMDVMHost
    // ENCODING_TABLE_2087: 0x01 -> 0xB08E, 0x02 -> 0xE093, i.e. parity 0x8EB, 0x93E).
    assert_eq!(get_int(&encode(0, 1)[8..]), 0x8EB);
    assert_eq!(get_int(&encode(0, 2)[8..]), 0x93E);
}

#[test]
fn all_values_round_trip_from_burst() {
    for cc in 0..16u8 {
        for dt in 0..16u8 {
            let st = SlotType::get_slot_type(&burst_with(&encode(cc, dt))).expect("valid");
            assert_eq!(st.color_code, cc);
            assert_eq!(st.data_type, DataType::from_value(dt));
            assert_eq!(st.corrected, 0);
        }
    }
    assert_eq!(DataType::from_value(3), DataType::Csbk);
    assert_eq!(DataType::from_value(9), DataType::SlotIdle);
}

#[test]
fn up_to_three_errors_corrected() {
    for v in 0..256u32 {
        let (cc, dt) = ((v >> 4) as u8, (v & 0xF) as u8);
        let cw = encode(cc, dt);
        for a in 0..20 {
            for b in a..20 {
                for c in b..20 {
                    let mut rx = cw;
                    rx[a] ^= 1;
                    if b != a {
                        rx[b] ^= 1;
                    }
                    if c != b {
                        rx[c] ^= 1;
                    }
                    let n = 1 + u32::from(b != a) + u32::from(c != b);
                    let st = SlotType::decode(&rx).expect("correctable");
                    assert_eq!((st.color_code, st.corrected), (cc, n));
                    assert_eq!(st.data_type, DataType::from_value(dt));
                }
            }
        }
    }
}

#[test]
fn four_errors_detected() {
    for v in [0u32, 0x13, 0xA9, 0xFF] {
        let cw = encode((v >> 4) as u8, (v & 0xF) as u8);
        for a in 0..20 {
            for b in a + 1..20 {
                for c in b + 1..20 {
                    for d in c + 1..20 {
                        let mut rx = cw;
                        for p in [a, b, c, d] {
                            rx[p] ^= 1;
                        }
                        assert_eq!(SlotType::decode(&rx), None);
                    }
                }
            }
        }
    }
}

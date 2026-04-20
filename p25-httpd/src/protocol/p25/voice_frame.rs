//! P25 Phase 1 voice frame extraction.
//!
//! Extracts the 9 raw 144-bit IMBE voice frames from an LDU1 or LDU2
//! body dibit stream. Sits between the `ControlChannelDecoder` framer
//! and the vocoder.
//!
//! Pipeline:
//!   1. Strip body status dibits at raw positions `{13, 49, 85, 121,
//!      ..., 13 + 36*k}` -- they carry network-status info, not voice
//!      payload.
//!   2. Pack the resulting 784 data dibits into 1568 bits, MSB-first
//!      big-endian within each dibit (dibit `0bAB` -> `[A, B]`).
//!      Matches SDRTrunk's `BinaryMessage` ordering.
//!   3. Extract 9 raw 144-bit IMBE frames at fixed bit positions
//!      `[0, 144, 328, 512, 696, 880, 1064, 1248, 1424]` from
//!      `LDUMessage.java:32-40`.
//!   4. Each frame becomes `ImbeFrameRaw { bits: [u8; 18] }`,
//!      MSB-first within each byte -- format accepted directly by
//!      both JMBE and mbelib.
//!
//! This module does no IMBE FEC (vocoder handles it) and no LC/ESS/LSD
//! parsing in `extract_imbe_frames` (separate helpers below).
//!
//! # Reference
//!
//! - SDRTrunk `LDUMessage.java:32-40`, `LDU1Message.java`,
//!   `LDU2Message.java` (upstream-verified)
//! - `reference_p25_ldu_bit_layout.md` memory for full bit layout

use super::types::{is_body_status_dibit, DataUnit};

// ── Hamming(10,6,3) decoder for LDU1 LC hexbits ──────────────────────
//
// LDU1's Link Control Word hexbits use Hamming(10,6,3) (not Golay(24,12)
// like TDULC). Each 10-bit codeword: 6 data bits + 4 parity bits,
// corrects 1 bit error. Used in series with RS(24,12,13) across the 24
// hexbits (12 LC payload + 12 RS parity) to recover the 72-bit LCW
// mid-call.
//
// Checksum table verbatim from SDRTrunk `Hamming10.CHECKSUMS`. Syndrome
// -> flip-position via `Hamming10.checkAndCorrect`: odd syndromes 1..=14
// are single-bit correctable; even syndromes 5, 6, 9, 10, 15 indicate
// 2+ bit errors and are not correctable.

const HAMMING10_CHECKSUMS: [u8; 6] = [0x0E, 0x0D, 0x0B, 0x07, 0x03, 0x0C];

/// Compute Hamming(10,6,3) syndrome of a 10-bit codeword. Returns a
/// 4-bit value (0..=15).
fn hamming10_syndrome(cw: &[bool; 10]) -> u8 {
    let mut calculated: u8 = 0;
    for i in 0..6 {
        if cw[i] {
            calculated ^= HAMMING10_CHECKSUMS[i];
        }
    }
    // Parity bits at positions 6..=9 are 4-bit checksum
    // (MSB at pos 6, LSB at pos 9).
    let mut parity: u8 = 0;
    for i in 0..4 {
        if cw[6 + i] {
            parity |= 1 << (3 - i);
        }
    }
    calculated ^ parity
}

/// Correct up to 1 bit error in a Hamming(10,6,3) codeword in place.
/// Returns:
///   * `Some(0)` — no errors
///   * `Some(1)` — one bit corrected
///   * `None` — 2+ bits in error; data left uncorrected so the
///     downstream RS(24,12,13) can try to recover
pub(crate) fn hamming10_correct(cw: &mut [bool; 10]) -> Option<u32> {
    let syn = hamming10_syndrome(cw);
    // Mapping from SDRTrunk `Hamming10.checkAndCorrect`.
    match syn {
        0 => Some(0),
        1 => { cw[9] ^= true; Some(1) }  // Parity 1
        2 => { cw[8] ^= true; Some(1) }  // Parity 2
        3 => { cw[4] ^= true; Some(1) }  // Data 2
        4 => { cw[7] ^= true; Some(1) }  // Parity 4
        7 => { cw[3] ^= true; Some(1) }  // Data 3
        8 => { cw[6] ^= true; Some(1) }  // Parity 8
        11 => { cw[2] ^= true; Some(1) } // Data 4
        12 => { cw[5] ^= true; Some(1) } // Data 1
        13 => { cw[1] ^= true; Some(1) } // Data 5
        14 => { cw[0] ^= true; Some(1) } // Data 6
        // 5, 6, 9, 10, 15: multi-bit errors Hamming can't localise.
        _ => None,
    }
}

// ── Golay(24,12,7) decoder for TDULC LC ──────────────────────────────
//
// 288-bit TDULC FEC block = 12 x 24-bit Golay codewords. Each codeword:
// 12 data bits (pos 0-11), 11 parity bits (pos 12-22), overall parity
// bit (pos 23). Corrects <= 3 bit errors per codeword.
//
// Checksums table verbatim from SDRTrunk `Golay24.CHECKSUMS`
// (`CRCUtil.generate(12, 11, 0xC75, 0x0, true)`). First 12 entries
// encode the parity contribution of each data bit; entries 12..=22 are
// powers of 2 so existing parity bits XOR in at their own positions.
// Syndrome = single XOR loop over set bits in the 23-bit Golay message.

const GOLAY24_CHECKSUMS: [u16; 23] = [
    0x63A, 0x31D, 0x7B4, 0x3DA, 0x1ED, 0x6CC, 0x366, 0x1B3,
    0x6E3, 0x54B, 0x49F, 0x475,
    0x400, 0x200, 0x100, 0x080,
    0x040, 0x020, 0x010, 0x008,
    0x004, 0x002, 0x001,
];

/// 11-bit Golay(23,12) syndrome of a 24-bit codeword. Bit 23 (the
/// 24th bit) is the overall parity — not part of the syndrome math
/// but flipped if `parity_error` is true so the caller can use it
/// as an extra single-bit-error signal.
fn golay24_syndrome(cw: &[bool; 24]) -> u16 {
    let mut syn: u16 = 0;
    for i in 0..23 {
        if cw[i] {
            syn ^= GOLAY24_CHECKSUMS[i];
        }
    }
    syn & 0x7FF
}

/// Correct up to 3 bit errors in a 24-bit Golay codeword in place.
/// Returns the Hamming-distance between the original and corrected
/// codewords (0..=3), or `None` if no error pattern of weight <= 3
/// explains the syndrome.
///
/// Brute-force syndrome -> error-pattern search over the 23
/// syndrome-contributing positions: 23 + 253 + 1771 = 2047 candidates,
/// each a single 16-bit XOR + equality compare.
pub(crate) fn golay24_correct(cw: &mut [bool; 24]) -> Option<u32> {
    let syn = golay24_syndrome(cw);
    if syn == 0 {
        return Some(0);
    }

    // Weight-1 error.
    for e1 in 0..23 {
        if GOLAY24_CHECKSUMS[e1] == syn {
            cw[e1] ^= true;
            return Some(1);
        }
    }
    // Weight-2 errors.
    for e1 in 0..23 {
        let s1 = GOLAY24_CHECKSUMS[e1];
        for e2 in (e1 + 1)..23 {
            if s1 ^ GOLAY24_CHECKSUMS[e2] == syn {
                cw[e1] ^= true;
                cw[e2] ^= true;
                return Some(2);
            }
        }
    }
    // Weight-3 errors. ~1770 XOR/compare triples per codeword.
    for e1 in 0..23 {
        let s1 = GOLAY24_CHECKSUMS[e1];
        for e2 in (e1 + 1)..23 {
            let s12 = s1 ^ GOLAY24_CHECKSUMS[e2];
            for e3 in (e2 + 1)..23 {
                if s12 ^ GOLAY24_CHECKSUMS[e3] == syn {
                    cw[e1] ^= true;
                    cw[e2] ^= true;
                    cw[e3] ^= true;
                    return Some(3);
                }
            }
        }
    }
    None
}

/// Golay(18,6,8) decode — the truncated Golay variant SDRTrunk uses to
/// protect each HDU hexbit (`HDUMessage` applies it at the 20 data
/// hexbit positions + first stage of the RS cascade).
///
/// Implementation mirrors SDRTrunk `Golay18.checkAndCorrect`: pack the
/// 18 input bits into positions [6..24] of a 24-bit Golay24 codeword,
/// leave positions [0..6] zero-filled, run the full Golay24 decoder,
/// and read back positions [6..24] as the corrected 18-bit value. The
/// top 6 bits act as known-zero anchor points that reduce the Golay24
/// decoder's search space to the 18-bit Golay18 subspace.
///
/// Returns the number of bits corrected (0..=3), or `None` when no
/// error pattern of weight ≤ 3 explains the syndrome.
pub(crate) fn golay18_correct(cw: &mut [bool; 18]) -> Option<u32> {
    let mut cw24 = [false; 24];
    cw24[6..24].copy_from_slice(cw);
    let errs = golay24_correct(&mut cw24)?;
    cw.copy_from_slice(&cw24[6..24]);
    Some(errs)
}

// ── TDULC Link Control Word parsing ──────────────────────────────────
//
// TDULC carries 72 bits of Link Control wrapped in 12 x Golay(24,12)
// per-hexbit protection + RS(24,12,13) across all 24 hexbits (per
// SDRTrunk `TDULCMessage.createLinkControlWord`). See
// `reference_tdulc_lcw_variants.md`.

/// Position of each LC hexbit in the 288-bit TDULC body before any
/// FEC. Sourced verbatim from SDRTrunk
/// `TDULCMessage.LC_HEX_0..11` arrays. 12 hexbits × 6 bits = 72-bit
/// post-FEC Link Control Word.
/// Starting bit offsets of the 12 RS(24,12,13) parity hexbits inside
/// a 294-bit TDULC body (after Hamming10 + Golay24 correction). Used
/// by both the TDULC parser and the LDU1 LCW reader. Matches
/// SDRTrunk's `TDULCMessage.RS_HEX_0..11` array.
const TDULC_RS_HEX_POSITIONS: [usize; 12] = [
    144, 150, 168, 174, 192, 198, 216, 222, 240, 246, 264, 270,
];

const LC_HEX_POSITIONS: [usize; 12] = [
    0,   // LC_HEX_0  -> LC bits 0-5
    6,   // LC_HEX_1  -> LC bits 6-11
    24,  // LC_HEX_2  -> LC bits 12-17
    30,  // LC_HEX_3  -> LC bits 18-23
    48,  // LC_HEX_4  -> LC bits 24-29
    54,  // LC_HEX_5  -> LC bits 30-35
    72,  // LC_HEX_6  -> LC bits 36-41
    78,  // LC_HEX_7  -> LC bits 42-47
    96,  // LC_HEX_8  -> LC bits 48-53
    102, // LC_HEX_9  -> LC bits 54-59
    120, // LC_HEX_10 -> LC bits 60-65
    126, // LC_HEX_11 -> LC bits 66-71
];

/// TDULC-specific body status dibit positions. SDRTrunk's framer
/// increments `mStatusSymbolDibitCounter` starting at 21 immediately
/// AFTER NID detection, inserting a status dibit whenever the counter
/// hits 36 -- putting the FIRST body status at position 14, and
/// subsequent ones at +36 intervals.
///
/// The shared `is_body_status_dibit` in `types.rs` uses the +13 pattern
/// (correct for TSDUs / control-channel frames); TDULC specifically
/// needs +14. Cross-checked against SDRTrunk `.bits` files: +14 decodes
/// cleanly as `GROUP VOICE CHANNEL USER FM:0 TO:300`; +13 shifted bits
/// and misread MFID as 0x02 instead of 0x00.
const TDULC_BODY_STATUS_POSITIONS: [usize; 5] = [14, 50, 86, 122, 158];

fn is_tdulc_body_status(pos: usize) -> bool {
    TDULC_BODY_STATUS_POSITIONS.iter().any(|&p| p == pos)
}

/// Parsed TDULC / LDU1 Link Control Word. 72-bit LC layout is identical
/// in both data units; only the FEC chain differs (Golay+RS on TDULC,
/// Hamming+RS on LDU1). See `parse_tdulc_lcw` / `parse_ldu1_lcw`.
///
/// Variants mirror SDRTrunk's `LinkControlOpcode` enum plus the one
/// Motorola vendor extension we act on (`MOTOROLA_TALK_COMPLETE`).
/// Unknown opcodes fall through to `Other` so the caller can render
/// them diagnostically without trusting field values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TdulcLcw {
    /// Standard LCW opcode 0x00 -- `GROUP VOICE CHANNEL USER`.
    ///
    /// `source_radio_id` is the 24-bit FM: field at LC bits 48-71
    /// (SDRTrunk `OCTET_6_BIT_48`). TDULC GVCU always has 0 per spec;
    /// LDU1 GVCU carries the speaker's radio ID. `service_options` is
    /// the 8-bit byte at LC bits 16-23 (`OCTET_2_BIT_16`) --
    /// emergency/encryption/duplex/priority flags.
    GroupVoiceChannelUser {
        talkgroup: u16,
        source_radio_id: u32,
        service_options: u8,
    },

    /// Motorola MFID 0x90 + opcode 0x0F -- `TALK_COMPLETE`. Carries
    /// the last speaker's 24-bit radio ID in ADDRESS (bits 48-71).
    /// Exactly one per speaker on Motorola sites.
    MotorolaTalkComplete { by_radio_id: u32 },

    /// Standard LCW opcode 0x02 -- `GROUP VOICE CHANNEL UPDATE`.
    /// Announces two (TG, channel) pairs. Channel B is optional per
    /// SDRTrunk's `hasChannelB() == CHAN_B != 0 && GROUP_A != GROUP_B`.
    GroupVoiceChannelUpdate {
        talkgroup_a: u16,
        channel_a_band: u8,
        channel_a_number: u16,
        talkgroup_b: u16,
        channel_b_band: u8,
        channel_b_number: u16,
        has_channel_b: bool,
    },

    /// Standard LCW opcode 0x0F -- `CALL TERMINATION`. Carries the
    /// 24-bit radio ID at bits 48-71 that ended the call (or
    /// 0xFFFFFD / 0xFFFFFF / 0x000000 for system-controller teardowns).
    CallTermination { by_radio_id: u32 },

    /// Standard LCW opcode 0x23 (35) -- `RFSS STATUS BROADCAST`.
    RfssStatusBroadcast {
        lra: u8,
        system_id: u16,
        rfss_id: u8,
        site_id: u8,
        channel_band: u8,
        channel_number: u16,
        service_class: u8,
    },

    /// Standard LCW opcode 0x24 (36) -- `NET STATUS BROADCAST`.
    NetStatusBroadcast {
        wacn: u32,
        system_id: u16,
        channel_band: u8,
        channel_number: u16,
        service_class: u8,
    },

    /// Any LC the caller doesn't need to act on (unknown opcode,
    /// non-Motorola vendor, or a bit-error that produced values we
    /// can't trust).
    Other { opcode: u8, mfid: u8 },
}

/// Parse a TDULC Link Control Word from the raw body dibit slice.
///
/// Expects exactly `DataUnit::TduLc.length_dibits()` (159) dibits in
/// the same format the software decoder's `du_buffer` holds.
/// Returns `None` if the length is wrong; returns `TdulcLcw::Other`
/// for unrecognized opcodes or bit-corrupt frames.
pub fn parse_tdulc_lcw(body_raw: &[u8]) -> Option<TdulcLcw> {
    if body_raw.len() != DataUnit::TduLc.length_dibits() {
        return None;
    }
    // TDULC-specific status strip (see TDULC_BODY_STATUS_POSITIONS).
    let data_dibits: Vec<u8> = body_raw
        .iter()
        .enumerate()
        .filter_map(
            |(pos, &d)| {
                if is_tdulc_body_status(pos) {
                    None
                } else {
                    Some(d)
                }
            },
        )
        .collect();
    let raw_bits = dibits_to_bits(&data_dibits);
    // 288-bit FEC block + 20 null-padding bits; if the strip
    // produced fewer data bits than the 144-bit LC-hexbit region
    // plus the 12-codeword Golay correction region, bail.
    if raw_bits.len() < 288 {
        return None;
    }

    // FEC chain (SDRTrunk `TDULCMessage.createLinkControlWord`):
    //   1. Golay(24,12) on each of the 12 x 24-bit codewords in the
    //      288-bit TDULC FEC block. Fixes <= 3 bit errors per codeword.
    //   2. RS(24,12,13) on the resulting 24 x 6-bit hexbits. Fixes
    //      <= 6 hexbit errors across the full LC.
    //
    // Bits 0..144 carry the 12 LC-payload hexbits; bits 144..288 carry
    // the 12 RS-parity hexbits. Run Golay on all 12 so the hexbits
    // feeding RS are pre-cleaned.
    let mut corrected_bits = [false; 288];
    for cw_idx in 0..12 {
        let base = cw_idx * 24;
        let mut cw = [false; 24];
        for b in 0..24 {
            cw[b] = raw_bits[base + b];
        }
        let _ = golay24_correct(&mut cw);
        for b in 0..24 {
            corrected_bits[base + b] = cw[b];
        }
    }

    // Pack all 24 hexbits (12 LC + 12 RS parity) into RS input.
    // SDRTrunk's decoder expects the hexbits in reverse order per
    // `TDULCMessage.createLinkControlWord`:
    //   input[0..=11]  = RS_HEX_11 .. RS_HEX_0
    //   input[12..=23] = LC_HEX_11 .. LC_HEX_0
    //   input[24..=62] = 0  (shortened code virtual padding)
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected_bits[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut rs_input = [0u32; 63];
    // indices 0..=11 = RS parity hexbits in reverse (RS_HEX_11 first)
    for i in 0..12 {
        rs_input[i] = hex_at(TDULC_RS_HEX_POSITIONS[11 - i]);
    }
    // indices 12..=23 = LC payload hexbits in reverse (LC_HEX_11 first)
    for i in 0..12 {
        rs_input[12 + i] = hex_at(LC_HEX_POSITIONS[11 - i]);
    }
    let rs_output = match super::fec::rs_24_12_13::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    // The 12 corrected LC hexbits are at output[23..=12] (SDRTrunk
    // `for x in (23..=12) pack into binaryMessage`), so output[23] is
    // LC_HEX_0, ..., output[12] is LC_HEX_11.
    let mut lc_bits = [false; 72];
    for i in 0..12 {
        let hexbit_val = rs_output[23 - i];
        for b in 0..6 {
            lc_bits[i * 6 + b] =
                ((hexbit_val >> (5 - b)) & 1) != 0;
        }
    }

    Some(classify_lcw(&lc_bits))
}

/// Classify a decoded 72-bit Link Control Word into a [`TdulcLcw`]
/// variant.
///
/// Both TDULC and LDU1 produce a 72-bit LCW after FEC; the layout of
/// the LCW itself is identical between them. This helper centralises
/// the opcode / MFID dispatch so `parse_tdulc_lcw` and
/// `parse_ldu1_lcw` only differ in the FEC stack that precedes it.
///
/// Dispatch follows SDRTrunk's `LinkControlWordFactory`:
///
/// 1. If the STANDARD_VENDOR_ID_FLAG (bit 1) is set OR the MFID byte
///    is 0x00, the LCW is in "standard" format — dispatch on opcode
///    alone (byte 1 is payload, not a vendor id).
/// 2. Else if MFID is 0x90, dispatch the Motorola vendor opcode set.
/// 3. Else fall back to `Other { opcode, mfid }`.
fn classify_lcw(lc_bits: &[bool; 72]) -> TdulcLcw {
    let byte = |start: usize, n: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..n {
            v = (v << 1) | if lc_bits[start + b] { 1 } else { 0 };
        }
        v
    };
    let standard_vendor_flag = lc_bits[1];
    let opcode = byte(2, 6) as u8;
    let mfid = byte(8, 8) as u8;
    let is_standard = standard_vendor_flag || mfid == 0x00;

    if !is_standard {
        if mfid == 0x90 && opcode == 0x0F {
            return TdulcLcw::MotorolaTalkComplete {
                by_radio_id: byte(48, 24),
            };
        }
        return TdulcLcw::Other { opcode, mfid };
    }

    // Standard LCW dispatch. Bit positions from the SDRTrunk
    // `io.github.dsheirer.module.decode.p25.phase1.message.lc.standard`
    // subclasses; each one reads 6-bit opcode at OCTET_0_BIT_2 and
    // then custom fields at the OCTET_*_BIT_* offsets called out
    // below.
    match opcode {
        // LCGroupVoiceChannelUser: GROUP_ADDRESS at OCTET_4_BIT_32
        // (bits 32-47).
        0x00 => TdulcLcw::GroupVoiceChannelUser {
            talkgroup: byte(32, 16) as u16,
            source_radio_id: byte(48, 24),
            service_options: byte(16, 8) as u8,
        },
        // LCGroupVoiceChannelUpdate:
        //   FREQ_BAND_A  OCTET_1_BIT_8  (4 bits)
        //   CHANNEL_A    OCTET_1_BIT_8+4 (12 bits)
        //   GROUP_A      OCTET_3_BIT_24 (16 bits)
        //   FREQ_BAND_B  OCTET_5_BIT_40 (4 bits)
        //   CHANNEL_B    OCTET_5_BIT_40+4 (12 bits)
        //   GROUP_B      OCTET_7_BIT_56 (16 bits)
        0x02 => {
            let tg_a = byte(24, 16) as u16;
            let tg_b = byte(56, 16) as u16;
            let ch_b = byte(44, 12) as u16;
            TdulcLcw::GroupVoiceChannelUpdate {
                channel_a_band: byte(8, 4) as u8,
                channel_a_number: byte(12, 12) as u16,
                talkgroup_a: tg_a,
                channel_b_band: byte(40, 4) as u8,
                channel_b_number: ch_b,
                talkgroup_b: tg_b,
                // Mirror SDRTrunk's `hasChannelB()`: CHAN_B non-zero
                // and GROUP_A != GROUP_B (avoids double-counting the
                // single-TG variant that just repeats channel info).
                has_channel_b: ch_b != 0 && tg_a != tg_b,
            }
        }
        // LCCallTermination: ADDRESS at OCTET_6_BIT_48 (24 bits).
        // Standard MFID 0x00 distinguishes it from Motorola
        // TALK_COMPLETE (vendor MFID 0x90, same opcode).
        0x0F => TdulcLcw::CallTermination {
            by_radio_id: byte(48, 24),
        },
        // LCRFSSStatusBroadcast:
        //   LRA            OCTET_1_BIT_8   (8 bits)
        //   SYSTEM         OCTET_2_BIT_16+4 (12 bits — bits 20-31)
        //   RFSS           OCTET_4_BIT_32  (8 bits)
        //   SITE           OCTET_5_BIT_40  (8 bits)
        //   FREQ_BAND      OCTET_6_BIT_48  (4 bits)
        //   CHANNEL_NUMBER OCTET_6_BIT_48+4 (12 bits)
        //   SERVICE_CLASS  OCTET_8_BIT_64  (8 bits)
        0x23 => TdulcLcw::RfssStatusBroadcast {
            lra: byte(8, 8) as u8,
            system_id: byte(20, 12) as u16,
            rfss_id: byte(32, 8) as u8,
            site_id: byte(40, 8) as u8,
            channel_band: byte(48, 4) as u8,
            channel_number: byte(52, 12) as u16,
            service_class: byte(64, 8) as u8,
        },
        // LCNetworkStatusBroadcast:
        //   WACN           OCTET_2_BIT_16  (20 bits — bits 16-35)
        //   SYSTEM         OCTET_4_BIT_32+4 (12 bits — bits 36-47)
        //   FREQ_BAND      OCTET_6_BIT_48  (4 bits)
        //   CHANNEL_NUMBER OCTET_6_BIT_48+4 (12 bits)
        //   SERVICE_CLASS  OCTET_8_BIT_64  (8 bits)
        0x24 => TdulcLcw::NetStatusBroadcast {
            wacn: byte(16, 20),
            system_id: byte(36, 12) as u16,
            channel_band: byte(48, 4) as u8,
            channel_number: byte(52, 12) as u16,
            service_class: byte(64, 8) as u8,
        },
        _ => TdulcLcw::Other { opcode, mfid },
    }
}

/// LDU1 hexbit positions (post-status-strip bit positions in the
/// 1568-bit LDU1 data region). Verbatim from SDRTrunk
/// `LDU1Message.CW_HEX_*` / `RS_HEX_*` / `GOLAY_WORD_STARTS`. Each
/// position is the start of a 10-bit Hamming(10,6,3) codeword whose
/// first 6 bits are the hexbit data.
const LDU1_CW_HEX_POSITIONS: [usize; 12] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686,
];
const LDU1_RS_HEX_POSITIONS: [usize; 12] = [
    840, 850, 860, 870, 1024, 1034, 1044, 1054, 1208, 1218, 1228, 1238,
];

/// Parse the Link Control Word embedded in an LDU1 voice frame.
/// Produces the same [`TdulcLcw`] variants as [`parse_tdulc_lcw`] --
/// the 72-bit LCW structure is identical; only the FEC stack
/// (Hamming+RS vs Golay+RS) and hexbit positions differ.
///
/// `body_raw` must be the full 807-dibit LDU1 body (including 23 body
/// status dibits). Returns `None` if the length is wrong.
///
/// Gives `FM:<source>` and `TO:<TG>` mid-call, without waiting for
/// the end-of-speaker Motorola TDULC.
pub fn parse_ldu1_lcw(body_raw: &[u8]) -> Option<TdulcLcw> {
    if body_raw.len() != LDU_RAW_DIBITS {
        return None;
    }
    // Status-dibit strip -> 784 data dibits -> 1568 data bits.
    // LDU1/LDU2/HDU all follow the shared "+13" pattern.
    let data_dibits = strip_body_status_dibits(body_raw);
    if data_dibits.len() != DataUnit::Ldu1.data_dibits() {
        return None;
    }
    let bits = dibits_to_bits(&data_dibits);
    if bits.len() < 1248 {
        return None;
    }

    // Run Hamming(10,6,3) across all 24 hexbit codewords. Leave
    // uncorrectable codewords untouched -- RS(24,12,13) handles up
    // to 6 hexbit errors so a handful of Hamming failures is fine.
    let starts: [usize; 24] = {
        let mut s = [0usize; 24];
        s[..12].copy_from_slice(&LDU1_CW_HEX_POSITIONS);
        s[12..].copy_from_slice(&LDU1_RS_HEX_POSITIONS);
        s
    };
    let mut corrected = bits;
    for &st in &starts {
        let mut cw = [false; 10];
        for b in 0..10 {
            cw[b] = corrected[st + b];
        }
        let _ = hamming10_correct(&mut cw);
        for b in 0..10 {
            corrected[st + b] = cw[b];
        }
    }

    // Pack 24 corrected hexbits into RS input, reverse order per
    // SDRTrunk: input[0..=11] = RS_HEX_11..0, input[12..=23] =
    // CW_HEX_11..0, input[24..63] zero-padded.
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut rs_input = [0u32; 63];
    for i in 0..12 {
        rs_input[i] = hex_at(LDU1_RS_HEX_POSITIONS[11 - i]);
    }
    for i in 0..12 {
        rs_input[12 + i] = hex_at(LDU1_CW_HEX_POSITIONS[11 - i]);
    }
    let rs_output = match super::fec::rs_24_12_13::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    // 12 corrected LC hexbits at output[23..=12], packed into the
    // 72-bit LC with LC_HEX_0 at bits 0-5, LC_HEX_1 at bits 6-11, etc.
    let mut lc_bits = [false; 72];
    for i in 0..12 {
        let hexbit_val = rs_output[23 - i];
        for b in 0..6 {
            lc_bits[i * 6 + b] =
                ((hexbit_val >> (5 - b)) & 1) != 0;
        }
    }

    Some(classify_lcw(&lc_bits))
}

/// Returns the raw 24-bit source radio ID from the LDU1 LC (bits
/// 48-71 = SDRTrunk's `SOURCE_ADDRESS` at `OCTET_6_BIT_48`). Every
/// LDU1 standard GVCU LC carries a FM:<source> value there.
pub fn parse_ldu1_source(body_raw: &[u8]) -> Option<u32> {
    match parse_ldu1_lcw(body_raw)? {
        TdulcLcw::MotorolaTalkComplete { by_radio_id } => Some(by_radio_id),
        TdulcLcw::GroupVoiceChannelUser { source_radio_id, .. } => {
            if source_radio_id == 0 { None } else { Some(source_radio_id) }
        }
        // GVU + status broadcasts are non-speaker LCWs; CallTermination's
        // ADDRESS is the terminating radio, not the speaker. Mirror
        // SDRTrunk: no source stamp.
        TdulcLcw::GroupVoiceChannelUpdate { .. }
        | TdulcLcw::CallTermination { .. }
        | TdulcLcw::RfssStatusBroadcast { .. }
        | TdulcLcw::NetStatusBroadcast { .. }
        | TdulcLcw::Other { .. } => None,
    }
}

/// LDU1 variant of [`tdulc_lc_bytes`]. Exposed for diagnostics + so
/// [`parse_ldu1_source`] can extract the full source address from a
/// standard GVCU LCW without a second parse.
pub fn ldu1_lc_bytes(body_raw: &[u8]) -> Option<[u8; 9]> {
    if body_raw.len() != LDU_RAW_DIBITS {
        return None;
    }
    let data_dibits = strip_body_status_dibits(body_raw);
    let bits = dibits_to_bits(&data_dibits);
    if bits.len() < 1248 {
        return None;
    }
    let mut corrected = bits;
    let starts: [usize; 24] = {
        let mut s = [0usize; 24];
        s[..12].copy_from_slice(&LDU1_CW_HEX_POSITIONS);
        s[12..].copy_from_slice(&LDU1_RS_HEX_POSITIONS);
        s
    };
    for &st in &starts {
        let mut cw = [false; 10];
        for b in 0..10 {
            cw[b] = corrected[st + b];
        }
        let _ = hamming10_correct(&mut cw);
        for b in 0..10 {
            corrected[st + b] = cw[b];
        }
    }
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut rs_input = [0u32; 63];
    for i in 0..12 {
        rs_input[i] = hex_at(LDU1_RS_HEX_POSITIONS[11 - i]);
    }
    for i in 0..12 {
        rs_input[12 + i] = hex_at(LDU1_CW_HEX_POSITIONS[11 - i]);
    }
    let rs_output = match super::fec::rs_24_12_13::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    let mut lc_bits = [false; 72];
    for i in 0..12 {
        let hexbit_val = rs_output[23 - i];
        for b in 0..6 {
            lc_bits[i * 6 + b] =
                ((hexbit_val >> (5 - b)) & 1) != 0;
        }
    }
    let mut out = [0u8; 9];
    for (byte_i, byte) in out.iter_mut().enumerate() {
        let mut v = 0u8;
        for b in 0..8 {
            v = (v << 1) | if lc_bits[byte_i * 8 + b] { 1 } else { 0 };
        }
        *byte = v;
    }
    Some(out)
}

/// Returns the 72-bit LC as 9 bytes (MSB-first) extracted from a
/// TDULC body dibit slice. First two bytes are opcode / MFID;
/// `bytes[6..9]` is the Motorola ADDRESS field. `None` if the body
/// length is wrong.
pub fn tdulc_lc_bytes(body_raw: &[u8]) -> Option<[u8; 9]> {
    if body_raw.len() != DataUnit::TduLc.length_dibits() {
        return None;
    }
    let data_dibits: Vec<u8> = body_raw
        .iter()
        .enumerate()
        .filter_map(
            |(pos, &d)| {
                if is_tdulc_body_status(pos) {
                    None
                } else {
                    Some(d)
                }
            },
        )
        .collect();
    let raw_bits = dibits_to_bits(&data_dibits);
    if raw_bits.len() < 288 {
        return None;
    }
    // Same Golay(24,12) + RS(24,12,13) as parse_tdulc_lcw so dumped
    // bytes match what the parser classified against.
    let mut corrected_bits = [false; 288];
    for cw_idx in 0..12 {
        let base = cw_idx * 24;
        let mut cw = [false; 24];
        for b in 0..24 {
            cw[b] = raw_bits[base + b];
        }
        let _ = golay24_correct(&mut cw);
        for b in 0..24 {
            corrected_bits[base + b] = cw[b];
        }
    }
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected_bits[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut rs_input = [0u32; 63];
    for i in 0..12 {
        rs_input[i] = hex_at(TDULC_RS_HEX_POSITIONS[11 - i]);
    }
    for i in 0..12 {
        rs_input[12 + i] = hex_at(LC_HEX_POSITIONS[11 - i]);
    }
    let rs_output = match super::fec::rs_24_12_13::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    let mut lc_bits = [false; 72];
    for i in 0..12 {
        let hexbit_val = rs_output[23 - i];
        for b in 0..6 {
            lc_bits[i * 6 + b] =
                ((hexbit_val >> (5 - b)) & 1) != 0;
        }
    }
    let mut out = [0u8; 9];
    for (byte_i, byte) in out.iter_mut().enumerate() {
        let mut v = 0u8;
        for b in 0..8 {
            v = (v << 1) | if lc_bits[byte_i * 8 + b] { 1 } else { 0 };
        }
        *byte = v;
    }
    Some(out)
}

/// Raw 144-bit IMBE voice frame, ready for direct vocoder input.
///
/// Bits are packed MSB-first within each byte: byte 0 bit 7 is the
/// first IMBE bit on-air, byte 0 bit 0 is the 8th, byte 1 bit 7 is
/// the 9th, etc. 144 bits / 8 = 18 bytes exactly. Both mbelib and
/// JMBE accept this format directly.
#[derive(Debug, Clone, Copy)]
pub struct ImbeFrameRaw {
    pub bits: [u8; 18],
}

impl ImbeFrameRaw {
    pub const BITS: usize = 144;
    pub const BYTES: usize = 18;

    /// All-zero placeholder. Used as a default when extraction fails
    /// (so the caller still gets 9 entries per LDU and can dispatch
    /// per-frame validity flags separately).
    pub const ZERO: Self = ImbeFrameRaw { bits: [0u8; 18] };
}

/// Bit positions of the 9 IMBE frames within an LDU body of 1568
/// data bits (after status dibits have been stripped).
///
/// From SDRTrunk `LDUMessage.java:32-40`:
///
/// ```text
/// IMBE_FRAME_1 = 0,     IMBE_FRAME_2 = 144,   IMBE_FRAME_3 = 328,
/// IMBE_FRAME_4 = 512,   IMBE_FRAME_5 = 696,   IMBE_FRAME_6 = 880,
/// IMBE_FRAME_7 = 1064,  IMBE_FRAME_8 = 1248,  IMBE_FRAME_9 = 1424
/// ```
///
/// Each frame is exactly 144 bits. Non-uniform spacing comes from
/// LC/ESS/LSD chunks interleaved between frames -- see
/// `reference_p25_ldu_bit_layout.md`.
pub const IMBE_FRAME_BIT_POSITIONS: [usize; 9] = [
    0, 144, 328, 512, 696, 880, 1064, 1248, 1424,
];

/// LDU1 / LDU2 body data length in bits, after status dibit strip.
/// Equals `DataUnit::Ldu1.data_dibits() * 2` = 784 * 2 = 1568.
pub const LDU_DATA_BITS: usize = 1568;

/// LDU1 / LDU2 body raw length in dibits, including body status
/// dibits. Equals `DataUnit::Ldu1.length_dibits()` = 807.
pub const LDU_RAW_DIBITS: usize = 807;

/// Strips body status dibits from a raw body dibit slice. Returns
/// only the data dibits, in the same order.
///
/// Universal body status pattern -- works for any P25 Phase 1 data
/// unit (HDU, TDU, LDU1, LDU2, TSDU, TDU_LC).
pub fn strip_body_status_dibits(body_raw: &[u8]) -> Vec<u8> {
    body_raw
        .iter()
        .enumerate()
        .filter_map(|(pos, &d)| {
            if is_body_status_dibit(pos) {
                None
            } else {
                Some(d)
            }
        })
        .collect()
}

/// Packs a sequence of dibits into a bit string, MSB-first within
/// each dibit. Dibit `0bAB` (with A as the MSB) becomes bits
/// `[A, B]`. Output bit 0 = first bit on-air = first dibit's MSB.
pub fn dibits_to_bits(dibits: &[u8]) -> Vec<bool> {
    let mut bits = Vec::with_capacity(dibits.len() * 2);
    for &d in dibits {
        // MSB first: bit 1 of dibit, then bit 0
        bits.push((d & 0b10) != 0);
        bits.push((d & 0b01) != 0);
    }
    bits
}

/// Extracts the 9 raw 144-bit IMBE frames from an LDU body raw dibit
/// slice (807 dibits, including body status dibits).
///
/// Returns 9 frames in transmission order (frame 0 = oldest 20 ms,
/// frame 8 = newest). Each LDU = 9 * 20 ms = 180 ms of audio; at
/// one LDU per ~140 ms the stream is continuous 50 frames/second.
pub fn extract_imbe_frames(body_raw: &[u8]) -> Option<[ImbeFrameRaw; 9]> {
    if body_raw.len() != LDU_RAW_DIBITS {
        return None;
    }
    // Strip status dibits -> 784 data dibits = 1568 data bits.
    let data_dibits = strip_body_status_dibits(body_raw);
    debug_assert_eq!(data_dibits.len(), DataUnit::Ldu1.data_dibits());
    let bits = dibits_to_bits(&data_dibits);
    debug_assert_eq!(bits.len(), LDU_DATA_BITS);

    let mut frames = [ImbeFrameRaw::ZERO; 9];
    for (i, &start) in IMBE_FRAME_BIT_POSITIONS.iter().enumerate() {
        for byte_idx in 0..ImbeFrameRaw::BYTES {
            let mut byte: u8 = 0;
            for bit_in_byte in 0..8 {
                let abs_bit = start + byte_idx * 8 + bit_in_byte;
                if bits[abs_bit] {
                    // MSB-first within each byte
                    byte |= 1 << (7 - bit_in_byte);
                }
            }
            frames[i].bits[byte_idx] = byte;
        }
    }
    Some(frames)
}

// ── HDU header parsing ──────────────────────────────────────────────
//
// HDU body layout (329 post-status-strip data dibits = 658 data bits,
// 648 used):
//
// - 36 Golay18 codewords at 18-bit intervals (positions 0, 18, 36, ...,
//   630). Each codeword = 6 data bits (hexbit) + 12 Golay18 parity bits.
//   SDRTrunk's GOLAY_WORD_STARTS has a typo at index 21 (`278` where
//   `378` is required by `+18*k`); computed programmatically below.
// - After Golay18: 36 hexbits = 20 CW (payload) + 16 RS parity. Fed to
//   RS(63,47,17) which corrects up to 8 hexbit errors.
// - 120-bit HDU header emerges from 20 corrected CW hexbits, per
//   `HeaderData.java`:
//     * bits   0-71: Message Indicator (72-bit encryption IV)
//     * bits  72-79: Vendor ID
//     * bits  80-87: Algorithm ID (0x80 = UNENCRYPTED per TIA-102.AABD)
//     * bits  88-103: Key ID
//     * bits 104-119: Talkgroup ID

/// HDU Golay18 codeword start positions (0, 18, ..., 630) computed
/// programmatically from the `+18*k` rule to dodge SDRTrunk's typo.
const HDU_GOLAY_STARTS: [usize; 36] = {
    let mut s = [0usize; 36];
    let mut i = 0;
    while i < 36 {
        s[i] = i * 18;
        i += 1;
    }
    s
};

/// Start bit of each CW hexbit (first 6 bits of Golay18 word 0..19).
const HDU_CW_HEX_POSITIONS: [usize; 20] = {
    let mut p = [0usize; 20];
    let mut i = 0;
    while i < 20 {
        p[i] = i * 18;
        i += 1;
    }
    p
};

/// Start bit of each RS parity hexbit (first 6 bits of Golay18 words
/// 20..=35, offset 360 + 18*k).
const HDU_RS_HEX_POSITIONS: [usize; 16] = {
    let mut p = [0usize; 16];
    let mut i = 0;
    while i < 16 {
        p[i] = 360 + i * 18;
        i += 1;
    }
    p
};

/// Parsed HDU header payload. `None`-result from `parse_hdu_body`
/// indicates the FEC chain couldn't recover a valid message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HduHeader {
    pub talkgroup: u16,
    pub key_id: u16,
    /// 8-bit algorithm id. `0x80` = unencrypted per TIA-102.AABD.
    pub algorithm_id: u8,
    /// 72-bit message indicator (AES-256 / DES-OFB IV).
    pub message_indicator: [u8; 9],
}

impl HduHeader {
    /// Per SDRTrunk `HeaderData.isEncryptedAudio` / `Encryption.UNENCRYPTED`.
    pub fn is_encrypted(&self) -> bool {
        self.algorithm_id != super::wire::ALGORITHM_CLEAR
    }

    /// True iff `algorithm_id` is in the TIA-102.AABD / SDRTrunk
    /// `Encryption.fromValue` known set. The call-encrypted gate uses
    /// this to reject byte values that only a bit-corrupt FEC decode
    /// could have produced (phantom-ENC guard).
    pub fn is_spec_algorithm(&self) -> bool {
        is_spec_algorithm_id(self.algorithm_id)
    }
}

/// Parse the HDU body through SDRTrunk's Golay18 + RS(63,47,17) FEC
/// chain and return the decoded 120-bit header. `body_raw` must be
/// `DataUnit::Hdu.length_dibits()` long.
pub fn parse_hdu_body(body_raw: &[u8]) -> Option<HduHeader> {
    if body_raw.len() != DataUnit::Hdu.length_dibits() {
        return None;
    }
    // HDU follows the universal +13 status dibit pattern.
    let data_dibits = strip_body_status_dibits(body_raw);
    if data_dibits.len() != DataUnit::Hdu.data_dibits() {
        return None;
    }
    let bits = dibits_to_bits(&data_dibits);
    if bits.len() < 648 {
        return None;
    }
    let mut corrected = bits;
    for &st in HDU_GOLAY_STARTS.iter() {
        let mut cw = [false; 18];
        for b in 0..18 {
            cw[b] = corrected[st + b];
        }
        let _ = golay18_correct(&mut cw);
        for b in 0..18 {
            corrected[st + b] = cw[b];
        }
    }
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected[start + b] { 1 } else { 0 };
        }
        v
    };
    // SDRTrunk's RS input order:
    //   input[0..=15]  = RS_HEX_15..0 (reverse)
    //   input[16..=35] = CW_HEX_19..0 (reverse)
    //   input[36..63]  = 0 (virtual padding for the full 63-symbol code)
    let mut rs_input = [0u32; 63];
    for i in 0..16 {
        rs_input[i] = hex_at(HDU_RS_HEX_POSITIONS[15 - i]);
    }
    for i in 0..20 {
        rs_input[16 + i] = hex_at(HDU_CW_HEX_POSITIONS[19 - i]);
    }
    let rs_output = match super::fec::rs_63_47_17::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    // Corrected CW hexbits at output[35..=16]; pack into the 120-bit
    // header in CW_HEX_0..=CW_HEX_19 order (bit 0 = MSB of CW_HEX_0
    // = first MI bit).
    let mut header_bits = [false; 120];
    for i in 0..20 {
        let hex = rs_output[35 - i];
        for b in 0..6 {
            header_bits[i * 6 + b] = ((hex >> (5 - b)) & 1) != 0;
        }
    }
    let bytes_fn = |start: usize, n: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..n {
            v = (v << 1) | if header_bits[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut mi = [0u8; 9];
    for i in 0..9 {
        mi[i] = bytes_fn(i * 8, 8) as u8;
    }
    let algorithm_id = bytes_fn(80, 8) as u8;
    let key_id = bytes_fn(88, 16) as u16;
    let talkgroup = bytes_fn(104, 16) as u16;
    Some(HduHeader {
        talkgroup,
        key_id,
        algorithm_id,
        message_indicator: mi,
    })
}

// ── LDU2 ESS (Encryption Sync Signature) parsing ────────────────────
//
// LDU2 carries 9 IMBE voice frames + a 96-bit ESS that refreshes the
// encryption state every LDU (~180 ms). ESS is protected by:
//
// - 24 Hamming(10,6,3) codewords at SDRTrunk's `GOLAY_WORD_STARTS`
//   (288, 298, ..., 1238 -- scattered between IMBE frames, NOT
//   contiguous).
// - RS(24,16,9) across the 24 resulting hexbits (16 CW + 8 RS parity).
//
// Decoded ESS layout per `EncryptionSyncParameters.java`:
//     * bits  0-71: Message Indicator (refreshed per LDU2)
//     * bits 72-79: Algorithm ID
//     * bits 80-95: Key ID

/// LDU2 Hamming(10,6,3) codeword starts. 24 codewords; the first 16
/// carry CW hexbits, the last 8 carry RS parity.
const LDU2_HAMMING_STARTS: [usize; 24] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686,
    840, 850, 860, 870, 1024, 1034, 1044, 1054, 1208, 1218, 1228, 1238,
];
const LDU2_CW_HEX_POSITIONS: [usize; 16] = [
    288, 298, 308, 318, 472, 482, 492, 502, 656, 666, 676, 686,
    840, 850, 860, 870,
];
const LDU2_RS_HEX_POSITIONS: [usize; 8] = [
    1024, 1034, 1044, 1054, 1208, 1218, 1228, 1238,
];

/// Parsed LDU2 Encryption Sync Signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ldu2Ess {
    /// 8-bit algorithm id. `0x80` = unencrypted.
    pub algorithm_id: u8,
    pub key_id: u16,
    /// 72-bit message indicator; changes every LDU2 on encrypted calls.
    pub message_indicator: [u8; 9],
}

impl Ldu2Ess {
    pub fn is_encrypted(&self) -> bool {
        self.algorithm_id != super::wire::ALGORITHM_CLEAR
    }

    /// See [`HduHeader::is_spec_algorithm`].
    pub fn is_spec_algorithm(&self) -> bool {
        is_spec_algorithm_id(self.algorithm_id)
    }
}

/// Set of algorithm IDs recognised by TIA-102.AABD + widely-used
/// Motorola extensions catalogued in SDRTrunk `Encryption.java`.
///
/// LDU2 ESS FEC [RS(24,16,9) over GF(2^6)] and HDU FEC
/// [Golay(18,6,8) + RS(63,47,17)] both accept near-valid codewords
/// even when input bits are corrupt. A successful decode with
/// `algorithm_id` outside this set is evidence of a bit-corrupt
/// codeword, not a new algorithm. The `call_encrypted` gate on a
/// clear talkgroup rejects these phantoms.
pub fn is_spec_algorithm_id(id: u8) -> bool {
    matches!(
        id,
        // Type-1 (classified) algorithms
        0x00 | 0x01 | 0x02 | 0x03 | 0x04 | 0x05 | 0x41
        // Mainstream open standards
        | 0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x88 | 0x89
        // Motorola + SDRTrunk-catalogued 0x9F..=0xB0 range
        | 0x9F | 0xA0 | 0xA1 | 0xA2 | 0xA3 | 0xA4 | 0xA5 | 0xA6
        | 0xA7 | 0xA8 | 0xA9 | 0xAA | 0xAB | 0xAC | 0xAD | 0xAE
        | 0xAF | 0xB0
    )
}

/// Parse the LDU2 ESS through Hamming10 + RS(24,16,9). `body_raw`
/// must be the full 807-dibit LDU2 body (including 23 body status
/// dibits).
pub fn parse_ldu2_ess(body_raw: &[u8]) -> Option<Ldu2Ess> {
    if body_raw.len() != LDU_RAW_DIBITS {
        return None;
    }
    let data_dibits = strip_body_status_dibits(body_raw);
    if data_dibits.len() != DataUnit::Ldu2.data_dibits() {
        return None;
    }
    let bits = dibits_to_bits(&data_dibits);
    if bits.len() < 1248 {
        return None;
    }
    let mut corrected = bits;
    for &st in LDU2_HAMMING_STARTS.iter() {
        let mut cw = [false; 10];
        for b in 0..10 {
            cw[b] = corrected[st + b];
        }
        let _ = hamming10_correct(&mut cw);
        for b in 0..10 {
            corrected[st + b] = cw[b];
        }
    }
    let hex_at = |start: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..6 {
            v = (v << 1) | if corrected[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut rs_input = [0u32; 63];
    for i in 0..8 {
        rs_input[i] = hex_at(LDU2_RS_HEX_POSITIONS[7 - i]);
    }
    for i in 0..16 {
        rs_input[8 + i] = hex_at(LDU2_CW_HEX_POSITIONS[15 - i]);
    }
    let rs_output = match super::fec::rs_24_16_9::decode(&rs_input) {
        Ok(v) => v,
        Err(v) => v,
    };
    let mut ess_bits = [false; 96];
    for i in 0..16 {
        let hex = rs_output[23 - i];
        for b in 0..6 {
            ess_bits[i * 6 + b] = ((hex >> (5 - b)) & 1) != 0;
        }
    }
    let bytes_fn = |start: usize, n: usize| -> u32 {
        let mut v = 0u32;
        for b in 0..n {
            v = (v << 1) | if ess_bits[start + b] { 1 } else { 0 };
        }
        v
    };
    let mut mi = [0u8; 9];
    for i in 0..9 {
        mi[i] = bytes_fn(i * 8, 8) as u8;
    }
    let algorithm_id = bytes_fn(72, 8) as u8;
    let key_id = bytes_fn(80, 16) as u16;
    Some(Ldu2Ess {
        algorithm_id,
        key_id,
        message_indicator: mi,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golay-encode 12 data bits -> 24-bit codeword (data + 11 parity
    /// bits from CHECKSUMS + overall parity at bit 23). Builds
    /// known-good codewords for unit tests.
    fn golay_encode(data: u16) -> [bool; 24] {
        let mut cw = [false; 24];
        for i in 0..12 {
            cw[i] = (data >> (11 - i)) & 1 != 0;
        }
        let mut syn: u16 = 0;
        for i in 0..12 {
            if cw[i] {
                syn ^= GOLAY24_CHECKSUMS[i];
            }
        }
        // Spread syn across positions 12..22: bit 10 -> cw[12],
        // bit 0 -> cw[22].
        for i in 0..11 {
            cw[12 + i] = (syn >> (10 - i)) & 1 != 0;
        }
        // Overall parity: bit 23 = XOR of all other bits.
        let mut parity = false;
        for i in 0..23 {
            if cw[i] {
                parity ^= true;
            }
        }
        cw[23] = parity;
        cw
    }

    #[test]
    fn golay24_no_errors_syndrome_zero() {
        let cw = golay_encode(0x345);
        assert_eq!(golay24_syndrome(&cw), 0);
    }

    #[test]
    fn golay24_corrects_single_bit_errors() {
        for flip in 0..23 {
            let mut cw = golay_encode(0x345);
            cw[flip] ^= true;
            let n = golay24_correct(&mut cw).expect("single-bit correctable");
            assert_eq!(n, 1, "flip@{}", flip);
            // Data bits should match the original 0x345.
            let mut got: u16 = 0;
            for i in 0..12 {
                got = (got << 1) | if cw[i] { 1 } else { 0 };
            }
            assert_eq!(got, 0x345, "flip@{} data corrupted", flip);
        }
    }

    #[test]
    fn golay24_corrects_double_bit_errors() {
        for a in 0..23 {
            for b in (a + 1)..23 {
                let mut cw = golay_encode(0xABC);
                cw[a] ^= true;
                cw[b] ^= true;
                let n = golay24_correct(&mut cw)
                    .expect("double-bit correctable");
                assert!(n <= 2, "weight should be 1 or 2 (got {})", n);
                let mut got: u16 = 0;
                for i in 0..12 {
                    got = (got << 1) | if cw[i] { 1 } else { 0 };
                }
                assert_eq!(got, 0xABC, "flip@{},{} data corrupted", a, b);
            }
        }
    }

    /// 807 dibits with status dibits stripped -> 784 data dibits.
    #[test]
    fn strip_807_dibits_yields_784() {
        let raw: Vec<u8> = (0..807).map(|i| (i & 0x03) as u8).collect();
        let stripped = strip_body_status_dibits(&raw);
        assert_eq!(stripped.len(), 784);
        // Verify the first few removed positions are exactly the
        // body status positions {13, 49, 85, ...}.
        let removed: Vec<usize> = (0..807)
            .filter(|p| is_body_status_dibit(*p))
            .collect();
        assert_eq!(removed[0], 13);
        assert_eq!(removed[1], 49);
        assert_eq!(removed[2], 85);
        assert_eq!(removed[3], 121);
        assert_eq!(removed.len(), 23);
    }

    /// dibits_to_bits packs dibit MSB first: 0b10 -> [true, false],
    /// 0b01 -> [false, true], 0b11 -> [true, true], 0b00 -> [false, false].
    #[test]
    fn dibits_to_bits_msb_first() {
        let dibits = vec![0b10, 0b01, 0b11, 0b00];
        let bits = dibits_to_bits(&dibits);
        assert_eq!(
            bits,
            vec![true, false, false, true, true, true, false, false]
        );
    }

    /// All-ones body: every IMBE frame should be 18 bytes of 0xFF.
    #[test]
    fn extract_all_ones_body_yields_all_ones_frames() {
        let raw = vec![0b11_u8; LDU_RAW_DIBITS];
        let frames = extract_imbe_frames(&raw).expect("len matches");
        for (i, frame) in frames.iter().enumerate() {
            for (j, &b) in frame.bits.iter().enumerate() {
                assert_eq!(b, 0xFF, "frame {} byte {}", i, j);
            }
        }
    }

    /// Marker bit at IMBE frame 0 bit 0 should land at frame 0
    /// byte 0 bit 7.
    #[test]
    fn extract_first_bit_position() {
        // Set body data dibit 0 to 0b10 -> bit 0 = true, bit 1 = false.
        // Frame 0 starts at bit 0 so byte 0 bit 7 is the marker.
        let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
        raw[0] = 0b10; // body dibit 0
        let frames = extract_imbe_frames(&raw).expect("len matches");
        assert_eq!(
            frames[0].bits[0], 0x80,
            "frame 0 byte 0 should have bit 7 set (= IMBE bit 0)"
        );
        for &b in &frames[0].bits[1..] {
            assert_eq!(b, 0x00);
        }
    }

    /// Frame 1 starts at LDU data bit 144 (body data dibit 72).
    /// Body data 72 = body raw 74 after skipping status dibits at
    /// raw positions {13, 49}.
    #[test]
    fn extract_frame_1_marker_bit() {
        let mut raw = vec![0b00_u8; LDU_RAW_DIBITS];
        raw[74] = 0b11;
        let frames = extract_imbe_frames(&raw).expect("len matches");
        // All frame 0 bytes should be zero.
        for &b in &frames[0].bits {
            assert_eq!(b, 0x00);
        }
        // Frame 1 byte 0 should have bits 7 and 6 set = 0xC0.
        assert_eq!(frames[1].bits[0], 0xC0);
        for &b in &frames[1].bits[1..] {
            assert_eq!(b, 0x00);
        }
    }
}

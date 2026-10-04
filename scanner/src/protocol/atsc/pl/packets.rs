//! The PL's output in the lane ring's packets (079's lane packet, 4 KB of 64-bit words, a
//! format of its own): an 8-word header, then 19 codewords of 26 words each, 207 bytes and the
//! syndrome flag. 12,894 codewords a second fill 679 packets, 2.78 MB/s.
//!
//! | Word | Bits | Field |
//! |---|---|---|
//! | 0 | 15:0 | magic `0x5243` |
//! | 0 | 19:16 | format (2: 8-VSB codewords) |
//! | 0 | 23:20 | lane (15: the 8-VSB block) |
//! | 0 | 31:24 | flags: bit 0 lost (codewords dropped before this packet), bit 1 segments counted, bit 2 fields counted |
//! | 0 | 47:32 | codewords in the packet (at most 19) |
//! | 0 | 63:48 | sequence |
//! | 1 | 63:0 | sample index: the AD9361 sample at the first codeword's field sync |
//! | 2 | 31:0 | the first codeword's field (counted from the first field sync found) |
//! | 2 | 40:32 | its place in the field (0-311) |
//! | 2 | 63:48 | tap sets loaded before its field |
//! | 3 | 63:0 | its field's error energy: Σ (output − level)² over the data symbols, in 1/256 of a level² |
//! | 4 | 31:0 | the carrier loop's frequency at its field sync (θ a symbol beyond the quarter turn, 2^32 a turn) |
//! | 4 | 63:32 | the timing loop's rate (2^-32 samples a symbol beyond the nominal step) |
//! | 5 | 23:0 | the gain word |
//! | 5 | 63:32 | the field sync segment's sync correlation (128 a level) |
//! | 6 | | reserved |
//! | 7 | 31:0 | check: the XOR of the packet's other 1023 32-bit halves |
//! | 8 + 26c | | codeword c: bytes 0-206, then the flag (bit 0: a syndrome is not zero) |

use super::decoder::Codeword;
use super::front::{Field, Output};
use super::{FieldResult, Widths};
use crate::protocol::atsc::fec::rs;

pub const PACKET: usize = 4096;
pub const PER_PACKET: usize = 19;
const HEADER: usize = 64;
const SLOT: usize = 208;
const MAGIC: u64 = 0x5243;
const FORMAT: u64 = 2;
const LANE: u64 = 15;

/// A packet's header, as the PS reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Header {
    pub flags: u8,
    pub count: usize,
    pub sequence: u16,
    pub sample_index: u64,
    pub field: u32,
    pub index: u16,
    pub taps: u16,
    pub error: u64,
    pub freq: i32,
    pub rate: i32,
    pub gain: u32,
    pub sync: i32,
}

fn word(p: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(p[8 * i..8 * i + 8].try_into().unwrap_or([0; 8]))
}

fn check(p: &[u8]) -> u32 {
    p.chunks_exact(4).enumerate().filter(|(i, _)| *i != 14).fold(0, |a, (_, c)| a ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

/// The codewords in packets, each field's header fields from the front end and the decoder.
pub fn pack(codewords: &[Codeword], fields: &[Field], front: &Output, results: &[FieldResult], w: &Widths) -> Vec<[u8; PACKET]> {
    let unit = 2.0 * f64::from(w.eq_out as i32 - 6);
    let scale = 256.0 / 2f64.powf(unit);
    let mut out = Vec::new();
    for (seq, chunk) in codewords.chunks(PER_PACKET).enumerate() {
        let mut p = [0u8; PACKET];
        let c0 = &chunk[0];
        let field = fields.get(c0.field);
        let seg = field.and_then(|f| front.segments.iter().find(|s| s.start == f.start)).copied().unwrap_or_default();
        let result = results.get(c0.field).copied().unwrap_or_default();
        let words: [u64; 7] = [
            MAGIC | FORMAT << 16 | LANE << 20 | 0b110 << 24 | (chunk.len() as u64) << 32 | ((seq as u64) & 0xFFFF) << 48,
            seg.pos >> 32,
            c0.field as u64 | (c0.index as u64) << 32 | ((result.taps as u64) & 0xFFFF) << 48,
            (result.error * scale).round() as u64,
            u64::from(seg.freq as i32 as u32) | u64::from(seg.rate as i32 as u32) << 32,
            u64::from(seg.gain & 0xFF_FFFF) | u64::from(seg.sync as i32 as u32) << 32,
            0,
        ];
        for (i, v) in words.iter().enumerate() {
            p[8 * i..8 * i + 8].copy_from_slice(&v.to_le_bytes());
        }
        for (c, cw) in chunk.iter().enumerate() {
            let at = HEADER + SLOT * c;
            p[at..at + rs::N].copy_from_slice(&cw.bytes);
            p[at + rs::N] = u8::from(cw.flag);
        }
        let x = check(&p);
        p[56..60].copy_from_slice(&x.to_le_bytes());
        out.push(p);
    }
    out
}

/// One codeword as the PS reads it: field, place in the field, bytes, flag.
pub type Read = (u32, u16, [u8; rs::N], bool);

/// A packet's header and codewords; `None` for another format or a failed check.
pub fn unpack(p: &[u8; PACKET]) -> Option<(Header, Vec<Read>)> {
    let w0 = word(p, 0);
    if w0 & 0xFFFF != MAGIC || (w0 >> 16) & 0xF != FORMAT || u64::from(check(p)) != word(p, 7) & 0xFFFF_FFFF {
        return None;
    }
    let (w2, w4, w5) = (word(p, 2), word(p, 4), word(p, 5));
    let h = Header {
        flags: (w0 >> 24) as u8,
        count: ((w0 >> 32) & 0xFFFF) as usize,
        sequence: (w0 >> 48) as u16,
        sample_index: word(p, 1),
        field: w2 as u32,
        index: ((w2 >> 32) & 0x1FF) as u16,
        taps: (w2 >> 48) as u16,
        error: word(p, 3),
        freq: w4 as u32 as i32,
        rate: (w4 >> 32) as u32 as i32,
        gain: (w5 & 0xFF_FFFF) as u32,
        sync: (w5 >> 32) as u32 as i32,
    };
    let mut out = Vec::with_capacity(h.count);
    let (mut field, mut index) = (h.field, h.index);
    for c in 0..h.count.min(PER_PACKET) {
        let at = HEADER + SLOT * c;
        let mut b = [0u8; rs::N];
        b.copy_from_slice(&p[at..at + rs::N]);
        out.push((field, index, b, p[at + rs::N] & 1 != 0));
        index += 1;
        if usize::from(index) == crate::protocol::atsc::vsb::DATA_SEGMENTS {
            index = 0;
            field += 1;
        }
    }
    Some((h, out))
}

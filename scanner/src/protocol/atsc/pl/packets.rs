//! The model's codewords in the lane ring's format 2 packets, laid out as `live::packets`
//! describes and reads them: 12,894 codewords a second fill 679 packets, 2.78 MB/s.

use super::decoder::Codeword;
use super::front::{Field, Output};
use super::{FieldResult, Widths};
use crate::protocol::atsc::fec::rs;
pub use crate::protocol::atsc::live::packets::{unpack, Header, Read, PACKET, PER_PACKET};
use crate::protocol::atsc::live::packets::{check, CODEWORDS, HEADER, LANE, MAGIC, SLOT};

/// The codewords in packets, each field's header fields from the front end and the decoder;
/// `tracking`: the symbol from which the timing loop tracks.
pub fn pack(codewords: &[Codeword], fields: &[Field], front: &Output, results: &[FieldResult], w: &Widths, tracking: usize) -> Vec<[u8; PACKET]> {
    let unit = 2.0 * f64::from(w.eq_out as i32 - 6);
    let scale = 256.0 / 2f64.powf(unit);
    let mut out = Vec::new();
    for (seq, chunk) in codewords.chunks(PER_PACKET).enumerate() {
        let mut p = [0u8; PACKET];
        let c0 = &chunk[0];
        let field = fields.get(c0.field);
        let seg = field.and_then(|f| front.segments.iter().find(|s| s.start == f.start)).copied().unwrap_or_default();
        let result = results.get(c0.field).copied().unwrap_or_default();
        let before = c0.field.checked_sub(1).and_then(|f| results.get(f)).copied().unwrap_or_default();
        let flags: u64 = 0b110 | u64::from(field.is_some_and(|f| f.start > tracking)) << 3;
        let words: [u64; 7] = [
            MAGIC | CODEWORDS << 16 | LANE << 20 | flags << 24 | (chunk.len() as u64) << 32 | ((seq as u64) & 0xFFFF) << 48,
            seg.pos >> 32,
            c0.field as u64 | (c0.index as u64) << 32 | ((result.taps as u64) & 0xFFFF) << 48,
            (before.error * scale).round() as u64,
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

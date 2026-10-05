//! The PS half on the model's output, as `live` runs it on the block's.
//!
//! - **Taps:** from a snapshot of the equalizer's input (the segment before a field sync, the
//!   field sync, and data segments after it), by 081's least squares (`live::taps`).
//! - **The stream:** each packet's codewords, the flagged ones corrected by 081's Reed-Solomon
//!   decoder, derandomized, and read for PSIP and the programs' streams as 081 reads its own.

use super::packets::{self, PACKET};
use super::Widths;
use crate::protocol::atsc::fec::{self, rs};
pub use crate::protocol::atsc::live::taps::{solve, Snapshot};
use crate::protocol::atsc::live::transport;
use crate::protocol::atsc::psip::Psip;
use crate::protocol::atsc::receiver;
use crate::protocol::atsc::streams::Multiplex;
use crate::protocol::atsc::vsb::SEGMENT;

/// The snapshot of field sync segment `start` of the equalizer's input `y`: the segment before
/// it, it, and `segments` data segments.
pub fn snapshot(y: &[i32], start: usize, inverted: bool, segments: usize, w: &Widths) -> Option<Snapshot> {
    let from = start.checked_sub(SEGMENT)?;
    let to = start + (segments + 1) * SEGMENT;
    let scale = 1.0 / f64::from(1u32 << (w.y - 5));
    Some(Snapshot { y: y.get(from..to)?.iter().map(|&v| (f64::from(v) * scale) as f32).collect(), sync: SEGMENT, inverted })
}

/// What the PS reads from the packets.
pub struct Station {
    pub psip: Psip,
    pub multiplex: Multiplex,
    pub packets: usize,
    pub corrected: usize,
    pub failed: usize,
    /// Each field's MER from the headers (the first packet of the field after it), dB.
    pub mer_db: Vec<(u32, f64)>,
}

pub fn station(packets: &[[u8; PACKET]]) -> Station {
    let (out, mer_db) = stream(packets);
    let corrected = out.iter().filter(|p| matches!(p.outcome, rs::Outcome::Corrected(_))).count();
    let failed = out.iter().filter(|p| p.outcome == rs::Outcome::Failed).count();
    let (psip, multiplex) = receiver::read(&out);
    Station { psip, multiplex, packets: out.len(), corrected, failed, mer_db }
}

/// The transport stream in the packets: their codewords corrected where flagged and
/// derandomized, in order; and each field's MER from the headers.
pub fn stream(packets: &[[u8; PACKET]]) -> (Vec<fec::Packet>, Vec<(u32, f64)>) {
    let mut out = Vec::new();
    let mut mer_db: Vec<(u32, f64)> = Vec::new();
    for p in packets {
        let Some((h, cws)) = packets::unpack(p) else { continue };
        // The header carries the error energy of the field before its own.
        if h.field > 0 && mer_db.last().is_none_or(|m| m.0 != h.field - 1) {
            if let Some(db) = h.mer_before_db() {
                mer_db.push((h.field - 1, db));
            }
        }
        out.extend(cws.iter().map(|(_, index, bytes, flag)| transport(*index, bytes, *flag)));
    }
    (out, mer_db)
}

/// Packets as a transport stream file's bytes: those Reed-Solomon could not correct carry the
/// transport error indicator, so that a player skips them.
pub fn ts_bytes(packets: &[fec::Packet]) -> Vec<u8> {
    let mut out = Vec::with_capacity(packets.len() * 188);
    for p in packets {
        let mut b = p.bytes;
        if p.outcome == rs::Outcome::Failed {
            b[1] |= 0x80;
        }
        out.extend_from_slice(&b);
    }
    out
}

//! The PS half on the model's output.
//!
//! - **Taps:** from a snapshot of the equalizer's input (the segment before a field sync, the
//!   field sync, and data segments after it), by 081's least squares: the field sync's known
//!   symbols (or, if asked, the last taps), then rounds on the taps' own decisions over runs in
//!   the snapshot.
//! - **The stream:** each packet's codewords, the flagged ones corrected by 081's Reed-Solomon
//!   decoder, derandomized, and read for PSIP and the programs' streams as 081 reads its own.

use super::packets::{self, PACKET};
use super::study::solve_on;
use super::Widths;
use crate::protocol::atsc::fec::{self, randomizer, rs};
use crate::protocol::atsc::psip::Psip;
use crate::protocol::atsc::receiver;
use crate::protocol::atsc::streams::Multiplex;
use crate::protocol::atsc::vsb::{DATA_SEGMENTS, SEGMENT};

/// The equalizer's input around a field sync, in levels.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub y: Vec<f32>,
    /// Where the field sync segment starts in `y`.
    pub sync: usize,
    pub inverted: bool,
}

/// The snapshot of field sync segment `start` of the equalizer's input `y`: the segment before
/// it, it, and `segments` data segments.
pub fn snapshot(y: &[i32], start: usize, inverted: bool, segments: usize, w: &Widths) -> Option<Snapshot> {
    let from = start.checked_sub(SEGMENT)?;
    let to = start + (segments + 1) * SEGMENT;
    let scale = 1.0 / f64::from(1u32 << (w.y - 5));
    Some(Snapshot { y: y.get(from..to)?.iter().map(|&v| (f64::from(v) * scale) as f32).collect(), sync: SEGMENT, inverted })
}

/// The taps from a snapshot; `start`: the last taps, else the field sync's known symbols first.
pub fn solve(s: &Snapshot, start: Option<Vec<f32>>, rounds: usize) -> Option<Vec<f32>> {
    solve_on(&s.y, s.sync, s.inverted, s.sync + SEGMENT..s.y.len(), rounds, start)
}

/// What the PS reads from the packets.
pub struct Station {
    pub psip: Psip,
    pub multiplex: Multiplex,
    pub packets: usize,
    pub corrected: usize,
    pub failed: usize,
    /// Each field's MER from the headers (its first codeword's packet), dB.
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
    let rand = randomizer();
    let mut out = Vec::new();
    let mut mer_db: Vec<(u32, f64)> = Vec::new();
    for p in packets {
        let Some((h, cws)) = packets::unpack(p) else { continue };
        if mer_db.last().is_none_or(|m| m.0 != h.field) && h.error > 0 {
            let symbols = (DATA_SEGMENTS * (SEGMENT - 4)) as f64;
            mer_db.push((h.field, 10.0 * (symbols * 21.0 * 256.0 / h.error as f64).log10()));
        }
        for (_, index, bytes, flag) in cws {
            let mut cw = bytes;
            let outcome = if flag { rs::decode(&mut cw) } else { rs::Outcome::Clean };
            let mut b = [0u8; 188];
            b[0] = 0x47;
            let r = &rand[usize::from(index) * rs::K..];
            for (j, v) in b[1..].iter_mut().enumerate() {
                *v = cw[j] ^ r[j];
            }
            out.push(fec::Packet { bytes: b, outcome });
        }
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

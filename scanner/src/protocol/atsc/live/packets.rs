//! The 8-VSB block's packets in the lane ring (079's lane packet, 4 KB of 64-bit little-endian
//! words, lane 15; doc/changes/085): format 2 carries codewords, format 3 snapshots of the
//! equalizer's input. Word 7's low half makes every packet's 32-bit halves XOR to zero.
//!
//! Format 2: an 8-word header, then 19 codewords of 26 words each, 207 bytes and the flag.
//!
//! | Word | Bits | Field |
//! |---|---|---|
//! | 0 | 15:0 | magic `0x5243` |
//! | 0 | 19:16 | format (2) |
//! | 0 | 23:20 | lane (15) |
//! | 0 | 31:24 | flags: bit 0 lost (codewords dropped before this packet), bit 1 segments counted, bit 2 fields counted, bit 3 the timing loop tracks (past its acquisition gear at the first codeword's field sync: the PS's first snapshot waits for it), bit 4 a run after the fields were lost |
//! | 0 | 47:32 | codewords in the packet (at most 19) |
//! | 0 | 63:48 | sequence |
//! | 1 | 63:0 | the AD9361 sample index at the first codeword's field sync |
//! | 2 | 31:0 | the first codeword's field (counted from the first field sync found) |
//! | 2 | 40:32 | its place in the field (0-311) |
//! | 2 | 63:48 | tap sets loaded before its field |
//! | 3 | 63:0 | the error energy of the field before its field (0 for the first): Σ (output − level)² over the data symbols, in 1/256 of a level². A field's first codewords leave the deinterleaver long before its last symbol is equalized, so a packet carries the energy of the field before, which is complete by then |
//! | 4 | 31:0 | the carrier loop's frequency at its field sync (θ a symbol beyond the quarter turn, 2^32 a turn) |
//! | 4 | 63:32 | the timing loop's rate (2^-32 samples a symbol beyond the nominal step) |
//! | 5 | 23:0 | the gain word |
//! | 5 | 63:32 | the field sync segment's sync correlation (128 a level) |
//! | 6 | | reserved |
//! | 7 | 31:0 | check: the XOR of the packet's other 1023 32-bit halves |
//! | 8 + 26c | | codeword c: bytes 0-206, then the flag (bit 0: a syndrome is not zero) |
//!
//! Format 3: the segment before a field sync, the field sync segment and 10 data segments (9,984
//! values) in five packets of up to 2,016, four 16-bit values a word, the earliest lowest.
//!
//! | Word | Bits | Field |
//! |---|---|---|
//! | 0 | 31:0 | magic, format 3, lane 15; flags: bit 0 lost (the snapshot is incomplete), bits 1-3 as format 2's, bit 4 the field sync's middle PN63 inverted, bit 5 its PN511 passed the threshold |
//! | 0 | 47:32 | values in the packet |
//! | 0 | 63:48 | sequence |
//! | 1 | 63:0 | the AD9361 sample index at the snapshot's field sync |
//! | 2 | 31:0 | the field of the snapshot's field sync |
//! | 2 | 47:32 | the place of the packet's first value in the snapshot |
//! | 2 | 63:48 | tap sets loaded before that field |

use crate::protocol::atsc::fec::rs;
use crate::protocol::atsc::vsb::{DATA_SEGMENTS, SEGMENT};

pub const PACKET: usize = 4096;
pub const PER_PACKET: usize = 19;
pub const HEADER: usize = 64;
pub const SLOT: usize = 208;
pub const MAGIC: u64 = 0x5243;
pub const CODEWORDS: u64 = 2;
pub const SNAPSHOT: u64 = 3;
pub const LANE: u64 = 15;
/// A snapshot: the segment before a field sync, the field sync segment, 10 data segments.
pub const SNAPSHOT_SEGMENTS: usize = 12;
pub const SNAPSHOT_VALUES: usize = SNAPSHOT_SEGMENTS * SEGMENT;
pub const VALUES_PER_PACKET: usize = (PACKET - HEADER) / 2;

pub const FLAG_LOST: u8 = 1 << 0;
pub const FLAG_TRACKING: u8 = 1 << 3;
/// Codewords: a run after the fields were lost.
pub const FLAG_RERUN: u8 = 1 << 4;
/// Snapshots: the field sync's middle PN63 inverted; its PN511 found.
pub const FLAG_INVERTED: u8 = 1 << 4;
#[cfg(test)]
pub const FLAG_FOUND: u8 = 1 << 5;

/// A codeword packet's header.
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

impl Header {
    /// The MER of the field before this packet's field, from its error energy (`None` without it).
    pub fn mer_before_db(&self) -> Option<f64> {
        if self.error == 0 {
            return None;
        }
        // 21 is the mean square of the eight levels; the energy is in 1/256 of a level².
        let symbols = (DATA_SEGMENTS * (SEGMENT - 4)) as f64;
        Some(10.0 * (symbols * 21.0 * 256.0 / self.error as f64).log10())
    }
}

/// One codeword as read: field, place in the field, bytes, flag.
pub type Read = (u32, u16, [u8; rs::N], bool);

pub fn word(p: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(p[8 * i..8 * i + 8].try_into().unwrap_or([0; 8]))
}

/// The XOR of the packet's 32-bit halves but word 7's low one.
pub fn check(p: &[u8]) -> u32 {
    p.chunks_exact(4).enumerate().filter(|(i, _)| *i != 14).fold(0, |a, (_, c)| a ^ u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

/// The block's packet format, if `p` is a whole packet of lane 15 whose check holds.
pub fn format(p: &[u8]) -> Option<u64> {
    if p.len() != PACKET {
        return None;
    }
    let w0 = word(p, 0);
    let f = (w0 >> 16) & 0xF;
    (w0 & 0xFFFF == MAGIC && (w0 >> 20) & 0xF == LANE && (f == CODEWORDS || f == SNAPSHOT) && u64::from(check(p)) == word(p, 7) & 0xFFFF_FFFF)
        .then_some(f)
}

/// A codeword packet's header and codewords; `None` for another format or a failed check.
pub fn unpack(p: &[u8; PACKET]) -> Option<(Header, Vec<Read>)> {
    if format(p) != Some(CODEWORDS) {
        return None;
    }
    let (w0, w2, w4, w5) = (word(p, 0), word(p, 2), word(p, 4), word(p, 5));
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
        if usize::from(index) == DATA_SEGMENTS {
            index = 0;
            field += 1;
        }
    }
    Some((h, out))
}

/// A snapshot packet's header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotHeader {
    pub flags: u8,
    pub sequence: u16,
    pub sample_index: u64,
    pub field: u32,
    /// The place of the packet's first value in the snapshot.
    pub place: usize,
    pub taps: u16,
}

/// A snapshot packet's header and values; `None` for another format or a failed check.
pub fn unpack_snapshot(p: &[u8; PACKET]) -> Option<(SnapshotHeader, Vec<i16>)> {
    if format(p) != Some(SNAPSHOT) {
        return None;
    }
    let (w0, w2) = (word(p, 0), word(p, 2));
    let count = (((w0 >> 32) & 0xFFFF) as usize).min(VALUES_PER_PACKET);
    let h = SnapshotHeader {
        flags: (w0 >> 24) as u8,
        sequence: (w0 >> 48) as u16,
        sample_index: word(p, 1),
        field: w2 as u32,
        place: ((w2 >> 32) & 0xFFFF) as usize,
        taps: (w2 >> 48) as u16,
    };
    let values = p[HEADER..HEADER + 2 * count].chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect();
    Some((h, values))
}

/// A snapshot gathered from its packets.
#[derive(Debug, Clone, Default)]
pub struct Gathered {
    pub field: u32,
    pub flags: u8,
    pub values: Vec<i16>,
}

/// Gathers a snapshot's packets, which come in order: the one being gathered, if any.
#[derive(Debug, Default)]
pub struct Gatherer {
    current: Option<Gathered>,
}

impl Gatherer {
    /// Takes a snapshot packet; returns the snapshot it completes. A packet out of its place
    /// drops what was gathered.
    pub fn push(&mut self, h: &SnapshotHeader, values: &[i16]) -> Option<Gathered> {
        if h.place == 0 {
            self.current = Some(Gathered { field: h.field, flags: h.flags, values: Vec::with_capacity(SNAPSHOT_VALUES) });
        }
        let g = self.current.as_mut()?;
        if g.field != h.field || g.values.len() != h.place {
            self.current = None;
            return None;
        }
        g.flags |= h.flags & FLAG_LOST;
        g.values.extend_from_slice(values);
        if g.values.len() >= SNAPSHOT_VALUES {
            return self.current.take().filter(|g| g.values.len() == SNAPSHOT_VALUES && g.flags & FLAG_LOST == 0);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(words: &[(usize, u64)], body: impl Fn(&mut [u8])) -> [u8; PACKET] {
        let mut p = [0u8; PACKET];
        for &(i, w) in words {
            p[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        body(&mut p[HEADER..]);
        let x = check(&p);
        p[56..60].copy_from_slice(&x.to_le_bytes());
        p
    }

    #[test]
    fn snapshots_gather_from_their_five_packets() {
        let mut g = Gatherer::default();
        let mut done = None;
        for k in 0..5 {
            let count = if k < 4 { VALUES_PER_PACKET } else { SNAPSHOT_VALUES - 4 * VALUES_PER_PACKET };
            let w0 = MAGIC | SNAPSHOT << 16 | LANE << 20 | u64::from(FLAG_TRACKING | FLAG_FOUND) << 24 | (count as u64) << 32 | (k as u64) << 48;
            let w2 = 7 | ((VALUES_PER_PACKET * k) as u64) << 32 | 3 << 48;
            let p = packet(&[(0, w0), (1, 12345), (2, w2)], |b| {
                for v in 0..count {
                    let x = ((VALUES_PER_PACKET * k + v) as i16).wrapping_mul(3);
                    b[2 * v..2 * v + 2].copy_from_slice(&x.to_le_bytes());
                }
            });
            assert_eq!(format(&p), Some(SNAPSHOT));
            let (h, values) = unpack_snapshot(&p).unwrap();
            assert_eq!((h.field, h.place, h.taps, values.len()), (7, VALUES_PER_PACKET * k, 3, count));
            done = g.push(&h, &values);
            assert_eq!(done.is_some(), k == 4);
        }
        let s = done.unwrap();
        assert_eq!((s.field, s.flags), (7, FLAG_TRACKING | FLAG_FOUND));
        assert!(s.values.iter().enumerate().all(|(i, &v)| v == (i as i16).wrapping_mul(3)));
    }

    #[test]
    fn a_packet_out_of_place_drops_the_snapshot() {
        let mut g = Gatherer::default();
        let h = |place| SnapshotHeader { field: 3, place, ..Default::default() };
        assert!(g.push(&h(0), &[0; VALUES_PER_PACKET]).is_none());
        assert!(g.push(&h(2 * VALUES_PER_PACKET), &[0; VALUES_PER_PACKET]).is_none());
        assert!(g.push(&h(3 * VALUES_PER_PACKET), &[0; VALUES_PER_PACKET]).is_none());
        assert!(g.current.is_none());
    }

    #[test]
    fn a_failed_check_or_another_lane_is_not_a_packet() {
        let w0 = MAGIC | CODEWORDS << 16 | LANE << 20;
        let mut p = packet(&[(0, w0)], |_| {});
        assert_eq!(format(&p), Some(CODEWORDS));
        p[100] ^= 1;
        assert_eq!(format(&p), None);
        let p = packet(&[(0, MAGIC | CODEWORDS << 16 | 3 << 20)], |_| {});
        assert_eq!(format(&p), None);
    }
}

//! From equalized symbols to MPEG-2 transport stream packets: the 12 trellis decoders, the
//! bytes back in field order, the convolutional deinterleaver (52 branches, 4 bytes more delay
//! each), Reed-Solomon (207,187) and the randomizer. A field carries 312 packets; its data
//! bytes, the interleaver's commutator and the randomizer all start afresh at its field sync.

pub mod rs;
pub mod trellis;

use std::sync::OnceLock;

use super::on_both_cores;
use super::vsb::{DATA_SEGMENTS, DATA_SYMBOLS, FIELD_SEGMENTS, SEGMENT, SYNC_SYMBOLS};
use trellis::ENCODERS;

/// Reed-Solomon-coded bytes a field.
pub const FIELD_BYTES: usize = rs::N * DATA_SEGMENTS;
const BRANCHES: usize = 52;
/// Bytes of delay the interleaver adds a branch.
const BRANCH_STEP: usize = 4 * BRANCHES;

/// Field byte b goes to encoder `.0[b]` as its `.1[b]`-th byte of the field. The encoders take
/// the bytes in turn, the turn moving on by 4 at each segment's start rounded up to 12 bytes.
fn byte_map() -> &'static (Vec<u8>, Vec<u16>) {
    static MAP: OnceLock<(Vec<u8>, Vec<u16>)> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut enc = vec![0u8; FIELD_BYTES];
        let mut nth = vec![0u16; FIELD_BYTES];
        let mut count = [0u16; ENCODERS];
        let (mut shift, mut seg) = (0usize, 1usize);
        for b in 0..FIELD_BYTES {
            while seg <= DATA_SEGMENTS && (rs::N * seg).div_ceil(ENCODERS) * ENCODERS == b {
                shift = (shift + 4) % ENCODERS;
                seg += 1;
            }
            let e = (b + shift) % ENCODERS;
            enc[b] = e as u8;
            nth[b] = count[e];
            count[e] += 1;
        }
        (enc, nth)
    })
}

/// The randomizer's bytes for a field's 312 packets (sync bytes left out): x^16 + x^13 + x^12 +
/// x^11 + x^7 + x^6 + x^3 + x + 1 from 0xF180.
fn randomizer() -> &'static [u8] {
    static R: OnceLock<Vec<u8>> = OnceLock::new();
    R.get_or_init(|| {
        let mut st: u32 = 0xF180;
        (0..rs::K * DATA_SEGMENTS)
            .map(|_| {
                let o = ((st & 0x3C00) >> 6) | ((st & 0x0040) >> 3) | ((st & 0x000C) >> 1) | (st & 1);
                st <<= 1;
                if st & 0x1_0000 != 0 {
                    st ^= (0x9C65 << 1) | 1;
                }
                o as u8
            })
            .collect()
    })
}

/// One transport stream packet, and whether Reed-Solomon passed it.
#[derive(Debug, Clone)]
pub struct Packet {
    pub bytes: [u8; 188],
    pub outcome: rs::Outcome,
}

/// Decoding counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FecStats {
    pub fields: usize,
    pub packets: usize,
    pub corrected: usize,
    pub failed: usize,
}

/// The packets of the whole fields from the field sync segment `first_sync` on, in order, from
/// `symbols` (segment after segment). The last 51 or so packets are lost to the deinterleaver's
/// delay.
pub fn decode(symbols: &[f32], first_sync: usize) -> (Vec<Packet>, FecStats) {
    let fields = (symbols.len() / SEGMENT).saturating_sub(first_sync) / FIELD_SEGMENTS;
    let mut stats = FecStats { fields, ..Default::default() };
    if fields == 0 {
        return (Vec::new(), stats);
    }
    // The soft symbols in one pass, field by field and within a field encoder by encoder: in a
    // data segment the encoders take the symbols in turn, the first being encoder(segment, 0).
    let per_field = DATA_SEGMENTS * DATA_SYMBOLS / ENCODERS;
    let per_segment = DATA_SYMBOLS / ENCODERS;
    let mut soft = vec![0i32; fields * ENCODERS * per_field];
    on_both_cores(&mut soft, ENCODERS * per_field, |first, half| {
        for (i, field) in half.chunks_exact_mut(ENCODERS * per_field).enumerate() {
            let f = first / (ENCODERS * per_field) + i;
            for dseg in 0..DATA_SEGMENTS {
                let seg = first_sync + f * FIELD_SEGMENTS + 1 + dseg;
                let row = &symbols[seg * SEGMENT + SYNC_SYMBOLS..(seg + 1) * SEGMENT];
                let e0 = trellis::encoder(dseg, 0);
                for (q, turn) in row.chunks_exact(ENCODERS).enumerate() {
                    for (j, &s) in turn.iter().enumerate() {
                        let e = if e0 + j < ENCODERS { e0 + j } else { e0 + j - ENCODERS };
                        field[e * per_field + dseg * per_segment + q] = trellis::soft(s);
                    }
                }
            }
        }
    });
    // Each encoder's bit pairs: six encoders on each core.
    let mut pairs = vec![Vec::new(); ENCODERS];
    on_both_cores(&mut pairs, 1, |first, out| {
        for (i, o) in out.iter_mut().enumerate() {
            let e = first + i;
            let seq = (0..fields).flat_map(|f| soft[(f * ENCODERS + e) * per_field..][..per_field].iter().copied());
            *o = trellis::decode(fields * per_field, seq);
        }
    });
    // Each encoder's bytes, then the interleaved stream in field order.
    let (enc, nth) = byte_map();
    let bytes_per_field = FIELD_BYTES / ENCODERS;
    let mut stream = vec![0u8; fields * FIELD_BYTES];
    on_both_cores(&mut stream, FIELD_BYTES, |first, half| {
        for (i, field) in half.chunks_exact_mut(FIELD_BYTES).enumerate() {
            let f = first / FIELD_BYTES + i;
            for (b, out) in field.iter_mut().enumerate() {
                let k = (f * bytes_per_field + nth[b] as usize) * 4;
                let p = &pairs[enc[b] as usize][k..k + 4];
                *out = (p[0] << 6) | (p[1] << 4) | (p[2] << 2) | p[3];
            }
        }
    });
    // Byte m before the interleaver is byte m + 208 (m mod 52) after it: the codewords whose
    // bytes have all arrived, corrected half on each core.
    let at = |m: usize| m + BRANCH_STEP * (m % BRANCHES);
    let total = (0..stream.len() / rs::N).take_while(|s| (0..rs::N).all(|j| at(s * rs::N + j) < stream.len())).count();
    let rand = randomizer();
    let mut packets = vec![Packet { bytes: [0; 188], outcome: rs::Outcome::Clean }; total];
    on_both_cores(&mut packets, 1, |first, half| {
        let mut cw = [0u8; rs::N];
        for (i, packet) in half.iter_mut().enumerate() {
            let s = first + i;
            for (j, c) in cw.iter_mut().enumerate() {
                *c = stream[at(s * rs::N + j)];
            }
            packet.outcome = rs::decode(&mut cw);
            let fseg = s % DATA_SEGMENTS;
            packet.bytes[0] = 0x47;
            for (j, b) in packet.bytes[1..].iter_mut().enumerate() {
                *b = cw[j] ^ rand[fseg * rs::K + j];
            }
        }
    });
    for p in &packets {
        match p.outcome {
            rs::Outcome::Corrected(_) => stats.corrected += 1,
            rs::Outcome::Failed => stats.failed += 1,
            rs::Outcome::Clean => {}
        }
    }
    stats.packets = packets.len();
    (packets, stats)
}

/// Test signals: packets through the randomizer, Reed-Solomon, the interleaver and the trellis
/// encoders, into data segments (each `[0; 4]` sync, then 828 symbols, pilot not added), a field
/// sync segment (zeros) before each 312.
#[cfg(test)]
pub fn encode(packets: &[[u8; 188]]) -> Vec<[f32; SEGMENT]> {
    use super::vsb::level;
    let fields = packets.len() / DATA_SEGMENTS;
    let rand = randomizer();
    let mut orig = Vec::with_capacity(fields * FIELD_BYTES);
    for (i, p) in packets.iter().take(fields * DATA_SEGMENTS).enumerate() {
        let fseg = i % DATA_SEGMENTS;
        let data: Vec<u8> = p[1..].iter().enumerate().map(|(j, &b)| b ^ rand[fseg * rs::K + j]).collect();
        orig.extend_from_slice(&data);
        orig.extend_from_slice(&rs::parity(&data));
    }
    // The interleaver as if the same fields had run before (a running transmitter's delay lines
    // are full of data, not zeros).
    let inter: Vec<u8> = (0..orig.len())
        .map(|t| orig[(t + orig.len() - BRANCH_STEP * (t % BRANCHES) % orig.len()) % orig.len()])
        .collect();
    let (enc, nth) = byte_map();
    let mut encoders = [trellis::Encoder::default(); ENCODERS];
    let mut segs = Vec::new();
    for f in 0..fields {
        // Each encoder's bytes of this field, in order.
        let mut by_enc = vec![vec![0u8; FIELD_BYTES / ENCODERS]; ENCODERS];
        for b in 0..FIELD_BYTES {
            by_enc[enc[b] as usize][nth[b] as usize] = inter[f * FIELD_BYTES + b];
        }
        let mut sent = [0usize; ENCODERS];
        segs.push([0f32; SEGMENT]);
        for dseg in 0..DATA_SEGMENTS {
            let mut seg = [0f32; SEGMENT];
            for k in 0..DATA_SYMBOLS {
                let e = trellis::encoder(dseg, k);
                let n = sent[e];
                sent[e] += 1;
                let byte = by_enc[e][n / 4];
                let pair = (byte >> (6 - 2 * (n % 4))) & 3;
                seg[SYNC_SYMBOLS + k] = level(encoders[e].symbol(pair >> 1, pair & 1));
            }
            segs.push(seg);
        }
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(n: usize) -> Vec<[u8; 188]> {
        (0..n)
            .map(|i| {
                let mut p = [0u8; 188];
                p[0] = 0x47;
                for (j, b) in p[1..].iter_mut().enumerate() {
                    *b = (i * 7 + j * 13) as u8;
                }
                p
            })
            .collect()
    }

    #[test]
    fn every_encoder_takes_the_same_share_of_a_field() {
        let (enc, nth) = byte_map();
        for e in 0..ENCODERS {
            assert_eq!(enc.iter().filter(|&&x| x as usize == e).count(), FIELD_BYTES / ENCODERS);
        }
        assert_eq!(*nth.iter().max().unwrap() as usize, FIELD_BYTES / ENCODERS - 1);
        // The first bytes go to the encoders in turn.
        assert_eq!(&enc[..13], &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 0]);
    }

    #[test]
    fn three_fields_come_back_through_the_whole_chain() {
        let sent = packets(3 * DATA_SEGMENTS);
        let segs = encode(&sent);
        let (got, stats) = decode(&segs.concat(), 0);
        assert_eq!(stats.fields, 3);
        assert!(got.len() >= 3 * DATA_SEGMENTS - 52, "{}", got.len());
        assert_eq!((stats.failed, stats.corrected), (0, 0));
        for (g, s) in got.iter().zip(&sent) {
            assert_eq!(g.bytes, *s);
        }
    }

    #[test]
    fn noise_is_corrected_by_the_trellis_and_reed_solomon() {
        let sent = packets(2 * DATA_SEGMENTS);
        let mut segs = encode(&sent);
        let mut x = 5u64;
        for seg in segs.iter_mut() {
            for s in seg[SYNC_SYMBOLS..].iter_mut() {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                // Uniform noise of ±0.95: a slicer's errors are rare but not absent.
                *s += ((x >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 1.9;
            }
        }
        let (got, stats) = decode(&segs.concat(), 0);
        assert_eq!(stats.failed, 0, "{stats:?}");
        for (g, s) in got.iter().zip(&sent) {
            assert_eq!(g.bytes, *s);
        }
    }
}

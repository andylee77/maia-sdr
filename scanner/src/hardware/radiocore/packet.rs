//! The lane ring's packets (maia-sdr doc/changes/079, "The lane packet"): 4 KB each, an 8-word
//! header and up to 1008 IQ samples. Every 64-bit word is little-endian, so the IQ is
//! interleaved `i16` I, Q in byte order.

/// Bytes per packet.
pub const PACKET_BYTES: usize = 4096;
/// Samples a full packet holds.
pub const MAX_SAMPLES: usize = 1008;

const HEADER_BYTES: usize = 64;
const MAGIC: u16 = 0x5243;
const FORMAT: u8 = 1;

/// Why a packet was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// Not a packet (never written, or another format).
    Magic(u16),
    Format(u8),
    Count(u16),
    /// The XOR of the packet's 32-bit words is not zero: part of it is stale.
    Check(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// Samples were dropped before this packet.
    pub lost: bool,
    /// The first packet after the lane's enable, or with a tag other than the packet before.
    pub retuned: bool,
    /// The lane's disable closed this packet.
    pub last: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub lane: u8,
    pub flags: Flags,
    /// Valid samples.
    pub count: u16,
    pub tag: u16,
    /// The AD9361 sample count (since the core's reset) when the DDC made the first sample.
    pub sample_index: u64,
    /// Σ I² + Q² over the samples.
    pub power: u64,
    /// The largest |I| or |Q|.
    pub peak: u16,
    /// The lane's 28-bit NCO word at the first sample.
    pub nco: u32,
    /// The lane's packet count (wraps).
    pub sequence: u16,
    /// The running count of AD9361 samples at full scale.
    pub adc_clips: u32,
}

/// A checked packet: its header and its samples' bytes.
#[derive(Debug, Clone, Copy)]
pub struct Packet<'a> {
    pub header: Header,
    iq: &'a [u8],
}

impl Packet<'_> {
    /// The samples as interleaved I, Q.
    pub fn iq(&self) -> impl Iterator<Item = i16> + '_ {
        self.iq.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]]))
    }
}

fn word(bytes: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(bytes[8 * i..8 * i + 8].try_into().unwrap())
}

/// Check and read one packet.
pub fn parse(bytes: &[u8]) -> Result<Packet<'_>, Fault> {
    assert_eq!(bytes.len(), PACKET_BYTES);
    let w0 = word(bytes, 0);
    let magic = w0 as u16;
    if magic != MAGIC {
        return Err(Fault::Magic(magic));
    }
    let format = (w0 >> 16) as u8 & 0xF;
    if format != FORMAT {
        return Err(Fault::Format(format));
    }
    let count = (w0 >> 32) as u16;
    if count == 0 || count as usize > MAX_SAMPLES {
        return Err(Fault::Count(count));
    }
    let check = bytes.chunks_exact(4).fold(0u32, |x, b| x ^ u32::from_le_bytes(b.try_into().unwrap()));
    if check != 0 {
        return Err(Fault::Check(check));
    }
    let flags = (w0 >> 24) as u8;
    let (w1, w2, w3, w4) = (word(bytes, 1), word(bytes, 2), word(bytes, 3), word(bytes, 4));
    let header = Header {
        lane: (w0 >> 20) as u8 & 0xF,
        flags: Flags { lost: flags & 1 != 0, retuned: flags & 2 != 0, last: flags & 4 != 0 },
        count,
        tag: (w0 >> 48) as u16,
        sample_index: w1,
        power: w2 & ((1 << 48) - 1),
        peak: (w2 >> 48) as u16,
        nco: w3 as u32 & 0x0FFF_FFFF,
        sequence: (w3 >> 32) as u16,
        adc_clips: w4 as u32,
    };
    Ok(Packet { header, iq: &bytes[HEADER_BYTES..HEADER_BYTES + 4 * count as usize] })
}

/// A packet as the gateware writes it (tests and simulations).
#[cfg(test)]
pub fn build(h: &Header, iq: &[(i16, i16)]) -> Vec<u8> {
    assert_eq!(h.count as usize, iq.len());
    let flags = h.flags.lost as u64 | (h.flags.retuned as u64) << 1 | (h.flags.last as u64) << 2;
    let mut words = vec![0u64; PACKET_BYTES / 8];
    words[0] = MAGIC as u64
        | (FORMAT as u64) << 16
        | (h.lane as u64) << 20
        | flags << 24
        | (h.count as u64) << 32
        | (h.tag as u64) << 48;
    words[1] = h.sample_index;
    words[2] = h.power | (h.peak as u64) << 48;
    words[3] = h.nco as u64 | (h.sequence as u64) << 32;
    words[4] = h.adc_clips as u64;
    for (k, &(i, q)) in iq.iter().enumerate() {
        let sample = (i as u16 as u64) | (q as u16 as u64) << 16;
        words[8 + k / 2] |= sample << (32 * (k % 2));
    }
    let check = words.iter().fold(0u32, |x, w| x ^ *w as u32 ^ (*w >> 32) as u32);
    words[7] = check as u64;
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(count: u16) -> Header {
        Header {
            lane: 2,
            flags: Flags { lost: false, retuned: true, last: false },
            count,
            tag: 0xBEEF,
            sample_index: 0x1_2345_6789,
            power: 0xABCD_1234_5678,
            peak: 32767,
            nco: 0x0ABC_DEF0,
            sequence: 513,
            adc_clips: 77,
        }
    }

    fn samples(n: usize) -> Vec<(i16, i16)> {
        (0..n).map(|k| ((k as i16).wrapping_mul(37), (k as i16).wrapping_mul(-91))).collect()
    }

    #[test]
    fn a_packet_reads_back() {
        for n in [1, 2, 301, MAX_SAMPLES] {
            let iq = samples(n);
            let bytes = build(&header(n as u16), &iq);
            let p = parse(&bytes).unwrap();
            assert_eq!(p.header, header(n as u16));
            let flat: Vec<i16> = iq.iter().flat_map(|&(i, q)| [i, q]).collect();
            assert_eq!(p.iq().collect::<Vec<_>>(), flat, "{n} samples");
        }
    }

    #[test]
    fn a_stale_word_fails_the_check() {
        let mut bytes = build(&header(500), &samples(500));
        bytes[2000] ^= 0x10;
        assert!(matches!(parse(&bytes), Err(Fault::Check(_))));
        // A cache line left from a previous lap, beyond the samples, is caught too.
        let mut bytes = build(&header(500), &samples(500));
        bytes[4000] = 1;
        assert!(matches!(parse(&bytes), Err(Fault::Check(_))));
    }

    #[test]
    fn unwritten_memory_is_not_a_packet() {
        assert_eq!(parse(&[0u8; PACKET_BYTES]).unwrap_err(), Fault::Magic(0));
    }

    /// The words the gateware's reference model writes (maia-hdl `lane_packetizer.header_words`,
    /// which the HDL simulations hold the packetiser to) for three samples on lane 1.
    #[test]
    fn the_layout_is_the_gateware_s() {
        const WORDS: [u64; 10] = [
            0xBEEF000302115243,
            0x0000000123456789,
            0x800000007FFFC38E,
            0x000002010ABCDEF0,
            0x000000000000004D,
            0,
            0,
            0x0000000015C5AA9B,
            0x7FFF8000FF380064,
            0x00000000FFFA0005,
        ];
        let mut bytes = vec![0u8; PACKET_BYTES];
        for (i, w) in WORDS.iter().enumerate() {
            bytes[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        let p = parse(&bytes).unwrap();
        let iq = [(100, -200), (-32768, 32767), (5, -6)];
        assert_eq!(
            p.header,
            Header {
                lane: 1,
                flags: Flags { lost: false, retuned: true, last: false },
                count: 3,
                tag: 0xBEEF,
                sample_index: 0x1_2345_6789,
                power: iq.iter().map(|&(i, q): &(i64, i64)| (i * i + q * q) as u64).sum(),
                peak: 32768,
                nco: 0x0ABC_DEF0,
                sequence: 513,
                adc_clips: 77,
            }
        );
        assert_eq!(p.iq().collect::<Vec<_>>(), vec![100, -200, -32768, 32767, 5, -6]);
        assert_eq!(build(&p.header, &[(100, -200), (-32768, 32767), (5, -6)]), bytes);
    }
}

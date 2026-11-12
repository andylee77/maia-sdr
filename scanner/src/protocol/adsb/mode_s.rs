//! The Mode S messages in the window's samples, at four samples a microsecond.
//!
//! Each sample is taken as its amplitude, in sixteenths of the ADC's step. A message begins
//! where the four pulses of a preamble (half a microsecond each, starting 0, 1.0, 3.5 and
//! 4.5 us in) stand over what is between them in the preamble's 8 us, at the sample where they
//! hold the most. Its bits follow, a microsecond each: a one when the first half holds more
//! than the second. The first five bits are the format: 17 and 18 are extended squitters (112
//! bits), 11 an all-call reply (56 bits). A message is taken when the parity over the whole of
//! it leaves nothing.
//!
//! No bit is corrected, so a message taken is one that was sent, but for chance: one try in
//! 16.7 million passes the parity, and noise is tried many thousand times a second, which is
//! why an aircraft counts once two messages gave its address (`services::adsb`).

/// Samples to a bit and to a preamble, the bits of a long and of a short message, and the
/// samples from a preamble's first to a long message's last.
const BIT: usize = 4;
const PREAMBLE: usize = 32;
const LONG: usize = 112;
const SHORT: usize = 56;
const SPAN: usize = PREAMBLE + LONG * BIT;
/// A preamble: the eight samples of its pulses hold more than this share of all its 32, which
/// is their mean one and a half times the mean of the other 24.
const PULSES: u32 = 3;
const OF_ALL: u32 = 1;
/// The same message found again within this many samples is the one message.
const SAME: u64 = 200;
/// The ADC's full scale: its samples are 12 bits, sign extended.
const FULL: i32 = 2048;
/// The parity's generator, less its leading bit.
const GENERATOR: u32 = 0xFF_F409;

const PARITY: [u32; 256] = parity_table();

const fn parity_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut byte = 0;
    while byte < 256 {
        let mut c = (byte as u32) << 16;
        let mut k = 0;
        while k < 8 {
            let next = c << 1;
            c = (if c & 0x80_0000 != 0 { next ^ GENERATOR } else { next }) & 0xFF_FFFF;
            k += 1;
        }
        table[byte] = c;
        byte += 1;
    }
    table
}

/// What the parity's generator leaves of `bytes`: nothing, of a whole message received as sent.
pub fn parity(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0, |c, &b| ((c << 8) & 0xFF_FFFF) ^ PARITY[usize::from((c >> 16) as u8 ^ b)])
}

/// A sample's amplitude in sixteenths of the ADC's step.
fn amplitude(i: i16, q: i16) -> u32 {
    let (i, q) = (i32::from(i).clamp(-FULL, FULL), i32::from(q).clamp(-FULL, FULL));
    (((i * i + q * q) as f32).sqrt() * 16.0 + 0.5) as u32
}

/// A message taken from the air.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The sample its preamble begins at, counted from the first given.
    pub at: u64,
    /// Its bytes: 14 of a long message, 7 of a short one and the rest zero.
    pub bytes: [u8; 14],
    pub long: bool,
    /// Its preamble's pulses against the ADC's full scale, dB.
    pub level_dbfs: f32,
}

/// The search, fed the window as it comes.
#[derive(Default)]
pub struct Demod {
    /// The amplitudes still to be searched: the end of what was given before, which a message
    /// may have begun in, and what was given since.
    level: Vec<u32>,
    /// The sample `level` begins at.
    base: u64,
    /// At each sample, what the eight samples of a preamble's pulses hold were it to begin
    /// there.
    pulses: Vec<u32>,
    /// The last short and the last long message taken, and where.
    last: [Option<(u64, [u8; 14])>; 2],
    tried: u64,
}

impl Demod {
    /// Preambles found and read since the start.
    pub fn tried(&self) -> u64 {
        self.tried
    }

    /// Samples were lost: what was held of before them is no part of what comes.
    pub fn afresh(&mut self) {
        self.base += self.level.len() as u64;
        self.level.clear();
    }

    /// More of the window: whole samples of 16-bit I and Q, little-endian. The messages found
    /// are added to `out`, in the order they were sent.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Frame>) {
        let Demod { level, base, pulses, last, tried } = self;
        level.extend(bytes.chunks_exact(4).map(|s| amplitude(i16::from_le_bytes([s[0], s[1]]), i16::from_le_bytes([s[2], s[3]]))));
        if level.len() <= SPAN + 1 {
            return;
        }
        // Samples 1 to `end` (not `end` itself) have the one before them and a whole long
        // message after them here.
        let end = level.len() - SPAN;
        let p = level.as_slice();
        let count = end + 1;
        let (a, b, c, d) = (&p[..count + 1], &p[4..count + 5], &p[14..count + 15], &p[18..count + 19]);
        pulses.clear();
        pulses.extend((0..count).map(|n| a[n] + a[n + 1] + b[n] + b[n + 1] + c[n] + c[n + 1] + d[n] + d[n + 1]));
        let mut all: u32 = p[1..=PREAMBLE].iter().sum();
        for n in 1..end {
            let here = pulses[n];
            if PULSES * here > OF_ALL * all && here >= pulses[n - 1] && here > pulses[n + 1] {
                *tried += 1;
                if let Some((message, long)) = read(p, n) {
                    let at = *base + n as u64;
                    let slot = &mut last[usize::from(long)];
                    if !slot.is_some_and(|(was, same)| same == message && at - was <= SAME) {
                        *slot = Some((at, message));
                        // The mean of the pulses' eight samples, in sixteenths.
                        let level_dbfs = 20.0 * (here as f32 / (8 * 16 * FULL) as f32).log10();
                        out.push(Frame { at, bytes: message, long, level_dbfs });
                    }
                }
            }
            all = all + p[n + PREAMBLE] - p[n];
        }
        level.drain(..end - 1);
        *base += (end - 1) as u64;
    }
}

/// The message whose preamble begins at `n`, if its format is one read here and its parity is
/// right.
fn read(p: &[u32], n: usize) -> Option<([u8; 14], bool)> {
    let bit = |k: usize| {
        let s = &p[n + PREAMBLE + k * BIT..][..BIT];
        s[0] + s[1] > s[2] + s[3]
    };
    let format = (0..5).fold(0u8, |f, k| f << 1 | u8::from(bit(k)));
    let bits = match format {
        17 | 18 => LONG,
        11 => SHORT,
        _ => return None,
    };
    let mut bytes = [0u8; 14];
    for k in (0..bits).filter(|&k| bit(k)) {
        bytes[k / 8] |= 0x80 >> (k % 8);
    }
    (parity(&bytes[..bits / 8]) == 0).then_some((bytes, bits == LONG))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read;

    use super::*;
    use crate::protocol::adsb::squitter::tests::frame;

    /// The window's bytes with `messages` in it, each at its sample and of its amplitude, over
    /// a little noise.
    pub(crate) fn window(samples: usize, messages: &[(usize, &str, i16)]) -> Vec<u8> {
        let mut seed = 0x2545_F491u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed % 9) as i16 - 4
        };
        let mut level = vec![0i16; samples];
        for &(at, hex, amplitude) in messages {
            let f = frame(hex);
            let bits = if f.long { LONG } else { SHORT };
            // What falls past the window's end is not sent.
            let mut pulse = |from: usize| {
                if let Some(two) = level.get_mut(from..from + 2) {
                    two.fill(amplitude);
                }
            };
            [0, 4, 14, 18].into_iter().for_each(|k| pulse(at + k));
            for k in 0..bits {
                let one = f.bytes[k / 8] & (0x80 >> (k % 8)) != 0;
                pulse(at + PREAMBLE + k * BIT + if one { 0 } else { 2 });
            }
        }
        level.iter().flat_map(|&a| [(a + noise()).to_le_bytes(), noise().to_le_bytes()]).flatten().collect()
    }

    const CALLSIGN: &str = "8D4840D6202CC371C32CE0576098";
    const EVEN: &str = "8D40621D58C382D690C8AC2863A7";
    const ALL_CALL: &str = "5DA1BAC2892941";

    #[test]
    fn the_parity_leaves_nothing_of_a_message_as_sent() {
        for hex in [CALLSIGN, EVEN, ALL_CALL] {
            let f = frame(hex);
            let whole = &f.bytes[..hex.len() / 2];
            assert_eq!(parity(whole), 0, "{hex}");
            let mut wrong = whole.to_vec();
            wrong[5] ^= 0x04;
            assert_ne!(parity(&wrong), 0);
        }
    }

    #[test]
    fn messages_are_found_where_they_are_whatever_the_blocks() {
        let sent = [(1_000, CALLSIGN, 400), (3_000, ALL_CALL, 60), (5_003, EVEN, 1_500), (9_000, EVEN, 25)];
        let bytes = window(12_000, &sent);
        for block in [bytes.len(), 4096, 1028, 400] {
            let (mut demod, mut found) = (Demod::default(), Vec::new());
            for part in bytes.chunks(block) {
                demod.push(part, &mut found);
            }
            let got: Vec<(u64, [u8; 14], bool)> = found.iter().map(|f| (f.at, f.bytes, f.long)).collect();
            let want: Vec<(u64, [u8; 14], bool)> = sent.iter().map(|&(at, hex, _)| (at as u64, frame(hex).bytes, hex.len() == 28)).collect();
            assert_eq!(got, want, "in blocks of {block} bytes");
            // Pulses of 400 and of 25 against a full scale of 2048.
            assert!((found[0].level_dbfs + 14.2).abs() < 0.5 && (found[3].level_dbfs + 38.3).abs() < 1.5, "{:?}", found.iter().map(|f| f.level_dbfs).collect::<Vec<_>>());
            assert!(demod.tried() >= 4);
        }
    }

    #[test]
    fn a_message_with_a_bit_wrong_is_not_taken_and_noise_is_nothing() {
        let mut bytes = window(4_000, &[(1_000, CALLSIGN, 400)]);
        // The twentieth bit's two halves changed about: a one sent as a zero.
        let bit = (1_000 + PREAMBLE + 20 * BIT) * 4;
        let (first, second) = (bytes[bit..bit + 8].to_vec(), bytes[bit + 8..bit + 16].to_vec());
        bytes[bit..bit + 8].copy_from_slice(&second);
        bytes[bit + 8..bit + 16].copy_from_slice(&first);
        let (mut demod, mut found) = (Demod::default(), Vec::new());
        demod.push(&bytes, &mut found);
        demod.push(&window(40_000, &[]), &mut found);
        assert_eq!(found, []);
    }

    #[test]
    fn after_lost_samples_the_count_goes_on_and_nothing_is_joined() {
        let (mut demod, mut found) = (Demod::default(), Vec::new());
        let bytes = window(4_000, &[(3_800, CALLSIGN, 400), (1_000, EVEN, 400)]);
        demod.push(&bytes, &mut found);
        // The message cut off at the end is not finished by what comes after the loss.
        demod.afresh();
        demod.push(&window(4_000, &[(500, CALLSIGN, 400)]), &mut found);
        assert_eq!(found.iter().map(|f| f.at).collect::<Vec<_>>(), [1_000, 4_500]);
    }

    /// The captures of `ADSB_CAPTURE_DIR` that have the look tool's list of their messages
    /// beside them (`tools/adsb_look.py FILE --messages`): this finds the same ones, at the
    /// same samples, whatever the blocks it is given.
    #[test]
    fn a_capture_gives_the_messages_the_look_tool_lists() {
        let Ok(dir) = std::env::var("ADSB_CAPTURE_DIR") else { return };
        let mut held = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let listed = path.with_extension("messages");
            if path.extension().is_none_or(|e| e != "cs16") || !listed.exists() {
                continue;
            }
            let want: Vec<(u64, String)> = std::fs::read_to_string(&listed).unwrap().lines().map(|l| {
                let (at, hex) = l.split_once(' ').unwrap();
                (at.parse().unwrap(), hex.to_string())
            }).collect();
            let (mut demod, mut found) = (Demod::default(), Vec::new());
            let mut file = std::fs::File::open(&path).unwrap();
            let mut block = vec![0u8; 1 << 20];
            loop {
                let mut filled = 0;
                while filled < block.len() {
                    match file.read(&mut block[filled..]).unwrap() {
                        0 => break,
                        n => filled += n,
                    }
                }
                demod.push(&block[..filled], &mut found);
                if filled < block.len() {
                    break;
                }
            }
            let got: Vec<(u64, String)> = found.iter().map(|f| (f.at, f.bytes[..if f.long { 14 } else { 7 }].iter().map(|b| format!("{b:02x}")).collect())).collect();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            println!("{name}: {} messages ({} long), {} preambles tried", got.len(), found.iter().filter(|f| f.long).count(), demod.tried());
            assert_eq!(got.len(), want.len(), "{name}");
            assert!(got == want, "{name}: the first that differs: {:?}", got.iter().zip(&want).find(|(g, w)| g != w));
            held += 1;
        }
        assert!(held > 0, "no capture with its messages listed in {dir}");
    }
}

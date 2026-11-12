//! UAT, the second link aircraft send of themselves on (978 MHz): the messages in the window's
//! samples, at four samples a microsecond with the LO a megahertz above the link, so that the
//! radio's own DC is clear of it.
//!
//! UAT is keyed by frequency: 1.041667 Mbit/s, a one 312.5 kHz above the carrier and a zero as
//! far below, 3.84 samples a bit here. Each sample is turned a quarter of a turn more than the
//! one before (which moves the link to the middle), four are summed (which is nothing at the
//! radio's DC, now a megahertz off), and the way the phase turns from one sum to the next is
//! the frequency: its sign, over three samples together, is all that is kept. A message begins
//! where the 36 bits of an aircraft's synchronisation fit, read at the middle of each bit, with
//! at most four wrong; of neighbouring samples that fit, the middle one of those that fit
//! best. Its bits follow: 240 for a short message, 384 for a long one, the first five saying
//! which. It is taken when its Reed-Solomon code (`code`) can put it right; `message` reads
//! what it says.
//!
//! Written from the link's facts, not from another decoder's code. `tools/uat_look.py` reads a
//! capture the same way on the PC, sample for sample, and is what this is held against. A
//! ground station's messages (the weather and traffic it sends up, and the other traffic it
//! relays) are not read: none was on the air to hold a reader against.

pub mod code;
pub mod message;

use message::{LONG, SHORT};

/// The link's frequency, and where the reader wants the LO: a megahertz above it.
pub const FREQ_HZ: u64 = 978_000_000;
pub const LO_HZ: u64 = FREQ_HZ + 1_000_000;

/// An aircraft's synchronisation, the first bit sent the highest.
const SYNC: u64 = 0xE_ACDD_A4E2;
const SYNC_BITS: usize = 36;
/// A message's bits after it, and a long message's block with its parity.
const LONG_BITS: usize = 384;
const BITS: usize = SYNC_BITS + LONG_BITS;
const BLOCK: usize = LONG_BITS / 8;
/// Samples summed before the frequency is taken.
const BOX: usize = 4;
/// Neighbouring samples that fit are one message's: this many from the first.
const GROUP: usize = 4;
/// Samples a level is taken over, from a message's first.
const LEVEL: usize = 64;
/// The ADC's full scale: its samples are 12 bits, sign extended.
const FULL: i32 = 2048;

/// Where each bit is read, in samples from the first of the synchronisation: the middle of the
/// bit, at 3.84 samples a bit.
const OFFSETS: [usize; BITS] = offsets();
/// The last of them, and the samples beyond a place that must be here before it is tried.
const LAST: usize = OFFSETS[BITS - 1];
const AHEAD: usize = LAST + 16;

const fn offsets() -> [usize; BITS] {
    let mut at = [0; BITS];
    let mut k = 0;
    while k < BITS {
        at[k] = ((2 * k + 1) * 192 + 50) / 100;
        k += 1;
    }
    at
}

/// A message taken from the air.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The sample its synchronisation begins at, counted from the first given.
    pub at: u64,
    /// Its bytes, put right: 18 of a short message, 34 of a long one and the rest zero.
    pub payload: [u8; LONG],
    pub long: bool,
    /// The bytes its code put right.
    pub mended: u8,
    /// Its level against the ADC's full scale, dB.
    pub level_dbfs: f32,
}

impl Frame {
    pub fn bytes(&self) -> &[u8] {
        &self.payload[..if self.long { LONG } else { SHORT }]
    }
}

/// The reader, fed the window as it comes.
#[derive(Default)]
pub struct Demod {
    /// The sample the present run began at: after lost samples a run begins afresh.
    origin: u64,
    /// Samples given in this run.
    count: usize,
    /// The last four samples, turned, and their sum.
    ring: [(i32, i32); BOX],
    sum: (i32, i32),
    /// The turn of phase at the last two samples.
    turns: [i32; 2],
    /// The frequency's sign at each sample still to be searched, 32 to a word, the earliest
    /// the lowest bit; and each sample's power.
    signs: Vec<u32>,
    power: Vec<u32>,
    /// The sample `signs` and `power` begin at, and the signs held.
    base: usize,
    held: usize,
    /// The next sample to try, and the one a message taken or a group tried ends before.
    next: usize,
    past: usize,
    fitted: u64,
}

impl Demod {
    /// Places the synchronisation fitted since the start.
    pub fn fitted(&self) -> u64 {
        self.fitted
    }

    /// Samples were lost: what was held of before them is no part of what comes.
    pub fn afresh(&mut self) {
        *self = Demod { origin: self.origin + self.count as u64, fitted: self.fitted, ..Default::default() };
    }

    /// More of the window: whole samples of 16-bit I and Q, little-endian. The messages found
    /// are added to `out`, in the order they were sent.
    pub fn push(&mut self, bytes: &[u8], out: &mut Vec<Frame>) {
        self.power.reserve(bytes.len() / 4);
        for s in bytes.chunks_exact(4) {
            let i = i32::from(i16::from_le_bytes([s[0], s[1]])).clamp(-FULL, FULL);
            let q = i32::from(i16::from_le_bytes([s[2], s[3]])).clamp(-FULL, FULL);
            self.power.push((i * i + q * q) as u32);
            let turned = match self.count & 3 {
                0 => (i, q),
                1 => (-q, i),
                2 => (-i, -q),
                _ => (q, -i),
            };
            let gone = std::mem::replace(&mut self.ring[self.count % BOX], turned);
            let sum = (self.sum.0 + turned.0 - gone.0, self.sum.1 + turned.1 - gone.1);
            // Which way the phase turned from the last sum to this one.
            let turn = if self.count == 0 { 0 } else { sum.1 * self.sum.0 - sum.0 * self.sum.1 };
            self.sum = sum;
            if self.count > 0 {
                // The sample before, with its neighbours either side.
                let one = self.turns[0] + self.turns[1] + turn > 0;
                if self.held % 32 == 0 {
                    self.signs.push(0);
                }
                if one {
                    *self.signs.last_mut().expect("a word was pushed") |= 1 << (self.held % 32);
                }
                self.held += 1;
            }
            self.turns = [self.turns[1], turn];
            self.count += 1;
        }
        self.search(out);
    }

    fn bit(&self, at: usize) -> bool {
        let at = at - self.base;
        self.signs[at / 32] >> (at % 32) & 1 == 1
    }

    /// The signs of the 32 samples from `at`, the first the lowest bit.
    fn window(&self, at: usize) -> u32 {
        let (word, shift) = ((at - self.base) / 32, (at - self.base) % 32);
        let low = self.signs[word] >> shift;
        if shift == 0 {
            low
        } else {
            low | self.signs.get(word + 1).copied().unwrap_or(0) << (32 - shift)
        }
    }

    /// The bits of the synchronisation that are wrong, were a message to begin at `at`.
    fn wrong(&self, at: usize) -> u32 {
        (0..SYNC_BITS).filter(|&k| self.bit(at + OFFSETS[k]) != (SYNC >> (SYNC_BITS - 1 - k) & 1 == 1)).count() as u32
    }

    /// Try every place that has all of a long message, and of its group, after it.
    fn search(&mut self, out: &mut Vec<Frame>) {
        let Some(end) = self.count.checked_sub(AHEAD) else { return };
        while self.next <= end {
            let first = self.next;
            let width = (end + 1 - first).min(32);
            // The wrong bits at each of 32 places at once, counted to four in three bits a
            // place: beyond that a place does not fit.
            let (mut ones, mut twos, mut fours, mut over) = (0u32, 0u32, 0u32, 0u32);
            for (k, &offset) in OFFSETS[..SYNC_BITS].iter().enumerate() {
                let heard = self.window(first + offset);
                let wrong = if SYNC >> (SYNC_BITS - 1 - k) & 1 == 1 { !heard } else { heard };
                let carry = ones & wrong;
                ones ^= wrong;
                let carried = twos & carry;
                twos ^= carry;
                over |= fours & carried;
                fours ^= carried;
            }
            let mut fits = !over & (!fours | !(twos | ones));
            if width < 32 {
                fits &= (1 << width) - 1;
            }
            while fits != 0 {
                let at = first + fits.trailing_zeros() as usize;
                fits &= fits - 1;
                self.fitted += 1;
                if at >= self.past {
                    self.group(at, out);
                }
            }
            self.next = first + width;
        }
        // What is before the next place to try is done with, a whole word at a time.
        let words = (self.next - self.base) / 32;
        self.signs.drain(..words);
        self.power.drain(..words * 32);
        self.base += words * 32;
        self.held -= words * 32;
    }

    /// The places from `at` that fit are one message's: it is read at the middle one of those
    /// that fit best.
    fn group(&mut self, at: usize, out: &mut Vec<Frame>) {
        let wrong: [u32; GROUP] = std::array::from_fn(|m| self.wrong(at + m));
        let least = wrong.iter().copied().min().unwrap_or(0);
        let best: Vec<usize> = (0..GROUP).filter(|&m| wrong[m] == least).collect();
        let middle = at + best[(best.len() - 1) / 2];
        self.past = at + GROUP;
        let mut block = [0u8; BLOCK];
        for (k, &offset) in OFFSETS[SYNC_BITS..].iter().enumerate() {
            if self.bit(middle + offset) {
                block[k / 8] |= 0x80 >> (k % 8);
            }
        }
        // The length its first five bits say first; a wrong bit there may say the other.
        let short_said = block[0] >> 3 == 0;
        for long in [!short_said, short_said] {
            let (len, roots, bytes) = if long { (BLOCK, 14, LONG) } else { (30, 12, SHORT) };
            let mut tried = block;
            let Some(mended) = code::correct(&mut tried[..len], roots) else { continue };
            if (tried[0] >> 3 == 0) == long {
                continue;
            }
            let mut payload = [0u8; LONG];
            payload[..bytes].copy_from_slice(&tried[..bytes]);
            let from = middle - self.base;
            let mean = self.power[from..from + LEVEL].iter().map(|&p| u64::from(p)).sum::<u64>() as f32 / LEVEL as f32;
            let level_dbfs = 10.0 * (mean.max(1.0) / (FULL * FULL) as f32).log10();
            out.push(Frame { at: self.origin + middle as u64, payload, long, mended: mended as u8, level_dbfs });
            self.past = middle + OFFSETS[SYNC_BITS + len * 8 - 1];
            return;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Read;

    use super::message::tests::{bytes, LONG_CALLSIGN, SHORT_HEARD};
    use super::*;

    /// The window's bytes with `messages` in it, each a message's bytes sent from its sample at
    /// its amplitude, over a little noise: the link's keying a megahertz under the middle.
    pub(crate) fn window(samples: usize, messages: &[(usize, &str, f64)]) -> Vec<u8> {
        let mut seed = 0x1234_5679u32;
        let mut noise = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            f64::from(seed % 9) - 4.0
        };
        let mut level = vec![(0.0f64, 0.0f64); samples];
        for &(at, hex, amplitude) in messages {
            let payload = bytes(hex);
            let roots = if payload.len() == SHORT { 12 } else { 14 };
            let mut block = payload.clone();
            block.extend(code::parity(&payload, roots));
            let mut sent: Vec<bool> = (0..SYNC_BITS).map(|k| SYNC >> (SYNC_BITS - 1 - k) & 1 == 1).collect();
            sent.extend(block.iter().flat_map(|&b| (0..8).map(move |k| b & (0x80 >> k) != 0)));
            // The phase runs on from bit to bit, 0.3 of a turn each, and a quarter of a turn
            // back each sample for the megahertz the link is under the LO.
            let mut phase = 0.0f64;
            for (n, slot) in level.iter_mut().enumerate().skip(at) {
                let bit = ((n - at) as f64 / 3.84) as usize;
                if bit >= sent.len() {
                    break;
                }
                phase += std::f64::consts::TAU * (if sent[bit] { 312_500.0 } else { -312_500.0 } - 1_000_000.0) / 4_000_000.0;
                *slot = (amplitude * phase.cos(), amplitude * phase.sin());
            }
        }
        level.iter().flat_map(|&(i, q)| [((i + noise()) as i16).to_le_bytes(), ((q + noise()) as i16).to_le_bytes()]).flatten().collect()
    }

    #[test]
    fn the_bits_are_read_at_their_middles() {
        assert_eq!((OFFSETS[0], OFFSETS[1], OFFSETS[2], OFFSETS[3], OFFSETS[35], OFFSETS[36], LAST), (2, 6, 10, 13, 136, 140, 1611));
    }

    #[test]
    fn messages_are_found_where_they_are_whatever_the_blocks() {
        let sent = [(1_000, SHORT_HEARD, 400.0), (4_001, LONG_CALLSIGN, 60.0), (9_002, SHORT_HEARD, 25.0)];
        let window = window(14_000, &sent);
        for block in [window.len(), 4096, 1028, 400] {
            let (mut demod, mut found) = (Demod::default(), Vec::new());
            for part in window.chunks(block) {
                demod.push(part, &mut found);
            }
            let got: Vec<(Vec<u8>, bool, u8)> = found.iter().map(|f| (f.bytes().to_vec(), f.long, f.mended)).collect();
            let want: Vec<(Vec<u8>, bool, u8)> = sent.iter().map(|&(_, hex, _)| (bytes(hex), hex.len() == 2 * LONG, 0)).collect();
            assert_eq!(got, want, "in blocks of {block} bytes");
            // Each where it was sent, to the sample or two the middle of those that fit is.
            for (f, &(at, _, _)) in found.iter().zip(&sent) {
                assert!(f.at.abs_diff(at as u64) <= 2, "{} for {at}", f.at);
            }
            assert!((found[0].level_dbfs + 14.2).abs() < 0.5 && (found[2].level_dbfs + 38.0).abs() < 2.0, "{:?}", found.iter().map(|f| f.level_dbfs).collect::<Vec<_>>());
            assert!(demod.fitted() >= 3);
        }
    }

    #[test]
    fn wrong_bytes_are_put_right_and_too_many_or_noise_are_nothing() {
        let clean = window(4_000, &[(1_000, SHORT_HEARD, 400.0)]);
        // A stretch of the message silenced: six of its bytes, which its code puts right.
        let silenced = |from_bit: usize, bits: usize| {
            let mut bytes = clean.clone();
            let (from, to) = ((1_000 + OFFSETS[SYNC_BITS + from_bit] - 2) * 4, (1_000 + OFFSETS[SYNC_BITS + from_bit + bits] - 2) * 4);
            bytes[from..to].fill(0);
            bytes
        };
        let (mut demod, mut found) = (Demod::default(), Vec::new());
        demod.push(&silenced(80, 44), &mut found);
        assert_eq!(found.len(), 1);
        assert!(found[0].bytes() == bytes(SHORT_HEARD) && (1..=6).contains(&found[0].mended), "{:?}", found[0]);
        // More than it mends, and nothing but noise: nothing.
        let (mut demod, mut found) = (Demod::default(), Vec::new());
        demod.push(&silenced(40, 150), &mut found);
        demod.push(&window(60_000, &[]), &mut found);
        assert_eq!(found, []);
    }

    #[test]
    fn after_lost_samples_the_count_goes_on() {
        let (mut demod, mut found) = (Demod::default(), Vec::new());
        demod.push(&window(6_000, &[(1_000, SHORT_HEARD, 400.0)]), &mut found);
        demod.afresh();
        demod.push(&window(6_000, &[(500, LONG_CALLSIGN, 400.0)]), &mut found);
        assert_eq!(found.len(), 2);
        assert!(found[0].at.abs_diff(1_000) <= 2 && found[1].at.abs_diff(6_500) <= 2, "{} {}", found[0].at, found[1].at);
    }

    /// The captures of `UAT_CAPTURE_DIR` that have the look tool's list of their messages
    /// beside them (`tools/uat_look.py FILE --messages`): this finds the same ones, at the same
    /// samples, with as many bytes put right, whatever the blocks it is given.
    #[test]
    fn a_capture_gives_the_messages_the_look_tool_lists() {
        let Ok(dir) = std::env::var("UAT_CAPTURE_DIR") else { return };
        let mut held = 0;
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let listed = path.with_extension("messages");
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if path.extension().is_none_or(|e| e != "cs16") || !name.starts_with("uat_") || !listed.exists() {
                continue;
            }
            let want: Vec<String> = std::fs::read_to_string(&listed).unwrap().lines().map(str::to_string).collect();
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
            let got: Vec<String> = found.iter().map(|f| format!("{} {} {}", f.at, f.bytes().iter().map(|b| format!("{b:02x}")).collect::<String>(), f.mended)).collect();
            println!("{name}: {} messages ({} long), {} places fitted", got.len(), found.iter().filter(|f| f.long).count(), demod.fitted());
            assert_eq!(got, want, "{name}");
            held += 1;
        }
        assert!(held > 0, "no UAT capture with its messages listed in {dir}");
    }
}

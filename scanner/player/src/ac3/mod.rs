//! AC-3 (ATSC A/52): a syncframe's six audio blocks to 1,536 samples a channel, following the
//! standard's decoding process (section 6):
//!
//! - the syncinfo and bit stream information (5.3.1, 5.3.2), both CRCs checked (7.10);
//! - each block's side information (5.3.3): block switching and dither flags, dynamic range,
//!   coupling, rematrixing, exponent strategies;
//! - the exponents (7.1), the parametric bit allocation (7.2), the mantissas ungrouped and
//!   dequantized with dither where none were sent (7.3);
//! - decoupling (7.4), rematrixing (7.5), dynamic range (7.7.1);
//! - the inverse transform, window and overlap (7.9, `imdct`);
//! - a two-channel downmix for the player (7.8.2, LoRo).
//!
//! Channels come out in A/52's order: the full-bandwidth ones as Table 5.8 lists them for acmod,
//! then the LFE.

mod imdct;
pub mod tables;

pub use imdct::window;

use crate::bits::Bits;
use std::f32::consts::FRAC_1_SQRT_2;
use tables::*;

pub const BLOCK: usize = 256;
pub const FRAME_SAMPLES: usize = 6 * BLOCK;
/// The coupling channel's and the LFE's places in the per-channel arrays.
const CPL: usize = 5;
const LFE: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a syncframe: no sync word, or a reserved sample rate or frame size.
    Sync,
    /// A CRC failed.
    Crc,
    /// A bit stream this decoder does not decode (bsid above 8: E-AC-3 and later).
    Bsid(u8),
    /// A value the standard reserves or forbids, or a block that reads past the frame.
    Invalid,
}

/// The channels an audio coding mode carries, as A/52 writes them (Table 5.8): "3/2", "2/0",
/// "1+1" and so on, with "+LFE".
pub fn channels(acmod: u8, lfe: bool) -> String {
    let base = ["1+1", "1/0", "2/0", "3/0", "2/1", "3/1", "2/2", "3/2"][usize::from(acmod & 7)];
    format!("{base}{}", if lfe { "+LFE" } else { "" })
}

/// A syncframe's length in bytes, from its first five; `None` if they do not start one.
pub fn frame_len(h: &[u8]) -> Option<usize> {
    if h.len() < 5 || h[0] != 0x0B || h[1] != 0x77 {
        return None;
    }
    let (fscod, frmsizecod) = (usize::from(h[4] >> 6), usize::from(h[4] & 0x3F));
    if fscod == 3 || frmsizecod >= FRAME_WORDS.len() {
        return None;
    }
    Some(usize::from(FRAME_WORDS[frmsizecod][fscod]) * 2)
}

/// CRC-16, x^16 + x^15 + x^2 + 1, most significant bit first (7.10.1).
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

/// A decoded syncframe.
#[derive(Debug, Clone)]
pub struct Frame {
    pub sample_rate: u32,
    pub acmod: u8,
    pub lfe: bool,
    pub bitrate_kbps: u32,
    /// Each channel's samples, in A/52's channel order.
    pub pcm: Vec<Vec<f32>>,
    /// Center and surround mix levels for a downmix.
    pub clev: f32,
    pub slev: f32,
    /// With `Decoder::record`: each channel's six blocks.
    pub record: Vec<Vec<BlockRecord>>,
}

/// What a block was, for tests: its transform length, and the coefficients that carried no
/// bits (dither or zero; after rematrixing, either channel's).
#[derive(Debug, Clone)]
pub struct BlockRecord {
    pub short: bool,
    pub noise: [bool; 256],
}

impl Frame {
    /// The frame as two channels (LoRo, 7.8.2), scaled so that no output can exceed full scale
    /// from full-scale inputs; the LFE is left out, as the standard allows.
    pub fn stereo(&self) -> Vec<[f32; 2]> {
        let mut out = vec![[0f32; 2]; FRAME_SAMPLES];
        let ch = |i: usize| &self.pcm[i];
        // (input channel, gain to left, gain to right) by acmod.
        let mix: Vec<(usize, f32, f32)> = match self.acmod {
            0 => vec![(0, 1.0, 0.0), (1, 0.0, 1.0)],
            1 => vec![(0, FRAC_1_SQRT_2, FRAC_1_SQRT_2)],
            2 => vec![(0, 1.0, 0.0), (1, 0.0, 1.0)],
            3 => vec![(0, 1.0, 0.0), (1, self.clev, self.clev), (2, 0.0, 1.0)],
            4 => vec![(0, 1.0, 0.0), (1, 0.0, 1.0), (2, FRAC_1_SQRT_2 * self.slev, FRAC_1_SQRT_2 * self.slev)],
            5 => vec![(0, 1.0, 0.0), (1, self.clev, self.clev), (2, 0.0, 1.0), (3, FRAC_1_SQRT_2 * self.slev, FRAC_1_SQRT_2 * self.slev)],
            6 => vec![(0, 1.0, 0.0), (1, 0.0, 1.0), (2, self.slev, 0.0), (3, 0.0, self.slev)],
            _ => vec![(0, 1.0, 0.0), (1, self.clev, self.clev), (2, 0.0, 1.0), (3, self.slev, 0.0), (4, 0.0, self.slev)],
        };
        let norm = 1.0 / mix.iter().map(|m| m.1).sum::<f32>().max(mix.iter().map(|m| m.2).sum::<f32>()).max(1.0);
        for (c, l, r) in mix {
            for (o, &s) in out.iter_mut().zip(ch(c)) {
                o[0] += s * l * norm;
                o[1] += s * r * norm;
            }
        }
        out
    }
}

/// What persists from block to block within a syncframe.
struct State {
    acmod: usize,
    nfchans: usize,
    lfeon: bool,
    fscod: usize,
    // Coupling.
    cplinu: bool,
    chincpl: [bool; 5],
    phsflginu: bool,
    cplbegf: usize,
    cplendf: usize,
    cplbndstrc: [bool; 18],
    ncplbnd: usize,
    /// Each coupled channel's coordinate for each coupling band, and the 2/0 phase flags.
    cplco: [[f32; 18]; 5],
    phsflg: [bool; 18],
    rematflg: [bool; 4],
    // Exponents and bit allocation, by channel (CPL and LFE included).
    endmant: [usize; 5],
    exps: [[u8; 256]; 7],
    bap: [[u8; 256]; 7],
    sdcycod: usize,
    fdcycod: usize,
    sgaincod: usize,
    dbpbcod: usize,
    floorcod: usize,
    csnroffst: i32,
    fsnroffst: [i32; 7],
    fgaincod: [usize; 7],
    cplfleak: i32,
    cplsleak: i32,
    deltbae: [u8; 7],
    delta: [Vec<(usize, usize, i32)>; 7],
    dynrng: [f32; 2],
}

impl State {
    fn new(acmod: usize, lfeon: bool, fscod: usize) -> State {
        State {
            acmod,
            nfchans: NFCHANS[acmod],
            lfeon,
            fscod,
            cplinu: false,
            chincpl: [false; 5],
            phsflginu: false,
            cplbegf: 0,
            cplendf: 0,
            cplbndstrc: [false; 18],
            ncplbnd: 0,
            cplco: [[0.0; 18]; 5],
            phsflg: [false; 18],
            rematflg: [false; 4],
            endmant: [0; 5],
            exps: [[0; 256]; 7],
            bap: [[0; 256]; 7],
            sdcycod: 0,
            fdcycod: 0,
            sgaincod: 0,
            dbpbcod: 0,
            floorcod: 0,
            csnroffst: 0,
            fsnroffst: [0; 7],
            fgaincod: [0; 7],
            cplfleak: 0,
            cplsleak: 0,
            deltbae: [2; 7],
            delta: Default::default(),
            dynrng: [1.0; 2],
        }
    }

    fn cplstrtmant(&self) -> usize {
        37 + 12 * self.cplbegf
    }

    fn cplendmant(&self) -> usize {
        37 + 12 * (self.cplendf + 3)
    }

    /// A channel's mantissa range: (start, end).
    fn range(&self, ch: usize) -> (usize, usize) {
        match ch {
            CPL => (self.cplstrtmant(), self.cplendmant()),
            LFE => (0, 7),
            _ => (0, self.endmant[ch]),
        }
    }
}

/// Grouped mantissas left over from a group already read (3-, 5- and 11-level quantizers).
#[derive(Default)]
struct Groups {
    b1: (usize, [u32; 3]),
    b2: (usize, [u32; 3]),
    b4: (usize, [u32; 2]),
}

pub struct Decoder {
    imdct: imdct::Imdct,
    delay: [[f32; 256]; 6],
    rng: u32,
    /// The share of the dynamic range control applied: 1 (the default) is all of it.
    pub drc_scale: f32,
    /// Dither where no mantissa bits were sent (7.3.4); off, those coefficients are zero.
    pub dither: bool,
    /// Keep each block's `BlockRecord` in the frame.
    pub record: bool,
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder { imdct: imdct::Imdct::default(), delay: [[0.0; 256]; 6], rng: 0x1234_5678, drc_scale: 1.0, dither: true, record: false }
    }
}

/// A dynamic range word as a gain (7.7.1): X0 X1 X2 . Y3 Y4 Y5 Y6 Y7, X signed, 0.1Y unsigned.
fn dynrng_gain(word: u32, scale: f32) -> f32 {
    let x = ((word as i32) << 24 >> 29) + 1;
    let y = (32 + (word & 31)) as f32 / 64.0;
    (2f32.powi(x) * y).powf(scale)
}

impl Decoder {
    fn dither_value(&mut self) -> f32 {
        // A 32-bit LCG; its top bits, uniform over ±0.707 (7.3.4).
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        ((self.rng >> 8) as f32 / (1u32 << 24) as f32 * 2.0 - 1.0) * 0.707
    }

    /// Decodes one syncframe (exactly `frame_len` bytes).
    pub fn decode(&mut self, frame: &[u8]) -> Result<Frame, Error> {
        let len = frame_len(frame).ok_or(Error::Sync)?;
        if frame.len() < len {
            return Err(Error::Sync);
        }
        let frame = &frame[..len];
        // crc1 covers the first 5/8 of the frame (after the sync word), crc2 all of it.
        let five_eighths = (((len / 2) >> 1) + ((len / 2) >> 3)) * 2;
        if crc16(&frame[2..five_eighths]) != 0 || crc16(&frame[2..]) != 0 {
            return Err(Error::Crc);
        }
        let mut b = Bits::new(frame);
        b.skip(32);
        let fscod = b.get(2) as usize;
        let frmsizecod = b.get(6) as usize;
        // bsi (5.3.2).
        let bsid = b.get(5) as u8;
        if bsid > 8 {
            return Err(Error::Bsid(bsid));
        }
        let _bsmod = b.get(3);
        let acmod = b.get(3) as usize;
        let mut clev = CLEV[1];
        let mut slev = SLEV[1];
        if acmod & 1 == 1 && acmod != 1 {
            clev = CLEV[b.get(2) as usize];
        }
        if acmod & 4 != 0 {
            slev = SLEV[b.get(2) as usize];
        }
        if acmod == 2 {
            b.skip(2); // dsurmod
        }
        let lfeon = b.flag();
        let skip_ch_info = |b: &mut Bits| {
            b.skip(5); // dialnorm
            if b.flag() {
                b.skip(8); // compr
            }
            if b.flag() {
                b.skip(8); // langcod
            }
            if b.flag() {
                b.skip(7); // mixlevel, roomtyp
            }
        };
        skip_ch_info(&mut b);
        if acmod == 0 {
            skip_ch_info(&mut b);
        }
        b.skip(2); // copyrightb, origbs
        for _ in 0..2 {
            if b.flag() {
                b.skip(14); // timecod1, timecod2 (xbsi1, xbsi2 for bsid 6: the same sizes)
            }
        }
        if b.flag() {
            let addbsil = b.get(6) as usize;
            b.skip((addbsil + 1) * 8);
        }
        let mut s = State::new(acmod, lfeon, fscod);
        let nch = s.nfchans + usize::from(lfeon);
        let mut pcm = vec![vec![0f32; FRAME_SAMPLES]; nch];
        let mut record = vec![Vec::new(); if self.record { nch } else { 0 }];
        for blk in 0..6 {
            let mut coef = [[0f32; 256]; 6];
            let (blksw, noise) = self.block(&mut b, &mut s, blk, &mut coef)?;
            for (ch, r) in record.iter_mut().enumerate() {
                let src = if ch < s.nfchans { ch } else { 5 };
                r.push(BlockRecord { short: src < 5 && blksw[src], noise: noise[src] });
            }
            for ch in 0..nch {
                let (src, short) = if ch < s.nfchans { (ch, blksw[ch]) } else { (5, false) };
                self.imdct.block(&coef[src], short, &mut self.delay[src], &mut pcm[ch][blk * BLOCK..(blk + 1) * BLOCK]);
            }
        }
        if b.overrun() || b.position() > (len - 2) * 8 {
            return Err(Error::Invalid);
        }
        Ok(Frame {
            sample_rate: [48_000, 44_100, 32_000][fscod],
            acmod: acmod as u8,
            lfe: lfeon,
            bitrate_kbps: [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640][frmsizecod / 2],
            pcm,
            clev,
            slev,
            record,
        })
    }

    /// One audio block (5.3.3) to its coefficients: each full-bandwidth channel in `coef[ch]`,
    /// the LFE in `coef[5]`. Returns the block switch flags, and each channel's coefficients
    /// that carried no bits.
    #[allow(clippy::type_complexity)]
    fn block(&mut self, b: &mut Bits, s: &mut State, blk: usize, coef: &mut [[f32; 256]; 6]) -> Result<([bool; 5], [[bool; 256]; 6]), Error> {
        let nf = s.nfchans;
        let mut blksw = [false; 5];
        let mut dithflag = [false; 5];
        for f in blksw.iter_mut().take(nf) {
            *f = b.flag();
        }
        for f in dithflag.iter_mut().take(nf) {
            *f = b.flag();
        }
        for i in 0..1 + usize::from(s.acmod == 0) {
            if b.flag() {
                s.dynrng[i] = dynrng_gain(b.get(8), self.drc_scale);
            } else if blk == 0 {
                s.dynrng[i] = 1.0;
            }
        }
        // Coupling strategy.
        let was_cpl = s.chincpl;
        if b.flag() {
            s.cplinu = b.flag();
            if s.cplinu {
                for ch in 0..nf {
                    s.chincpl[ch] = b.flag();
                }
                s.phsflginu = s.acmod == 2 && b.flag();
                s.cplbegf = b.get(4) as usize;
                s.cplendf = b.get(4) as usize;
                if s.cplbegf > s.cplendf + 2 {
                    return Err(Error::Invalid);
                }
                let nsub = 3 + s.cplendf - s.cplbegf;
                s.cplbndstrc = [false; 18];
                for sb in 1..nsub {
                    s.cplbndstrc[sb] = b.flag();
                }
                s.ncplbnd = nsub - s.cplbndstrc[1..nsub].iter().filter(|&&x| x).count();
            } else {
                s.chincpl = [false; 5];
                s.phsflginu = false;
            }
        } else if blk == 0 {
            return Err(Error::Invalid);
        }
        // Coupling coordinates and phase flags.
        if s.cplinu {
            let mut any = false;
            for ch in 0..nf {
                if !s.chincpl[ch] {
                    continue;
                }
                if b.flag() {
                    any = true;
                    let mstrcplco = 3 * b.get(2) as i32;
                    for bnd in 0..s.ncplbnd {
                        let (exp, mant) = (b.get(4) as i32, b.get(4) as f32);
                        let m = if exp == 15 { mant / 16.0 } else { (mant + 16.0) / 32.0 };
                        s.cplco[ch][bnd] = m * 2f32.powi(-(exp + mstrcplco));
                    }
                } else if !was_cpl[ch] {
                    return Err(Error::Invalid);
                }
            }
            if s.acmod == 2 && s.phsflginu && any {
                for bnd in 0..s.ncplbnd {
                    s.phsflg[bnd] = b.flag();
                }
            } else if !s.phsflginu {
                s.phsflg = [false; 18];
            }
        }
        // Rematrixing flags (2/0 only).
        let nrematbd = if !s.cplinu || s.cplbegf > 2 {
            4
        } else if s.cplbegf > 0 {
            3
        } else {
            2
        };
        if s.acmod == 2 {
            if b.flag() {
                for f in s.rematflg.iter_mut().take(nrematbd) {
                    *f = b.flag();
                }
            } else if blk == 0 {
                return Err(Error::Invalid);
            }
        }
        // Exponent strategies.
        let cplexpstr = if s.cplinu { b.get(2) } else { 0 };
        let mut chexpstr = [0u32; 5];
        for e in chexpstr.iter_mut().take(nf) {
            *e = b.get(2);
        }
        let lfeexpstr = if s.lfeon { b.get(1) } else { 0 };
        for ch in 0..nf {
            if chexpstr[ch] != 0 {
                if s.chincpl[ch] {
                    s.endmant[ch] = s.cplstrtmant();
                } else {
                    let chbwcod = b.get(6) as usize;
                    if chbwcod > 60 {
                        return Err(Error::Invalid);
                    }
                    s.endmant[ch] = 37 + 3 * (chbwcod + 12);
                }
            } else if blk == 0 {
                return Err(Error::Invalid);
            }
        }
        // Exponents (7.1.3).
        if s.cplinu && cplexpstr != 0 {
            let absexp = (b.get(4) as u8) << 1;
            let (start, end) = (s.cplstrtmant(), s.cplendmant());
            let grpsize = 1usize << (cplexpstr - 1);
            let ngrps = (end - start) / (3 * grpsize);
            decode_exponents(b, absexp, ngrps, grpsize, &mut s.exps[CPL][start..end], false)?;
        }
        for ch in 0..nf {
            if chexpstr[ch] != 0 {
                let grpsize = 1usize << (chexpstr[ch] - 1);
                let end = s.endmant[ch];
                let ngrps = (end - 1 + 3 * grpsize - 3) / (3 * grpsize);
                let absexp = b.get(4) as u8;
                decode_exponents(b, absexp, ngrps, grpsize, &mut s.exps[ch][..end], true)?;
                b.skip(2); // gainrng
            }
        }
        if s.lfeon && lfeexpstr != 0 {
            let absexp = b.get(4) as u8;
            decode_exponents(b, absexp, 2, 1, &mut s.exps[LFE][..7], true)?;
        }
        // Bit allocation parameters (5.3.3, 7.2).
        if b.flag() {
            s.sdcycod = b.get(2) as usize;
            s.fdcycod = b.get(2) as usize;
            s.sgaincod = b.get(2) as usize;
            s.dbpbcod = b.get(2) as usize;
            s.floorcod = b.get(3) as usize;
        } else if blk == 0 {
            return Err(Error::Invalid);
        }
        if b.flag() {
            s.csnroffst = b.get(6) as i32;
            if s.cplinu {
                s.fsnroffst[CPL] = b.get(4) as i32;
                s.fgaincod[CPL] = b.get(3) as usize;
            }
            for ch in 0..nf {
                s.fsnroffst[ch] = b.get(4) as i32;
                s.fgaincod[ch] = b.get(3) as usize;
            }
            if s.lfeon {
                s.fsnroffst[LFE] = b.get(4) as i32;
                s.fgaincod[LFE] = b.get(3) as usize;
            }
        } else if blk == 0 {
            return Err(Error::Invalid);
        }
        if s.cplinu {
            if b.flag() {
                s.cplfleak = b.get(3) as i32;
                s.cplsleak = b.get(3) as i32;
            } else if !was_cpl.iter().any(|&c| c) {
                return Err(Error::Invalid);
            }
        }
        // Delta bit allocation.
        if b.flag() {
            if s.cplinu {
                s.deltbae[CPL] = b.get(2) as u8;
            }
            for ch in 0..nf {
                s.deltbae[ch] = b.get(2) as u8;
            }
            let order: Vec<usize> = if s.cplinu { std::iter::once(CPL).chain(0..nf).collect() } else { (0..nf).collect() };
            for ch in order {
                match s.deltbae[ch] {
                    1 => {
                        let nseg = b.get(3) as usize + 1;
                        s.delta[ch] = (0..nseg).map(|_| (b.get(5) as usize, b.get(4) as usize, b.get(3) as i32)).collect();
                    }
                    3 => return Err(Error::Invalid),
                    _ => {}
                }
            }
        } else if blk == 0 {
            s.deltbae = [2; 7];
        }
        // Skipped data.
        if b.flag() {
            let skipl = b.get(9) as usize;
            b.skip(skipl * 8);
        }
        // Bit allocation (7.2.2), every channel in use.
        let all_zero = s.csnroffst == 0 && (0..7).filter(|&c| c < nf || (c == CPL && s.cplinu) || (c == LFE && s.lfeon)).all(|c| s.fsnroffst[c] == 0);
        let mut chans: Vec<usize> = (0..nf).collect();
        if s.cplinu {
            chans.push(CPL);
        }
        if s.lfeon {
            chans.push(LFE);
        }
        for &ch in &chans {
            let (start, end) = s.range(ch);
            if all_zero {
                s.bap[ch] = [0; 256];
            } else {
                bit_allocation(s, ch, start, end);
            }
        }
        // Mantissas (7.3), in the bit stream's order: each channel, the coupling channel after
        // the first coupled one, then the LFE.
        let mut groups = Groups::default();
        let mut cplmant = [0f32; 256];
        let mut cpl_bap0 = [false; 256];
        let mut got_cpl = false;
        for ch in 0..nf {
            for bin in 0..s.endmant[ch] {
                let m = match s.bap[ch][bin] {
                    0 if dithflag[ch] && self.dither => self.dither_value(),
                    bap => mantissa(b, bap, &mut groups),
                };
                coef[ch][bin] = m * 2f32.powi(-i32::from(s.exps[ch][bin]));
            }
            if s.cplinu && s.chincpl[ch] && !got_cpl {
                let (start, end) = (s.cplstrtmant(), s.cplendmant());
                for bin in start..end {
                    let bap = s.bap[CPL][bin];
                    cpl_bap0[bin] = bap == 0;
                    cplmant[bin] = mantissa(b, bap, &mut groups) * 2f32.powi(-i32::from(s.exps[CPL][bin]));
                }
                got_cpl = true;
            }
        }
        if s.lfeon {
            for bin in 0..7 {
                coef[5][bin] = mantissa(b, s.bap[LFE][bin], &mut groups) * 2f32.powi(-i32::from(s.exps[LFE][bin]));
            }
        }
        if b.overrun() {
            return Err(Error::Invalid);
        }
        // Decoupling (7.4): each coupled channel from the coupling channel, its own dither where
        // the coupling channel had no bits.
        if s.cplinu {
            let (start, end) = (s.cplstrtmant(), s.cplendmant());
            for ch in 0..nf {
                if !s.chincpl[ch] {
                    continue;
                }
                // Coupling sub-band to band.
                let mut bnd = 0;
                for sb in 0..(end - start) / 12 {
                    if sb > 0 && !s.cplbndstrc[sb] {
                        bnd += 1;
                    }
                    let co = s.cplco[ch][bnd] * 8.0;
                    let flip = ch == 1 && s.acmod == 2 && s.phsflginu && s.phsflg[bnd];
                    for bin in start + 12 * sb..start + 12 * (sb + 1) {
                        let m = if cpl_bap0[bin] {
                            if dithflag[ch] && self.dither {
                                self.dither_value() * 2f32.powi(-i32::from(s.exps[CPL][bin]))
                            } else {
                                0.0
                            }
                        } else {
                            cplmant[bin]
                        };
                        coef[ch][bin] = if flip { -m * co } else { m * co };
                    }
                }
            }
        }
        // Rematrixing (7.5), up to the narrower channel's end and the coupling's start.
        if s.acmod == 2 {
            let mut top = s.endmant[0].min(s.endmant[1]);
            if s.cplinu {
                top = top.min(s.cplstrtmant());
            }
            let edges = [13usize, 25, 37, 61, 253];
            for (band, &flag) in s.rematflg.iter().enumerate().take(nrematbd) {
                if !flag {
                    continue;
                }
                for bin in edges[band]..edges[band + 1].min(top) {
                    let (l, r) = (coef[0][bin], coef[1][bin]);
                    coef[0][bin] = l + r;
                    coef[1][bin] = l - r;
                }
            }
        }
        // The coefficients that carried no bits.
        let mut noise = [[false; 256]; 6];
        for ch in 0..nf {
            for bin in 0..s.endmant[ch] {
                noise[ch][bin] = s.bap[ch][bin] == 0;
            }
            if s.cplinu && s.chincpl[ch] {
                for bin in s.cplstrtmant()..s.cplendmant() {
                    noise[ch][bin] = s.bap[CPL][bin] == 0;
                }
            }
        }
        if s.lfeon {
            for bin in 0..7 {
                noise[5][bin] = s.bap[LFE][bin] == 0;
            }
        }
        if s.acmod == 2 {
            let edges = [13usize, 25, 37, 61, 253];
            for (band, &flag) in s.rematflg.iter().enumerate().take(nrematbd) {
                if flag {
                    for bin in edges[band]..edges[band + 1] {
                        let either = noise[0][bin] || noise[1][bin];
                        noise[0][bin] = either;
                        noise[1][bin] = either;
                    }
                }
            }
        }
        // Dynamic range (7.7.1): dynrng2 for the second channel in 1+1, dynrng for the rest.
        for ch in 0..nf + usize::from(s.lfeon) {
            let g = s.dynrng[usize::from(s.acmod == 0 && ch == 1)];
            let c = if ch < nf { ch } else { 5 };
            for v in coef[c].iter_mut() {
                *v *= g;
            }
        }
        Ok((blksw, noise))
    }
}

/// Unpacks `ngrps` groups of three mapped differential exponents after the absolute one, into
/// `out` (each value repeated `grpsize` times). `first_is_absolute`: out[0] is the absolute
/// exponent (full-bandwidth and LFE channels); else out starts at the first differential's bin
/// (the coupling channel).
fn decode_exponents(b: &mut Bits, absexp: u8, ngrps: usize, grpsize: usize, out: &mut [u8], first_is_absolute: bool) -> Result<(), Error> {
    let mut prev = i32::from(absexp);
    let mut at = 0;
    if first_is_absolute {
        if out.is_empty() {
            return Err(Error::Invalid);
        }
        out[0] = absexp;
        at = 1;
    }
    for _ in 0..ngrps {
        let g = b.get(7);
        if g >= 125 {
            return Err(Error::Invalid);
        }
        for m in [g / 25, (g % 25) / 5, g % 5] {
            prev += m as i32 - 2;
            if !(0..=24).contains(&prev) {
                return Err(Error::Invalid);
            }
            for _ in 0..grpsize {
                if at < out.len() {
                    out[at] = prev as u8;
                }
                at += 1;
            }
        }
    }
    Ok(())
}

/// One mantissa's value (a fraction of full scale, before its exponent).
fn mantissa(b: &mut Bits, bap: u8, g: &mut Groups) -> f32 {
    match bap {
        0 => 0.0,
        1 => {
            if g.b1.0 == 0 {
                let c = b.get(5);
                g.b1 = (3, [c / 9, (c % 9) / 3, c % 3]);
            }
            let v = g.b1.1[3 - g.b1.0];
            g.b1.0 -= 1;
            (v as f32 - 1.0) * 2.0 / 3.0
        }
        2 => {
            if g.b2.0 == 0 {
                let c = b.get(7);
                g.b2 = (3, [c / 25, (c % 25) / 5, c % 5]);
            }
            let v = g.b2.1[3 - g.b2.0];
            g.b2.0 -= 1;
            (v as f32 - 2.0) * 2.0 / 5.0
        }
        3 => (b.get(3) as f32 - 3.0) * 2.0 / 7.0,
        4 => {
            if g.b4.0 == 0 {
                let c = b.get(7);
                g.b4 = (2, [c / 11, c % 11]);
            }
            let v = g.b4.1[2 - g.b4.0];
            g.b4.0 -= 1;
            (v as f32 - 5.0) * 2.0 / 11.0
        }
        5 => (b.get(4) as f32 - 7.0) * 2.0 / 15.0,
        _ => {
            let bits = QNTZTAB[usize::from(bap)];
            b.signed(bits) as f32 / (1u32 << (bits - 1)) as f32
        }
    }
}

fn logadd(a: i32, b: i32) -> i32 {
    let c = a - b;
    let address = ((c.abs() >> 1) as usize).min(255);
    if c >= 0 { a + i32::from(LATAB[address]) } else { b + i32::from(LATAB[address]) }
}

fn calc_lowcomp(a: i32, b0: i32, b1: i32, bin: usize) -> i32 {
    if bin < 7 {
        if b0 + 256 == b1 {
            384
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else if bin < 20 {
        if b0 + 256 == b1 {
            320
        } else if b0 > b1 {
            (a - 64).max(0)
        } else {
            a
        }
    } else {
        (a - 128).max(0)
    }
}

/// The band of each bin (Table 7.13, from the banding tables).
fn masktab() -> [u8; 256] {
    let mut m = [0u8; 256];
    for band in 0..50 {
        let (first, size) = (usize::from(BNDTAB[band]), usize::from(BNDSZ[band]));
        for v in m.iter_mut().skip(first).take(size) {
            *v = band as u8;
        }
    }
    m
}

/// The parametric bit allocation (7.2.2) for channel `ch` over bins start..end, into s.bap[ch].
fn bit_allocation(s: &mut State, ch: usize, start: usize, end: usize) {
    if end <= start {
        return;
    }
    let masktab = masktab();
    let sdecay = SLOWDEC[s.sdcycod];
    let fdecay = FASTDEC[s.fdcycod];
    let sgain = SLOWGAIN[s.sgaincod];
    let dbknee = DBPBTAB[s.dbpbcod];
    let floor = FLOORTAB[s.floorcod];
    let fgain = FASTGAIN[s.fgaincod[ch]];
    let snroffset = (((s.csnroffst - 15) << 4) + s.fsnroffst[ch]) << 2;
    let exp = &s.exps[ch];
    // Exponents to PSD.
    let mut psd = [0i32; 256];
    for bin in start..end {
        psd[bin] = 3072 - (i32::from(exp[bin]) << 7);
    }
    // PSD integration.
    let mut bndpsd = [0i32; 50];
    let mut j = start;
    let mut k = usize::from(masktab[start]);
    loop {
        let lastbin = (usize::from(BNDTAB[k]) + usize::from(BNDSZ[k])).min(end);
        bndpsd[k] = psd[j];
        j += 1;
        while j < lastbin {
            bndpsd[k] = logadd(bndpsd[k], psd[j]);
            j += 1;
        }
        k += 1;
        if end <= lastbin {
            break;
        }
    }
    // Excitation.
    let bndstrt = usize::from(masktab[start]);
    let bndend = usize::from(masktab[end - 1]) + 1;
    let mut excite = [0i32; 50];
    let (mut fastleak, mut slowleak) = (0, 0);
    let begin;
    if bndstrt == 0 {
        let lfe_last = |bin: usize| bndend == 7 && bin == 6;
        let mut lowcomp = calc_lowcomp(0, bndpsd[0], bndpsd[1], 0);
        excite[0] = bndpsd[0] - fgain - lowcomp;
        lowcomp = calc_lowcomp(lowcomp, bndpsd[1], bndpsd[2], 1);
        excite[1] = bndpsd[1] - fgain - lowcomp;
        let mut b7 = 7;
        for bin in 2..7 {
            if !lfe_last(bin) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = bndpsd[bin] - fgain;
            slowleak = bndpsd[bin] - sgain;
            excite[bin] = fastleak - lowcomp;
            if !lfe_last(bin) && bndpsd[bin] <= bndpsd[bin + 1] {
                b7 = bin + 1;
                break;
            }
        }
        for bin in b7..bndend.min(22) {
            if !lfe_last(bin) {
                lowcomp = calc_lowcomp(lowcomp, bndpsd[bin], bndpsd[bin + 1], bin);
            }
            fastleak = (fastleak - fdecay).max(bndpsd[bin] - fgain);
            slowleak = (slowleak - sdecay).max(bndpsd[bin] - sgain);
            excite[bin] = (fastleak - lowcomp).max(slowleak);
        }
        begin = 22;
    } else {
        fastleak = (s.cplfleak << 8) + 768;
        slowleak = (s.cplsleak << 8) + 768;
        begin = bndstrt;
    }
    for bin in begin..bndend {
        fastleak = (fastleak - fdecay).max(bndpsd[bin] - fgain);
        slowleak = (slowleak - sdecay).max(bndpsd[bin] - sgain);
        excite[bin] = fastleak.max(slowleak);
    }
    // Masking curve.
    let mut mask = [0i32; 50];
    for bin in bndstrt..bndend {
        if bndpsd[bin] < dbknee {
            excite[bin] += (dbknee - bndpsd[bin]) >> 2;
        }
        mask[bin] = excite[bin].max(i32::from(HTH[s.fscod][bin]));
    }
    // Delta bit allocation.
    if ch != LFE && (s.deltbae[ch] == 0 || s.deltbae[ch] == 1) {
        let mut band = 0;
        for &(offst, len, ba) in &s.delta[ch] {
            band += offst;
            let delta = if ba >= 4 { (ba - 3) << 7 } else { (ba - 4) << 7 };
            for _ in 0..len {
                if band < 50 {
                    mask[band] += delta;
                }
                band += 1;
            }
        }
    }
    // Bit allocation pointers.
    let mut i = start;
    let mut j = usize::from(masktab[start]);
    loop {
        let lastbin = (usize::from(BNDTAB[j]) + usize::from(BNDSZ[j])).min(end);
        let mut m = mask[j] - snroffset - floor;
        if m < 0 {
            m = 0;
        }
        m &= 0x1fe0;
        m += floor;
        while i < lastbin {
            let address = ((psd[i] - m) >> 5).clamp(0, 63) as usize;
            s.bap[ch][i] = BAPTAB[address];
            i += 1;
        }
        j += 1;
        if end <= lastbin {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masktab_is_table_7_13() {
        // Spot values from the printed table.
        let m = masktab();
        assert_eq!((m[0], m[27], m[28], m[30], m[31], m[48], m[49], m[96], m[97], m[252]), (0, 27, 28, 28, 29, 34, 35, 41, 42, 49));
    }

    #[test]
    fn frame_sizes_follow_table_5_18() {
        assert_eq!(frame_len(&[0x0B, 0x77, 0, 0, 0b00_011110]), Some(896 * 2), "448 kbps at 48 kHz");
        assert_eq!(frame_len(&[0x0B, 0x77, 0, 0, 0b01_000001]), Some(70 * 2), "32 kbps at 44.1 kHz, odd");
        assert_eq!(frame_len(&[0x0B, 0x77, 0, 0, 0b11_000000]), None, "reserved rate");
    }

    #[test]
    fn dynamic_range_words_are_their_gains() {
        assert_eq!(dynrng_gain(0, 1.0), 1.0, "0 dB");
        assert!((dynrng_gain(0b011_11111, 1.0) - 16.0 * 63.0 / 64.0).abs() < 1e-6, "+23.95 dB");
        assert!((dynrng_gain(0b100_00000, 1.0) - 0.125 * 0.5).abs() < 1e-6, "-24.08 dB");
        assert_eq!(dynrng_gain(0b100_00000, 0.0), 1.0, "no compression");
    }

    #[test]
    fn data_followed_by_its_crc_leaves_zero() {
        // As crc2 at a frame's end: the register reads zero after it (7.10.1).
        let mut d = vec![0x12, 0x34, 0x56, 0x0B];
        let c = crc16(&d);
        d.extend_from_slice(&c.to_be_bytes());
        assert_eq!(crc16(&d), 0);
        d[1] ^= 4;
        assert_ne!(crc16(&d), 0);
    }
}

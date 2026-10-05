//! MPEG-2 video (ITU-T H.262, 02/2000 with its corrigenda), Main Profile: 4:2:0, I, P and B
//! pictures, frame and field pictures, every prediction mode. The elementary stream comes in any
//! pieces with the PTS of the PES packets it came in; decoded frames come out in display order
//! with their presentation time.
//!
//! - The stream is cut at its start codes (6.2.1); each unit is handled when the next start code
//!   ends it, a slice decoded as soon as it is whole.
//! - A PES packet's PTS belongs to the first picture that starts in it (H.222.0 2.4.3.7); a frame
//!   without one takes the last frame's time plus its duration (two fields, three with
//!   repeat_first_field).
//! - Reference frames and frame re-ordering (6.1.1.11): an I or P frame is output when the next
//!   one starts, a B frame when it is decoded. Decoding starts at the first I-picture; a B frame
//!   whose forward reference was never decoded (the open GOP at the start) is left out.
//! - Two field pictures make one frame (6.3.10); the second field of a P frame predicts the
//!   opposite parity from the first.
//! - A slice that fails to decode is abandoned at the failure; the macroblocks it left are
//!   copied from the latest reference frame (or left as they were).

pub mod idct;
mod slice;
pub mod tables;
pub mod vlc;

use std::collections::VecDeque;

use tables::*;
use vlc::{Reader, Vlc};

/// The VLC tables, built once.
pub struct Tables {
    mb_addr_inc: Vlc<u8>,
    mb_type_i: Vlc<MbFlags>,
    mb_type_p: Vlc<MbFlags>,
    mb_type_b: Vlc<MbFlags>,
    cbp: Vlc<u8>,
    motion_code: Vlc<i8>,
    dmvector: Vlc<i8>,
    dc_size_luma: Vlc<u8>,
    dc_size_chroma: Vlc<u8>,
    dct_zero: Vlc<Coef>,
    dct_one: Vlc<Coef>,
    /// inverse_scan[alternate_scan][n]: the coefficient (v * 8 + u) the n-th in the scan is.
    inverse_scan: [[u8; 64]; 2],
}

impl Default for Tables {
    fn default() -> Tables {
        let mut inverse_scan = [[0u8; 64]; 2];
        for alt in 0..2 {
            for pos in 0..64 {
                inverse_scan[alt][usize::from(SCAN[alt][pos])] = pos as u8;
            }
        }
        Tables {
            mb_addr_inc: Vlc::new(&MB_ADDR_INC, 8),
            mb_type_i: Vlc::new(&MB_TYPE_I, 2),
            mb_type_p: Vlc::new(&MB_TYPE_P, 6),
            mb_type_b: Vlc::new(&MB_TYPE_B, 6),
            cbp: Vlc::new(&CBP, 9),
            motion_code: Vlc::new(&MOTION_CODE, 8),
            dmvector: Vlc::new(&DMVECTOR, 2),
            dc_size_luma: Vlc::new(&DC_SIZE_LUMA, 9),
            dc_size_chroma: Vlc::new(&DC_SIZE_CHROMA, 10),
            dct_zero: Vlc::new(&DCT_ZERO, 9),
            dct_one: Vlc::new(&DCT_ONE, 9),
            inverse_scan,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Structure {
    Top,
    Bottom,
    Frame,
}

impl Structure {
    /// A field's parity: 0 top, 1 bottom; none for a frame.
    pub fn parity(self) -> Option<usize> {
        match self {
            Structure::Top => Some(0),
            Structure::Bottom => Some(1),
            Structure::Frame => None,
        }
    }
}

/// The sequence header and its extensions (6.2.2).
#[derive(Clone, Debug)]
pub struct Sequence {
    pub width: usize,
    pub height: usize,
    pub aspect_ratio_information: u8,
    /// The frame rate as a fraction.
    pub rate_num: u32,
    pub rate_den: u32,
    pub progressive_sequence: bool,
    pub chroma_format: u8,
    /// Table 6-9 (0: not signalled).
    pub matrix_coefficients: u8,
}

/// picture_header() and picture_coding_extension() (6.2.3).
#[derive(Clone, Copy, Debug)]
pub struct Picture {
    pub coding_type: u8,
    pub temporal_reference: u16,
    pub f_code: [[u8; 2]; 2],
    pub intra_dc_precision: u8,
    pub structure: Structure,
    pub top_field_first: bool,
    pub frame_pred_frame_dct: bool,
    pub concealment_mv: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    pub repeat_first_field: bool,
    pub progressive_frame: bool,
}

/// A frame being decoded or kept as a reference.
#[derive(Default)]
pub struct Buf {
    y: Vec<u8>,
    c: [Vec<u8>; 2],
    /// The macroblocks of the picture (frame or field) being decoded that are done.
    done: Vec<bool>,
    pts: Option<u64>,
    coding_type: u8,
    top_field_first: bool,
    repeat_first_field: bool,
    progressive_frame: bool,
    /// Decoded and not yet output.
    pending: bool,
}

/// What a slice needs to decode.
pub struct Context {
    t: Tables,
    seq: Sequence,
    pic: Picture,
    mb_width: usize,
    /// Frame macroblock rows.
    mb_height: usize,
    stride: usize,
    coded_height: usize,
    intra_q: [u8; 64],
    non_intra_q: [u8; 64],
    fwd: Option<usize>,
    bwd: Option<usize>,
    cur: usize,
    second_field: bool,
}

impl Context {
    fn picture_mb_rows(&self) -> usize {
        if self.pic.structure == Structure::Frame {
            self.mb_height
        } else {
            self.mb_height / 2
        }
    }
}

/// A decoded frame: the planes at their coded size (luma `stride` wide, chroma half that each
/// way), the picture `width` × `height` at their top left.
#[derive(Clone, Debug)]
pub struct Frame {
    pub width: usize,
    pub height: usize,
    pub stride: usize,
    pub y: Vec<u8>,
    pub cb: Vec<u8>,
    pub cr: Vec<u8>,
    /// Presentation time, 90 kHz.
    pub pts: Option<u64>,
    /// 'I', 'P' or 'B' (the first field's).
    pub kind: char,
    pub progressive: bool,
    pub top_field_first: bool,
    pub repeat_first_field: bool,
    pub matrix_coefficients: u8,
    pub aspect_ratio_information: u8,
}

pub struct Decoder {
    cx: Context,
    have_seq: bool,
    /// The picture header waiting for its coding extension.
    header: Option<(u8, u16, Option<u64>)>,
    /// A picture is being decoded.
    decoding: bool,
    /// The current picture is not decoded (no reference for it), slices passed over.
    skipping: bool,
    /// The first field of a frame whose second is to come: its buffer and structure.
    first_field: Option<(usize, Structure)>,
    bufs: Vec<Buf>,
    seen_i: bool,
    es: Vec<u8>,
    /// Where es[0] is in the whole stream, where scanning resumes, and the start of the unit not
    /// yet ended.
    es_base: u64,
    scanned: usize,
    open: Option<usize>,
    stamps: VecDeque<(u64, u64)>,
    out: VecDeque<Frame>,
    last_out: Option<(u64, u64)>,
    /// Slices that failed to decode.
    pub slice_errors: u32,
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder {
            cx: Context {
                t: Tables::default(),
                seq: Sequence { width: 0, height: 0, aspect_ratio_information: 0, rate_num: 30000, rate_den: 1001, progressive_sequence: false, chroma_format: 1, matrix_coefficients: 0 },
                pic: Picture {
                    coding_type: 1,
                    temporal_reference: 0,
                    f_code: [[15; 2]; 2],
                    intra_dc_precision: 0,
                    structure: Structure::Frame,
                    top_field_first: false,
                    frame_pred_frame_dct: true,
                    concealment_mv: false,
                    q_scale_type: false,
                    intra_vlc_format: false,
                    alternate_scan: false,
                    repeat_first_field: false,
                    progressive_frame: true,
                },
                mb_width: 0,
                mb_height: 0,
                stride: 0,
                coded_height: 0,
                intra_q: DEFAULT_INTRA,
                non_intra_q: [16; 64],
                fwd: None,
                bwd: None,
                cur: 0,
                second_field: false,
            },
            have_seq: false,
            header: None,
            decoding: false,
            skipping: true,
            first_field: None,
            bufs: Vec::new(),
            seen_i: false,
            es: Vec::new(),
            es_base: 0,
            scanned: 0,
            open: None,
            stamps: VecDeque::new(),
            out: VecDeque::new(),
            last_out: None,
            slice_errors: 0,
        }
    }
}

const FRAME_RATES: [(u32, u32); 9] = [(0, 1), (24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (50, 1), (60000, 1001), (60, 1)];

/// A quantiser matrix from the stream (in the zigzag order, 7.3.1).
fn read_matrix(r: &mut Reader) -> [u8; 64] {
    let mut m = [0u8; 64];
    let coded: Vec<u8> = (0..64).map(|_| r.get(8) as u8).collect();
    for (pos, w) in m.iter_mut().enumerate() {
        *w = coded[usize::from(SCAN[0][pos])];
    }
    m
}

impl Decoder {
    pub fn sequence(&self) -> Option<&Sequence> {
        self.have_seq.then_some(&self.cx.seq)
    }

    /// The next decoded frame, in display order.
    pub fn next_frame(&mut self) -> Option<Frame> {
        self.out.pop_front()
    }

    /// A run of the elementary stream; `pts` belongs to the first picture starting in it.
    pub fn push(&mut self, data: &[u8], pts: Option<u64>) {
        if let Some(t) = pts {
            self.stamps.push_back((self.es_base + self.es.len() as u64, t));
        }
        self.es.extend_from_slice(data);
        // Each unit runs from its start code to the next.
        let mut i = self.scanned;
        while i + 2 < self.es.len() {
            if self.es[i] == 0 && self.es[i + 1] == 0 && self.es[i + 2] == 1 {
                if let Some(s) = self.open {
                    self.unit(s, i);
                }
                self.open = Some(i);
                i += 3;
            } else {
                i += 1;
            }
        }
        // Keep the open unit (or the last bytes, which may begin a start code).
        let cut = self.open.unwrap_or(i.min(self.es.len().saturating_sub(2)));
        self.es.drain(..cut);
        self.es_base += cut as u64;
        self.scanned = i - cut;
        self.open = self.open.map(|o| o - cut);
    }

    /// The end of the stream: the last unit, the last picture and the last reference frame.
    pub fn finish(&mut self) {
        if let Some(s) = self.open.take() {
            let end = self.es.len();
            if s + 3 < end {
                self.unit(s, end);
            }
        }
        self.es.clear();
        self.scanned = 0;
        self.end_picture();
        self.flush_references();
    }

    fn unit(&mut self, s: usize, e: usize) {
        if s + 3 >= e {
            return;
        }
        let code = self.es[s + 3];
        let at = self.es_base + s as u64;
        let payload = self.es[s + 4..e].to_vec();
        match code {
            0xB3 => {
                self.end_picture();
                self.sequence_header(&payload);
            }
            0xB5 => self.extension(&payload),
            0xB8 => self.end_picture(),
            0xB7 => {
                self.end_picture();
                self.flush_references();
            }
            0x00 => {
                self.end_picture();
                let mut r = Reader::new(&payload);
                let temporal_reference = r.get(10) as u16;
                let coding_type = r.get(3) as u8;
                // The PTS of the PES packet this picture starts in, if it has not been given.
                let mut pts = None;
                while self.stamps.front().is_some_and(|&(o, _)| o <= at) {
                    pts = self.stamps.pop_front().map(|(_, t)| t);
                }
                self.header = Some((coding_type, temporal_reference, pts));
            }
            0x01..=0xAF => {
                if self.decoding && !self.skipping {
                    let row = usize::from(code) - 1;
                    if row < self.cx.picture_mb_rows() && slice::decode(&self.cx, &mut self.bufs, &payload, row).is_err() {
                        self.slice_errors += 1;
                    }
                }
            }
            _ => {}
        }
    }

    fn sequence_header(&mut self, d: &[u8]) {
        let mut r = Reader::new(d);
        let w = r.get(12) as usize;
        let h = r.get(12) as usize;
        let aspect = r.get(4) as u8;
        let rate = r.get(4) as usize;
        r.skip(18 + 1 + 10 + 1);
        self.cx.intra_q = if r.flag() { read_matrix(&mut r) } else { DEFAULT_INTRA };
        self.cx.non_intra_q = if r.flag() { read_matrix(&mut r) } else { [16; 64] };
        // The low 12 bits; sequence_extension() gives the rest.
        let s = &mut self.cx.seq;
        s.width = w;
        s.height = h;
        s.aspect_ratio_information = aspect;
        let (n, d) = FRAME_RATES.get(rate).copied().unwrap_or((30000, 1001));
        if n != 0 {
            s.rate_num = n;
            s.rate_den = d;
        }
    }

    fn extension(&mut self, d: &[u8]) {
        let mut r = Reader::new(d);
        match r.get(4) {
            1 => {
                r.skip(8);
                let progressive = r.flag();
                let chroma = r.get(2) as u8;
                let hx = r.get(2) as usize;
                let vx = r.get(2) as usize;
                r.skip(12 + 1 + 8 + 1);
                let rn = r.get(2) + 1;
                let rd = r.get(5) + 1;
                let s = &mut self.cx.seq;
                s.width |= hx << 12;
                s.height |= vx << 12;
                s.progressive_sequence = progressive;
                s.chroma_format = chroma;
                let (n, dd) = FRAME_RATES.iter().copied().find(|&(n, dd)| n == s.rate_num && dd == s.rate_den).unwrap_or((s.rate_num, s.rate_den));
                s.rate_num = n * rn;
                s.rate_den = dd * rd;
                self.configure();
            }
            2 => {
                r.skip(3);
                if r.flag() {
                    r.skip(16);
                    self.cx.seq.matrix_coefficients = r.get(8) as u8;
                }
            }
            3 => {
                if r.flag() {
                    self.cx.intra_q = read_matrix(&mut r);
                }
                if r.flag() {
                    self.cx.non_intra_q = read_matrix(&mut r);
                }
            }
            8 => {
                let Some((coding_type, temporal_reference, pts)) = self.header.take() else { return };
                let mut f_code = [[0u8; 2]; 2];
                for s in &mut f_code {
                    for t in s.iter_mut() {
                        *t = r.get(4) as u8;
                    }
                }
                let p = Picture {
                    coding_type,
                    temporal_reference,
                    f_code,
                    intra_dc_precision: r.get(2) as u8,
                    structure: match r.get(2) {
                        1 => Structure::Top,
                        2 => Structure::Bottom,
                        _ => Structure::Frame,
                    },
                    top_field_first: r.flag(),
                    frame_pred_frame_dct: r.flag(),
                    concealment_mv: r.flag(),
                    q_scale_type: r.flag(),
                    intra_vlc_format: r.flag(),
                    alternate_scan: r.flag(),
                    repeat_first_field: r.flag(),
                    progressive_frame: {
                        r.skip(1);
                        r.flag()
                    },
                };
                self.start_picture(p, pts);
            }
            _ => {}
        }
    }

    /// The picture size from the sequence (6.3.3), and frame buffers to match.
    fn configure(&mut self) {
        let s = &self.cx.seq;
        let mb_width = s.width.div_ceil(16);
        let mb_height = if s.progressive_sequence { s.height.div_ceil(16) } else { 2 * s.height.div_ceil(32) };
        if self.have_seq && mb_width == self.cx.mb_width && mb_height == self.cx.mb_height {
            return;
        }
        self.cx.mb_width = mb_width;
        self.cx.mb_height = mb_height;
        self.cx.stride = mb_width * 16;
        self.cx.coded_height = mb_height * 16;
        self.have_seq = s.chroma_format == 1 && mb_width > 0 && mb_height > 0;
        self.bufs.clear();
        self.cx.fwd = None;
        self.cx.bwd = None;
        self.first_field = None;
        self.seen_i = false;
    }

    /// A buffer that is neither reference nor waiting for output.
    fn free_buf(&mut self) -> usize {
        let used = |i: usize, cx: &Context| Some(i) == cx.fwd || Some(i) == cx.bwd;
        if let Some(i) = (0..self.bufs.len()).find(|&i| !used(i, &self.cx) && !self.bufs[i].pending) {
            return i;
        }
        let (luma, chroma) = (self.cx.stride * self.cx.coded_height, self.cx.stride * self.cx.coded_height / 4);
        self.bufs.push(Buf { y: vec![16; luma], c: [vec![128; chroma], vec![128; chroma]], ..Buf::default() });
        self.bufs.len() - 1
    }

    fn start_picture(&mut self, p: Picture, pts: Option<u64>) {
        self.cx.pic = p;
        self.decoding = true;
        self.skipping = true;
        if !self.have_seq || !(1..=3).contains(&p.coding_type) {
            return;
        }
        // The second field of a frame, or a new frame.
        let second = match (self.first_field, p.structure) {
            (Some((b, s)), now) if now != Structure::Frame && now != s => Some(b),
            _ => None,
        };
        // A first field whose second never came: a B frame goes out as it is.
        if let (None, Some((b, _))) = (second, self.first_field) {
            if self.bufs[b].coding_type == 3 {
                self.output(b);
            }
        }
        self.first_field = None;
        if let Some(b) = second {
            self.cx.cur = b;
            self.cx.second_field = true;
            self.skipping = false;
        } else {
            self.cx.second_field = false;
            if p.coding_type == 1 {
                self.seen_i = true;
            }
            if !self.seen_i {
                return;
            }
            let b = self.free_buf();
            if p.coding_type != 3 {
                // The older reference is output now; the new frame becomes the newer.
                if let Some(old) = self.cx.bwd {
                    self.output(old);
                }
                self.cx.fwd = self.cx.bwd;
                self.cx.bwd = Some(b);
            } else if self.cx.fwd.is_none() || self.cx.bwd.is_none() {
                return;
            }
            if p.coding_type == 2 && self.cx.fwd.is_none() {
                return;
            }
            let f = &mut self.bufs[b];
            f.pts = pts;
            f.coding_type = p.coding_type;
            f.top_field_first = p.top_field_first;
            f.repeat_first_field = p.repeat_first_field;
            f.progressive_frame = p.progressive_frame;
            f.pending = true;
            self.cx.cur = b;
            self.skipping = false;
        }
        let mbs = self.cx.mb_width * self.cx.picture_mb_rows();
        let done = &mut self.bufs[self.cx.cur].done;
        done.clear();
        done.resize(mbs, false);
    }

    /// The current picture is complete: conceal what it lacks; a whole B frame goes out.
    fn end_picture(&mut self) {
        if !self.decoding {
            return;
        }
        self.decoding = false;
        if self.skipping {
            return;
        }
        self.conceal();
        let p = self.cx.pic;
        if p.structure != Structure::Frame && !self.cx.second_field {
            self.first_field = Some((self.cx.cur, p.structure));
            return;
        }
        if p.coding_type == 3 {
            self.output(self.cx.cur);
        }
    }

    /// Macroblocks no slice decoded, from the latest reference frame (the same lines).
    fn conceal(&mut self) {
        let cur = self.cx.cur;
        let from = if self.cx.pic.coding_type == 3 { self.cx.bwd } else { self.cx.fwd };
        let Some(src) = from.filter(|&s| s != cur) else { return };
        let missing: Vec<usize> = self.bufs[cur].done.iter().enumerate().filter(|(_, d)| !**d).map(|(i, _)| i).collect();
        if missing.is_empty() {
            return;
        }
        let (start, step) = match self.cx.pic.structure.parity() {
            None => (0, 1),
            Some(par) => (par, 2),
        };
        let (s, mbw) = (self.cx.stride, self.cx.mb_width);
        let (a, b) = if src < cur { let (x, y) = self.bufs.split_at_mut(cur); (&x[src], &mut y[0]) } else { let (x, y) = self.bufs.split_at_mut(src); (&y[0], &mut x[cur]) };
        for m in missing {
            let (mbx, mby) = (m % mbw, m / mbw);
            for yy in 0..16 {
                let o = ((mby * 16 + yy) * step + start) * s + mbx * 16;
                b.y[o..o + 16].copy_from_slice(&a.y[o..o + 16]);
            }
            for k in 0..2 {
                for yy in 0..8 {
                    let o = ((mby * 8 + yy) * step + start) * (s / 2) + mbx * 8;
                    b.c[k][o..o + 8].copy_from_slice(&a.c[k][o..o + 8]);
                }
            }
        }
    }

    fn flush_references(&mut self) {
        if let Some(b) = self.cx.bwd {
            self.output(b);
        }
    }

    /// A frame to the output, once, with its time (or the last frame's plus its duration).
    fn output(&mut self, b: usize) {
        if !self.bufs[b].pending {
            return;
        }
        self.bufs[b].pending = false;
        let s = &self.cx.seq;
        let f = &self.bufs[b];
        // A frame lasts two fields, three with repeat_first_field (progressive sequences: one,
        // two or three frames).
        let fields = if s.progressive_sequence {
            2 * (1 + u64::from(f.repeat_first_field) * (1 + u64::from(f.top_field_first)))
        } else {
            2 + u64::from(f.repeat_first_field)
        };
        let duration = 90_000 * u64::from(s.rate_den) * fields / (2 * u64::from(s.rate_num));
        let pts = f.pts.or(self.last_out.map(|(t, d)| (t + d) & ((1 << 33) - 1)));
        if let Some(t) = pts {
            self.last_out = Some((t, duration));
        }
        self.out.push_back(Frame {
            width: s.width,
            height: s.height,
            stride: self.cx.stride,
            y: f.y.clone(),
            cb: f.c[0].clone(),
            cr: f.c[1].clone(),
            pts,
            kind: [' ', 'I', 'P', 'B'][usize::from(f.coding_type.min(3))],
            progressive: f.progressive_frame,
            top_field_first: f.top_field_first,
            repeat_first_field: f.repeat_first_field,
            matrix_coefficients: s.matrix_coefficients,
            aspect_ratio_information: s.aspect_ratio_information,
        });
    }
}

//! A slice's macroblocks (H.262 6.2.4-6.2.6, 7.1-7.6): their modes, motion vectors and coded
//! blocks; the coefficients' variable length decoding, inverse scan and quantisation, the IDCT;
//! the motion compensated prediction (frame, field, 16×8 and dual prime, half-sample filtered,
//! forward and backward averaged) and the reconstructed samples written into the picture.
//! 4:2:0 only, as Main Profile is.

use super::idct::idct;
use super::tables::{Coef, QUANTISER_SCALE};
use super::vlc::Reader;
use super::{Buf, Context, Structure};

/// Prediction kinds: frame_motion_type and field_motion_type (Tables 6-17, 6-18).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Motion {
    /// Frame-based (frame pictures) or field-based (field pictures): one vector a direction.
    Whole,
    /// Field-based in a frame picture: a vector for each field.
    FieldsOfFrame,
    /// 16×8 MC in a field picture: a vector for each half.
    Halves,
    DualPrime,
}

/// What persists from macroblock to macroblock within a slice.
struct State {
    qscale_code: u8,
    dc_pred: [i32; 3],
    /// PMV[r][s][t].
    pmv: [[[i32; 2]; 2]; 2],
    /// The last coded macroblock's directions, for skipped macroblocks in B-pictures.
    prev_fwd: bool,
    prev_bwd: bool,
}

/// A macroblock's motion: kind, which directions, vector'[r][s][t] and field selects[r][s].
struct Mc {
    kind: Motion,
    fwd: bool,
    bwd: bool,
    vec: [[[i32; 2]; 2]; 4],
    select: [[bool; 2]; 2],
}

pub struct Error;

/// The sample planes a prediction reads: a frame, or one field of it (every second line).
#[derive(Clone, Copy)]
struct Plane<'a> {
    data: &'a [u8],
    start: usize,
    stride: usize,
    width: i32,
    height: i32,
}

fn plane(data: &[u8], frame_stride: usize, frame_height: usize, field: Option<usize>) -> Plane<'_> {
    match field {
        None => Plane { data, start: 0, stride: frame_stride, width: frame_stride as i32, height: frame_height as i32 },
        Some(p) => Plane { data, start: p * frame_stride, stride: 2 * frame_stride, width: frame_stride as i32, height: (frame_height / 2) as i32 },
    }
}

/// One prediction block: w × h samples at (x, y) of `src` moved by the half-sample vector
/// (mx, my) (7.6.4), into `dst` (rows `dst_step` apart), averaged with what is there when `avg`.
#[allow(clippy::too_many_arguments)]
fn predict(src: Plane, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, dst: &mut [u8], dst_start: usize, dst_step: usize, avg: bool) {
    let (ix, iy) = (x + (mx >> 1), y + (my >> 1));
    let (hx, hy) = (mx & 1 != 0, my & 1 != 0);
    let inside = ix >= 0 && iy >= 0 && ix + w as i32 + i32::from(hx) <= src.width && iy + h as i32 + i32::from(hy) <= src.height;
    let at = |xx: i32, yy: i32| -> u32 {
        let (xx, yy) = if inside { (xx, yy) } else { (xx.clamp(0, src.width - 1), yy.clamp(0, src.height - 1)) };
        u32::from(src.data[src.start + yy as usize * src.stride + xx as usize])
    };
    for r in 0..h {
        let sy = iy + r as i32;
        let d = dst_start + r * dst_step;
        for c in 0..w {
            let sx = ix + c as i32;
            let v = match (hx, hy) {
                (false, false) => at(sx, sy),
                (false, true) => (at(sx, sy) + at(sx, sy + 1) + 1) >> 1,
                (true, false) => (at(sx, sy) + at(sx + 1, sy) + 1) >> 1,
                (true, true) => (at(sx, sy) + at(sx + 1, sy) + at(sx, sy + 1) + at(sx + 1, sy + 1) + 2) >> 2,
            } as u8;
            dst[d + c] = if avg { ((u32::from(dst[d + c]) + u32::from(v) + 1) >> 1) as u8 } else { v };
        }
    }
}

/// `//` for halving (7.6.3.6): rounding half away from zero.
fn half_rounded(x: i32) -> i32 {
    if x >= 0 {
        (x + 1) / 2
    } else {
        -((-x + 1) / 2)
    }
}

/// Decodes the slice whose payload (after the start code) is `data`, at macroblock row `row`.
pub fn decode(cx: &Context, bufs: &mut [Buf], data: &[u8], row: usize) -> Result<(), Error> {
    let p = &cx.pic;
    let mut r = Reader::new(data);
    if cx.seq.height > 2800 {
        r.skip(3);
    }
    let mut st = State { qscale_code: r.get(5) as u8, dc_pred: [0; 3], pmv: [[[0; 2]; 2]; 2], prev_fwd: false, prev_bwd: false };
    if st.qscale_code == 0 {
        return Err(Error);
    }
    if r.peek(1) == 1 {
        r.skip(1 + 1 + 1 + 6);
        while r.flag() {
            r.skip(8);
        }
    } else {
        r.skip(1);
    }
    let reset_dc = 1i32 << (7 + p.intra_dc_precision);
    st.dc_pred = [reset_dc; 3];
    let mb_width = cx.mb_width;
    let mbs = mb_width * cx.picture_mb_rows();
    let mut addr: isize = (row * mb_width) as isize - 1;
    let mut first = true;
    loop {
        // Macroblock address increment, escapes first.
        let mut inc = 0usize;
        // macroblock_escape, '0000 0001 000'.
        while r.peek(11) == 0b000_0000_1000 {
            r.skip(11);
            inc += 33;
        }
        let i = cx.t.mb_addr_inc.read(&mut r).ok_or(Error)?;
        if i == 0 {
            return Err(Error);
        }
        inc += usize::from(i);
        if !first && inc > 1 {
            // Skipped macroblocks.
            st.dc_pred = [reset_dc; 3];
            for k in 1..inc {
                let a = addr as usize + k;
                if a >= mbs {
                    return Err(Error);
                }
                skipped(cx, bufs, &mut st, a);
            }
        }
        addr += inc as isize;
        first = false;
        if addr < 0 || addr as usize >= mbs {
            return Err(Error);
        }
        macroblock(cx, bufs, &mut r, &mut st, addr as usize)?;
        if r.left() < 0 {
            return Err(Error);
        }
        // The slice ends where 23 zero bits begin (the next start code, or the data's end).
        if r.peek(23) == 0 {
            return Ok(());
        }
    }
}

/// A skipped macroblock (7.6.6): its prediction is its decoded value.
fn skipped(cx: &Context, bufs: &mut [Buf], st: &mut State, addr: usize) {
    let p = &cx.pic;
    let mut mc = Mc { kind: Motion::Whole, fwd: true, bwd: false, vec: [[[0; 2]; 2]; 4], select: [[false; 2]; 2] };
    let parity = p.structure.parity();
    if p.coding_type == 2 {
        st.pmv = [[[0; 2]; 2]; 2];
    } else {
        mc.fwd = st.prev_fwd;
        mc.bwd = st.prev_bwd;
        for s in 0..2 {
            mc.vec[0][s] = st.pmv[0][s];
        }
    }
    if let Some(par) = parity {
        mc.select = [[par == 1; 2]; 2];
    }
    let mut mb = Mb::default();
    form_prediction(cx, bufs, &mc, addr, &mut mb);
    write_mb(cx, &mut bufs[cx.cur], addr, &mb);
}

/// The samples of one macroblock: luma 16×16 and the two chroma 8×8, laid out as in the picture
/// being decoded (frame lines, or the field's lines).
struct Mb {
    y: [u8; 256],
    c: [[u8; 64]; 2],
}

impl Default for Mb {
    fn default() -> Mb {
        Mb { y: [0; 256], c: [[0; 64]; 2] }
    }
}

fn macroblock(cx: &Context, bufs: &mut [Buf], r: &mut Reader, st: &mut State, addr: usize) -> Result<(), Error> {
    let p = &cx.pic;
    let t = &cx.t;
    let frame = p.structure == Structure::Frame;
    let (quant, fwd, bwd, pattern, intra) = match p.coding_type {
        1 => t.mb_type_i.read(r),
        2 => t.mb_type_p.read(r),
        _ => t.mb_type_b.read(r),
    }
    .ok_or(Error)?;
    // Prediction kind (6.3.17.1).
    let mut kind = Motion::Whole;
    if fwd || bwd {
        if frame {
            if !p.frame_pred_frame_dct {
                kind = match r.get(2) {
                    1 => Motion::FieldsOfFrame,
                    2 => Motion::Whole,
                    3 => Motion::DualPrime,
                    _ => return Err(Error),
                };
            }
        } else {
            kind = match r.get(2) {
                1 => Motion::Whole,
                2 => Motion::Halves,
                3 => Motion::DualPrime,
                _ => return Err(Error),
            };
        }
    }
    let dct_field = frame && !p.frame_pred_frame_dct && (intra || pattern) && r.flag();
    if quant {
        st.qscale_code = r.get(5) as u8;
        if st.qscale_code == 0 {
            return Err(Error);
        }
    }
    let mut mc = Mc { kind, fwd, bwd, vec: [[[0; 2]; 2]; 4], select: [[false; 2]; 2] };
    let mut dmv = [0i32; 2];
    // Motion vectors (6.2.5.2, 7.6.3).
    let concealment = intra && p.concealment_mv;
    for s in 0..2 {
        let present = if s == 0 { fwd || concealment } else { bwd };
        if !present {
            continue;
        }
        // Field vectors: every kind but frame-based in a frame picture.
        let field_format = !(frame && kind == Motion::Whole);
        let count = if matches!(kind, Motion::FieldsOfFrame | Motion::Halves) { 2 } else { 1 };
        for rr in 0..count {
            if (count == 2 || field_format) && kind != Motion::DualPrime {
                mc.select[rr][s] = r.flag();
            }
            for tt in 0..2 {
                let v = motion_vector(cx, r, st, rr, s, tt, field_format && frame)?;
                mc.vec[rr][s][tt] = v;
                if kind == Motion::DualPrime {
                    dmv[tt] = i32::from(t.dmvector.read(r).ok_or(Error)?);
                }
            }
        }
        // Predictor updates (Tables 7-9, 7-10).
        if count == 1 {
            st.pmv[1][s] = st.pmv[0][s];
        }
    }
    if concealment {
        r.skip(1);
    }
    if intra && !p.concealment_mv {
        st.pmv = [[[0; 2]; 2]; 2];
    }
    if !intra && p.coding_type == 2 && !fwd {
        // P with no motion: forward, zero vector, predictors reset (7.6.3.5).
        st.pmv = [[[0; 2]; 2]; 2];
        mc.fwd = true;
        mc.kind = Motion::Whole;
        if let Some(par) = p.structure.parity() {
            mc.select = [[par == 1; 2]; 2];
        }
    }
    if kind == Motion::DualPrime {
        dual_prime(cx, &mut mc, dmv);
    }
    // Coded blocks.
    let cbp = if pattern {
        t.cbp.read(r).ok_or(Error)?
    } else if intra {
        63
    } else {
        0
    };
    let mut mb = Mb::default();
    if !intra {
        st.dc_pred = [1 << (7 + p.intra_dc_precision); 3];
        form_prediction(cx, bufs, &mc, addr, &mut mb);
        st.prev_fwd = mc.fwd;
        st.prev_bwd = mc.bwd;
    }
    let qscale = i32::from(QUANTISER_SCALE[usize::from(p.q_scale_type)][usize::from(st.qscale_code)]);
    for b in 0..6 {
        if cbp & (1 << (5 - b)) == 0 {
            continue;
        }
        let cc = if b < 4 { 0 } else { b - 3 };
        let mut f = [0i32; 64];
        coefficients(cx, r, st, intra, cc, qscale, &mut f)?;
        idct(&mut f);
        // Where the block's samples go in the macroblock (6.1.3).
        let (dst, start, step): (&mut [u8], usize, usize) = if b < 4 {
            let x0 = (b & 1) * 8;
            if dct_field {
                (&mut mb.y, (b >> 1) * 16 + x0, 32)
            } else {
                (&mut mb.y, (b >> 1) * 128 + x0, 16)
            }
        } else {
            (&mut mb.c[b - 4], 0, 8)
        };
        for yy in 0..8 {
            for xx in 0..8 {
                let i = start + yy * step + xx;
                let pred = if intra { 0 } else { i32::from(dst[i]) };
                dst[i] = (f[yy * 8 + xx] + pred).clamp(0, 255) as u8;
            }
        }
    }
    write_mb(cx, &mut bufs[cx.cur], addr, &mb);
    Ok(())
}

/// One motion vector component (7.6.3.1), its predictor updated. `halved`: the vertical part of
/// a field vector in a frame picture, whose predictor is kept in frame units.
fn motion_vector(cx: &Context, r: &mut Reader, st: &mut State, rr: usize, s: usize, t: usize, halved: bool) -> Result<i32, Error> {
    let code = i32::from(cx.t.motion_code.read(r).ok_or(Error)?);
    let f_code = u32::from(cx.pic.f_code[s][t]);
    if !(1..=9).contains(&f_code) {
        return Err(Error);
    }
    let r_size = f_code - 1;
    let f = 1i32 << r_size;
    let delta = if f == 1 || code == 0 {
        code
    } else {
        let residual = r.get(r_size) as i32;
        let d = (code.abs() - 1) * f + residual + 1;
        if code < 0 {
            -d
        } else {
            d
        }
    };
    let (high, low, range) = (16 * f - 1, -16 * f, 32 * f);
    let vertical_halved = halved && t == 1;
    let prediction = if vertical_halved { st.pmv[rr][s][t] >> 1 } else { st.pmv[rr][s][t] };
    let mut v = prediction + delta;
    if v < low {
        v += range;
    }
    if v > high {
        v -= range;
    }
    st.pmv[rr][s][t] = if vertical_halved { v * 2 } else { v };
    Ok(v)
}

/// Dual prime's derived vectors (7.6.3.6): vector'[2] (and [3] in frame pictures).
fn dual_prime(cx: &Context, mc: &mut Mc, dmv: [i32; 2]) {
    let p = &cx.pic;
    let v = mc.vec[0][0];
    let derive = |m: i32, e: i32| [half_rounded(v[0] * m) + dmv[0], half_rounded(v[1] * m) + e + dmv[1]];
    match p.structure.parity() {
        // Field picture: the opposite parity field, m 1, e -1 for a top field and +1 for a bottom.
        Some(0) => mc.vec[2][0] = derive(1, -1),
        Some(_) => mc.vec[2][0] = derive(1, 1),
        // Frame picture: the top field from the bottom (m[1][0], e -1), the bottom from the top
        // (m[0][1], e +1).
        None => {
            let (m10, m01) = if p.top_field_first { (1, 3) } else { (3, 1) };
            mc.vec[2][0] = derive(m10, -1);
            mc.vec[3][0] = derive(m01, 1);
        }
    }
}

/// The reference frame or field a prediction reads: `dir` 0 forward, 1 backward; `parity` the
/// field's in field predictions.
fn reference<'a>(cx: &Context, bufs: &'a [Buf], dir: usize, parity: Option<usize>, luma: bool, chroma: usize) -> Plane<'a> {
    let p = &cx.pic;
    let index = if dir == 1 {
        cx.bwd
    } else if p.coding_type == 2 && cx.second_field && parity.is_some() && parity != p.structure.parity() {
        // The second field of a P frame predicts the opposite parity from the frame's first.
        Some(cx.cur)
    } else {
        cx.fwd
    };
    let b = &bufs[index.unwrap_or(cx.cur)];
    let (data, stride, height) = if luma {
        (&b.y[..], cx.stride, cx.coded_height)
    } else {
        (&b.c[chroma][..], cx.stride / 2, cx.coded_height / 2)
    };
    plane(data, stride, height, parity)
}

/// The macroblock's prediction (7.6.4-7.6.7) into `mb`.
fn form_prediction(cx: &Context, bufs: &[Buf], mc: &Mc, addr: usize, mb: &mut Mb) {
    let p = &cx.pic;
    let (mbx, mby) = ((addr % cx.mb_width) as i32, (addr / cx.mb_width) as i32);
    let mut first = true;
    let chroma = |v: [i32; 2]| [v[0] / 2, v[1] / 2];
    for dir in 0..2 {
        if (dir == 0 && !mc.fwd) || (dir == 1 && !mc.bwd) {
            continue;
        }
        let avg = !first;
        first = false;
        match (p.structure.parity(), mc.kind) {
            (None, Motion::Whole) => {
                let v = mc.vec[0][dir];
                predict(reference(cx, bufs, dir, None, true, 0), mbx * 16, mby * 16, v[0], v[1], 16, 16, &mut mb.y, 0, 16, avg);
                let c = chroma(v);
                for k in 0..2 {
                    predict(reference(cx, bufs, dir, None, false, k), mbx * 8, mby * 8, c[0], c[1], 8, 8, &mut mb.c[k], 0, 8, avg);
                }
            }
            (None, Motion::FieldsOfFrame) => {
                for f in 0..2 {
                    let v = mc.vec[f][dir];
                    let field = Some(usize::from(mc.select[f][dir]));
                    predict(reference(cx, bufs, dir, field, true, 0), mbx * 16, mby * 8, v[0], v[1], 16, 8, &mut mb.y, f * 16, 32, avg);
                    let c = chroma(v);
                    for k in 0..2 {
                        predict(reference(cx, bufs, dir, field, false, k), mbx * 8, mby * 4, c[0], c[1], 8, 4, &mut mb.c[k], f * 8, 16, avg);
                    }
                }
            }
            (None, _) => {
                // Dual prime in a frame picture: each field from both reference fields, averaged.
                for f in 0..2 {
                    for (pass, (v, field)) in [(mc.vec[0][0], f), (mc.vec[2 + f][0], 1 - f)].into_iter().enumerate() {
                        let a = pass == 1;
                        predict(reference(cx, bufs, 0, Some(field), true, 0), mbx * 16, mby * 8, v[0], v[1], 16, 8, &mut mb.y, f * 16, 32, a);
                        let c = chroma(v);
                        for k in 0..2 {
                            predict(reference(cx, bufs, 0, Some(field), false, k), mbx * 8, mby * 4, c[0], c[1], 8, 4, &mut mb.c[k], f * 8, 16, a);
                        }
                    }
                }
            }
            (Some(par), Motion::Halves) => {
                let _ = par;
                for h in 0..2 {
                    let v = mc.vec[h][dir];
                    let field = Some(usize::from(mc.select[h][dir]));
                    predict(reference(cx, bufs, dir, field, true, 0), mbx * 16, mby * 16 + 8 * h as i32, v[0], v[1], 16, 8, &mut mb.y, h * 128, 16, avg);
                    let c = chroma(v);
                    for k in 0..2 {
                        predict(reference(cx, bufs, dir, field, false, k), mbx * 8, mby * 8 + 4 * h as i32, c[0], c[1], 8, 4, &mut mb.c[k], h * 32, 8, avg);
                    }
                }
            }
            (Some(par), Motion::DualPrime) => {
                for (pass, (v, field)) in [(mc.vec[0][0], par), (mc.vec[2][0], 1 - par)].into_iter().enumerate() {
                    let a = pass == 1;
                    predict(reference(cx, bufs, 0, Some(field), true, 0), mbx * 16, mby * 16, v[0], v[1], 16, 16, &mut mb.y, 0, 16, a);
                    let c = chroma(v);
                    for k in 0..2 {
                        predict(reference(cx, bufs, 0, Some(field), false, k), mbx * 8, mby * 8, c[0], c[1], 8, 8, &mut mb.c[k], 0, 8, a);
                    }
                }
            }
            (Some(_), _) => {
                let v = mc.vec[0][dir];
                let field = Some(usize::from(mc.select[0][dir]));
                predict(reference(cx, bufs, dir, field, true, 0), mbx * 16, mby * 16, v[0], v[1], 16, 16, &mut mb.y, 0, 16, avg);
                let c = chroma(v);
                for k in 0..2 {
                    predict(reference(cx, bufs, dir, field, false, k), mbx * 8, mby * 8, c[0], c[1], 8, 8, &mut mb.c[k], 0, 8, avg);
                }
            }
        }
    }
}

/// A block's coefficients (7.2-7.4): variable length decoded, inverse scanned and quantised,
/// saturated and mismatch controlled, into `f` (F[v * 8 + u]).
fn coefficients(cx: &Context, r: &mut Reader, st: &mut State, intra: bool, cc: usize, qscale: i32, f: &mut [i32; 64]) -> Result<(), Error> {
    let p = &cx.pic;
    let t = &cx.t;
    let scan = &cx.t.inverse_scan[usize::from(p.alternate_scan)];
    let w = if intra { &cx.intra_q } else { &cx.non_intra_q };
    let mut sum = 0i32;
    let mut n = 0usize;
    if intra {
        let size = if cc == 0 { t.dc_size_luma.read(r) } else { t.dc_size_chroma.read(r) }.ok_or(Error)?;
        let diff = if size == 0 {
            0
        } else {
            let half = 1i32 << (size - 1);
            let d = r.get(u32::from(size)) as i32;
            if d >= half {
                d
            } else {
                d + 1 - 2 * half
            }
        };
        let qfs0 = st.dc_pred[cc] + diff;
        st.dc_pred[cc] = qfs0;
        let dc = (qfs0 * (8 >> p.intra_dc_precision)).clamp(-2048, 2047);
        f[0] = dc;
        sum += dc;
        n = 1;
    }
    let table = if intra && p.intra_vlc_format { &t.dct_one } else { &t.dct_zero };
    let mut first = !intra;
    loop {
        let (run, level) = if first && r.peek(1) == 1 {
            // A non-intra block's first coefficient: '1s' is run 0, level 1 (Table B.14, Note 3).
            r.skip(1);
            first = false;
            let s = r.get(1);
            (0usize, if s == 1 { -1 } else { 1 })
        } else {
            first = false;
            match table.read(r).ok_or(Error)? {
                Coef::Eob => break,
                Coef::Escape => {
                    let run = r.get(6) as usize;
                    let l = r.get(12) as i32;
                    let l = if l >= 2048 { l - 4096 } else { l };
                    if l == 0 || l == -2048 {
                        return Err(Error);
                    }
                    (run, l)
                }
                Coef::Run(run, level) => {
                    let s = r.get(1);
                    let l = i32::from(level);
                    (usize::from(run), if s == 1 { -l } else { l })
                }
            }
        };
        n += run;
        if n >= 64 {
            return Err(Error);
        }
        let pos = usize::from(scan[n]);
        let k = if intra { 0 } else { level.signum() };
        let v = (((2 * level + k) * i32::from(w[pos]) * qscale) / 32).clamp(-2048, 2047);
        f[pos] = v;
        sum += v;
        n += 1;
    }
    if sum & 1 == 0 {
        f[63] ^= 1;
    }
    Ok(())
}

/// The macroblock's samples into the picture: frame lines, or the field's lines.
fn write_mb(cx: &Context, b: &mut Buf, addr: usize, mb: &Mb) {
    let (mbx, mby) = (addr % cx.mb_width, addr / cx.mb_width);
    let (start, step) = match cx.pic.structure.parity() {
        None => (0, 1),
        Some(par) => (par, 2),
    };
    let s = cx.stride;
    for yy in 0..16 {
        let line = (mby * 16 + yy) * step + start;
        let o = line * s + mbx * 16;
        b.y[o..o + 16].copy_from_slice(&mb.y[yy * 16..yy * 16 + 16]);
    }
    let cs = s / 2;
    for k in 0..2 {
        for yy in 0..8 {
            let line = (mby * 8 + yy) * step + start;
            let o = line * cs + mbx * 8;
            b.c[k][o..o + 8].copy_from_slice(&mb.c[k][yy * 8..yy * 8 + 8]);
        }
    }
    b.done[addr] = true;
}

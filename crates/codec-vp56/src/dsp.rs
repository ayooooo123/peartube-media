// Ported from FFmpeg (commit 2da55bf): libavcodec/vp3dsp.c (idct, idct10,
// vp3_idct_put_c/add_c/dc_add_c, ff_vp3dsp_idct10_put/add,
// vp3_v/h_loop_filter_c with count 12, put_no_rnd_pixels_l2,
// ff_vp3dsp_set_bounding_values), rnd_avg.h (no_rnd_avg32), vp5dsp.c
// (vp5_adjust, the VP56 edge filters), vp6dsp.c (vp6_filter_diag4_c),
// vp6.c (vp6_block_variance, vp6_filter_hv4), h264chroma_template.c
// (put_h264_chroma_mc8) and hpeldsp.c (put_pixels8/16).
// License: LGPL-2.1-or-later

//! The pixel operations of VP5/VP6. Each works on a plane (`&mut [u8]` or
//! `&[u8]`) at an offset with a row stride, like FFmpeg's pointer and
//! stride. Reads outside a slice see 0 and writes outside it are dropped,
//! so damaged streams cannot index out of bounds.

const X_C1S7: i32 = 64277;
const X_C2S6: i32 = 60547;
const X_C3S5: i32 = 54491;
const X_C4S4: i32 = 46341;
const X_C5S3: i32 = 36410;
const X_C6S2: i32 = 25080;
const X_C7S1: i32 = 12785;
/// IdctAdjustBeforeShift.
const IDCT_ADJUST_BEFORE_SHIFT: i32 = 8;

/// M(a, b): (int)((unsigned)a * b) >> 16.
fn m(a: i32, b: i32) -> i32 {
    ((a as u32).wrapping_mul(b as u32) as i32) >> 16
}

fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

pub(crate) fn get(src: &[u8], i: isize) -> i32 {
    usize::try_from(i).ok().and_then(|i| src.get(i)).map_or(0, |&v| i32::from(v))
}

pub(crate) fn set(dst: &mut [u8], i: isize, v: u8) {
    if let Some(p) = usize::try_from(i).ok().and_then(|i| dst.get_mut(i)) {
        *p = v;
    }
}

/// The add (`put == false`) or put of FFmpeg's VP3 IDCT pass 2 outputs.
fn store(dst: &mut [u8], at: isize, put: bool, v: i32) {
    let v = if put { clip_u8(v) } else { clip_u8(get(dst, at) + v) };
    set(dst, at, v);
}

/// vp3dsp.c idct: type 1 puts, type 2 adds; the block is zeroed after.
fn idct(dst: &mut [u8], off: isize, stride: isize, input: &mut [i16; 64], put: bool) {
    // Pass 1: each column of the stored (transposed) coefficients.
    for i in 0..8 {
        let ip = |k: usize| i32::from(input[k * 8 + i]);
        if (0..8).any(|k| ip(k) != 0) {
            let a = m(X_C1S7, ip(1)).wrapping_add(m(X_C7S1, ip(7)));
            let b = m(X_C7S1, ip(1)).wrapping_sub(m(X_C1S7, ip(7)));
            let c = m(X_C3S5, ip(3)).wrapping_add(m(X_C5S3, ip(5)));
            let d = m(X_C3S5, ip(5)).wrapping_sub(m(X_C5S3, ip(3)));
            let ad = m(X_C4S4, a.wrapping_sub(c));
            let bd = m(X_C4S4, b.wrapping_sub(d));
            let cd = a.wrapping_add(c);
            let dd = b.wrapping_add(d);
            let e = m(X_C4S4, ip(0).wrapping_add(ip(4)));
            let f = m(X_C4S4, ip(0).wrapping_sub(ip(4)));
            let g = m(X_C2S6, ip(2)).wrapping_add(m(X_C6S2, ip(6)));
            let h = m(X_C6S2, ip(2)).wrapping_sub(m(X_C2S6, ip(6)));
            let (ed, gd) = (e.wrapping_sub(g), e.wrapping_add(g));
            let (add, bdd) = (f.wrapping_add(ad), bd.wrapping_sub(h));
            let (fd, hd) = (f.wrapping_sub(ad), bd.wrapping_add(h));
            let out = [
                gd.wrapping_add(cd),
                add.wrapping_add(hd),
                add.wrapping_sub(hd),
                ed.wrapping_add(dd),
                ed.wrapping_sub(dd),
                fd.wrapping_add(bdd),
                fd.wrapping_sub(bdd),
                gd.wrapping_sub(cd),
            ];
            for (k, v) in out.into_iter().enumerate() {
                input[k * 8 + i] = v as i16;
            }
        }
    }
    // Pass 2: each row, written down a column of the destination.
    for i in 0..8 {
        let ip = |k: usize| i32::from(input[i * 8 + k]);
        let col = off + i as isize;
        if (1..8).any(|k| ip(k) != 0) {
            let a = m(X_C1S7, ip(1)).wrapping_add(m(X_C7S1, ip(7)));
            let b = m(X_C7S1, ip(1)).wrapping_sub(m(X_C1S7, ip(7)));
            let c = m(X_C3S5, ip(3)).wrapping_add(m(X_C5S3, ip(5)));
            let d = m(X_C3S5, ip(5)).wrapping_sub(m(X_C5S3, ip(3)));
            let ad = m(X_C4S4, a.wrapping_sub(c));
            let bd = m(X_C4S4, b.wrapping_sub(d));
            let cd = a.wrapping_add(c);
            let dd = b.wrapping_add(d);
            let mut e = m(X_C4S4, ip(0).wrapping_add(ip(4))).wrapping_add(8);
            let mut f = m(X_C4S4, ip(0).wrapping_sub(ip(4))).wrapping_add(8);
            if put {
                e = e.wrapping_add(16 * 128);
                f = f.wrapping_add(16 * 128);
            }
            let g = m(X_C2S6, ip(2)).wrapping_add(m(X_C6S2, ip(6)));
            let h = m(X_C6S2, ip(2)).wrapping_sub(m(X_C2S6, ip(6)));
            let (ed, gd) = (e.wrapping_sub(g), e.wrapping_add(g));
            let (add, bdd) = (f.wrapping_add(ad), bd.wrapping_sub(h));
            let (fd, hd) = (f.wrapping_sub(ad), bd.wrapping_add(h));
            let out = [
                gd.wrapping_add(cd),
                add.wrapping_add(hd),
                add.wrapping_sub(hd),
                ed.wrapping_add(dd),
                ed.wrapping_sub(dd),
                fd.wrapping_add(bdd),
                fd.wrapping_sub(bdd),
                gd.wrapping_sub(cd),
            ];
            for (k, v) in out.into_iter().enumerate() {
                store(dst, col + k as isize * stride, put, v >> 4);
            }
        } else if put {
            let v = clip_u8(128 + (X_C4S4.wrapping_mul(ip(0)).wrapping_add(IDCT_ADJUST_BEFORE_SHIFT << 16) >> 20));
            for k in 0..8 {
                set(dst, col + k * stride, v);
            }
        } else if ip(0) != 0 {
            let v = X_C4S4.wrapping_mul(ip(0)).wrapping_add(IDCT_ADJUST_BEFORE_SHIFT << 16) >> 20;
            for k in 0..8 {
                store(dst, col + k * stride, false, v);
            }
        }
    }
    *input = [0; 64];
}

/// vp3dsp.c idct10: the IDCT of a block whose coefficients past the tenth
/// in zigzag order are zero; the block is zeroed after.
fn idct10(dst: &mut [u8], off: isize, stride: isize, input: &mut [i16; 64], put: bool) {
    for i in 0..4 {
        let ip = |k: usize| i32::from(input[k * 8 + i]);
        if ip(0) != 0 || ip(1) != 0 || ip(2) != 0 || ip(3) != 0 {
            let a = m(X_C1S7, ip(1));
            let b = m(X_C7S1, ip(1));
            let c = m(X_C3S5, ip(3));
            let d = m(X_C5S3, ip(3)).wrapping_neg();
            let ad = m(X_C4S4, a.wrapping_sub(c));
            let bd = m(X_C4S4, b.wrapping_sub(d));
            let cd = a.wrapping_add(c);
            let dd = b.wrapping_add(d);
            let e = m(X_C4S4, ip(0));
            let f = e;
            let g = m(X_C2S6, ip(2));
            let h = m(X_C6S2, ip(2));
            let (ed, gd) = (e.wrapping_sub(g), e.wrapping_add(g));
            let (add, bdd) = (f.wrapping_add(ad), bd.wrapping_sub(h));
            let (fd, hd) = (f.wrapping_sub(ad), bd.wrapping_add(h));
            let out = [
                gd.wrapping_add(cd),
                add.wrapping_add(hd),
                add.wrapping_sub(hd),
                ed.wrapping_add(dd),
                ed.wrapping_sub(dd),
                fd.wrapping_add(bdd),
                fd.wrapping_sub(bdd),
                gd.wrapping_sub(cd),
            ];
            for (k, v) in out.into_iter().enumerate() {
                input[k * 8 + i] = v as i16;
            }
        }
    }
    for i in 0..8 {
        let ip = |k: usize| i32::from(input[i * 8 + k]);
        let col = off + i as isize;
        if ip(0) != 0 || ip(1) != 0 || ip(2) != 0 || ip(3) != 0 {
            let a = m(X_C1S7, ip(1));
            let b = m(X_C7S1, ip(1));
            let c = m(X_C3S5, ip(3));
            let d = m(X_C5S3, ip(3)).wrapping_neg();
            let ad = m(X_C4S4, a.wrapping_sub(c));
            let bd = m(X_C4S4, b.wrapping_sub(d));
            let cd = a.wrapping_add(c);
            let dd = b.wrapping_add(d);
            let mut e = m(X_C4S4, ip(0));
            if put {
                e = e.wrapping_add(16 * 128);
            }
            let f = e;
            let g = m(X_C2S6, ip(2));
            let h = m(X_C6S2, ip(2));
            let (ed, gd) = (e.wrapping_sub(g).wrapping_add(8), e.wrapping_add(g).wrapping_add(8));
            let (add, bdd) = (f.wrapping_add(ad).wrapping_add(8), bd.wrapping_sub(h));
            let (fd, hd) = (f.wrapping_sub(ad).wrapping_add(8), bd.wrapping_add(h));
            let out = [
                gd.wrapping_add(cd),
                add.wrapping_add(hd),
                add.wrapping_sub(hd),
                ed.wrapping_add(dd),
                ed.wrapping_sub(dd),
                fd.wrapping_add(bdd),
                fd.wrapping_sub(bdd),
                gd.wrapping_sub(cd),
            ];
            for (k, v) in out.into_iter().enumerate() {
                store(dst, col + k as isize * stride, put, v >> 4);
            }
        } else if put {
            for k in 0..8 {
                set(dst, col + k * stride, 128);
            }
        }
    }
    *input = [0; 64];
}

/// vp56_idct_put: the full IDCT unless only the first 10 coefficients can
/// be set (selector 2..=10).
pub(crate) fn idct_put(dst: &mut [u8], off: isize, stride: isize, block: &mut [i16; 64], selector: i32) {
    if selector > 10 || selector == 1 {
        idct(dst, off, stride, block, true);
    } else {
        idct10(dst, off, stride, block, true);
    }
}

/// vp56_idct_add: full, 10-coefficient or DC-only.
pub(crate) fn idct_add(dst: &mut [u8], off: isize, stride: isize, block: &mut [i16; 64], selector: i32) {
    if selector > 10 {
        idct(dst, off, stride, block, false);
    } else if selector > 1 {
        idct10(dst, off, stride, block, false);
    } else {
        // vp3_idct_dc_add_c
        let dc = (i32::from(block[0]) + 15) >> 5;
        for r in 0..8 {
            for c in 0..8 {
                let at = off + r * stride + c;
                store(dst, at, false, dc);
            }
        }
        block[0] = 0;
    }
}

/// ff_vp3dsp_set_bounding_values: the loop filter's response, indexed by
/// value + 127.
pub(crate) fn bounding_values(filter_limit: i32) -> [i32; 256] {
    let mut bv = [0i32; 256];
    let at = |x: i32| (127 + x) as usize;
    for x in 0..filter_limit {
        bv[at(-x)] = -x;
        bv[at(x)] = x;
    }
    let (mut x, mut value) = (filter_limit, filter_limit);
    while x < 128 && value != 0 {
        bv[at(x)] = value;
        bv[at(-x)] = -value;
        x += 1;
        value -= 1;
    }
    if value != 0 {
        bv[at(128)] = value;
    }
    bv
}

/// vp3_h_loop_filter_c / vp3_v_loop_filter_c over 12 pixels: `pix` steps
/// across the edge, `line` along it.
pub(crate) fn vp3_loop_filter_12(buf: &mut [u8], first: isize, pix: isize, line: isize, bounding: &[i32; 256]) {
    for k in 0..12 {
        let p = first + k * line;
        let fv = (get(buf, p - 2 * pix) - get(buf, p + pix)) + (get(buf, p) - get(buf, p - pix)) * 3;
        let fv = bounding[(127 + ((fv + 4) >> 3)).clamp(0, 255) as usize];
        let (a, b) = (get(buf, p - pix), get(buf, p));
        set(buf, p - pix, clip_u8(a + fv));
        set(buf, p, clip_u8(b - fv));
    }
}

/// vp5_adjust.
fn vp5_adjust(v: i32, t: i32) -> i32 {
    let s1 = v >> 31;
    let mut v = (v ^ s1) - s1;
    v *= i32::from(v < 2 * t);
    v -= t;
    let s2 = v >> 31;
    v = (v ^ s2) - s2;
    v = t - v;
    v += s1;
    v ^ s1
}

/// vp5_edge_filter_hor / _ver: `pix` across the edge, `line` along it.
pub(crate) fn vp5_edge_filter(buf: &mut [u8], at: isize, pix: isize, line: isize, t: i32) {
    for k in 0..12 {
        let p = at + k * line;
        let v = (get(buf, p - 2 * pix) + 3 * (get(buf, p) - get(buf, p - pix)) - get(buf, p + pix) + 4) >> 3;
        let v = vp5_adjust(v, t);
        let (a, b) = (get(buf, p - pix), get(buf, p));
        set(buf, p - pix, clip_u8(a + v));
        set(buf, p, clip_u8(b - v));
    }
}

/// put_pixels8 / put_pixels16 (hpeldsp): `w` x `h` bytes copied.
pub(crate) fn copy(dst: &mut [u8], d: isize, dst_stride: isize, src: &[u8], s: isize, src_stride: isize, w: isize, h: isize) {
    for r in 0..h {
        for c in 0..w {
            set(dst, d + r * dst_stride + c, get(src, s + r * src_stride + c) as u8);
        }
    }
}

/// put_no_rnd_pixels_l2 (vp3dsp, no_rnd_avg32 per byte): 8 x 8.
pub(crate) fn put_no_rnd_pixels_l2(dst: &mut [u8], d: isize, src: &[u8], s1: isize, s2: isize, stride: isize) {
    for r in 0..8 {
        for c in 0..8 {
            let (a, b) = (get(src, s1 + r * stride + c), get(src, s2 + r * stride + c));
            set(dst, d + r * stride + c, ((a & b) + ((a ^ b) >> 1)) as u8);
        }
    }
}

/// put_h264_chroma_mc8 (8-bit C): an 8-wide bilinear of eighth-pel `x`,
/// `y`, `h` rows; `src` and `dst` share `stride`.
pub(crate) fn h264_chroma_mc8(dst: &mut [u8], d: isize, src: &[u8], s: isize, stride: isize, h: isize, x: i32, y: i32) {
    h264_chroma_mc8_strides(dst, d, stride, src, s, stride, h, x, y);
}

/// vp6_block_variance: of every other pixel of every other row of 8 x 8.
pub(crate) fn vp6_block_variance(src: &[u8], s: isize, stride: isize) -> i32 {
    let (mut sum, mut square_sum) = (0, 0);
    for r in (0..8).step_by(2) {
        for c in (0..8).step_by(2) {
            let v = get(src, s + r * stride + c);
            sum += v;
            square_sum += v * v;
        }
    }
    (16 * square_sum - sum * sum) >> 8
}

/// vp6_filter_hv4: a 4-tap filter along `delta` (1 across, `stride`
/// down) for 8 x 8.
pub(crate) fn vp6_filter_hv4(dst: &mut [u8], d: isize, src: &[u8], s: isize, stride: isize, delta: isize, w: &[i16; 4]) {
    let w = w.map(i32::from);
    for r in 0..8 {
        for c in 0..8 {
            let p = s + r * stride + c;
            let v = get(src, p - delta) * w[0] + get(src, p) * w[1] + get(src, p + delta) * w[2] + get(src, p + 2 * delta) * w[3] + 64;
            set(dst, d + r * stride + c, clip_u8(v >> 7));
        }
    }
}

/// vp6_filter_diag4_c: 4-tap horizontal then vertical for 8 x 8.
pub(crate) fn vp6_filter_diag4(dst: &mut [u8], d: isize, src: &[u8], s: isize, stride: isize, hw: &[i16; 4], vw: &[i16; 4]) {
    let (hw, vw) = (hw.map(i32::from), vw.map(i32::from));
    let mut tmp = [0i32; 8 * 11];
    let mut src_row = s - stride;
    for t in tmp.chunks_exact_mut(8) {
        for (x, v) in t.iter_mut().enumerate() {
            let p = src_row + x as isize;
            *v = i32::from(clip_u8(
                (get(src, p - 1) * hw[0] + get(src, p) * hw[1] + get(src, p + 1) * hw[2] + get(src, p + 2) * hw[3] + 64) >> 7,
            ));
        }
        src_row += stride;
    }
    for y in 0..8 {
        for x in 0..8 {
            let t = |k: usize| tmp[(y + k) * 8 + x];
            let v = t(0) * vw[0] + t(1) * vw[1] + t(2) * vw[2] + t(3) * vw[3] + 64;
            set(dst, d + y as isize * stride + x as isize, clip_u8(v >> 7));
        }
    }
}

/// vp6_filter_diag2: put_h264_chroma_mc8 across 9 rows (horizontal weight
/// only), then down 8 rows (vertical only), through a 9 x 8 scratch.
pub(crate) fn vp6_filter_diag2(dst: &mut [u8], d: isize, src: &[u8], s: isize, stride: isize, x: i32, y: i32) {
    let mut tmp = [0u8; 9 * 8];
    h264_chroma_mc8_strides(&mut tmp, 0, 8, src, s, stride, 9, x, 0);
    h264_chroma_mc8_strides(dst, d, stride, &tmp, 0, 8, 8, 0, y);
}

/// put_h264_chroma_mc8 with separate destination and source strides.
#[allow(clippy::too_many_arguments)]
fn h264_chroma_mc8_strides(dst: &mut [u8], d: isize, dst_stride: isize, src: &[u8], s: isize, src_stride: isize, h: isize, x: i32, y: i32) {
    let a = (8 - x) * (8 - y);
    let b = x * (8 - y);
    let c = (8 - x) * y;
    let dd = x * y;
    for r in 0..h {
        let (row, out) = (s + r * src_stride, d + r * dst_stride);
        for k in 0..8 {
            let p = |i: isize| get(src, row + i);
            let v = if dd != 0 {
                a * p(k) + b * p(k + 1) + c * p(src_stride + k) + dd * p(src_stride + k + 1)
            } else if b + c != 0 {
                let step = if c != 0 { src_stride } else { 1 };
                a * p(k) + (b + c) * p(step + k)
            } else {
                a * p(k)
            };
            set(dst, out + k, ((v + 32) >> 6) as u8);
        }
    }
}

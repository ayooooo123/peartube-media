// Ported from FFmpeg libavcodec/vp3dsp.c and libavcodec/rnd_avg.h
// (no_rnd_avg32, behind hpeldsp.c's put_no_rnd_pixels8_x2/_y2) at commit
// 2da55bf.
// Licensed under GNU Lesser General Public License 2.1 or later.

//! VP3 DSP: the IDCT, the loop filters, the bounding-value table and the
//! no-rounding average.
//!
//! These functions take the memory index of every row they touch instead
//! of a stride, so the caller states the row layout once and the
//! arithmetic here stays FFmpeg's.

const IDCT_ADJUST_BEFORE_SHIFT: i32 = 8;
const XC1S7: i32 = 64277;
const XC2S6: i32 = 60547;
const XC3S5: i32 = 54491;
const XC4S4: i32 = 46341;
const XC5S3: i32 = 36410;
const XC6S2: i32 = 25080;
const XC7S1: i32 = 12785;

/// FFmpeg's `M(a, b)`: `(int)((SUINT)(a) * (b)) >> 16`.
#[inline(always)]
fn m(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b) >> 16
}

#[inline(always)]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Bounding values for the loop filters: FFmpeg's `bounding_values_array`
/// (indexed from its centre at 127).
pub type BoundingValues = [i32; 260];

/// FFmpeg's `idct(dst, stride, input, type)`: `type_` 1 writes (put), 2
/// adds. `rows[k]` is the memory index of output row `k`, column 0.
fn idct(dst: &mut [u8], rows: &[usize; 8], input: &mut [i16; 64], type_: i32) {
    // Rows.
    for i in 0..8 {
        let ip = |k: usize| i32::from(input[i + 8 * k]);
        if (input[i]
            | input[i + 8]
            | input[i + 16]
            | input[i + 24]
            | input[i + 32]
            | input[i + 40]
            | input[i + 48]
            | input[i + 56])
            != 0
        {
            let a = m(XC1S7, ip(1)) + m(XC7S1, ip(7));
            let b = m(XC7S1, ip(1)) - m(XC1S7, ip(7));
            let c = m(XC3S5, ip(3)) + m(XC5S3, ip(5));
            let d = m(XC3S5, ip(5)) - m(XC5S3, ip(3));
            let ad = m(XC4S4, a - c);
            let bd = m(XC4S4, b - d);
            let cd = a + c;
            let dd = b + d;
            let e = m(XC4S4, ip(0) + ip(4));
            let f = m(XC4S4, ip(0) - ip(4));
            let g = m(XC2S6, ip(2)) + m(XC6S2, ip(6));
            let h = m(XC6S2, ip(2)) - m(XC2S6, ip(6));
            let ed = e - g;
            let gd = e + g;
            let add = f + ad;
            let bdd = bd - h;
            let fd = f - ad;
            let hd = bd + h;
            input[i] = (gd + cd) as i16;
            input[i + 56] = (gd - cd) as i16;
            input[i + 8] = (add + hd) as i16;
            input[i + 16] = (add - hd) as i16;
            input[i + 24] = (ed + dd) as i16;
            input[i + 32] = (ed - dd) as i16;
            input[i + 40] = (fd + bdd) as i16;
            input[i + 48] = (fd - bdd) as i16;
        }
    }

    // Columns: iteration `i` reads input[8i..8i+8] and writes output
    // column `i`.
    for i in 0..8 {
        let ip = |k: usize| i32::from(input[8 * i + k]);
        let at = |k: usize| rows[k] + i;
        if (input[8 * i + 1]
            | input[8 * i + 2]
            | input[8 * i + 3]
            | input[8 * i + 4]
            | input[8 * i + 5]
            | input[8 * i + 6]
            | input[8 * i + 7])
            != 0
        {
            let a = m(XC1S7, ip(1)) + m(XC7S1, ip(7));
            let b = m(XC7S1, ip(1)) - m(XC1S7, ip(7));
            let c = m(XC3S5, ip(3)) + m(XC5S3, ip(5));
            let d = m(XC3S5, ip(5)) - m(XC5S3, ip(3));
            let ad = m(XC4S4, a - c);
            let bd = m(XC4S4, b - d);
            let cd = a + c;
            let dd = b + d;
            let mut e = m(XC4S4, ip(0) + ip(4)) + 8;
            let mut f = m(XC4S4, ip(0) - ip(4)) + 8;
            if type_ == 1 {
                e += 16 * 128;
                f += 16 * 128;
            }
            let g = m(XC2S6, ip(2)) + m(XC6S2, ip(6));
            let h = m(XC6S2, ip(2)) - m(XC2S6, ip(6));
            let ed = e - g;
            let gd = e + g;
            let add = f + ad;
            let bdd = bd - h;
            let fd = f - ad;
            let hd = bd + h;
            let out = [
                gd + cd,
                add + hd,
                add - hd,
                ed + dd,
                ed - dd,
                fd + bdd,
                fd - bdd,
                gd - cd,
            ];
            for (k, v) in out.into_iter().enumerate() {
                let p = &mut dst[at(k)];
                *p = if type_ == 1 {
                    clip_u8(v >> 4)
                } else {
                    clip_u8(i32::from(*p) + (v >> 4))
                };
            }
        } else if type_ == 1 {
            let v = clip_u8(128 + ((XC4S4 * ip(0) + (IDCT_ADJUST_BEFORE_SHIFT << 16)) >> 20));
            for k in 0..8 {
                dst[at(k)] = v;
            }
        } else if ip(0) != 0 {
            let v = (XC4S4 * ip(0) + (IDCT_ADJUST_BEFORE_SHIFT << 16)) >> 20;
            for k in 0..8 {
                let p = &mut dst[at(k)];
                *p = clip_u8(i32::from(*p) + v);
            }
        }
    }
}

/// FFmpeg's `vp3_idct_put_c`; clears `block`.
pub fn idct_put(dst: &mut [u8], rows: &[usize; 8], block: &mut [i16; 64]) {
    idct(dst, rows, block, 1);
    *block = [0; 64];
}

/// FFmpeg's `vp3_idct_add_c`; clears `block`.
pub fn idct_add(dst: &mut [u8], rows: &[usize; 8], block: &mut [i16; 64]) {
    idct(dst, rows, block, 2);
    *block = [0; 64];
}

/// FFmpeg's `vp3_idct_dc_add_c`; clears `block[0]`.
pub fn idct_dc_add(dst: &mut [u8], rows: &[usize; 8], block: &mut [i16; 64]) {
    let dc = (i32::from(block[0]) + 15) >> 5;
    for &row in rows {
        for p in &mut dst[row..row + 8] {
            *p = clip_u8(i32::from(*p) + dc);
        }
    }
    block[0] = 0;
}

/// FFmpeg's `vp3_v_loop_filter_c` (an edge between two rows) over `count`
/// columns. `rows` are the memory indices of the first column in the rows
/// two before, one before, at and one after the edge, in VP3 order
/// (FFmpeg's `first_pixel[2 * nstride]`, `[nstride]`, `[0]`, `[stride]`).
pub fn v_loop_filter(dst: &mut [u8], rows: [usize; 4], count: usize, bv: &BoundingValues) {
    let [p2, p1, p0, n1] = rows;
    for x in 0..count {
        let fv = (i32::from(dst[p2 + x]) - i32::from(dst[n1 + x]))
            + (i32::from(dst[p0 + x]) - i32::from(dst[p1 + x])) * 3;
        let fv = bv[(127 + ((fv + 4) >> 3)) as usize];
        dst[p1 + x] = clip_u8(i32::from(dst[p1 + x]) + fv);
        dst[p0 + x] = clip_u8(i32::from(dst[p0 + x]) - fv);
    }
}

/// FFmpeg's `vp3_h_loop_filter_c` (an edge between two columns): `rows[k]`
/// is the memory index of the pixel just right of the edge in row `k`.
pub fn h_loop_filter(dst: &mut [u8], rows: &[usize], bv: &BoundingValues) {
    for &p in rows {
        let fv = (i32::from(dst[p - 2]) - i32::from(dst[p + 1]))
            + (i32::from(dst[p]) - i32::from(dst[p - 1])) * 3;
        let fv = bv[(127 + ((fv + 4) >> 3)) as usize];
        dst[p - 1] = clip_u8(i32::from(dst[p - 1]) + fv);
        dst[p] = clip_u8(i32::from(dst[p]) - fv);
    }
}

/// FFmpeg's `ff_vp3dsp_set_bounding_values` (non-x86 branch);
/// `filter_limit < 128`.
pub fn set_bounding_values(bv: &mut BoundingValues, filter_limit: u8) {
    let filter_limit = i32::from(filter_limit.min(127));
    *bv = [0; 260];
    let at = |x: i32| (127 + x) as usize;
    for x in 0..filter_limit {
        bv[at(-x)] = -x;
        bv[at(x)] = x;
    }
    let mut x = filter_limit;
    let mut value = filter_limit;
    while x < 128 && value != 0 {
        bv[at(x)] = value;
        bv[at(-x)] = -value;
        x += 1;
        value -= 1;
    }
    if value != 0 {
        bv[at(128)] = value;
    }
    // [129] and [130] hold SIMD constants in FFmpeg; the C filters never
    // read them.
}

/// FFmpeg's `no_rnd_avg32` on one byte: `(a + b) >> 1`.
#[inline(always)]
pub fn no_rnd_avg(a: u8, b: u8) -> u8 {
    ((u16::from(a) + u16::from(b)) >> 1) as u8
}

//! RV30/RV40 DSP: 4x4 inverse transforms, luma third-/quarter-pel and
//! chroma motion compensation, RV40 B-frame weighting and the RV40
//! deblocking filter kernels.
//!
//! Ported from FFmpeg libavcodec/rv34dsp.c, rv30dsp.c, rv40dsp.c,
//! h264qpel_template.c (full-pel copy/average, 6-tap half-pel),
//! h264chroma_template.c and hpel_template.c (`pixels_xy2`) at commit
//! 2da55bf; LGPL-2.1-or-later.
//!
//! Motion compensation reads its source from a block fetched with the
//! reference picture's edge clamping (FFmpeg's `emulated_edge_mc` result,
//! which equals a direct read whenever the block lies inside the picture).

#[inline]
fn cm(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `rv34_row_transform`.
#[inline]
fn row_transform(temp: &mut [i32; 16], block: &[i16; 16]) {
    for i in 0..4 {
        let b0 = block[i] as i32;
        let b1 = block[i + 4] as i32;
        let b2 = block[i + 8] as i32;
        let b3 = block[i + 12] as i32;
        let z0 = 13 * (b0 + b2);
        let z1 = 13 * (b0 - b2);
        let z2 = 7 * b1 - 17 * b3;
        let z3 = 17 * b1 + 7 * b3;
        temp[4 * i] = z0 + z3;
        temp[4 * i + 1] = z1 + z2;
        temp[4 * i + 2] = z1 - z2;
        temp[4 * i + 3] = z0 - z3;
    }
}

/// `rv34_idct_add_c`: transforms `block`, adds it to `dst` and clears it.
pub fn idct_add(dst: &mut [u8], pos: usize, stride: usize, block: &mut [i16; 16]) {
    let mut temp = [0i32; 16];
    row_transform(&mut temp, block);
    *block = [0; 16];
    for i in 0..4 {
        let z0 = 13 * (temp[i] + temp[8 + i]) + 0x200;
        let z1 = 13 * (temp[i] - temp[8 + i]) + 0x200;
        let z2 = 7 * temp[4 + i] - 17 * temp[12 + i];
        let z3 = 17 * temp[4 + i] + 7 * temp[12 + i];
        let o = pos + i * stride;
        dst[o] = cm(dst[o] as i32 + ((z0 + z3) >> 10));
        dst[o + 1] = cm(dst[o + 1] as i32 + ((z1 + z2) >> 10));
        dst[o + 2] = cm(dst[o + 2] as i32 + ((z1 - z2) >> 10));
        dst[o + 3] = cm(dst[o + 3] as i32 + ((z0 - z3) >> 10));
    }
}

/// `rv34_inv_transform_noround_c` (luma DC block of 16x16 macroblocks).
pub fn inv_transform_noround(block: &mut [i16; 16]) {
    let mut temp = [0i32; 16];
    row_transform(&mut temp, block);
    for i in 0..4 {
        let z0 = 39 * (temp[i] + temp[8 + i]);
        let z1 = 39 * (temp[i] - temp[8 + i]);
        let z2 = 21 * temp[4 + i] - 51 * temp[12 + i];
        let z3 = 51 * temp[4 + i] + 21 * temp[12 + i];
        block[i * 4] = ((z0 + z3) >> 11) as i16;
        block[i * 4 + 1] = ((z1 + z2) >> 11) as i16;
        block[i * 4 + 2] = ((z1 - z2) >> 11) as i16;
        block[i * 4 + 3] = ((z0 - z3) >> 11) as i16;
    }
}

/// `rv34_idct_dc_add_c`.
pub fn idct_dc_add(dst: &mut [u8], pos: usize, stride: usize, dc: i32) {
    let dc = (13 * 13 * dc + 0x200) >> 10;
    for i in 0..4 {
        let o = pos + i * stride;
        for v in &mut dst[o..o + 4] {
            *v = cm(*v as i32 + dc);
        }
    }
}

/// `rv34_inv_transform_dc_noround_c`.
pub fn inv_transform_dc_noround(block: &mut [i16; 16]) {
    let dc = ((13 * 13 * 3 * block[0] as i32) >> 11) as i16;
    *block = [dc; 16];
}

/// A source block for motion compensation: `data[origin]` is the sample
/// at the motion vector's integer position.
pub struct McSrc<'a> {
    pub data: &'a [u8],
    pub stride: usize,
    pub origin: usize,
}

impl McSrc<'_> {
    #[inline]
    fn at(&self, x: isize, y: isize) -> i32 {
        self.data[(self.origin as isize + y * self.stride as isize + x) as usize] as i32
    }
}

#[inline]
fn store(dst: &mut [u8], idx: usize, v: u8, avg: bool) {
    dst[idx] = if avg { ((dst[idx] as u32 + v as u32 + 1) >> 1) as u8 } else { v };
}

/// Destination block: `data[pos]` is its top-left sample.
pub struct McDst<'a> {
    pub data: &'a mut [u8],
    pub stride: usize,
    pub pos: usize,
}

/// RV40 6-tap filter with taps `(1, -5, c1, c2, -5, 1)`.
#[inline]
fn rv40_tap(p: [i32; 6], c1: i32, c2: i32, shift: u32) -> u8 {
    cm((p[0] + p[5] - 5 * (p[1] + p[4]) + p[2] * c1 + p[3] * c2 + (1 << (shift - 1))) >> shift)
}

/// Filter parameters for a quarter-pel phase (0 means "no filter").
fn rv40_phase(f: usize) -> (i32, i32, u32) {
    match f {
        1 => (52, 20, 6),
        2 => (20, 20, 5),
        _ => (20, 52, 6),
    }
}

/// RV40 luma quarter-pel motion compensation of a `size`x`size` block
/// (`put`/`avg_rv40_qpel{8,16}_mc{lx}{ly}_c` and the H.264 full/half-pel
/// entries FFmpeg installs in the same table).
pub fn rv40_qpel(dst: &mut McDst, src: &McSrc, size: usize, lx: usize, ly: usize, avg: bool) {
    let s = size as isize;
    if lx == 3 && ly == 3 {
        // put/avg_pixels{8,16}_xy2: (a + b + c + d + 2) >> 2.
        for y in 0..s {
            for x in 0..s {
                let v = (src.at(x, y) + src.at(x + 1, y) + src.at(x, y + 1) + src.at(x + 1, y + 1) + 2) >> 2;
                store(dst.data, dst.pos + y as usize * dst.stride + x as usize, v as u8, avg);
            }
        }
        return;
    }
    match (lx, ly) {
        (0, 0) => {
            for y in 0..s {
                for x in 0..s {
                    store(dst.data, dst.pos + y as usize * dst.stride + x as usize, src.at(x, y) as u8, avg);
                }
            }
        }
        (_, 0) => {
            let (c1, c2, sh) = rv40_phase(lx);
            for y in 0..s {
                for x in 0..s {
                    let p = [src.at(x - 2, y), src.at(x - 1, y), src.at(x, y), src.at(x + 1, y), src.at(x + 2, y), src.at(x + 3, y)];
                    store(dst.data, dst.pos + y as usize * dst.stride + x as usize, rv40_tap(p, c1, c2, sh), avg);
                }
            }
        }
        (0, _) => {
            let (c1, c2, sh) = rv40_phase(ly);
            for y in 0..s {
                for x in 0..s {
                    let p = [src.at(x, y - 2), src.at(x, y - 1), src.at(x, y), src.at(x, y + 1), src.at(x, y + 2), src.at(x, y + 3)];
                    store(dst.data, dst.pos + y as usize * dst.stride + x as usize, rv40_tap(p, c1, c2, sh), avg);
                }
            }
        }
        _ => {
            // Horizontal pass into `full` (rows -2 .. size+2), then vertical.
            let (hc1, hc2, hsh) = rv40_phase(lx);
            let (vc1, vc2, vsh) = rv40_phase(ly);
            let mut full = [0u8; 16 * 21];
            for y in 0..s + 5 {
                for x in 0..s {
                    let yy = y - 2;
                    let p = [src.at(x - 2, yy), src.at(x - 1, yy), src.at(x, yy), src.at(x + 1, yy), src.at(x + 2, yy), src.at(x + 3, yy)];
                    full[(y * s + x) as usize] = rv40_tap(p, hc1, hc2, hsh);
                }
            }
            let f = |x: isize, y: isize| full[((y + 2) * s + x) as usize] as i32;
            for y in 0..s {
                for x in 0..s {
                    let p = [f(x, y - 2), f(x, y - 1), f(x, y), f(x, y + 1), f(x, y + 2), f(x, y + 3)];
                    store(dst.data, dst.pos + y as usize * dst.stride + x as usize, rv40_tap(p, vc1, vc2, vsh), avg);
                }
            }
        }
    }
}

/// RV30 luma third-pel motion compensation (`put`/`avg_rv30_tpel{8,16}_mc{lx}{ly}_c`
/// plus the H.264 full-pel entry for `mc00`).
pub fn rv30_tpel(dst: &mut McDst, src: &McSrc, size: usize, lx: usize, ly: usize, avg: bool) {
    let s = size as isize;
    for y in 0..s {
        for x in 0..s {
            let v = match (lx, ly) {
                (0, 0) => src.at(x, y),
                (1, 0) | (2, 0) => {
                    let (c1, c2) = if lx == 1 { (12, 6) } else { (6, 12) };
                    (-(src.at(x - 1, y) + src.at(x + 2, y)) + src.at(x, y) * c1 + src.at(x + 1, y) * c2 + 8) >> 4
                }
                (0, 1) | (0, 2) => {
                    let (c1, c2) = if ly == 1 { (12, 6) } else { (6, 12) };
                    (-(src.at(x, y - 1) + src.at(x, y + 2)) + src.at(x, y) * c1 + src.at(x, y + 1) * c2 + 8) >> 4
                }
                (1, 1) => {
                    let r = |dy: isize| src.at(x - 1, y + dy);
                    let _ = r;
                    (src.at(x - 1, y - 1) - 12 * src.at(x, y - 1) - 6 * src.at(x + 1, y - 1) + src.at(x + 2, y - 1)
                        - 12 * src.at(x - 1, y)
                        + 144 * src.at(x, y)
                        + 72 * src.at(x + 1, y)
                        - 12 * src.at(x + 2, y)
                        - 6 * src.at(x - 1, y + 1)
                        + 72 * src.at(x, y + 1)
                        + 36 * src.at(x + 1, y + 1)
                        - 6 * src.at(x + 2, y + 1)
                        + src.at(x - 1, y + 2)
                        - 12 * src.at(x, y + 2)
                        - 6 * src.at(x + 1, y + 2)
                        + src.at(x + 2, y + 2)
                        + 128)
                        >> 8
                }
                (2, 1) => {
                    // hhv
                    (src.at(x - 1, y - 1) - 12 * src.at(x + 1, y - 1) - 6 * src.at(x, y - 1) + src.at(x + 2, y - 1)
                        - 12 * src.at(x - 1, y)
                        + 144 * src.at(x + 1, y)
                        + 72 * src.at(x, y)
                        - 12 * src.at(x + 2, y)
                        - 6 * src.at(x - 1, y + 1)
                        + 72 * src.at(x + 1, y + 1)
                        + 36 * src.at(x, y + 1)
                        - 6 * src.at(x + 2, y + 1)
                        + src.at(x - 1, y + 2)
                        - 12 * src.at(x + 1, y + 2)
                        - 6 * src.at(x, y + 2)
                        + src.at(x + 2, y + 2)
                        + 128)
                        >> 8
                }
                (1, 2) => {
                    // hvv
                    (src.at(x - 1, y - 1) - 12 * src.at(x, y - 1) - 6 * src.at(x + 1, y - 1) + src.at(x + 2, y - 1)
                        - 6 * src.at(x - 1, y)
                        + 72 * src.at(x, y)
                        + 36 * src.at(x + 1, y)
                        - 6 * src.at(x + 2, y)
                        - 12 * src.at(x - 1, y + 1)
                        + 144 * src.at(x, y + 1)
                        + 72 * src.at(x + 1, y + 1)
                        - 12 * src.at(x + 2, y + 1)
                        + src.at(x - 1, y + 2)
                        - 12 * src.at(x, y + 2)
                        - 6 * src.at(x + 1, y + 2)
                        + src.at(x + 2, y + 2)
                        + 128)
                        >> 8
                }
                _ => {
                    // (2, 2): hhvv
                    (36 * src.at(x, y) + 54 * src.at(x + 1, y) + 6 * src.at(x + 2, y)
                        + 54 * src.at(x, y + 1)
                        + 81 * src.at(x + 1, y + 1)
                        + 9 * src.at(x + 2, y + 1)
                        + 6 * src.at(x, y + 2)
                        + 9 * src.at(x + 1, y + 2)
                        + src.at(x + 2, y + 2)
                        + 128)
                        >> 8
                }
            };
            store(dst.data, dst.pos + y as usize * dst.stride + x as usize, cm(v), avg);
        }
    }
}

/// RV40 chroma rounding bias (`ff_rv40_bias`).
const RV40_BIAS: [[i32; 4]; 4] = [[0, 16, 32, 16], [32, 28, 32, 28], [0, 32, 16, 32], [32, 28, 32, 28]];

/// Bilinear eighth-pel chroma MC of a `w`x`h` block: `h264_chroma_mc{4,8}`
/// (`rv40 == false`, rounding +32) or `rv40_chroma_mc{4,8}` (position bias).
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(dst: &mut McDst, src: &McSrc, w: usize, h: usize, x: i32, y: i32, rv40: bool, avg: bool) {
    let a = (8 - x) * (8 - y);
    let b = x * (8 - y);
    let c = (8 - x) * y;
    let d = x * y;
    let bias = if rv40 { RV40_BIAS[(y >> 1) as usize][(x >> 1) as usize] } else { 32 };
    for j in 0..h as isize {
        for i in 0..w as isize {
            let v = a * src.at(i, j) + b * src.at(i + 1, j) + c * src.at(i, j + 1) + d * src.at(i + 1, j + 1);
            let v = ((v + bias) >> 6) as u8;
            store(dst.data, dst.pos + j as usize * dst.stride + i as usize, v, avg);
        }
    }
}

/// `rv40_weight_func_{rnd,nornd}_{16,8}`: `dst = w2 * src1 + w1 * src2`
/// with FFmpeg's unsigned wrap-around and 8-bit store.
#[allow(clippy::too_many_arguments)]
pub fn rv40_weight(
    dst: &mut [u8],
    dpos: usize,
    dstride: usize,
    src1: &[u8],
    src2: &[u8],
    sstride: usize,
    size: usize,
    w1: i32,
    w2: i32,
    scaled: bool,
) {
    let (w1, w2) = (w1 as u32, w2 as u32);
    for j in 0..size {
        for i in 0..size {
            let a = src1[j * sstride + i] as u32;
            let b = src2[j * sstride + i] as u32;
            let v = if scaled {
                w2.wrapping_mul(a).wrapping_add(w1.wrapping_mul(b)).wrapping_add(0x10) >> 5
            } else {
                ((w2.wrapping_mul(a) >> 9).wrapping_add(w1.wrapping_mul(b) >> 9).wrapping_add(0x10)) >> 5
            };
            dst[dpos + j * dstride + i] = v as u8;
        }
    }
}

const RV40_DITHER_L: [i32; 16] = [0x40, 0x50, 0x20, 0x60, 0x30, 0x50, 0x40, 0x30, 0x50, 0x40, 0x50, 0x30, 0x60, 0x20, 0x50, 0x40];
const RV40_DITHER_R: [i32; 16] = [0x40, 0x30, 0x60, 0x20, 0x50, 0x30, 0x30, 0x40, 0x40, 0x40, 0x50, 0x30, 0x20, 0x60, 0x30, 0x40];

/// Sample `k` steps across the edge at line `i` (`src[i*stride + k*step]`).
#[inline]
fn px(buf: &[u8], src: usize, i: usize, stride: usize, step: usize, k: isize) -> i32 {
    buf[(src as isize + (i * stride) as isize + k * step as isize) as usize] as i32
}

#[inline]
fn set(buf: &mut [u8], src: usize, i: usize, stride: usize, step: usize, k: isize, v: u8) {
    buf[(src as isize + (i * stride) as isize + k * step as isize) as usize] = v;
}

/// `rv40_weak_loop_filter`; `step` crosses the edge, `stride` runs along it.
#[allow(clippy::too_many_arguments)]
pub fn rv40_weak_loop_filter(
    buf: &mut [u8],
    src: usize,
    step: usize,
    stride: usize,
    filter_p1: bool,
    filter_q1: bool,
    alpha: i32,
    beta: i32,
    lim_p0q0: i32,
    lim_q1: i32,
    lim_p1: i32,
) {
    for i in 0..4 {
        let p = |k: isize| px(buf, src, i, stride, step, k);
        let diff_p1p0 = p(-2) - p(-1);
        let diff_q1q0 = p(1) - p(0);
        let diff_p1p2 = p(-2) - p(-3);
        let diff_q1q2 = p(1) - p(2);

        let mut t = p(0) - p(-1);
        if t == 0 {
            continue;
        }
        let u = (alpha * t.abs()) >> 7;
        if u > 3 - (filter_p1 && filter_q1) as i32 {
            continue;
        }
        t *= 4;
        if filter_p1 && filter_q1 {
            t += p(-2) - p(1);
        }
        let diff = ((t + 4) >> 3).clamp(-lim_p0q0, lim_p0q0);
        let (pm1, p0) = (p(-1), p(0));
        set(buf, src, i, stride, step, -1, cm(pm1 + diff));
        set(buf, src, i, stride, step, 0, cm(p0 - diff));

        if filter_p1 && diff_p1p2.abs() <= beta {
            let t = (diff_p1p0 + diff_p1p2 - diff) >> 1;
            let v = px(buf, src, i, stride, step, -2);
            set(buf, src, i, stride, step, -2, cm(v - t.clamp(-lim_p1, lim_p1)));
        }
        if filter_q1 && diff_q1q2.abs() <= beta {
            let t = (diff_q1q0 + diff_q1q2 + diff) >> 1;
            let v = px(buf, src, i, stride, step, 1);
            set(buf, src, i, stride, step, 1, cm(v - t.clamp(-lim_q1, lim_q1)));
        }
    }
}

/// `rv40_strong_loop_filter`.
#[allow(clippy::too_many_arguments)]
pub fn rv40_strong_loop_filter(buf: &mut [u8], src: usize, step: usize, stride: usize, alpha: i32, lims: i32, dmode: usize, chroma: bool) {
    for i in 0..4 {
        let p = |k: isize| px(buf, src, i, stride, step, k);
        let t = p(0) - p(-1);
        if t == 0 {
            continue;
        }
        let sflag = (alpha * t.abs()) >> 7;
        if sflag > 1 {
            continue;
        }
        let dl = RV40_DITHER_L[dmode + i];
        let dr = RV40_DITHER_R[dmode + i];
        let mut p0 = (25 * p(-3) + 26 * p(-2) + 26 * p(-1) + 26 * p(0) + 25 * p(1) + dl) >> 7;
        let mut q0 = (25 * p(-2) + 26 * p(-1) + 26 * p(0) + 26 * p(1) + 25 * p(2) + dr) >> 7;
        if sflag != 0 {
            p0 = p0.clamp(p(-1) - lims, p(-1) + lims);
            q0 = q0.clamp(p(0) - lims, p(0) + lims);
        }
        let mut p1 = (25 * p(-4) + 26 * p(-3) + 26 * p(-2) + 26 * p0 + 25 * p(0) + dl) >> 7;
        let mut q1 = (25 * p(-1) + 26 * q0 + 26 * p(1) + 26 * p(2) + 25 * p(3) + dr) >> 7;
        if sflag != 0 {
            p1 = p1.clamp(p(-2) - lims, p(-2) + lims);
            q1 = q1.clamp(p(1) - lims, p(1) + lims);
        }
        set(buf, src, i, stride, step, -2, p1 as u8);
        set(buf, src, i, stride, step, -1, p0 as u8);
        set(buf, src, i, stride, step, 0, q0 as u8);
        set(buf, src, i, stride, step, 1, q1 as u8);
        if !chroma {
            let p = |k: isize| px(buf, src, i, stride, step, k);
            let v3 = (25 * p(-1) + 26 * p(-2) + 51 * p(-3) + 26 * p(-4) + 64) >> 7;
            let v2 = (25 * p(0) + 26 * p(1) + 51 * p(2) + 26 * p(3) + 64) >> 7;
            set(buf, src, i, stride, step, -3, v3 as u8);
            set(buf, src, i, stride, step, 2, v2 as u8);
        }
    }
}

/// `rv40_loop_filter_strength`: returns `(strong, filter_p1, filter_q1)`.
pub fn rv40_loop_filter_strength(buf: &[u8], src: usize, step: usize, stride: usize, beta: i32, beta2: i32, edge: bool) -> (bool, bool, bool) {
    let mut sum_p1p0 = 0;
    let mut sum_q1q0 = 0;
    for i in 0..4 {
        sum_p1p0 += px(buf, src, i, stride, step, -2) - px(buf, src, i, stride, step, -1);
        sum_q1q0 += px(buf, src, i, stride, step, 1) - px(buf, src, i, stride, step, 0);
    }
    let p1 = sum_p1p0.abs() < (beta << 2);
    let q1 = sum_q1q0.abs() < (beta << 2);
    if !p1 && !q1 {
        return (false, p1, q1);
    }
    if !edge {
        return (false, p1, q1);
    }
    let mut sum_p1p2 = 0;
    let mut sum_q1q2 = 0;
    for i in 0..4 {
        sum_p1p2 += px(buf, src, i, stride, step, -2) - px(buf, src, i, stride, step, -3);
        sum_q1q2 += px(buf, src, i, stride, step, 1) - px(buf, src, i, stride, step, 2);
    }
    let strong0 = p1 && sum_p1p2.abs() < beta2;
    let strong1 = q1 && sum_q1q2.abs() < beta2;
    (strong0 && strong1, p1, q1)
}

/// `rv30_weak_loop_filter` (rv30.c).
pub fn rv30_weak_loop_filter(buf: &mut [u8], src: usize, step: usize, stride: usize, lim: i32) {
    for i in 0..4 {
        let p = |k: isize| px(buf, src, i, stride, step, k);
        let diff = ((p(-2) - p(1)) - (p(-1) - p(0)) * 4) >> 3;
        let diff = diff.clamp(-lim, lim);
        let (pm1, p0) = (p(-1), p(0));
        set(buf, src, i, stride, step, -1, cm(pm1 + diff));
        set(buf, src, i, stride, step, 0, cm(p0 - diff));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dc_only_transform_matches_full_transform() {
        for dc in [-300i16, -17, 0, 5, 64, 999] {
            let mut a = [0i16; 16];
            a[0] = dc;
            let mut b = a;
            inv_transform_noround(&mut a);
            inv_transform_dc_noround(&mut b);
            assert_eq!(a, b, "dc {dc}");
        }
    }
}

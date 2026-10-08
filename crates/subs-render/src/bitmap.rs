// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
// Copyright (C) 2011 Grigori Goronzy <greg@chown.ath.cx>
// Copyright (c) 2011-2014, Yu Zhuohuang <yuzhuohuang@qq.com>
// Copyright (C) 2015 Vabishchevich Nikolay <vabnick@gmail.com>
// Copyright (C) 2009-2022 libass contributors
//
// This file is part of libass.
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Derived from libass 0.17.5 (commit 4a05d81): libass/ass_bitmap.c,
// libass/ass_blur.c, libass/c/blur_template.h, libass/c/c_blur.c,
// libass/c/c_be_blur.c, libass/c/c_blend_bitmaps.c and the two-outline union
// in libass/ass_rasterizer.c and libass/c/rasterizer_template.h. Changed for
// PearTube on 2026-10-08: ported to safe Rust; the C kernels' stripe layout
// replaced by plain rows (the same zero-padded arithmetic); outlines
// filled by ab_glyph_rasterizer's exact-area rasterizer instead of
// libass's.

//! 8-bit coverage bitmaps: outlines filled, blurred, shifted, combined.

use ab_glyph_rasterizer::{point, Rasterizer};

use crate::outline::{Outline, Rect};

/// Largest bitmap, in pixels (a hostile `\fs` or drawing cannot ask for
/// more).
pub const MAX_BITMAP_PIXELS: i64 = 1 << 25;

/// A coverage bitmap at `left`, `top` (pixels), `w x h`, rows `stride`
/// apart.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bitmap {
    pub left: i32,
    pub top: i32,
    pub w: i32,
    pub h: i32,
    pub stride: usize,
    pub buffer: Vec<u8>,
}

impl Bitmap {
    /// `ass_alloc_bitmap`, zeroed; `None` past the size bound.
    pub fn alloc(w: i32, h: i32) -> Option<Bitmap> {
        if w < 0 || h < 0 || i64::from(w) * i64::from(h) > MAX_BITMAP_PIXELS {
            return None;
        }
        let stride = w as usize;
        Some(Bitmap { left: 0, top: 0, w, h, stride, buffer: vec![0; stride * h as usize] })
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn row(&self, y: usize) -> &[u8] {
        &self.buffer[y * self.stride..y * self.stride + self.w as usize]
    }
}

/// `ass_outline_to_bitmap`: `outline1` and `outline2` (the two borders of a
/// stroke, or one fill) rasterized separately with nonzero winding, then
/// combined by maximum coverage. Opposite winding must not cancel a stroke.
pub fn outline_to_bitmap(outline1: Option<&Outline>, outline2: Option<&Outline>) -> Option<Bitmap> {
    let mut bbox = Rect::reset();
    for o in [outline1, outline2].into_iter().flatten() {
        o.update_cbox(&mut bbox);
    }
    if bbox.x_min > bbox.x_max || bbox.y_min > bbox.y_max {
        return None;
    }
    // Enlarge by 1/64th of a pixel, add a pixel for shift_bitmap.
    let x_min = (bbox.x_min - 1) >> 6;
    let y_min = (bbox.y_min - 1) >> 6;
    let x_max = (bbox.x_max + 127) >> 6;
    let y_max = (bbox.y_max + 127) >> 6;
    let (w, h) = (x_max - x_min, y_max - y_min);
    let mut bm = Bitmap::alloc(w, h)?;
    bm.left = x_min;
    bm.top = y_min;
    if w == 0 || h == 0 {
        return Some(bm);
    }
    let mut r = Rasterizer::new(w as usize, h as usize);
    let (ox, oy) = (x_min as f32, y_min as f32);
    let p = |v: crate::outline::Vector| point(v.x as f32 / 64.0 - ox, v.y as f32 / 64.0 - oy);
    for (index, o) in [outline1, outline2].into_iter().flatten().filter(|o| !o.is_empty()).enumerate() {
        if index != 0 { r.clear(); }
        o.for_each_segment(|pts| match pts.len() {
            2 => r.draw_line(p(pts[0]), p(pts[1])),
            3 => r.draw_quad(p(pts[0]), p(pts[1]), p(pts[2])),
            4 => r.draw_cubic(p(pts[0]), p(pts[1]), p(pts[2]), p(pts[3])),
            _ => {}
        });
        r.for_each_pixel(|i, a| bm.buffer[i] = bm.buffer[i].max((a * 255.0 + 0.5) as u8));
    }
    Some(bm)
}

/// `ass_fix_outline`: the glyph's coverage taken out of its border's, as
/// VSFilter does for borders under transparent glyphs.
pub fn fix_outline(bm_g: &Bitmap, bm_o: &mut Bitmap) {
    if bm_g.is_empty() || bm_o.is_empty() {
        return;
    }
    let l = bm_o.left.max(bm_g.left);
    let t = bm_o.top.max(bm_g.top);
    let r = (bm_o.left + bm_o.stride as i32).min(bm_g.left + bm_g.stride as i32);
    let b = (bm_o.top + bm_o.h).min(bm_g.top + bm_g.h);
    for y in 0..(b - t).max(0) {
        let gy = (t - bm_g.top + y) as usize * bm_g.stride;
        let oy = (t - bm_o.top + y) as usize * bm_o.stride;
        for x in 0..(r - l).max(0) {
            let g = bm_g.buffer[gy + (l - bm_g.left + x) as usize];
            let o = &mut bm_o.buffer[oy + (l - bm_o.left + x) as usize];
            *o = if *o > g { *o - g / 2 } else { 0 };
        }
    }
}

/// `ass_shift_bitmap`: moved right and down by `shift_x`, `shift_y`
/// 64ths of a pixel (each below 64).
pub fn shift_bitmap(bm: &mut Bitmap, shift_x: i32, shift_y: i32) {
    if bm.is_empty() || shift_x & !63 != 0 || shift_y & !63 != 0 {
        return;
    }
    let (w, h, s) = (bm.w as usize, bm.h as usize, bm.stride);
    let buf = &mut bm.buffer;
    if shift_x != 0 {
        for y in 0..h {
            for x in (1..w).rev() {
                let b = (u32::from(buf[x + y * s - 1]) * shift_x as u32 >> 6) as u8;
                buf[x + y * s - 1] -= b;
                buf[x + y * s] = buf[x + y * s].wrapping_add(b);
            }
        }
    }
    if shift_y != 0 {
        for x in 0..w {
            for y in (1..h).rev() {
                let b = (u32::from(buf[x + y * s - s]) * shift_y as u32 >> 6) as u8;
                buf[x + y * s - s] -= b;
                buf[x + y * s] = buf[x + y * s].wrapping_add(b);
            }
        }
    }
}

/// `ass_add_bitmaps_c`: saturating sum into `dst`.
pub fn add_bitmaps(dst: &mut [u8], dst_stride: usize, src: &[u8], src_stride: usize, width: usize, height: usize) {
    for y in 0..height {
        for x in 0..width {
            let out = u32::from(dst[y * dst_stride + x]) + u32::from(src[y * src_stride + x]);
            dst[y * dst_stride + x] = out.min(255) as u8;
        }
    }
}

/// `ass_imul_bitmaps_c`: `dst` times the inverse of `src`.
pub fn imul_bitmaps(dst: &mut [u8], dst_stride: usize, src: &[u8], src_stride: usize, width: usize, height: usize) {
    for y in 0..height {
        for x in 0..width {
            let d = &mut dst[y * dst_stride + x];
            *d = ((u32::from(*d) * (255 - u32::from(src[y * src_stride + x])) + 255) >> 8) as u8;
        }
    }
}

/// `ass_mul_bitmaps_c`: `src1` times `src2` into `dst`.
#[allow(clippy::too_many_arguments)]
pub fn mul_bitmaps(dst: &mut [u8], dst_stride: usize, src1: &[u8], src1_stride: usize, src2: &[u8], src2_stride: usize, width: usize, height: usize) {
    for y in 0..height {
        for x in 0..width {
            dst[y * dst_stride + x] = ((u32::from(src1[y * src1_stride + x]) * u32::from(src2[y * src2_stride + x]) + 255) >> 8) as u8;
        }
    }
}

/// `ass_be_blur_c`: VSFilter's [[1,2,1],[2,4,2],[1,2,1]] blur, in place.
fn be_blur(buf: &mut [u8], stride: usize, width: usize, height: usize) {
    if width < 2 || height < 2 {
        return;
    }
    let mut col_pix_buf = vec![0u16; stride.max(width)];
    let mut col_sum_buf = vec![0u16; stride.max(width)];
    fn sliding_sum(prev: &mut u16, next: u16) -> u16 {
        let sum = prev.wrapping_add(next);
        *prev = next;
        sum
    }
    {
        let mut x = 1;
        let mut sum = u16::from(buf[x - 1]);
        while x < width {
            let col_pix = sliding_sum(&mut sum, u16::from(buf[x - 1]) + u16::from(buf[x]));
            col_pix_buf[x - 1] = col_pix;
            col_sum_buf[x - 1] = col_pix;
            x += 1;
        }
        let col_pix = sum.wrapping_add(u16::from(buf[x - 1]));
        col_pix_buf[x - 1] = col_pix;
        col_sum_buf[x - 1] = col_pix;
    }
    let mut row = 0;
    for _ in 1..height {
        let dst = row;
        row += stride;
        let mut x = 1;
        let mut sum = u16::from(buf[row + x - 1]);
        while x < width {
            let col_pix = sliding_sum(&mut sum, u16::from(buf[row + x - 1]) + u16::from(buf[row + x]));
            let col_sum = sliding_sum(&mut col_pix_buf[x - 1], col_pix);
            buf[dst + x - 1] = (sliding_sum(&mut col_sum_buf[x - 1], col_sum) >> 4) as u8;
            x += 1;
        }
        let col_pix = sum.wrapping_add(u16::from(buf[row + x - 1]));
        let col_sum = sliding_sum(&mut col_pix_buf[x - 1], col_pix);
        buf[dst + x - 1] = (sliding_sum(&mut col_sum_buf[x - 1], col_sum) >> 4) as u8;
    }
    for x in 0..width {
        buf[row + x] = (col_sum_buf[x].wrapping_add(col_pix_buf[x]) >> 4) as u8;
    }
}

/// `ass_synth_blur`: a gaussian blur, then `be` passes of the box blur.
pub fn synth_blur(bm: &mut Bitmap, mut be: i32, blur_r2x: f64, blur_r2y: f64) {
    if bm.is_empty() {
        return;
    }
    if blur_r2x > 0.001 || blur_r2y > 0.001 {
        gaussian_blur(bm, blur_r2x, blur_r2y);
    }
    if be <= 0 {
        return;
    }
    let (w, h, stride) = (bm.w as usize, bm.h as usize, bm.stride);
    be -= 1;
    if be > 0 {
        // Equivalent to (value * 64 + 127) / 255 for 0..=256.
        for y in 0..h {
            for v in &mut bm.buffer[y * stride..y * stride + w] {
                *v = ((*v >> 1) + 1) >> 1;
            }
        }
        while be > 0 {
            be_blur(&mut bm.buffer, stride, w, h);
            be -= 1;
        }
        // Equivalent to (value * 255 + 32) / 64 for 0..=96.
        for y in 0..h {
            for v in &mut bm.buffer[y * stride..y * stride + w] {
                *v = (*v << 2).wrapping_sub(u8::from(*v > 32));
            }
        }
    }
    be_blur(&mut bm.buffer, stride, w, h);
}

fn calc_gauss(res: &mut [f64], n: usize, r2: f64) {
    let alpha = 0.5 / r2;
    let mut mul = (-alpha).exp();
    let mul2 = mul * mul;
    let mut cur = (alpha / std::f64::consts::PI).sqrt();
    res[0] = cur;
    cur *= mul;
    res[1] = cur;
    for r in res.iter_mut().take(n).skip(2) {
        mul *= mul2;
        cur *= mul;
        *r = cur;
    }
}

fn coeff_filter(coeff: &mut [f64], n: usize, kernel: &[f64; 4]) {
    let (mut prev1, mut prev2, mut prev3) = (coeff[1], coeff[2], coeff[3]);
    for i in 0..n {
        let res = coeff[i] * kernel[0] + (prev1 + coeff[i + 1]) * kernel[1] + (prev2 + coeff[i + 2]) * kernel[2] + (prev3 + coeff[i + 3]) * kernel[3];
        prev3 = prev2;
        prev2 = prev1;
        prev1 = coeff[i];
        coeff[i] = res;
    }
}

fn calc_matrix(mat: &mut [[f64; 8]; 8], mat_freq: &[f64], n: usize) {
    for i in 0..n {
        mat[i][i] = mat_freq[2 * i + 2] + 3.0 * mat_freq[0] - 4.0 * mat_freq[i + 1];
        for j in i + 1..n {
            let v = mat_freq[i + j + 2] + mat_freq[j - i] + 2.0 * (mat_freq[0] - mat_freq[i + 1] - mat_freq[j + 1]);
            mat[i][j] = v;
            mat[j][i] = v;
        }
    }
    // Invert transpose.
    for k in 0..n {
        let z = 1.0 / mat[k][k];
        mat[k][k] = 1.0;
        for i in 0..n {
            if i == k {
                continue;
            }
            let mul = mat[i][k] * z;
            mat[i][k] = 0.0;
            for j in 0..n {
                mat[i][j] -= mat[k][j] * mul;
            }
        }
        for j in 0..n {
            mat[k][j] *= z;
        }
    }
}

fn calc_coeff(mu: &mut [f64; 8], n: usize, r2: f64, mul: f64) {
    let w = 12096.0;
    let kernel = [
        (((3280.0 / w) * mul + 1092.0 / w) * mul + 2520.0 / w) * mul + 5204.0 / w,
        (((-2460.0 / w) * mul - 273.0 / w) * mul - 210.0 / w) * mul + 2943.0 / w,
        (((984.0 / w) * mul - 546.0 / w) * mul - 924.0 / w) * mul + 486.0 / w,
        (((-164.0 / w) * mul + 273.0 / w) * mul - 126.0 / w) * mul + 17.0 / w,
    ];
    let mut mat_freq = [0.0; 17];
    mat_freq[..4].copy_from_slice(&kernel);
    coeff_filter(&mut mat_freq, 7, &kernel);
    let mut vec_freq = [0.0; 12];
    calc_gauss(&mut vec_freq, n + 4, r2 * mul);
    coeff_filter(&mut vec_freq, n + 1, &kernel);
    let mut mat = [[0.0; 8]; 8];
    calc_matrix(&mut mat, &mat_freq, n);
    let mut vec = [0.0; 8];
    for i in 0..n {
        vec[i] = mat_freq[0] - mat_freq[i + 1] - vec_freq[0] + vec_freq[i + 1];
    }
    for i in 0..n {
        let mut res = 0.0;
        for j in 0..n {
            res += mat[i][j] * vec[j];
        }
        mu[i] = res.max(0.0);
    }
}

#[derive(Clone, Copy, Debug)]
struct BlurMethod {
    level: i32,
    radius: usize,
    coeff: [i16; 8],
}

/// C `frexp` for positive finite values: the mantissa in [0.5, 1) and
/// the exponent.
fn frexp(x: f64) -> (f64, i32) {
    if x == 0.0 || !x.is_finite() {
        return (x, 0);
    }
    let mut e = x.abs().log2().floor() as i32 + 1;
    let mut m = x / 2f64.powi(e);
    // Guard rounding at powers of two.
    if m.abs() >= 1.0 {
        m /= 2.0;
        e += 1;
    } else if m.abs() < 0.5 {
        m *= 2.0;
        e -= 1;
    }
    (m, e)
}

fn find_best_method(r2: f64) -> BlurMethod {
    let mut mu = [0.0; 8];
    let mut blur = BlurMethod { level: 0, radius: 4, coeff: [0; 8] };
    if r2 < 0.5 {
        mu[1] = 0.085 * r2 * r2 * r2;
        mu[0] = 0.5 * r2 - 4.0 * mu[1];
    } else {
        let (frac, level) = frexp((0.11569 * r2 + 0.20591047).sqrt());
        blur.level = level;
        let mul = 0.25f64.powi(level);
        let radius = 8 - ((10.1525 + 0.8335 * mul) * (1.0 - frac)) as i32;
        blur.radius = radius.clamp(4, 8) as usize;
        calc_coeff(&mut mu, blur.radius, r2, mul);
    }
    for i in 0..blur.radius {
        blur.coeff[i] = (0x10000 as f64 * mu[i] + 0.5) as i32 as i16;
    }
    blur
}

/// An image of 14-bit values (0..=0x4000), `w x h`, row-major; pixels
/// outside read as zero.
struct Plane {
    w: usize,
    h: usize,
    px: Vec<i16>,
}

impl Plane {
    fn at(&self, x: isize, y: isize) -> i16 {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            0
        } else {
            self.px[y as usize * self.w + x as usize]
        }
    }
}

fn shrink_func(p1p: i16, p1n: i16, z0p: i16, z0n: i16, n1p: i16, n1n: i16) -> i16 {
    let (p1p, p1n, z0p, z0n, n1p, n1n) = (i32::from(p1p), i32::from(p1n), i32::from(z0p), i32::from(z0n), i32::from(n1p), i32::from(n1n));
    let mut r = (p1p + p1n + n1p + n1n) >> 1;
    r = (r + z0p + z0n) >> 1;
    r = (r + p1n + n1p) >> 1;
    ((r + z0p + z0n + 2) >> 2) as i16
}

fn expand_func(p1: i16, z0: i16, n1: i16) -> (i16, i16) {
    let (p1, z0, n1) = (i32::from(p1), i32::from(z0), i32::from(n1));
    let r = ((((p1 + n1) as u16 >> 1) as i32 + z0) as u16) >> 1;
    let rp = ((((i32::from(r) + p1) as u16 >> 1) as i32 + z0 + 1) as u16) >> 1;
    let rn = ((((i32::from(r) + n1) as u16 >> 1) as i32 + z0 + 1) as u16) >> 1;
    (rp as i16, rn as i16)
}

fn shrink_horz(src: &Plane) -> Plane {
    let w = (src.w + 5) >> 1;
    let mut px = vec![0i16; w * src.h];
    for y in 0..src.h as isize {
        for x in 0..w as isize {
            let s = |dx: isize| src.at(2 * x + dx, y);
            px[y as usize * w + x as usize] = shrink_func(s(-4), s(-3), s(-2), s(-1), s(0), s(1));
        }
    }
    Plane { w, h: src.h, px }
}

fn shrink_vert(src: &Plane) -> Plane {
    let h = (src.h + 5) >> 1;
    let mut px = vec![0i16; src.w * h];
    for y in 0..h as isize {
        for x in 0..src.w as isize {
            let s = |dy: isize| src.at(x, 2 * y + dy);
            px[y as usize * src.w + x as usize] = shrink_func(s(-4), s(-3), s(-2), s(-1), s(0), s(1));
        }
    }
    Plane { w: src.w, h, px }
}

fn expand_horz(src: &Plane) -> Plane {
    let w = 2 * src.w + 4;
    let mut px = vec![0i16; w * src.h];
    for y in 0..src.h as isize {
        for j in 0..(src.w + 2) as isize {
            let (rp, rn) = expand_func(src.at(j - 2, y), src.at(j - 1, y), src.at(j, y));
            px[y as usize * w + 2 * j as usize] = rp;
            px[y as usize * w + 2 * j as usize + 1] = rn;
        }
    }
    Plane { w, h: src.h, px }
}

fn expand_vert(src: &Plane) -> Plane {
    let h = 2 * src.h + 4;
    let mut px = vec![0i16; src.w * h];
    for j in 0..(src.h + 2) as isize {
        for x in 0..src.w as isize {
            let (rp, rn) = expand_func(src.at(x, j - 2), src.at(x, j - 1), src.at(x, j));
            px[2 * j as usize * src.w + x as usize] = rp;
            px[(2 * j as usize + 1) * src.w + x as usize] = rn;
        }
    }
    Plane { w: src.w, h, px }
}

fn blur_horz(src: &Plane, param: &[i16], n: usize) -> Plane {
    let w = src.w + 2 * n;
    let mut px = vec![0i16; w * src.h];
    for y in 0..src.h as isize {
        for x in 0..w as isize {
            let center = src.at(x - n as isize, y);
            let mut acc: i32 = 0x8000;
            for i in (1..=n as isize).rev() {
                let p = i32::from(param[i as usize - 1]);
                acc = acc.wrapping_add(i32::from(src.at(x - n as isize - i, y).wrapping_sub(center)) * p + i32::from(src.at(x - n as isize + i, y).wrapping_sub(center)) * p);
            }
            px[y as usize * w + x as usize] = center.wrapping_add((acc >> 16) as i16);
        }
    }
    Plane { w, h: src.h, px }
}

fn blur_vert(src: &Plane, param: &[i16], n: usize) -> Plane {
    let h = src.h + 2 * n;
    let mut px = vec![0i16; src.w * h];
    for y in 0..h as isize {
        for x in 0..src.w as isize {
            let center = src.at(x, y - n as isize);
            let mut acc: i32 = 0x8000;
            for i in (1..=n as isize).rev() {
                let p = i32::from(param[i as usize - 1]);
                acc = acc.wrapping_add(i32::from(src.at(x, y - n as isize - i).wrapping_sub(center)) * p + i32::from(src.at(x, y - n as isize + i).wrapping_sub(center)) * p);
            }
            px[y as usize * src.w + x as usize] = center.wrapping_add((acc >> 16) as i16);
        }
    }
    Plane { w: src.w, h, px }
}

/// `ass_gaussian_blur`: the cascade blur, standard deviations squared
/// `r2x`, `r2y`. The bitmap grows by the blur's reach.
pub fn gaussian_blur(bm: &mut Bitmap, r2x: f64, r2y: f64) -> bool {
    let blur_x = find_best_method(r2x);
    let blur_y = if r2y == r2x { blur_x } else { find_best_method(r2y) };
    let (w, h) = (bm.w as u32, bm.h as u32);
    if w == 0 || h == 0 {
        return true;
    }
    let offset_x = (((2 * blur_x.radius as i64 + 9) << blur_x.level) - 5) as u32;
    let offset_y = (((2 * blur_y.radius as i64 + 9) << blur_y.level) - 5) as u32;
    let end_w = (w.wrapping_add(offset_x) & !((1u32 << blur_x.level) - 1)).wrapping_sub(4);
    let end_h = (h.wrapping_add(offset_y) & !((1u32 << blur_y.level) - 1)).wrapping_sub(4);
    if u64::from(end_w) * u64::from(end_h) > (i32::MAX / 4) as u64 || i64::from(end_w) * i64::from(end_h) > MAX_BITMAP_PIXELS {
        return false;
    }
    // Unpack: value * 0x4000 / 255, rounded.
    let mut plane = Plane { w: w as usize, h: h as usize, px: Vec::with_capacity(w as usize * h as usize) };
    for y in 0..h as usize {
        for &v in bm.row(y) {
            let v = u16::from(v);
            plane.px.push(((((v << 7) | (v >> 1)) + 1) >> 1) as i16);
        }
    }
    for _ in 0..blur_y.level {
        plane = shrink_vert(&plane);
    }
    for _ in 0..blur_x.level {
        plane = shrink_horz(&plane);
    }
    plane = blur_horz(&plane, &blur_x.coeff, blur_x.radius);
    plane = blur_vert(&plane, &blur_y.coeff, blur_y.radius);
    for _ in 0..blur_x.level {
        plane = expand_horz(&plane);
    }
    for _ in 0..blur_y.level {
        plane = expand_vert(&plane);
    }
    if plane.w != end_w as usize || plane.h != end_h as usize {
        return false;
    }
    let Some(mut out) = Bitmap::alloc(end_w as i32, end_h as i32) else { return false };
    out.left = bm.left - (((blur_x.radius as i32 + 4) << blur_x.level) - 4);
    out.top = bm.top - (((blur_y.radius as i32 + 4) << blur_y.level) - 4);
    const DITHER: [[i16; 2]; 2] = [[8, 40], [56, 24]];
    for y in 0..plane.h {
        for x in 0..plane.w {
            let s = plane.px[y * plane.w + x];
            let v = (s.wrapping_sub(s >> 8).wrapping_add(DITHER[y & 1][x & 1]) as u16) >> 6;
            out.buffer[y * out.stride + x] = v as u8;
        }
    }
    *bm = out;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_square_fills_its_pixels() {
        let mut o = Outline::default();
        o.add_rect(64, 64, 64 * 5, 64 * 3);
        let bm = outline_to_bitmap(Some(&o), None).unwrap();
        // Pixels 1..5 x 1..3 full, the rest empty.
        let at = |x: i32, y: i32| bm.buffer[((y - bm.top) * bm.w + (x - bm.left)) as usize];
        assert_eq!((at(1, 1), at(4, 2), at(0, 1), at(5, 2)), (255, 255, 0, 0));
    }

    #[test]
    fn stroke_masks_union_without_filling_font_counters() {
        let mut outer = Outline::default();
        outer.add_rect(64, 64, 5 * 64, 5 * 64);
        let mut inner = Outline::default();
        inner.add_rect(2 * 64, 4 * 64, 4 * 64, 2 * 64);
        let at = |bm: &Bitmap, x: i32, y: i32| bm.buffer[(y - bm.top) as usize * bm.stride + (x - bm.left) as usize];
        let stroke = outline_to_bitmap(Some(&outer), Some(&inner)).unwrap();
        assert_eq!(at(&stroke, 3, 3), 255, "separate stroke outlines form a union");
        outer.points.extend(inner.points);
        outer.segments.extend(inner.segments);
        let glyph = outline_to_bitmap(Some(&outer), None).unwrap();
        assert_eq!((at(&glyph, 1, 3), at(&glyph, 3, 3)), (255, 0), "a glyph's counter remains empty");
    }

    #[test]
    fn the_cascade_blur_keeps_the_mass_and_spreads_it() {
        let mut bm = Bitmap::alloc(9, 9).unwrap();
        bm.buffer[4 * 9 + 4] = 255;
        let before: u32 = bm.buffer.iter().map(|&v| u32::from(v)).sum();
        assert!(gaussian_blur(&mut bm, 2.0, 2.0));
        let after: u32 = bm.buffer.iter().map(|&v| u32::from(v)).sum();
        assert!(after.abs_diff(before) < 40, "{before} {after}");
        let peak = *bm.buffer.iter().max().unwrap();
        assert!(peak < 64, "{peak}");
    }

    #[test]
    fn frexp_matches_c() {
        assert_eq!(frexp(1.0), (0.5, 1));
        assert_eq!(frexp(0.75), (0.75, 0));
        assert_eq!(frexp(3.0), (0.75, 2));
    }
}

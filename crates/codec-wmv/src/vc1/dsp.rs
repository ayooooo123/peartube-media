//! VC-1 DSP: inverse transforms, overlap smoothing, in-loop deblocking,
//! bicubic ("mspel") luma MC, bilinear chroma MC and the half-pel ops.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vc1dsp.c`,
//! `h264chroma_template.c` (8-bit `put/avg_h264_chroma_mc8/4`), `hpeldsp.c`
//! / `hpel_template.c` (`put/avg[_no_rnd]_pixels*`) and the
//! `put_pixels_clamped` helpers of `idctdsp.c`. LGPL-2.1-or-later.
//!
//! All arithmetic mirrors the C code, using wrapping operations where the C
//! code relies on 32-bit / 16-bit truncation.

#[inline(always)]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ───────────────────────── inverse transforms ─────────────────────────

/// `vc1_inv_trans_8x8_c`: in-place.
pub fn inv_trans_8x8(block: &mut [i16; 64]) {
    let mut temp = [0i16; 64];
    for i in 0..8 {
        let s = |k: usize| block[i + k] as i32;
        let mut t1 = 12i32.wrapping_mul(s(0).wrapping_add(s(32))).wrapping_add(4);
        let mut t2 = 12i32.wrapping_mul(s(0).wrapping_sub(s(32))).wrapping_add(4);
        let mut t3 = (16 * s(16)).wrapping_add(6 * s(48));
        let mut t4 = (6 * s(16)).wrapping_sub(16 * s(48));
        let t5 = t1.wrapping_add(t3);
        let t6 = t2.wrapping_add(t4);
        let t7 = t2.wrapping_sub(t4);
        let t8 = t1.wrapping_sub(t3);
        t1 = (16 * s(8)).wrapping_add(15 * s(24)).wrapping_add(9 * s(40)).wrapping_add(4 * s(56));
        t2 = (15 * s(8)).wrapping_sub(4 * s(24)).wrapping_sub(16 * s(40)).wrapping_sub(9 * s(56));
        t3 = (9 * s(8)).wrapping_sub(16 * s(24)).wrapping_add(4 * s(40)).wrapping_add(15 * s(56));
        t4 = (4 * s(8)).wrapping_sub(9 * s(24)).wrapping_add(15 * s(40)).wrapping_sub(16 * s(56));
        let d = &mut temp[i * 8..i * 8 + 8];
        d[0] = (t5.wrapping_add(t1) >> 3) as i16;
        d[1] = (t6.wrapping_add(t2) >> 3) as i16;
        d[2] = (t7.wrapping_add(t3) >> 3) as i16;
        d[3] = (t8.wrapping_add(t4) >> 3) as i16;
        d[4] = (t8.wrapping_sub(t4) >> 3) as i16;
        d[5] = (t7.wrapping_sub(t3) >> 3) as i16;
        d[6] = (t6.wrapping_sub(t2) >> 3) as i16;
        d[7] = (t5.wrapping_sub(t1) >> 3) as i16;
    }
    for i in 0..8 {
        let s = |k: usize| temp[i + k] as i32;
        let mut t1 = 12i32.wrapping_mul(s(0).wrapping_add(s(32))).wrapping_add(64);
        let mut t2 = 12i32.wrapping_mul(s(0).wrapping_sub(s(32))).wrapping_add(64);
        let mut t3 = (16 * s(16)).wrapping_add(6 * s(48));
        let mut t4 = (6 * s(16)).wrapping_sub(16 * s(48));
        let t5 = t1.wrapping_add(t3);
        let t6 = t2.wrapping_add(t4);
        let t7 = t2.wrapping_sub(t4);
        let t8 = t1.wrapping_sub(t3);
        t1 = (16 * s(8)).wrapping_add(15 * s(24)).wrapping_add(9 * s(40)).wrapping_add(4 * s(56));
        t2 = (15 * s(8)).wrapping_sub(4 * s(24)).wrapping_sub(16 * s(40)).wrapping_sub(9 * s(56));
        t3 = (9 * s(8)).wrapping_sub(16 * s(24)).wrapping_add(4 * s(40)).wrapping_add(15 * s(56));
        t4 = (4 * s(8)).wrapping_sub(9 * s(24)).wrapping_add(15 * s(40)).wrapping_sub(16 * s(56));
        block[i] = (t5.wrapping_add(t1) >> 7) as i16;
        block[i + 8] = (t6.wrapping_add(t2) >> 7) as i16;
        block[i + 16] = (t7.wrapping_add(t3) >> 7) as i16;
        block[i + 24] = (t8.wrapping_add(t4) >> 7) as i16;
        block[i + 32] = (t8.wrapping_sub(t4).wrapping_add(1) >> 7) as i16;
        block[i + 40] = (t7.wrapping_sub(t3).wrapping_add(1) >> 7) as i16;
        block[i + 48] = (t6.wrapping_sub(t2).wrapping_add(1) >> 7) as i16;
        block[i + 56] = (t5.wrapping_sub(t1).wrapping_add(1) >> 7) as i16;
    }
}

/// `vc1_inv_trans_8x8_dc_c`.
pub fn inv_trans_8x8_dc(dest: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (3 * dc + 16) >> 5;
    add_dc(dest, off, stride, 8, 8, dc);
}

fn add_dc(dest: &mut [u8], off: usize, stride: usize, w: usize, h: usize, dc: i32) {
    for r in 0..h {
        for d in &mut dest[off + r * stride..off + r * stride + w] {
            *d = clip_u8(*d as i32 + dc);
        }
    }
}

/// `vc1_inv_trans_8x4_dc_c` (8 wide, 4 tall).
pub fn inv_trans_8x4_dc(dest: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (17 * dc + 64) >> 7;
    add_dc(dest, off, stride, 8, 4, dc);
}

/// `vc1_inv_trans_4x8_dc_c` (4 wide, 8 tall).
pub fn inv_trans_4x8_dc(dest: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (12 * dc + 64) >> 7;
    add_dc(dest, off, stride, 4, 8, dc);
}

/// `vc1_inv_trans_4x4_dc_c`.
pub fn inv_trans_4x4_dc(dest: &mut [u8], off: usize, stride: usize, block: &[i16]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (17 * dc + 64) >> 7;
    add_dc(dest, off, stride, 4, 4, dc);
}

/// `vc1_inv_trans_8x4_c`: `block` holds the 8x4 sub-block in rows 0..4
/// (row stride 8); the result is added to `dest`.
pub fn inv_trans_8x4(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..4 {
        let s = |k: usize| block[i * 8 + k] as i32;
        let mut t1 = (12i32.wrapping_mul(s(0).wrapping_add(s(4)))).wrapping_add(4);
        let mut t2 = (12i32.wrapping_mul(s(0).wrapping_sub(s(4)))).wrapping_add(4);
        let mut t3 = (16 * s(2)).wrapping_add(6 * s(6));
        let mut t4 = (6 * s(2)).wrapping_sub(16 * s(6));
        let t5 = t1.wrapping_add(t3);
        let t6 = t2.wrapping_add(t4);
        let t7 = t2.wrapping_sub(t4);
        let t8 = t1.wrapping_sub(t3);
        t1 = (16 * s(1)).wrapping_add(15 * s(3)).wrapping_add(9 * s(5)).wrapping_add(4 * s(7));
        t2 = (15 * s(1)).wrapping_sub(4 * s(3)).wrapping_sub(16 * s(5)).wrapping_sub(9 * s(7));
        t3 = (9 * s(1)).wrapping_sub(16 * s(3)).wrapping_add(4 * s(5)).wrapping_add(15 * s(7));
        t4 = (4 * s(1)).wrapping_sub(9 * s(3)).wrapping_add(15 * s(5)).wrapping_sub(16 * s(7));
        let d = &mut block[i * 8..i * 8 + 8];
        d[0] = (t5.wrapping_add(t1) >> 3) as i16;
        d[1] = (t6.wrapping_add(t2) >> 3) as i16;
        d[2] = (t7.wrapping_add(t3) >> 3) as i16;
        d[3] = (t8.wrapping_add(t4) >> 3) as i16;
        d[4] = (t8.wrapping_sub(t4) >> 3) as i16;
        d[5] = (t7.wrapping_sub(t3) >> 3) as i16;
        d[6] = (t6.wrapping_sub(t2) >> 3) as i16;
        d[7] = (t5.wrapping_sub(t1) >> 3) as i16;
    }
    for i in 0..8 {
        let s = |k: usize| block[i + k] as i32;
        let t1 = (17i32.wrapping_mul(s(0).wrapping_add(s(16)))).wrapping_add(64);
        let t2 = (17i32.wrapping_mul(s(0).wrapping_sub(s(16)))).wrapping_add(64);
        let t3 = (22 * s(8)).wrapping_add(10 * s(24));
        let t4 = (22 * s(24)).wrapping_sub(10 * s(8));
        let v = [
            t1.wrapping_add(t3) >> 7,
            t2.wrapping_sub(t4) >> 7,
            t2.wrapping_add(t4) >> 7,
            t1.wrapping_sub(t3) >> 7,
        ];
        for (r, &x) in v.iter().enumerate() {
            let p = &mut dest[off + r * stride + i];
            *p = clip_u8((*p as i32).wrapping_add(x));
        }
    }
}

/// `vc1_inv_trans_4x8_c`: 4 wide (columns 0..4 of the 8-stride block).
pub fn inv_trans_4x8(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..8 {
        let s = |k: usize| block[i * 8 + k] as i32;
        let t1 = (17i32.wrapping_mul(s(0).wrapping_add(s(2)))).wrapping_add(4);
        let t2 = (17i32.wrapping_mul(s(0).wrapping_sub(s(2)))).wrapping_add(4);
        let t3 = (22 * s(1)).wrapping_add(10 * s(3));
        let t4 = (22 * s(3)).wrapping_sub(10 * s(1));
        let d = &mut block[i * 8..i * 8 + 4];
        d[0] = (t1.wrapping_add(t3) >> 3) as i16;
        d[1] = (t2.wrapping_sub(t4) >> 3) as i16;
        d[2] = (t2.wrapping_add(t4) >> 3) as i16;
        d[3] = (t1.wrapping_sub(t3) >> 3) as i16;
    }
    for i in 0..4 {
        let s = |k: usize| block[i + k] as i32;
        let mut t1 = (12i32.wrapping_mul(s(0).wrapping_add(s(32)))).wrapping_add(64);
        let mut t2 = (12i32.wrapping_mul(s(0).wrapping_sub(s(32)))).wrapping_add(64);
        let mut t3 = (16 * s(16)).wrapping_add(6 * s(48));
        let mut t4 = (6 * s(16)).wrapping_sub(16 * s(48));
        let t5 = t1.wrapping_add(t3);
        let t6 = t2.wrapping_add(t4);
        let t7 = t2.wrapping_sub(t4);
        let t8 = t1.wrapping_sub(t3);
        t1 = (16 * s(8)).wrapping_add(15 * s(24)).wrapping_add(9 * s(40)).wrapping_add(4 * s(56));
        t2 = (15 * s(8)).wrapping_sub(4 * s(24)).wrapping_sub(16 * s(40)).wrapping_sub(9 * s(56));
        t3 = (9 * s(8)).wrapping_sub(16 * s(24)).wrapping_add(4 * s(40)).wrapping_add(15 * s(56));
        t4 = (4 * s(8)).wrapping_sub(9 * s(24)).wrapping_add(15 * s(40)).wrapping_sub(16 * s(56));
        let v = [
            t5.wrapping_add(t1) >> 7,
            t6.wrapping_add(t2) >> 7,
            t7.wrapping_add(t3) >> 7,
            t8.wrapping_add(t4) >> 7,
            t8.wrapping_sub(t4).wrapping_add(1) >> 7,
            t7.wrapping_sub(t3).wrapping_add(1) >> 7,
            t6.wrapping_sub(t2).wrapping_add(1) >> 7,
            t5.wrapping_sub(t1).wrapping_add(1) >> 7,
        ];
        for (r, &x) in v.iter().enumerate() {
            let p = &mut dest[off + r * stride + i];
            *p = clip_u8((*p as i32).wrapping_add(x));
        }
    }
}

/// `vc1_inv_trans_4x4_c`.
pub fn inv_trans_4x4(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16]) {
    for i in 0..4 {
        let s = |k: usize| block[i * 8 + k] as i32;
        let t1 = (17i32.wrapping_mul(s(0).wrapping_add(s(2)))).wrapping_add(4);
        let t2 = (17i32.wrapping_mul(s(0).wrapping_sub(s(2)))).wrapping_add(4);
        let t3 = (22 * s(1)).wrapping_add(10 * s(3));
        let t4 = (22 * s(3)).wrapping_sub(10 * s(1));
        let d = &mut block[i * 8..i * 8 + 4];
        d[0] = (t1.wrapping_add(t3) >> 3) as i16;
        d[1] = (t2.wrapping_sub(t4) >> 3) as i16;
        d[2] = (t2.wrapping_add(t4) >> 3) as i16;
        d[3] = (t1.wrapping_sub(t3) >> 3) as i16;
    }
    for i in 0..4 {
        let s = |k: usize| block[i + k] as i32;
        let t1 = (17i32.wrapping_mul(s(0).wrapping_add(s(16)))).wrapping_add(64);
        let t2 = (17i32.wrapping_mul(s(0).wrapping_sub(s(16)))).wrapping_add(64);
        let t3 = (22 * s(8)).wrapping_add(10 * s(24));
        let t4 = (22 * s(24)).wrapping_sub(10 * s(8));
        let v = [
            t1.wrapping_add(t3) >> 7,
            t2.wrapping_sub(t4) >> 7,
            t2.wrapping_add(t4) >> 7,
            t1.wrapping_sub(t3) >> 7,
        ];
        for (r, &x) in v.iter().enumerate() {
            let p = &mut dest[off + r * stride + i];
            *p = clip_u8((*p as i32).wrapping_add(x));
        }
    }
}

/// `put_pixels_clamped_c`.
pub fn put_pixels_clamped(block: &[i16; 64], dest: &mut [u8], off: usize, stride: usize) {
    for r in 0..8 {
        for c in 0..8 {
            dest[off + r * stride + c] = clip_u8(block[r * 8 + c] as i32);
        }
    }
}

/// `put_signed_pixels_clamped_c`.
pub fn put_signed_pixels_clamped(block: &[i16; 64], dest: &mut [u8], off: usize, stride: usize) {
    for r in 0..8 {
        for c in 0..8 {
            let v = block[r * 8 + c] as i32;
            dest[off + r * stride + c] = if v < -128 {
                0
            } else if v > 127 {
                255
            } else {
                (v + 128) as u8
            };
        }
    }
}

/// `add_pixels_clamped_c`.
pub fn add_pixels_clamped(block: &[i16; 64], dest: &mut [u8], off: usize, stride: usize) {
    for r in 0..8 {
        for c in 0..8 {
            let p = &mut dest[off + r * stride + c];
            *p = clip_u8(*p as i32 + block[r * 8 + c] as i32);
        }
    }
}

// ───────────────────────── overlap smoothing ─────────────────────────

/// `vc1_v_s_overlap_c` on two coefficient-domain blocks of `buf` (top
/// rows 6,7 at `top`, bottom rows 0,1 at `bottom`).
pub fn v_s_overlap(buf: &mut [i16], top: usize, bottom: usize) {
    let mut rnd1 = 4;
    let mut rnd2 = 3;
    for i in 0..8 {
        let a = buf[top + 48 + i] as i32;
        let b = buf[top + 56 + i] as i32;
        let c = buf[bottom + i] as i32;
        let d = buf[bottom + 8 + i] as i32;
        let d1 = a - d;
        let d2 = a - d + b - c;
        buf[top + 48 + i] = ((a * 8 - d1 + rnd1) >> 3) as i16;
        buf[top + 56 + i] = ((b * 8 - d2 + rnd2) >> 3) as i16;
        buf[bottom + i] = ((c * 8 + d2 + rnd1) >> 3) as i16;
        buf[bottom + 8 + i] = ((d * 8 + d1 + rnd2) >> 3) as i16;
        rnd2 = 7 - rnd2;
        rnd1 = 7 - rnd1;
    }
}

/// `vc1_h_s_overlap_c` on `buf`: `left`/`right` are offsets of the first
/// coefficient of each (possibly half-) block.
pub fn h_s_overlap(buf: &mut [i16], left: usize, right: usize, left_stride: usize, right_stride: usize, flags: i32) {
    let mut rnd1 = if flags & 2 != 0 { 3 } else { 4 };
    let mut rnd2 = 7 - rnd1;
    for i in 0..8 {
        let l = left + i * left_stride;
        let r = right + i * right_stride;
        let a = buf[l + 6] as i32;
        let b = buf[l + 7] as i32;
        let c = buf[r] as i32;
        let d = buf[r + 1] as i32;
        let d1 = a - d;
        let d2 = a - d + b - c;
        buf[l + 6] = ((a * 8 - d1 + rnd1) >> 3) as i16;
        buf[l + 7] = ((b * 8 - d2 + rnd2) >> 3) as i16;
        buf[r] = ((c * 8 + d2 + rnd1) >> 3) as i16;
        buf[r + 1] = ((d * 8 + d1 + rnd2) >> 3) as i16;
        if flags & 1 != 0 {
            rnd2 = 7 - rnd2;
            rnd1 = 7 - rnd1;
        }
    }
}

// ───────────────────────── loop filter ─────────────────────────

/// `vc1_filter_line`: filters across the edge between `p - stride` and `p`.
#[inline]
fn filter_line(src: &mut [u8], p: isize, stride: isize, pq: i32) -> bool {
    let g = |s: &[u8], k: isize| s[(p + k * stride) as usize] as i32;
    let mut a0 = (2 * (g(src, -2) - g(src, 1)) - 5 * (g(src, -1) - g(src, 0)) + 4) >> 3;
    let a0_sign = a0 >> 31;
    a0 = (a0 ^ a0_sign) - a0_sign;
    if a0 < pq {
        let a1 = ((2 * (g(src, -4) - g(src, -1)) - 5 * (g(src, -3) - g(src, -2)) + 4) >> 3).abs();
        let a2 = ((2 * (g(src, 0) - g(src, 3)) - 5 * (g(src, 1) - g(src, 2)) + 4) >> 3).abs();
        if a1 < a0 || a2 < a0 {
            let mut clip = g(src, -1) - g(src, 0);
            let clip_sign = clip >> 31;
            clip = ((clip ^ clip_sign) - clip_sign) >> 1;
            if clip != 0 {
                let a3 = a1.min(a2);
                let mut d = (5 * (a0 - a3)) >> 3;
                if (a0_sign ^ clip_sign) != 0 {
                    d = d.min(clip);
                    d = (d ^ clip_sign) - clip_sign;
                    let i1 = (p - stride) as usize;
                    let i0 = p as usize;
                    src[i1] = clip_u8(src[i1] as i32 - d);
                    src[i0] = clip_u8(src[i0] as i32 + d);
                }
                return true;
            }
        }
    }
    false
}

/// `vc1_loop_filter`: `step` walks along the edge, `stride` across it.
pub fn loop_filter(src: &mut [u8], off: isize, step: isize, stride: isize, len: usize, pq: i32) {
    let mut p = off;
    let mut i = 0;
    while i < len {
        if filter_line(src, p + 2 * step, stride, pq) {
            filter_line(src, p, stride, pq);
            filter_line(src, p + step, stride, pq);
            filter_line(src, p + 3 * step, stride, pq);
        }
        p += step * 4;
        i += 4;
    }
}

/// `vc1_v_loop_filter{4,8,16}_c`: horizontal edge above `off`.
pub fn v_loop_filter(src: &mut [u8], off: isize, stride: isize, len: usize, pq: i32) {
    loop_filter(src, off, 1, stride, len, pq);
}

/// `vc1_h_loop_filter{4,8,16}_c`: vertical edge left of `off`.
pub fn h_loop_filter(src: &mut [u8], off: isize, stride: isize, len: usize, pq: i32) {
    loop_filter(src, off, stride, 1, len, pq);
}

// ───────────────────────── luma MC ─────────────────────────

/// Source for MC: `src[off + y * stride + x]`.
#[derive(Clone, Copy)]
pub struct Src<'a> {
    pub data: &'a [u8],
    pub off: isize,
    pub stride: isize,
}

impl Src<'_> {
    #[inline(always)]
    fn at(&self, x: isize, y: isize) -> i32 {
        self.data[(self.off + y * self.stride + x) as usize] as i32
    }
}

#[inline(always)]
fn ver_filter_16bits(s: &Src, x: isize, y: isize, mode: i32) -> i32 {
    match mode {
        1 => -4 * s.at(x, y - 1) + 53 * s.at(x, y) + 18 * s.at(x, y + 1) - 3 * s.at(x, y + 2),
        2 => -s.at(x, y - 1) + 9 * s.at(x, y) + 9 * s.at(x, y + 1) - s.at(x, y + 2),
        3 => -3 * s.at(x, y - 1) + 18 * s.at(x, y) + 53 * s.at(x, y + 1) - 4 * s.at(x, y + 2),
        _ => 0,
    }
}

#[inline(always)]
fn hor_filter_16bits(t: &[i16], i: usize, mode: i32) -> i32 {
    let g = |k: usize| t[k] as i32;
    match mode {
        1 => -4 * g(i - 1) + 53 * g(i) + 18 * g(i + 1) - 3 * g(i + 2),
        2 => -g(i - 1) + 9 * g(i) + 9 * g(i + 1) - g(i + 2),
        3 => -3 * g(i - 1) + 18 * g(i) + 53 * g(i + 1) - 4 * g(i + 2),
        _ => 0,
    }
}

/// `vc1_mspel_filter` along direction (dx, dy).
#[inline(always)]
fn mspel_filter(s: &Src, x: isize, y: isize, dx: isize, dy: isize, mode: i32, r: i32) -> i32 {
    match mode {
        0 => s.at(x, y),
        1 => (-4 * s.at(x - dx, y - dy) + 53 * s.at(x, y) + 18 * s.at(x + dx, y + dy) - 3 * s.at(x + 2 * dx, y + 2 * dy)
            + 32
            - r)
            >> 6,
        2 => (-s.at(x - dx, y - dy) + 9 * s.at(x, y) + 9 * s.at(x + dx, y + dy) - s.at(x + 2 * dx, y + 2 * dy) + 8 - r) >> 4,
        _ => (-3 * s.at(x - dx, y - dy) + 18 * s.at(x, y) + 53 * s.at(x + dx, y + dy) - 4 * s.at(x + 2 * dx, y + 2 * dy)
            + 32
            - r)
            >> 6,
    }
}

/// `put_/avg_vc1_mspel_mc` (8x8 and 16x16) for `dxy = hmode + 4 * vmode`.
#[allow(clippy::too_many_arguments)]
pub fn vc1_mspel_mc(dst: &mut [u8], doff: usize, dstride: usize, src: &Src, n: usize, dxy: usize, rnd: i32, avg: bool) {
    let hmode = (dxy & 3) as i32;
    let vmode = (dxy >> 2) as i32;
    let op = |d: &mut u8, v: i32| {
        if avg {
            *d = ((*d as i32 + clip_u8(v) as i32 + 1) >> 1) as u8;
        } else {
            *d = clip_u8(v);
        }
    };
    if hmode == 0 && vmode == 0 {
        // put/avg_pixels{8x8,16x16}_c
        for y in 0..n {
            for x in 0..n {
                let v = src.at(x as isize, y as isize);
                let d = &mut dst[doff + y * dstride + x];
                if avg {
                    *d = ((*d as i32 + v + 1) >> 1) as u8;
                } else {
                    *d = v as u8;
                }
            }
        }
        return;
    }
    if vmode != 0 {
        if hmode != 0 {
            const SHIFT_VALUE: [i32; 4] = [0, 5, 1, 5];
            let shift = (SHIFT_VALUE[hmode as usize] + SHIFT_VALUE[vmode as usize]) >> 1;
            let w = n + 3;
            let mut tmp = [0i16; 19 * 16];
            let r = (1 << (shift - 1)) + rnd - 1;
            for j in 0..n {
                for i in 0..w {
                    tmp[j * w + i] =
                        ((ver_filter_16bits(src, i as isize - 1, j as isize, vmode) + r) >> shift) as i16;
                }
            }
            let r = 64 - rnd;
            for j in 0..n {
                for i in 0..n {
                    let v = (hor_filter_16bits(&tmp[j * w..], i + 1, hmode) + r) >> 7;
                    op(&mut dst[doff + j * dstride + i], v);
                }
            }
            return;
        }
        let r = 1 - rnd;
        for j in 0..n {
            for i in 0..n {
                let v = mspel_filter(src, i as isize, j as isize, 0, 1, vmode, r);
                op(&mut dst[doff + j * dstride + i], v);
            }
        }
        return;
    }
    for j in 0..n {
        for i in 0..n {
            let v = mspel_filter(src, i as isize, j as isize, 1, 0, hmode, rnd);
            op(&mut dst[doff + j * dstride + i], v);
        }
    }
}

/// hpeldsp `put/avg[_no_rnd]_pixels{8,16}[_x2,_y2,_xy2]`.
#[allow(clippy::too_many_arguments)]
pub fn hpel(dst: &mut [u8], doff: usize, dstride: usize, src: &Src, n: usize, dxy: usize, no_rnd: bool, avg: bool) {
    let r1 = if no_rnd { 0 } else { 1 };
    let r2 = if no_rnd { 1 } else { 2 };
    for y in 0..n {
        for x in 0..n {
            let (xi, yi) = (x as isize, y as isize);
            let v = match dxy {
                0 => src.at(xi, yi),
                1 => (src.at(xi, yi) + src.at(xi + 1, yi) + r1) >> 1,
                2 => (src.at(xi, yi) + src.at(xi, yi + 1) + r1) >> 1,
                _ => (src.at(xi, yi) + src.at(xi + 1, yi) + src.at(xi, yi + 1) + src.at(xi + 1, yi + 1) + r2) >> 2,
            };
            let d = &mut dst[doff + y * dstride + x];
            if avg {
                *d = ((*d as i32 + v + 1) >> 1) as u8;
            } else {
                *d = v as u8;
            }
        }
    }
}

// ───────────────────────── chroma MC ─────────────────────────

/// `put/avg_h264_chroma_mc{8,4}` (`no_rnd = false`) and
/// `put/avg_no_rnd_vc1_chroma_mc{8,4}` (`no_rnd = true`): `w` wide, `h` rows,
/// eighth-pel (`x`, `y`).
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(
    dst: &mut [u8],
    doff: usize,
    dstride: usize,
    src: &Src,
    w: usize,
    h: usize,
    x: i32,
    y: i32,
    no_rnd: bool,
    avg: bool,
) {
    let a = (8 - x) * (8 - y);
    let b = x * (8 - y);
    let c = (8 - x) * y;
    let d = x * y;
    let bias = if no_rnd { 32 - 4 } else { 32 };
    for j in 0..h {
        let jy = j as isize;
        for i in 0..w {
            let ix = i as isize;
            let mut sum = a * src.at(ix, jy);
            if b != 0 {
                sum += b * src.at(ix + 1, jy);
            }
            if c != 0 {
                sum += c * src.at(ix, jy + 1);
            }
            if d != 0 {
                sum += d * src.at(ix + 1, jy + 1);
            }
            let v = (sum + bias) >> 6;
            let p = &mut dst[doff + j * dstride + i];
            if avg {
                *p = ((*p as i32 + v + 1) >> 1) as u8;
            } else {
                *p = v as u8;
            }
        }
    }
}

// ───────────────────────── start codes ─────────────────────────

/// `vc1_unescape_buffer`.
pub fn unescape_buffer(src: &[u8]) -> Vec<u8> {
    let size = src.len();
    if size < 4 {
        return src.to_vec();
    }
    let mut dst = Vec::with_capacity(size);
    let mut i = 0;
    while i < size {
        if src[i] == 3 && i >= 2 && src[i - 1] == 0 && src[i - 2] == 0 && i < size - 1 && src[i + 1] < 4 {
            dst.push(src[i + 1]);
            i += 2;
        } else {
            dst.push(src[i]);
            i += 1;
        }
    }
    dst
}

/// `find_next_marker`: index of the next `00 00 01 xx` marker at or after
/// `from`, or `buf.len()`.
pub fn find_next_marker(buf: &[u8], from: usize) -> usize {
    if buf.len() < from + 4 {
        return buf.len();
    }
    let mut i = from;
    while i + 3 < buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            return i;
        }
        i += 1;
    }
    buf.len()
}

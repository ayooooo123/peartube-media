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
#[inline(always)]
fn filter_line(src: &mut [u8], p: isize, stride: isize, pq: i32) -> bool {
    let at = |k: isize| (p + k * stride) as usize;
    let (m2, m1, z0, p1) = (src[at(-2)] as i32, src[at(-1)] as i32, src[at(0)] as i32, src[at(1)] as i32);
    let mut a0 = (2 * (m2 - p1) - 5 * (m1 - z0) + 4) >> 3;
    let a0_sign = a0 >> 31;
    a0 = (a0 ^ a0_sign) - a0_sign;
    if a0 < pq {
        let (m4, m3, p2, p3) = (src[at(-4)] as i32, src[at(-3)] as i32, src[at(2)] as i32, src[at(3)] as i32);
        let a1 = ((2 * (m4 - m1) - 5 * (m3 - m2) + 4) >> 3).abs();
        let a2 = ((2 * (z0 - p3) - 5 * (p1 - p2) + 4) >> 3).abs();
        if a1 < a0 || a2 < a0 {
            let mut clip = m1 - z0;
            let clip_sign = clip >> 31;
            clip = ((clip ^ clip_sign) - clip_sign) >> 1;
            if clip != 0 {
                let a3 = a1.min(a2);
                let mut d = (5 * (a0 - a3)) >> 3;
                if (a0_sign ^ clip_sign) != 0 {
                    d = d.min(clip);
                    d = (d ^ clip_sign) - clip_sign;
                    src[at(-1)] = clip_u8(m1 - d);
                    src[at(0)] = clip_u8(z0 + d);
                }
                return true;
            }
        }
    }
    false
}

/// Core 8-tap VC-1 edge filter math on `[m4, m3, m2, m1, z0, p1, p2, p3]`, updating `(m1, z0)` in place.
#[inline(always)]
fn filter_taps(m4: u8, m3: u8, m2: u8, m1: &mut u8, z0: &mut u8, p1: u8, p2: u8, p3: u8, pq: i32) -> bool {
    let (m2i, m1i, z0i, p1i) = (m2 as i32, *m1 as i32, *z0 as i32, p1 as i32);
    let mut a0 = (2 * (m2i - p1i) - 5 * (m1i - z0i) + 4) >> 3;
    let a0_sign = a0 >> 31;
    a0 = (a0 ^ a0_sign) - a0_sign;
    if a0 < pq {
        let (m4i, m3i, p2i, p3i) = (m4 as i32, m3 as i32, p2 as i32, p3 as i32);
        let a1 = ((2 * (m4i - m1i) - 5 * (m3i - m2i) + 4) >> 3).abs();
        let a2 = ((2 * (z0i - p3i) - 5 * (p1i - p2i) + 4) >> 3).abs();
        if a1 < a0 || a2 < a0 {
            let mut clip = m1i - z0i;
            let clip_sign = clip >> 31;
            clip = ((clip ^ clip_sign) - clip_sign) >> 1;
            if clip != 0 {
                let a3 = a1.min(a2);
                let mut d = (5 * (a0 - a3)) >> 3;
                if (a0_sign ^ clip_sign) != 0 {
                    d = d.min(clip);
                    d = (d ^ clip_sign) - clip_sign;
                    *m1 = clip_u8(m1i - d);
                    *z0 = clip_u8(z0i + d);
                }
                return true;
            }
        }
    }
    false
}

/// `vc1_loop_filter`: fallback when `stride <= 0`.
#[inline(always)]
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

/// `vc1_v_loop_filter{4,8,16}_c`: horizontal edge above `off`, split once across the 8 rows so inner taps run bounds-free.
pub fn v_loop_filter(src: &mut [u8], off: isize, stride: isize, len: usize, pq: i32) {
    if stride <= 0 || len == 0 || (len & 3) != 0 || len > stride as usize {
        loop_filter(src, off, 1, stride, len, pq);
        return;
    }
    let s = stride as usize;
    let base = (off - 4 * stride) as usize;
    let Some(win) = src.get_mut(base..base + 7 * s + len) else {
        loop_filter(src, off, 1, stride, len, pq);
        return;
    };
    let (r_m4, rest) = win.split_at_mut(s);
    let (r_m3, rest) = rest.split_at_mut(s);
    let (r_m2, rest) = rest.split_at_mut(s);
    let (r_m1, rest) = rest.split_at_mut(s);
    let (r_0, rest) = rest.split_at_mut(s);
    let (r_p1, rest) = rest.split_at_mut(s);
    let (r_p2, r_p3) = rest.split_at_mut(s);
    let mut x = 0;
    while x < len {
        let i2 = x + 2;
        if filter_taps(r_m4[i2], r_m3[i2], r_m2[i2], &mut r_m1[i2], &mut r_0[i2], r_p1[i2], r_p2[i2], r_p3[i2], pq) {
            for k in [x, x + 1, x + 3] {
                filter_taps(r_m4[k], r_m3[k], r_m2[k], &mut r_m1[k], &mut r_0[k], r_p1[k], r_p2[k], r_p3[k], pq);
            }
        }
        x += 4;
    }
}

/// `vc1_h_loop_filter{4,8,16}_c`: vertical edge left of `off`, taking an exact `[u8; 8]` slice per row.
pub fn h_loop_filter(src: &mut [u8], off: isize, stride: isize, len: usize, pq: i32) {
    if stride < 8 || len == 0 || (len & 3) != 0 || off < 4 {
        loop_filter(src, off, stride, 1, len, pq);
        return;
    }
    let s = stride as usize;
    let base = (off - 4) as usize;
    let mut y = 0;
    while y < len {
        let r2_off = base + (y + 2) * s;
        let hit = if let [m4, m3, m2, m1, z0, p1, p2, p3] = &mut src[r2_off..r2_off + 8] {
            filter_taps(*m4, *m3, *m2, m1, z0, *p1, *p2, *p3, pq)
        } else {
            false
        };
        if hit {
            for ry in [y, y + 1, y + 3] {
                let ro = base + ry * s;
                if let [m4, m3, m2, m1, z0, p1, p2, p3] = &mut src[ro..ro + 8] {
                    filter_taps(*m4, *m3, *m2, m1, z0, *p1, *p2, *p3, pq);
                }
            }
        }
        y += 4;
    }
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
    /// `len` pixels of row `y` starting at column `x`.
    #[inline(always)]
    fn row(&self, x: isize, y: isize, len: usize) -> &[u8] {
        let start = (self.off + y * self.stride + x) as usize;
        &self.data[start..start + len]
    }
}

/// `vc1_mspel_{ver,hor}_filter_16bits` on the four taps `a`..`d`.
#[inline(always)]
fn filter_16bits(a: i32, b: i32, c: i32, d: i32, mode: i32) -> i32 {
    match mode {
        1 => -4 * a + 53 * b + 18 * c - 3 * d,
        2 => -a + 9 * b + 9 * c - d,
        3 => -3 * a + 18 * b + 53 * c - 4 * d,
        _ => 0,
    }
}

/// `vc1_mspel_filter` (modes 1..3) on the four taps `a`..`d`.
#[inline(always)]
fn mspel_filter(a: i32, b: i32, c: i32, d: i32, mode: i32, r: i32) -> i32 {
    match mode {
        1 => (-4 * a + 53 * b + 18 * c - 3 * d + 32 - r) >> 6,
        2 => (-a + 9 * b + 9 * c - d + 8 - r) >> 4,
        _ => (-3 * a + 18 * b + 53 * c - 4 * d + 32 - r) >> 6,
    }
}

/// `op_put` / `op_avg` of the mspel functions.
#[inline(always)]
fn mspel_op(d: &mut u8, v: i32, avg: bool) {
    if avg {
        *d = ((*d as i32 + clip_u8(v) as i32 + 1) >> 1) as u8;
    } else {
        *d = clip_u8(v);
    }
}

#[inline(always)]
fn filter_16bits_mode<const MODE: i32>(a: i32, b: i32, c: i32, d: i32) -> i32 {
    match MODE {
        1 => -4 * a + 53 * b + 18 * c - 3 * d,
        2 => -a + 9 * b + 9 * c - d,
        _ => -3 * a + 18 * b + 53 * c - 4 * d,
    }
}

#[inline(always)]
fn mspel_filter_mode<const MODE: i32>(a: i32, b: i32, c: i32, d: i32, r: i32) -> i32 {
    match MODE {
        1 => (-4 * a + 53 * b + 18 * c - 3 * d + 32 - r) >> 6,
        2 => (-a + 9 * b + 9 * c - d + 8 - r) >> 4,
        _ => (-3 * a + 18 * b + 53 * c - 4 * d + 32 - r) >> 6,
    }
}

#[inline(always)]
fn mspel_write<const AVG: bool>(d: &mut u8, v: i32) {
    let c = v.clamp(0, 255) as u8;
    if AVG {
        *d = ((u16::from(*d) + u16::from(c) + 1) >> 1) as u8;
    } else {
        *d = c;
    }
}

#[inline(always)]
fn mspel_vert_only<const N: usize, const AVG: bool, const VMODE: i32>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src, r: i32,
) {
    for j in 0..N {
        let jy = j as isize;
        let (a, b, c, d) = (src.row(0, jy - 1, N), src.row(0, jy, N), src.row(0, jy + 1, N), src.row(0, jy + 2, N));
        let drow = &mut dst[doff + j * dstride..doff + j * dstride + N];
        for i in 0..N {
            mspel_write::<AVG>(&mut drow[i], mspel_filter_mode::<VMODE>(a[i] as i32, b[i] as i32, c[i] as i32, d[i] as i32, r));
        }
    }
}

#[inline(always)]
fn mspel_horiz_only<const N: usize, const AVG: bool, const HMODE: i32>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src, rnd: i32,
) {
    for j in 0..N {
        let s = src.row(-1, j as isize, N + 3);
        let (s0, s1, s2, s3) = (&s[..N], &s[1..N + 1], &s[2..N + 2], &s[3..N + 3]);
        let drow = &mut dst[doff + j * dstride..doff + j * dstride + N];
        for i in 0..N {
            mspel_write::<AVG>(&mut drow[i], mspel_filter_mode::<HMODE>(s0[i] as i32, s1[i] as i32, s2[i] as i32, s3[i] as i32, rnd));
        }
    }
}

#[inline(always)]
fn mspel_hv<const N: usize, const W: usize, const AVG: bool, const VMODE: i32, const HMODE: i32>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src, rnd: i32,
) {
    const SHIFT_VALUE: [i32; 4] = [0, 5, 1, 5];
    let shift = (SHIFT_VALUE[HMODE as usize] + SHIFT_VALUE[VMODE as usize]) >> 1;
    let rv = (1 << (shift - 1)) + rnd - 1;
    let rh = 64 - rnd;
    for j in 0..N {
        let jy = j as isize;
        let (a, b, c, d) = (src.row(-1, jy - 1, W), src.row(-1, jy, W), src.row(-1, jy + 1, W), src.row(-1, jy + 2, W));
        let mut tmp = [0i16; W];
        for i in 0..W {
            let v = filter_16bits_mode::<VMODE>(a[i] as i32, b[i] as i32, c[i] as i32, d[i] as i32);
            tmp[i] = ((v + rv) >> shift) as i16;
        }
        let (t0, t1, t2, t3) = (&tmp[..N], &tmp[1..N + 1], &tmp[2..N + 2], &tmp[3..N + 3]);
        let drow = &mut dst[doff + j * dstride..doff + j * dstride + N];
        for i in 0..N {
            let v = filter_16bits_mode::<HMODE>(t0[i] as i32, t1[i] as i32, t2[i] as i32, t3[i] as i32);
            mspel_write::<AVG>(&mut drow[i], (v + rh) >> 7);
        }
    }
}

fn vc1_mspel_fixed<const N: usize, const W: usize, const AVG: bool>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src, dxy: usize, rnd: i32,
) {
    let hmode = (dxy & 3) as i32;
    let vmode = (dxy >> 2) as i32;
    match (vmode, hmode) {
        (0, 0) => {
            for y in 0..N {
                let s = src.row(0, y as isize, N);
                let d = &mut dst[doff + y * dstride..doff + y * dstride + N];
                if AVG {
                    for i in 0..N {
                        d[i] = ((u16::from(d[i]) + u16::from(s[i]) + 1) >> 1) as u8;
                    }
                } else {
                    d.copy_from_slice(s);
                }
            }
        }
        (0, 1) => mspel_horiz_only::<N, AVG, 1>(dst, doff, dstride, src, rnd),
        (0, 2) => mspel_horiz_only::<N, AVG, 2>(dst, doff, dstride, src, rnd),
        (0, _) => mspel_horiz_only::<N, AVG, 3>(dst, doff, dstride, src, rnd),
        (1, 0) => mspel_vert_only::<N, AVG, 1>(dst, doff, dstride, src, 1 - rnd),
        (2, 0) => mspel_vert_only::<N, AVG, 2>(dst, doff, dstride, src, 1 - rnd),
        (_, 0) => mspel_vert_only::<N, AVG, 3>(dst, doff, dstride, src, 1 - rnd),
        (1, 1) => mspel_hv::<N, W, AVG, 1, 1>(dst, doff, dstride, src, rnd),
        (1, 2) => mspel_hv::<N, W, AVG, 1, 2>(dst, doff, dstride, src, rnd),
        (1, _) => mspel_hv::<N, W, AVG, 1, 3>(dst, doff, dstride, src, rnd),
        (2, 1) => mspel_hv::<N, W, AVG, 2, 1>(dst, doff, dstride, src, rnd),
        (2, 2) => mspel_hv::<N, W, AVG, 2, 2>(dst, doff, dstride, src, rnd),
        (2, _) => mspel_hv::<N, W, AVG, 2, 3>(dst, doff, dstride, src, rnd),
        (_, 1) => mspel_hv::<N, W, AVG, 3, 1>(dst, doff, dstride, src, rnd),
        (_, 2) => mspel_hv::<N, W, AVG, 3, 2>(dst, doff, dstride, src, rnd),
        (_, _) => mspel_hv::<N, W, AVG, 3, 3>(dst, doff, dstride, src, rnd),
    }
}

/// `put_/avg_vc1_mspel_mc` (8x8 and 16x16) for `dxy = hmode + 4 * vmode`.
#[allow(clippy::too_many_arguments)]
pub fn vc1_mspel_mc(dst: &mut [u8], doff: usize, dstride: usize, src: &Src, n: usize, dxy: usize, rnd: i32, avg: bool) {
    match (n, avg) {
        (16, false) => vc1_mspel_fixed::<16, 19, false>(dst, doff, dstride, src, dxy, rnd),
        (16, true) => vc1_mspel_fixed::<16, 19, true>(dst, doff, dstride, src, dxy, rnd),
        (8, false) => vc1_mspel_fixed::<8, 11, false>(dst, doff, dstride, src, dxy, rnd),
        (8, true) => vc1_mspel_fixed::<8, 11, true>(dst, doff, dstride, src, dxy, rnd),
        _ => {
            let hmode = (dxy & 3) as i32;
            let vmode = (dxy >> 2) as i32;
            if hmode == 0 && vmode == 0 {
                for y in 0..n {
                    let s = src.row(0, y as isize, n);
                    let d = &mut dst[doff + y * dstride..doff + y * dstride + n];
                    if avg {
                        for (d, &s) in d.iter_mut().zip(s) {
                            *d = ((*d as i32 + s as i32 + 1) >> 1) as u8;
                        }
                    } else {
                        d.copy_from_slice(s);
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
                        let jy = j as isize;
                        let (a, b, c, d) = (src.row(-1, jy - 1, w), src.row(-1, jy, w), src.row(-1, jy + 1, w), src.row(-1, jy + 2, w));
                        for (i, t) in tmp[j * w..(j + 1) * w].iter_mut().enumerate() {
                            let v = filter_16bits(a[i] as i32, b[i] as i32, c[i] as i32, d[i] as i32, vmode);
                            *t = ((v + r) >> shift) as i16;
                        }
                    }
                    let r = 64 - rnd;
                    for j in 0..n {
                        let t = &tmp[j * w..(j + 1) * w];
                        let drow = &mut dst[doff + j * dstride..doff + j * dstride + n];
                        for (i, d) in drow.iter_mut().enumerate() {
                            let v = filter_16bits(t[i] as i32, t[i + 1] as i32, t[i + 2] as i32, t[i + 3] as i32, hmode);
                            mspel_op(d, (v + r) >> 7, avg);
                        }
                    }
                    return;
                }
                let r = 1 - rnd;
                for j in 0..n {
                    let jy = j as isize;
                    let (a, b, c, d) = (src.row(0, jy - 1, n), src.row(0, jy, n), src.row(0, jy + 1, n), src.row(0, jy + 2, n));
                    let drow = &mut dst[doff + j * dstride..doff + j * dstride + n];
                    for (i, o) in drow.iter_mut().enumerate() {
                        mspel_op(o, mspel_filter(a[i] as i32, b[i] as i32, c[i] as i32, d[i] as i32, vmode, r), avg);
                    }
                }
                return;
            }
            for j in 0..n {
                let s = src.row(-1, j as isize, n + 3);
                let drow = &mut dst[doff + j * dstride..doff + j * dstride + n];
                for (i, o) in drow.iter_mut().enumerate() {
                    mspel_op(o, mspel_filter(s[i] as i32, s[i + 1] as i32, s[i + 2] as i32, s[i + 3] as i32, hmode, rnd), avg);
                }
            }
        }
    }
}

fn hpel_fixed<const N: usize, const AVG: bool>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src, dxy: usize, no_rnd: bool,
) {
    let r1: u16 = if no_rnd { 0 } else { 1 };
    let r2: u16 = if no_rnd { 1 } else { 2 };
    let write = |d: &mut u8, v: u8| {
        if AVG {
            *d = ((u16::from(*d) + u16::from(v) + 1) >> 1) as u8;
        } else {
            *d = v;
        }
    };
    match dxy {
        0 => {
            for y in 0..N {
                let s = src.row(0, y as isize, N);
                let drow = &mut dst[doff + y * dstride..doff + y * dstride + N];
                if AVG {
                    for i in 0..N {
                        write(&mut drow[i], s[i]);
                    }
                } else {
                    drow.copy_from_slice(s);
                }
            }
        }
        1 => {
            for y in 0..N {
                let s = src.row(0, y as isize, N + 1);
                let (s0, s1) = (&s[..N], &s[1..N + 1]);
                let drow = &mut dst[doff + y * dstride..doff + y * dstride + N];
                for i in 0..N {
                    write(&mut drow[i], ((u16::from(s0[i]) + u16::from(s1[i]) + r1) >> 1) as u8);
                }
            }
        }
        2 => {
            for y in 0..N {
                let (s, t) = (src.row(0, y as isize, N), src.row(0, y as isize + 1, N));
                let drow = &mut dst[doff + y * dstride..doff + y * dstride + N];
                for i in 0..N {
                    write(&mut drow[i], ((u16::from(s[i]) + u16::from(t[i]) + r1) >> 1) as u8);
                }
            }
        }
        _ => {
            for y in 0..N {
                let (s, t) = (src.row(0, y as isize, N + 1), src.row(0, y as isize + 1, N + 1));
                let (s0, s1, t0, t1) = (&s[..N], &s[1..N + 1], &t[..N], &t[1..N + 1]);
                let drow = &mut dst[doff + y * dstride..doff + y * dstride + N];
                for i in 0..N {
                    write(
                        &mut drow[i],
                        ((u16::from(s0[i]) + u16::from(s1[i]) + u16::from(t0[i]) + u16::from(t1[i]) + r2) >> 2) as u8,
                    );
                }
            }
        }
    }
}

/// hpeldsp `put/avg[_no_rnd]_pixels{8,16}[_x2,_y2,_xy2]`.
#[allow(clippy::too_many_arguments)]
pub fn hpel(dst: &mut [u8], doff: usize, dstride: usize, src: &Src, n: usize, dxy: usize, no_rnd: bool, avg: bool) {
    match (n, avg) {
        (16, false) => hpel_fixed::<16, false>(dst, doff, dstride, src, dxy, no_rnd),
        (16, true) => hpel_fixed::<16, true>(dst, doff, dstride, src, dxy, no_rnd),
        (8, false) => hpel_fixed::<8, false>(dst, doff, dstride, src, dxy, no_rnd),
        (8, true) => hpel_fixed::<8, true>(dst, doff, dstride, src, dxy, no_rnd),
        _ => {
            let r1 = if no_rnd { 0 } else { 1 };
            let r2 = if no_rnd { 1 } else { 2 };
            let op = |d: &mut u8, v: i32| {
                if avg {
                    *d = ((*d as i32 + v + 1) >> 1) as u8;
                } else {
                    *d = v as u8;
                }
            };
            for y in 0..n {
                let yi = y as isize;
                let drow = &mut dst[doff + y * dstride..doff + y * dstride + n];
                match dxy {
                    0 => {
                        let s = src.row(0, yi, n);
                        for (d, &s) in drow.iter_mut().zip(s) {
                            op(d, s as i32);
                        }
                    }
                    1 => {
                        let s = src.row(0, yi, n + 1);
                        for (i, d) in drow.iter_mut().enumerate() {
                            op(d, (s[i] as i32 + s[i + 1] as i32 + r1) >> 1);
                        }
                    }
                    2 => {
                        let (s, t) = (src.row(0, yi, n), src.row(0, yi + 1, n));
                        for (i, d) in drow.iter_mut().enumerate() {
                            op(d, (s[i] as i32 + t[i] as i32 + r1) >> 1);
                        }
                    }
                    _ => {
                        let (s, t) = (src.row(0, yi, n + 1), src.row(0, yi + 1, n + 1));
                        for (i, d) in drow.iter_mut().enumerate() {
                            op(d, (s[i] as i32 + s[i + 1] as i32 + t[i] as i32 + t[i + 1] as i32 + r2) >> 2);
                        }
                    }
                }
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
    if avg {
        chroma_rows::<true>(dst, doff, dstride, src, w, h, x, y, no_rnd);
    } else {
        chroma_rows::<false>(dst, doff, dstride, src, w, h, x, y, no_rnd);
    }
}

#[allow(clippy::too_many_arguments)]
fn chroma_rows<const AVG: bool>(
    dst: &mut [u8], doff: usize, dstride: usize, src: &Src,
    w: usize, h: usize, x: i32, y: i32, no_rnd: bool,
) {
    let a = (8 - x) * (8 - y);
    let b = x * (8 - y);
    let c = (8 - x) * y;
    let d = x * y;
    let bias = if no_rnd { 28 } else { 32 };
    let put = |p: &mut u8, sum: i32| {
        let v = (sum + bias) >> 6;
        *p = if AVG { ((i32::from(*p) + v + 1) >> 1) as u8 } else { v as u8 };
    };
    // Select weighted taps once per block. Exact row slices let each inner
    // loop vectorize without per-pixel phase branches or speculative reads.
    match (x == 0, y == 0) {
        (true, true) => {
            for j in 0..h {
                let s = src.row(0, j as isize, w);
                let out = &mut dst[doff + j * dstride..doff + j * dstride + w];
                for (p, &s) in out.iter_mut().zip(s) { put(p, a * i32::from(s)); }
            }
        }
        (false, true) => {
            for j in 0..h {
                let s = src.row(0, j as isize, w + 1);
                let out = &mut dst[doff + j * dstride..doff + j * dstride + w];
                for ((p, &s0), &s1) in out.iter_mut().zip(&s[..w]).zip(&s[1..]) {
                    put(p, a * i32::from(s0) + b * i32::from(s1));
                }
            }
        }
        (true, false) => {
            for j in 0..h {
                let s = src.row(0, j as isize, w);
                let t = src.row(0, j as isize + 1, w);
                let out = &mut dst[doff + j * dstride..doff + j * dstride + w];
                for ((p, &s), &t) in out.iter_mut().zip(s).zip(t) {
                    put(p, a * i32::from(s) + c * i32::from(t));
                }
            }
        }
        (false, false) => {
            for j in 0..h {
                let s = src.row(0, j as isize, w + 1);
                let t = src.row(0, j as isize + 1, w + 1);
                let out = &mut dst[doff + j * dstride..doff + j * dstride + w];
                for ((((p, &s0), &s1), &t0), &t1) in out.iter_mut()
                    .zip(&s[..w]).zip(&s[1..]).zip(&t[..w]).zip(&t[1..])
                {
                    put(p, a * i32::from(s0) + b * i32::from(s1)
                        + c * i32::from(t0) + d * i32::from(t1));
                }
            }
        }
    }
}

// ───────────────────────── start codes ─────────────────────────

/// `vc1_unescape_buffer` — copies bulk unescaped spans instead of pushing one byte at a time.
pub fn unescape_buffer(src: &[u8]) -> Vec<u8> {
    let size = src.len();
    if size < 4 {
        return src.to_vec();
    }
    let mut dst = Vec::with_capacity(size);
    let mut anchor = 0usize;
    let mut i = 2usize;
    while i + 1 < size {
        if src[i] == 3 {
            if src[i - 1] == 0 && src[i - 2] == 0 && src[i + 1] < 4 {
                dst.extend_from_slice(&src[anchor..i]);
                dst.push(src[i + 1]);
                anchor = i + 2;
                i = anchor + 1;
                continue;
            }
            i += 3;
        } else if src[i] == 0 {
            i += 1;
        } else {
            i += 3;
        }
    }
    if anchor == 0 {
        dst.extend_from_slice(src);
    } else if anchor < size {
        dst.extend_from_slice(&src[anchor..]);
    }
    dst
}

/// Unescapes up to `out.len()` bytes onto the stack with zero heap allocation (for B-frame header peeks).
pub fn unescape_prefix(src: &[u8], out: &mut [u8]) -> usize {
    let size = src.len();
    let cap = out.len();
    let mut n = 0usize;
    let mut i = 0usize;
    while i < size && n < cap {
        if src[i] == 3 && i >= 2 && src[i - 1] == 0 && src[i - 2] == 0 && i + 1 < size && src[i + 1] < 4 {
            out[n] = src[i + 1];
            n += 1;
            i += 2;
        } else {
            out[n] = src[i];
            n += 1;
            i += 1;
        }
    }
    n
}

/// `find_next_marker`: index of the next `00 00 01 xx` marker at or after
/// `from`, or `buf.len()`, skipping non-zero runs rapidly.
pub fn find_next_marker(buf: &[u8], from: usize) -> usize {
    let len = buf.len();
    if len < from + 4 {
        return len;
    }
    let end = len - 3;
    let mut i = from;
    while i < end {
        let Some(rel) = buf[i..end].iter().position(|&b| b == 0) else {
            return len;
        };
        i += rel;
        if buf[i + 1] == 0 && buf[i + 2] == 1 {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chroma_phases_preserve_rounding_and_unweighted_edges() {
        for w in [1usize, 4, 8, 13] {
            for h in [1usize, 4, 8] {
                for x in 0..8 {
                    for y in 0..8 {
                        let sw = w + usize::from(x != 0);
                        let sh = h + usize::from(y != 0);
                        let stride = sw + 3;
                        // The final row ends at its last weighted sample.
                        let data: Vec<u8> = (0..5 + (sh - 1) * stride + sw)
                            .map(|i| (i * 73 + i / 7 * 191) as u8).collect();
                        let src = Src { data: &data, off: 5, stride: stride as isize };
                        let ds = w + 5;
                        for no_rnd in [false, true] {
                            for avg in [false, true] {
                                let mut actual: Vec<u8> = (0..7 + h * ds)
                                    .map(|i| (i * 151 + 19) as u8).collect();
                                let mut expected = actual.clone();
                                for row in 0..h {
                                    for col in 0..w {
                                        let mut sum = 0;
                                        for (dx, wx) in [(0, 8 - x), (1, x)] {
                                            for (dy, wy) in [(0, 8 - y), (1, y)] {
                                                if wx * wy != 0 {
                                                    sum += wx * wy * i32::from(data[5 + (row + dy) * stride + col + dx]);
                                                }
                                            }
                                        }
                                        let v = (sum + if no_rnd { 28 } else { 32 }) >> 6;
                                        let p = &mut expected[7 + row * ds + col];
                                        *p = if avg { ((i32::from(*p) + v + 1) >> 1) as u8 } else { v as u8 };
                                    }
                                }
                                chroma_mc(&mut actual, 7, ds, &src, w, h, x, y, no_rnd, avg);
                                assert_eq!(actual, expected, "{w}x{h}, phase {x}/{y}, no_rnd={no_rnd}, avg={avg}");
                            }
                        }
                    }
                }
            }
        }
    }
}

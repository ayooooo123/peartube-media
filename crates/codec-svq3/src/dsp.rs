// Ported from FFmpeg (commit 2da55bf): libavcodec/h264pred_template.c
// (pred4x4_vertical, _horizontal, _dc, _left_dc, _top_dc, _128_dc,
// _down_right; pred16x16_vertical, _horizontal, _dc, _left_dc, _top_dc,
// _128_dc, pred16x16_plane_compat; pred8x8_dc, _left_dc, _top_dc,
// _128_dc), libavcodec/h264pred.c (pred4x4_down_left_svq3_c,
// pred16x16_plane_svq3_c), libavcodec/h264_parse.c
// (ff_h264_check_intra4x4_pred_mode, ff_h264_check_intra_pred_mode),
// libavcodec/hpeldsp.c and pel_template.c (put/avg pixels, _x2, _y2,
// _xy2), libavcodec/tpeldsp.c, libavcodec/videodsp_template.c
// (emulated_edge_mc), libavcodec/h264idct_template.c
// (ff_h264_chroma_dc_dequant_idct) and libavcodec/svq3.c
// (svq3_luma_dc_dequant_idct_c, svq3_add_idct_c,
// init_dequant4_coeff_table).
// License: LGPL-2.1-or-later

//! The pixel operations SVQ3 runs: H.264 intra prediction with SVQ3's
//! own down-left and plane predictors, half-pel and third-pel motion
//! compensation, SVQ3's inverse transforms, and edge emulation. Every
//! function takes a plane, the offset of the block's top-left sample and
//! the plane's stride.

use crate::tables::{DEQUANT4_COEFF_INIT, QUANT_DIV6, QUANT_REM6, SCAN8, SVQ3_DEQUANT_COEFF};

/// H.264 4x4 intra prediction modes (h264pred.h).
pub const VERT_PRED: i8 = 0;
pub const HOR_PRED: i8 = 1;
pub const DC_PRED: i8 = 2;
pub const DIAG_DOWN_LEFT_PRED: i8 = 3;
pub const DIAG_DOWN_RIGHT_PRED: i8 = 4;
pub const LEFT_DC_PRED: i8 = 9;
pub const TOP_DC_PRED: i8 = 10;
pub const DC_128_PRED: i8 = 11;

/// H.264 16x16 and chroma 8x8 intra prediction modes (h264pred.h).
pub const DC_PRED8X8: i32 = 0;
pub const HOR_PRED8X8: i32 = 1;
pub const VERT_PRED8X8: i32 = 2;
pub const PLANE_PRED8X8: i32 = 3;
pub const LEFT_DC_PRED8X8: i32 = 4;
pub const TOP_DC_PRED8X8: i32 = 5;
pub const DC_128_PRED8X8: i32 = 6;

fn fill(p: &mut [u8], at: usize, stride: usize, w: usize, h: usize, v: u8) {
    for y in 0..h {
        p[at + y * stride..at + y * stride + w].fill(v);
    }
}

/// `hpc.pred4x4[mode]` as FFmpeg sets it up for SVQ3. SVQ3's prediction
/// code (svq3_pred_1) and the availability checks produce only the modes
/// below; the down-left predictor is SVQ3's own, which reads no top-right
/// samples.
pub fn pred4x4(mode: i8, p: &mut [u8], at: usize, s: usize) {
    let top = |p: &[u8], i: usize| u32::from(p[at - s + i]);
    let left = |p: &[u8], i: usize| u32::from(p[at - 1 + i * s]);
    match mode {
        VERT_PRED => {
            let mut row = [0u8; 4];
            row.copy_from_slice(&p[at - s..at - s + 4]);
            for y in 0..4 {
                p[at + y * s..at + y * s + 4].copy_from_slice(&row);
            }
        }
        HOR_PRED => {
            for y in 0..4 {
                let v = p[at - 1 + y * s];
                p[at + y * s..at + y * s + 4].fill(v);
            }
        }
        DC_PRED => {
            let dc = (0..4).map(|i| top(p, i) + left(p, i)).sum::<u32>();
            fill(p, at, s, 4, 4, ((dc + 4) >> 3) as u8);
        }
        DIAG_DOWN_LEFT_PRED => {
            // pred4x4_down_left_svq3_c
            let (t1, t2, t3) = (top(p, 1), top(p, 2), top(p, 3));
            let (l1, l2, l3) = (left(p, 1), left(p, 2), left(p, 3));
            fill(p, at, s, 4, 4, ((l3 + t3) >> 1) as u8);
            p[at] = ((l1 + t1) >> 1) as u8;
            let v = ((l2 + t2) >> 1) as u8;
            p[at + 1] = v;
            p[at + s] = v;
        }
        DIAG_DOWN_RIGHT_PRED => {
            let lt = u32::from(p[at - 1 - s]);
            let (t0, t1, t2, t3) = (top(p, 0), top(p, 1), top(p, 2), top(p, 3));
            let (l0, l1, l2, l3) = (left(p, 0), left(p, 1), left(p, 2), left(p, 3));
            let mut set = |x: usize, y: usize, v: u32| p[at + x + y * s] = v as u8;
            set(0, 3, (l3 + 2 * l2 + l1 + 2) >> 2);
            let v = (l2 + 2 * l1 + l0 + 2) >> 2;
            set(0, 2, v);
            set(1, 3, v);
            let v = (l1 + 2 * l0 + lt + 2) >> 2;
            set(0, 1, v);
            set(1, 2, v);
            set(2, 3, v);
            let v = (l0 + 2 * lt + t0 + 2) >> 2;
            set(0, 0, v);
            set(1, 1, v);
            set(2, 2, v);
            set(3, 3, v);
            let v = (lt + 2 * t0 + t1 + 2) >> 2;
            set(1, 0, v);
            set(2, 1, v);
            set(3, 2, v);
            let v = (t0 + 2 * t1 + t2 + 2) >> 2;
            set(2, 0, v);
            set(3, 1, v);
            set(3, 0, (t1 + 2 * t2 + t3 + 2) >> 2);
        }
        LEFT_DC_PRED => {
            let dc = (0..4).map(|i| left(p, i)).sum::<u32>();
            fill(p, at, s, 4, 4, ((dc + 2) >> 2) as u8);
        }
        TOP_DC_PRED => {
            let dc = (0..4).map(|i| top(p, i)).sum::<u32>();
            fill(p, at, s, 4, 4, ((dc + 2) >> 2) as u8);
        }
        // DC_128_PRED, the only other mode SVQ3 produces.
        _ => fill(p, at, s, 4, 4, 128),
    }
}

/// `hpc.pred16x16[mode]` for SVQ3 (the plane predictor is SVQ3's).
/// `mode` comes from `check_intra_pred_mode`, so it is 0..=6.
pub fn pred16x16(mode: i32, p: &mut [u8], at: usize, s: usize) {
    match mode {
        VERT_PRED8X8 => {
            let mut row = [0u8; 16];
            row.copy_from_slice(&p[at - s..at - s + 16]);
            for y in 0..16 {
                p[at + y * s..at + y * s + 16].copy_from_slice(&row);
            }
        }
        HOR_PRED8X8 => {
            for y in 0..16 {
                let v = p[at - 1 + y * s];
                p[at + y * s..at + y * s + 16].fill(v);
            }
        }
        DC_PRED8X8 => {
            let dc: u32 = (0..16).map(|i| u32::from(p[at - 1 + i * s]) + u32::from(p[at - s + i])).sum();
            fill(p, at, s, 16, 16, ((dc + 16) >> 5) as u8);
        }
        PLANE_PRED8X8 => plane16x16_svq3(p, at, s),
        LEFT_DC_PRED8X8 => {
            let dc: u32 = (0..16).map(|i| u32::from(p[at - 1 + i * s])).sum();
            fill(p, at, s, 16, 16, ((dc + 8) >> 4) as u8);
        }
        TOP_DC_PRED8X8 => {
            let dc: u32 = (0..16).map(|i| u32::from(p[at - s + i])).sum();
            fill(p, at, s, 16, 16, ((dc + 8) >> 4) as u8);
        }
        _ => fill(p, at, s, 16, 16, 128),
    }
}

/// pred16x16_plane_compat with `svq3` set.
fn plane16x16_svq3(p: &mut [u8], at: usize, s: usize) {
    let top = |p: &[u8], i: isize| i32::from(p[(at as isize - s as isize + i) as usize]);
    let left = |p: &[u8], i: isize| i32::from(p[(at as isize - 1 + i * s as isize) as usize]);
    let mut h = top(p, 8) - top(p, 6);
    let mut v = left(p, 8) - left(p, 6);
    for k in 2..=8isize {
        h += k as i32 * (top(p, 7 + k) - top(p, 7 - k));
        v += k as i32 * (left(p, 7 + k) - left(p, 7 - k));
    }
    h = (5 * (h / 4)) / 16;
    v = (5 * (v / 4)) / 16;
    // "required for 100% accuracy"
    std::mem::swap(&mut h, &mut v);
    let mut a = 16 * (left(p, 15) + top(p, 15) + 1) - 7 * (v + h);
    for y in 0..16 {
        let mut b = a;
        a += v;
        for x in 0..16 {
            p[at + x + y * s] = (b >> 5).clamp(0, 255) as u8;
            b += h;
        }
    }
}

/// `hpc.pred8x8[mode]` for a chroma block; SVQ3's chroma mode is DC
/// checked against the edges, so 0, 4, 5 or 6.
pub fn pred8x8(mode: i32, p: &mut [u8], at: usize, s: usize) {
    let top = |p: &[u8], i: usize| u32::from(p[at - s + i]);
    let left = |p: &[u8], i: usize| u32::from(p[at - 1 + i * s]);
    match mode {
        DC_PRED8X8 => {
            let (mut dc0, mut dc1, mut dc2) = (0, 0, 0);
            for i in 0..4 {
                dc0 += left(p, i) + top(p, i);
                dc1 += top(p, 4 + i);
                dc2 += left(p, 4 + i);
            }
            fill(p, at, s, 4, 4, ((dc0 + 4) >> 3) as u8);
            fill(p, at + 4, s, 4, 4, ((dc1 + 2) >> 2) as u8);
            fill(p, at + 4 * s, s, 4, 4, ((dc2 + 2) >> 2) as u8);
            fill(p, at + 4 + 4 * s, s, 4, 4, ((dc1 + dc2 + 4) >> 3) as u8);
        }
        LEFT_DC_PRED8X8 => {
            let dc0: u32 = (0..4).map(|i| left(p, i)).sum();
            let dc2: u32 = (4..8).map(|i| left(p, i)).sum();
            fill(p, at, s, 8, 4, ((dc0 + 2) >> 2) as u8);
            fill(p, at + 4 * s, s, 8, 4, ((dc2 + 2) >> 2) as u8);
        }
        TOP_DC_PRED8X8 => {
            let dc0: u32 = (0..4).map(|i| top(p, i)).sum();
            let dc1: u32 = (4..8).map(|i| top(p, i)).sum();
            fill(p, at, s, 4, 8, ((dc0 + 2) >> 2) as u8);
            fill(p, at + 4, s, 4, 8, ((dc1 + 2) >> 2) as u8);
        }
        _ => fill(p, at, s, 8, 8, 128),
    }
}

/// ff_h264_check_intra4x4_pred_mode: the cache's top row and left column
/// switched to modes that need no unavailable edge; false (with the
/// entries before the offending one already switched) when a mode cannot
/// do without one.
pub fn check_intra4x4_pred_mode(cache: &mut [i8; 40], top_samples_available: u32, left_samples_available: u32) -> bool {
    const TOP: [i8; 12] = [-1, 0, LEFT_DC_PRED, -1, -1, -1, -1, -1, 0, 0, 0, 0];
    const LEFT: [i8; 12] = [0, -1, TOP_DC_PRED, 0, -1, -1, -1, 0, -1, DC_128_PRED, 0, 0];
    let scan0 = usize::from(SCAN8[0]);
    if top_samples_available & 0x8000 == 0 {
        for i in 0..4 {
            let status = TOP.get(cache[scan0 + i] as usize).copied().unwrap_or(-1);
            if status < 0 {
                return false;
            } else if status != 0 {
                cache[scan0 + i] = status;
            }
        }
    }
    if left_samples_available & 0x8888 != 0x8888 {
        const MASK: [u32; 4] = [0x8000, 0x2000, 0x80, 0x20];
        for (i, mask) in MASK.into_iter().enumerate() {
            if left_samples_available & mask == 0 {
                let status = LEFT.get(cache[scan0 + 8 * i] as usize).copied().unwrap_or(-1);
                if status < 0 {
                    return false;
                } else if status != 0 {
                    cache[scan0 + 8 * i] = status;
                }
            }
        }
    }
    true
}

/// ff_h264_check_intra_pred_mode for 16x16 luma and chroma: `mode` (0..=3)
/// switched to one that needs no unavailable edge, or None.
pub fn check_intra_pred_mode(top_samples_available: u32, left_samples_available: u32, mode: i32, is_chroma: bool) -> Option<i32> {
    const TOP: [i32; 4] = [LEFT_DC_PRED8X8, 1, -1, -1];
    const LEFT: [i32; 5] = [TOP_DC_PRED8X8, -1, 2, -1, DC_128_PRED8X8];
    const ALZHEIMER_DC_L0T_PRED8X8: i32 = 7;
    if !(0..=3).contains(&mode) {
        return None;
    }
    let mut mode = mode;
    if top_samples_available & 0x8000 == 0 {
        mode = TOP[mode as usize];
        if mode < 0 {
            return None;
        }
    }
    if left_samples_available & 0x8080 != 0x8080 {
        mode = LEFT[mode as usize];
        if mode < 0 {
            return None;
        }
        if is_chroma && left_samples_available & 0x8080 != 0 {
            mode = ALZHEIMER_DC_L0T_PRED8X8 + i32::from(left_samples_available & 0x8000 == 0) + 2 * i32::from(mode == DC_128_PRED8X8);
        }
    }
    Some(mode)
}

/// One block of motion compensation from `src` into `dst`, `w`×`h`:
/// `hdsp.{put,avg}_pixels_tab` (half-pel `dxy` 0..=3) or
/// `tdsp.{put,avg}_tpel_pixels_tab` (third-pel `dxy`: x + 4·y thirds).
/// FFmpeg's C avg_pixels2_xy2 stores its interpolation without averaging
/// with the destination ("FIXME non put"); AArch64 replaces only the 16
/// and 8 wide functions, so that stays.
#[allow(clippy::too_many_arguments)]
pub fn mc(dst: &mut [u8], doff: usize, ds: usize, src: &[u8], soff: usize, ss: usize, w: usize, h: usize, dxy: usize, thirdpel: bool, avg: bool) {
    let store_only = avg && !thirdpel && w == 2 && dxy == 3;
    for i in 0..h {
        let r0 = soff + i * ss;
        let r1 = r0 + ss;
        for j in 0..w {
            let a = i32::from(src[r0 + j]);
            let v = if thirdpel {
                match dxy {
                    0 => a,
                    1 => ((2 * a + i32::from(src[r0 + j + 1]) + 1) * 683) >> 11,
                    2 => ((a + 2 * i32::from(src[r0 + j + 1]) + 1) * 683) >> 11,
                    4 => ((2 * a + i32::from(src[r1 + j]) + 1) * 683) >> 11,
                    8 => ((a + 2 * i32::from(src[r1 + j]) + 1) * 683) >> 11,
                    _ => {
                        let (b, c, d) = (i32::from(src[r0 + j + 1]), i32::from(src[r1 + j]), i32::from(src[r1 + j + 1]));
                        let sum = match dxy {
                            5 => 4 * a + 3 * b + 3 * c + 2 * d,
                            6 => 3 * a + 4 * b + 2 * c + 3 * d,
                            9 => 3 * a + 2 * b + 4 * c + 3 * d,
                            // 10: the only third-pel position left
                            _ => 2 * a + 3 * b + 3 * c + 4 * d,
                        };
                        ((sum + 6) * 2731) >> 15
                    }
                }
            } else {
                match dxy {
                    0 => a,
                    1 => (a + i32::from(src[r0 + j + 1]) + 1) >> 1,
                    2 => (a + i32::from(src[r1 + j]) + 1) >> 1,
                    _ => (a + i32::from(src[r0 + j + 1]) + i32::from(src[r1 + j]) + i32::from(src[r1 + j + 1]) + 2) >> 2,
                }
            };
            let d = &mut dst[doff + i * ds + j];
            *d = if avg && !store_only { ((i32::from(*d) + v + 1) >> 1) as u8 } else { v as u8 };
        }
    }
}

/// emulated_edge_mc: the `bw`×`bh` block at (`sx`, `sy`) of the `w`×`h`
/// picture whose top-left sample is `src[origin]`, samples outside it
/// repeating the nearest edge sample, into `buf` with stride `bw`.
#[allow(clippy::too_many_arguments)]
pub fn emulated_edge(buf: &mut Vec<u8>, src: &[u8], origin: usize, ss: usize, bw: usize, bh: usize, sx: i32, sy: i32, w: i32, h: i32) {
    buf.clear();
    for y in 0..bh as i32 {
        let row = origin + (sy + y).clamp(0, h - 1) as usize * ss;
        for x in 0..bw as i32 {
            buf.push(src[row + (sx + x).clamp(0, w - 1) as usize]);
        }
    }
}

/// svq3_luma_dc_dequant_idct_c: the 4x4 luma DC block into the DC of the
/// 16 luma blocks of `mb`.
pub fn luma_dc_dequant_idct(mb: &mut [i16], input: &[i16; 16], qp: usize) {
    const STRIDE: usize = 16;
    const X_OFFSET: [usize; 4] = [0, STRIDE, 4 * STRIDE, 5 * STRIDE];
    let qmul = SVQ3_DEQUANT_COEFF[qp];
    let mut temp = [0i32; 16];
    for i in 0..4 {
        let x = |k: usize| i32::from(input[4 * i + k]);
        let z0 = 13 * (x(0) + x(2));
        let z1 = 13 * (x(0) - x(2));
        let z2 = 7 * x(1) - 17 * x(3);
        let z3 = 17 * x(1) + 7 * x(3);
        temp[4 * i] = z0 + z3;
        temp[4 * i + 1] = z1 + z2;
        temp[4 * i + 2] = z1 - z2;
        temp[4 * i + 3] = z0 - z3;
    }
    let out = |z: i32| ((z as u32).wrapping_mul(qmul).wrapping_add(0x80000) as i32 >> 20) as i16;
    for (i, offset) in X_OFFSET.into_iter().enumerate() {
        let z0 = 13 * (temp[i] + temp[8 + i]);
        let z1 = 13 * (temp[i] - temp[8 + i]);
        let z2 = 7 * temp[4 + i] - 17 * temp[12 + i];
        let z3 = 17 * temp[4 + i] + 7 * temp[12 + i];
        mb[offset] = out(z0 + z3);
        mb[STRIDE * 2 + offset] = out(z1 + z2);
        mb[STRIDE * 8 + offset] = out(z1 - z2);
        mb[STRIDE * 10 + offset] = out(z0 - z3);
    }
}

/// svq3_add_idct_c: the 4x4 residual `block` (then zeroed) added to the
/// block of `dst` at `at`. `dc` 1 dequantises the DC as luma intra, 2 as
/// chroma.
pub fn add_idct(dst: &mut [u8], at: usize, stride: usize, block: &mut [i16], qp: usize, dc: i32) {
    let qmul = SVQ3_DEQUANT_COEFF[qp] as i32;
    let mut dcv = 0i32;
    if dc != 0 {
        dcv = if dc == 1 {
            169u32.wrapping_mul(1538u32.wrapping_mul(block[0] as i32 as u32)) as i32
        } else {
            169i32.wrapping_mul(qmul.wrapping_mul(i32::from(block[0] >> 3)) / 2)
        };
        block[0] = 0;
    }
    for i in 0..4 {
        let b = |k: usize| i32::from(block[4 * i + k]);
        let z0 = 13 * (b(0) + b(2));
        let z1 = 13 * (b(0) - b(2));
        let z2 = 7 * b(1) - 17 * b(3);
        let z3 = 17 * b(1) + 7 * b(3);
        block[4 * i] = (z0 + z3) as i16;
        block[4 * i + 1] = (z1 + z2) as i16;
        block[4 * i + 2] = (z1 - z2) as i16;
        block[4 * i + 3] = (z0 - z3) as i16;
    }
    let rr = (dcv as u32).wrapping_add(0x80000);
    let res = |z: u32| (z.wrapping_mul(qmul as u32).wrapping_add(rr) as i32) >> 20;
    for i in 0..4 {
        let b = |k: usize| i32::from(block[i + 4 * k]);
        let z0 = (13 * (b(0) + b(2))) as u32;
        let z1 = (13 * (b(0) - b(2))) as u32;
        let z2 = (7 * b(1) - 17 * b(3)) as u32;
        let z3 = (17 * b(1) + 7 * b(3)) as u32;
        for (row, z) in [z0.wrapping_add(z3), z1.wrapping_add(z2), z1.wrapping_sub(z2), z0.wrapping_sub(z3)].into_iter().enumerate() {
            let d = &mut dst[at + i + row * stride];
            *d = (i32::from(*d) + res(z)).clamp(0, 255) as u8;
        }
    }
    block[..16].fill(0);
}

/// ff_h264_chroma_dc_dequant_idct: the 2x2 chroma DC (the DC of the four
/// blocks of `block`, 16 apart) dequantised by `qmul`.
pub fn chroma_dc_dequant_idct(block: &mut [i16], qmul: i32) {
    let (a, b, c, d) = (block[0] as i32 as u32, block[16] as i32 as u32, block[32] as i32 as u32, block[48] as i32 as u32);
    let e = a.wrapping_sub(b);
    let a = a.wrapping_add(b);
    let b = c.wrapping_sub(d);
    let c = c.wrapping_add(d);
    let out = |v: u32| (v.wrapping_mul(qmul as u32) as i32 >> 7) as i16;
    block[0] = out(a.wrapping_add(c));
    block[16] = out(e.wrapping_add(b));
    block[32] = out(a.wrapping_sub(c));
    block[48] = out(e.wrapping_sub(b));
}

/// init_dequant4_coeff_table's entry SVQ3 uses: qP 4, coefficient 0 (the
/// chroma DC quantiser).
pub fn chroma_dc_qmul() -> i32 {
    let q = 4;
    let shift = u32::from(QUANT_DIV6[q]) + 2;
    let idx = usize::from(QUANT_REM6[q]);
    ((u32::from(DEQUANT4_COEFF_INIT[idx][0]) * 16) << shift) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 2-wide half-pel xy2 average of FFmpeg's C path stores the
    /// interpolation; the other widths average with the destination.
    #[test]
    fn avg_xy2_two_wide_stores() {
        let src = [10u8, 20, 30, 40, 50, 60, 70, 80, 90];
        let mut dst = vec![200u8; 9];
        mc(&mut dst, 0, 3, &src, 0, 3, 2, 2, 3, false, true);
        assert_eq!(&dst[..2], &[30, 40], "stored, not averaged");
        let mut dst = vec![200u8; 9];
        mc(&mut dst, 0, 3, &src, 0, 3, 2, 2, 1, false, true);
        assert_eq!(dst[0], ((200 + 15 + 1) >> 1) as u8, "x2 averages");
    }

    #[test]
    fn chroma_dc_quantiser_is_1024() {
        assert_eq!(chroma_dc_qmul(), 1024);
    }
}

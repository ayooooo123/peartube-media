//! Intra prediction used by RV30/RV40: the H.264 4x4, 8x8 (chroma) and
//! 16x16 predictors as FFmpeg configures them for `AV_CODEC_ID_RV40`
//! (8-bit, 4:2:0), including the RV40-specific diagonal, plane and DC
//! variants.
//!
//! Ported from FFmpeg libavcodec/h264pred.c and h264pred_template.c
//! (commit 2da55bf); LGPL-2.1-or-later.
//!
//! Every function predicts the block whose top-left sample is `buf[pos]`
//! in a plane of row pitch `stride`; neighbours are read at negative
//! offsets, which the plane margins keep in bounds.

pub const VERT_PRED: usize = 0;
pub const HOR_PRED: usize = 1;
pub const DC_PRED: usize = 2;
pub const DIAG_DOWN_LEFT_PRED: usize = 3;
pub const DIAG_DOWN_RIGHT_PRED: usize = 4;
pub const VERT_RIGHT_PRED: usize = 5;
pub const HOR_DOWN_PRED: usize = 6;
pub const VERT_LEFT_PRED: usize = 7;
pub const HOR_UP_PRED: usize = 8;
pub const LEFT_DC_PRED: usize = 9;
pub const TOP_DC_PRED: usize = 10;
pub const DC_128_PRED: usize = 11;
pub const DIAG_DOWN_LEFT_PRED_RV40_NODOWN: usize = 12;
pub const HOR_UP_PRED_RV40_NODOWN: usize = 13;
pub const VERT_LEFT_PRED_RV40_NODOWN: usize = 14;

pub const DC_PRED8X8: usize = 0;
pub const HOR_PRED8X8: usize = 1;
pub const VERT_PRED8X8: usize = 2;
pub const PLANE_PRED8X8: usize = 3;
pub const LEFT_DC_PRED8X8: usize = 4;
pub const TOP_DC_PRED8X8: usize = 5;
pub const DC_128_PRED8X8: usize = 6;

#[inline]
fn fill4(buf: &mut [u8], pos: usize, stride: usize, v: u8) {
    for y in 0..4 {
        buf[pos + y * stride..pos + y * stride + 4].fill(v);
    }
}

/// `h->pred4x4[itype](src, topright, stride)` for the RV40 table.
/// `topright` holds the four samples FFmpeg reads through its `topright`
/// pointer.
pub fn pred4x4(itype: usize, buf: &mut [u8], pos: usize, stride: usize, topright: [u8; 4]) {
    let up = pos - stride;
    let lt = buf[up - 1] as u32;
    let t0 = buf[up] as u32;
    let t1 = buf[up + 1] as u32;
    let t2 = buf[up + 2] as u32;
    let t3 = buf[up + 3] as u32;
    let [t4, t5, t6, t7] = topright.map(|v| v as u32);
    let l = |k: usize| buf[pos - 1 + k * stride] as u32;
    let (l0, l1, l2, l3) = (l(0), l(1), l(2), l(3));
    let (l4, l5, l6, l7) = (l(4), l(5), l(6), l(7));

    // Writes a 4x4 block given row-major values.
    let mut out = [[0u32; 4]; 4];
    match itype {
        VERT_PRED => {
            for row in &mut out {
                *row = [t0, t1, t2, t3];
            }
        }
        HOR_PRED => {
            out[0] = [l0; 4];
            out[1] = [l1; 4];
            out[2] = [l2; 4];
            out[3] = [l3; 4];
        }
        DC_PRED => {
            let dc = (t0 + t1 + t2 + t3 + l0 + l1 + l2 + l3 + 4) >> 3;
            out = [[dc; 4]; 4];
        }
        LEFT_DC_PRED => {
            let dc = (l0 + l1 + l2 + l3 + 2) >> 2;
            out = [[dc; 4]; 4];
        }
        TOP_DC_PRED => {
            let dc = (t0 + t1 + t2 + t3 + 2) >> 2;
            out = [[dc; 4]; 4];
        }
        DC_128_PRED => {
            out = [[128; 4]; 4];
        }
        DIAG_DOWN_RIGHT_PRED => {
            out[3][0] = (l3 + 2 * l2 + l1 + 2) >> 2;
            let v = (l2 + 2 * l1 + l0 + 2) >> 2;
            out[2][0] = v;
            out[3][1] = v;
            let v = (l1 + 2 * l0 + lt + 2) >> 2;
            out[1][0] = v;
            out[2][1] = v;
            out[3][2] = v;
            let v = (l0 + 2 * lt + t0 + 2) >> 2;
            out[0][0] = v;
            out[1][1] = v;
            out[2][2] = v;
            out[3][3] = v;
            let v = (lt + 2 * t0 + t1 + 2) >> 2;
            out[0][1] = v;
            out[1][2] = v;
            out[2][3] = v;
            let v = (t0 + 2 * t1 + t2 + 2) >> 2;
            out[0][2] = v;
            out[1][3] = v;
            out[0][3] = (t1 + 2 * t2 + t3 + 2) >> 2;
        }
        VERT_RIGHT_PRED => {
            let v = (lt + t0 + 1) >> 1;
            out[0][0] = v;
            out[2][1] = v;
            let v = (t0 + t1 + 1) >> 1;
            out[0][1] = v;
            out[2][2] = v;
            let v = (t1 + t2 + 1) >> 1;
            out[0][2] = v;
            out[2][3] = v;
            out[0][3] = (t2 + t3 + 1) >> 1;
            let v = (l0 + 2 * lt + t0 + 2) >> 2;
            out[1][0] = v;
            out[3][1] = v;
            let v = (lt + 2 * t0 + t1 + 2) >> 2;
            out[1][1] = v;
            out[3][2] = v;
            let v = (t0 + 2 * t1 + t2 + 2) >> 2;
            out[1][2] = v;
            out[3][3] = v;
            out[1][3] = (t1 + 2 * t2 + t3 + 2) >> 2;
            out[2][0] = (lt + 2 * l0 + l1 + 2) >> 2;
            out[3][0] = (l0 + 2 * l1 + l2 + 2) >> 2;
        }
        HOR_DOWN_PRED => {
            let v = (lt + l0 + 1) >> 1;
            out[0][0] = v;
            out[1][2] = v;
            let v = (l0 + 2 * lt + t0 + 2) >> 2;
            out[0][1] = v;
            out[1][3] = v;
            out[0][2] = (lt + 2 * t0 + t1 + 2) >> 2;
            out[0][3] = (t0 + 2 * t1 + t2 + 2) >> 2;
            let v = (l0 + l1 + 1) >> 1;
            out[1][0] = v;
            out[2][2] = v;
            let v = (lt + 2 * l0 + l1 + 2) >> 2;
            out[1][1] = v;
            out[2][3] = v;
            let v = (l1 + l2 + 1) >> 1;
            out[2][0] = v;
            out[3][2] = v;
            let v = (l0 + 2 * l1 + l2 + 2) >> 2;
            out[2][1] = v;
            out[3][3] = v;
            out[3][0] = (l2 + l3 + 1) >> 1;
            out[3][1] = (l1 + 2 * l2 + l3 + 2) >> 2;
        }
        DIAG_DOWN_LEFT_PRED => {
            // pred4x4_down_left_rv40_c
            out[0][0] = (t0 + t2 + 2 * t1 + 2 + l0 + l2 + 2 * l1 + 2) >> 3;
            let v = (t1 + t3 + 2 * t2 + 2 + l1 + l3 + 2 * l2 + 2) >> 3;
            out[0][1] = v;
            out[1][0] = v;
            let v = (t2 + t4 + 2 * t3 + 2 + l2 + l4 + 2 * l3 + 2) >> 3;
            out[0][2] = v;
            out[1][1] = v;
            out[2][0] = v;
            let v = (t3 + t5 + 2 * t4 + 2 + l3 + l5 + 2 * l4 + 2) >> 3;
            out[0][3] = v;
            out[1][2] = v;
            out[2][1] = v;
            out[3][0] = v;
            let v = (t4 + t6 + 2 * t5 + 2 + l4 + l6 + 2 * l5 + 2) >> 3;
            out[1][3] = v;
            out[2][2] = v;
            out[3][1] = v;
            let v = (t5 + t7 + 2 * t6 + 2 + l5 + l7 + 2 * l6 + 2) >> 3;
            out[2][3] = v;
            out[3][2] = v;
            out[3][3] = (t6 + t7 + 1 + l6 + l7 + 1) >> 2;
        }
        DIAG_DOWN_LEFT_PRED_RV40_NODOWN => {
            out[0][0] = (t0 + t2 + 2 * t1 + 2 + l0 + l2 + 2 * l1 + 2) >> 3;
            let v = (t1 + t3 + 2 * t2 + 2 + l1 + l3 + 2 * l2 + 2) >> 3;
            out[0][1] = v;
            out[1][0] = v;
            let v = (t2 + t4 + 2 * t3 + 2 + l2 + 3 * l3 + 2) >> 3;
            out[0][2] = v;
            out[1][1] = v;
            out[2][0] = v;
            let v = (t3 + t5 + 2 * t4 + 2 + l3 * 4 + 2) >> 3;
            out[0][3] = v;
            out[1][2] = v;
            out[2][1] = v;
            out[3][0] = v;
            let v = (t4 + t6 + 2 * t5 + 2 + l3 * 4 + 2) >> 3;
            out[1][3] = v;
            out[2][2] = v;
            out[3][1] = v;
            let v = (t5 + t7 + 2 * t6 + 2 + l3 * 4 + 2) >> 3;
            out[2][3] = v;
            out[3][2] = v;
            out[3][3] = (t6 + t7 + 1 + 2 * l3 + 1) >> 2;
        }
        VERT_LEFT_PRED | VERT_LEFT_PRED_RV40_NODOWN => {
            // pred4x4_vertical_left_rv40 with l4 = l3 for the no-down case.
            let l4 = if itype == VERT_LEFT_PRED { l4 } else { l3 };
            out[0][0] = (2 * t0 + 2 * t1 + l1 + 2 * l2 + l3 + 4) >> 3;
            let v = (t1 + t2 + 1) >> 1;
            out[0][1] = v;
            out[2][0] = v;
            let v = (t2 + t3 + 1) >> 1;
            out[0][2] = v;
            out[2][1] = v;
            let v = (t3 + t4 + 1) >> 1;
            out[0][3] = v;
            out[2][2] = v;
            out[2][3] = (t4 + t5 + 1) >> 1;
            out[1][0] = (t0 + 2 * t1 + t2 + l2 + 2 * l3 + l4 + 4) >> 3;
            let v = (t1 + 2 * t2 + t3 + 2) >> 2;
            out[1][1] = v;
            out[3][0] = v;
            let v = (t2 + 2 * t3 + t4 + 2) >> 2;
            out[1][2] = v;
            out[3][1] = v;
            let v = (t3 + 2 * t4 + t5 + 2) >> 2;
            out[1][3] = v;
            out[3][2] = v;
            out[3][3] = (t4 + 2 * t5 + t6 + 2) >> 2;
        }
        HOR_UP_PRED => {
            // pred4x4_horizontal_up_rv40_c
            out[0][0] = (t1 + 2 * t2 + t3 + 2 * l0 + 2 * l1 + 4) >> 3;
            out[0][1] = (t2 + 2 * t3 + t4 + l0 + 2 * l1 + l2 + 4) >> 3;
            let v = (t3 + 2 * t4 + t5 + 2 * l1 + 2 * l2 + 4) >> 3;
            out[0][2] = v;
            out[1][0] = v;
            let v = (t4 + 2 * t5 + t6 + l1 + 2 * l2 + l3 + 4) >> 3;
            out[0][3] = v;
            out[1][1] = v;
            let v = (t5 + 2 * t6 + t7 + 2 * l2 + 2 * l3 + 4) >> 3;
            out[1][2] = v;
            out[2][0] = v;
            let v = (t6 + 3 * t7 + l2 + 3 * l3 + 4) >> 3;
            out[1][3] = v;
            out[2][1] = v;
            let v = (l3 + 2 * l4 + l5 + 2) >> 2;
            out[2][3] = v;
            out[3][1] = v;
            let v = (t6 + t7 + l3 + l4 + 2) >> 2;
            out[3][0] = v;
            out[2][2] = v;
            out[3][2] = (l4 + l5 + 1) >> 1;
            out[3][3] = (l4 + 2 * l5 + l6 + 2) >> 2;
        }
        HOR_UP_PRED_RV40_NODOWN => {
            out[0][0] = (t1 + 2 * t2 + t3 + 2 * l0 + 2 * l1 + 4) >> 3;
            out[0][1] = (t2 + 2 * t3 + t4 + l0 + 2 * l1 + l2 + 4) >> 3;
            let v = (t3 + 2 * t4 + t5 + 2 * l1 + 2 * l2 + 4) >> 3;
            out[0][2] = v;
            out[1][0] = v;
            let v = (t4 + 2 * t5 + t6 + l1 + 2 * l2 + l3 + 4) >> 3;
            out[0][3] = v;
            out[1][1] = v;
            let v = (t5 + 2 * t6 + t7 + 2 * l2 + 2 * l3 + 4) >> 3;
            out[1][2] = v;
            out[2][0] = v;
            let v = (t6 + 3 * t7 + l2 + 3 * l3 + 4) >> 3;
            out[1][3] = v;
            out[2][1] = v;
            out[2][3] = l3;
            out[3][1] = l3;
            let v = (t6 + t7 + 2 * l3 + 2) >> 2;
            out[3][0] = v;
            out[2][2] = v;
            out[3][2] = l3;
            out[3][3] = l3;
        }
        _ => {
            // Not reachable: callers map RV types through `ittrans`.
            fill4(buf, pos, stride, 128);
            return;
        }
    }
    for (y, row) in out.iter().enumerate() {
        let o = pos + y * stride;
        for (x, &v) in row.iter().enumerate() {
            buf[o + x] = v as u8;
        }
    }
}

fn fill_rect(buf: &mut [u8], pos: usize, stride: usize, w: usize, h: usize, v: u8) {
    for y in 0..h {
        buf[pos + y * stride..pos + y * stride + w].fill(v);
    }
}

/// `h->pred8x8[itype](src, stride)` (chroma) for the RV40 table.
pub fn pred8x8(itype: usize, buf: &mut [u8], pos: usize, stride: usize) {
    match itype {
        VERT_PRED8X8 => {
            let mut top = [0u8; 8];
            top.copy_from_slice(&buf[pos - stride..pos - stride + 8]);
            for y in 0..8 {
                buf[pos + y * stride..pos + y * stride + 8].copy_from_slice(&top);
            }
        }
        HOR_PRED8X8 => {
            for y in 0..8 {
                let v = buf[pos + y * stride - 1];
                buf[pos + y * stride..pos + y * stride + 8].fill(v);
            }
        }
        DC_PRED8X8 => {
            // pred8x8_dc_rv40_c
            let mut dc0: u32 = 0;
            for i in 0..4 {
                dc0 += buf[pos - 1 + i * stride] as u32 + buf[pos + i - stride] as u32;
                dc0 += buf[pos + 4 + i - stride] as u32;
                dc0 += buf[pos - 1 + (i + 4) * stride] as u32;
            }
            fill_rect(buf, pos, stride, 8, 8, ((dc0 + 8) >> 4) as u8);
        }
        LEFT_DC_PRED8X8 => {
            let mut dc0: u32 = 0;
            for i in 0..8 {
                dc0 += buf[pos - 1 + i * stride] as u32;
            }
            fill_rect(buf, pos, stride, 8, 8, ((dc0 + 4) >> 3) as u8);
        }
        TOP_DC_PRED8X8 => {
            let mut dc0: u32 = 0;
            for i in 0..8 {
                dc0 += buf[pos + i - stride] as u32;
            }
            fill_rect(buf, pos, stride, 8, 8, ((dc0 + 4) >> 3) as u8);
        }
        DC_128_PRED8X8 => fill_rect(buf, pos, stride, 8, 8, 128),
        _ => {
            // PLANE is remapped to DC for chroma before prediction.
            fill_rect(buf, pos, stride, 8, 8, 128);
        }
    }
}

/// `h->pred16x16[itype](src, stride)` for the RV40 table.
pub fn pred16x16(itype: usize, buf: &mut [u8], pos: usize, stride: usize) {
    match itype {
        VERT_PRED8X8 => {
            let mut top = [0u8; 16];
            top.copy_from_slice(&buf[pos - stride..pos - stride + 16]);
            for y in 0..16 {
                buf[pos + y * stride..pos + y * stride + 16].copy_from_slice(&top);
            }
        }
        HOR_PRED8X8 => {
            for y in 0..16 {
                let v = buf[pos + y * stride - 1];
                buf[pos + y * stride..pos + y * stride + 16].fill(v);
            }
        }
        DC_PRED8X8 => {
            let mut dc: i32 = 0;
            for i in 0..16 {
                dc += buf[pos - 1 + i * stride] as i32;
            }
            for i in 0..16 {
                dc += buf[pos + i - stride] as i32;
            }
            fill_rect(buf, pos, stride, 16, 16, ((dc + 16) >> 5) as u8);
        }
        LEFT_DC_PRED8X8 => {
            let mut dc: i32 = 0;
            for i in 0..16 {
                dc += buf[pos - 1 + i * stride] as i32;
            }
            fill_rect(buf, pos, stride, 16, 16, ((dc + 8) >> 4) as u8);
        }
        TOP_DC_PRED8X8 => {
            let mut dc: i32 = 0;
            for i in 0..16 {
                dc += buf[pos + i - stride] as i32;
            }
            fill_rect(buf, pos, stride, 16, 16, ((dc + 8) >> 4) as u8);
        }
        DC_128_PRED8X8 => fill_rect(buf, pos, stride, 16, 16, 128),
        PLANE_PRED8X8 => pred16x16_plane_rv40(buf, pos, stride),
        _ => fill_rect(buf, pos, stride, 16, 16, 128),
    }
}

/// `pred16x16_plane_compat_8_c(src, stride, 0, 1)`.
fn pred16x16_plane_rv40(buf: &mut [u8], pos: usize, stride: usize) {
    let s = stride as isize;
    let p = pos as isize;
    let at = |o: isize| buf[o as usize] as i32;
    let src0 = p + 7 - s;
    let mut src1 = p + 8 * s - 1;
    let mut src2 = src1 - 2 * s;
    let mut h = at(src0 + 1) - at(src0 - 1);
    let mut v = at(src1) - at(src2);
    for k in 2..=8isize {
        src1 += s;
        src2 -= s;
        h += k as i32 * (at(src0 + k) - at(src0 - k));
        v += k as i32 * (at(src1) - at(src2));
    }
    h = (h + (h >> 2)) >> 4;
    v = (v + (v >> 2)) >> 4;
    let mut a = 16 * (at(src1) + at(src2 + 16) + 1) - 7 * (v + h);
    for j in 0..16usize {
        let mut b = a;
        a += v;
        let row = pos + j * stride;
        for i in 0..16usize {
            buf[row + i] = (b >> 5).clamp(0, 255) as u8;
            b += h;
        }
    }
}

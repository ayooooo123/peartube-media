//! Simple IDCT (8-bit), ported from FFmpeg libavcodec/simple_idct_template.c
//! and simple_idct.c (commit 2da55bf), plus `ff_put_pixels_clamped_c` /
//! `ff_add_pixels_clamped_c` from idctdsp.c.
//!
//! License: GNU Lesser General Public License, version 2.1 or later.
//!
//! `idctRowCondDC` keeps FFmpeg's DC short-circuit: a row with only the DC
//! term non-zero skips the multiplies. All arithmetic is exact i32/i64 as in
//! the C; row results are truncated to i16 the same way.

#![forbid(unsafe_code)]

use crate::bitread::av_clip_uint8;

const W1: i64 = 22725; // cos(i*M_PI/16)*sqrt(2)*(1<<14) + 0.5
const W2: i64 = 21407;
const W3: i64 = 19266;
const W4: i64 = 16383;
const W5: i64 = 12873;
const W6: i64 = 8867;
const W7: i64 = 4520;

const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;
const DC_SHIFT: u32 = 3;

#[inline]
fn mul16(a: i64, b: i64) -> i64 {
    ((a as i16 as i64) * (b as i16 as i64)) as i64
}

/// `idctRowCondDC` for 8-bit: transforms one row of 8 coefficients in place.
pub fn idct_row_cond_dc(row: &mut [i16; 64], offset: usize) {
    // DC-only shortcut: row[1..8] all zero.
    let dc_only = (1..8).all(|i| row[offset + i] == 0);
    if dc_only {
        let temp = (row[offset] as i64 * (1i64 << DC_SHIFT)) & 0xFFFF;
        let t = temp as u16;
        for i in 0..8 {
            row[offset + i] = t as i16;
        }
        return;
    }
    let r = |i: usize| row[offset + i] as i64;

    let mut a0: i64 = (W4 * r(0)) + (1 << (ROW_SHIFT - 1));
    let mut a1: i64 = a0;
    let mut a2: i64 = a0;
    let mut a3: i64 = a0;

    a0 += W2 * r(2);
    a1 += W6 * r(2);
    a2 -= W6 * r(2);
    a3 -= W2 * r(2);

    let mut b0: i64 = mul16(W1, r(1));
    let mut b1: i64 = mul16(W3, r(1));
    let mut b2: i64 = mul16(W5, r(1));
    let mut b3: i64 = mul16(W7, r(1));

    b0 += mul16(W3, r(3));
    b1 -= mul16(W7, r(3));
    b2 -= mul16(W1, r(3));
    b3 -= mul16(W5, r(3));

    if (4..8).any(|i| row[offset + i] != 0) {
        a0 += W4 * r(4) + W6 * r(6);
        a1 += -W4 * r(4) - W2 * r(6);
        a2 += -W4 * r(4) + W2 * r(6);
        a3 += W4 * r(4) - W6 * r(6);

        b0 += mul16(W5, r(5)) + mul16(W7, r(7));
        b1 += mul16(-W1, r(5)) + mul16(-W5, r(7));
        b2 += mul16(W7, r(5)) + mul16(W3, r(7));
        b3 += mul16(W3, r(5)) + mul16(-W1, r(7));
    }

    row[offset + 0] = ((a0 + b0) >> ROW_SHIFT) as i16;
    row[offset + 7] = ((a0 - b0) >> ROW_SHIFT) as i16;
    row[offset + 1] = ((a1 + b1) >> ROW_SHIFT) as i16;
    row[offset + 6] = ((a1 - b1) >> ROW_SHIFT) as i16;
    row[offset + 2] = ((a2 + b2) >> ROW_SHIFT) as i16;
    row[offset + 5] = ((a2 - b2) >> ROW_SHIFT) as i16;
    row[offset + 3] = ((a3 + b3) >> ROW_SHIFT) as i16;
    row[offset + 4] = ((a3 - b3) >> ROW_SHIFT) as i16;
}

/// `idctSparseColPut`: writes one output column of 8 pixels, clamped.
fn idct_sparse_col_put(block: &[i16; 64], col_off: usize, dst: &mut [u8], dst_off: usize, line_size: usize) {
    macro_rules! idct_cols {
        () => {{
            let col = |i: usize| block[col_off + 8 * i] as i64;
            let mut a0: i64 = W4 * (col(0) + ((1 << (COL_SHIFT - 1)) / W4));
            let mut a1: i64 = a0;
            let mut a2: i64 = a0;
            let mut a3: i64 = a0;

            a0 += W2 * col(2);
            a1 += W6 * col(2);
            a2 -= W6 * col(2);
            a3 -= W2 * col(2);

            let mut b0: i64 = mul16(W1, col(1));
            let mut b1: i64 = mul16(W3, col(1));
            let mut b2: i64 = mul16(W5, col(1));
            let mut b3: i64 = mul16(W7, col(1));

            b0 += mul16(W3, col(3));
            b1 -= mul16(W7, col(3));
            b2 -= mul16(W1, col(3));
            b3 -= mul16(W5, col(3));

            if col(4) != 0 {
                a0 += W4 * col(4);
                a1 -= W4 * col(4);
                a2 -= W4 * col(4);
                a3 += W4 * col(4);
            }
            if col(5) != 0 {
                b0 += mul16(W5, col(5));
                b1 -= mul16(W1, col(5));
                b2 += mul16(W7, col(5));
                b3 += mul16(W3, col(5));
            }
            if col(6) != 0 {
                a0 += W6 * col(6);
                a1 -= W2 * col(6);
                a2 += W2 * col(6);
                a3 -= W6 * col(6);
            }
            if col(7) != 0 {
                b0 += mul16(W7, col(7));
                b1 -= mul16(W5, col(7));
                b2 += mul16(W3, col(7));
                b3 -= mul16(W1, col(7));
            }
            (a0, a1, a2, a3, b0, b1, b2, b3)
        }};
    }
    let (a0, a1, a2, a3, b0, b1, b2, b3) = idct_cols!();
    let mut d = dst_off;
    dst[d] = av_clip_uint8(((a0 + b0) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a1 + b1) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a2 + b2) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a3 + b3) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a3 - b3) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a2 - b2) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a1 - b1) >> COL_SHIFT) as i32);
    d += line_size;
    dst[d] = av_clip_uint8(((a0 - b0) >> COL_SHIFT) as i32);
}

/// `idctSparseColAdd`: adds one output column of 8 pixels, clamped.
fn idct_sparse_col_add(block: &[i16; 64], col_off: usize, dst: &mut [u8], dst_off: usize, line_size: usize) {
    let (a0, a1, a2, a3, b0, b1, b2, b3) = {
        let col = |i: usize| block[col_off + 8 * i] as i64;
        let mut a0: i64 = W4 * (col(0) + ((1 << (COL_SHIFT - 1)) / W4));
        let mut a1: i64 = a0;
        let mut a2: i64 = a0;
        let mut a3: i64 = a0;

        a0 += W2 * col(2);
        a1 += W6 * col(2);
        a2 -= W6 * col(2);
        a3 -= W2 * col(2);

        let mut b0: i64 = mul16(W1, col(1));
        let mut b1: i64 = mul16(W3, col(1));
        let mut b2: i64 = mul16(W5, col(1));
        let mut b3: i64 = mul16(W7, col(1));

        b0 += mul16(W3, col(3));
        b1 -= mul16(W7, col(3));
        b2 -= mul16(W1, col(3));
        b3 -= mul16(W5, col(3));

        if col(4) != 0 {
            a0 += W4 * col(4);
            a1 -= W4 * col(4);
            a2 -= W4 * col(4);
            a3 += W4 * col(4);
        }
        if col(5) != 0 {
            b0 += mul16(W5, col(5));
            b1 -= mul16(W1, col(5));
            b2 += mul16(W7, col(5));
            b3 += mul16(W3, col(5));
        }
        if col(6) != 0 {
            a0 += W6 * col(6);
            a1 -= W2 * col(6);
            a2 += W2 * col(6);
            a3 -= W6 * col(6);
        }
        if col(7) != 0 {
            b0 += mul16(W7, col(7));
            b1 -= mul16(W5, col(7));
            b2 += mul16(W3, col(7));
            b3 -= mul16(W1, col(7));
        }
        (a0, a1, a2, a3, b0, b1, b2, b3)
    };
    let mut d = dst_off;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a0 + b0) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a1 + b1) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a2 + b2) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a3 + b3) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a3 - b3) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a2 - b2) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a1 - b1) >> COL_SHIFT) as i32));
    d += line_size;
    dst[d] = av_clip_uint8(dst[d] as i32 + (((a0 - b0) >> COL_SHIFT) as i32));
}

/// `ff_simple_idct_put_int16_8bit`: 8x8 IDCT written to a byte image.
/// `dst` is the full plane; `(x, y)` is the top-left of the 8x8 block.
pub fn simple_idct_put(block: &mut [i16; 64], dst: &mut [u8], dst_off: usize, line_size: usize) {
    for i in 0..8 {
        idct_row_cond_dc(block, i * 8);
    }
    for i in 0..8 {
        idct_sparse_col_put(block, i, dst, dst_off + i, line_size);
    }
}

/// `ff_simple_idct_add_int16_8bit`.
pub fn simple_idct_add(block: &mut [i16; 64], dst: &mut [u8], dst_off: usize, line_size: usize) {
    for i in 0..8 {
        idct_row_cond_dc(block, i * 8);
    }
    for i in 0..8 {
        idct_sparse_col_add(block, i, dst, dst_off + i, line_size);
    }
}

/// `ff_put_pixels_clamped_c` for an 8x8 block at `dst_off`.
pub fn put_pixels_clamped(block: &[i16], dst: &mut [u8], dst_off: usize, line_size: usize) {
    for i in 0..8 {
        for j in 0..8 {
            dst[dst_off + i * line_size + j] = av_clip_uint8(block[i * 8 + j] as i32);
        }
    }
}

/// `ff_add_pixels_clamped_c` for an 8x8 block at `dst_off`.
pub fn add_pixels_clamped(block: &[i16], dst: &mut [u8], dst_off: usize, line_size: usize) {
    for i in 0..8 {
        for j in 0..8 {
            let idx = dst_off + i * line_size + j;
            dst[idx] = av_clip_uint8(dst[idx] as i32 + block[i * 8 + j] as i32);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DC-only block of level 256: FFmpeg's row shortcut stores
    /// `(row[0] << DC_SHIFT) & 0xffff` = 2048 in every entry (no i16 wrap
    /// because 2048 < 32768); the column pass yields 32 for every pixel.
    #[test]
    fn dc_only() {
        let mut block = [0i16; 64];
        block[0] = 256;
        let mut dst = vec![0u8; 16 * 16];
        simple_idct_put(&mut block, &mut dst, 0, 16);
        for i in 0..8 {
            for j in 0..8 {
                assert_eq!(dst[i * 16 + j], 32, "pixel ({i},{j})");
            }
        }
    }
}

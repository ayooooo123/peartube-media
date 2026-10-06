//! Integer IDCTs ported from FFmpeg commit 2da55bf:
//! - `wmv2dsp.c` `wmv2_idct_add/put_c` — the WMV2 + IntraX8 transform.
//! - `simple_idct_template.c` — the 8-bit row/column kernels used by the
//!   msmpeg4 family (`ff_simple_idct_int8_8x8` add/put).
//! - `vc1dsp.c` — `vc1_inv_trans_8x8/8x4/4x8/4x4` (+ DC variants).
//! All fixed-point; bit-exact to FFmpeg's C code.

#[inline(always)]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ───────────────────────── WMV2 DSP ─────────────────────────
// libavcodec/wmv2dsp.c (LGPL-2.1-or-later).

const W0: i64 = 2048;
const W1: i64 = 2841;
const W2: i64 = 2676;
const W3: i64 = 2408;
const W4: i64 = 2048;
const W5: i64 = 1609;
const W6: i64 = 1108;
const W7: i64 = 565;

#[inline]
fn wmv2_idct_row(b: &mut [i16; 64], row: usize) {
    let s = |i: usize| b[row * 8 + i] as i64;
    let a1 = W1 * s(1) + W7 * s(7);
    let a7 = W7 * s(1) - W1 * s(7);
    let a5 = W5 * s(5) + W3 * s(3);
    let a3 = W3 * s(5) - W5 * s(3);
    let a2 = W2 * s(2) + W6 * s(6);
    let a6 = W6 * s(2) - W2 * s(6);
    let a0 = W0 * s(0) + W0 * s(4);
    let a4 = W0 * s(0) - W0 * s(4);

    let s1 = (181i64 * (a1 - a5 + a7 - a3) + 128) >> 8;
    let s2 = (181i64 * (a1 - a5 - a7 + a3) + 128) >> 8;

    b[row * 8] = ((a0 + a2 + a1 + a5 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 1] = ((a4 + a6 + s1 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 2] = ((a4 - a6 + s2 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 3] = ((a0 - a2 + a7 + a3 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 4] = ((a0 - a2 - a7 - a3 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 5] = ((a4 - a6 - s2 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 6] = ((a4 + a6 - s1 + (1 << 7)) >> 8) as i16;
    b[row * 8 + 7] = ((a0 + a2 - a1 - a5 + (1 << 7)) >> 8) as i16;
}

#[inline]
fn wmv2_idct_col(b: &mut [i16; 64], col: usize) {
    let s = |r: usize| b[r * 8 + col] as i64;
    let a1 = (W1 * s(1) + W7 * s(7) + 4) >> 3;
    let a7 = (W7 * s(1) - W1 * s(7) + 4) >> 3;
    let a5 = (W5 * s(5) + W3 * s(3) + 4) >> 3;
    let a3 = (W3 * s(5) - W5 * s(3) + 4) >> 3;
    let a2 = (W2 * s(2) + W6 * s(6) + 4) >> 3;
    let a6 = (W6 * s(2) - W2 * s(6) + 4) >> 3;
    let a0 = (W0 * s(0) + W0 * s(4)) >> 3;
    let a4 = (W0 * s(0) - W0 * s(4)) >> 3;

    let s1 = (181i64 * (a1 - a5 + a7 - a3) + 128) >> 8;
    let s2 = (181i64 * (a1 - a5 - a7 + a3) + 128) >> 8;

    b[0 * 8 + col] = ((a0 + a2 + a1 + a5 + (1 << 13)) >> 14) as i16;
    b[1 * 8 + col] = ((a4 + a6 + s1 + (1 << 13)) >> 14) as i16;
    b[2 * 8 + col] = ((a4 - a6 + s2 + (1 << 13)) >> 14) as i16;
    b[3 * 8 + col] = ((a0 - a2 + a7 + a3 + (1 << 13)) >> 14) as i16;
    b[4 * 8 + col] = ((a0 - a2 - a7 - a3 + (1 << 13)) >> 14) as i16;
    b[5 * 8 + col] = ((a4 - a6 - s2 + (1 << 13)) >> 14) as i16;
    b[6 * 8 + col] = ((a4 + a6 - s1 + (1 << 13)) >> 14) as i16;
    b[7 * 8 + col] = ((a0 + a2 - a1 - a5 + (1 << 13)) >> 14) as i16;
}

/// In-place WMV2 8×8 IDCT (row + column passes).
pub fn wmv2_idct(block: &mut [i16; 64]) {
    for r in 0..8 {
        wmv2_idct_row(block, r);
    }
    for c in 0..8 {
        wmv2_idct_col(block, c);
    }
}

/// WMV2 `idct_add`: transform in place, then add to `dest` with u8 clipping.
pub fn wmv2_idct_add(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    wmv2_idct(block);
    for r in 0..8 {
        for c in 0..8 {
            let d = &mut dest[r * dstride + c];
            *d = clip_u8(*d as i32 + block[r * 8 + c] as i32);
        }
    }
}

/// WMV2 `idct_put`: transform in place, then store to `dest` with clipping.
pub fn wmv2_idct_put(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    wmv2_idct(block);
    for r in 0..8 {
        for c in 0..8 {
            dest[r * dstride + c] = clip_u8(block[r * 8 + c] as i32);
        }
    }
}

// ───────────────────────── simple IDCT ─────────────────────────
// libavcodec/simple_idct_template.c, BIT_DEPTH 8 (LGPL-2.1-or-later):
// W1..W7 below, ROW_SHIFT 11, COL_SHIFT 20, MUL16 = plain 16-bit multiply.

const SW1: i64 = 22725;
const SW2: i64 = 21407;
const SW3: i64 = 19266;
const SW4: i64 = 16383;
const SW5: i64 = 12873;
const SW6: i64 = 8867;
const SW7: i64 = 4520;
const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;
const DC_SHIFT: u32 = 3;

/// FFmpeg's `idctRowCondDC` on one row of an 8×8 block.
#[inline]
fn simple_idct_row(row: &mut [i16; 64], r: usize) {
    let base = r * 8;
    // DC shortcut: rows 1..7 all zero → replicate row[0] scaled (the C code
    // tests the 64-bit words; the arithmetic result is identical).
    if row[base + 1] == 0
        && row[base + 2] == 0
        && row[base + 3] == 0
        && row[base + 4] == 0
        && row[base + 5] == 0
        && row[base + 6] == 0
        && row[base + 7] == 0
    {
        let temp = ((row[base] as i64 * (1i64 << DC_SHIFT)) & 0xffff) as i16;
        let v = temp as i16;
        for i in 0..8 {
            row[base + i] = v;
        }
        return;
    }
    let s = |i: usize| row[base + i] as i64;
    let a0 = SW4 * s(0) + (1 << (ROW_SHIFT - 1))
        + SW2 * s(2)
        + SW4 * s(4) + SW6 * s(6);
    let a1 = SW4 * s(0) + (1 << (ROW_SHIFT - 1))
        + SW6 * s(2)
        - SW4 * s(4) - SW2 * s(6);
    let a2 = SW4 * s(0) + (1 << (ROW_SHIFT - 1))
        - SW6 * s(2)
        - SW4 * s(4) + SW2 * s(6);
    let a3 = SW4 * s(0) + (1 << (ROW_SHIFT - 1))
        - SW2 * s(2)
        + SW4 * s(4) - SW6 * s(6);

    let b0 = SW1 * s(1) + SW3 * s(3) + SW5 * s(5) + SW7 * s(7);
    let b1 = SW3 * s(1) - SW7 * s(3) - SW1 * s(5) - SW5 * s(7);
    let b2 = SW5 * s(1) - SW1 * s(3) + SW7 * s(5) + SW3 * s(7);
    let b3 = SW7 * s(1) - SW5 * s(3) + SW3 * s(5) - SW1 * s(7);

    row[base] = ((a0 + b0) >> ROW_SHIFT) as i16;
    row[base + 7] = ((a0 - b0) >> ROW_SHIFT) as i16;
    row[base + 1] = ((a1 + b1) >> ROW_SHIFT) as i16;
    row[base + 6] = ((a1 - b1) >> ROW_SHIFT) as i16;
    row[base + 2] = ((a2 + b2) >> ROW_SHIFT) as i16;
    row[base + 5] = ((a2 - b2) >> ROW_SHIFT) as i16;
    row[base + 3] = ((a3 + b3) >> ROW_SHIFT) as i16;
    row[base + 4] = ((a3 - b3) >> ROW_SHIFT) as i16;
}

/// FFmpeg's `idctSparseColPut` on one column, writing to `dest`.
#[inline]
fn simple_idct_sparse_col_put(dest: &mut [u8], dstride: usize, col: usize, block: &[i16; 64]) {
    let c = |r: usize| block[r * 8 + col] as i64;
    let a0 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW2 * c(2) + SW4 * c(4) + SW6 * c(6);
    let a1 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW6 * c(2) - SW4 * c(4) - SW2 * c(6);
    let a2 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW6 * c(2) - SW4 * c(4) + SW2 * c(6);
    let a3 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW2 * c(2) + SW4 * c(4) - SW6 * c(6);

    let b0 = SW1 * c(1) + SW3 * c(3) + SW5 * c(5) + SW7 * c(7);
    let b1 = SW3 * c(1) - SW7 * c(3) - SW1 * c(5) - SW5 * c(7);
    let b2 = SW5 * c(1) - SW1 * c(3) + SW7 * c(5) + SW3 * c(7);
    let b3 = SW7 * c(1) - SW5 * c(3) + SW3 * c(5) - SW1 * c(7);

    dest[col] = clip_u8(((a0 + b0) >> COL_SHIFT) as i32);
    dest[dstride + col] = clip_u8(((a1 + b1) >> COL_SHIFT) as i32);
    dest[2 * dstride + col] = clip_u8(((a2 + b2) >> COL_SHIFT) as i32);
    dest[3 * dstride + col] = clip_u8(((a3 + b3) >> COL_SHIFT) as i32);
    dest[4 * dstride + col] = clip_u8(((a3 - b3) >> COL_SHIFT) as i32);
    dest[5 * dstride + col] = clip_u8(((a2 - b2) >> COL_SHIFT) as i32);
    dest[6 * dstride + col] = clip_u8(((a1 - b1) >> COL_SHIFT) as i32);
    dest[7 * dstride + col] = clip_u8(((a0 - b0) >> COL_SHIFT) as i32);
}

/// `ff_simple_idct_int8_8x8_put`.
pub fn simple_idct_put(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        simple_idct_row(block, r);
    }
    for c in 0..8 {
        simple_idct_sparse_col_put(dest, dstride, c, block);
    }
}

/// FFmpeg's `idctSparseColAdd` on one column, adding into `dest`.
#[inline]
fn simple_idct_sparse_col_add(dest: &mut [u8], dstride: usize, col: usize, block: &[i16; 64]) {
    let c = |r: usize| block[r * 8 + col] as i64;
    let a0 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW2 * c(2) + SW4 * c(4) + SW6 * c(6);
    let a1 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW6 * c(2) - SW4 * c(4) - SW2 * c(6);
    let a2 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW6 * c(2) - SW4 * c(4) + SW2 * c(6);
    let a3 = SW4 * (c(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW2 * c(2) + SW4 * c(4) - SW6 * c(6);

    let b0 = SW1 * c(1) + SW3 * c(3) + SW5 * c(5) + SW7 * c(7);
    let b1 = SW3 * c(1) - SW7 * c(3) - SW1 * c(5) - SW5 * c(7);
    let b2 = SW5 * c(1) - SW1 * c(3) + SW7 * c(5) + SW3 * c(7);
    let b3 = SW7 * c(1) - SW5 * c(3) + SW3 * c(5) - SW1 * c(7);

    dest[col] = clip_u8(dest[col] as i32 + ((a0 + b0) >> COL_SHIFT) as i32);
    dest[dstride + col] = clip_u8(dest[dstride + col] as i32 + ((a1 + b1) >> COL_SHIFT) as i32);
    dest[2 * dstride + col] =
        clip_u8(dest[2 * dstride + col] as i32 + ((a2 + b2) >> COL_SHIFT) as i32);
    dest[3 * dstride + col] =
        clip_u8(dest[3 * dstride + col] as i32 + ((a3 + b3) >> COL_SHIFT) as i32);
    dest[4 * dstride + col] =
        clip_u8(dest[4 * dstride + col] as i32 + ((a3 - b3) >> COL_SHIFT) as i32);
    dest[5 * dstride + col] =
        clip_u8(dest[5 * dstride + col] as i32 + ((a2 - b2) >> COL_SHIFT) as i32);
    dest[6 * dstride + col] =
        clip_u8(dest[6 * dstride + col] as i32 + ((a1 - b1) >> COL_SHIFT) as i32);
    dest[7 * dstride + col] =
        clip_u8(dest[7 * dstride + col] as i32 + ((a0 - b0) >> COL_SHIFT) as i32);
}

/// `ff_simple_idct_int8_8x8_add`.
pub fn simple_idct_add(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        simple_idct_row(block, r);
    }
    for c in 0..8 {
        simple_idct_sparse_col_add(dest, dstride, c, block);
    }
}

/// `ff_simple_idct84_add` — 8×4 IDCT add (two 8×4 sub-blocks in one 64-slot
/// block: rows 0..4 normal, rows 4..8 the second half), used by WMV2 ABT.
pub fn simple_idct84_add(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    // Rows first on rows 0..4 and 4..8, then columns over 8 rows of 4.
    fn row4(block: &mut [i16; 64], r: usize) {
        let base = r * 8;
        if block[base + 1] == 0
            && block[base + 2] == 0
            && block[base + 3] == 0
            && block[base + 4] == 0
            && block[base + 5] == 0
            && block[base + 6] == 0
            && block[base + 7] == 0
        {
            let temp = ((block[base] as i64 * (1i64 << (DC_SHIFT - 3))) & 0xffff) as i16;
            for i in 0..8 {
                block[base + i] = temp;
            }
            return;
        }
        // Same butterfly as the 8-row pass; the C code reuses idctRowCondDC
        // with extra_shift=3 for the 84 variant.
        let s = |i: usize| block[base + i] as i64;
        let a0 = SW4 * s(0) + (1 << (ROW_SHIFT + 3 - 1))
            + SW2 * s(2)
            + SW4 * s(4) + SW6 * s(6);
        let a1 = SW4 * s(0) + (1 << (ROW_SHIFT + 3 - 1))
            + SW6 * s(2)
            - SW4 * s(4) - SW2 * s(6);
        let a2 = SW4 * s(0) + (1 << (ROW_SHIFT + 3 - 1))
            - SW6 * s(2)
            - SW4 * s(4) + SW2 * s(6);
        let a3 = SW4 * s(0) + (1 << (ROW_SHIFT + 3 - 1))
            - SW2 * s(2)
            + SW4 * s(4) - SW6 * s(6);

        let b0 = SW1 * s(1) + SW3 * s(3) + SW5 * s(5) + SW7 * s(7);
        let b1 = SW3 * s(1) - SW7 * s(3) - SW1 * s(5) - SW5 * s(7);
        let b2 = SW5 * s(1) - SW1 * s(3) + SW7 * s(5) + SW3 * s(7);
        let b3 = SW7 * s(1) - SW5 * s(3) + SW3 * s(5) - SW1 * s(7);

        block[base] = ((a0 + b0) >> (ROW_SHIFT + 3)) as i16;
        block[base + 7] = ((a0 - b0) >> (ROW_SHIFT + 3)) as i16;
        block[base + 1] = ((a1 + b1) >> (ROW_SHIFT + 3)) as i16;
        block[base + 6] = ((a1 - b1) >> (ROW_SHIFT + 3)) as i16;
        block[base + 2] = ((a2 + b2) >> (ROW_SHIFT + 3)) as i16;
        block[base + 5] = ((a2 - b2) >> (ROW_SHIFT + 3)) as i16;
        block[base + 3] = ((a3 + b3) >> (ROW_SHIFT + 3)) as i16;
        block[base + 4] = ((a3 - b3) >> (ROW_SHIFT + 3)) as i16;
    }
    for r in 0..8 {
        row4(block, r);
    }
    // Column pass over 4 rows (the "84" transform): COL_SHIFT adjusted.
    for c in 0..8 {
        let cs = |r: usize| block[r * 8 + c] as i64;
        let a0 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 3 - 1)) / SW4))
            + SW2 * cs(2) + SW4 * cs(4) + SW6 * cs(6);
        let a1 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 3 - 1)) / SW4))
            + SW6 * cs(2) - SW4 * cs(4) - SW2 * cs(6);
        let a2 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 3 - 1)) / SW4))
            - SW6 * cs(2) - SW4 * cs(4) + SW2 * cs(6);
        let a3 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 3 - 1)) / SW4))
            - SW2 * cs(2) + SW4 * cs(4) - SW6 * cs(6);
        let b0 = SW1 * cs(1) + SW3 * cs(3) + SW5 * cs(5) + SW7 * cs(7);
        let b1 = SW3 * cs(1) - SW7 * cs(3) - SW1 * cs(5) - SW5 * cs(7);
        let b2 = SW5 * cs(1) - SW1 * cs(3) + SW7 * cs(5) + SW3 * cs(7);
        let b3 = SW7 * cs(1) - SW5 * cs(3) + SW3 * cs(5) - SW1 * cs(7);
        let sh = COL_SHIFT - 3;
        dest[c] = clip_u8(dest[c] as i32 + ((a0 + b0) >> sh) as i32);
        dest[dstride + c] = clip_u8(dest[dstride + c] as i32 + ((a1 + b1) >> sh) as i32);
        dest[2 * dstride + c] = clip_u8(dest[2 * dstride + c] as i32 + ((a2 + b2) >> sh) as i32);
        dest[3 * dstride + c] = clip_u8(dest[3 * dstride + c] as i32 + ((a3 + b3) >> sh) as i32);
        dest[4 * dstride + c] = clip_u8(dest[4 * dstride + c] as i32 + ((a3 - b3) >> sh) as i32);
        dest[5 * dstride + c] = clip_u8(dest[5 * dstride + c] as i32 + ((a2 - b2) >> sh) as i32);
        dest[6 * dstride + c] = clip_u8(dest[6 * dstride + c] as i32 + ((a1 - b1) >> sh) as i32);
        dest[7 * dstride + c] = clip_u8(dest[7 * dstride + c] as i32 + ((a0 - b0) >> sh) as i32);
    }
}

/// `ff_simple_idct48_add` — 4-column IDCT add over 8 rows (WMV2 ABT type 2).
pub fn simple_idct48_add(dest: &mut [u8], dstride: usize, block: &mut [i16; 64]) {
    // Row pass over 4 columns only (the C code operates on cols 0..4).
    fn row4(block: &mut [i16; 64], r: usize) {
        let base = r * 8;
        let s = |i: usize| block[base + i] as i64;
        let a0 = SW4 * s(0) + (1 << (ROW_SHIFT - 1)) + SW2 * s(2);
        let a1 = SW4 * s(0) + (1 << (ROW_SHIFT - 1)) + SW6 * s(2);
        let a2 = SW4 * s(0) + (1 << (ROW_SHIFT - 1)) - SW6 * s(2);
        let a3 = SW4 * s(0) + (1 << (ROW_SHIFT - 1)) - SW2 * s(2);
        let b0 = SW1 * s(1) + SW3 * s(3);
        let b1 = SW3 * s(1) - SW7 * s(3);
        let b2 = SW5 * s(1) - SW1 * s(3);
        let b3 = SW7 * s(1) - SW5 * s(3);
        block[base] = ((a0 + b0) >> ROW_SHIFT) as i16;
        block[base + 3] = ((a0 - b0) >> ROW_SHIFT) as i16;
        block[base + 1] = ((a1 + b1) >> ROW_SHIFT) as i16;
        block[base + 2] = ((a2 + b2) >> ROW_SHIFT) as i16;
        let _ = a3;
    }
    for r in 0..8 {
        row4(block, r);
    }
    for c in 0..4 {
        let cs = |r: usize| block[r * 8 + c] as i64;
        let a0 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW2 * cs(2);
        let a1 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 1)) / SW4)) + SW6 * cs(2);
        let a2 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW6 * cs(2);
        let a3 = SW4 * (cs(0) + ((1 << (COL_SHIFT - 1)) / SW4)) - SW2 * cs(2);
        let b0 = SW1 * cs(1) + SW3 * cs(3);
        let b1 = SW3 * cs(1) - SW7 * cs(3);
        let b2 = SW5 * cs(1) - SW1 * cs(3);
        let b3 = SW7 * cs(1) - SW5 * cs(3);
        dest[c] = clip_u8(dest[c] as i32 + ((a0 + b0) >> COL_SHIFT) as i32);
        dest[dstride + c] = clip_u8(dest[dstride + c] as i32 + ((a1 + b1) >> COL_SHIFT) as i32);
        dest[2 * dstride + c] =
            clip_u8(dest[2 * dstride + c] as i32 + ((a2 + b2) >> COL_SHIFT) as i32);
        dest[3 * dstride + c] =
            clip_u8(dest[3 * dstride + c] as i32 + ((a3 + b3) >> COL_SHIFT) as i32);
        dest[4 * dstride + c] =
            clip_u8(dest[4 * dstride + c] as i32 + ((a3 - b3) >> COL_SHIFT) as i32);
        dest[5 * dstride + c] =
            clip_u8(dest[5 * dstride + c] as i32 + ((a2 - b2) >> COL_SHIFT) as i32);
        dest[6 * dstride + c] =
            clip_u8(dest[6 * dstride + c] as i32 + ((a1 - b1) >> COL_SHIFT) as i32);
        dest[7 * dstride + c] =
            clip_u8(dest[7 * dstride + c] as i32 + ((a0 - b0) >> COL_SHIFT) as i32);
    }
}

// ───────────────────────── VC-1 transforms ─────────────────────────
// libavcodec/vc1dsp.c (LGPL-2.1-or-later).

/// `vc1_inv_trans_8x8_c`: in-place transform of the coefficient block.
pub fn vc1_inv_trans_8x8(block: &mut [i16; 64]) {
    let mut temp = [0i16; 64];
    for r in 0..8 {
        let s = |i: usize| block[r * 8 + i] as i64;
        let t1 = 12 * (s(0) + s(32)) + 4;
        let t2 = 12 * (s(0) - s(32)) + 4;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;

        let t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);

        temp[r * 8] = ((t5 + t1) >> 3) as i16;
        temp[r * 8 + 1] = ((t6 + t2) >> 3) as i16;
        temp[r * 8 + 2] = ((t7 + t3) >> 3) as i16;
        temp[r * 8 + 3] = ((t8 + t4) >> 3) as i16;
        temp[r * 8 + 4] = ((t8 - t4) >> 3) as i16;
        temp[r * 8 + 5] = ((t7 - t3) >> 3) as i16;
        temp[r * 8 + 6] = ((t6 - t2) >> 3) as i16;
        temp[r * 8 + 7] = ((t5 - t1) >> 3) as i16;
    }
    for c in 0..8 {
        let s = |r: usize| temp[r * 8 + c] as i64;
        let t1 = 12 * (s(0) + s(32)) + 64;
        let t2 = 12 * (s(0) - s(32)) + 64;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;

        let t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);

        block[0 * 8 + c] = ((t5 + t1) >> 7) as i16;
        block[1 * 8 + c] = ((t6 + t2) >> 7) as i16;
        block[2 * 8 + c] = ((t7 + t3) >> 7) as i16;
        block[3 * 8 + c] = ((t8 + t4) >> 7) as i16;
        block[4 * 8 + c] = ((t8 - t4 + 1) >> 7) as i16;
        block[5 * 8 + c] = ((t7 - t3 + 1) >> 7) as i16;
        block[6 * 8 + c] = ((t6 - t2 + 1) >> 7) as i16;
        block[7 * 8 + c] = ((t5 - t1 + 1) >> 7) as i16;
    }
}

/// `vc1_inv_trans_8x8_dc_c`.
pub fn vc1_inv_trans_8x8_dc(dest: &mut [u8], stride: usize, block: &[i16; 64]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (3 * dc + 16) >> 5;
    for r in 0..8 {
        for c in 0..8 {
            let d = &mut dest[r * stride + c];
            *d = clip_u8(*d as i32 + dc);
        }
    }
}

/// `vc1_inv_trans_4x8_c`: 4-wide × 8-tall, add to dest.
pub fn vc1_inv_trans_4x8(dest: &mut [u8], stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        let s = |i: usize| block[r * 8 + i] as i64;
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        block[r * 8] = ((t1 + t3) >> 3) as i16;
        block[r * 8 + 1] = ((t2 - t4) >> 3) as i16;
        block[r * 8 + 2] = ((t2 + t4) >> 3) as i16;
        block[r * 8 + 3] = ((t1 - t3) >> 3) as i16;
    }
    for c in 0..4 {
        let s = |r: usize| block[r * 8 + c] as i64;
        let t1 = 12 * (s(0) + s(32)) + 64;
        let t2 = 12 * (s(0) - s(32)) + 64;
        let t3 = 16 * s(16) + 6 * s(48);
        let t4 = 6 * s(16) - 16 * s(48);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;

        let t1 = 16 * s(8) + 15 * s(24) + 9 * s(40) + 4 * s(56);
        let t2 = 15 * s(8) - 4 * s(24) - 16 * s(40) - 9 * s(56);
        let t3 = 9 * s(8) - 16 * s(24) + 4 * s(40) + 15 * s(56);
        let t4 = 4 * s(8) - 9 * s(24) + 15 * s(40) - 16 * s(56);

        dest[c] = clip_u8(dest[c] as i32 + ((t5 + t1) >> 7) as i32);
        dest[stride + c] = clip_u8(dest[stride + c] as i32 + ((t6 + t2) >> 7) as i32);
        dest[2 * stride + c] = clip_u8(dest[2 * stride + c] as i32 + ((t7 + t3) >> 7) as i32);
        dest[3 * stride + c] = clip_u8(dest[3 * stride + c] as i32 + ((t8 + t4) >> 7) as i32);
        dest[4 * stride + c] = clip_u8(dest[4 * stride + c] as i32 + ((t8 - t4 + 1) >> 7) as i32);
        dest[5 * stride + c] = clip_u8(dest[5 * stride + c] as i32 + ((t7 - t3 + 1) >> 7) as i32);
        dest[6 * stride + c] = clip_u8(dest[6 * stride + c] as i32 + ((t6 - t2 + 1) >> 7) as i32);
        dest[7 * stride + c] = clip_u8(dest[7 * stride + c] as i32 + ((t5 - t1 + 1) >> 7) as i32);
    }
}

/// `vc1_inv_trans_4x8_dc_c`.
pub fn vc1_inv_trans_4x8_dc(dest: &mut [u8], stride: usize, block: &[i16; 64]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (12 * dc + 64) >> 7;
    for r in 0..8 {
        for c in 0..4 {
            let d = &mut dest[r * stride + c];
            *d = clip_u8(*d as i32 + dc);
        }
    }
}

/// `vc1_inv_trans_8x4_c`: 8-wide × 4-tall, add to dest.
pub fn vc1_inv_trans_8x4(dest: &mut [u8], stride: usize, block: &mut [i16; 64]) {
    for r in 0..4 {
        let s = |i: usize| block[r * 8 + i] as i64;
        let t1 = 12 * (s(0) + s(4)) + 4;
        let t2 = 12 * (s(0) - s(4)) + 4;
        let t3 = 16 * s(2) + 6 * s(6);
        let t4 = 6 * s(2) - 16 * s(6);
        let t5 = t1 + t3;
        let t6 = t2 + t4;
        let t7 = t2 - t4;
        let t8 = t1 - t3;

        let t1 = 16 * s(1) + 15 * s(3) + 9 * s(5) + 4 * s(7);
        let t2 = 15 * s(1) - 4 * s(3) - 16 * s(5) - 9 * s(7);
        let t3 = 9 * s(1) - 16 * s(3) + 4 * s(5) + 15 * s(7);
        let t4 = 4 * s(1) - 9 * s(3) + 15 * s(5) - 16 * s(7);

        block[r * 8] = ((t5 + t1) >> 3) as i16;
        block[r * 8 + 1] = ((t6 + t2) >> 3) as i16;
        block[r * 8 + 2] = ((t7 + t3) >> 3) as i16;
        block[r * 8 + 3] = ((t8 + t4) >> 3) as i16;
        block[r * 8 + 4] = ((t8 - t4) >> 3) as i16;
        block[r * 8 + 5] = ((t7 - t3) >> 3) as i16;
        block[r * 8 + 6] = ((t6 - t2) >> 3) as i16;
        block[r * 8 + 7] = ((t5 - t1) >> 3) as i16;
    }
    for c in 0..8 {
        let s = |r: usize| block[r * 8 + c] as i64;
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        dest[c] = clip_u8(dest[c] as i32 + ((t1 + t3) >> 7) as i32);
        dest[stride + c] = clip_u8(dest[stride + c] as i32 + ((t2 - t4) >> 7) as i32);
        dest[2 * stride + c] = clip_u8(dest[2 * stride + c] as i32 + ((t2 + t4) >> 7) as i32);
        dest[3 * stride + c] = clip_u8(dest[3 * stride + c] as i32 + ((t1 - t3) >> 7) as i32);
    }
}

/// `vc1_inv_trans_8x4_dc_c`.
pub fn vc1_inv_trans_8x4_dc(dest: &mut [u8], stride: usize, block: &[i16; 64]) {
    let mut dc = block[0] as i32;
    dc = (3 * dc + 1) >> 1;
    dc = (17 * dc + 64) >> 7;
    for r in 0..4 {
        for c in 0..8 {
            let d = &mut dest[r * stride + c];
            *d = clip_u8(*d as i32 + dc);
        }
    }
}

/// `vc1_inv_trans_4x4_c`: add to dest.
pub fn vc1_inv_trans_4x4(dest: &mut [u8], stride: usize, block: &mut [i16; 64]) {
    for r in 0..4 {
        let s = |i: usize| block[r * 8 + i] as i64;
        let t1 = 17 * (s(0) + s(2)) + 4;
        let t2 = 17 * (s(0) - s(2)) + 4;
        let t3 = 22 * s(1) + 10 * s(3);
        let t4 = 22 * s(3) - 10 * s(1);
        block[r * 8] = ((t1 + t3) >> 3) as i16;
        block[r * 8 + 1] = ((t2 - t4) >> 3) as i16;
        block[r * 8 + 2] = ((t2 + t4) >> 3) as i16;
        block[r * 8 + 3] = ((t1 - t3) >> 3) as i16;
    }
    for c in 0..4 {
        let s = |r: usize| block[r * 8 + c] as i64;
        let t1 = 17 * (s(0) + s(16)) + 64;
        let t2 = 17 * (s(0) - s(16)) + 64;
        let t3 = 22 * s(8) + 10 * s(24);
        let t4 = 22 * s(24) - 10 * s(8);
        dest[c] = clip_u8(dest[c] as i32 + ((t1 + t3) >> 7) as i32);
        dest[stride + c] = clip_u8(dest[stride + c] as i32 + ((t2 - t4) >> 7) as i32);
        dest[2 * stride + c] = clip_u8(dest[2 * stride + c] as i32 + ((t2 + t4) >> 7) as i32);
        dest[3 * stride + c] = clip_u8(dest[3 * stride + c] as i32 + ((t1 - t3) >> 7) as i32);
    }
}

/// `vc1_inv_trans_4x4_dc_c`.
pub fn vc1_inv_trans_4x4_dc(dest: &mut [u8], stride: usize, block: &[i16; 64]) {
    let mut dc = block[0] as i32;
    dc = (17 * dc + 4) >> 3;
    dc = (17 * dc + 64) >> 7;
    for r in 0..4 {
        for c in 0..4 {
            let d = &mut dest[r * stride + c];
            *d = clip_u8(*d as i32 + dc);
        }
    }
}

/// `put_pixels_clamped` on a transformed block (VC-1 non-overlap path).
pub fn put_pixels_clamped(block: &[i16; 64], dest: &mut [u8], stride: usize) {
    for r in 0..8 {
        for c in 0..8 {
            dest[r * stride + c] = clip_u8(block[r * 8 + c] as i32);
        }
    }
}

/// `put_signed_pixels_clamped` (libavcodec/idctdsp.c): each sample becomes
/// `clip(block[i] + 128)` with saturating semantics (v < -128 → 0, v > 127
/// → 255). Used by the VC-1 overlap path where the block holds
/// prediction-relative samples.
pub fn put_signed_pixels_clamped(block: &[i16; 64], dest: &mut [u8], stride: usize) {
    for r in 0..8 {
        for c in 0..8 {
            let v = block[r * 8 + c] as i32;
            dest[r * stride + c] = if v < -128 {
                0
            } else if v > 127 {
                255
            } else {
                (v + 128) as u8
            };
        }
    }
}

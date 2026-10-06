//! Integer IDCTs of the MS-MPEG-4 / WMV1 / WMV2 decoders.
//!
//! Ported from FFmpeg commit 2da55bf (LGPL-2.1-or-later):
//! - `libavcodec/wmv2dsp.c`: `wmv2_idct_put_c` / `wmv2_idct_add_c` (WMV2 and
//!   IntraX8 transform).
//! - `libavcodec/simple_idct_template.c` (8-bit, 16-bit coefficients):
//!   `ff_simple_idct_put_int16_8bit` / `ff_simple_idct_add_int16_8bit`, the C
//!   IDCT FFmpeg selects with `-idct simple` for MS-MPEG-4 v1-v3 and WMV1.
//! - `libavcodec/simple_idct.c`: `ff_simple_idct84_add` /
//!   `ff_simple_idct48_add` (WMV2 adaptive block transform).
//!
//! Arithmetic wraps exactly like the C code's 32-bit (unsigned) math so that
//! corrupt coefficients cannot trigger overflow panics.

#[inline(always)]
fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

// ───────────────────────── WMV2 ─────────────────────────

const WW0: i32 = 2048;
const WW1: i32 = 2841;
const WW2: i32 = 2676;
const WW3: i32 = 2408;
const WW5: i32 = 1609;
const WW6: i32 = 1108;
const WW7: i32 = 565;

#[inline(always)]
fn m(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

fn wmv2_idct_row(b: &mut [i16]) {
    let (b0, b1, b2, b3) = (b[0] as i32, b[1] as i32, b[2] as i32, b[3] as i32);
    let (b4, b5, b6, b7) = (b[4] as i32, b[5] as i32, b[6] as i32, b[7] as i32);
    let a1 = m(WW1, b1).wrapping_add(m(WW7, b7));
    let a7 = m(WW7, b1).wrapping_sub(m(WW1, b7));
    let a5 = m(WW5, b5).wrapping_add(m(WW3, b3));
    let a3 = m(WW3, b5).wrapping_sub(m(WW5, b3));
    let a2 = m(WW2, b2).wrapping_add(m(WW6, b6));
    let a6 = m(WW6, b2).wrapping_sub(m(WW2, b6));
    let a0 = m(WW0, b0).wrapping_add(m(WW0, b4));
    let a4 = m(WW0, b0).wrapping_sub(m(WW0, b4));
    let s1 = (181i32.wrapping_mul(a1.wrapping_sub(a5).wrapping_add(a7).wrapping_sub(a3)).wrapping_add(128)) >> 8;
    let s2 = (181i32.wrapping_mul(a1.wrapping_sub(a5).wrapping_sub(a7).wrapping_add(a3)).wrapping_add(128)) >> 8;
    let r = |v: i32| (v.wrapping_add(1 << 7) >> 8) as i16;
    b[0] = r(a0.wrapping_add(a2).wrapping_add(a1).wrapping_add(a5));
    b[1] = r(a4.wrapping_add(a6).wrapping_add(s1));
    b[2] = r(a4.wrapping_sub(a6).wrapping_add(s2));
    b[3] = r(a0.wrapping_sub(a2).wrapping_add(a7).wrapping_add(a3));
    b[4] = r(a0.wrapping_sub(a2).wrapping_sub(a7).wrapping_sub(a3));
    b[5] = r(a4.wrapping_sub(a6).wrapping_sub(s2));
    b[6] = r(a4.wrapping_add(a6).wrapping_sub(s1));
    b[7] = r(a0.wrapping_add(a2).wrapping_sub(a1).wrapping_sub(a5));
}

fn wmv2_idct_col(b: &mut [i16; 64], c: usize) {
    let g = |r: usize| b[8 * r + c] as i32;
    let a1 = m(WW1, g(1)).wrapping_add(m(WW7, g(7))).wrapping_add(4) >> 3;
    let a7 = m(WW7, g(1)).wrapping_sub(m(WW1, g(7))).wrapping_add(4) >> 3;
    let a5 = m(WW5, g(5)).wrapping_add(m(WW3, g(3))).wrapping_add(4) >> 3;
    let a3 = m(WW3, g(5)).wrapping_sub(m(WW5, g(3))).wrapping_add(4) >> 3;
    let a2 = m(WW2, g(2)).wrapping_add(m(WW6, g(6))).wrapping_add(4) >> 3;
    let a6 = m(WW6, g(2)).wrapping_sub(m(WW2, g(6))).wrapping_add(4) >> 3;
    let a0 = m(WW0, g(0)).wrapping_add(m(WW0, g(4))) >> 3;
    let a4 = m(WW0, g(0)).wrapping_sub(m(WW0, g(4))) >> 3;
    let s1 = (181i32.wrapping_mul(a1.wrapping_sub(a5).wrapping_add(a7).wrapping_sub(a3)).wrapping_add(128)) >> 8;
    let s2 = (181i32.wrapping_mul(a1.wrapping_sub(a5).wrapping_sub(a7).wrapping_add(a3)).wrapping_add(128)) >> 8;
    let r = |v: i32| (v.wrapping_add(1 << 13) >> 14) as i16;
    b[c] = r(a0.wrapping_add(a2).wrapping_add(a1).wrapping_add(a5));
    b[8 + c] = r(a4.wrapping_add(a6).wrapping_add(s1));
    b[16 + c] = r(a4.wrapping_sub(a6).wrapping_add(s2));
    b[24 + c] = r(a0.wrapping_sub(a2).wrapping_add(a7).wrapping_add(a3));
    b[32 + c] = r(a0.wrapping_sub(a2).wrapping_sub(a7).wrapping_sub(a3));
    b[40 + c] = r(a4.wrapping_sub(a6).wrapping_sub(s2));
    b[48 + c] = r(a4.wrapping_add(a6).wrapping_sub(s1));
    b[56 + c] = r(a0.wrapping_add(a2).wrapping_sub(a1).wrapping_sub(a5));
}

fn wmv2_idct(block: &mut [i16; 64]) {
    for r in 0..8 {
        wmv2_idct_row(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..8 {
        wmv2_idct_col(block, c);
    }
}

/// `wmv2_idct_put_c`.
pub fn wmv2_idct_put(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    wmv2_idct(block);
    for r in 0..8 {
        let d = &mut dest[off + r * stride..off + r * stride + 8];
        for c in 0..8 {
            d[c] = clip_u8(block[8 * r + c] as i32);
        }
    }
}

/// `wmv2_idct_add_c`.
pub fn wmv2_idct_add(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    wmv2_idct(block);
    for r in 0..8 {
        let d = &mut dest[off + r * stride..off + r * stride + 8];
        for c in 0..8 {
            d[c] = clip_u8(d[c] as i32 + block[8 * r + c] as i32);
        }
    }
}

// ───────────────────────── simple IDCT (8-bit) ─────────────────────────

const W1: i32 = 22725;
const W2: i32 = 21407;
const W3: i32 = 19266;
const W4: i32 = 16383;
const W5: i32 = 12873;
const W6: i32 = 8867;
const W7: i32 = 4520;
const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;
const DC_SHIFT: u32 = 3;

/// `idctRowCondDC_int16_8bit(row, 0)` (64-bit build: the DC shortcut tests
/// coefficients 1..7 of the row).
fn idct_row_cond_dc(row: &mut [i16]) {
    if row[1..8].iter().all(|&v| v == 0) {
        let t = ((row[0] as i32) << DC_SHIFT) as i16;
        row[..8].fill(t);
        return;
    }
    let r = |i: usize| row[i] as i32;
    let mut a0 = m(W4, r(0)).wrapping_add(1 << (ROW_SHIFT - 1));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(m(W2, r(2)));
    a1 = a1.wrapping_add(m(W6, r(2)));
    a2 = a2.wrapping_sub(m(W6, r(2)));
    a3 = a3.wrapping_sub(m(W2, r(2)));
    let mut b0 = m(W1, r(1)).wrapping_add(m(W3, r(3)));
    let mut b1 = m(W3, r(1)).wrapping_add(m(-W7, r(3)));
    let mut b2 = m(W5, r(1)).wrapping_add(m(-W1, r(3)));
    let mut b3 = m(W7, r(1)).wrapping_add(m(-W5, r(3)));
    if row[4] != 0 || row[5] != 0 || row[6] != 0 || row[7] != 0 {
        a0 = a0.wrapping_add(m(W4, r(4))).wrapping_add(m(W6, r(6)));
        a1 = a1.wrapping_add(m(-W4, r(4))).wrapping_sub(m(W2, r(6)));
        a2 = a2.wrapping_add(m(-W4, r(4))).wrapping_add(m(W2, r(6)));
        a3 = a3.wrapping_add(m(W4, r(4))).wrapping_sub(m(W6, r(6)));
        b0 = b0.wrapping_add(m(W5, r(5))).wrapping_add(m(W7, r(7)));
        b1 = b1.wrapping_add(m(-W1, r(5))).wrapping_add(m(-W5, r(7)));
        b2 = b2.wrapping_add(m(W7, r(5))).wrapping_add(m(W3, r(7)));
        b3 = b3.wrapping_add(m(W3, r(5))).wrapping_add(m(-W1, r(7)));
    }
    row[0] = (a0.wrapping_add(b0) >> ROW_SHIFT) as i16;
    row[7] = (a0.wrapping_sub(b0) >> ROW_SHIFT) as i16;
    row[1] = (a1.wrapping_add(b1) >> ROW_SHIFT) as i16;
    row[6] = (a1.wrapping_sub(b1) >> ROW_SHIFT) as i16;
    row[2] = (a2.wrapping_add(b2) >> ROW_SHIFT) as i16;
    row[5] = (a2.wrapping_sub(b2) >> ROW_SHIFT) as i16;
    row[3] = (a3.wrapping_add(b3) >> ROW_SHIFT) as i16;
    row[4] = (a3.wrapping_sub(b3) >> ROW_SHIFT) as i16;
}

/// `IDCT_COLS` of the template: returns the eight column outputs (before
/// the final shift), in output row order.
fn idct_cols(b: &[i16; 64], c: usize) -> [i32; 8] {
    let col = |r: usize| b[8 * r + c] as i32;
    let mut a0 = m(W4, col(0).wrapping_add((1 << (COL_SHIFT - 1)) / W4));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(m(W2, col(2)));
    a1 = a1.wrapping_add(m(W6, col(2)));
    a2 = a2.wrapping_add(m(-W6, col(2)));
    a3 = a3.wrapping_add(m(-W2, col(2)));
    let mut b0 = m(W1, col(1));
    let mut b1 = m(W3, col(1));
    let mut b2 = m(W5, col(1));
    let mut b3 = m(W7, col(1));
    b0 = b0.wrapping_add(m(W3, col(3)));
    b1 = b1.wrapping_add(m(-W7, col(3)));
    b2 = b2.wrapping_add(m(-W1, col(3)));
    b3 = b3.wrapping_add(m(-W5, col(3)));
    if col(4) != 0 {
        a0 = a0.wrapping_add(m(W4, col(4)));
        a1 = a1.wrapping_add(m(-W4, col(4)));
        a2 = a2.wrapping_add(m(-W4, col(4)));
        a3 = a3.wrapping_add(m(W4, col(4)));
    }
    if col(5) != 0 {
        b0 = b0.wrapping_add(m(W5, col(5)));
        b1 = b1.wrapping_add(m(-W1, col(5)));
        b2 = b2.wrapping_add(m(W7, col(5)));
        b3 = b3.wrapping_add(m(W3, col(5)));
    }
    if col(6) != 0 {
        a0 = a0.wrapping_add(m(W6, col(6)));
        a1 = a1.wrapping_add(m(-W2, col(6)));
        a2 = a2.wrapping_add(m(W2, col(6)));
        a3 = a3.wrapping_add(m(-W6, col(6)));
    }
    if col(7) != 0 {
        b0 = b0.wrapping_add(m(W7, col(7)));
        b1 = b1.wrapping_add(m(-W5, col(7)));
        b2 = b2.wrapping_add(m(W3, col(7)));
        b3 = b3.wrapping_add(m(-W1, col(7)));
    }
    [
        a0.wrapping_add(b0) >> COL_SHIFT,
        a1.wrapping_add(b1) >> COL_SHIFT,
        a2.wrapping_add(b2) >> COL_SHIFT,
        a3.wrapping_add(b3) >> COL_SHIFT,
        a3.wrapping_sub(b3) >> COL_SHIFT,
        a2.wrapping_sub(b2) >> COL_SHIFT,
        a1.wrapping_sub(b1) >> COL_SHIFT,
        a0.wrapping_sub(b0) >> COL_SHIFT,
    ]
}

/// `ff_simple_idct_put_int16_8bit`.
pub fn simple_idct_put(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        idct_row_cond_dc(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..8 {
        let v = idct_cols(block, c);
        for (r, &x) in v.iter().enumerate() {
            dest[off + r * stride + c] = clip_u8(x);
        }
    }
}

/// `ff_simple_idct_add_int16_8bit`.
pub fn simple_idct_add(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        idct_row_cond_dc(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..8 {
        let v = idct_cols(block, c);
        for (r, &x) in v.iter().enumerate() {
            let d = &mut dest[off + r * stride + c];
            *d = clip_u8((*d as i32).wrapping_add(x));
        }
    }
}

// 4-point stages of simple_idct.c (C_FIX/R_FIX evaluated as FFmpeg does).
const C1: i32 = 3784;
const C2: i32 = 1567;
const C3: i32 = 2896;
const C_SHIFT: u32 = 4 + 1 + 12;
const R1: i32 = 30274;
const R2: i32 = 12540;
const R3: i32 = 23170;
const R_SHIFT: u32 = 11;

/// `idct4col_add`.
fn idct4col_add(dest: &mut [u8], off: usize, stride: usize, b: &[i16; 64], c: usize) {
    let a0 = b[c] as i32;
    let a1 = b[8 + c] as i32;
    let a2 = b[16 + c] as i32;
    let a3 = b[24 + c] as i32;
    let c0 = m(a0.wrapping_add(a2), C3).wrapping_add(1 << (C_SHIFT - 1));
    let c2 = m(a0.wrapping_sub(a2), C3).wrapping_add(1 << (C_SHIFT - 1));
    let c1 = m(a1, C1).wrapping_add(m(a3, C2));
    let c3 = m(a1, C2).wrapping_sub(m(a3, C1));
    let v = [
        c0.wrapping_add(c1) >> C_SHIFT,
        c2.wrapping_add(c3) >> C_SHIFT,
        c2.wrapping_sub(c3) >> C_SHIFT,
        c0.wrapping_sub(c1) >> C_SHIFT,
    ];
    for (r, &x) in v.iter().enumerate() {
        let d = &mut dest[off + r * stride + c];
        *d = clip_u8((*d as i32).wrapping_add(x));
    }
}

/// `idct4row`.
fn idct4row(row: &mut [i16]) {
    let a0 = row[0] as i32;
    let a1 = row[1] as i32;
    let a2 = row[2] as i32;
    let a3 = row[3] as i32;
    let c0 = m(a0.wrapping_add(a2), R3).wrapping_add(1 << (R_SHIFT - 1));
    let c2 = m(a0.wrapping_sub(a2), R3).wrapping_add(1 << (R_SHIFT - 1));
    let c1 = m(a1, R1).wrapping_add(m(a3, R2));
    let c3 = m(a1, R2).wrapping_sub(m(a3, R1));
    // Unsigned shift in C; the low 16 bits equal the arithmetic shift's.
    row[0] = ((c0.wrapping_add(c1) as u32) >> R_SHIFT) as i16;
    row[1] = ((c2.wrapping_add(c3) as u32) >> R_SHIFT) as i16;
    row[2] = ((c2.wrapping_sub(c3) as u32) >> R_SHIFT) as i16;
    row[3] = ((c0.wrapping_sub(c1) as u32) >> R_SHIFT) as i16;
}

/// `ff_simple_idct84_add`: 8 wide, 4 tall (coefficients in rows 0..4).
pub fn simple_idct84_add(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..4 {
        idct_row_cond_dc(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..8 {
        idct4col_add(dest, off, stride, block, c);
    }
}

/// `ff_simple_idct48_add`: 4 wide, 8 tall (coefficients in columns 0..4).
pub fn simple_idct48_add(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        idct4row(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..4 {
        let v = idct_cols(block, c);
        for (r, &x) in v.iter().enumerate() {
            let d = &mut dest[off + r * stride + c];
            *d = clip_u8((*d as i32).wrapping_add(x));
        }
    }
}

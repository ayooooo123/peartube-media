// Ported from FFmpeg (commit 2da55bf): libavcodec/simple_idct_template.c
// (BIT_DEPTH 8, 16-bit coefficients: idctRowCondDC, IDCT_COLS,
// idctSparseCol, idctSparseColPut, ff_simple_idct_int16_8bit,
// ff_simple_idct_put_int16_8bit) and libavcodec/simple_idct.c
// (idct4col_put, ff_simple_idct248_put).
// License: LGPL-2.1-or-later

//! The integer IDCTs FFmpeg's DV decoder uses: the C simple IDCT (what
//! `-idct simple` selects; arm64's default NEON one rounds differently),
//! in place or stored, and the 2-4-8 IDCT of interlaced blocks.
//! Arithmetic wraps like the C code's, so no coefficient can overflow.
//! Stores go to `dest[off + row * stride + col]` for an 8x8 area and are
//! skipped whole when the area does not fit `dest`.

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

fn m(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

fn clip_u8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Whether an 8x8 store at `off` with `stride` fits `len` bytes.
fn fits(len: usize, off: usize, stride: usize) -> bool {
    stride.checked_mul(7).and_then(|r| r.checked_add(off)).and_then(|o| o.checked_add(8)).is_some_and(|end| end <= len)
}

/// idctRowCondDC_int16_8bit(row, 0) on a 64-bit build.
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

/// IDCT_COLS on column `c`: its eight outputs, in row order.
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

/// ff_simple_idct_int16_8bit: the block transformed in place.
pub fn simple_idct(block: &mut [i16; 64]) {
    for r in 0..8 {
        idct_row_cond_dc(&mut block[8 * r..8 * r + 8]);
    }
    for c in 0..8 {
        let v = idct_cols(block, c);
        for (r, &x) in v.iter().enumerate() {
            block[8 * r + c] = x as i16;
        }
    }
}

/// ff_simple_idct_put_int16_8bit.
pub fn simple_idct_put(dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]) {
    if !fits(dest.len(), off, stride) {
        return;
    }
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

// simple_idct.c's 2x4x8 constants: C_FIX(x) = (int)(x * (1 << 12) + 0.5).
const C1: i32 = 2676;
const C2: i32 = 1108;
const CN_SHIFT: u32 = 12;
const C_SHIFT: u32 = 4 + 1 + 12;

/// idct4col_put: rows 0, 2, 4, 6 of column `c` of `b` (from row `first`)
/// to `dest[off + k * line_size]`.
fn idct4col_put(dest: &mut [u8], off: usize, line_size: usize, b: &[i16; 64], first: usize, c: usize) {
    let col = |r: usize| b[8 * (first + r) + c] as i32;
    let (a0, a1, a2, a3) = (col(0), col(2), col(4), col(6));
    let c0 = (a0 + a2) * (1 << (CN_SHIFT - 1)) + (1 << (C_SHIFT - 1));
    let c2 = (a0 - a2) * (1 << (CN_SHIFT - 1)) + (1 << (C_SHIFT - 1));
    let c1 = a1 * C1 + a3 * C2;
    let c3 = a1 * C2 - a3 * C1;
    dest[off] = clip_u8((c0 + c1) >> C_SHIFT);
    dest[off + line_size] = clip_u8((c2 + c3) >> C_SHIFT);
    dest[off + 2 * line_size] = clip_u8((c2 - c3) >> C_SHIFT);
    dest[off + 3 * line_size] = clip_u8((c0 - c1) >> C_SHIFT);
}

/// ff_simple_idct248_put: an interlaced block (the two fields' rows
/// interleaved) through the 2-4-8 IDCT.
pub fn simple_idct248_put(dest: &mut [u8], off: usize, line_size: usize, block: &mut [i16; 64]) {
    if !fits(dest.len(), off, line_size) {
        return;
    }
    // butterfly
    for pair in 0..4 {
        for k in 0..8 {
            let (i0, i1) = (16 * pair + k, 16 * pair + 8 + k);
            let (a0, a1) = (block[i0] as i32, block[i1] as i32);
            block[i0] = (a0 + a1) as i16;
            block[i1] = (a0 - a1) as i16;
        }
    }
    // IDCT8 on each line
    for r in 0..8 {
        idct_row_cond_dc(&mut block[8 * r..8 * r + 8]);
    }
    // IDCT4 and store
    for i in 0..8 {
        idct4col_put(dest, off + i, 2 * line_size, block, 0, i);
        idct4col_put(dest, off + line_size + i, 2 * line_size, block, 1, i);
    }
}

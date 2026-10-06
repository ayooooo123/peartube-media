//! DSP for the H.263-based RealVideo decoders: the C `simple` 8x8 IDCT,
//! half-pel motion compensation, H.263 dequantisation and the H.263
//! deblocking filter.
//!
//! Ported from FFmpeg libavcodec/simple_idct_template.c (8-bit, int16),
//! hpel_template.c / hpeldsp.c, mpegvideo_unquantize.c
//! (`dct_unquantize_h263_{intra,inter}_c`) and h263dsp.c at commit 2da55bf;
//! LGPL-2.1-or-later.

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

#[inline]
fn mul(a: i32, b: i32) -> i32 {
    a.wrapping_mul(b)
}

/// `idctRowCondDC` (8-bit, 64-bit DC shortcut).
fn idct_row(row: &mut [i16]) {
    if row[1..8].iter().all(|&v| v == 0) {
        let temp = ((row[0] as i32).wrapping_mul(1 << DC_SHIFT) & 0xffff) as u16 as i16;
        row[..8].fill(temp);
        return;
    }
    let r = |i: usize| row[i] as i32;
    let mut a0 = mul(W4, r(0)).wrapping_add(1 << (ROW_SHIFT - 1));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(mul(W2, r(2)));
    a1 = a1.wrapping_add(mul(W6, r(2)));
    a2 = a2.wrapping_sub(mul(W6, r(2)));
    a3 = a3.wrapping_sub(mul(W2, r(2)));

    let mut b0 = mul(W1, r(1)).wrapping_add(mul(W3, r(3)));
    let mut b1 = mul(W3, r(1)).wrapping_add(mul(-W7, r(3)));
    let mut b2 = mul(W5, r(1)).wrapping_add(mul(-W1, r(3)));
    let mut b3 = mul(W7, r(1)).wrapping_add(mul(-W5, r(3)));

    if r(4) != 0 || r(5) != 0 || r(6) != 0 || r(7) != 0 {
        a0 = a0.wrapping_add(mul(W4, r(4)).wrapping_add(mul(W6, r(6))));
        a1 = a1.wrapping_add(mul(-W4, r(4)).wrapping_sub(mul(W2, r(6))));
        a2 = a2.wrapping_add(mul(-W4, r(4)).wrapping_add(mul(W2, r(6))));
        a3 = a3.wrapping_add(mul(W4, r(4)).wrapping_sub(mul(W6, r(6))));

        b0 = b0.wrapping_add(mul(W5, r(5))).wrapping_add(mul(W7, r(7)));
        b1 = b1.wrapping_add(mul(-W1, r(5))).wrapping_add(mul(-W5, r(7)));
        b2 = b2.wrapping_add(mul(W7, r(5))).wrapping_add(mul(W3, r(7)));
        b3 = b3.wrapping_add(mul(W3, r(5))).wrapping_add(mul(-W1, r(7)));
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

/// `IDCT_COLS` for column `i`: the eight outputs, top to bottom.
fn idct_col(block: &[i16; 64], i: usize) -> [i32; 8] {
    let c = |k: usize| block[i + 8 * k] as i32;
    let mut a0 = mul(W4, c(0).wrapping_add((1 << (COL_SHIFT - 1)) / W4));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(mul(W2, c(2)));
    a1 = a1.wrapping_add(mul(W6, c(2)));
    a2 = a2.wrapping_add(mul(-W6, c(2)));
    a3 = a3.wrapping_add(mul(-W2, c(2)));

    let mut b0 = mul(W1, c(1));
    let mut b1 = mul(W3, c(1));
    let mut b2 = mul(W5, c(1));
    let mut b3 = mul(W7, c(1));
    b0 = b0.wrapping_add(mul(W3, c(3)));
    b1 = b1.wrapping_add(mul(-W7, c(3)));
    b2 = b2.wrapping_add(mul(-W1, c(3)));
    b3 = b3.wrapping_add(mul(-W5, c(3)));

    a0 = a0.wrapping_add(mul(W4, c(4)));
    a1 = a1.wrapping_add(mul(-W4, c(4)));
    a2 = a2.wrapping_add(mul(-W4, c(4)));
    a3 = a3.wrapping_add(mul(W4, c(4)));

    b0 = b0.wrapping_add(mul(W5, c(5)));
    b1 = b1.wrapping_add(mul(-W1, c(5)));
    b2 = b2.wrapping_add(mul(W7, c(5)));
    b3 = b3.wrapping_add(mul(W3, c(5)));

    a0 = a0.wrapping_add(mul(W6, c(6)));
    a1 = a1.wrapping_add(mul(-W2, c(6)));
    a2 = a2.wrapping_add(mul(W2, c(6)));
    a3 = a3.wrapping_add(mul(-W6, c(6)));

    b0 = b0.wrapping_add(mul(W7, c(7)));
    b1 = b1.wrapping_add(mul(-W5, c(7)));
    b2 = b2.wrapping_add(mul(W3, c(7)));
    b3 = b3.wrapping_add(mul(-W1, c(7)));

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
pub fn simple_idct_put(dst: &mut [u8], pos: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        idct_row(&mut block[r * 8..r * 8 + 8]);
    }
    for i in 0..8 {
        let out = idct_col(block, i);
        for (k, v) in out.iter().enumerate() {
            dst[pos + k * stride + i] = (*v).clamp(0, 255) as u8;
        }
    }
}

/// `ff_simple_idct_add_int16_8bit`.
pub fn simple_idct_add(dst: &mut [u8], pos: usize, stride: usize, block: &mut [i16; 64]) {
    for r in 0..8 {
        idct_row(&mut block[r * 8..r * 8 + 8]);
    }
    for i in 0..8 {
        let out = idct_col(block, i);
        for (k, v) in out.iter().enumerate() {
            let p = &mut dst[pos + k * stride + i];
            *p = (*p as i32 + *v).clamp(0, 255) as u8;
        }
    }
}

/// Half-pel prediction flavour (`put_pixels_tab`, `put_no_rnd_pixels_tab`,
/// `avg_pixels_tab`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PixOp {
    Put,
    PutNoRnd,
    Avg,
}

/// `{put,put_no_rnd,avg}_pixels{16,8}{,_x2,_y2,_xy2}_8_c`: a `w`x`h` block
/// from `src` (row pitch `sstride`, top-left at `so`) with half-pel phase
/// `dxy` (bit 0: x, bit 1: y).
#[allow(clippy::too_many_arguments)]
pub fn hpel(dst: &mut [u8], dpos: usize, dstride: usize, src: &[u8], so: usize, sstride: usize, w: usize, h: usize, dxy: usize, op: PixOp) {
    let rnd = op != PixOp::PutNoRnd;
    for y in 0..h {
        let s = so + y * sstride;
        for x in 0..w {
            let a = src[s + x] as u32;
            let v = match dxy {
                0 => a,
                1 => {
                    let b = src[s + x + 1] as u32;
                    (a + b + rnd as u32) >> 1
                }
                2 => {
                    let b = src[s + x + sstride] as u32;
                    (a + b + rnd as u32) >> 1
                }
                _ => {
                    let b = src[s + x + 1] as u32;
                    let c = src[s + x + sstride] as u32;
                    let d = src[s + x + sstride + 1] as u32;
                    (a + b + c + d + if rnd { 2 } else { 1 }) >> 2
                }
            };
            let o = &mut dst[dpos + y * dstride + x];
            *o = if op == PixOp::Avg { ((*o as u32 + v + 1) >> 1) as u8 } else { v as u8 };
        }
    }
}

/// `dct_unquantize_h263_intra_c`. `n_coeffs` is
/// `intra_scantable.raster_end[block_last_index]` (or 63 with AC
/// prediction); `dc_scale` is `None` with advanced intra coding.
pub fn unquantize_h263_intra(block: &mut [i16; 64], qscale: i32, dc_scale: Option<i32>, n_coeffs: usize) {
    let qmul = qscale << 1;
    let qadd = match dc_scale {
        Some(scale) => {
            block[0] = (block[0] as i32).wrapping_mul(scale) as i16;
            (qscale - 1) | 1
        }
        None => 0,
    };
    for v in &mut block[1..=n_coeffs] {
        let level = *v as i32;
        if level != 0 {
            let level = if level < 0 { level * qmul - qadd } else { level * qmul + qadd };
            *v = level as i16;
        }
    }
}

/// `dct_unquantize_h263_inter_c`.
pub fn unquantize_h263_inter(block: &mut [i16; 64], qscale: i32, n_coeffs: usize) {
    let qadd = (qscale - 1) | 1;
    let qmul = qscale << 1;
    for v in &mut block[..=n_coeffs] {
        let level = *v as i32;
        if level != 0 {
            let level = if level < 0 { level * qmul - qadd } else { level * qmul + qadd };
            *v = level as i16;
        }
    }
}

const H263_LOOP_FILTER_STRENGTH: [i32; 32] = [0, 1, 1, 2, 2, 3, 3, 4, 4, 4, 5, 5, 6, 6, 7, 7, 7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12, 12, 12];

/// One line of the H.263 deblocking filter across the edge between
/// samples `p1 = src[-step]` and `p2 = src[0]`.
#[inline]
fn h263_filter_line(buf: &mut [u8], src: usize, step: usize, strength: i32) {
    let p0 = buf[src - 2 * step] as i32;
    let mut p1 = buf[src - step] as i32;
    let mut p2 = buf[src] as i32;
    let p3 = buf[src + step] as i32;
    let d = (p0 - p3 + 4 * (p2 - p1)) / 8;
    let d1 = if d < -2 * strength {
        0
    } else if d < -strength {
        -2 * strength - d
    } else if d < strength {
        d
    } else if d < 2 * strength {
        2 * strength - d
    } else {
        0
    };
    p1 += d1;
    p2 -= d1;
    if p1 & 256 != 0 {
        p1 = !(p1 >> 31);
    }
    if p2 & 256 != 0 {
        p2 = !(p2 >> 31);
    }
    buf[src - step] = p1 as u8;
    buf[src] = p2 as u8;
    let ad1 = d1.abs() >> 1;
    let d2 = ((p0 - p3) / 4).clamp(-ad1, ad1);
    buf[src - 2 * step] = (p0 - d2) as u8;
    buf[src + step] = (p3 + d2) as u8;
}

/// `h263_h_loop_filter_c`: filters the vertical edge left of `src` over
/// 8 rows.
pub fn h263_h_loop_filter(buf: &mut [u8], src: usize, stride: usize, qscale: i32) {
    let strength = H263_LOOP_FILTER_STRENGTH[(qscale & 31) as usize];
    for y in 0..8 {
        h263_filter_line(buf, src + y * stride, 1, strength);
    }
}

/// `h263_v_loop_filter_c`: filters the horizontal edge above `src` over
/// 8 columns.
pub fn h263_v_loop_filter(buf: &mut [u8], src: usize, stride: usize, qscale: i32) {
    let strength = H263_LOOP_FILTER_STRENGTH[(qscale & 31) as usize];
    for x in 0..8 {
        h263_filter_line(buf, src + x, stride, strength);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Floating-point reference IDCT, as FFmpeg's dct-test compares with.
    fn ref_idct(block: &[i16; 64]) -> [f64; 64] {
        let mut out = [0.0; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0;
                for v in 0..8 {
                    for u in 0..8 {
                        let cu = if u == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
                        let cv = if v == 0 { std::f64::consts::FRAC_1_SQRT_2 } else { 1.0 };
                        s += cu
                            * cv
                            * block[v * 8 + u] as f64
                            * (((2 * x + 1) as f64 * u as f64 * std::f64::consts::PI) / 16.0).cos()
                            * (((2 * y + 1) as f64 * v as f64 * std::f64::consts::PI) / 16.0).cos();
                    }
                }
                out[y * 8 + x] = s / 4.0;
            }
        }
        out
    }

    #[test]
    fn idct_put_tracks_reference() {
        let mut seed = 12345u32;
        for _ in 0..200 {
            let mut block = [0i16; 64];
            for v in block.iter_mut().take(20) {
                seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
                *v = ((seed >> 16) % 64) as i16 - 32;
            }
            block[0] += 512;
            let reference = ref_idct(&block);
            let mut dst = [0u8; 64];
            let mut b = block;
            simple_idct_put(&mut dst, 0, 8, &mut b);
            for i in 0..64 {
                let r = reference[i].round().clamp(0.0, 255.0);
                assert!((dst[i] as f64 - r).abs() <= 1.0, "pixel {i}: {} vs {r}", dst[i]);
            }
        }
    }
}

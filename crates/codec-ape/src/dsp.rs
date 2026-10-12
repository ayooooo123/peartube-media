// Ported from FFmpeg libavcodec/lossless_audiodsp.c and libavcodec/bswapdsp.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

/// Vector scalar product and multiply-add for 16-bit integers.
///
/// Ported from `scalarproduct_and_madd_int16_c` in `lossless_audiodsp.c`.
#[inline]
pub fn scalarproduct_and_madd_int16(
    v1: &mut [i16],
    v2: &[i16],
    v3: &[i16],
    order: usize,
    mul: i32,
) -> i32 {
    let mut res: u32 = 0;
    for i in 0..order {
        res = res.wrapping_add((v1[i] as i32).wrapping_mul(v2[i] as i32) as u32);
        v1[i] = (v1[i] as i32).wrapping_add(mul.wrapping_mul(v3[i] as i32)) as i16;
    }
    res as i32
}

/// Swaps 32-bit words in place or into a destination buffer.
///
/// Ported from `bswap_buf` in `bswapdsp.c`.
#[inline]
pub fn bswap_buf(dst: &mut [u8], src: &[u8]) {
    let n_words = (src.len() & !3) / 4;
    for i in 0..n_words {
        dst[i * 4] = src[i * 4 + 3];
        dst[i * 4 + 1] = src[i * 4 + 2];
        dst[i * 4 + 2] = src[i * 4 + 1];
        dst[i * 4 + 3] = src[i * 4];
    }
}

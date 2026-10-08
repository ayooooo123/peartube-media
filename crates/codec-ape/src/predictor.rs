// Ported from FFmpeg libavcodec/apedec.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

use crate::filter::{ape_apply_filters, APEFilter, APE_FILTER_LEVELS};

pub const HISTORY_SIZE: usize = 512;
pub const PREDICTOR_ORDER: usize = 8;
pub const PREDICTOR_SIZE: usize = 50;
pub const PREDICTOR_BUF_SIZE: usize = HISTORY_SIZE + PREDICTOR_SIZE + 16; // 578

pub const YDELAYA: usize = 18 + PREDICTOR_ORDER * 4; // 50
pub const YDELAYB: usize = 18 + PREDICTOR_ORDER * 3; // 42
pub const XDELAYA: usize = 18 + PREDICTOR_ORDER * 2; // 34
pub const XDELAYB: usize = 18 + PREDICTOR_ORDER;     // 26

pub const YADAPTCOEFFSA: usize = 18;
pub const XADAPTCOEFFSA: usize = 14;
pub const YADAPTCOEFFSB: usize = 10;
pub const XADAPTCOEFFSB: usize = 5;

/// Get inverse sign of integer (-1 for positive, 1 for negative, 0 for zero).
#[inline(always)]
pub fn ape_sign(x: i32) -> i32 {
    (x < 0) as i32 - (x > 0) as i32
}

/// 64-bit inverse sign of integer.
#[inline(always)]
pub fn ape_sign64(x: i64) -> i64 {
    (x < 0) as i64 - (x > 0) as i64
}

#[derive(Clone)]
#[allow(non_snake_case)]
pub struct APEPredictor {
    pub buf_offset: usize,
    pub lastA: [i32; 2],
    pub filterA: [i32; 2],
    pub filterB: [i32; 2],
    pub coeffsA: [[i32; 4]; 2],
    pub coeffsB: [[i32; 5]; 2],
    pub historybuffer: [i32; PREDICTOR_BUF_SIZE],
    pub sample_pos: u32,
}

impl Default for APEPredictor {
    fn default() -> Self {
        Self {
            buf_offset: 0,
            lastA: [0; 2],
            filterA: [0; 2],
            filterB: [0; 2],
            coeffsA: [[0; 4]; 2],
            coeffsB: [[0; 5]; 2],
            historybuffer: [0; PREDICTOR_BUF_SIZE],
            sample_pos: 0,
        }
    }
}

#[derive(Clone)]
#[allow(non_snake_case)]
pub struct APEPredictor64 {
    pub buf_offset: usize,
    pub lastA: [i64; 2],
    pub filterA: [i64; 2],
    pub filterB: [i64; 2],
    pub coeffsA: [[i64; 4]; 2],
    pub coeffsB: [[i64; 5]; 2],
    pub historybuffer: [i64; PREDICTOR_BUF_SIZE],
}

impl Default for APEPredictor64 {
    fn default() -> Self {
        Self {
            buf_offset: 0,
            lastA: [0; 2],
            filterA: [0; 2],
            filterB: [0; 2],
            coeffsA: [[0; 4]; 2],
            coeffsB: [[0; 5]; 2],
            historybuffer: [0; PREDICTOR_BUF_SIZE],
        }
    }
}

static INITIAL_COEFFS_FAST_3320: [i32; 1] = [375];
static INITIAL_COEFFS_A_3800: [i32; 3] = [64, 115, 64];
static INITIAL_COEFFS_B_3800: [i32; 2] = [740, 0];
static INITIAL_COEFFS_3930: [i32; 4] = [360, 317, -109, 98];
static INITIAL_COEFFS_3930_64BIT: [i64; 4] = [360, 317, -109, 98];

pub fn init_predictor_decoder(
    fileversion: i32,
    compression_level: i32,
    p: &mut APEPredictor,
    p64: &mut APEPredictor64,
) {
    p.historybuffer.fill(0);
    p64.historybuffer.fill(0);
    p.buf_offset = 0;
    p64.buf_offset = 0;

    p.coeffsA = [[0; 4]; 2];
    p64.coeffsA = [[0; 4]; 2];
    p.coeffsB = [[0; 5]; 2];
    p64.coeffsB = [[0; 5]; 2];

    if fileversion < 3930 {
        if compression_level == 1000 {
            p.coeffsA[0][0] = INITIAL_COEFFS_FAST_3320[0];
            p.coeffsA[1][0] = INITIAL_COEFFS_FAST_3320[0];
        } else {
            p.coeffsA[0][..3].copy_from_slice(&INITIAL_COEFFS_A_3800);
            p.coeffsA[1][..3].copy_from_slice(&INITIAL_COEFFS_A_3800);
        }
        p.coeffsB[0][..2].copy_from_slice(&INITIAL_COEFFS_B_3800);
        p.coeffsB[1][..2].copy_from_slice(&INITIAL_COEFFS_B_3800);
    } else {
        p.coeffsA[0].copy_from_slice(&INITIAL_COEFFS_3930);
        p.coeffsA[1].copy_from_slice(&INITIAL_COEFFS_3930);
        p64.coeffsA[0].copy_from_slice(&INITIAL_COEFFS_3930_64BIT);
        p64.coeffsA[1].copy_from_slice(&INITIAL_COEFFS_3930_64BIT);
    }

    p.filterA = [0; 2];
    p.filterB = [0; 2];
    p.lastA = [0; 2];

    p64.filterA = [0; 2];
    p64.filterB = [0; 2];
    p64.lastA = [0; 2];

    p.sample_pos = 0;
}

#[inline(always)]
pub fn filter_fast_3320(
    p: &mut APEPredictor,
    decoded: i32,
    filter: usize,
    delay_a: usize,
) -> i32 {
    let buf_a = p.buf_offset + delay_a;
    p.historybuffer[buf_a] = p.lastA[filter];
    if p.sample_pos < 3 {
        p.lastA[filter] = decoded;
        p.filterA[filter] = decoded;
        return decoded;
    }
    let pred_a = (p.historybuffer[buf_a] as u32)
        .wrapping_mul(2)
        .wrapping_sub(p.historybuffer[buf_a - 1] as u32) as i32;
    p.lastA[filter] = (decoded as u32).wrapping_add(
        ((pred_a.wrapping_mul(p.coeffsA[filter][0])) >> 9) as u32,
    ) as i32;

    if (decoded ^ pred_a) > 0 {
        p.coeffsA[filter][0] = p.coeffsA[filter][0].wrapping_add(1);
    } else {
        p.coeffsA[filter][0] = p.coeffsA[filter][0].wrapping_sub(1);
    }

    p.filterA[filter] = (p.filterA[filter] as u32).wrapping_add(p.lastA[filter] as u32) as i32;
    p.filterA[filter]
}

#[inline(always)]
pub fn filter_3800(
    p: &mut APEPredictor,
    decoded: i32,
    filter: usize,
    delay_a: usize,
    delay_b: usize,
    start: u32,
    shift: u32,
) -> i32 {
    let buf_a = p.buf_offset + delay_a;
    let buf_b = p.buf_offset + delay_b;
    p.historybuffer[buf_a] = p.lastA[filter];
    p.historybuffer[buf_b] = p.filterB[filter];
    if p.sample_pos < start {
        let pred_a = (decoded as u32).wrapping_add(p.filterA[filter] as u32) as i32;
        p.lastA[filter] = decoded;
        p.filterB[filter] = decoded;
        p.filterA[filter] = pred_a;
        return pred_a;
    }

    let d2 = p.historybuffer[buf_a];
    let d1 = ((p.historybuffer[buf_a] as u32).wrapping_sub(p.historybuffer[buf_a - 1] as u32)).wrapping_mul(2) as i32;
    let d0 = (p.historybuffer[buf_a] as u32).wrapping_add(
        ((p.historybuffer[buf_a - 2] as u32).wrapping_sub(p.historybuffer[buf_a - 1] as u32)).wrapping_mul(8)
    ) as i32;
    let d3 = (p.historybuffer[buf_b] as u32).wrapping_mul(2).wrapping_sub(p.historybuffer[buf_b - 1] as u32) as i32;
    let d4 = p.historybuffer[buf_b];

    let pred_a = d0.wrapping_mul(p.coeffsA[filter][0])
        .wrapping_add(d1.wrapping_mul(p.coeffsA[filter][1]))
        .wrapping_add(d2.wrapping_mul(p.coeffsA[filter][2]));

    let sign = ape_sign(decoded);
    p.coeffsA[filter][0] = p.coeffsA[filter][0].wrapping_add((((d0 >> 30) & 2) - 1).wrapping_mul(sign));
    p.coeffsA[filter][1] = p.coeffsA[filter][1].wrapping_add((((d1 >> 28) & 8) - 4).wrapping_mul(sign));
    p.coeffsA[filter][2] = p.coeffsA[filter][2].wrapping_add((((d2 >> 28) & 8) - 4).wrapping_mul(sign));

    let pred_b = d3.wrapping_mul(p.coeffsB[filter][0])
        .wrapping_sub(d4.wrapping_mul(p.coeffsB[filter][1]));

    p.lastA[filter] = (decoded as u32).wrapping_add((pred_a >> 11) as u32) as i32;
    let sign_b = ape_sign(p.lastA[filter]);
    p.coeffsB[filter][0] = p.coeffsB[filter][0].wrapping_add((((d3 >> 29) & 4) - 2).wrapping_mul(sign_b));
    p.coeffsB[filter][1] = p.coeffsB[filter][1].wrapping_sub((((d4 >> 30) & 2) - 1).wrapping_mul(sign_b));

    p.filterB[filter] = (p.lastA[filter] as u32).wrapping_add((pred_b >> shift) as u32) as i32;
    let scaled_fa = ((p.filterA[filter] as u32).wrapping_mul(31) as i32) >> 5;
    p.filterA[filter] = (p.filterB[filter] as u32).wrapping_add(scaled_fa as u32) as i32;

    p.filterA[filter]
}

pub fn long_filter_high_3800(buffer: &mut [i32], order: usize, shift: u32, length: usize) {
    if order >= length {
        return;
    }
    let mut coeffs = [0i32; 256];
    let mut delay = [0i32; 512];
    for i in 0..order {
        delay[i] = buffer[i];
    }
    let mut delay_offset = 0usize;
    for i in order..length {
        let mut dotprod: i32 = 0;
        let sign = ape_sign(buffer[i]);
        if sign == 1 {
            for j in 0..order {
                let d = delay[delay_offset + j];
                dotprod = dotprod.wrapping_add((d as u32).wrapping_mul(coeffs[j] as u32) as i32);
                let inc = if d >= 0 { 1 } else { -1 };
                coeffs[j] = coeffs[j].wrapping_add(inc);
            }
        } else if sign == -1 {
            for j in 0..order {
                let d = delay[delay_offset + j];
                dotprod = dotprod.wrapping_add((d as u32).wrapping_mul(coeffs[j] as u32) as i32);
                let inc = if d >= 0 { 1 } else { -1 };
                coeffs[j] = coeffs[j].wrapping_sub(inc);
            }
        } else {
            for j in 0..order {
                let d = delay[delay_offset + j];
                dotprod = dotprod.wrapping_add((d as u32).wrapping_mul(coeffs[j] as u32) as i32);
            }
        }
        buffer[i] = (buffer[i] as u32).wrapping_sub((dotprod >> shift) as u32) as i32;
        delay_offset += 1;
        delay[delay_offset + order - 1] = buffer[i];
        if delay_offset == 256 {
            delay.copy_within(256..512, 0);
            delay_offset = 0;
        }
    }
}

pub fn long_filter_ehigh_3830(buffer: &mut [i32]) {
    let mut delay = [0i32; 8];
    let mut coeffs = [0u32; 8];

    for val in buffer.iter_mut() {
        let mut dotprod: i32 = 0;
        let sign = ape_sign(*val);
        for j in (0..=7).rev() {
            dotprod = dotprod.wrapping_add((delay[j] as u32).wrapping_mul(coeffs[j]) as i32);
            let inc = (if delay[j] >= 0 { 1i32 } else { -1i32 }).wrapping_mul(sign);
            coeffs[j] = coeffs[j].wrapping_add(inc as u32);
        }
        for j in (1..=7).rev() {
            delay[j] = delay[j - 1];
        }
        delay[0] = *val;
        *val = (*val as u32).wrapping_sub((dotprod >> 9) as u32) as i32;
    }
}

pub fn predictor_decode_stereo_3800(
    fileversion: i32,
    compression_level: i32,
    p: &mut APEPredictor,
    decoded0: &mut [i32],
    decoded1: &mut [i32],
    count: usize,
) {
    let mut start = 4u32;
    let mut shift = 10u32;

    if compression_level == 3000 {
        start = 16;
        long_filter_high_3800(decoded0, 16, 9, count);
        long_filter_high_3800(decoded1, 16, 9, count);
    } else if compression_level == 4000 {
        let mut order = 128usize;
        let mut shift2 = 11u32;

        if fileversion >= 3830 {
            order <<= 1;
            shift += 1;
            shift2 += 1;
            if count > order {
                long_filter_ehigh_3830(&mut decoded0[order..count]);
                long_filter_ehigh_3830(&mut decoded1[order..count]);
            }
        }
        start = order as u32;
        long_filter_high_3800(decoded0, order, shift2, count);
        long_filter_high_3800(decoded1, order, shift2, count);
    }

    for i in 0..count {
        let x = decoded0[i];
        let y = decoded1[i];
        if compression_level == 1000 {
            decoded0[i] = filter_fast_3320(p, y, 0, YDELAYA);
            decoded1[i] = filter_fast_3320(p, x, 1, XDELAYA);
        } else {
            decoded0[i] = filter_3800(p, y, 0, YDELAYA, YDELAYB, start, shift);
            decoded1[i] = filter_3800(p, x, 1, XDELAYA, XDELAYB, start, shift);
        }

        p.buf_offset += 1;
        p.sample_pos = p.sample_pos.wrapping_add(1);

        if p.buf_offset == HISTORY_SIZE {
            p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
            p.buf_offset = 0;
        }
    }
}

pub fn predictor_decode_mono_3800(
    fileversion: i32,
    compression_level: i32,
    p: &mut APEPredictor,
    decoded0: &mut [i32],
    count: usize,
) {
    let mut start = 4u32;
    let mut shift = 10u32;

    if compression_level == 3000 {
        start = 16;
        long_filter_high_3800(decoded0, 16, 9, count);
    } else if compression_level == 4000 {
        let mut order = 128usize;
        let mut shift2 = 11u32;

        if fileversion >= 3830 {
            order <<= 1;
            shift += 1;
            shift2 += 1;
            if count > order {
                long_filter_ehigh_3830(&mut decoded0[order..count]);
            }
        }
        start = order as u32;
        long_filter_high_3800(decoded0, order, shift2, count);
    }

    for i in 0..count {
        if compression_level == 1000 {
            decoded0[i] = filter_fast_3320(p, decoded0[i], 0, YDELAYA);
        } else {
            decoded0[i] = filter_3800(p, decoded0[i], 0, YDELAYA, YDELAYB, start, shift);
        }

        p.buf_offset += 1;
        p.sample_pos = p.sample_pos.wrapping_add(1);

        if p.buf_offset == HISTORY_SIZE {
            p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
            p.buf_offset = 0;
        }
    }
}

#[inline(always)]
pub fn predictor_update_3930(
    p: &mut APEPredictor,
    decoded: i32,
    filter: usize,
    delay_a: usize,
) -> i32 {
    let buf_a = p.buf_offset + delay_a;
    p.historybuffer[buf_a] = p.lastA[filter];
    let d0 = p.historybuffer[buf_a];
    let d1 = (p.historybuffer[buf_a] as u32).wrapping_sub(p.historybuffer[buf_a - 1] as u32) as i32;
    let d2 = (p.historybuffer[buf_a - 1] as u32).wrapping_sub(p.historybuffer[buf_a - 2] as u32) as i32;
    let d3 = (p.historybuffer[buf_a - 2] as u32).wrapping_sub(p.historybuffer[buf_a - 3] as u32) as i32;

    let pred_a = d0.wrapping_mul(p.coeffsA[filter][0])
        .wrapping_add(d1.wrapping_mul(p.coeffsA[filter][1]))
        .wrapping_add(d2.wrapping_mul(p.coeffsA[filter][2]))
        .wrapping_add(d3.wrapping_mul(p.coeffsA[filter][3]));

    p.lastA[filter] = (decoded as u32).wrapping_add((pred_a >> 9) as u32) as i32;
    let scaled_fa = ((p.filterA[filter] as u32).wrapping_mul(31) as i32) >> 5;
    p.filterA[filter] = (p.lastA[filter] as u32).wrapping_add(scaled_fa as u32) as i32;

    let sign = ape_sign(decoded);
    p.coeffsA[filter][0] = p.coeffsA[filter][0].wrapping_add(((d0 < 0) as i32 * 2 - 1).wrapping_mul(sign));
    p.coeffsA[filter][1] = p.coeffsA[filter][1].wrapping_add(((d1 < 0) as i32 * 2 - 1).wrapping_mul(sign));
    p.coeffsA[filter][2] = p.coeffsA[filter][2].wrapping_add(((d2 < 0) as i32 * 2 - 1).wrapping_mul(sign));
    p.coeffsA[filter][3] = p.coeffsA[filter][3].wrapping_add(((d3 < 0) as i32 * 2 - 1).wrapping_mul(sign));

    p.filterA[filter]
}

pub fn predictor_decode_stereo_3930(
    version: i32,
    fset: usize,
    filters: &mut [[APEFilter; 2]; APE_FILTER_LEVELS],
    p: &mut APEPredictor,
    decoded0: &mut [i32],
    decoded1: &mut [i32],
    count: usize,
) {
    ape_apply_filters(version, fset, filters, decoded0, Some(decoded1));

    for i in 0..count {
        let y = decoded1[i];
        let x = decoded0[i];
        decoded0[i] = predictor_update_3930(p, y, 0, YDELAYA);
        decoded1[i] = predictor_update_3930(p, x, 1, XDELAYA);

        p.buf_offset += 1;

        if p.buf_offset == HISTORY_SIZE {
            p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
            p.buf_offset = 0;
        }
    }
}

pub fn predictor_decode_mono_3930(
    version: i32,
    fset: usize,
    filters: &mut [[APEFilter; 2]; APE_FILTER_LEVELS],
    p: &mut APEPredictor,
    decoded0: &mut [i32],
    count: usize,
) {
    ape_apply_filters(version, fset, filters, decoded0, None);

    for i in 0..count {
        decoded0[i] = predictor_update_3930(p, decoded0[i], 0, YDELAYA);

        p.buf_offset += 1;

        if p.buf_offset == HISTORY_SIZE {
            p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
            p.buf_offset = 0;
        }
    }
}

#[inline(always)]
pub fn predictor_update_filter(
    p: &mut APEPredictor64,
    decoded: i32,
    filter: usize,
    delay_a: usize,
    delay_b: usize,
    adapt_a: usize,
    adapt_b: usize,
    interim_mode: bool,
) -> i32 {
    let buf_a = p.buf_offset + delay_a;
    let buf_b = p.buf_offset + delay_b;
    let buf_adapt_a = p.buf_offset + adapt_a;
    let buf_adapt_b = p.buf_offset + adapt_b;

    p.historybuffer[buf_a] = p.lastA[filter];
    p.historybuffer[buf_adapt_a] = ape_sign64(p.historybuffer[buf_a]);
    p.historybuffer[buf_a - 1] = (p.historybuffer[buf_a] as u64)
        .wrapping_sub(p.historybuffer[buf_a - 1] as u64) as i64;
    p.historybuffer[buf_adapt_a - 1] = ape_sign64(p.historybuffer[buf_a - 1]);

    let pred_a = (p.historybuffer[buf_a] as u64).wrapping_mul(p.coeffsA[filter][0] as u64)
        .wrapping_add((p.historybuffer[buf_a - 1] as u64).wrapping_mul(p.coeffsA[filter][1] as u64))
        .wrapping_add((p.historybuffer[buf_a - 2] as u64).wrapping_mul(p.coeffsA[filter][2] as u64))
        .wrapping_add((p.historybuffer[buf_a - 3] as u64).wrapping_mul(p.coeffsA[filter][3] as u64)) as i64;

    let other = filter ^ 1;
    let scaled_fb = ((p.filterB[filter] as u64).wrapping_mul(31) as i64) >> 5;
    p.historybuffer[buf_b] = p.filterA[other].wrapping_sub(scaled_fb);
    p.historybuffer[buf_adapt_b] = ape_sign64(p.historybuffer[buf_b]);
    p.historybuffer[buf_b - 1] = (p.historybuffer[buf_b] as u64)
        .wrapping_sub(p.historybuffer[buf_b - 1] as u64) as i64;
    p.historybuffer[buf_adapt_b - 1] = ape_sign64(p.historybuffer[buf_b - 1]);
    p.filterB[filter] = p.filterA[other];

    let pred_b = (p.historybuffer[buf_b] as u64).wrapping_mul(p.coeffsB[filter][0] as u64)
        .wrapping_add((p.historybuffer[buf_b - 1] as u64).wrapping_mul(p.coeffsB[filter][1] as u64))
        .wrapping_add((p.historybuffer[buf_b - 2] as u64).wrapping_mul(p.coeffsB[filter][2] as u64))
        .wrapping_add((p.historybuffer[buf_b - 3] as u64).wrapping_mul(p.coeffsB[filter][3] as u64))
        .wrapping_add((p.historybuffer[buf_b - 4] as u64).wrapping_mul(p.coeffsB[filter][4] as u64)) as i64;

    if !interim_mode {
        let pa = pred_a as i32;
        let pb = pred_b as i32;
        p.lastA[filter] = (decoded as i32).wrapping_add(
            (pa as u32).wrapping_add((pb >> 1) as u32) as i32 >> 10
        ) as i64;
    } else {
        let sum = (pred_a as u64).wrapping_add((pred_b >> 1) as u64) as i64;
        p.lastA[filter] = (decoded as i64).wrapping_add(sum >> 10);
    }

    let scaled_fa = ((p.filterA[filter] as u64).wrapping_mul(31) as i64) >> 5;
    p.filterA[filter] = p.lastA[filter].wrapping_add(scaled_fa);

    let sign = ape_sign(decoded) as i64;
    p.coeffsA[filter][0] = p.coeffsA[filter][0].wrapping_add(p.historybuffer[buf_adapt_a].wrapping_mul(sign));
    p.coeffsA[filter][1] = p.coeffsA[filter][1].wrapping_add(p.historybuffer[buf_adapt_a - 1].wrapping_mul(sign));
    p.coeffsA[filter][2] = p.coeffsA[filter][2].wrapping_add(p.historybuffer[buf_adapt_a - 2].wrapping_mul(sign));
    p.coeffsA[filter][3] = p.coeffsA[filter][3].wrapping_add(p.historybuffer[buf_adapt_a - 3].wrapping_mul(sign));

    p.coeffsB[filter][0] = p.coeffsB[filter][0].wrapping_add(p.historybuffer[buf_adapt_b].wrapping_mul(sign));
    p.coeffsB[filter][1] = p.coeffsB[filter][1].wrapping_add(p.historybuffer[buf_adapt_b - 1].wrapping_mul(sign));
    p.coeffsB[filter][2] = p.coeffsB[filter][2].wrapping_add(p.historybuffer[buf_adapt_b - 2].wrapping_mul(sign));
    p.coeffsB[filter][3] = p.coeffsB[filter][3].wrapping_add(p.historybuffer[buf_adapt_b - 3].wrapping_mul(sign));
    p.coeffsB[filter][4] = p.coeffsB[filter][4].wrapping_add(p.historybuffer[buf_adapt_b - 4].wrapping_mul(sign));

    p.filterA[filter] as i32
}

pub fn predictor_decode_stereo_3950(
    version: i32,
    fset: usize,
    filters: &mut [[APEFilter; 2]; APE_FILTER_LEVELS],
    p_default: &mut APEPredictor64,
    interim_mode_state: &mut i32,
    interim: &mut [Vec<i32>; 2],
    decoded0: &mut [i32],
    decoded1: &mut [i32],
    count: usize,
) {
    ape_apply_filters(version, fset, filters, decoded0, Some(decoded1));

    let mut p_interim = p_default.clone();
    let mut num_passes = 1;
    if *interim_mode_state == -1 {
        num_passes += 1;
        interim[0].clear();
        interim[0].extend_from_slice(&decoded0[..count]);
        interim[1].clear();
        interim[1].extend_from_slice(&decoded1[..count]);
    }

    for pass in 0..num_passes {
        let interim_mode = *interim_mode_state > 0 || pass > 0;
        let (first, second) = interim.split_at_mut(1);
        let (p, d0, d1) = if pass > 0 {
            (&mut p_interim, &mut first[0][..count], &mut second[0][..count])
        } else {
            (&mut *p_default, &mut decoded0[..count], &mut decoded1[..count])
        };
        p.buf_offset = 0;

        for i in 0..count {
            let a0 = predictor_update_filter(
                p, d0[i], 0, YDELAYA, YDELAYB, YADAPTCOEFFSA, YADAPTCOEFFSB, interim_mode,
            );
            let a1 = predictor_update_filter(
                p, d1[i], 1, XDELAYA, XDELAYB, XADAPTCOEFFSA, XADAPTCOEFFSB, interim_mode,
            );
            d0[i] = a0;
            d1[i] = a1;

            if num_passes > 1 {
                let half_a0 = (a0 / 2) as u32;
                let left = (a1 as u32).wrapping_sub(half_a0) as i32;
                let right = (left as u32).wrapping_add(a0 as u32) as i32;

                let abs_left = left.abs_diff(0);
                let abs_right = right.abs_diff(0);
                if abs_left > (1 << 23) || abs_right > (1 << 23) {
                    *interim_mode_state = if interim_mode { 0 } else { 1 };
                    break;
                }
            }

            p.buf_offset += 1;

            if p.buf_offset == HISTORY_SIZE {
                p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
                p.buf_offset = 0;
            }
        }
    }

    if num_passes > 1 && *interim_mode_state > 0 {
        decoded0[..count].copy_from_slice(&interim[0][..count]);
        decoded1[..count].copy_from_slice(&interim[1][..count]);
        *p_default = p_interim;
        p_default.buf_offset = 0;
    }
}

pub fn predictor_decode_mono_3950(
    version: i32,
    fset: usize,
    filters: &mut [[APEFilter; 2]; APE_FILTER_LEVELS],
    p: &mut APEPredictor64,
    decoded0: &mut [i32],
    count: usize,
) {
    ape_apply_filters(version, fset, filters, decoded0, None);

    let mut current_a = p.lastA[0];

    for i in 0..count {
        let a = decoded0[i];

        let buf_y = p.buf_offset + YDELAYA;
        p.historybuffer[buf_y] = current_a;
        p.historybuffer[buf_y - 1] = (p.historybuffer[buf_y] as u64)
            .wrapping_sub(p.historybuffer[buf_y - 1] as u64) as i64;

        let pred_a = (p.historybuffer[buf_y] as u64).wrapping_mul(p.coeffsA[0][0] as u64)
            .wrapping_add((p.historybuffer[buf_y - 1] as u64).wrapping_mul(p.coeffsA[0][1] as u64))
            .wrapping_add((p.historybuffer[buf_y - 2] as u64).wrapping_mul(p.coeffsA[0][2] as u64))
            .wrapping_add((p.historybuffer[buf_y - 3] as u64).wrapping_mul(p.coeffsA[0][3] as u64)) as i64;

        current_a = (a as i64).wrapping_add(pred_a >> 10);

        let buf_adapt = p.buf_offset + YADAPTCOEFFSA;
        p.historybuffer[buf_adapt] = ape_sign64(p.historybuffer[buf_y]);
        p.historybuffer[buf_adapt - 1] = ape_sign64(p.historybuffer[buf_y - 1]);

        let sign = ape_sign(a) as i64;
        p.coeffsA[0][0] = p.coeffsA[0][0].wrapping_add(p.historybuffer[buf_adapt].wrapping_mul(sign));
        p.coeffsA[0][1] = p.coeffsA[0][1].wrapping_add(p.historybuffer[buf_adapt - 1].wrapping_mul(sign));
        p.coeffsA[0][2] = p.coeffsA[0][2].wrapping_add(p.historybuffer[buf_adapt - 2].wrapping_mul(sign));
        p.coeffsA[0][3] = p.coeffsA[0][3].wrapping_add(p.historybuffer[buf_adapt - 3].wrapping_mul(sign));

        p.buf_offset += 1;

        if p.buf_offset == HISTORY_SIZE {
            p.historybuffer.copy_within(HISTORY_SIZE..HISTORY_SIZE + PREDICTOR_SIZE, 0);
            p.buf_offset = 0;
        }

        let scaled_fa = ((p.filterA[0] as u64).wrapping_mul(31) as i64) >> 5;
        p.filterA[0] = current_a.wrapping_add(scaled_fa);
        decoded0[i] = p.filterA[0] as i32;
    }

    p.lastA[0] = current_a;
}

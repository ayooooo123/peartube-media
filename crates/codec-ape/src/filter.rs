// Ported from FFmpeg libavcodec/apedec.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

pub const HISTORY_SIZE: usize = 512;
pub const APE_FILTER_LEVELS: usize = 3;

/// Filter orders depending on compression level.
pub const APE_FILTER_ORDERS: [[usize; APE_FILTER_LEVELS]; 5] = [
    [0, 0, 0],
    [16, 0, 0],
    [64, 0, 0],
    [32, 256, 0],
    [16, 256, 1280],
];

/// Filter fraction bits depending on compression level.
pub const APE_FILTER_FRACBITS: [[u32; APE_FILTER_LEVELS]; 5] = [
    [0, 0, 0],
    [11, 0, 0],
    [11, 0, 0],
    [10, 13, 0],
    [11, 13, 15],
];

/// Adaptive filter applied to decoded data.
#[derive(Clone)]
pub struct APEFilter {
    pub coeffs: Vec<i16>,
    pub historybuffer: Vec<i16>,
    pub delay_offset: usize,
    pub adaptcoeffs_offset: usize,
    pub avg: u32,
    pub order: usize,
}

impl APEFilter {
    pub fn new(order: usize) -> Self {
        let mut f = Self {
            coeffs: vec![0; order],
            historybuffer: vec![0; order * 2 + HISTORY_SIZE],
            delay_offset: order * 2,
            adaptcoeffs_offset: order,
            avg: 0,
            order,
        };
        f.reset();
        f
    }

    pub fn reset(&mut self) {
        self.coeffs.fill(0);
        self.historybuffer.fill(0);
        self.delay_offset = self.order * 2;
        self.adaptcoeffs_offset = self.order;
        self.avg = 0;
    }
}

pub fn do_apply_filter(
    version: i32,
    f: &mut APEFilter,
    data: &mut [i32],
    order: usize,
    fracbits: u32,
) {
    if order == 0 {
        return;
    }
    for sample in data.iter_mut() {
        let dotprod = crate::dsp::scalarproduct_and_madd_int16(
            &mut f.coeffs,
            &f.historybuffer[f.delay_offset - order..f.delay_offset],
            &f.historybuffer[f.adaptcoeffs_offset - order..f.adaptcoeffs_offset],
            order,
            crate::predictor::ape_sign(*sample),
        );
        let res64 = (dotprod as i64).wrapping_add(1i64 << (fracbits - 1)) >> fracbits;
        let res = (res64 as i32).wrapping_add(*sample);
        *sample = res;

        f.historybuffer[f.delay_offset] = res.clamp(-32768, 32767) as i16;
        f.delay_offset += 1;

        if version < 3980 {
            f.historybuffer[f.adaptcoeffs_offset] = if res == 0 {
                0
            } else {
                (((res >> 28) & 8) - 4) as i16
            };
            f.historybuffer[f.adaptcoeffs_offset - 4] >>= 1;
            f.historybuffer[f.adaptcoeffs_offset - 8] >>= 1;
        } else {
            let absres = (res as i64).unsigned_abs() as u32;
            if absres != 0 {
                let shift_cond = (absres as u64 > f.avg as u64 * 3) as u32
                    + (absres as u64 > (f.avg as u64 + f.avg as u64 / 3)) as u32;
                f.historybuffer[f.adaptcoeffs_offset] =
                    (crate::predictor::ape_sign(res) * (8 << shift_cond)) as i16;
            } else {
                f.historybuffer[f.adaptcoeffs_offset] = 0;
            }

            f.avg = f.avg.wrapping_add(((absres as i32).wrapping_sub(f.avg as i32) / 16) as u32);

            f.historybuffer[f.adaptcoeffs_offset - 1] >>= 1;
            f.historybuffer[f.adaptcoeffs_offset - 2] >>= 1;
            f.historybuffer[f.adaptcoeffs_offset - 8] >>= 1;
        }

        f.adaptcoeffs_offset += 1;

        if f.delay_offset == HISTORY_SIZE + order * 2 {
            let src_start = f.delay_offset - order * 2;
            f.historybuffer.copy_within(src_start..f.delay_offset, 0);
            f.delay_offset = order * 2;
            f.adaptcoeffs_offset = order;
        }
    }
}

pub fn ape_apply_filters(
    version: i32,
    fset: usize,
    filters: &mut [[APEFilter; 2]; APE_FILTER_LEVELS],
    decoded0: &mut [i32],
    decoded1: Option<&mut [i32]>,
) {
    if fset >= 5 {
        return;
    }
    match decoded1 {
        Some(d1) => {
            for i in 0..APE_FILTER_LEVELS {
                let order = APE_FILTER_ORDERS[fset][i];
                if order == 0 {
                    break;
                }
                let fracbits = APE_FILTER_FRACBITS[fset][i];
                do_apply_filter(version, &mut filters[i][0], decoded0, order, fracbits);
                do_apply_filter(version, &mut filters[i][1], d1, order, fracbits);
            }
        }
        None => {
            for i in 0..APE_FILTER_LEVELS {
                let order = APE_FILTER_ORDERS[fset][i];
                if order == 0 {
                    break;
                }
                let fracbits = APE_FILTER_FRACBITS[fset][i];
                do_apply_filter(version, &mut filters[i][0], decoded0, order, fracbits);
            }
        }
    }
}

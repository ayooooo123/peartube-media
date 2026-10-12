// Port of FFmpeg's shared ATRAC routines (libavcodec/atrac.c, atrac.h,
// FFmpeg commit 2da55bf): the scale factor table, the 48-tap QMF synthesis
// filter and the gain compensation.
// Copyright (c) 2006-2008 Maxim Poliakovski, (c) 2006-2008 Benjamin
// Larsson; LGPL-2.1-or-later (see LICENSE).

use std::sync::LazyLock;

/// `ff_atrac_sf_table`: `2^((i - 15) / 3)`.
pub(crate) static SF_TABLE: LazyLock<[f32; 64]> = LazyLock::new(|| {
    let mut t = [0f32; 64];
    for (i, v) in t.iter_mut().enumerate() {
        *v = 2f64.powf((i as f64 - 15.0) / 3.0) as f32;
    }
    t
});

const QMF_48TAP_HALF: [f32; 24] = [
    -0.00001461907,
    -0.00009205479,
    -0.000056157569,
    0.00030117269,
    0.0002422519,
    -0.00085293897,
    -0.0005205574,
    0.0020340169,
    0.00078333891,
    -0.0042153862,
    -0.00075614988,
    0.0078402944,
    -0.000061169922,
    -0.01344162,
    0.0024626821,
    0.021736089,
    -0.007801671,
    -0.034090221,
    0.01880949,
    0.054326009,
    -0.043596379,
    -0.099384367,
    0.13207909,
    0.46424159,
];

/// The symmetric 48-tap QMF window (`qmf_window`).
static QMF_WINDOW: LazyLock<[f32; 48]> = LazyLock::new(|| {
    let mut w = [0f32; 48];
    for i in 0..24 {
        let s = (f64::from(QMF_48TAP_HALF[i]) * 2.0) as f32;
        w[i] = s;
        w[47 - i] = s;
    }
    w
});

/// Delay line of one QMF synthesis stage.
pub(crate) type QmfDelay = [f32; 46];

/// `ff_atrac_iqmf`: merges `n_in` (at most 512) samples of a low and a high
/// band into `2 * n_in` samples. The inputs are read before the output is
/// written, so callers may pass outputs overlapping inputs through copies.
pub(crate) fn iqmf(inlo: &[f32], inhi: &[f32], n_in: usize, out: &mut [f32], delay: &mut QmfDelay) {
    let window = &*QMF_WINDOW;
    let mut temp = [0f32; 46 + 2 * 512];
    let temp = &mut temp[..46 + 2 * n_in];
    temp[..46].copy_from_slice(delay);
    {
        let p3 = &mut temp[46..];
        for i in (0..n_in).step_by(2) {
            p3[2 * i] = inlo[i] + inhi[i];
            p3[2 * i + 1] = inlo[i] - inhi[i];
            p3[2 * i + 2] = inlo[i + 1] + inhi[i + 1];
            p3[2 * i + 3] = inlo[i + 1] - inhi[i + 1];
        }
    }
    for j in 0..n_in {
        let p1 = &temp[2 * j..2 * j + 48];
        let mut s1 = 0f32;
        let mut s2 = 0f32;
        for i in (0..48).step_by(2) {
            s1 += p1[i] * window[i];
            s2 += p1[i + 1] * window[i + 1];
        }
        out[2 * j] = s2;
        out[2 * j + 1] = s1;
    }
    delay.copy_from_slice(&temp[2 * n_in..2 * n_in + 46]);
}

/// `AtracGainInfo`: gain control points of one band.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(crate) struct GainInfo {
    pub num_points: i32,
    pub lev_code: [i32; 7],
    pub loc_code: [i32; 7],
}

/// `AtracGCContext`.
pub(crate) struct GainContext {
    gain_tab1: [f32; 16],
    gain_tab2: [f32; 31],
    id2exp_offset: i32,
    loc_scale: u32,
    loc_size: usize,
}

impl GainContext {
    /// `ff_atrac_init_gain_compensation`.
    pub(crate) fn new(id2exp_offset: i32, loc_scale: u32) -> Self {
        let loc_size = 1usize << loc_scale;
        let mut gain_tab1 = [0f32; 16];
        for (i, v) in gain_tab1.iter_mut().enumerate() {
            *v = 2f32.powf((id2exp_offset - i as i32) as f32);
        }
        let mut gain_tab2 = [0f32; 31];
        for i in -15i32..16 {
            gain_tab2[(i + 15) as usize] = 2f32.powf(-1.0f32 / loc_size as f32 * i as f32);
        }
        Self {
            gain_tab1,
            gain_tab2,
            id2exp_offset,
            loc_scale,
            loc_size,
        }
    }

    /// Level table entry; a code outside it (only from a corrupt stream)
    /// reads as unity gain.
    fn tab1(&self, code: i32) -> f32 {
        usize::try_from(code)
            .ok()
            .and_then(|c| self.gain_tab1.get(c))
            .copied()
            .unwrap_or(1.0)
    }

    /// `ff_atrac_gain_compensation`: overlap-adds `input[..n]` (scaled by
    /// the next block's first gain) onto `prev`, applies the current
    /// block's gain curve, writes `out[..n]`, and keeps `input[n..2n]` as
    /// the next overlap.
    pub(crate) fn compensate(
        &self,
        input: &[f32],
        prev: &mut [f32],
        now: &GainInfo,
        next: &GainInfo,
        n: usize,
        out: &mut [f32],
    ) {
        let gc_scale = if next.num_points != 0 {
            self.tab1(next.lev_code[0])
        } else {
            1.0
        };

        if now.num_points == 0 {
            for pos in 0..n {
                out[pos] = input[pos] * gc_scale + prev[pos];
            }
        } else {
            let points = (now.num_points.max(0) as usize).min(7);
            let mut pos = 0usize;
            for i in 0..points {
                let lastpos = ((now.loc_code[i].max(0) as usize) << self.loc_scale).min(n);
                let mut lev = self.tab1(now.lev_code[i]);
                let next_lev = if i + 1 < points {
                    now.lev_code[i + 1]
                } else {
                    self.id2exp_offset
                };
                let idx = next_lev - now.lev_code[i] + 15;
                let gain_inc = usize::try_from(idx)
                    .ok()
                    .and_then(|k| self.gain_tab2.get(k))
                    .copied()
                    .unwrap_or(1.0);

                // constant gain level and overlap
                while pos < lastpos {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    pos += 1;
                }
                // interpolate between two gain levels
                let end = (lastpos + self.loc_size).min(n);
                while pos < end {
                    out[pos] = (input[pos] * gc_scale + prev[pos]) * lev;
                    lev *= gain_inc;
                    pos += 1;
                }
            }
            while pos < n {
                out[pos] = input[pos] * gc_scale + prev[pos];
                pos += 1;
            }
        }

        prev[..n].copy_from_slice(&input[n..2 * n]);
    }
}

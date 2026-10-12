//! RealAudio SIPR / ACELP.NET speech decoder.
//!
//! Ported from FFmpeg (commit 2da55bf):
//! - libavcodec/sipr.c, libavcodec/sipr.h
//! - libavcodec/sipr16k.c
//! - libavcodec/siprdata.h
//! - libavcodec/sipr16kdata.h
//! - libavcodec/acelp_pitch_delay.c
//! - libavcodec/acelp_vectors.c
//! - libavcodec/acelp_filters.c
//! - libavcodec/celp_filters.c
//! - libavcodec/lsp.c
//! - libavutil/float_scalarproduct.c, libavutil/ffmath.h
//!
//! License: LGPL-2.1-or-later.
//!
//! Float evaluation follows the C code operation by operation: whatever C
//! computes in double is computed in `f64` here, and every `a * b + c` that
//! clang contracts inside one C expression (its default `-ffp-contract=on`,
//! which FFmpeg's arm64 builds use) is a single-rounding `mul_add`. The LPC
//! and pitch filters are recursive, so rounding differences grow; staying on
//! FFmpeg's rounding path is what keeps the output on FFmpeg's.

#![forbid(unsafe_code)]

use std::f64::consts::{LN_10, LN_2, LOG2_10, PI};

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame,
    Packet, Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderLe;
use crate::sipr_tables::*;

pub const LP_FILTER_ORDER: usize = 10;
pub const LP_FILTER_ORDER_16K: usize = 16;
pub const PITCH_MIN: i32 = 30;
pub const PITCH_MAX: i32 = 281;
pub const PITCH_DELAY_MIN: i32 = 20;
pub const PITCH_DELAY_MAX: i32 = 143;
/// Minimum LSF spacing; a double constant in sipr.h.
pub const LSFQ_DIFF_MIN: f64 = 0.0125 * PI;
/// Number of past samples needed for excitation interpolation.
pub const L_INTERPOL: usize = LP_FILTER_ORDER + 1;
/// Subframe size for every mode except 16k.
pub const SUBFR_SIZE: usize = 48;
pub const L_SUBFR_16K: usize = 80;
pub const SUBFRAME_COUNT_16K: usize = 2;

/// Excitation history kept in front of the current NB frame.
const EXC_HISTORY_NB: usize = PITCH_DELAY_MAX as usize + L_INTERPOL;
/// Excitation history kept in front of the current 16k frame.
const EXC_HISTORY_16K: usize = PITCH_MAX as usize + L_INTERPOL;
/// `SiprContext.excitation` length.
const EXCITATION_LEN: usize = L_INTERPOL + PITCH_MAX as usize + 2 * L_SUBFR_16K;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SiprMode {
    Mode16k,
    Mode8k5,
    Mode6k5,
    Mode5k0,
}

impl SiprMode {
    pub fn packet_size(self) -> usize {
        match self {
            Self::Mode16k => 20,
            Self::Mode8k5 => 19,
            Self::Mode6k5 => 29,
            Self::Mode5k0 => 37,
        }
    }

    pub fn sample_rate(self) -> u32 {
        match self {
            Self::Mode16k => 16000,
            _ => 8000,
        }
    }

    pub fn subframe_count(self) -> usize {
        match self {
            Self::Mode16k => 2,
            Self::Mode8k5 => 3,
            Self::Mode6k5 => 3,
            Self::Mode5k0 => 5,
        }
    }

    pub fn frames_per_packet(self) -> usize {
        match self {
            Self::Mode16k | Self::Mode8k5 => 1,
            Self::Mode6k5 | Self::Mode5k0 => 2,
        }
    }

    pub fn samples_per_packet(self) -> usize {
        match self {
            Self::Mode16k => 160,
            Self::Mode8k5 => 144,
            Self::Mode6k5 => 288,
            Self::Mode5k0 => 480,
        }
    }

    pub fn pitch_sharp_factor(self) -> f32 {
        match self {
            Self::Mode16k => 0.0,
            Self::Mode8k5 | Self::Mode6k5 => 0.8,
            Self::Mode5k0 => 0.85,
        }
    }
}

#[derive(Default, Clone, Debug)]
struct AmrFixed {
    n: usize,
    x: [usize; 10],
    y: [f32; 10],
    pitch_lag: usize,
    pitch_fac: f32,
}

#[derive(Default, Clone, Debug)]
struct SiprParameters {
    ma_pred_switch: usize,
    vq_indexes: [usize; 5],
    pitch_delay: [usize; 5],
    gp_index: [usize; 5],
    fc_indexes: [[i16; 10]; 5],
    gc_index: [usize; 5],
}

fn decode_parameters(gb: &mut BitReaderLe, mode: SiprMode) -> Option<SiprParameters> {
    let mut parms = SiprParameters::default();
    match mode {
        SiprMode::Mode16k => {
            parms.ma_pred_switch = gb.read_bits(1)? as usize;
            parms.vq_indexes[0] = gb.read_bits(7)? as usize;
            parms.vq_indexes[1] = gb.read_bits(8)? as usize;
            parms.vq_indexes[2] = gb.read_bits(7)? as usize;
            parms.vq_indexes[3] = gb.read_bits(7)? as usize;
            parms.vq_indexes[4] = gb.read_bits(7)? as usize;
            let pitch_bits = [9, 6];
            let fc_bits = [4, 5, 4, 5, 4, 5, 4, 5, 4, 5];
            for i in 0..2 {
                parms.pitch_delay[i] = gb.read_bits(pitch_bits[i])? as usize;
                parms.gp_index[i] = gb.read_bits(4)? as usize;
                for j in 0..10 {
                    parms.fc_indexes[i][j] = gb.read_bits(fc_bits[j])? as i16;
                }
                parms.gc_index[i] = gb.read_bits(5)? as usize;
            }
        }
        SiprMode::Mode8k5 => {
            parms.vq_indexes[0] = gb.read_bits(6)? as usize;
            parms.vq_indexes[1] = gb.read_bits(7)? as usize;
            parms.vq_indexes[2] = gb.read_bits(7)? as usize;
            parms.vq_indexes[3] = gb.read_bits(7)? as usize;
            parms.vq_indexes[4] = gb.read_bits(5)? as usize;
            let pitch_bits = [8, 5, 5];
            for i in 0..3 {
                parms.pitch_delay[i] = gb.read_bits(pitch_bits[i])? as usize;
                for j in 0..3 {
                    parms.fc_indexes[i][j] = gb.read_bits(9)? as i16;
                }
                parms.gc_index[i] = gb.read_bits(7)? as usize;
            }
        }
        SiprMode::Mode6k5 => {
            parms.vq_indexes[0] = gb.read_bits(6)? as usize;
            parms.vq_indexes[1] = gb.read_bits(7)? as usize;
            parms.vq_indexes[2] = gb.read_bits(7)? as usize;
            parms.vq_indexes[3] = gb.read_bits(7)? as usize;
            parms.vq_indexes[4] = gb.read_bits(5)? as usize;
            let pitch_bits = [8, 5, 5];
            for i in 0..3 {
                parms.pitch_delay[i] = gb.read_bits(pitch_bits[i])? as usize;
                for j in 0..3 {
                    parms.fc_indexes[i][j] = gb.read_bits(5)? as i16;
                }
                parms.gc_index[i] = gb.read_bits(7)? as usize;
            }
        }
        SiprMode::Mode5k0 => {
            parms.vq_indexes[0] = gb.read_bits(6)? as usize;
            parms.vq_indexes[1] = gb.read_bits(7)? as usize;
            parms.vq_indexes[2] = gb.read_bits(7)? as usize;
            parms.vq_indexes[3] = gb.read_bits(7)? as usize;
            parms.vq_indexes[4] = gb.read_bits(5)? as usize;
            let pitch_bits = [8, 5, 8, 5, 5];
            for i in 0..5 {
                parms.pitch_delay[i] = gb.read_bits(pitch_bits[i])? as usize;
                parms.fc_indexes[i][0] = gb.read_bits(10)? as i16;
                parms.gc_index[i] = gb.read_bits(7)? as usize;
            }
        }
    }
    Some(parms)
}

/// `FFMIN(a, b)`: `b` when `a > b`, else `a` (keeps a NaN `a`, unlike `f32::min`).
#[inline]
fn ffmin(a: f32, b: f32) -> f32 {
    if a > b { b } else { a }
}

/// ff_scalarproduct_float_c over the shorter slice ([`crate::sums`]).
fn scalarproduct(v1: &[f32], v2: &[f32]) -> f32 {
    crate::sums::scalarproduct_float(v1, v2, v1.len().min(v2.len()))
}

fn sort_nearly_sorted_floats(vals: &mut [f32]) {
    let len = vals.len();
    if len <= 1 {
        return;
    }
    for i in 0..len - 1 {
        let mut j = i;
        while vals[j] > vals[j + 1] {
            vals.swap(j, j + 1);
            if j == 0 {
                break;
            }
            j -= 1;
        }
    }
}

/// ff_set_min_dist_lsf: `prev = lsf[i] = FFMAX(lsf[i], prev + min_spacing)`,
/// the sum and the comparison in double.
fn set_min_dist_lsf(lsf: &mut [f32], min_spacing: f64) {
    let mut prev = 0.0f32;
    for x in lsf.iter_mut() {
        let floor = prev as f64 + min_spacing;
        let v = *x as f64;
        *x = (if v > floor { v } else { floor }) as f32;
        prev = *x;
    }
}

fn lsp2polyf(lsp: &[f64], f: &mut [f64], lp_half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=lp_half_order {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val.mul_add(f[i - 1], 2.0 * f[i - 2]);
        for j in (2..i).rev() {
            f[j] += f[j - 1].mul_add(val, f[j - 2]);
        }
        f[1] += val;
    }
}

/// ff_amrwb_lsp2lpc for order 10.
fn amrwb_lsp2lpc(lsp: &[f64; LP_FILTER_ORDER], lp: &mut [f32]) {
    let lp_order = LP_FILTER_ORDER;
    let lp_half_order = lp_order / 2;
    let mut pa = [0.0f64; 6];
    // qa[k] here is qa[k - 1] in C, whose qa[-1] is 0.
    let mut qa = [0.0f64; 6];
    lsp2polyf(lsp, &mut pa, lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa[1..], lp_half_order - 1);

    for i in 1..lp_half_order {
        let j = lp_order - i;
        let paf = pa[i] * (1.0 + lsp[lp_order - 1]);
        let qaf = (qa[i + 1] - qa[i - 1]) * (1.0 - lsp[lp_order - 1]);
        lp[i - 1] = ((paf + qaf) * 0.5) as f32;
        lp[j - 1] = ((paf - qaf) * 0.5) as f32;
    }

    lp[lp_half_order - 1] = ((1.0 + lsp[lp_order - 1]) * pa[lp_half_order] * 0.5) as f32;
    lp[lp_order - 1] = lsp[lp_order - 1] as f32;
}

/// ff_acelp_lspd2lpc.
fn acelp_lspd2lpc(lsp: &[f64], lpc: &mut [f32], lp_half_order: usize) {
    let mut pa = [0.0f64; 11];
    let mut qa = [0.0f64; 11];
    lsp2polyf(lsp, &mut pa[..=lp_half_order], lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa[..=lp_half_order], lp_half_order);

    for k in (0..lp_half_order).rev() {
        let paf = pa[k + 1] + pa[k];
        let qaf = qa[k + 1] - qa[k];
        lpc[k] = (0.5 * (paf + qaf)) as f32;
        lpc[2 * lp_half_order - 1 - k] = (0.5 * (paf - qaf)) as f32;
    }
}

/// acelp_lp_decodef (sipr16k.c).
fn acelp_lp_decodef(
    lp_1st: &mut [f32; LP_FILTER_ORDER_16K],
    lp_2nd: &mut [f32; LP_FILTER_ORDER_16K],
    lsp_2nd: &[f64; LP_FILTER_ORDER_16K],
    lsp_prev: &[f64; LP_FILTER_ORDER_16K],
) {
    let mut lsp_1st = [0.0f64; LP_FILTER_ORDER_16K];
    for i in 0..LP_FILTER_ORDER_16K {
        lsp_1st[i] = (lsp_2nd[i] + lsp_prev[i]) * 0.5;
    }
    acelp_lspd2lpc(&lsp_1st, lp_1st, LP_FILTER_ORDER_16K / 2);
    acelp_lspd2lpc(lsp_2nd, lp_2nd, LP_FILTER_ORDER_16K / 2);
}

fn lsf_decode_fp_16k(
    lsf_history: &mut [f32; LP_FILTER_ORDER_16K],
    isp_new: &mut [f32; LP_FILTER_ORDER_16K],
    parm: &[usize; 5],
    ma_pred: usize,
) {
    let mut isp_q = [0.0f32; LP_FILTER_ORDER_16K];
    isp_q[0..3].copy_from_slice(&LSF_CB1_16K[parm[0] & 127]);
    isp_q[3..6].copy_from_slice(&LSF_CB2_16K[parm[1] & 255]);
    isp_q[6..9].copy_from_slice(&LSF_CB3_16K[parm[2] & 127]);
    isp_q[9..12].copy_from_slice(&LSF_CB4_16K[parm[3] & 127]);
    isp_q[12..16].copy_from_slice(&LSF_CB5_16K[parm[4] & 127]);

    let q = QU[ma_pred & 1];
    for i in 0..LP_FILTER_ORDER_16K {
        isp_new[i] = (1.0 - q).mul_add(isp_q[i], q * lsf_history[i]) + MEAN_LSF_16K[i];
    }
    *lsf_history = isp_q;
}

fn dec_delay3_1st(index: i32) -> i32 {
    if index < 390 {
        index + 88
    } else {
        3 * index - 690
    }
}

fn dec_delay3_2nd(index: i32, pit_min: i32, pit_max: i32, pitch_lag_prev: i32) -> i32 {
    if index < 62 {
        let pitch_delay_min = (pitch_lag_prev - 10).clamp(pit_min, pit_max - 19);
        3 * pitch_delay_min + index - 2
    } else {
        3 * pitch_lag_prev
    }
}

#[inline]
fn divide_by_3(x: i32) -> i32 {
    (x * 10923) >> 15
}

/// ff_decode_pitch_lag.
fn decode_pitch_lag(
    pitch_index: usize,
    prev_lag_int: i32,
    subframe: usize,
    third_as_first: bool,
    resolution: usize,
) -> (i32, i32) {
    let mut idx = pitch_index as i32;
    if subframe == 0 || (subframe == 2 && third_as_first) {
        if idx < 197 {
            idx += 59;
        } else {
            idx = 3 * idx - 335;
        }
    } else if resolution == 4 {
        let search_range_min = (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9);
        if idx < 4 {
            idx = 3 * (idx + search_range_min) + 1;
        } else if idx < 12 {
            idx += 3 * search_range_min + 7;
        } else {
            idx = 3 * (idx + search_range_min - 6) + 1;
        }
    } else {
        idx -= 1;
        if resolution == 5 {
            idx += 3 * (prev_lag_int - 10).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 19);
        } else {
            idx += 3 * (prev_lag_int - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9);
        }
    }
    let lag_int = (idx * 10923) >> 15;
    let lag_frac = idx - 3 * lag_int - 1;
    (lag_int, lag_frac)
}

/// ff_acelp_interpolatef on one buffer: `out = buf[out_pos..]`, `in = buf[in_pos..]`.
/// The ranges may overlap; like the C loop, each output is written before
/// later taps read it.
#[allow(clippy::too_many_arguments)]
fn acelp_interpolatef(
    buf: &mut [f32],
    out_pos: usize,
    in_pos: usize,
    filter_coeffs: &[f32],
    precision: usize,
    frac_pos: usize,
    filter_length: usize,
    length: usize,
) {
    for n in 0..length {
        let mut idx = 0;
        let mut v = 0.0f32;
        for i in 0..filter_length {
            v = buf[in_pos + n + i].mul_add(filter_coeffs[idx + frac_pos], v);
            idx += precision;
            v = buf[in_pos + n - (i + 1)].mul_add(filter_coeffs[idx - frac_pos], v);
        }
        buf[out_pos + n] = v;
    }
}

/// ff_celp_lp_synthesis_filterf: `out[n] = in[n] - sum(filter_coeffs[i-1] * out[n-i])`,
/// evaluated in FFmpeg's four-samples-at-a-time order, then the samples left
/// over one by one in a sum FFmpeg's build vectorizes (the coefficients never
/// overlap `out`). `out[n]` is `buf[out_pos + n]`;
/// `buf[out_pos - filter_length..out_pos]` is the filter memory.
/// `filter_length` must be even and at least 4.
fn celp_lp_synthesis_filterf(
    buf: &mut [f32],
    out_pos: usize,
    filter_coeffs: &[f32],
    input: &[f32],
    filter_length: usize,
) {
    let fc = filter_coeffs;
    let buffer_length = input.len();

    let a = fc[0];
    let mut b = fc[1];
    let mut c = fc[2];
    b = (-fc[0]).mul_add(fc[0], b);
    c = (-fc[1]).mul_add(fc[0], c);
    c = (-fc[0]).mul_add(b, c);

    let mut old_out0 = buf[out_pos - 4];
    let mut old_out1 = buf[out_pos - 3];
    let mut old_out2 = buf[out_pos - 2];
    let mut old_out3 = buf[out_pos - 1];
    let mut n = 0;
    while n + 4 <= buffer_length {
        let o = out_pos + n;
        let mut out0 = input[n];
        let mut out1 = input[n + 1];
        let mut out2 = input[n + 2];
        let mut out3 = input[n + 3];

        out0 = (-fc[2]).mul_add(old_out1, out0);
        out1 = (-fc[2]).mul_add(old_out2, out1);
        out2 = (-fc[2]).mul_add(old_out3, out2);

        out0 = (-fc[1]).mul_add(old_out2, out0);
        out1 = (-fc[1]).mul_add(old_out3, out1);

        out0 = (-fc[0]).mul_add(old_out3, out0);

        let val = fc[3];
        out0 = (-val).mul_add(old_out0, out0);
        out1 = (-val).mul_add(old_out1, out1);
        out2 = (-val).mul_add(old_out2, out2);
        out3 = (-val).mul_add(old_out3, out3);

        let mut i = 5;
        while i < filter_length {
            old_out3 = buf[o - i];
            let val = fc[i - 1];
            out0 = (-val).mul_add(old_out3, out0);
            out1 = (-val).mul_add(old_out0, out1);
            out2 = (-val).mul_add(old_out1, out2);
            out3 = (-val).mul_add(old_out2, out3);

            old_out2 = buf[o - i - 1];
            let val = fc[i];
            out0 = (-val).mul_add(old_out2, out0);
            out1 = (-val).mul_add(old_out3, out1);
            out2 = (-val).mul_add(old_out0, out2);
            out3 = (-val).mul_add(old_out1, out3);

            std::mem::swap(&mut old_out0, &mut old_out2);
            old_out1 = old_out3;
            i += 2;
        }

        let tmp0 = out0;
        let tmp1 = out1;
        let tmp2 = out2;

        out3 = (-a).mul_add(tmp2, out3);
        out2 = (-a).mul_add(tmp1, out2);
        out1 = (-a).mul_add(tmp0, out1);

        out3 = (-b).mul_add(tmp1, out3);
        out2 = (-b).mul_add(tmp0, out2);

        out3 = (-c).mul_add(tmp0, out3);

        buf[o] = out0;
        buf[o + 1] = out1;
        buf[o + 2] = out2;
        buf[o + 3] = out3;

        old_out0 = out0;
        old_out1 = out1;
        old_out2 = out2;
        old_out3 = out3;
        n += 4;
    }

    let split = crate::sums::unfused_terms(filter_length);
    while n < buffer_length {
        let o = out_pos + n;
        let mut v = input[n];
        for i in 1..=split {
            v -= fc[i - 1] * buf[o - i];
        }
        for i in split + 1..=filter_length {
            v = (-fc[i - 1]).mul_add(buf[o - i], v);
        }
        buf[o] = v;
        n += 1;
    }
}

/// ff_celp_lp_zero_synthesis_filterf: `out[n] = in[n] + sum(filter_coeffs[i-1] * in[n-i])`
/// where `in[n]` is `buf[in_pos + n]`. FFmpeg's build vectorizes the sum
/// when `out` overlaps neither `in` nor the coefficients, as in sipr's call:
/// the first [`crate::sums::unfused_terms`] products are rounded, the rest
/// fused.
fn celp_lp_zero_synthesis_filterf(
    out: &mut [f32],
    filter_coeffs: &[f32],
    buf: &[f32],
    in_pos: usize,
    filter_length: usize,
) {
    let split = crate::sums::unfused_terms(filter_length);
    for (n, o) in out.iter_mut().enumerate() {
        let mut v = buf[in_pos + n];
        for i in 1..=split {
            v += filter_coeffs[i - 1] * buf[in_pos + n - i];
        }
        for i in split + 1..=filter_length {
            v = filter_coeffs[i - 1].mul_add(buf[in_pos + n - i], v);
        }
        *o = v;
    }
}

/// ff_decode_10_pulses_35bits.
fn decode_10_pulses_35bits(
    fixed_index: &[i16; 10],
    fixed_sparse: &mut AmrFixed,
    gray_decode: &[u8; 16],
    half_pulse_count: usize,
    bits: usize,
) {
    let mask = (1 << bits) - 1;
    fixed_sparse.n = 2 * half_pulse_count;
    for i in 0..half_pulse_count {
        let pos1 = gray_decode[(fixed_index[2 * i + 1] as usize) & mask] as usize + i;
        let pos2 = gray_decode[(fixed_index[2 * i] as usize) & mask] as usize + i;
        let sign = if (fixed_index[2 * i + 1] & (1 << bits)) != 0 {
            -1.0f32
        } else {
            1.0f32
        };
        fixed_sparse.x[2 * i + 1] = pos1;
        fixed_sparse.x[2 * i] = pos2;
        fixed_sparse.y[2 * i + 1] = sign;
        fixed_sparse.y[2 * i] = if pos2 < pos1 { -sign } else { sign };
    }
}

/// ff_set_fixed_vector (no pulse has its repeat bit cleared here).
fn set_fixed_vector(out: &mut [f32], fixed: &AmrFixed, scale: f32) {
    let size = out.len();
    for i in 0..fixed.n {
        let mut x = fixed.x[i];
        let mut y = fixed.y[i] * scale;
        if fixed.pitch_lag > 0 {
            while x < size {
                out[x] += y;
                y *= fixed.pitch_fac;
                x += fixed.pitch_lag;
            }
        }
    }
}

/// acelp_decode_gain_codef (sipr16k.c).
fn acelp_decode_gain_codef(
    gain_corr_factor: f32,
    fc_v: &[f32],
    mut mr_energy: f32,
    quant_energy: &[f32],
    ma_prediction_coeff: &[f32],
) -> f32 {
    mr_energy += scalarproduct(quant_energy, ma_prediction_coeff);
    (gain_corr_factor as f64 * (LN_10 / 20.0 * mr_energy as f64).exp()
        / (0.01 + scalarproduct(fc_v, fc_v) as f64).sqrt()) as f32
}

fn lsf_decode_fp(lsfnew: &mut [f32; 10], lsf_history: &mut [f32], vq_indexes: &[usize; 5]) {
    let mut lsf_tmp = [0.0f32; 10];
    lsf_tmp[0..2].copy_from_slice(&LSF_CB1[vq_indexes[0] & 63]);
    lsf_tmp[2..4].copy_from_slice(&LSF_CB2[vq_indexes[1] & 127]);
    lsf_tmp[4..6].copy_from_slice(&LSF_CB3[vq_indexes[2] & 127]);
    lsf_tmp[6..8].copy_from_slice(&LSF_CB4[vq_indexes[3] & 127]);
    lsf_tmp[8..10].copy_from_slice(&LSF_CB5[vq_indexes[4] & 31]);

    for i in 0..10 {
        lsfnew[i] = ((lsf_history[i] as f64).mul_add(0.33, lsf_tmp[i] as f64)
            + MEAN_LSF[i] as f64) as f32;
    }

    sort_nearly_sorted_floats(&mut lsfnew[..9]);
    // No minimum distance between the last value and the previous one,
    // contrary to ff_acelp_reorder_lsf().
    set_min_dist_lsf(&mut lsfnew[..9], LSFQ_DIFF_MIN);
    if lsfnew[9] as f64 > 1.3 * PI {
        lsfnew[9] = (1.3 * PI) as f32;
    }

    lsf_history.copy_from_slice(&lsf_tmp);

    for x in &mut lsfnew[..9] {
        *x = (*x as f64).cos() as f32;
    }
    lsfnew[9] = (lsfnew[9] as f64 * (6.153848 / PI)) as f32;
}

fn sipr_decode_lp(lsfnew: &[f32; 10], lsfold: &[f32; 10], az: &mut [f32], num_subfr: usize) {
    let t0 = (1.0 / num_subfr as f64) as f32;
    let mut t = (t0 as f64 * 0.5) as f32;
    for out_az in az.chunks_exact_mut(LP_FILTER_ORDER).take(num_subfr) {
        let mut lsfint = [0.0f64; LP_FILTER_ORDER];
        for j in 0..LP_FILTER_ORDER {
            lsfint[j] = lsfold[j].mul_add(1.0 - t, t * lsfnew[j]) as f64;
        }
        amrwb_lsp2lpc(&lsfint, out_az);
        t += t0;
    }
}

/// Adaptive impulse response; `ir_buf[..LP_FILTER_ORDER]` is zero filter memory.
fn eval_ir(
    az: &[f32],
    pitch_lag: usize,
    ir_buf: &mut [f32; SUBFR_SIZE + LP_FILTER_ORDER],
    pitch_sharp_factor: f32,
) {
    let mut tmp1 = [0.0f32; SUBFR_SIZE + 1];
    let mut tmp2 = [0.0f32; LP_FILTER_ORDER + 1];

    tmp1[0] = 1.0;
    for i in 0..LP_FILTER_ORDER {
        tmp1[i + 1] = az[i] * FF_POW_0_55[i];
        tmp2[i] = az[i] * FF_POW_0_7[i];
    }

    celp_lp_synthesis_filterf(ir_buf, LP_FILTER_ORDER, &tmp2, &tmp1[..SUBFR_SIZE], LP_FILTER_ORDER);

    // pitch_sharpening
    let freq = &mut ir_buf[LP_FILTER_ORDER..];
    for i in pitch_lag..SUBFR_SIZE {
        freq[i] = pitch_sharp_factor.mul_add(freq[i - pitch_lag], freq[i]);
    }
}

fn decode_fixed_sparse(fixed_sparse: &mut AmrFixed, pulses: &[i16; 10], mode: SiprMode, low_gain: bool) {
    match mode {
        SiprMode::Mode6k5 => {
            for i in 0..3 {
                fixed_sparse.x[i] = 3 * ((pulses[i] as usize) & 0xf) + i;
                fixed_sparse.y[i] = if (pulses[i] & 0x10) != 0 { -1.0 } else { 1.0 };
            }
            fixed_sparse.n = 3;
        }
        SiprMode::Mode8k5 => {
            for i in 0..3 {
                fixed_sparse.x[2 * i] = 3 * (((pulses[i] as usize) >> 4) & 0xf) + i;
                fixed_sparse.x[2 * i + 1] = 3 * ((pulses[i] as usize) & 0xf) + i;

                fixed_sparse.y[2 * i] = if (pulses[i] & 0x100) != 0 { -1.0 } else { 1.0 };
                fixed_sparse.y[2 * i + 1] = if fixed_sparse.x[2 * i + 1] < fixed_sparse.x[2 * i] {
                    -fixed_sparse.y[2 * i]
                } else {
                    fixed_sparse.y[2 * i]
                };
            }
            fixed_sparse.n = 6;
        }
        SiprMode::Mode5k0 | SiprMode::Mode16k => {
            if low_gain {
                let offset = if (pulses[0] & 0x200) != 0 { 2 } else { 0 };
                let mut val = pulses[0] as usize;
                for i in 0..3 {
                    let index = (val & 0x7) * 6 + 4 - i * 2;
                    fixed_sparse.y[i] = if ((offset + index) & 0x3) != 0 { -1.0 } else { 1.0 };
                    fixed_sparse.x[i] = index;
                    val >>= 3;
                }
                fixed_sparse.n = 3;
            } else {
                let pulse_subset = ((pulses[0] >> 8) & 1) as usize;
                fixed_sparse.x[0] = (((pulses[0] as usize) >> 4) & 15) * 3 + pulse_subset;
                fixed_sparse.x[1] = ((pulses[0] as usize) & 15) * 3 + pulse_subset + 1;
                fixed_sparse.y[0] = if (pulses[0] & 0x200) != 0 { -1.0 } else { 1.0 };
                fixed_sparse.y[1] = -fixed_sparse.y[0];
                fixed_sparse.n = 2;
            }
        }
    }
}

/// Convolution of `shape` with the sparse pulse vector.
fn convolute_with_sparse(out: &mut [f32; SUBFR_SIZE], pulses: &AmrFixed, shape: &[f32]) {
    out.fill(0.0);
    for i in 0..pulses.n {
        let px = pulses.x[i];
        let py = pulses.y[i];
        for j in px..SUBFR_SIZE {
            out[j] = py.mul_add(shape[j - px], out[j]);
        }
    }
}

/// ff_amr_set_fixed_gain.
fn amr_set_fixed_gain(
    fixed_gain_factor: f32,
    fixed_mean_energy: f32,
    prediction_error: &mut [f32; 4],
    energy_mean: f32,
    pred_table: &[f32; 4],
) -> f32 {
    // ff_exp10(x) is exp2(M_LOG2_10 * x).
    let exponent = 0.05 * (scalarproduct(pred_table, prediction_error) + energy_mean) as f64;
    let mean_energy = if fixed_mean_energy != 0.0 { fixed_mean_energy } else { 1.0 };
    let val = (fixed_gain_factor as f64 * (LOG2_10 * exponent).exp2() / mean_energy.sqrt() as f64)
        as f32;

    prediction_error.copy_within(1..4, 0);
    prediction_error[3] = (20.0 * fixed_gain_factor.log10() as f64) as f32;

    val
}

/// ff_weighted_vector_sumf with `out == in_a`, as sipr calls it.
fn weighted_vector_sumf(in_a_out: &mut [f32], in_b: &[f32], weight_coeff_a: f32, weight_coeff_b: f32) {
    for (a, &b) in in_a_out.iter_mut().zip(in_b) {
        *a = weight_coeff_a.mul_add(*a, weight_coeff_b * b);
    }
}

/// ff_tilt_compensation.
fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32]) {
    let size = samples.len();
    let new_tilt_mem = samples[size - 1];
    for i in (1..size).rev() {
        samples[i] = (-tilt).mul_add(samples[i - 1], samples[i]);
    }
    samples[0] = (-tilt).mul_add(*mem, samples[0]);
    *mem = new_tilt_mem;
}

/// ff_adaptive_gain_control in place (`out == in`), as sipr calls it.
fn adaptive_gain_control(samples: &mut [f32], speech_energ: f32, alpha: f32, gain_mem: &mut f32) {
    let postfilter_energ = scalarproduct(samples, samples);
    let mut gain_scale_factor = 1.0f32;
    if postfilter_energ != 0.0 {
        gain_scale_factor = ((speech_energ / postfilter_energ) as f64).sqrt() as f32;
    }
    gain_scale_factor = (gain_scale_factor as f64 * (1.0 - alpha as f64)) as f32;

    let mut mem = *gain_mem;
    for x in samples.iter_mut() {
        mem = alpha.mul_add(mem, gain_scale_factor);
        *x *= mem;
    }
    *gain_mem = mem;
}

/// ff_acelp_apply_order_2_transfer_function.
fn acelp_apply_order_2_transfer_function(
    out: &mut [f32],
    input: &[f32],
    zero_coeffs: [f32; 2],
    pole_coeffs: [f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
) {
    for (o, &x) in out.iter_mut().zip(input) {
        let tmp = (-pole_coeffs[1]).mul_add(mem[1], gain.mul_add(x, -(pole_coeffs[0] * mem[0])));
        *o = zero_coeffs[1].mul_add(mem[1], zero_coeffs[0].mul_add(mem[0], tmp));
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

pub struct SiprContext {
    pub mode: SiprMode,
    past_pitch_gain: f32,
    lsf_history: [f32; LP_FILTER_ORDER_16K],
    excitation: [f32; EXCITATION_LEN],
    /// `synth_buf` of sipr.c: NB keeps its 10 synthesis memory samples at
    /// [6..16] and the frame at [16..].
    synth_buf: [f32; LP_FILTER_ORDER + 5 * SUBFR_SIZE + 6],
    lsp_history: [f32; LP_FILTER_ORDER],
    gain_mem: f32,
    energy_history: [f32; 4],
    highpass_filt_mem: [f32; 2],
    postfilter_mem: [f32; LP_FILTER_ORDER],
    tilt_mem: f32,
    postfilter_agc: f32,
    postfilter_mem5k0: [f32; LP_FILTER_ORDER],
    postfilter_syn5k0: [f32; LP_FILTER_ORDER + 5 * SUBFR_SIZE],
    pitch_lag_prev: i32,
    iir_mem: [f32; LP_FILTER_ORDER_16K],
    filt_buf: [[f32; LP_FILTER_ORDER_16K]; 2],
    /// Which `filt_buf` is `filt_mem[0]` (the pointers FFmpeg swaps per frame).
    filt_idx: usize,
    mem_preemph: [f32; LP_FILTER_ORDER_16K],
    synth_16k: [f32; LP_FILTER_ORDER_16K],
    lsp_history_16k: [f64; LP_FILTER_ORDER_16K],
}

impl SiprContext {
    pub fn new(mode: SiprMode) -> Self {
        let mut ctx = Self {
            mode,
            past_pitch_gain: 0.0,
            lsf_history: [0.0; LP_FILTER_ORDER_16K],
            excitation: [0.0; EXCITATION_LEN],
            synth_buf: [0.0; LP_FILTER_ORDER + 5 * SUBFR_SIZE + 6],
            lsp_history: [0.0; LP_FILTER_ORDER],
            gain_mem: 0.0,
            energy_history: [0.0; 4],
            highpass_filt_mem: [0.0; 2],
            postfilter_mem: [0.0; LP_FILTER_ORDER],
            tilt_mem: 0.0,
            postfilter_agc: 0.0,
            postfilter_mem5k0: [0.0; LP_FILTER_ORDER],
            postfilter_syn5k0: [0.0; LP_FILTER_ORDER + 5 * SUBFR_SIZE],
            pitch_lag_prev: 0,
            iir_mem: [0.0; LP_FILTER_ORDER_16K],
            filt_buf: [[0.0; LP_FILTER_ORDER_16K]; 2],
            filt_idx: 0,
            mem_preemph: [0.0; LP_FILTER_ORDER_16K],
            synth_16k: [0.0; LP_FILTER_ORDER_16K],
            lsp_history_16k: [0.0; LP_FILTER_ORDER_16K],
        };
        ctx.reset_state();
        ctx
    }

    /// The state sipr_decoder_init() and ff_sipr_init_16k() leave in a zeroed context.
    pub fn reset_state(&mut self) {
        self.past_pitch_gain = 0.0;
        self.lsf_history.fill(0.0);
        self.excitation.fill(0.0);
        self.synth_buf.fill(0.0);
        for (i, x) in self.lsp_history.iter_mut().enumerate() {
            *x = (((i + 1) as f64 * PI / (LP_FILTER_ORDER + 1) as f64).cos()) as f32;
        }
        self.gain_mem = 0.0;
        self.energy_history = [-14.0; 4];
        self.highpass_filt_mem = [0.0; 2];
        self.postfilter_mem.fill(0.0);
        self.tilt_mem = 0.0;
        self.postfilter_agc = 0.0;
        self.postfilter_mem5k0.fill(0.0);
        self.postfilter_syn5k0.fill(0.0);
        self.pitch_lag_prev = 180;
        self.iir_mem.fill(0.0);
        self.filt_buf = [[0.0; LP_FILTER_ORDER_16K]; 2];
        self.filt_idx = 0;
        self.mem_preemph.fill(0.0);
        self.synth_16k.fill(0.0);
        for (i, x) in self.lsp_history_16k.iter_mut().enumerate() {
            *x = ((i + 1) as f64 * PI / (LP_FILTER_ORDER_16K + 1) as f64).cos();
        }
    }

    fn postfilter_5k0(&mut self, lpc: &[f32], samples: &mut [f32; SUBFR_SIZE]) {
        let mut buf = [0.0f32; SUBFR_SIZE + LP_FILTER_ORDER];
        let mut lpc_n = [0.0f32; LP_FILTER_ORDER];
        let mut lpc_d = [0.0f32; LP_FILTER_ORDER];

        for i in 0..LP_FILTER_ORDER {
            lpc_d[i] = lpc[i] * FF_POW_0_75[i];
            lpc_n[i] = lpc[i] * FF_POW_0_5[i];
        }

        // pole_out is buf[LP_FILTER_ORDER..].
        buf[..LP_FILTER_ORDER].copy_from_slice(&self.postfilter_mem);
        celp_lp_synthesis_filterf(&mut buf, LP_FILTER_ORDER, &lpc_d, samples, LP_FILTER_ORDER);
        self.postfilter_mem.copy_from_slice(&buf[SUBFR_SIZE..]);

        tilt_compensation(&mut self.tilt_mem, 0.4, &mut buf[LP_FILTER_ORDER..]);

        buf[..LP_FILTER_ORDER].copy_from_slice(&self.postfilter_mem5k0);
        self.postfilter_mem5k0.copy_from_slice(&buf[SUBFR_SIZE..]);

        celp_lp_zero_synthesis_filterf(samples, &lpc_n, &buf, LP_FILTER_ORDER, LP_FILTER_ORDER);
    }

    /// postfilter() of sipr16k.c; `synth_buf[..16]` is free scratch on entry
    /// (the synthesis memory is already saved) and the frame is `synth_buf[16..]`.
    fn postfilter_16k(
        &mut self,
        synth_buf: &mut [f32; LP_FILTER_ORDER_16K + 2 * L_SUBFR_16K],
        out_data: &mut [f32],
    ) {
        const ORDER: usize = LP_FILTER_ORDER_16K;
        let cur = self.filt_idx;
        let prev = cur ^ 1;
        let mut buf = [0.0f32; 30 + ORDER];

        for i in 0..ORDER {
            self.filt_buf[cur][i] = self.iir_mem[i] * FF_POW_0_5[i];
        }

        // tmpbuf is buf[ORDER..]: the first 30 samples through last frame's filter.
        buf[..ORDER].copy_from_slice(&self.mem_preemph);
        celp_lp_synthesis_filterf(&mut buf, ORDER, &self.filt_buf[prev], &synth_buf[ORDER..ORDER + 30], ORDER);

        // The same samples through this frame's filter, in place in synth.
        let head: [f32; 30] = synth_buf[ORDER..ORDER + 30].try_into().unwrap();
        synth_buf[..ORDER].copy_from_slice(&self.mem_preemph);
        celp_lp_synthesis_filterf(synth_buf, ORDER, &self.filt_buf[cur], &head, ORDER);

        out_data[30 - ORDER..30].copy_from_slice(&synth_buf[30..30 + ORDER]);
        celp_lp_synthesis_filterf(
            out_data,
            30,
            &self.filt_buf[cur],
            &synth_buf[ORDER + 30..ORDER + 2 * L_SUBFR_16K],
            ORDER,
        );

        self.mem_preemph
            .copy_from_slice(&out_data[2 * L_SUBFR_16K - ORDER..2 * L_SUBFR_16K]);

        self.filt_idx = prev;

        // Cross-fade from the old filter to the new one; s accumulates 1.0/30 in float.
        let mut s = 0.0f32;
        for i in 0..30 {
            let old = buf[ORDER + i];
            out_data[i] = s.mul_add(synth_buf[ORDER + i] - old, old);
            s = (s as f64 + 1.0 / 30.0) as f32;
        }
    }

    fn decode_frame_16k(&mut self, params: &SiprParameters, out_data: &mut [f32]) {
        const ORDER: usize = LP_FILTER_ORDER_16K;
        let frame_size = SUBFRAME_COUNT_16K * L_SUBFR_16K;
        let mut lsf_new = [0.0f32; ORDER];
        let mut lsp_new = [0.0f64; ORDER];
        let mut az = [[0.0f32; ORDER]; 2];
        let mut fixed_vector = [0.0f32; L_SUBFR_16K];

        lsf_decode_fp_16k(&mut self.lsf_history, &mut lsf_new, &params.vq_indexes, params.ma_pred_switch);

        set_min_dist_lsf(&mut lsf_new, LSFQ_DIFF_MIN / 2.0);

        // lsf2lsp: cosf, widened to double.
        for i in 0..ORDER {
            lsp_new[i] = lsf_new[i].cos() as f64;
        }

        let [az0, az1] = &mut az;
        acelp_lp_decodef(az0, az1, &lsp_new, &self.lsp_history_16k);
        self.lsp_history_16k = lsp_new;

        // synth_buf: ORDER samples of synthesis memory, then the frame.
        let mut synth_buf = [0.0f32; ORDER + 2 * L_SUBFR_16K];
        synth_buf[..ORDER].copy_from_slice(&self.synth_16k);

        let gain_codef_scale = (L_SUBFR_16K as f64).sqrt() as f32;
        let energy_mean = (19.0 - 15.0 / (0.05 * LN_10 / LN_2)) as f32;

        for i in 0..SUBFRAME_COUNT_16K {
            let i_subfr = i * L_SUBFR_16K;
            let pitch_delay_3x = if i == 0 {
                dec_delay3_1st(params.pitch_delay[i] as i32)
            } else {
                dec_delay3_2nd(params.pitch_delay[i] as i32, PITCH_MIN, PITCH_MAX, self.pitch_lag_prev)
            };

            let pitch_fac = GAIN_PITCH_CB_16K[params.gp_index[i] & 15];
            let mut f = AmrFixed {
                pitch_fac: ffmin(pitch_fac, 1.0),
                pitch_lag: divide_by_3(pitch_delay_3x + 1) as usize,
                ..AmrFixed::default()
            };
            self.pitch_lag_prev = f.pitch_lag as i32;

            let pitch_delay_int = divide_by_3(pitch_delay_3x + 2);
            let pitch_delay_frac = pitch_delay_3x + 2 - 3 * pitch_delay_int;

            let exc = EXC_HISTORY_16K + i_subfr;
            acelp_interpolatef(
                &mut self.excitation,
                exc,
                (exc as i32 - pitch_delay_int + 1) as usize,
                &SINC_WIN,
                3,
                (pitch_delay_frac + 1) as usize,
                LP_FILTER_ORDER,
                L_SUBFR_16K,
            );

            fixed_vector.fill(0.0);
            decode_10_pulses_35bits(&params.fc_indexes[i], &mut f, &FF_FC_4PULSES_8BITS_TRACKS_13, 5, 4);
            set_fixed_vector(&mut fixed_vector, &f, 1.0);

            let gain_corr_factor = GAIN_CB_16K[params.gc_index[i] & 31];
            let gain_code = gain_corr_factor
                * acelp_decode_gain_codef(
                    gain_codef_scale,
                    &fixed_vector,
                    energy_mean,
                    &PRED_16K,
                    &self.energy_history[..2],
                );

            self.energy_history[1] = self.energy_history[0];
            self.energy_history[0] = (20.0 * gain_corr_factor.log10() as f64) as f32;

            weighted_vector_sumf(
                &mut self.excitation[exc..exc + L_SUBFR_16K],
                &fixed_vector,
                pitch_fac,
                gain_code,
            );

            celp_lp_synthesis_filterf(
                &mut synth_buf[i_subfr..],
                ORDER,
                &az[i],
                &self.excitation[exc..exc + L_SUBFR_16K],
                ORDER,
            );
        }

        self.synth_16k.copy_from_slice(&synth_buf[frame_size..frame_size + ORDER]);

        self.excitation.copy_within(frame_size..frame_size + EXC_HISTORY_16K, 0);

        self.postfilter_16k(&mut synth_buf, &mut out_data[..frame_size]);

        self.iir_mem = az[1];
    }

    fn decode_frame_nb(&mut self, params: &SiprParameters, out_data: &mut [f32]) {
        let subframe_count = self.mode.subframe_count();
        let frame_size = subframe_count * SUBFR_SIZE;
        let mut az = [0.0f32; LP_FILTER_ORDER * 5];
        let mut lsf_new = [0.0f32; LP_FILTER_ORDER];
        let mut ir_buf = [0.0f32; SUBFR_SIZE + LP_FILTER_ORDER];
        let mut t0_first = 0i32;
        let energy_mean = (34.0 - 15.0 / (0.05 * LN_10 / LN_2)) as f32;

        lsf_decode_fp(&mut lsf_new, &mut self.lsf_history[..LP_FILTER_ORDER], &params.vq_indexes);
        sipr_decode_lp(&lsf_new, &self.lsp_history, &mut az, subframe_count);
        self.lsp_history = lsf_new;

        for i in 0..subframe_count {
            let p_az = &az[i * LP_FILTER_ORDER..(i + 1) * LP_FILTER_ORDER];
            let mut fixed_vector = [0.0f32; SUBFR_SIZE];

            let (t0, t0_frac) = decode_pitch_lag(
                params.pitch_delay[i],
                t0_first,
                i,
                self.mode == SiprMode::Mode5k0,
                6,
            );

            if i == 0 || (i == 2 && self.mode == SiprMode::Mode5k0) {
                t0_first = t0;
            }

            let exc = EXC_HISTORY_NB + i * SUBFR_SIZE;
            acelp_interpolatef(
                &mut self.excitation,
                exc,
                (exc as i32 - t0 + i32::from(t0_frac <= 0)) as usize,
                &FF_B60_SINC,
                6,
                (2 * ((2 + t0_frac) % 3 + 1)) as usize,
                LP_FILTER_ORDER,
                SUBFR_SIZE,
            );

            let mut fixed_cb = AmrFixed::default();
            let low_gain = (self.past_pitch_gain as f64) < 0.8;
            decode_fixed_sparse(&mut fixed_cb, &params.fc_indexes[i], self.mode, low_gain);

            eval_ir(p_az, t0 as usize, &mut ir_buf, self.mode.pitch_sharp_factor());

            convolute_with_sparse(&mut fixed_vector, &fixed_cb, &ir_buf[LP_FILTER_ORDER..]);

            let avg_energy = ((0.01 + scalarproduct(&fixed_vector, &fixed_vector) as f64)
                / SUBFR_SIZE as f64) as f32;

            let gain = GAIN_CB[params.gc_index[i] & 127];
            let mut pitch_gain = gain[0];
            self.past_pitch_gain = pitch_gain;

            let mut gain_code =
                amr_set_fixed_gain(gain[1], avg_energy, &mut self.energy_history, energy_mean, &PRED);

            let excitation = &mut self.excitation[exc..exc + SUBFR_SIZE];
            weighted_vector_sumf(excitation, &fixed_vector, pitch_gain, gain_code);

            pitch_gain = (pitch_gain as f64 * (0.5 * pitch_gain as f64)) as f32;
            if pitch_gain as f64 > 0.4 {
                pitch_gain = 0.4;
            }

            self.gain_mem = 0.7f64.mul_add(self.gain_mem as f64, 0.3 * pitch_gain as f64) as f32;
            self.gain_mem = ffmin(self.gain_mem, pitch_gain);
            gain_code *= self.gain_mem;

            for (fv, &e) in fixed_vector.iter_mut().zip(excitation.iter()) {
                *fv = (-gain_code).mul_add(*fv, e);
            }

            if self.mode == SiprMode::Mode5k0 {
                self.postfilter_5k0(p_az, &mut fixed_vector);

                celp_lp_synthesis_filterf(
                    &mut self.postfilter_syn5k0[i * SUBFR_SIZE..],
                    LP_FILTER_ORDER,
                    p_az,
                    &self.excitation[exc..exc + SUBFR_SIZE],
                    LP_FILTER_ORDER,
                );
            }

            // synth = synth_buf + 16; its 10 memory samples sit at synth_buf[6..16].
            celp_lp_synthesis_filterf(
                &mut self.synth_buf[6 + i * SUBFR_SIZE..],
                LP_FILTER_ORDER,
                p_az,
                &fixed_vector,
                LP_FILTER_ORDER,
            );
        }

        self.synth_buf.copy_within(6 + frame_size..16 + frame_size, 6);

        if self.mode == SiprMode::Mode5k0 {
            for i in 0..subframe_count {
                let start = LP_FILTER_ORDER + i * SUBFR_SIZE;
                let syn = &self.postfilter_syn5k0[start..start + SUBFR_SIZE];
                let energy = scalarproduct(syn, syn);
                adaptive_gain_control(
                    &mut self.synth_buf[16 + i * SUBFR_SIZE..16 + (i + 1) * SUBFR_SIZE],
                    energy,
                    0.9,
                    &mut self.postfilter_agc,
                );
            }

            self.postfilter_syn5k0.copy_within(frame_size..frame_size + LP_FILTER_ORDER, 0);
        }

        self.excitation.copy_within(frame_size..frame_size + EXC_HISTORY_NB, 0);

        acelp_apply_order_2_transfer_function(
            &mut out_data[..frame_size],
            &self.synth_buf[16..16 + frame_size],
            [-1.99997, 1.000000000],
            [-1.93307352, 0.935891986],
            0.939805806,
            &mut self.highpass_filt_mem,
        );
    }

    pub fn decode_packet(&mut self, data: &[u8]) -> CoreResult<Vec<f32>> {
        let expected_size = self.mode.packet_size();
        if data.len() < expected_size {
            return Err(CoreError::invalid("sipr: packet too small"));
        }

        let mut gb = BitReaderLe::new(&data[..expected_size]);
        let frames_count = self.mode.frames_per_packet();
        let samples_per_frame = self.mode.samples_per_packet() / frames_count;
        let mut out = vec![0.0f32; self.mode.samples_per_packet()];

        for frame_out in out.chunks_exact_mut(samples_per_frame) {
            let parms = decode_parameters(&mut gb, self.mode)
                .ok_or_else(|| CoreError::invalid("sipr: bitstream truncated"))?;

            if self.mode == SiprMode::Mode16k {
                self.decode_frame_16k(&parms, frame_out);
            } else {
                self.decode_frame_nb(&parms, frame_out);
            }
        }

        Ok(out)
    }
}

pub struct SiprDecoder {
    pub inner: SiprContext,
    pub codec_id: CodecId,
    mode_determined: bool,
}

impl SiprDecoder {
    pub fn new(params: &CodecParameters) -> Self {
        let mode = if params.sample_rate == Some(16000) {
            SiprMode::Mode16k
        } else if let Some(br) = params.bit_rate {
            if br > 12200 {
                SiprMode::Mode16k
            } else if br > 7500 {
                SiprMode::Mode8k5
            } else if br > 5750 {
                SiprMode::Mode6k5
            } else {
                SiprMode::Mode5k0
            }
        } else {
            SiprMode::Mode8k5
        };

        let mode_determined = params.sample_rate == Some(16000) || params.bit_rate.is_some();

        Self {
            inner: SiprContext::new(mode),
            codec_id: CodecId::new("sipr"),
            mode_determined,
        }
    }
}

pub struct SiprDecoderWrapper {
    decoder: SiprDecoder,
    pending_frames: Vec<Frame>,
}

impl SiprDecoderWrapper {
    pub fn new(params: &CodecParameters) -> Self {
        Self {
            decoder: SiprDecoder::new(params),
            pending_frames: Vec::new(),
        }
    }
}

impl Decoder for SiprDecoderWrapper {
    fn codec_id(&self) -> &CodecId {
        &self.decoder.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::F32,
            sample_rate: self.decoder.inner.mode.sample_rate(),
            channels: 1,
        })
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if packet.data.is_empty() {
            return Ok(());
        }

        if !self.decoder.mode_determined {
            // Without a bit rate, the mode follows from the packet length.
            let mode = match packet.data.len() {
                20 => Some(SiprMode::Mode16k),
                19 => Some(SiprMode::Mode8k5),
                29 => Some(SiprMode::Mode6k5),
                37 => Some(SiprMode::Mode5k0),
                _ => None,
            };
            if let Some(mode) = mode {
                self.decoder.inner.mode = mode;
                self.decoder.inner.reset_state();
                self.decoder.mode_determined = true;
            }
        }

        let pkt_size = self.decoder.inner.mode.packet_size();
        if packet.data.len() < pkt_size {
            return Err(CoreError::invalid("sipr: packet too small"));
        }

        let mut pts = packet.pts;
        for chunk in packet.data.chunks_exact(pkt_size) {
            let samples = self.decoder.inner.decode_packet(chunk)?;
            let mut byte_data = Vec::with_capacity(samples.len() * 4);
            for s in &samples {
                byte_data.extend_from_slice(&s.to_le_bytes());
            }

            self.pending_frames.push(Frame::Audio(AudioFrame {
                samples: samples.len() as u32,
                pts,
                data: vec![byte_data],
            }));

            pts = None;
        }

        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        if self.pending_frames.is_empty() {
            Err(CoreError::NeedMore)
        } else {
            Ok(self.pending_frames.remove(0))
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.pending_frames.clear();
        self.decoder.inner.reset_state();
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    Ok(Box::new(SiprDecoderWrapper::new(params)))
}

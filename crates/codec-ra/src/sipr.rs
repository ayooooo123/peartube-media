//! RealAudio SIPR / ACELP.NET speech decoder.
//!
//! Ported from FFmpeg (commit 2da55bf):
//! - libavcodec/sipr.c
//! - libavcodec/sipr16k.c
//! - libavcodec/siprdata.h
//! - libavcodec/sipr16kdata.h
//! - libavcodec/acelp_pitch_delay.c
//! - libavcodec/acelp_vectors.c
//! - libavcodec/acelp_filters.c
//! - libavcodec/celp_filters.c
//! - libavcodec/lsp.c
//!
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

use std::f32::consts::PI;
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
pub const LSFQ_DIFF_MIN: f32 = 0.0125 * PI;
pub const L_INTERPOL: usize = LP_FILTER_ORDER + 1; // 11
pub const SUBFR_SIZE: usize = 48;
pub const L_SUBFR_16K: usize = 80;
pub const SUBFRAME_COUNT_16K: usize = 2;

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

fn set_min_dist_lsf(lsf: &mut [f32], min_spacing: f32) {
    let mut prev = 0.0f32;
    for x in lsf.iter_mut() {
        prev = (*x).max(prev + min_spacing);
        *x = prev;
    }
}

fn lsp2polyf(lsp: &[f64], f: &mut [f64], lp_half_order: usize) {
    f[0] = 1.0;
    f[1] = -2.0 * lsp[0];
    for i in 2..=lp_half_order {
        let val = -2.0 * lsp[2 * i - 2];
        f[i] = val * f[i - 1] + 2.0 * f[i - 2];
        for j in (2..i).rev() {
            f[j] += f[j - 1] * val + f[j - 2];
        }
        f[1] += val;
    }
}

fn amrwb_lsp2lpc(lsp: &[f64; 10], lp: &mut [f32; 10]) {
    let lp_order = 10;
    let lp_half_order = 5;
    let mut pa = [0.0f64; 6];
    let mut qa_raw = [0.0f64; 5];
    lsp2polyf(lsp, &mut pa, lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa_raw, lp_half_order - 1);

    let mut qa = [0.0f64; 6];
    qa[1..].copy_from_slice(&qa_raw);
    qa[0] = 0.0; // qa[-1] in C

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

fn acelp_lspd2lpc(lsp: &[f64], lpc: &mut [f32], lp_half_order: usize) {
    let mut pa = [0.0f64; 11];
    let mut qa = [0.0f64; 11];
    lsp2polyf(lsp, &mut pa[..=lp_half_order], lp_half_order);
    lsp2polyf(&lsp[1..], &mut qa[..=lp_half_order], lp_half_order);

    for k in 0..lp_half_order {
        let paf = pa[k + 1] + pa[k];
        let qaf = qa[k + 1] - qa[k];
        lpc[k] = (0.5 * (paf + qaf)) as f32;
        lpc[2 * lp_half_order - 1 - k] = (0.5 * (paf - qaf)) as f32;
    }
}

fn acelp_lp_decodef(
    lp_1st: &mut [f32; 16],
    lp_2nd: &mut [f32; 16],
    lsp_2nd: &[f64; 16],
    lsp_prev: &[f64; 16],
) {
    let mut lsp_1st = [0.0f64; 16];
    for i in 0..16 {
        lsp_1st[i] = (lsp_2nd[i] + lsp_prev[i]) * 0.5;
    }
    acelp_lspd2lpc(&lsp_1st, lp_1st, 8);
    acelp_lspd2lpc(lsp_2nd, lp_2nd, 8);
}

fn lsf_decode_fp_16k(
    lsf_history: &mut [f32; 16],
    isp_new: &mut [f32; 16],
    parm: &[usize; 5],
    ma_pred: usize,
) {
    let mut isp_q = [0.0f32; 16];
    let cb1 = &LSF_CB1_16K[parm[0] & 127];
    let cb2 = &LSF_CB2_16K[parm[1] & 255];
    let cb3 = &LSF_CB3_16K[parm[2] & 127];
    let cb4 = &LSF_CB4_16K[parm[3] & 127];
    let cb5 = &LSF_CB5_16K[parm[4] & 127];
    isp_q[0..3].copy_from_slice(cb1);
    isp_q[3..6].copy_from_slice(cb2);
    isp_q[6..9].copy_from_slice(cb3);
    isp_q[9..12].copy_from_slice(cb4);
    isp_q[12..16].copy_from_slice(cb5);

    let q = QU[ma_pred & 1];
    for i in 0..16 {
        isp_new[i] = (1.0 - q) * isp_q[i] + q * lsf_history[i] + MEAN_LSF_16K[i];
    }
    lsf_history.copy_from_slice(&isp_q);
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
            v += buf[in_pos + n + i] * filter_coeffs[idx + frac_pos];
            idx += precision;
            v += buf[in_pos + n - (i + 1)] * filter_coeffs[idx - frac_pos];
        }
        buf[out_pos + n] = v;
    }
}

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

fn set_fixed_vector(out: &mut [f32], fixed: &AmrFixed, scale: f32, size: usize) {
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

fn acelp_decode_gain_codef(
    gain_corr_factor: f32,
    fc_v: &[f32],
    mut mr_energy: f32,
    quant_energy: &[f32],
    ma_prediction_coeff: &[f32],
    subframe_size: usize,
    ma_pred_order: usize,
) -> f32 {
    let dot_quant: f32 = quant_energy[..ma_pred_order]
        .iter()
        .zip(&ma_prediction_coeff[..ma_pred_order])
        .map(|(a, b)| a * b)
        .sum();
    mr_energy += dot_quant;

    let dot_fc: f32 = fc_v[..subframe_size].iter().map(|x| x * x).sum();

    gain_corr_factor * ((std::f32::consts::LN_10 / 20.0) * mr_energy).exp()
        / (0.01 + dot_fc).sqrt()
}

fn celp_lp_synthesis_filterf(
    buf: &mut [f32],
    history_len: usize,
    filter_coeffs: &[f32],
    in_samples: &[f32],
    buffer_length: usize,
    filter_length: usize,
) {
    for n in 0..buffer_length {
        let mut val = in_samples[n];
        for i in 1..=filter_length {
            val -= filter_coeffs[i - 1] * buf[history_len + n - i];
        }
        buf[history_len + n] = val;
    }
}

fn lsf_decode_fp(lsfnew: &mut [f32; 10], lsf_history: &mut [f32], vq_indexes: &[usize; 5]) {
    let mut lsf_tmp = [0.0f32; 10];
    lsf_tmp[0..2].copy_from_slice(&LSF_CB1[vq_indexes[0] & 63]);
    lsf_tmp[2..4].copy_from_slice(&LSF_CB2[vq_indexes[1] & 127]);
    lsf_tmp[4..6].copy_from_slice(&LSF_CB3[vq_indexes[2] & 127]);
    lsf_tmp[6..8].copy_from_slice(&LSF_CB4[vq_indexes[3] & 127]);
    lsf_tmp[8..10].copy_from_slice(&LSF_CB5[vq_indexes[4] & 31]);

    for i in 0..10 {
        lsfnew[i] = lsf_history[i] * 0.33 + lsf_tmp[i] + MEAN_LSF[i];
    }

    sort_nearly_sorted_floats(&mut lsfnew[..9]);
    set_min_dist_lsf(&mut lsfnew[..9], LSFQ_DIFF_MIN);
    lsfnew[9] = lsfnew[9].min(1.3 * PI);

    lsf_history.copy_from_slice(&lsf_tmp);

    for i in 0..9 {
        lsfnew[i] = lsfnew[i].cos();
    }
    lsfnew[9] *= 6.153848 / PI;
}

fn sipr_decode_lp(
    lsfnew: &[f32; 10],
    lsfold: &[f32; 10],
    az: &mut [f32],
    num_subfr: usize,
) {
    let mut lsfint = [0.0f64; 10];
    let t0 = 1.0f32 / num_subfr as f32;
    let mut t = t0 * 0.5;
    for i in 0..num_subfr {
        for j in 0..10 {
            lsfint[j] = (lsfold[j] * (1.0 - t) + t * lsfnew[j]) as f64;
        }
        let out_az: &mut [f32; 10] = (&mut az[i * 10..(i + 1) * 10]).try_into().unwrap();
        amrwb_lsp2lpc(&lsfint, out_az);
        t += t0;
    }
}

fn eval_ir(
    az: &[f32],
    pitch_lag: usize,
    ir_buf: &mut [f32], // size 58 (10 history + 48 output)
    pitch_sharp_factor: f32,
) {
    let mut tmp1 = [0.0f32; SUBFR_SIZE + 1]; // 49
    let mut tmp2 = [0.0f32; LP_FILTER_ORDER]; // 10

    tmp1[0] = 1.0;
    for i in 0..LP_FILTER_ORDER {
        tmp1[i + 1] = az[i] * FF_POW_0_55[i];
        tmp2[i] = az[i] * FF_POW_0_7[i];
    }
    tmp1[11..48].fill(0.0);

    celp_lp_synthesis_filterf(ir_buf, LP_FILTER_ORDER, &tmp2, &tmp1[..SUBFR_SIZE], SUBFR_SIZE, LP_FILTER_ORDER);

    let freq = &mut ir_buf[LP_FILTER_ORDER..LP_FILTER_ORDER + SUBFR_SIZE];
    if pitch_lag < SUBFR_SIZE {
        for i in pitch_lag..SUBFR_SIZE {
            freq[i] += pitch_sharp_factor * freq[i - pitch_lag];
        }
    }
}

fn decode_fixed_sparse(
    fixed_sparse: &mut AmrFixed,
    pulses: &[i16; 10],
    mode: SiprMode,
    low_gain: bool,
) {
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

fn convolute_with_sparse(
    out: &mut [f32],
    pulses: &AmrFixed,
    shape: &[f32],
    length: usize,
) {
    out[..length].fill(0.0);
    for i in 0..pulses.n {
        let px = pulses.x[i];
        let py = pulses.y[i];
        if px < length {
            for j in px..length {
                out[j] += py * shape[j - px];
            }
        }
    }
}

fn amr_set_fixed_gain(
    fixed_gain_factor: f32,
    fixed_mean_energy: f32,
    prediction_error: &mut [f32; 4],
    energy_mean: f32,
    pred_table: &[f32; 4],
) -> f32 {
    let dot: f32 = pred_table.iter().zip(prediction_error.iter()).map(|(a, b)| a * b).sum();
    let exp_arg = 0.05 * (dot + energy_mean);
    let exp_val = 10.0f64.powf(exp_arg as f64) as f32;
    let denom = if fixed_mean_energy != 0.0 {
        fixed_mean_energy.sqrt()
    } else {
        1.0
    };
    let val = fixed_gain_factor * exp_val / denom;

    prediction_error.copy_within(1..4, 0);
    prediction_error[3] = 20.0 * fixed_gain_factor.log10();

    val
}

fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32], size: usize) {
    let new_tilt_mem = samples[size - 1];
    for i in (1..size).rev() {
        samples[i] -= tilt * samples[i - 1];
    }
    samples[0] -= tilt * *mem;
    *mem = new_tilt_mem;
}

fn adaptive_gain_control(
    out: &mut [f32],
    input: &[f32],
    speech_energ: f32,
    size: usize,
    alpha: f32,
    gain_mem: &mut f32,
) {
    let postfilter_energ: f32 = input[..size].iter().map(|x| x * x).sum();
    let mut gain_scale_factor = 1.0f32;
    if postfilter_energ != 0.0 {
        gain_scale_factor = (speech_energ / postfilter_energ).sqrt();
    }
    gain_scale_factor *= 1.0 - alpha;

    let mut mem = *gain_mem;
    for i in 0..size {
        mem = alpha * mem + gain_scale_factor;
        out[i] = input[i] * mem;
    }
    *gain_mem = mem;
}

fn acelp_apply_order_2_transfer_function(
    out: &mut [f32],
    input: &[f32],
    zero_coeffs: [f32; 2],
    pole_coeffs: [f32; 2],
    gain: f32,
    mem: &mut [f32; 2],
    n: usize,
) {
    for i in 0..n {
        let tmp = gain * input[i] - pole_coeffs[0] * mem[0] - pole_coeffs[1] * mem[1];
        out[i] = tmp + zero_coeffs[0] * mem[0] + zero_coeffs[1] * mem[1];
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

pub struct SiprContext {
    pub mode: SiprMode,
    past_pitch_gain: f32,
    lsf_history: [f32; LP_FILTER_ORDER_16K],
    excitation: [f32; L_INTERPOL + PITCH_MAX as usize + 2 * L_SUBFR_16K + 64], // 516
    synth_buf: [f32; LP_FILTER_ORDER + 5 * SUBFR_SIZE + 16], // 266
    lsp_history: [f32; LP_FILTER_ORDER],
    gain_mem: f32,
    energy_history: [f32; 4],
    highpass_filt_mem: [f32; 2],
    postfilter_mem: [f32; LP_FILTER_ORDER],
    tilt_mem: f32,
    postfilter_agc: f32,
    postfilter_mem5k0: [f32; LP_FILTER_ORDER],
    postfilter_syn5k0: [f32; LP_FILTER_ORDER + 5 * SUBFR_SIZE], // 250
    pitch_lag_prev: i32,
    iir_mem: [f32; LP_FILTER_ORDER_16K],
    filt_buf: [[f32; LP_FILTER_ORDER_16K]; 2],
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
            excitation: [0.0; L_INTERPOL + PITCH_MAX as usize + 2 * L_SUBFR_16K + 64],
            synth_buf: [0.0; LP_FILTER_ORDER + 5 * SUBFR_SIZE + 16],
            lsp_history: [0.0; LP_FILTER_ORDER],
            gain_mem: 0.0,
            energy_history: [-14.0; 4],
            highpass_filt_mem: [0.0; 2],
            postfilter_mem: [0.0; LP_FILTER_ORDER],
            tilt_mem: 0.0,
            postfilter_agc: 0.0,
            postfilter_mem5k0: [0.0; LP_FILTER_ORDER],
            postfilter_syn5k0: [0.0; LP_FILTER_ORDER + 5 * SUBFR_SIZE],
            pitch_lag_prev: 180,
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

    pub fn reset_state(&mut self) {
        self.past_pitch_gain = 0.0;
        self.lsf_history.fill(0.0);
        self.excitation.fill(0.0);
        self.synth_buf.fill(0.0);
        for i in 0..LP_FILTER_ORDER {
            self.lsp_history[i] = ((i + 1) as f32 * PI / (LP_FILTER_ORDER + 1) as f32).cos();
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
        for i in 0..LP_FILTER_ORDER_16K {
            self.lsp_history_16k[i] =
                (((i + 1) as f64) * std::f64::consts::PI / (LP_FILTER_ORDER_16K + 1) as f64).cos();
        }
    }

    fn postfilter_5k0(&mut self, lpc: &[f32], samples: &mut [f32]) {
        let mut buf = [0.0f32; SUBFR_SIZE + LP_FILTER_ORDER]; // 58
        let mut lpc_n = [0.0f32; LP_FILTER_ORDER];
        let mut lpc_d = [0.0f32; LP_FILTER_ORDER];

        for i in 0..LP_FILTER_ORDER {
            lpc_d[i] = lpc[i] * FF_POW_0_75[i];
            lpc_n[i] = lpc[i] * FF_POW_0_5[i];
        }

        buf[..LP_FILTER_ORDER].copy_from_slice(&self.postfilter_mem);
        celp_lp_synthesis_filterf(
            &mut buf,
            LP_FILTER_ORDER,
            &lpc_d,
            samples,
            SUBFR_SIZE,
            LP_FILTER_ORDER,
        );
        self.postfilter_mem
            .copy_from_slice(&buf[SUBFR_SIZE..SUBFR_SIZE + LP_FILTER_ORDER]);

        tilt_compensation(&mut self.tilt_mem, 0.4, &mut buf[LP_FILTER_ORDER..], SUBFR_SIZE);

        buf[..LP_FILTER_ORDER].copy_from_slice(&self.postfilter_mem5k0);
        self.postfilter_mem5k0
            .copy_from_slice(&buf[SUBFR_SIZE..SUBFR_SIZE + LP_FILTER_ORDER]);

        // celp_lp_zero_synthesis_filterf: samples[n] = buf[10 + n] + sum(lpc_n[i-1] * buf[10 + n - i])
        for n in 0..SUBFR_SIZE {
            let mut val = buf[LP_FILTER_ORDER + n];
            for i in 1..=LP_FILTER_ORDER {
                val += lpc_n[i - 1] * buf[LP_FILTER_ORDER + n - i];
            }
            samples[n] = val;
        }
    }

    fn postfilter_16k(&mut self, synth: &[f32], out_data: &mut [f32; 160]) {
        let mut buf = [0.0f32; 30 + LP_FILTER_ORDER_16K]; // 46
        let curr_filt = self.filt_idx;
        let prev_filt = 1 - curr_filt;

        for i in 0..LP_FILTER_ORDER_16K {
            self.filt_buf[curr_filt][i] = self.iir_mem[i] * FF_POW_0_5[i];
        }

        buf[..LP_FILTER_ORDER_16K].copy_from_slice(&self.mem_preemph);
        // tmpbuf = buf[16..46], using filt_buf[prev_filt]
        celp_lp_synthesis_filterf(
            &mut buf,
            LP_FILTER_ORDER_16K,
            &self.filt_buf[prev_filt],
            &synth[..30],
            30,
            LP_FILTER_ORDER_16K,
        );

        // synth_buf[0..16] = mem_preemph, synth_buf[16..46] filtered in-place with filt_buf[curr_filt]
        let mut synth_buf = [0.0f32; 16 + 160];
        synth_buf[..LP_FILTER_ORDER_16K].copy_from_slice(&self.mem_preemph);
        celp_lp_synthesis_filterf(
            &mut synth_buf,
            LP_FILTER_ORDER_16K,
            &self.filt_buf[curr_filt],
            &synth[..30],
            30,
            LP_FILTER_ORDER_16K,
        );

        // out_data[14..30] = synth_buf[30..46]
        out_data[30 - LP_FILTER_ORDER_16K..30]
            .copy_from_slice(&synth_buf[30..46]);

        // celp_lp_synthesis_filterf on out_data + 30 (130 samples), using history out_data[14..30]
        for n in 0..(160 - 30) {
            let mut val = synth[30 + n];
            for i in 1..=LP_FILTER_ORDER_16K {
                val -= self.filt_buf[curr_filt][i - 1] * out_data[30 + n - i];
            }
            out_data[30 + n] = val;
        }

        self.mem_preemph
            .copy_from_slice(&out_data[160 - LP_FILTER_ORDER_16K..160]);

        self.filt_idx = 1 - self.filt_idx;

        for i in 0..30 {
            let s = (i as f32) / 30.0;
            out_data[i] = buf[LP_FILTER_ORDER_16K + i]
                + s * (synth_buf[LP_FILTER_ORDER_16K + i] - buf[LP_FILTER_ORDER_16K + i]);
        }
    }

    fn decode_frame_16k(&mut self, params: &SiprParameters, out_data: &mut [f32]) {
        let frame_size = SUBFRAME_COUNT_16K * L_SUBFR_16K; // 160
        let mut lsf_new = [0.0f32; LP_FILTER_ORDER_16K];
        let mut lsp_new = [0.0f64; LP_FILTER_ORDER_16K];
        let mut az = [[0.0f32; LP_FILTER_ORDER_16K]; 2];
        let mut fixed_vector = [0.0f32; L_SUBFR_16K];

        let exc_start = 292; // PITCH_MAX (281) + L_INTERPOL (11)

        lsf_decode_fp_16k(
            &mut self.lsf_history,
            &mut lsf_new,
            &params.vq_indexes,
            params.ma_pred_switch,
        );

        set_min_dist_lsf(&mut lsf_new, LSFQ_DIFF_MIN * 0.5);

        for i in 0..LP_FILTER_ORDER_16K {
            lsp_new[i] = (lsf_new[i].cos()) as f64;
        }

        let (az0, az1) = az.split_at_mut(1);
        acelp_lp_decodef(&mut az0[0], &mut az1[0], &lsp_new, &self.lsp_history_16k);
        self.lsp_history_16k.copy_from_slice(&lsp_new);
        // synth_buf: history 16, samples 160
        let mut synth_buf = [0.0f32; LP_FILTER_ORDER_16K + 160];
        synth_buf[..LP_FILTER_ORDER_16K].copy_from_slice(&self.synth_16k);

        for i in 0..SUBFRAME_COUNT_16K {
            let i_subfr = i * L_SUBFR_16K;
            let pitch_delay_3x = if i == 0 {
                dec_delay3_1st(params.pitch_delay[0] as i32)
            } else {
                dec_delay3_2nd(
                    params.pitch_delay[1] as i32,
                    PITCH_MIN,
                    PITCH_MAX,
                    self.pitch_lag_prev,
                )
            };

            let pitch_fac = GAIN_PITCH_CB_16K[params.gp_index[i] & 15];
            let mut f = AmrFixed {
                n: 0,
                x: [0; 10],
                y: [0.0; 10],
                pitch_lag: divide_by_3(pitch_delay_3x + 1) as usize,
                pitch_fac: pitch_fac.min(1.0),
            };
            self.pitch_lag_prev = f.pitch_lag as i32;

            let pitch_delay_int = divide_by_3(pitch_delay_3x + 2);
            let pitch_delay_frac = pitch_delay_3x + 2 - 3 * pitch_delay_int;

            let cur_exc_pos = exc_start + i_subfr;
            let in_pos = (cur_exc_pos as i32 - pitch_delay_int + 1) as usize;

            acelp_interpolatef(
                &mut self.excitation,
                cur_exc_pos,
                in_pos,
                &SINC_WIN,
                3,
                (pitch_delay_frac + 1) as usize,
                LP_FILTER_ORDER,
                L_SUBFR_16K,
            );

            fixed_vector.fill(0.0);
            decode_10_pulses_35bits(
                &params.fc_indexes[i],
                &mut f,
                &FF_FC_4PULSES_8BITS_TRACKS_13,
                5,
                4,
            );
            set_fixed_vector(&mut fixed_vector, &f, 1.0, L_SUBFR_16K);

            let gain_corr_factor = GAIN_CB_16K[params.gc_index[i] & 31];
            let energy_mean = 19.0 - 15.0 / (0.05 * std::f32::consts::LN_10 / std::f32::consts::LN_2);
            let gain_code = gain_corr_factor
                * acelp_decode_gain_codef(
                    (L_SUBFR_16K as f32).sqrt(),
                    &fixed_vector,
                    energy_mean,
                    &PRED_16K,
                    &self.energy_history[..2],
                    L_SUBFR_16K,
                    2,
                );


            self.energy_history[1] = self.energy_history[0];
            self.energy_history[0] = 20.0 * gain_corr_factor.log10();

            for j in 0..L_SUBFR_16K {
                self.excitation[cur_exc_pos + j] =
                    pitch_fac * self.excitation[cur_exc_pos + j] + gain_code * fixed_vector[j];
            }


            celp_lp_synthesis_filterf(
                &mut synth_buf[i_subfr..],
                LP_FILTER_ORDER_16K,
                &az[i],
                &self.excitation[cur_exc_pos..cur_exc_pos + L_SUBFR_16K],
                L_SUBFR_16K,
                LP_FILTER_ORDER_16K,
            );

        }

        self.synth_16k
            .copy_from_slice(&synth_buf[frame_size..frame_size + LP_FILTER_ORDER_16K]);

        // memmove(ctx->excitation, ctx->excitation + 160, (11 + 281) * sizeof(float));
        self.excitation
            .copy_within(frame_size..frame_size + 292, 0);

        let synth_samples = &synth_buf[LP_FILTER_ORDER_16K..LP_FILTER_ORDER_16K + 160];
        let out_160: &mut [f32; 160] = (&mut out_data[..160]).try_into().unwrap();
        self.postfilter_16k(synth_samples, out_160);

        self.iir_mem.copy_from_slice(&az[1]);

    }

    fn decode_frame_nb(&mut self, params: &SiprParameters, out_data: &mut [f32]) {
        let subframe_count = self.mode.subframe_count();
        let frame_size = subframe_count * SUBFR_SIZE;
        let mut az = [0.0f32; LP_FILTER_ORDER * 5];
        let mut lsf_new = [0.0f32; LP_FILTER_ORDER];
        let mut ir_buf = [0.0f32; SUBFR_SIZE + LP_FILTER_ORDER]; // 58
        let mut t0_first = 0i32;

        lsf_decode_fp(&mut lsf_new, &mut self.lsf_history[..LP_FILTER_ORDER], &params.vq_indexes);
        sipr_decode_lp(&lsf_new, &self.lsp_history, &mut az[..subframe_count * 10], subframe_count);
        self.lsp_history.copy_from_slice(&lsf_new);

        let exc_start = PITCH_DELAY_MAX as usize + L_INTERPOL; // 143 + 11 = 154

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

            let cur_exc_pos = exc_start + i * SUBFR_SIZE;
            let in_offset = if t0_frac <= 0 { 1 } else { 0 };
            let in_pos = (cur_exc_pos as i32 - t0 + in_offset) as usize;
            let frac_pos = 2 * (((2 + t0_frac) % 3) + 1) as usize;

            acelp_interpolatef(
                &mut self.excitation,
                cur_exc_pos,
                in_pos,
                &FF_B60_SINC,
                6,
                frac_pos,
                LP_FILTER_ORDER,
                SUBFR_SIZE,
            );

            let mut fixed_cb = AmrFixed::default();
            decode_fixed_sparse(
                &mut fixed_cb,
                &params.fc_indexes[i],
                self.mode,
                self.past_pitch_gain < 0.8,
            );

            ir_buf[..LP_FILTER_ORDER].fill(0.0);
            eval_ir(p_az, t0 as usize, &mut ir_buf, self.mode.pitch_sharp_factor());

            convolute_with_sparse(
                &mut fixed_vector,
                &fixed_cb,
                &ir_buf[LP_FILTER_ORDER..],
                SUBFR_SIZE,
            );

            let dot_fixed: f32 = fixed_vector.iter().map(|x| x * x).sum();
            let avg_energy = (0.01 + dot_fixed) / SUBFR_SIZE as f32;

            let mut pitch_gain = GAIN_CB[params.gc_index[i] & 127][0];
            self.past_pitch_gain = pitch_gain;

            let energy_mean = 34.0 - 15.0 / (0.05 * std::f32::consts::LN_10 / std::f32::consts::LN_2);
            let mut gain_code = amr_set_fixed_gain(
                GAIN_CB[params.gc_index[i] & 127][1],
                avg_energy,
                &mut self.energy_history,
                energy_mean,
                &PRED,
            );

            for j in 0..SUBFR_SIZE {
                self.excitation[cur_exc_pos + j] =
                    pitch_gain * self.excitation[cur_exc_pos + j] + gain_code * fixed_vector[j];
            }

            pitch_gain *= 0.5 * pitch_gain;
            pitch_gain = pitch_gain.min(0.4);

            self.gain_mem = 0.7 * self.gain_mem + 0.3 * pitch_gain;
            self.gain_mem = self.gain_mem.min(pitch_gain);
            gain_code *= self.gain_mem;

            for j in 0..SUBFR_SIZE {
                fixed_vector[j] = self.excitation[cur_exc_pos + j] - gain_code * fixed_vector[j];
            }

            if self.mode == SiprMode::Mode5k0 {
                self.postfilter_5k0(p_az, &mut fixed_vector);

                celp_lp_synthesis_filterf(
                    &mut self.postfilter_syn5k0[i * SUBFR_SIZE..],
                    LP_FILTER_ORDER,
                    p_az,
                    &self.excitation[cur_exc_pos..cur_exc_pos + SUBFR_SIZE],
                    SUBFR_SIZE,
                    LP_FILTER_ORDER,
                );
            }

            // synth_buf[6..16] is 10 history samples, synth_buf[16..] is output
            celp_lp_synthesis_filterf(
                &mut self.synth_buf[6 + i * SUBFR_SIZE..],
                LP_FILTER_ORDER,
                p_az,
                &fixed_vector,
                SUBFR_SIZE,
                LP_FILTER_ORDER,
            );
        }

        // Copy last 10 samples of synth to synth_buf[6..16]
        self.synth_buf.copy_within(6 + frame_size..16 + frame_size, 6);

        if self.mode == SiprMode::Mode5k0 {
            for i in 0..subframe_count {
                let syn_slice = &self.postfilter_syn5k0[LP_FILTER_ORDER + i * SUBFR_SIZE..LP_FILTER_ORDER + (i + 1) * SUBFR_SIZE];
                let energy: f32 = syn_slice.iter().map(|x| x * x).sum();
                let synth_subfr = &mut self.synth_buf[16 + i * SUBFR_SIZE..16 + (i + 1) * SUBFR_SIZE];
                let in_copy = synth_subfr.to_vec();
                adaptive_gain_control(
                    synth_subfr,
                    &in_copy,
                    energy,
                    SUBFR_SIZE,
                    0.9,
                    &mut self.postfilter_agc,
                );
            }

            self.postfilter_syn5k0.copy_within(frame_size..frame_size + LP_FILTER_ORDER, 0);
        }

        // memmove(ctx->excitation, excitation - 154, 154 * sizeof(float));
        let final_exc_pos = exc_start + frame_size;
        self.excitation.copy_within(final_exc_pos - exc_start..final_exc_pos, 0);

        acelp_apply_order_2_transfer_function(
            &mut out_data[..frame_size],
            &self.synth_buf[16..16 + frame_size],
            [-1.99997, 1.0],
            [-1.9330735, 0.935892],
            0.9398058,
            &mut self.highpass_filt_mem,
            frame_size,
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

        for f_idx in 0..frames_count {
            let parms = decode_parameters(&mut gb, self.mode)
                .ok_or_else(|| CoreError::invalid("sipr: bitstream truncated"))?;

            let frame_out = &mut out[f_idx * samples_per_frame..(f_idx + 1) * samples_per_frame];
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
            // Determine mode from packet length if possible
            match packet.data.len() {
                20 => {
                    self.decoder.inner.mode = SiprMode::Mode16k;
                    self.decoder.inner.reset_state();
                    self.decoder.mode_determined = true;
                }
                19 => {
                    self.decoder.inner.mode = SiprMode::Mode8k5;
                    self.decoder.inner.reset_state();
                    self.decoder.mode_determined = true;
                }
                29 => {
                    self.decoder.inner.mode = SiprMode::Mode6k5;
                    self.decoder.inner.reset_state();
                    self.decoder.mode_determined = true;
                }
                37 => {
                    self.decoder.inner.mode = SiprMode::Mode5k0;
                    self.decoder.inner.reset_state();
                    self.decoder.mode_determined = true;
                }
                _ => {}
            }
        }

        let pkt_size = self.decoder.inner.mode.packet_size();
        if packet.data.len() < pkt_size {
            return Err(CoreError::invalid("sipr: packet too small"));
        }

        let mut offset = 0;
        let mut pts = packet.pts;
        while offset + pkt_size <= packet.data.len() {
            let samples = self.decoder.inner.decode_packet(&packet.data[offset..offset + pkt_size])?;
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
            offset += pkt_size;
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

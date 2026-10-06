// Ported from FFmpeg (commit 2da55bf): libavcodec/wmavoice.c (tables from
// libavcodec/wmavoice_data.h in wmavoice_tables.rs; the CELP helpers it
// calls are in celp.rs, the av_tx transforms in tx.rs).
// GNU Lesser General Public License 2.1 or later.

//! Windows Media Audio Voice decoder (`wmavoice`).
//!
//! Floating point follows FFmpeg's C path as clang compiles it for arm64:
//! every `a*b ± c` inside one C expression is a single fused multiply-add
//! (`mul_add` below) and every float/double promotion of the C source is
//! kept, so the output matches FFmpeg's C decoder bit for bit.

use std::collections::VecDeque;

use crate::celp::{
    acelp_apply_order_2_transfer_function, acelp_interpolatef, acelp_lspd2lpc, celp_lp_synthesis_filterf,
    celp_lp_zero_synthesis_filterf, scalarproduct_float, set_fixed_vector, sine_window_init, tilt_compensation,
    weighted_vector_sumf_inplace, AmrFixed,
};
use crate::getbits::{GetBits, PutBits};
use crate::tx::{DctI64, DstI64, Rdft128};
use crate::vlc::VlcTable;
use crate::wma_common::av_ceil_log2;
use crate::wmavoice_tables::*;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

const MAX_BLOCKS: usize = 8;
const MAX_LSPS: usize = 16;
const MAX_LSPS_ALIGN16: usize = 16;
const MAX_FRAMES: usize = 3;
const MAX_FRAMESIZE: usize = 160;
const MAX_SIGNAL_HISTORY: usize = 416;
const MAX_SFRAMESIZE: usize = MAX_FRAMESIZE * MAX_FRAMES;
const SFRAME_CACHE_MAXSIZE: usize = 256;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;

const ACB_TYPE_NONE: u8 = 0;
const ACB_TYPE_ASYMMETRIC: u8 = 1;
const ACB_TYPE_HAMMING: u8 = 2;

const FCB_TYPE_SILENCE: u8 = 0;
const FCB_TYPE_HARDCODED: u8 = 1;
const FCB_TYPE_AW_PULSES: u8 = 2;
const FCB_TYPE_EXC_PULSES: u8 = 3;

/// `frame_type_desc`.
#[derive(Clone, Copy)]
struct FrameTypeDesc {
    n_blocks: u8,
    log_n_blocks: u8,
    acb_type: u8,
    fcb_type: u8,
    dbl_pulses: u8,
}

const fn fd(n_blocks: u8, log_n_blocks: u8, acb_type: u8, fcb_type: u8, dbl_pulses: u8) -> FrameTypeDesc {
    FrameTypeDesc { n_blocks, log_n_blocks, acb_type, fcb_type, dbl_pulses }
}

const FRAME_DESCS: [FrameTypeDesc; 17] = [
    fd(1, 0, ACB_TYPE_NONE, FCB_TYPE_SILENCE, 0),
    fd(2, 1, ACB_TYPE_NONE, FCB_TYPE_HARDCODED, 0),
    fd(2, 1, ACB_TYPE_ASYMMETRIC, FCB_TYPE_AW_PULSES, 0),
    fd(2, 1, ACB_TYPE_ASYMMETRIC, FCB_TYPE_EXC_PULSES, 2),
    fd(2, 1, ACB_TYPE_ASYMMETRIC, FCB_TYPE_EXC_PULSES, 5),
    fd(4, 2, ACB_TYPE_ASYMMETRIC, FCB_TYPE_EXC_PULSES, 0),
    fd(4, 2, ACB_TYPE_ASYMMETRIC, FCB_TYPE_EXC_PULSES, 2),
    fd(4, 2, ACB_TYPE_ASYMMETRIC, FCB_TYPE_EXC_PULSES, 5),
    fd(2, 1, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 0),
    fd(2, 1, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 2),
    fd(2, 1, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 5),
    fd(4, 2, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 0),
    fd(4, 2, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 2),
    fd(4, 2, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 5),
    fd(8, 3, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 0),
    fd(8, 3, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 2),
    fd(8, 3, ACB_TYPE_HAMMING, FCB_TYPE_EXC_PULSES, 5),
];

/// `FFMAX` / `FFMIN` as the C macros evaluate them.
#[inline]
fn ffmax<T: PartialOrd>(a: T, b: T) -> T {
    if a > b {
        a
    } else {
        b
    }
}

#[inline]
fn ffmin<T: PartialOrd>(a: T, b: T) -> T {
    if a > b {
        b
    } else {
        a
    }
}

/// `av_clip`.
#[inline]
fn av_clip(a: i32, amin: i32, amax: i32) -> i32 {
    if a < amin {
        amin
    } else if a > amax {
        amax
    } else {
        a
    }
}

/// Why a superframe did not decode; `Flushed` means `wmavoice_flush` ran.
enum SfError {
    Invalid,
    Flushed,
}

/// The postfilter transforms and windows (allocated only with `do_apf`).
struct Apf {
    rdft: Rdft128,
    irdft: Rdft128,
    dct: DctI64,
    dst: DstI64,
    /// 8-bit cosine/sine windows over [-π, π].
    sin: [f32; 511],
    cos: [f32; 511],
}

impl Apf {
    fn new() -> Self {
        let mut sin = [0f32; 511];
        let mut cos = [0f32; 511];
        sine_window_init(&mut cos, 256);
        sin[255..511].copy_from_slice(&cos[..256]);
        for n in 0..255 {
            sin[n] = -sin[510 - n];
            cos[510 - n] = cos[n];
        }
        Self { rdft: Rdft128::new(false), irdft: Rdft128::new(true), dct: DctI64::new(), dst: DstI64::new(), sin, cos }
    }
}

/// The synthesis state of `WMAVoiceContext` (everything but the packet
/// reader and the superframe cache).
struct Synth {
    vbm_tree: [i8; 25],
    frame_type_vlc: VlcTable,
    history_nsamples: usize,

    do_apf: bool,
    denoise_strength: usize,
    denoise_tilt_corr: bool,
    dc_level: u32,

    lsps: usize,
    lsp_q_mode: bool,
    lsp_def_mode: bool,

    min_pitch_val: i32,
    max_pitch_val: i32,
    pitch_nbits: u32,
    block_pitch_nbits: u32,
    block_pitch_range: i32,
    block_delta_pitch_nbits: u32,
    block_delta_pitch_hrange: i32,
    block_conv_table: [i32; 4],

    has_residual_lsps: bool,

    prev_lsps: [f64; MAX_LSPS],
    last_pitch_val: i32,
    last_acb_type: u8,
    pitch_diff_sh16: i32,
    silence_gain: f32,

    aw_idx_is_ext: bool,
    aw_pulse_range: i32,
    aw_n_pulses: [i32; 2],
    aw_first_pulse_off: [i32; 2],
    aw_next_pulse_off_cache: i32,

    frame_cntr: i32,
    gain_pred_err: [f32; 6],
    excitation_history: [f32; MAX_SIGNAL_HISTORY],
    synth_history: [f32; MAX_LSPS],

    apf: Option<Box<Apf>>,
    postfilter_agc: f32,
    dcf_mem: [f32; 2],
    zero_exc_pf: [f32; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE],
    denoise_filter_cache: [f32; MAX_FRAMESIZE],
    denoise_filter_cache_size: usize,
    tilted_lpcs_pf: [f32; 0x82],
    denoise_coeffs_pf: [f32; 0x82],
    synth_filter_out_buf: [f32; 0x80 + MAX_LSPS_ALIGN16],
}

/// `pRNG`: a number in `[0, 1000 - block_size)` from the frame counter and
/// block index.
fn prng(frame_cntr: i32, block_num: usize, block_size: usize) -> usize {
    const DIV_TBL: [[u32; 2]; 9] = [
        [8332, 3 * 715827883],
        [4545, 0],
        [3124, 11 * 268435456],
        [2380, 15 * 204522253],
        [1922, 23 * 165191050],
        [1612, 23 * 138547333],
        [1388, 27 * 119304648],
        [1219, 16 * 104755300],
        [1086, 39 * 93368855],
    ];
    let mut x = (block_num as i32 * 1877).wrapping_add(frame_cntr) as u32;
    if x >= 0xFFFF {
        x -= 0xFFFF;
    }
    let mulh = ((477218589i64 * x as i32 as i64) >> 32) as u32;
    let y = x.wrapping_sub(9u32.wrapping_mul(mulh)) as usize;
    let Some(div) = DIV_TBL.get(y) else { return 0 };
    let umulh = ((x as u64 * div[1] as u64) >> 32) as u32;
    let z = x.wrapping_mul(div[0]).wrapping_add(umulh) as u16 as u32;
    (z % (1000 - block_size as u32)) as usize
}

/// `stabilize_lsps`.
fn stabilize_lsps(lsps: &mut [f64], num: usize) {
    use core::f64::consts::PI;
    lsps[0] = ffmax(lsps[0], 0.0015 * PI);
    for n in 1..num {
        lsps[n] = ffmax(lsps[n], lsps[n - 1] + 0.0125 * PI);
    }
    lsps[num - 1] = ffmin(lsps[num - 1], 0.9985 * PI);

    for n in 1..num {
        if lsps[n] < lsps[n - 1] {
            for m in 1..num {
                let tmp = lsps[m];
                let mut l = m;
                while l > 0 {
                    if lsps[l - 1] <= tmp {
                        break;
                    }
                    lsps[l] = lsps[l - 1];
                    l -= 1;
                }
                lsps[l] = tmp;
            }
            break;
        }
    }
}

/// `dequant_lsps`.
#[allow(clippy::too_many_arguments)]
fn dequant_lsps(
    lsps: &mut [f64],
    num: usize,
    values: &[u16],
    sizes: &[u16],
    n_stages: usize,
    table: &[u8],
    mul_q: &[f64],
    base_q: &[f64],
) {
    lsps[..num].fill(0.0);
    let mut table_off = 0;
    for n in 0..n_stages {
        let t_off = &table[table_off + values[n] as usize * num..];
        let (base, mul) = (base_q[n], mul_q[n]);
        for m in 0..num {
            lsps[m] += mul.mul_add(t_off[m] as f64, base);
        }
        table_off += sizes[n] as usize * num;
    }
}

/// `dequant_lsp10i`.
fn dequant_lsp10i(gb: &mut GetBits<'_>, lsps: &mut [f64]) {
    use core::f64::consts::PI;
    const VEC_SIZES: [u16; 4] = [256, 64, 32, 32];
    const MUL_LSF: [f64; 4] = [5.2187144800e-3, 1.4626986422e-3, 9.6179549166e-4, 1.1325736225e-3];
    const BASE_LSF: [f64; 4] = [PI * -2.15522e-1, PI * -6.1646e-2, PI * -3.3486e-2, PI * -5.7408e-2];
    let v = [gb.get_bits(8) as u16, gb.get_bits(6) as u16, gb.get_bits(5) as u16, gb.get_bits(5) as u16];
    dequant_lsps(lsps, 10, &v, &VEC_SIZES, 4, DQ_LSP10I, &MUL_LSF, &BASE_LSF);
}

/// `dequant_lsp10r`.
fn dequant_lsp10r(gb: &mut GetBits<'_>, i_lsps: &mut [f64], old: &[f64], a1: &mut [f64], a2: &mut [f64], q_mode: bool) {
    use core::f64::consts::PI;
    const VEC_SIZES: [u16; 3] = [128, 64, 64];
    const MUL_LSF: [f64; 3] = [2.5807601174e-3, 1.2354460219e-3, 1.1763821673e-3];
    const BASE_LSF: [f64; 3] = [PI * -1.07448e-1, PI * -5.2706e-2, PI * -5.1634e-2];
    let ipol_tab = if q_mode { LSP10_INTERCOEFF_B } else { LSP10_INTERCOEFF_A };

    dequant_lsp10i(gb, i_lsps);

    let interpol = gb.get_bits(5) as usize;
    let v = [gb.get_bits(7) as u16, gb.get_bits(6) as u16, gb.get_bits(6) as u16];

    for n in 0..10 {
        let delta = old[n] - i_lsps[n];
        a1[n] = (ipol_tab[interpol * 20 + n] as f64).mul_add(delta, i_lsps[n]);
        a1[10 + n] = (ipol_tab[interpol * 20 + 10 + n] as f64).mul_add(delta, i_lsps[n]);
    }

    dequant_lsps(a2, 20, &v, &VEC_SIZES, 3, DQ_LSP10R, &MUL_LSF, &BASE_LSF);
}

/// `dequant_lsp16i`.
fn dequant_lsp16i(gb: &mut GetBits<'_>, lsps: &mut [f64]) {
    use core::f64::consts::PI;
    const VEC_SIZES: [u16; 5] = [256, 64, 128, 64, 128];
    const MUL_LSF: [f64; 5] = [3.3439586280e-3, 6.9908173703e-4, 3.3216608306e-3, 1.0334960326e-3, 3.1899104283e-3];
    const BASE_LSF: [f64; 5] =
        [PI * -1.27576e-1, PI * -2.4292e-2, PI * -1.28094e-1, PI * -3.2128e-2, PI * -1.29816e-1];
    let v = [
        gb.get_bits(8) as u16,
        gb.get_bits(6) as u16,
        gb.get_bits(7) as u16,
        gb.get_bits(6) as u16,
        gb.get_bits(7) as u16,
    ];
    dequant_lsps(lsps, 5, &v, &VEC_SIZES, 2, DQ_LSP16I1, &MUL_LSF, &BASE_LSF);
    dequant_lsps(&mut lsps[5..], 5, &v[2..], &VEC_SIZES[2..], 2, DQ_LSP16I2, &MUL_LSF[2..], &BASE_LSF[2..]);
    dequant_lsps(&mut lsps[10..], 6, &v[4..], &VEC_SIZES[4..], 1, DQ_LSP16I3, &MUL_LSF[4..], &BASE_LSF[4..]);
}

/// `dequant_lsp16r`.
fn dequant_lsp16r(gb: &mut GetBits<'_>, i_lsps: &mut [f64], old: &[f64], a1: &mut [f64], a2: &mut [f64], q_mode: bool) {
    use core::f64::consts::PI;
    const VEC_SIZES: [u16; 3] = [128, 128, 128];
    const MUL_LSF: [f64; 3] = [1.2232979501e-3, 1.4062241527e-3, 1.6114744851e-3];
    const BASE_LSF: [f64; 3] = [PI * -5.5830e-2, PI * -5.2908e-2, PI * -5.4776e-2];
    let ipol_tab = if q_mode { LSP16_INTERCOEFF_B } else { LSP16_INTERCOEFF_A };

    dequant_lsp16i(gb, i_lsps);

    let interpol = gb.get_bits(5) as usize;
    let v = [gb.get_bits(7) as u16, gb.get_bits(7) as u16, gb.get_bits(7) as u16];

    for n in 0..16 {
        let delta = old[n] - i_lsps[n];
        a1[n] = (ipol_tab[interpol * 32 + n] as f64).mul_add(delta, i_lsps[n]);
        a1[16 + n] = (ipol_tab[interpol * 32 + 16 + n] as f64).mul_add(delta, i_lsps[n]);
    }

    dequant_lsps(a2, 10, &v, &VEC_SIZES, 1, DQ_LSP16R1, &MUL_LSF, &BASE_LSF);
    dequant_lsps(&mut a2[10..], 10, &v[1..], &VEC_SIZES[1..], 1, DQ_LSP16R2, &MUL_LSF[1..], &BASE_LSF[1..]);
    dequant_lsps(&mut a2[20..], 12, &v[2..], &VEC_SIZES[2..], 1, DQ_LSP16R3, &MUL_LSF[2..], &BASE_LSF[2..]);
}

/// `adaptive_gain_control`: energy as the sum of magnitudes.
fn adaptive_gain_control(out: &mut [f32], input: &[f32], speech_synth: &[f32], size: usize, alpha: f32, gain_mem: &mut f32) {
    let mut speech_energy = 0f32;
    let mut postfilter_energy = 0f32;
    for i in 0..size {
        speech_energy += speech_synth[i].abs();
        postfilter_energy += input[i].abs();
    }
    let gain_scale_factor = if postfilter_energy == 0.0 {
        0.0
    } else {
        ((1.0 - alpha as f64) * speech_energy as f64 / postfilter_energy as f64) as f32
    };

    let mut mem = *gain_mem;
    for i in 0..size {
        mem = alpha.mul_add(mem, gain_scale_factor);
        out[i] = input[i] * mem;
    }
    *gain_mem = mem;
}

/// `tilt_factor`.
fn tilt_factor(lpcs: &[f32], n_lpcs: usize) -> f32 {
    let rh0 = (1.0 + scalarproduct_float(lpcs, lpcs, n_lpcs) as f64) as f32;
    let rh1 = lpcs[0] + scalarproduct_float(lpcs, &lpcs[1..], n_lpcs - 1);
    rh1 / rh0
}

impl Synth {
    /// The synthesis half of `wmavoice_flush`.
    fn flush(&mut self) {
        self.postfilter_agc = 0.0;
        for n in 0..self.lsps {
            self.prev_lsps[n] = core::f64::consts::PI * (n as f64 + 1.0) / (self.lsps as f64 + 1.0);
        }
        self.excitation_history = [0.0; MAX_SIGNAL_HISTORY];
        self.synth_history = [0.0; MAX_LSPS];
        self.gain_pred_err = [0.0; 6];

        if self.do_apf {
            self.synth_filter_out_buf[MAX_LSPS_ALIGN16 - self.lsps..MAX_LSPS_ALIGN16].fill(0.0);
            self.dcf_mem = [0.0; 2];
            self.zero_exc_pf[..self.history_nsamples].fill(0.0);
            self.denoise_filter_cache = [0.0; MAX_FRAMESIZE];
        }
    }

    /// `kalman_smoothen`: smooth `zero_exc_pf[in_off..][..size]` with the
    /// best-matching history around `pitch` into `out`; false when no fit.
    fn kalman_smoothen(&self, pitch: i32, in_off: usize, out: &mut [f32], size: usize) -> bool {
        let buf = &self.zero_exc_pf;
        let input = &buf[in_off..];
        let mut optimal_gain = 0f32;
        let mut back = ffmax(self.min_pitch_val, pitch - 3) as usize;
        let end = ffmin(self.max_pitch_val, pitch + 3) as usize;
        let mut best = None;

        // find best fitting point in history
        loop {
            let dot = scalarproduct_float(input, &buf[in_off - back..], size);
            if dot > optimal_gain {
                optimal_gain = dot;
                best = Some(back);
            }
            back += 1;
            if back > end {
                break;
            }
        }

        let Some(best) = best else { return false };
        if optimal_gain <= 0.0 {
            return false;
        }
        let hist = &buf[in_off - best..];
        let mut dot = scalarproduct_float(hist, hist, size);
        if dot <= 0.0 {
            return false;
        }

        if optimal_gain <= dot {
            dot = (dot as f64 / 0.6f64.mul_add(optimal_gain as f64, dot as f64)) as f32;
        } else {
            dot = 0.625;
        }

        // actual smoothing
        for n in 0..size {
            out[n] = dot.mul_add(input[n] - hist[n], hist[n]);
        }
        true
    }

    /// `calc_input_response`: denoise filter coefficients (real domain)
    /// from `tilted_lpcs_pf` into `denoise_coeffs_pf`.
    fn calc_input_response(&mut self, fcb_type: u8, remainder: usize) {
        let Some(apf) = self.apf.as_mut() else { return };
        let mut max = -15.0f32;
        let mut min = 15.0f32;
        let mut coeffs = self.denoise_coeffs_pf;
        let mut lpcs = [0f32; 0x82];
        let mut lpcs_dct = [0f32; 0x82];

        // Create frequency power spectrum of speech input (i.e. RDFT of LPCs)
        apf.rdft.forward(&mut lpcs, &self.tilted_lpcs_pf);
        let mut log_range = |assign: f32| -> f32 {
            let tmp = assign.log10();
            max = ffmax(max, tmp);
            min = ffmin(min, tmp);
            tmp
        };
        let last_coeff = log_range(lpcs[64] * lpcs[64]);
        for n in 1..64 {
            lpcs[n] = log_range(lpcs[n * 2].mul_add(lpcs[n * 2], lpcs[n * 2 + 1] * lpcs[n * 2 + 1]));
        }
        lpcs[0] = log_range(lpcs[0] * lpcs[0]);
        let range = max - min;
        lpcs[64] = last_coeff;

        // Pick out the frequencies with higher (relative) power ("not
        // noise") and set up a table of gains per frequency.
        let irange = (64.0 / range as f64) as f32;
        let gain_mul =
            (range as f64 * if fcb_type == FCB_TYPE_HARDCODED { 5.0 / 13.0 } else { 5.0 / 14.7 }) as f32;
        let angle_mul = (gain_mul as f64 * (8.0 * core::f64::consts::LN_10 / core::f64::consts::PI)) as f32;
        let power = &DENOISE_POWER_TABLE[self.denoise_strength * 64..][..64];
        for n in 0..=64 {
            let idx = ((max - lpcs[n]).mul_add(irange, -1.0) as f64).round_ties_even() as i64 as i32;
            let idx = ffmax(0, idx).min(63) as usize;
            let pwr = power[idx];
            lpcs[n] = angle_mul * pwr;

            // 70.57 =~ 1/log10(1.0331663)
            let idx = ffmin(ffmax(((pwr * gain_mul) as f64 - 0.0295) * 70.570526123, 0.0), (i32::MAX / 2) as f64) as i32;
            coeffs[n] = if idx > 127 {
                ENERGY_TABLE[127] * 1.0331663f32.powf((idx - 127) as f32)
            } else {
                ENERGY_TABLE[ffmax(0, idx) as usize]
            };
        }

        // Hilbert transform of the gains (a phase shift).
        apf.dct.run(&mut lpcs_dct, &lpcs);
        apf.dst.run(&mut lpcs, &lpcs_dct);

        // Split out the coefficient indexes into phase/magnitude pairs
        let at = |v: f32| (255 + av_clip(v as i32, -255, 255)) as usize;
        coeffs[0] *= apf.cos[at(lpcs[64])];
        let last_coeff = coeffs[64] * apf.cos[at((-2.0f32).mul_add(lpcs[63], lpcs[64]))];
        let mut n = 63;
        loop {
            let idx = at((-2.0f32).mul_add(lpcs[n - 1], -lpcs[64]));
            coeffs[n * 2 + 1] = coeffs[n] * apf.sin[idx];
            coeffs[n * 2] = coeffs[n] * apf.cos[idx];

            n -= 1;
            if n == 0 {
                break;
            }

            let idx = at((-2.0f32).mul_add(lpcs[n - 1], lpcs[64]));
            coeffs[n * 2 + 1] = coeffs[n] * apf.sin[idx];
            coeffs[n * 2] = coeffs[n] * apf.cos[idx];
            n -= 1;
        }
        coeffs[64] = last_coeff;

        // move into real domain
        let coeffs_dst = &mut self.denoise_coeffs_pf;
        apf.irdft.inverse(coeffs_dst, &coeffs);

        // tilt correction and normalize scale
        coeffs_dst[remainder..128].fill(0.0);
        if self.denoise_tilt_corr {
            let mut tilt_mem = 0f32;
            coeffs_dst[remainder - 1] = 0.0;
            let tilt = (-1.8 * tilt_factor(&coeffs_dst[..], remainder - 1) as f64) as f32;
            tilt_compensation(&mut tilt_mem, tilt, &mut coeffs_dst[..], remainder);
        }
        let sq = ((1.0 / 64.0) * (1.0 / scalarproduct_float(&coeffs_dst[..], &coeffs_dst[..], remainder)).sqrt() as f64)
            as f32;
        for c in coeffs_dst[..remainder].iter_mut() {
            *c *= sq;
        }
    }

    /// `wiener_denoise` on `synth_filter_out_buf[MAX_LSPS_ALIGN16..]`.
    fn wiener_denoise(&mut self, fcb_type: u8, size: usize, lpcs: &[f32]) {
        const PF: usize = MAX_LSPS_ALIGN16;
        let mut remainder = 0;

        if fcb_type != FCB_TYPE_SILENCE {
            let lsps = self.lsps;
            let mut tilt_mem = 0f32;
            let tilted_lpcs = &mut self.tilted_lpcs_pf;
            tilted_lpcs[0] = 1.0;
            tilted_lpcs[1..=lsps].copy_from_slice(&lpcs[..lsps]);
            tilted_lpcs[lsps + 1..128].fill(0.0);
            let tilt = (0.7 * tilt_factor(lpcs, lsps) as f64) as f32;
            tilt_compensation(&mut tilt_mem, tilt, tilted_lpcs, lsps + 2);

            // The IRDFT output beyond the frame size is applied to the next
            // frame, and everything past min(size-1, 127-size) is ~zero.
            remainder = ffmin(127 - size, size - 1);
            self.calc_input_response(fcb_type, remainder);

            let Some(apf) = self.apf.as_ref() else { return };
            // apply coefficients (in frequency spectrum domain)
            let mut coeffs_f = [0f32; 0x82];
            let mut synth_f = [0f32; 0x82];
            self.synth_filter_out_buf[PF + size..PF + 128].fill(0.0);
            apf.rdft.forward(&mut synth_f, &self.synth_filter_out_buf[PF..]);
            apf.rdft.forward(&mut coeffs_f, &self.denoise_coeffs_pf);
            synth_f[0] *= coeffs_f[0];
            synth_f[1] *= coeffs_f[1];
            for n in 1..=64 {
                let (v1, v2) = (synth_f[n * 2], synth_f[n * 2 + 1]);
                synth_f[n * 2] = v1.mul_add(coeffs_f[n * 2], -(v2 * coeffs_f[n * 2 + 1]));
                synth_f[n * 2 + 1] = v2.mul_add(coeffs_f[n * 2], v1 * coeffs_f[n * 2 + 1]);
            }
            apf.irdft.inverse(&mut self.synth_filter_out_buf[PF..], &synth_f);
        }

        let synth_pf = &mut self.synth_filter_out_buf[PF..];
        // merge filter output with the history of previous runs
        if self.denoise_filter_cache_size != 0 {
            let lim = ffmin(self.denoise_filter_cache_size, size);
            for n in 0..lim {
                synth_pf[n] += self.denoise_filter_cache[n];
            }
            self.denoise_filter_cache_size -= lim;
            let keep = self.denoise_filter_cache_size;
            self.denoise_filter_cache.copy_within(size..size + keep, 0);
        }

        // move remainder of filter output into a cache for future runs
        if fcb_type != FCB_TYPE_SILENCE {
            let lim = ffmin(remainder, self.denoise_filter_cache_size);
            for n in 0..lim {
                self.denoise_filter_cache[n] += synth_pf[size + n];
            }
            if lim < remainder {
                self.denoise_filter_cache[lim..remainder].copy_from_slice(&synth_pf[size + lim..size + remainder]);
                self.denoise_filter_cache_size = remainder;
            }
        }
    }

    /// `postfilter`: the averaging projection filter on `synth[syn_off..]`
    /// (with its LPC history before it) into `samples[..size]`.
    #[allow(clippy::too_many_arguments)]
    fn postfilter(
        &mut self,
        synth: &[f32],
        syn_off: usize,
        samples: &mut [f32],
        size: usize,
        lpcs: &[f32],
        zero_exc_off: usize,
        fcb_type: u8,
        pitch: i32,
    ) {
        const PF: usize = MAX_LSPS_ALIGN16;
        let lsps = self.lsps;
        let mut synth_filter_in_buf = [0f32; MAX_FRAMESIZE / 2];

        // generate excitation from input signal
        celp_lp_zero_synthesis_filterf(&mut self.zero_exc_pf, zero_exc_off, lpcs, synth, syn_off, size, lsps);

        if !(fcb_type >= FCB_TYPE_AW_PULSES && self.kalman_smoothen(pitch, zero_exc_off, &mut synth_filter_in_buf, size))
        {
            synth_filter_in_buf[..size].copy_from_slice(&self.zero_exc_pf[zero_exc_off..zero_exc_off + size]);
        }

        // re-synthesize speech after smoothening, and keep history
        celp_lp_synthesis_filterf(&mut self.synth_filter_out_buf, PF, lpcs, &synth_filter_in_buf, size, lsps);
        self.synth_filter_out_buf.copy_within(PF + size - lsps..PF + size, PF - lsps);

        self.wiener_denoise(fcb_type, size, lpcs);

        adaptive_gain_control(
            samples,
            &self.synth_filter_out_buf[PF..],
            &synth[syn_off..],
            size,
            0.99,
            &mut self.postfilter_agc,
        );

        if self.dc_level > 8 {
            // remove ultra-low frequency DC noise / highpass filter
            acelp_apply_order_2_transfer_function(
                samples,
                [-1.99997, 1.0],
                [-1.9330735188, 0.93589198496],
                0.93980580475,
                &mut self.dcf_mem,
                size,
            );
        }
    }

    /// `aw_parse_coords`.
    fn aw_parse_coords(&mut self, gb: &mut GetBits<'_>, pitch: &[i32; MAX_BLOCKS]) {
        const START_OFFSET: [i16; 94] = [
            -11, -9, -7, -5, -3, -1, 1, 3, 5, 7, 9, 11, 13, 15, 18, 17, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29,
            30, 31, 32, 33, 35, 37, 39, 41, 43, 45, 47, 49, 51, 53, 55, 57, 59, 61, 63, 65, 67, 69, 71, 73, 75, 77,
            79, 81, 83, 85, 87, 89, 91, 93, 95, 97, 99, 101, 103, 105, 107, 109, 111, 113, 115, 117, 119, 121, 123,
            125, 127, 129, 131, 133, 135, 137, 139, 141, 143, 145, 147, 149, 151, 153, 155, 157, 159,
        ];
        const HALF: i32 = MAX_FRAMESIZE as i32 / 2;

        // position of pulse
        self.aw_idx_is_ext = false;
        let mut bits = gb.get_bits(6) as usize;
        if bits >= 54 {
            self.aw_idx_is_ext = true;
            bits += (bits - 54) * 3 + gb.get_bits(2) as usize;
        }
        let start = START_OFFSET[bits] as i32;

        // for a repeated pulse at pulse_off with a pitch_lag of pitch[],
        // count the distribution of the pulses in each block
        self.aw_pulse_range = if ffmin(pitch[0], pitch[1]) > 32 { 24 } else { 16 };
        let mut offset = start;
        while offset < 0 {
            offset += pitch[0];
        }
        self.aw_n_pulses[0] = (pitch[0] - 1 + HALF - offset) / pitch[0];
        self.aw_first_pulse_off[0] = offset - self.aw_pulse_range / 2;
        offset += self.aw_n_pulses[0] * pitch[0];
        self.aw_n_pulses[1] = (pitch[1] - 1 + MAX_FRAMESIZE as i32 - offset) / pitch[1];
        self.aw_first_pulse_off[1] = offset - (MAX_FRAMESIZE as i32 + self.aw_pulse_range) / 2;

        // if continuing from a position before the block, reset position to
        // start of block
        if start < HALF {
            while self.aw_first_pulse_off[1] - pitch[1] + self.aw_pulse_range > 0 {
                self.aw_first_pulse_off[1] -= pitch[1];
            }
            if start < 0 {
                while self.aw_first_pulse_off[0] - pitch[0] + self.aw_pulse_range > 0 {
                    self.aw_first_pulse_off[0] -= pitch[0];
                }
            }
        }
    }

    /// `aw_pulse_set2`; false when no pulse position is left.
    fn aw_pulse_set2(&mut self, gb: &mut GetBits<'_>, block_idx: usize, fcb: &mut AmrFixed) -> bool {
        const HALF: i32 = MAX_FRAMESIZE as i32 / 2;
        // use_mask[i] is use_mask_mem[i + 2]: 5 used words, 2 of padding
        // on each side.
        let mut use_mask_mem = [0u16; 9];
        let mut pulse_off = self.aw_first_pulse_off[block_idx];
        let mut start_off = 0;

        // set offset of first pulse to within this block
        if self.aw_n_pulses[block_idx] > 0 {
            while pulse_off + self.aw_pulse_range < 1 {
                pulse_off += fcb.pitch_lag;
            }
        }

        // find range per pulse
        let range = if self.aw_n_pulses[0] > 0 {
            if block_idx == 0 {
                32
            } else {
                if self.aw_n_pulses[block_idx] > 0 {
                    pulse_off = self.aw_next_pulse_off_cache;
                }
                8
            }
        } else {
            16
        };
        let mut pulse_start = if self.aw_n_pulses[block_idx] > 0 { pulse_off - range / 2 } else { 0 };

        // aw_pulse_set1() already applies pulses around pulse_off, so that
        // range is excluded here
        use_mask_mem[2..7].fill(0xFFFF);
        if self.aw_n_pulses[block_idx] > 0 {
            let mut idx = pulse_off;
            while idx < HALF {
                let mut excl_range = self.aw_pulse_range;
                let mut p = (2 + (idx >> 4)) as usize;
                let first_sh = 16 - (idx & 15);
                use_mask_mem[p] &= (0xFFFFu32 << first_sh) as u16;
                p += 1;
                excl_range -= first_sh;
                if excl_range >= 16 {
                    use_mask_mem[p] = 0;
                    p += 1;
                    use_mask_mem[p] &= (0xFFFFu32 >> (excl_range - 16)) as u16;
                } else {
                    use_mask_mem[p] &= (0xFFFFu32 >> excl_range) as u16;
                }
                idx += fcb.pitch_lag;
            }
        }

        // find the 'aidx'th offset that is not excluded
        let aidx = gb.get_bits(if self.aw_n_pulses[0] > 0 { 5 - 2 * block_idx as u32 } else { 4 }) as i32;
        let mut n = 0;
        while n <= aidx {
            let mut idx = pulse_start;
            while idx < 0 {
                idx += fcb.pitch_lag;
            }
            if idx >= HALF {
                // find from zero
                let Some(word) = (0..5).find(|&w| use_mask_mem[2 + w] != 0) else { return false };
                let v = use_mask_mem[2 + word];
                idx = (word as i32 * 16 + 15) - (15 - v.leading_zeros() as i32);
            }
            let w = 2 + (idx >> 4) as usize;
            let bit = 0x8000u16 >> (idx & 15);
            if use_mask_mem[w] & bit != 0 {
                use_mask_mem[w] &= !bit;
                n += 1;
                start_off = idx;
            }
            pulse_start += 1;
        }

        fcb.x[fcb.n] = start_off;
        fcb.y[fcb.n] = if gb.get_bits1() != 0 { -1.0 } else { 1.0 };
        fcb.n += 1;

        // set offset for next block, relative to start of that block
        let n = (HALF - start_off) % fcb.pitch_lag;
        self.aw_next_pulse_off_cache = if n != 0 { fcb.pitch_lag - n } else { 0 };
        true
    }

    /// `aw_pulse_set1`.
    fn aw_pulse_set1(&mut self, gb: &mut GetBits<'_>, block_idx: usize, fcb: &mut AmrFixed) {
        let mut val = gb.get_bits(12 - 2 * (self.aw_idx_is_ext && block_idx == 0) as u32) as i32;

        if self.aw_n_pulses[block_idx] > 0 {
            let (n_pulses, v_mask, i_mask, sh) = if self.aw_pulse_range == 24 {
                // 3 pulses, 1:sign + 3:index each
                (3, 8, 7, 4)
            } else {
                // 4 pulses, 1:sign + 2:index each
                (4, 4, 3, 3)
            };
            for n in (0..n_pulses).rev() {
                fcb.y[fcb.n] = if val & v_mask != 0 { -1.0 } else { 1.0 };
                let mut x = (val & i_mask) * n_pulses + n + self.aw_first_pulse_off[block_idx];
                while x < 0 {
                    x += fcb.pitch_lag;
                }
                fcb.x[fcb.n] = x;
                if x < MAX_FRAMESIZE as i32 / 2 {
                    fcb.n += 1;
                }
                val >>= sh;
            }
        } else {
            let num2 = (val & 0x1FF) >> 1;
            let (delta, idx) = if num2 < 79 {
                (1, num2 + 1)
            } else if num2 < 2 * 78 {
                (3, num2 + 1 - 77)
            } else if num2 < 3 * 77 {
                (5, num2 + 1 - 2 * 76)
            } else {
                (7, num2 + 1 - 3 * 75)
            };
            let v = if val & 0x200 != 0 { -1.0 } else { 1.0 };

            fcb.no_repeat_mask |= 3 << fcb.n;
            fcb.x[fcb.n] = idx - delta;
            fcb.y[fcb.n] = v;
            fcb.x[fcb.n + 1] = idx;
            fcb.y[fcb.n + 1] = if val & 1 != 0 { -v } else { v };
            fcb.n += 2;
        }
    }

    /// `synth_block_hardcoded`.
    fn synth_block_hardcoded(
        &mut self,
        gb: &mut GetBits<'_>,
        block_idx: usize,
        size: usize,
        desc: &FrameTypeDesc,
        excitation: &mut [f32],
    ) {
        let (r_idx, gain) = if desc.fcb_type == FCB_TYPE_SILENCE {
            (prng(self.frame_cntr, block_idx, size), self.silence_gain)
        } else {
            let r_idx = gb.get_bits(8) as usize;
            (r_idx, GAIN_UNIVERSAL[gb.get_bits(6) as usize])
        };

        // Clear gain prediction parameters
        self.gain_pred_err = [0.0; 6];

        // Apply gain to hardcoded codebook and use that as excitation signal
        for (e, c) in excitation[..size].iter_mut().zip(&STD_CODEBOOK[r_idx..]) {
            *e = c * gain;
        }
    }

    /// `synth_block_fcb_acb` on `excitation[off..][..size]`, whose history
    /// lies before `off`.
    #[allow(clippy::too_many_arguments)]
    fn synth_block_fcb_acb(
        &mut self,
        gb: &mut GetBits<'_>,
        block_idx: usize,
        size: usize,
        block_pitch_sh2: i32,
        desc: &FrameTypeDesc,
        excitation: &mut [f32],
        off: usize,
    ) -> core::result::Result<(), SfError> {
        const GAIN_COEFF: [f32; 6] = [0.8169, -0.06545, 0.1726, 0.0185, -0.0359, 0.0458];
        let mut pulses = [0f32; MAX_FRAMESIZE / 2];
        let mut fcb = AmrFixed { pitch_lag: block_pitch_sh2 >> 2, pitch_fac: 1.0, ..AmrFixed::default() };
        if fcb.pitch_lag < 1 {
            return Err(SfError::Invalid);
        }

        // For the other frame types, this is where we apply the innovation
        // (fixed) codebook pulses of the speech signal.
        if desc.fcb_type == FCB_TYPE_AW_PULSES {
            self.aw_pulse_set1(gb, block_idx, &mut fcb);
            if !self.aw_pulse_set2(gb, block_idx, &mut fcb) {
                // Conceal the block with silence and return, skipping the
                // bits of this block.
                let r_idx = prng(self.frame_cntr, block_idx, size);
                for (e, c) in excitation[off..off + size].iter_mut().zip(&STD_CODEBOOK[r_idx..]) {
                    *e = c * self.silence_gain;
                }
                gb.skip_bits(7 + 1);
                return Ok(());
            }
        } else {
            let offset_nbits = 5 - desc.log_n_blocks as u32;
            fcb.no_repeat_mask = -1;
            // similar to ff_decode_10_pulses_35bits(), but with single
            // pulses (instead of double) for a subset of pulses
            for n in 0..5 {
                let sign = if gb.get_bits1() != 0 { 1.0 } else { -1.0 };
                let pos1 = gb.get_bits(offset_nbits) as i32;
                fcb.x[fcb.n] = n + 5 * pos1;
                fcb.y[fcb.n] = sign;
                fcb.n += 1;
                if n < desc.dbl_pulses as i32 {
                    let pos2 = gb.get_bits(offset_nbits) as i32;
                    fcb.x[fcb.n] = n + 5 * pos2;
                    fcb.y[fcb.n] = if pos1 < pos2 { -sign } else { sign };
                    fcb.n += 1;
                }
            }
        }
        set_fixed_vector(&mut pulses, &fcb, 1.0, size);

        // Calculate gain for adaptive & fixed codebook signal.
        let idx = gb.get_bits(7) as usize;
        let fcb_gain = ((scalarproduct_float(&self.gain_pred_err, &GAIN_COEFF, 6) as f64 - 5.2409161640
            + GAIN_CODEBOOK_FCB[idx] as f64) as f32)
            .exp();
        let acb_gain = GAIN_CODEBOOK_ACB[idx];
        let pred_err = ffmin(ffmax(GAIN_CODEBOOK_FCB[idx], -2.9957322736f32), 1.6094379124f32);

        let gain_weight = 8 >> desc.log_n_blocks;
        self.gain_pred_err.copy_within(..6 - gain_weight, gain_weight);
        self.gain_pred_err[..gain_weight].fill(pred_err);

        // Calculation of adaptive codebook
        if desc.acb_type == ACB_TYPE_ASYMMETRIC {
            let mut n = 0;
            while n < size {
                let abs_idx = (block_idx * size + n) as i32;
                let pitch_sh16 = (self.last_pitch_val << 16).wrapping_add(self.pitch_diff_sh16.wrapping_mul(abs_idx));
                let pitch = (pitch_sh16 + 0x6FFF) >> 16;
                let idx_sh16 = ((pitch << 16) - pitch_sh16) * 8 + 0x58000;
                let idx = idx_sh16 >> 16;
                let len = if self.pitch_diff_sh16 != 0 {
                    let next_idx_sh16 = if self.pitch_diff_sh16 > 0 {
                        idx_sh16 & !0xFFFF
                    } else {
                        (idx_sh16 + 0x10000) & !0xFFFF
                    };
                    av_clip((idx_sh16 - next_idx_sh16) / self.pitch_diff_sh16 / 8, 1, (size - n) as i32) as usize
                } else {
                    size
                };

                let src = (off + n).checked_sub(pitch as usize).filter(|&s| pitch >= 1 && s >= 9);
                let Some(src) = src.filter(|_| (1..=8).contains(&idx)) else { return Err(SfError::Invalid) };
                acelp_interpolatef(excitation, off + n, src, IPOL1_COEFFS, 17, idx as usize, 9, len);
                n += len;
            }
        } else {
            let block_pitch = (block_pitch_sh2 >> 2) as usize;
            let idx = (block_pitch_sh2 & 3) as usize;
            let Some(src) = off.checked_sub(block_pitch).filter(|&s| s >= 8) else { return Err(SfError::Invalid) };
            if idx != 0 {
                acelp_interpolatef(excitation, off, src, IPOL2_COEFFS, 4, idx, 8, size);
            } else {
                for i in 0..size {
                    excitation[off + i] = excitation[src + i];
                }
            }
        }

        // Interpolate ACB/FCB and use as excitation signal
        weighted_vector_sumf_inplace(&mut excitation[off..], &pulses, acb_gain, fcb_gain, size);
        Ok(())
    }

    /// `synth_block`: one block of excitation at `excitation[exc_off..]`
    /// and speech at `synth[syn_off..]`.
    #[allow(clippy::too_many_arguments)]
    fn synth_block(
        &mut self,
        gb: &mut GetBits<'_>,
        block_idx: usize,
        size: usize,
        block_pitch_sh2: i32,
        lsps: &[f64; MAX_LSPS],
        prev_lsps: &[f64; MAX_LSPS],
        desc: &FrameTypeDesc,
        excitation: &mut [f32],
        exc_off: usize,
        synth: &mut [f32],
        syn_off: usize,
    ) -> core::result::Result<(), SfError> {
        if desc.acb_type == ACB_TYPE_NONE {
            self.synth_block_hardcoded(gb, block_idx, size, desc, &mut excitation[exc_off..]);
        } else {
            self.synth_block_fcb_acb(gb, block_idx, size, block_pitch_sh2, desc, excitation, exc_off)?;
        }

        // convert interpolated LSPs to LPCs
        let fac = ((block_idx as f64 + 0.5) / desc.n_blocks as f64) as f32;
        let mut i_lsps = [0f64; MAX_LSPS];
        for n in 0..self.lsps {
            i_lsps[n] = (fac as f64).mul_add(lsps[n] - prev_lsps[n], prev_lsps[n]).cos();
        }
        let mut lpcs = [0f32; MAX_LSPS];
        acelp_lspd2lpc(&i_lsps, &mut lpcs, self.lsps >> 1);

        // Speech synthesis
        celp_lp_synthesis_filterf(synth, syn_off, &lpcs, &excitation[exc_off..], size, self.lsps);
        Ok(())
    }

    /// `synth_frame`: frame `frame_idx` of the superframe into
    /// `samples[..160]`.
    #[allow(clippy::too_many_arguments)]
    fn synth_frame(
        &mut self,
        gb: &mut GetBits<'_>,
        frame_idx: usize,
        samples: &mut [f32],
        lsps: &[f64; MAX_LSPS],
        prev_lsps: &[f64; MAX_LSPS],
        excitation: &mut [f32],
        exc_off: usize,
        synth: &mut [f32],
        syn_off: usize,
    ) -> core::result::Result<(), SfError> {
        let mut pitch = [0i32; MAX_BLOCKS];
        let mut cur_pitch_val = 0;
        let mut last_block_pitch = 0;

        // Parse frame type ("frame header"), see FRAME_DESCS
        let sym = gb.get_vlc(&self.frame_type_vlc);
        let bd_idx = usize::try_from(sym).ok().and_then(|s| self.vbm_tree.get(s)).copied().unwrap_or(-1);

        pitch[0] = i32::MAX;

        if bd_idx < 0 {
            return Err(SfError::Invalid);
        }
        let desc = FRAME_DESCS[bd_idx as usize];
        let n_blocks = desc.n_blocks as usize;
        let block_nsamples = MAX_FRAMESIZE / n_blocks;

        // Pitch calculation for ACB_TYPE_ASYMMETRIC ("pitch-per-frame")
        if desc.acb_type == ACB_TYPE_ASYMMETRIC {
            let n_blocks_x2 = (desc.n_blocks as i32) << 1;
            let log_n_blocks_x2 = desc.log_n_blocks as i32 + 1;
            cur_pitch_val = self.min_pitch_val + gb.get_bits(self.pitch_nbits) as i32;
            cur_pitch_val = ffmin(cur_pitch_val, self.max_pitch_val - 1);
            if self.last_acb_type == ACB_TYPE_NONE
                || 20 * (cur_pitch_val - self.last_pitch_val).abs() > cur_pitch_val + self.last_pitch_val
            {
                self.last_pitch_val = cur_pitch_val;
            }

            // pitch per block
            for (n, p) in pitch.iter_mut().take(n_blocks).enumerate() {
                let fac = n as i32 * 2 + 1;
                *p = (fac * cur_pitch_val + (n_blocks_x2 - fac) * self.last_pitch_val + desc.n_blocks as i32)
                    >> log_n_blocks_x2;
            }

            // "pitch-diff-per-sample" for calculation of pitch per sample
            self.pitch_diff_sh16 = (cur_pitch_val - self.last_pitch_val) * (1 << 16) / MAX_FRAMESIZE as i32;
        }

        // Global gain (if silence) and pitch-adaptive window coordinates
        match desc.fcb_type {
            FCB_TYPE_SILENCE => self.silence_gain = GAIN_SILENCE[gb.get_bits(8) as usize],
            FCB_TYPE_AW_PULSES => self.aw_parse_coords(gb, &pitch),
            _ => {}
        }

        for n in 0..n_blocks {
            let bl_pitch_sh2 = match desc.acb_type {
                ACB_TYPE_HAMMING => {
                    // Pitch per block: absolute for the first block, then
                    // deltas, on a semi-logarithmic scale.
                    let conv = &self.block_conv_table;
                    let t1 = (conv[1] - conv[0]) << 2;
                    let t2 = (conv[2] - conv[1]) << 1;
                    let t3 = conv[3] - conv[2] + 1;

                    let mut block_pitch = if n == 0 {
                        gb.get_bits(self.block_pitch_nbits) as i32
                    } else {
                        last_block_pitch - self.block_delta_pitch_hrange
                            + gb.get_bits(self.block_delta_pitch_nbits) as i32
                    };
                    // Convert last_ so that any next delta is within _range
                    last_block_pitch = av_clip(
                        block_pitch,
                        self.block_delta_pitch_hrange,
                        self.block_pitch_range - self.block_delta_pitch_hrange,
                    );

                    // Convert semi-log-style scale back to normal scale
                    let bl = if block_pitch < t1 {
                        (conv[0] << 2) + block_pitch
                    } else {
                        block_pitch -= t1;
                        if block_pitch < t2 {
                            (conv[1] << 2) + (block_pitch << 1)
                        } else {
                            block_pitch -= t2;
                            if block_pitch < t3 {
                                (conv[2] + block_pitch) << 2
                            } else {
                                conv[3] << 2
                            }
                        }
                    };
                    pitch[n] = bl >> 2;
                    bl
                }
                ACB_TYPE_ASYMMETRIC => pitch[n] << 2,
                _ => 0,
            };

            self.synth_block(
                gb,
                n,
                block_nsamples,
                bl_pitch_sh2,
                lsps,
                prev_lsps,
                &desc,
                excitation,
                exc_off + n * block_nsamples,
                synth,
                syn_off + n * block_nsamples,
            )?;
        }

        // Averaging projection filter, if applicable. Else, just copy
        // samples from synthesis buffer
        if self.do_apf {
            let lsps_n = self.lsps;
            let mut i_lsps = [0f64; MAX_LSPS];
            let mut lpcs = [0f32; MAX_LSPS];

            if desc.fcb_type >= FCB_TYPE_AW_PULSES && pitch[0] == i32::MAX {
                return Err(SfError::Invalid);
            }

            for n in 0..lsps_n {
                i_lsps[n] = (0.5 * (prev_lsps[n] + lsps[n])).cos();
            }
            acelp_lspd2lpc(&i_lsps, &mut lpcs, lsps_n >> 1);
            let zero_exc = self.history_nsamples + MAX_FRAMESIZE * frame_idx;
            self.postfilter(synth, syn_off, &mut samples[..80], 80, &lpcs, zero_exc, desc.fcb_type, pitch[0]);

            for n in 0..lsps_n {
                i_lsps[n] = lsps[n].cos();
            }
            acelp_lspd2lpc(&i_lsps, &mut lpcs, lsps_n >> 1);
            self.postfilter(synth, syn_off + 80, &mut samples[80..160], 80, &lpcs, zero_exc + 80, desc.fcb_type, pitch[0]);
        } else {
            samples[..160].copy_from_slice(&synth[syn_off..syn_off + 160]);
        }

        // Cache values for next frame
        self.frame_cntr += 1;
        if self.frame_cntr >= 0xFFFF {
            self.frame_cntr -= 0xFFFF;
        }
        self.last_acb_type = desc.acb_type;
        match desc.acb_type {
            ACB_TYPE_NONE => self.last_pitch_val = 0,
            ACB_TYPE_ASYMMETRIC => self.last_pitch_val = cur_pitch_val,
            _ => self.last_pitch_val = pitch[n_blocks - 1],
        }

        Ok(())
    }

    /// `synth_superframe`: 3 frames (480 samples) into `samples`; returns
    /// the number of samples the superframe codes.
    fn superframe(
        &mut self,
        gb: &mut GetBits<'_>,
        samples: &mut [f32; MAX_SFRAMESIZE],
    ) -> core::result::Result<usize, SfError> {
        let lsps_n = self.lsps;
        let hist = self.history_nsamples;
        let mut n_samples = MAX_SFRAMESIZE;
        let mut lsps = [[0f64; MAX_LSPS]; MAX_FRAMES];
        let mean_lsf: &[f64] = if lsps_n == 16 {
            &MEAN_LSF16[16 * self.lsp_def_mode as usize..][..16]
        } else {
            &MEAN_LSF10[10 * self.lsp_def_mode as usize..][..10]
        };
        let mut excitation = [0f32; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE + 12];
        let mut synth = [0f32; MAX_LSPS + MAX_SFRAMESIZE];

        synth[..lsps_n].copy_from_slice(&self.synth_history[..lsps_n]);
        excitation[..hist].copy_from_slice(&self.excitation_history[..hist]);

        // The first bit tells speech from music (WMAPro-in-WMAVoice), which
        // FFmpeg does not decode.
        if gb.get_bits1() == 0 {
            return Err(SfError::Invalid);
        }

        // (optional) nr. of samples in superframe; always <= 480 and >= 0
        if gb.get_bits1() != 0 {
            n_samples = gb.get_bits(12) as usize;
            if n_samples > MAX_SFRAMESIZE {
                return Err(SfError::Invalid);
            }
        }

        // Parse LSPs, if global for the superframe (can also be per-frame).
        if self.has_residual_lsps {
            let mut prev_lsps = [0f64; MAX_LSPS];
            let mut a1 = [0f64; MAX_LSPS * 2];
            let mut a2 = [0f64; MAX_LSPS * 2];

            for n in 0..lsps_n {
                prev_lsps[n] = self.prev_lsps[n] - mean_lsf[n];
            }

            if lsps_n == 10 {
                dequant_lsp10r(gb, &mut lsps[2], &prev_lsps, &mut a1, &mut a2, self.lsp_q_mode);
            } else {
                dequant_lsp16r(gb, &mut lsps[2], &prev_lsps, &mut a1, &mut a2, self.lsp_q_mode);
            }

            for n in 0..lsps_n {
                lsps[0][n] = mean_lsf[n] + (a1[n] - a2[n * 2]);
                lsps[1][n] = mean_lsf[n] + (a1[lsps_n + n] - a2[n * 2 + 1]);
                lsps[2][n] += mean_lsf[n];
            }
            for l in lsps.iter_mut() {
                stabilize_lsps(l, lsps_n);
            }
        }

        // Parse frames, optionally preceded by per-frame (independent) LSPs.
        for n in 0..MAX_FRAMES {
            if !self.has_residual_lsps {
                if lsps_n == 10 {
                    dequant_lsp10i(gb, &mut lsps[n]);
                } else {
                    dequant_lsp16i(gb, &mut lsps[n]);
                }
                for m in 0..lsps_n {
                    lsps[n][m] += mean_lsf[m];
                }
                stabilize_lsps(&mut lsps[n], lsps_n);
            }

            let prev = if n == 0 { self.prev_lsps } else { lsps[n - 1] };
            let cur = lsps[n];
            self.synth_frame(
                gb,
                n,
                &mut samples[n * MAX_FRAMESIZE..(n + 1) * MAX_FRAMESIZE],
                &cur,
                &prev,
                &mut excitation,
                hist + n * MAX_FRAMESIZE,
                &mut synth,
                lsps_n + n * MAX_FRAMESIZE,
            )?;
        }

        // Statistics? FFmpeg skips them unchecked; an overrun is caught
        // below.
        if gb.get_bits1() != 0 {
            let res = gb.get_bits(4);
            gb.skip_bits(10 * (res + 1));
        }

        if gb.bits_left() < 0 {
            self.flush();
            return Err(SfError::Flushed);
        }

        // Update history
        self.prev_lsps[..lsps_n].copy_from_slice(&lsps[2][..lsps_n]);
        self.synth_history[..lsps_n].copy_from_slice(&synth[MAX_SFRAMESIZE..MAX_SFRAMESIZE + lsps_n]);
        self.excitation_history[..hist].copy_from_slice(&excitation[MAX_SFRAMESIZE..MAX_SFRAMESIZE + hist]);
        if self.do_apf {
            self.zero_exc_pf.copy_within(MAX_SFRAMESIZE..MAX_SFRAMESIZE + hist, 0);
        }

        Ok(n_samples)
    }
}

/// `decode_vbmtree`: the variable bit mode tree from extradata.
fn decode_vbmtree(gb: &mut GetBits<'_>) -> Option<[i8; 25]> {
    let mut cntr = [0usize; 8];
    let mut vbm_tree = [-1i8; 25];
    for n in 0..17 {
        let res = gb.get_bits(3) as usize;
        if cntr[res] > 3 {
            return None;
        }
        // res * 3 + cntr[res] is at most 7 * 3 + 3 = 24
        vbm_tree[res * 3 + cntr[res]] = n as i8;
        cntr[res] += 1;
    }
    Some(vbm_tree)
}

/// Copy `nbits` (unaligned) bits from `gb` over `data[..size]` into the
/// superframe cache (wmavoice.c `copy_bits`).
fn copy_bits(pb: &mut PutBits, cache: &mut [u8], data: &[u8], size: usize, gb: &mut GetBits<'_>, nbits: i64) {
    let rmn = gb.bits_left();
    if rmn < nbits || nbits < 0 {
        return;
    }
    if nbits > (SFRAME_CACHE_MAXSIZE * 8) as i64 - pb.count() as i64 {
        return;
    }
    let rmn_bytes = (rmn >> 3) as usize;
    let rmn_bits = ffmin(rmn & 7, nbits);
    if rmn_bits > 0 {
        let v = gb.get_bits(rmn_bits as u32);
        pb.put_bits(cache, rmn_bits as u32, v);
    }
    let len = ffmin(nbits - rmn_bits, (rmn_bytes << 3) as i64) as usize;
    pb.copy_bits(cache, &data[size - rmn_bytes..size], len);
}

/// WMA Voice decoder (FFmpeg's `wmavoice`).
pub struct WmaVoiceDecoder {
    codec_id: CodecId,
    sample_rate: u32,
    block_align: usize,
    spillover_bitsize: u32,
    spillover_nbits: i64,
    nb_superframes: i32,
    skip_bits_next: u32,
    sframe_cache: [u8; SFRAME_CACHE_MAXSIZE + AV_INPUT_BUFFER_PADDING_SIZE],
    sframe_cache_size: usize,
    pb: PutBits,
    syn: Synth,
    out: [f32; MAX_SFRAMESIZE],
    pending: VecDeque<AudioFrame>,
    eof: bool,
}

impl WmaVoiceDecoder {
    /// `wmavoice_decode_init`.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let extradata = &params.extradata;
        if extradata.len() != 46 {
            return Err(Error::invalid(format!(
                "wmavoice: invalid extradata size {} (should be 46)",
                extradata.len()
            )));
        }
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&b| b > 0 && b <= 1 << 22)
            .ok_or_else(|| Error::invalid("wmavoice: invalid block alignment"))? as usize;

        let flags = u32::from_le_bytes([extradata[18], extradata[19], extradata[20], extradata[21]]);
        let spillover_bitsize = 3 + av_ceil_log2(block_align as u32);
        let do_apf = flags & 0x1 != 0;
        let denoise_strength = ((flags >> 2) & 0xF) as usize;
        if denoise_strength >= 12 {
            return Err(Error::invalid(format!(
                "wmavoice: invalid denoise filter strength {denoise_strength} (max=11)"
            )));
        }
        let denoise_tilt_corr = flags & 0x40 != 0;
        let dc_level = (flags >> 7) & 0xF;
        let lsp_q_mode = flags & 0x2000 != 0;
        let lsp_def_mode = flags & 0x4000 != 0;
        let lsps = if flags & 0x1000 != 0 { 16 } else { 10 };
        let mut prev_lsps = [0f64; MAX_LSPS];
        for (n, v) in prev_lsps.iter_mut().take(lsps).enumerate() {
            *v = core::f64::consts::PI * (n as f64 + 1.0) / (lsps as f64 + 1.0);
        }

        let mut gb = GetBits::new(&extradata[22..], (extradata.len() - 22) * 8);
        let vbm_tree =
            decode_vbmtree(&mut gb).ok_or_else(|| Error::invalid("wmavoice: invalid VBM tree; broken extradata?"))?;

        let sample_rate = params.sample_rate.unwrap_or(0);
        if sample_rate as u64 >= (i32::MAX / (256 * 37)) as u64 {
            return Err(Error::invalid("wmavoice: invalid sample rate"));
        }
        let sr = sample_rate as i32;
        let min_pitch_val = ((sr << 8) / 400 + 50) >> 8;
        let max_pitch_val = ((sr << 8) * 37 / 2000 + 50) >> 8;
        let pitch_range = max_pitch_val - min_pitch_val;
        if pitch_range <= 0 {
            return Err(Error::invalid("wmavoice: invalid pitch range; broken extradata?"));
        }
        let pitch_nbits = av_ceil_log2(pitch_range as u32);
        let history_nsamples = (max_pitch_val + 8) as usize;
        if min_pitch_val < 1 || history_nsamples > MAX_SIGNAL_HISTORY {
            return Err(Error::unsupported(format!("wmavoice: unsupported sample rate {sample_rate}")));
        }

        let block_conv_table =
            [min_pitch_val, (pitch_range * 25) >> 6, (pitch_range * 44) >> 6, max_pitch_val - 1];
        let block_delta_pitch_hrange = (pitch_range >> 3) & !0xF;
        if block_delta_pitch_hrange <= 0 {
            return Err(Error::invalid("wmavoice: invalid delta pitch hrange; broken extradata?"));
        }
        let block_delta_pitch_nbits = 1 + av_ceil_log2(block_delta_pitch_hrange as u32);
        let block_pitch_range = block_conv_table[2]
            + block_conv_table[3]
            + 1
            + 2 * (block_conv_table[1] - 2 * min_pitch_val);
        let block_pitch_nbits = av_ceil_log2(block_pitch_range.max(0) as u32);

        const FRAME_TYPE_BITS: [i8; 22] = [2, 2, 2, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 14];
        let frame_type_vlc = VlcTable::from_lengths(&FRAME_TYPE_BITS, None, 0)?;

        let syn = Synth {
            vbm_tree,
            frame_type_vlc,
            history_nsamples,
            do_apf,
            denoise_strength,
            denoise_tilt_corr,
            dc_level,
            lsps,
            lsp_q_mode,
            lsp_def_mode,
            min_pitch_val,
            max_pitch_val,
            pitch_nbits,
            block_pitch_nbits,
            block_pitch_range,
            block_delta_pitch_nbits,
            block_delta_pitch_hrange,
            block_conv_table,
            has_residual_lsps: false,
            prev_lsps,
            last_pitch_val: 40,
            last_acb_type: ACB_TYPE_NONE,
            pitch_diff_sh16: 0,
            silence_gain: 0.0,
            aw_idx_is_ext: false,
            aw_pulse_range: 0,
            aw_n_pulses: [0; 2],
            aw_first_pulse_off: [0; 2],
            aw_next_pulse_off_cache: 0,
            frame_cntr: 0,
            gain_pred_err: [0.0; 6],
            excitation_history: [0.0; MAX_SIGNAL_HISTORY],
            synth_history: [0.0; MAX_LSPS],
            apf: do_apf.then(|| Box::new(Apf::new())),
            postfilter_agc: 0.0,
            dcf_mem: [0.0; 2],
            zero_exc_pf: [0.0; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE],
            denoise_filter_cache: [0.0; MAX_FRAMESIZE],
            denoise_filter_cache_size: 0,
            tilted_lpcs_pf: [0.0; 0x82],
            denoise_coeffs_pf: [0.0; 0x82],
            synth_filter_out_buf: [0.0; 0x80 + MAX_LSPS_ALIGN16],
        };

        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate,
            block_align,
            spillover_bitsize,
            spillover_nbits: 0,
            nb_superframes: 0,
            skip_bits_next: 0,
            sframe_cache: [0; SFRAME_CACHE_MAXSIZE + AV_INPUT_BUFFER_PADDING_SIZE],
            sframe_cache_size: 0,
            pb: PutBits::default(),
            syn,
            out: [0.0; MAX_SFRAMESIZE],
            pending: VecDeque::new(),
            eof: false,
        })
    }

    /// `wmavoice_flush`.
    fn flush_state(&mut self) {
        self.sframe_cache_size = 0;
        self.skip_bits_next = 0;
        self.syn.flush();
    }

    /// `synth_superframe`: decodes from the superframe cache when it holds
    /// data, else from `gb`, into `self.out`.
    fn synth_superframe(&mut self, gb: &mut GetBits<'_>) -> core::result::Result<usize, SfError> {
        let res = if self.sframe_cache_size > 0 {
            let mut s_gb = GetBits::new(&self.sframe_cache, self.sframe_cache_size);
            self.sframe_cache_size = 0;
            self.syn.superframe(&mut s_gb, &mut self.out)
        } else {
            self.syn.superframe(gb, &mut self.out)
        };
        if let Err(SfError::Flushed) = res {
            self.sframe_cache_size = 0;
            self.skip_bits_next = 0;
        }
        res
    }

    /// `parse_packet_header`: the number of superframes starting in this
    /// packet.
    fn parse_packet_header(&mut self, gb: &mut GetBits<'_>) -> Option<i32> {
        let mut n_superframes = 0u32;
        gb.skip_bits(4); // packet sequence number
        self.syn.has_residual_lsps = gb.get_bits1() != 0;
        loop {
            if gb.bits_left() < 6 + self.spillover_bitsize as i64 {
                return None;
            }
            // number of superframes per packet (minus first one if there is
            // spillover)
            let res = gb.get_bits(6);
            n_superframes += res;
            if res != 0x3F {
                break;
            }
        }
        self.spillover_nbits = gb.get_bits(self.spillover_bitsize) as i64;
        (gb.bits_left() >= 0).then_some(n_superframes as i32)
    }

    /// `wmavoice_decode_packet` on the unread rest `data` of a demuxer
    /// packet: the bytes it consumes and, when a superframe decoded, its
    /// sample count (samples in `self.out`). `None` drops the packet.
    fn decode_packet(&mut self, data: &[u8]) -> Option<(usize, Option<usize>)> {
        // Each block_align bytes start with a packet header; the demuxer may
        // concatenate several such codec packets, so the size is capped at
        // block_align.
        let mut size = data.len();
        while size > self.block_align {
            size -= self.block_align;
        }
        let mut gb = GetBits::new(data, size * 8);

        // size == block_align marks a new packet (as opposed to the rest of
        // one whose header was read before).
        if size % self.block_align == 0 {
            if size == 0 {
                self.spillover_nbits = 0;
                self.nb_superframes = 0;
            } else {
                self.nb_superframes = self.parse_packet_header(&mut gb)?;
            }

            // Push out the previous packet's superframe (plus the spillover
            // bits) before parsing new superframes in this packet.
            if self.sframe_cache_size > 0 {
                let cnt = gb.bits_count() as i64;
                if cnt + self.spillover_nbits > data.len() as i64 * 8 {
                    self.spillover_nbits = data.len() as i64 * 8 - cnt;
                }
                copy_bits(&mut self.pb, &mut self.sframe_cache, data, size, &mut gb, self.spillover_nbits);
                self.sframe_cache_size = self.pb.count();
                self.pb.flush(&mut self.sframe_cache);
                match self.synth_superframe(&mut gb) {
                    Ok(n) => {
                        let cnt = cnt + self.spillover_nbits;
                        self.skip_bits_next = (cnt & 7) as u32;
                        return Some(((cnt >> 3) as usize, Some(n)));
                    }
                    Err(_) => {
                        // resync
                        gb.skip_bits_long(self.spillover_nbits - cnt + gb.bits_count() as i64);
                    }
                }
            } else if self.spillover_nbits != 0 {
                gb.skip_bits_long(self.spillover_nbits); // resync
            }
        } else if self.skip_bits_next != 0 {
            gb.skip_bits(self.skip_bits_next);
        }

        // Try parsing superframes in current packet
        self.sframe_cache_size = 0;
        self.skip_bits_next = 0;
        let pos = gb.bits_left();
        let nb = self.nb_superframes;
        self.nb_superframes = nb.wrapping_sub(1);
        if nb == 0 {
            return Some((size, None));
        } else if self.nb_superframes > 0 {
            let n = self.synth_superframe(&mut gb).ok()?;
            let cnt = gb.bits_count();
            self.skip_bits_next = (cnt & 7) as u32;
            return Some((cnt >> 3, Some(n)));
        } else if pos > 0 {
            // cache the rest for the superframe spilling into the next
            // packet
            self.pb.reset();
            copy_bits(&mut self.pb, &mut self.sframe_cache, data, size, &mut gb, pos);
            self.sframe_cache_size = self.pb.count();
        }

        Some((size, None))
    }

    fn emit(&mut self, n_samples: usize) {
        let data = self.out[..n_samples].iter().flat_map(|v| v.to_le_bytes()).collect();
        self.pending.push_back(AudioFrame { samples: n_samples as u32, pts: None, data: vec![data] });
    }

    /// FFmpeg's decode loop over one demuxer packet: decode from the unread
    /// rest until it is consumed; an error drops what is left.
    fn decode_avpacket(&mut self, data: &[u8]) {
        let mut data = data;
        let mut stalls = 0;
        while !data.is_empty() {
            let Some((consumed, frame)) = self.decode_packet(data) else { break };
            if let Some(n) = frame {
                self.emit(n);
            }
            if consumed >= data.len() {
                break;
            }
            // a decoder that keeps consuming nothing would loop forever
            stalls = if consumed == 0 { stalls + 1 } else { 0 };
            if stalls > 2 {
                break;
            }
            data = &data[consumed..];
        }
    }

    /// Draining (`AV_CODEC_CAP_DELAY`): empty packets until no frame comes
    /// out; FFmpeg tolerates up to 21 errors on the way.
    fn drain(&mut self) {
        let mut errors = 0;
        loop {
            match self.decode_packet(&[]) {
                Some((_, Some(n))) => self.emit(n),
                Some((_, None)) => break,
                None => {
                    errors += 1;
                    if errors > 21 {
                        break;
                    }
                }
            }
        }
    }
}

impl Decoder for WmaVoiceDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.decode_avpacket(&packet.data);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.pop_front() {
            Some(f) => Ok(Frame::Audio(f)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    /// End of stream: drain the superframe still cached.
    fn flush(&mut self) -> Result<()> {
        if !self.eof {
            self.drain();
            self.eof = true;
        }
        Ok(())
    }

    /// Seek: FFmpeg's `wmavoice_flush`.
    fn reset(&mut self) -> Result<()> {
        self.flush_state();
        self.pending.clear();
        self.eof = false;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat { sample_format: SampleFormat::F32, sample_rate: self.sample_rate, channels: 1 })
    }
}

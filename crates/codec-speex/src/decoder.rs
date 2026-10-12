// Ported from FFmpeg libavcodec/speexdec.c at commit 2da55bf.
//
// Copyright 2002-2008  Xiph.org Foundation
// Copyright 2002-2008  Jean-Marc Valin
// Copyright 2005-2007  Analog Devices Inc.
// Copyright 2005-2008  Commonwealth Scientific and Industrial Research Organisation (CSIRO)
// Copyright 1993, 2002, 2006 David Rowe
// Copyright 2003       EpicGames
// Copyright 1992-1994  Jutta Degener, Carsten Bormann
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions
// are met:
//
// - Redistributions of source code must retain the above copyright
// notice, this list of conditions and the following disclaimer.
//
// - Redistributions in binary form must reproduce the above copyright
// notice, this list of conditions and the following disclaimer in the
// documentation and/or other materials provided with the distribution.
//
// - Neither the name of the Xiph.org Foundation nor the names of its
// contributors may be used to endorse or promote products derived from
// this software without specific prior written permission.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
// ``AS IS'' AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
// LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
// A PARTICULAR PURPOSE ARE DISCLAIMED.  IN NO EVENT SHALL THE FOUNDATION OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//
// As part of FFmpeg, also licensed under the GNU Lesser General Public
// License 2.1 or later; see LICENSE.

//! FFmpeg's Speex decoder: narrowband, wideband and ultra-wideband frames,
//! in-band stereo and the other in-band requests, with or without the Ogg
//! Speex header.
//!
//! FFmpeg never marks a frame as lost (`count_lost` stays 0), so its
//! packet-loss branches are left out, with the state only they read
//! (`last_pitch`, `last_pitch_gain`, `last_ol_gain`, `dtx_enabled`). The
//! LPC enhancer and `encode_submode` are always on there; they are not
//! options here.

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, CodecTag, Decoder, Error, Frame, Packet,
    Result, SampleFormat,
};

use crate::bitread::Gb;
use crate::tables::*;

const SPEEX_NB_MODES: usize = 3;
const SPEEX_INBAND_STEREO: u32 = 9;

const QMF_ORDER: usize = 64;
const NB_ORDER: usize = 10;
const NB_FRAME_SIZE: usize = 160;
const NB_SUBMODES: usize = 9;
const SB_SUBMODE_BITS: u32 = 3;

const NB_SUBFRAME_SIZE: usize = 40;
const NB_NB_SUBFRAMES: usize = 4;
const NB_PITCH_START: i32 = 17;
const NB_PITCH_END: usize = 144;

const NB_DEC_BUFFER: usize = NB_FRAME_SIZE + 2 * NB_PITCH_END + NB_SUBFRAME_SIZE + 12;
/// FFmpeg's `st->exc`: the current frame's excitation in `exc_buf`.
const EXC: usize = 2 * NB_PITCH_END + NB_SUBFRAME_SIZE + 6;

fn invalid(what: &str) -> Error {
    Error::invalid(format!("speex: {what}"))
}

// Floating point follows FFmpeg's build: clang contracts `a * b + c` and
// `c - a * b` written in one C expression into a fused multiply-add
// (`-ffp-contract=on`), taking the left product when both operands are
// products. Those expressions use `mul_add` here, in the same places.

fn lsp_linear(i: usize) -> f32 {
    0.25f32.mul_add(i as f32, 0.25)
}

fn lsp_linear_high(i: usize) -> f32 {
    0.3125f32.mul_add(i as f32, 0.75)
}

const LSP_DIV_256: f32 = 0.00390625;
const LSP_DIV_512: f32 = 0.001953125;
const LSP_DIV_1024: f32 = 0.0009765625;

/// `lsp += LSP_DIV_n(x)`.
fn lsp_add(lsp: &mut f32, div: f32, x: i8) {
    *lsp = div.mul_add(f32::from(x), *lsp);
}

/// libavutil's `av_clipf`: `FFMIN(FFMAX(a, amin), amax)`.
fn clipf(a: f32, amin: f32, amax: f32) -> f32 {
    let a = if a > amin { a } else { amin };
    if a > amax { amax } else { a }
}

struct LtpParam {
    gain_cdbk: &'static [i8],
    gain_bits: u32,
    pitch_bits: u32,
}

static LTP_PARAMS_VLBR: LtpParam = LtpParam { gain_cdbk: &GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 0 };
static LTP_PARAMS_LBR: LtpParam = LtpParam { gain_cdbk: &GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 7 };
static LTP_PARAMS_MED: LtpParam = LtpParam { gain_cdbk: &GAIN_CDBK_LBR, gain_bits: 5, pitch_bits: 7 };
static LTP_PARAMS_NB: LtpParam = LtpParam { gain_cdbk: &GAIN_CDBK_NB, gain_bits: 7, pitch_bits: 7 };

struct SplitCodebookParams {
    subvect_size: usize,
    nb_subvect: usize,
    shape_cb: &'static [i8],
    shape_bits: u32,
    have_sign: bool,
}

static SPLIT_CB_NB_ULBR: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 20, nb_subvect: 2, shape_cb: &EXC_20_32_TABLE, shape_bits: 5, have_sign: false };
static SPLIT_CB_NB_VLBR: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 10, nb_subvect: 4, shape_cb: &EXC_10_16_TABLE, shape_bits: 4, have_sign: false };
static SPLIT_CB_NB_LBR: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 10, nb_subvect: 4, shape_cb: &EXC_10_32_TABLE, shape_bits: 5, have_sign: false };
static SPLIT_CB_NB_MED: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 8, nb_subvect: 5, shape_cb: &EXC_8_128_TABLE, shape_bits: 7, have_sign: false };
static SPLIT_CB_NB: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 5, nb_subvect: 8, shape_cb: &EXC_5_64_TABLE, shape_bits: 6, have_sign: false };
static SPLIT_CB_SB: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 5, nb_subvect: 8, shape_cb: &EXC_5_256_TABLE, shape_bits: 8, have_sign: false };
static SPLIT_CB_HIGH: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 8, nb_subvect: 5, shape_cb: &HEXC_TABLE, shape_bits: 7, have_sign: true };
static SPLIT_CB_HIGH_LBR: SplitCodebookParams =
    SplitCodebookParams { subvect_size: 10, nb_subvect: 4, shape_cb: &HEXC_10_32_TABLE, shape_bits: 5, have_sign: false };

#[derive(Clone, Copy)]
enum LspUnquant {
    Lbr,
    Nb,
    High,
}

#[derive(Clone, Copy)]
enum LtpUnquant {
    Forced,
    ThreeTap(&'static LtpParam),
}

#[derive(Clone, Copy)]
enum Innovation {
    Noise,
    SplitCb(&'static SplitCodebookParams),
}

struct SpeexSubmode {
    /// -1 for "normal" modes, else the pitch varies by this around a
    /// global pitch.
    lbr_pitch: i32,
    /// One forced pitch gain for all sub-frames.
    forced_pitch_gain: bool,
    /// Bits of sub-frame innovation gain.
    have_subframe_gain: i32,
    /// Innovation coded twice.
    double_codebook: bool,
    lsp_unquant: LspUnquant,
    ltp_unquant: Option<LtpUnquant>,
    innovation: Option<Innovation>,
    /// Gain of the enhancer comb filter.
    comb_gain: f32,
}

/// 2150 bps "vocoder-like" mode for comfort noise.
static NB_SUBMODE1: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: true,
    have_subframe_gain: 0,
    double_codebook: false,
    lsp_unquant: LspUnquant::Lbr,
    ltp_unquant: Some(LtpUnquant::Forced),
    innovation: Some(Innovation::Noise),
    comb_gain: -1.0,
};

/// 5.95 kbps very low bit-rate mode.
static NB_SUBMODE2: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: false,
    have_subframe_gain: 0,
    double_codebook: false,
    lsp_unquant: LspUnquant::Lbr,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_VLBR)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB_VLBR)),
    comb_gain: 0.6,
};

/// 8 kbps low bit-rate mode.
static NB_SUBMODE3: SpeexSubmode = SpeexSubmode {
    lbr_pitch: -1,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: false,
    lsp_unquant: LspUnquant::Lbr,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_LBR)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB_LBR)),
    comb_gain: 0.55,
};

/// 11 kbps medium bit-rate mode.
static NB_SUBMODE4: SpeexSubmode = SpeexSubmode {
    lbr_pitch: -1,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: false,
    lsp_unquant: LspUnquant::Lbr,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_MED)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB_MED)),
    comb_gain: 0.45,
};

/// 15 kbps high bit-rate mode.
static NB_SUBMODE5: SpeexSubmode = SpeexSubmode {
    lbr_pitch: -1,
    forced_pitch_gain: false,
    have_subframe_gain: 3,
    double_codebook: false,
    lsp_unquant: LspUnquant::Nb,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_NB)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB)),
    comb_gain: 0.25,
};

/// 18.2 kbps high bit-rate mode.
static NB_SUBMODE6: SpeexSubmode = SpeexSubmode {
    lbr_pitch: -1,
    forced_pitch_gain: false,
    have_subframe_gain: 3,
    double_codebook: false,
    lsp_unquant: LspUnquant::Nb,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_NB)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_SB)),
    comb_gain: 0.15,
};

/// 24.6 kbps high bit-rate mode.
static NB_SUBMODE7: SpeexSubmode = SpeexSubmode {
    lbr_pitch: -1,
    forced_pitch_gain: false,
    have_subframe_gain: 3,
    double_codebook: true,
    lsp_unquant: LspUnquant::Nb,
    ltp_unquant: Some(LtpUnquant::ThreeTap(&LTP_PARAMS_NB)),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB)),
    comb_gain: 0.05,
};

/// 3.95 kbps very low bit-rate mode.
static NB_SUBMODE8: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: true,
    have_subframe_gain: 0,
    double_codebook: false,
    lsp_unquant: LspUnquant::Lbr,
    ltp_unquant: Some(LtpUnquant::Forced),
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_NB_ULBR)),
    comb_gain: 0.5,
};

static WB_SUBMODE1: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: false,
    lsp_unquant: LspUnquant::High,
    ltp_unquant: None,
    innovation: None,
    comb_gain: -1.0,
};

static WB_SUBMODE2: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: false,
    lsp_unquant: LspUnquant::High,
    ltp_unquant: None,
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_HIGH_LBR)),
    comb_gain: -1.0,
};

static WB_SUBMODE3: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: false,
    lsp_unquant: LspUnquant::High,
    ltp_unquant: None,
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_HIGH)),
    comb_gain: -1.0,
};

static WB_SUBMODE4: SpeexSubmode = SpeexSubmode {
    lbr_pitch: 0,
    forced_pitch_gain: false,
    have_subframe_gain: 1,
    double_codebook: true,
    lsp_unquant: LspUnquant::High,
    ltp_unquant: None,
    innovation: Some(Innovation::SplitCb(&SPLIT_CB_HIGH)),
    comb_gain: -1.0,
};

struct SpeexMode {
    mode_id: usize,
    /// Size of the frames this mode decodes.
    frame_size: usize,
    subframe_size: usize,
    /// Order of the LPC filter.
    lpc_size: usize,
    folding_gain: f32,
    submodes: [Option<&'static SpeexSubmode>; NB_SUBMODES],
    default_submode: usize,
}

static SPEEX_MODES: [SpeexMode; SPEEX_NB_MODES] = [
    SpeexMode {
        mode_id: 0,
        frame_size: NB_FRAME_SIZE,
        subframe_size: NB_SUBFRAME_SIZE,
        lpc_size: NB_ORDER,
        folding_gain: 0.0,
        submodes: [
            None,
            Some(&NB_SUBMODE1),
            Some(&NB_SUBMODE2),
            Some(&NB_SUBMODE3),
            Some(&NB_SUBMODE4),
            Some(&NB_SUBMODE5),
            Some(&NB_SUBMODE6),
            Some(&NB_SUBMODE7),
            Some(&NB_SUBMODE8),
        ],
        default_submode: 5,
    },
    SpeexMode {
        mode_id: 1,
        frame_size: NB_FRAME_SIZE,
        subframe_size: NB_SUBFRAME_SIZE,
        lpc_size: 8,
        folding_gain: 0.9,
        submodes: [
            None,
            Some(&WB_SUBMODE1),
            Some(&WB_SUBMODE2),
            Some(&WB_SUBMODE3),
            Some(&WB_SUBMODE4),
            None,
            None,
            None,
            None,
        ],
        default_submode: 3,
    },
    SpeexMode {
        mode_id: 2,
        frame_size: 320,
        subframe_size: 80,
        lpc_size: 8,
        folding_gain: 0.7,
        submodes: [None, Some(&WB_SUBMODE1), None, None, None, None, None, None, None],
        default_submode: 1,
    },
];

/// FFmpeg's `DecoderState`: one per band layer.
#[derive(Clone)]
struct DecoderState {
    mode: &'static SpeexMode,
    first: bool,
    full_frame_size: usize,
    is_wideband: bool,
    frame_size: usize,
    subframe_size: usize,
    nb_subframes: usize,
    lpc_size: usize,
    /// Where the innovation goes in the upper layer's output, if one is
    /// decoding on top of this layer.
    innov_save: Option<usize>,
    seed: u32,
    submode_id: usize,
    voc_m1: f32,
    voc_m2: f32,
    voc_mean: f32,
    voc_offset: i32,
    highpass_enabled: bool,
    mem_hp: [f32; 2],
    exc_buf: [f32; NB_DEC_BUFFER],
    old_qlsp: [f32; NB_ORDER],
    interp_qlpc: [f32; NB_ORDER],
    mem_sp: [f32; NB_ORDER],
    g0_mem: [f32; QMF_ORDER],
    g1_mem: [f32; QMF_ORDER],
    pi_gain: [f32; NB_NB_SUBFRAMES],
    exc_rms: [f32; NB_NB_SUBFRAMES],
}

impl DecoderState {
    /// FFmpeg's `decoder_init` on a zeroed state.
    fn new(mode: &'static SpeexMode) -> Self {
        Self {
            mode,
            first: true,
            full_frame_size: (1 + usize::from(mode.mode_id > 0)) * mode.frame_size,
            is_wideband: mode.mode_id > 0,
            frame_size: mode.frame_size,
            subframe_size: mode.subframe_size,
            nb_subframes: mode.frame_size / mode.subframe_size,
            lpc_size: mode.lpc_size,
            innov_save: None,
            seed: 1000,
            submode_id: mode.default_submode,
            voc_m1: 0.0,
            voc_m2: 0.0,
            voc_mean: 0.0,
            voc_offset: 0,
            highpass_enabled: mode.mode_id == 0,
            mem_hp: [0.0; 2],
            exc_buf: [0.0; NB_DEC_BUFFER],
            old_qlsp: [0.0; NB_ORDER],
            interp_qlpc: [0.0; NB_ORDER],
            mem_sp: [0.0; NB_ORDER],
            g0_mem: [0.0; QMF_ORDER],
            g1_mem: [0.0; QMF_ORDER],
            pi_gain: [0.0; NB_NB_SUBFRAMES],
            exc_rms: [0.0; NB_NB_SUBFRAMES],
        }
    }
}

#[derive(Clone, Copy)]
struct StereoState {
    /// Left/right balance.
    balance: f32,
    /// E(left+right) / (E(left) + E(right)).
    e_ratio: f32,
    smooth_left: f32,
    smooth_right: f32,
}

impl StereoState {
    fn new() -> Self {
        Self { balance: 1.0, e_ratio: 0.5, smooth_left: 1.0, smooth_right: 1.0 }
    }
}

/// Default handler for user in-band requests: skip them.
fn speex_default_user_handler(gb: &mut Gb<'_>) {
    let req_size = gb.get_bits(4) as usize;
    gb.skip_bits(5 + 8 * req_size);
}

fn speex_std_stereo(gb: &mut Gb<'_>, stereo: &mut StereoState) {
    let sign = if gb.get_bits1() != 0 { -1.0f32 } else { 1.0 };
    stereo.balance = f64::from(sign * 0.25 * gb.get_bits(5) as f32).exp() as f32;
    stereo.e_ratio = E_RATIO_QUANT[gb.get_bits(2) as usize];
}

fn speex_inband_handler(gb: &mut Gb<'_>, stereo: &mut StereoState) {
    let id = gb.get_bits(4);
    if id == SPEEX_INBAND_STEREO {
        speex_std_stereo(gb, stereo);
    } else {
        let adv = match id {
            0..=1 => 1,
            2..=7 => 4,
            8..=9 => 8,
            10..=11 => 16,
            12..=13 => 32,
            _ => 64,
        };
        gb.skip_bits(adv);
    }
}

fn lsp_unquant(kind: LspUnquant, lsp: &mut [f32; NB_ORDER], order: usize, gb: &mut Gb<'_>) {
    match kind {
        LspUnquant::Lbr => {
            for (i, l) in lsp.iter_mut().enumerate().take(order) {
                *l = lsp_linear(i);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..10 {
                lsp_add(&mut lsp[i], LSP_DIV_256, CDBK_NB[id * 10 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i], LSP_DIV_512, CDBK_NB_LOW1[id * 5 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i + 5], LSP_DIV_512, CDBK_NB_HIGH1[id * 5 + i]);
            }
        }
        LspUnquant::Nb => {
            for (i, l) in lsp.iter_mut().enumerate().take(order) {
                *l = lsp_linear(i);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..10 {
                lsp_add(&mut lsp[i], LSP_DIV_256, CDBK_NB[id * 10 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i], LSP_DIV_512, CDBK_NB_LOW1[id * 5 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i], LSP_DIV_1024, CDBK_NB_LOW2[id * 5 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i + 5], LSP_DIV_512, CDBK_NB_HIGH1[id * 5 + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..5 {
                lsp_add(&mut lsp[i + 5], LSP_DIV_1024, CDBK_NB_HIGH2[id * 5 + i]);
            }
        }
        LspUnquant::High => {
            for (i, l) in lsp.iter_mut().enumerate().take(order) {
                *l = lsp_linear_high(i);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..order {
                lsp_add(&mut lsp[i], LSP_DIV_256, HIGH_LSP_CDBK[id * order + i]);
            }
            let id = gb.get_bits(6) as usize;
            for i in 0..order {
                lsp_add(&mut lsp[i], LSP_DIV_512, HIGH_LSP_CDBK2[id * order + i]);
            }
        }
    }
}

/// FFmpeg's `forced_pitch_unquant`; returns the pitch.
fn forced_pitch_unquant(
    exc_buf: &mut [f32; NB_DEC_BUFFER],
    exc: usize,
    exc_out: &mut [f32],
    start: i32,
    pitch_coef: f32,
    nsf: usize,
    gain_val: &mut [f32; 3],
) -> i32 {
    let pitch_coef = pitch_coef.min(0.99);
    for i in 0..nsf {
        exc_out[i] = exc_buf[exc + i - start as usize] * pitch_coef;
        exc_buf[exc + i] = exc_out[i];
    }
    *gain_val = [0.0, pitch_coef, 0.0];
    start
}

fn speex_rand(std: f32, seed: &mut u32) -> f32 {
    const JFLONE: u32 = 0x3f80_0000;
    const JFLMSK: u32 = 0x007f_ffff;
    *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    let ran = JFLONE | (JFLMSK & *seed);
    let mut fran = f32::from_bits(ran);
    fran -= 1.5;
    fran *= std;
    fran
}

fn split_cb_shape_sign_unquant(exc: &mut [f32], params: &SplitCodebookParams, gb: &mut Gb<'_>) {
    let mut signs = [false; 10];
    let mut ind = [0usize; 10];
    for i in 0..params.nb_subvect {
        signs[i] = params.have_sign && gb.get_bits1() != 0;
        ind[i] = gb.get_bitsz(params.shape_bits) as usize;
    }
    let size = params.subvect_size;
    for i in 0..params.nb_subvect {
        let s = if signs[i] { -1.0f32 } else { 1.0 };
        for j in 0..size {
            let k = size * i + j;
            exc[k] = (s * 0.03125).mul_add(f32::from(params.shape_cb[ind[i] * size + j]), exc[k]);
        }
    }
}

fn innovation_unquant(kind: Innovation, exc: &mut [f32], nsf: usize, gb: &mut Gb<'_>, seed: &mut u32) {
    match kind {
        Innovation::Noise => {
            for e in &mut exc[..nsf] {
                *e = speex_rand(1.0, seed);
            }
        }
        Innovation::SplitCb(params) => split_cb_shape_sign_unquant(exc, params, gb),
    }
}

fn gain_3tap_to_1tap(g: &[f32; 3]) -> f32 {
    g[1].abs() + (if g[0] > 0.0 { g[0] } else { -0.5 * g[0] }) + (if g[2] > 0.0 { g[2] } else { -0.5 * g[2] })
}

/// FFmpeg's `pitch_unquant_3tap` (no lost frames); returns the pitch.
#[allow(clippy::too_many_arguments)]
fn pitch_unquant_3tap(
    exc_buf: &[f32; NB_DEC_BUFFER],
    exc: usize,
    exc_out: &mut [f32],
    start: i32,
    params: &LtpParam,
    nsf: usize,
    gain_val: &mut [f32; 3],
    gb: &mut Gb<'_>,
) -> i32 {
    let pitch = gb.get_bitsz(params.pitch_bits) as i32 + start;
    let gain_index = gb.get_bitsz(params.gain_bits) as usize;
    let cdbk = params.gain_cdbk;
    let gain = [
        0.015625f32.mul_add(f32::from(cdbk[gain_index * 4]), 0.5),
        0.015625f32.mul_add(f32::from(cdbk[gain_index * 4 + 1]), 0.5),
        0.015625f32.mul_add(f32::from(cdbk[gain_index * 4 + 2]), 0.5),
    ];
    *gain_val = gain;
    exc_out[..nsf].fill(0.0);

    let at = |k: i32| exc_buf[(exc as i32 + k) as usize];
    let nsf = nsf as i32;
    for i in 0..3 {
        let pp = pitch + 1 - i as i32;
        let tmp1 = nsf.min(pp);
        for j in 0..tmp1 {
            exc_out[j as usize] = gain[2 - i].mul_add(at(j - pp), exc_out[j as usize]);
        }
        let tmp3 = nsf.min(pp + pitch);
        for j in tmp1..tmp3 {
            exc_out[j as usize] = gain[2 - i].mul_add(at(j - pp - pitch), exc_out[j as usize]);
        }
    }
    pitch
}

fn compute_rms(x: &[f32]) -> f32 {
    let mut sum = 0.0f32;
    for &v in x {
        sum = v.mul_add(v, sum);
    }
    (0.1 + sum / x.len() as f32).sqrt()
}

fn bw_lpc(gamma: f32, lpc_in: &[f32], lpc_out: &mut [f32], order: usize) {
    let mut tmp = gamma;
    for i in 0..order {
        lpc_out[i] = tmp * lpc_in[i];
        tmp *= gamma;
    }
}

/// FFmpeg's `iir_mem` with input and output in `y`.
fn iir_mem(y: &mut [f32], den: &[f32], ord: usize, mem: &mut [f32]) {
    for v in y.iter_mut() {
        let yi = *v + mem[0];
        let nyi = -yi;
        for j in 0..ord - 1 {
            mem[j] = den[j].mul_add(nyi, mem[j + 1]);
        }
        mem[ord - 1] = den[ord - 1] * nyi;
        *v = yi;
    }
}

/// FFmpeg's `highpass`, in place.
fn highpass(y: &mut [f32], mem: &mut [f32; 2], wide: bool) {
    const PCOEF: [[f32; 3]; 2] = [[1.00000, -1.92683, 0.93071], [1.00000, -1.97226, 0.97332]];
    const ZCOEF: [[f32; 3]; 2] = [[0.96446, -1.92879, 0.96446], [0.98645, -1.97277, 0.98645]];
    let den = &PCOEF[usize::from(wide)];
    let num = &ZCOEF[usize::from(wide)];
    for v in y.iter_mut() {
        let x = *v;
        let yi = num[0].mul_add(x, mem[0]);
        mem[0] = (-den[1]).mul_add(yi, num[1].mul_add(x, mem[1]));
        mem[1] = num[2].mul_add(x, -den[2] * yi);
        *v = yi;
    }
}

fn sanitize_values(vec: &mut [f32], min_val: f32, max_val: f32) {
    for v in vec {
        *v = if !v.is_normal() || v.abs() < 1e-8 { 0.0 } else { clipf(*v, min_val, max_val) };
    }
}

fn signal_mul(x: &mut [f32], scale: f32) {
    for v in x {
        *v *= scale;
    }
}

/// FFmpeg's `inner_prod`: products summed eight at a time (`len` is a
/// multiple of 8).
fn inner_prod(x: &[f32], y: &[f32], len: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in (0..len).step_by(8) {
        let mut part = 0.0f32;
        for k in 0..8 {
            part = x[i + k].mul_add(y[i + k], part);
        }
        sum += part;
    }
    sum
}

/// FFmpeg's `interp_pitch` with `exc` at `exc_buf[exc]`.
fn interp_pitch(exc_buf: &[f32; NB_DEC_BUFFER], exc: usize, interp: &mut [f32], pitch: i32, len: usize) -> i32 {
    let base = |k: i32| (exc as i32 + k) as usize;
    let mut corr = [[0.0f32; 7]; 4];
    for (i, c) in corr[0].iter_mut().enumerate() {
        let y = base(-pitch - 3 + i as i32);
        *c = inner_prod(&exc_buf[exc..exc + len], &exc_buf[y..y + len], len);
    }
    for i in 0..3 {
        for j in 0..7usize {
            let i1 = 3usize.saturating_sub(j);
            let i2 = (10 - j).min(7);
            let mut tmp = 0.0f32;
            for k in i1..i2 {
                tmp = SHIFT_FILT[i][k].mul_add(corr[0][j + k - 3], tmp);
            }
            corr[i + 1][j] = tmp;
        }
    }
    let (mut maxi, mut maxj) = (0usize, 0i32);
    let mut maxcorr = corr[0][0];
    for (i, row) in corr.iter().enumerate() {
        for (j, &c) in row.iter().enumerate() {
            if c > maxcorr {
                maxcorr = c;
                maxi = i;
                maxj = j as i32;
            }
        }
    }
    let lag = pitch - maxj + 3;
    for (i, out) in interp.iter_mut().enumerate().take(len) {
        let mut tmp = 0.0f32;
        if maxi > 0 {
            for k in 0..7 {
                tmp = exc_buf[base(i as i32 - lag + k as i32 - 3)].mul_add(SHIFT_FILT[maxi - 1][k], tmp);
            }
        } else {
            tmp = exc_buf[base(i as i32 - lag)];
        }
        *out = tmp;
    }
    lag
}

/// FFmpeg's `multicomb` (the LPC enhancer) with `exc` at `exc_buf[exc]`.
fn multicomb(exc_buf: &[f32; NB_DEC_BUFFER], exc: usize, new_exc: &mut [f32], nsf: usize, pitch: i32, max_pitch: i32, comb_gain: f32) {
    let mut iexc = [0.0f32; 4 * NB_SUBFRAME_SIZE];
    let corr_pitch = pitch;
    let ex = &exc_buf[exc..exc + nsf];

    interp_pitch(exc_buf, exc, &mut iexc[..80], corr_pitch, 80);
    if corr_pitch > max_pitch {
        interp_pitch(exc_buf, exc, &mut iexc[nsf..], 2 * corr_pitch, 80);
    } else {
        interp_pitch(exc_buf, exc, &mut iexc[nsf..], -corr_pitch, 80);
    }

    let (iexc0, iexc1) = iexc.split_at(nsf);
    let iexc0_mag = (1000.0 + inner_prod(iexc0, iexc0, nsf)).sqrt();
    let iexc1_mag = (1000.0 + inner_prod(iexc1, iexc1, nsf)).sqrt();
    let exc_mag = (1.0 + inner_prod(ex, ex, nsf)).sqrt();
    let corr0 = inner_prod(iexc0, ex, nsf);
    let corr1 = inner_prod(iexc1, ex, nsf);
    let pgain1 = if corr0 > iexc0_mag * exc_mag { 1.0 } else { (corr0 / exc_mag) / iexc0_mag };
    let pgain2 = if corr1 > iexc1_mag * exc_mag { 1.0 } else { (corr1 / exc_mag) / iexc1_mag };
    let gg1 = exc_mag / iexc0_mag;
    let gg2 = exc_mag / iexc1_mag;
    let (c1, c2) = if comb_gain > 0.0 {
        let c1 = 0.4f32.mul_add(comb_gain, 0.07);
        (c1, 1.72f32.mul_add(c1 - 0.07, 0.5))
    } else {
        (0.0, 0.0)
    };
    let mut g1 = (-(c2 * pgain1)).mul_add(pgain1, 1.0);
    let mut g2 = (-(c2 * pgain2)).mul_add(pgain2, 1.0);
    g1 = g1.max(c1);
    g2 = g2.max(c1);
    g1 = c1 / g1;
    g2 = c1 / g2;

    let (gain0, gain1) = if corr_pitch > max_pitch {
        (0.7 * g1 * gg1, 0.3 * g2 * gg2)
    } else {
        (0.6 * g1 * gg1, 0.6 * g2 * gg2)
    };
    for i in 0..nsf {
        new_exc[i] = gain1.mul_add(iexc1[i], gain0.mul_add(iexc0[i], ex[i]));
    }
    let mut new_ener = compute_rms(&new_exc[..nsf]);
    let mut old_ener = compute_rms(ex);

    old_ener = old_ener.max(1.0);
    new_ener = new_ener.max(1.0);
    old_ener = old_ener.min(new_ener);
    let ngain = old_ener / new_ener;
    for v in &mut new_exc[..nsf] {
        *v *= ngain;
    }
}

fn lsp_interpolate(old_lsp: &[f32], new_lsp: &[f32], lsp: &mut [f32], len: usize, subframe: usize, nb_subframes: usize, margin: f32) {
    let tmp = (1.0 + subframe as f32) / nb_subframes as f32;
    // `M_PI - margin` is computed in double, then passed as a float.
    let high = (std::f64::consts::PI - f64::from(margin)) as f32;
    for i in 0..len {
        lsp[i] = (1.0 - tmp).mul_add(old_lsp[i], tmp * new_lsp[i]);
        lsp[i] = clipf(lsp[i], margin, high);
    }
    for i in 1..len - 1 {
        lsp[i] = lsp[i].max(lsp[i - 1] + margin);
        if lsp[i] > lsp[i + 1] - margin {
            lsp[i] = 0.5 * (lsp[i] + lsp[i + 1] - margin);
        }
    }
}

fn lsp_to_lpc(freq: &[f32], ak: &mut [f32], lpcrdr: usize) {
    let mut wp = [0.0f32; 4 * NB_ORDER + 2];
    let mut x_freq = [0.0f32; NB_ORDER];
    let m = lpcrdr >> 1;
    let (mut xin1, mut xin2) = (1.0f32, 1.0f32);

    for i in 0..lpcrdr {
        x_freq[i] = -freq[i].cos();
    }

    // Reconstruct P(z) and Q(z) by cascading second order polynomials of
    // the form 1 - 2xz(-1) + z(-2), where x is the LSP coefficient.
    let mut n0 = 0;
    for j in 0..=lpcrdr {
        let mut i2 = 0;
        for i in 0..m {
            n0 = i * 4;
            let xout1 = (2.0 * x_freq[i2]).mul_add(wp[n0], xin1) + wp[n0 + 1];
            let xout2 = (2.0 * x_freq[i2 + 1]).mul_add(wp[n0 + 2], xin2) + wp[n0 + 3];
            wp[n0 + 1] = wp[n0];
            wp[n0 + 3] = wp[n0 + 2];
            wp[n0] = xin1;
            wp[n0 + 2] = xin2;
            xin1 = xout1;
            xin2 = xout2;
            i2 += 2;
        }
        let xout1 = xin1 + wp[n0 + 4];
        let xout2 = xin2 - wp[n0 + 5];
        if j > 0 {
            ak[j - 1] = (xout1 + xout2) * 0.5;
        }
        wp[n0 + 4] = xin1;
        wp[n0 + 5] = xin2;

        xin1 = 0.0;
        xin2 = 0.0;
    }
}

/// FFmpeg's `qmf_synth`: the low band in `buf[..n / 2]` and the high band
/// in `buf[n / 2..n]` become `n` output samples in `buf`.
fn qmf_synth(buf: &mut [f32], n: usize, a: &[f32; QMF_ORDER], mem1: &mut [f32; QMF_ORDER], mem2: &mut [f32; QMF_ORDER]) {
    let m2 = QMF_ORDER >> 1;
    let n2 = n >> 1;
    let mut xx1 = [0.0f32; 352];
    let mut xx2 = [0.0f32; 352];

    for i in 0..n2 {
        xx1[i] = buf[n2 - 1 - i];
    }
    for i in 0..m2 {
        xx1[n2 + i] = mem1[2 * i + 1];
    }
    for i in 0..n2 {
        xx2[i] = buf[n2 + n2 - 1 - i];
    }
    for i in 0..m2 {
        xx2[n2 + i] = mem2[2 * i + 1];
    }

    for i in (0..n2).step_by(2) {
        let (mut y0, mut y1, mut y2, mut y3) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut x10 = xx1[n2 - 2 - i];
        let mut x20 = xx2[n2 - 2 - i];

        for j in (0..m2).step_by(2) {
            let mut a0 = a[2 * j];
            let mut a1 = a[2 * j + 1];
            let x11 = xx1[n2 - 1 + j - i];
            let x21 = xx2[n2 - 1 + j - i];

            y0 = a0.mul_add(x11 - x21, y0);
            y1 = a1.mul_add(x11 + x21, y1);
            y2 = a0.mul_add(x10 - x20, y2);
            y3 = a1.mul_add(x10 + x20, y3);
            a0 = a[2 * j + 2];
            a1 = a[2 * j + 3];
            x10 = xx1[n2 + j - i];
            x20 = xx2[n2 + j - i];

            y0 = a0.mul_add(x10 - x20, y0);
            y1 = a1.mul_add(x10 + x20, y1);
            y2 = a0.mul_add(x11 - x21, y2);
            y3 = a1.mul_add(x11 + x21, y3);
        }
        buf[2 * i] = 2.0 * y0;
        buf[2 * i + 1] = 2.0 * y1;
        buf[2 * i + 2] = 2.0 * y2;
        buf[2 * i + 3] = 2.0 * y3;
    }

    for i in 0..m2 {
        mem1[2 * i + 1] = xx1[i];
    }
    for i in 0..m2 {
        mem2[2 * i + 1] = xx2[i];
    }
}

/// FFmpeg's `speex_decode_stereo`: a mono frame at the start of `data`
/// becomes `frame_size` interleaved stereo samples.
fn speex_decode_stereo(data: &mut [f32], frame_size: usize, stereo: &mut StereoState) {
    let balance = stereo.balance;
    let e_ratio = stereo.e_ratio;
    let e_right = 1.0 / (e_ratio * (1.0 + balance)).sqrt();
    let e_left = balance.sqrt() * e_right;

    for i in (0..frame_size).rev() {
        let tmp = data[i];
        stereo.smooth_left = stereo.smooth_left.mul_add(0.98, e_left * 0.02);
        stereo.smooth_right = stereo.smooth_right.mul_add(0.98, e_right * 0.02);
        data[2 * i] = stereo.smooth_left * tmp;
        data[2 * i + 1] = stereo.smooth_right * tmp;
    }
}

/// The stream setup FFmpeg's `speex_decode_init` derives.
struct Setup {
    rate: i32,
    mode: usize,
    nb_channels: usize,
    frame_size: usize,
    frames_per_packet: usize,
    pkt_size: usize,
}

/// FFmpeg's `parse_speex_extradata`: the Speex header found anywhere in
/// `extradata` (Ogg passes it alone, AVI and MOV inside other bytes).
fn parse_speex_extradata(extradata: &[u8]) -> Result<Setup> {
    let start = extradata
        .windows(8)
        .position(|w| w == b"Speex   ")
        .ok_or_else(|| invalid("no Speex header in the extradata"))?;
    // Fields past the end read as zeros, like FFmpeg's zeroed padding.
    let field = |k: usize| {
        let at = start + 28 + 4 * k;
        i32::from_le_bytes(std::array::from_fn(|b| extradata.get(at + b).copied().unwrap_or(0)))
    };
    // Field 0 is version_id, 1 the header size.
    let rate = field(2);
    if rate <= 0 {
        return Err(invalid("bad sample rate"));
    }
    let mode = field(3);
    if !(0..SPEEX_NB_MODES as i32).contains(&mode) {
        return Err(invalid("bad mode"));
    }
    if field(4) != 4 {
        return Err(invalid("unsupported bitstream version"));
    }
    let nb_channels = field(5);
    if !(1..=2).contains(&nb_channels) {
        return Err(invalid("bad channel count"));
    }
    // Field 6 is the bit rate.
    let frame_size = field(7);
    let uwb = i32::from(mode > 1);
    if frame_size < (NB_FRAME_SIZE as i32) << uwb || frame_size > i32::MAX >> uwb {
        return Err(invalid("bad frame size"));
    }
    let frame_size = (frame_size << uwb).min((NB_FRAME_SIZE as i32) << mode);
    // Field 8 is the VBR flag.
    let frames_per_packet = field(9);
    if frames_per_packet <= 0 || frames_per_packet > 64 || frames_per_packet >= i32::MAX / nb_channels / frame_size {
        return Err(invalid("bad frames per packet"));
    }
    Ok(Setup {
        rate,
        mode: mode as usize,
        nb_channels: nb_channels as usize,
        frame_size: frame_size as usize,
        frames_per_packet: frames_per_packet as usize,
        pkt_size: 0,
    })
}

/// FFmpeg's `speex_decode_init`.
fn setup(params: &CodecParameters) -> Result<Setup> {
    let rate = params.sample_rate.map_or(0, |r| r.min(i32::MAX as u32) as i32);
    let mut s = if params.extradata.len() >= 80 {
        parse_speex_extradata(&params.extradata)?
    } else {
        if rate <= 0 {
            return Err(invalid("no sample rate"));
        }
        let nb_channels = usize::from(params.channels.unwrap_or(0));
        if !(1..=2).contains(&nb_channels) {
            return Err(invalid("bad channel count"));
        }
        let mode = match rate {
            8000 => 0,
            16000 => 1,
            _ => 2,
        };
        Setup { rate, mode, nb_channels, frame_size: NB_FRAME_SIZE << mode, frames_per_packet: 64, pkt_size: 0 }
    };

    // ZygoAudio: fixed-size packets of a quality the extradata gives.
    if params.tag == Some(CodecTag::fourcc(b"SPXN")) {
        if params.extradata.len() < 47 {
            return Err(invalid("missing or invalid extradata"));
        }
        let quality = usize::from(params.extradata[37]);
        if quality > 10 {
            return Err(Error::unsupported(format!("speex: unsupported quality mode {quality}")));
        }
        s.pkt_size = [5, 10, 15, 20, 20, 28, 28, 38, 38, 46, 62][quality];
        s.mode = 0;
        s.nb_channels = 1;
        s.rate = rate;
        if s.rate <= 0 {
            return Err(invalid("no sample rate"));
        }
        s.frames_per_packet = 1;
        s.frame_size = NB_FRAME_SIZE;
    }
    Ok(s)
}

/// Speex decoder (`speex`).
pub struct SpeexDecoder {
    codec_id: CodecId,
    rate: i32,
    mode: usize,
    nb_channels: usize,
    /// Output samples per frame.
    frame_size: usize,
    frames_per_packet: usize,
    /// ZygoAudio's meaningful bytes in a 62-byte packet; 0 otherwise.
    pkt_size: usize,
    stereo: StereoState,
    st: [DecoderState; SPEEX_NB_MODES],
    ready: VecDeque<Frame>,
    flushed: bool,
}

impl SpeexDecoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let s = setup(params)?;
        Ok(Self {
            codec_id: params.codec_id.clone(),
            rate: s.rate,
            mode: s.mode,
            nb_channels: s.nb_channels,
            frame_size: s.frame_size,
            frames_per_packet: s.frames_per_packet,
            pkt_size: s.pkt_size,
            stereo: StereoState::new(),
            st: std::array::from_fn(|m| DecoderState::new(&SPEEX_MODES[m])),
            ready: VecDeque::new(),
            flushed: false,
        })
    }

    /// The decode function of mode `m` (`speex_modes[m].decode`).
    fn decode_mode(&mut self, m: usize, gb: &mut Gb<'_>, out: &mut [f32], packets_left: usize) -> Result<()> {
        if m == 0 { self.nb_decode(gb, out) } else { self.sb_decode(m, gb, out, packets_left) }
    }

    /// FFmpeg's `nb_decode`: one narrowband frame into `out[..160]`.
    fn nb_decode(&mut self, gb: &mut Gb<'_>, out: &mut [f32]) -> Result<()> {
        let st = &mut self.st[0];
        let stereo = &mut self.stereo;
        let mut ol_pitch_coef = 0.0f32;
        let mut best_pitch_gain = 0.0f32;
        let mut ol_pitch = 0i32;
        let mut best_pitch = 40i32;
        let mut innov = [0.0f32; NB_SUBFRAME_SIZE];
        let mut exc32 = [0.0f32; NB_SUBFRAME_SIZE];
        let mut interp_qlsp = [0.0f32; NB_ORDER];
        let mut qlsp = [0.0f32; NB_ORDER];
        let mut ak = [0.0f32; NB_ORDER];
        let mut pitch_gain = [0.0f32; 3];

        // Find the next narrowband block: handle requests, skip wideband
        // blocks.
        let m = loop {
            if gb.bits_left() < 5 {
                return Err(invalid("frame data ends early"));
            }
            if gb.get_bits1() != 0 {
                // A wideband block, skipped for compatibility.
                let advance = i32::from(WB_SKIP_TABLE[gb.get_bits(SB_SUBMODE_BITS) as usize]) - (SB_SUBMODE_BITS as i32 + 1);
                if advance < 0 {
                    return Err(invalid("bad wideband submode"));
                }
                gb.skip_bits(advance as usize);
                if gb.bits_left() < 5 {
                    return Err(invalid("frame data ends early"));
                }
                if gb.get_bits1() != 0 {
                    let advance =
                        i32::from(WB_SKIP_TABLE[gb.get_bits(SB_SUBMODE_BITS) as usize]) - (SB_SUBMODE_BITS as i32 + 1);
                    if advance < 0 {
                        return Err(invalid("bad wideband submode"));
                    }
                    gb.skip_bits(advance as usize);
                    if gb.get_bits1() != 0 {
                        return Err(invalid("more than two wideband layers"));
                    }
                }
            }
            if gb.bits_left() < 4 {
                return Err(invalid("frame data ends early"));
            }
            let m = gb.get_bits(4);
            match m {
                15 => return Err(invalid("terminator")),
                14 => speex_inband_handler(gb, stereo),
                13 => speex_default_user_handler(gb),
                9..=12 => return Err(invalid("bad mode")),
                _ => break m as usize,
            }
        };
        st.submode_id = m;

        // Shift all buffers by one frame.
        st.exc_buf.copy_within(NB_FRAME_SIZE.., 0);

        let Some(submode) = st.mode.submodes[st.submode_id] else {
            // Null mode (no transmission).
            let mut lpc = [0.0f32; NB_ORDER];
            bw_lpc(0.93, &st.interp_qlpc, &mut lpc, NB_ORDER);
            let innov_gain = compute_rms(&st.exc_buf[EXC..EXC + NB_FRAME_SIZE]);
            for i in 0..NB_FRAME_SIZE {
                st.exc_buf[EXC + i] = speex_rand(innov_gain, &mut st.seed);
            }
            // Final signal synthesis from the excitation.
            out[..NB_FRAME_SIZE].copy_from_slice(&st.exc_buf[EXC..EXC + NB_FRAME_SIZE]);
            iir_mem(&mut out[..NB_FRAME_SIZE], &lpc, NB_ORDER, &mut st.mem_sp);
            return Ok(());
        };

        lsp_unquant(submode.lsp_unquant, &mut qlsp, NB_ORDER, gb);

        if st.first {
            st.old_qlsp = qlsp;
        }

        // Open-loop pitch for low bit-rate pitch coding.
        if submode.lbr_pitch != -1 {
            ol_pitch = NB_PITCH_START + gb.get_bits(7) as i32;
        }
        if submode.forced_pitch_gain {
            ol_pitch_coef = 0.066667 * gb.get_bits(4) as f32;
        }
        // Global excitation gain.
        let ol_gain = (gb.get_bits(5) as f32 / 3.5).exp();
        if st.submode_id == 1 {
            // DTX flag; FFmpeg never reads it back.
            gb.get_bits(4);
        }

        for sub in 0..NB_NB_SUBFRAMES {
            let offset = NB_SUBFRAME_SIZE * sub;
            let exc = EXC + offset;
            st.exc_buf[exc..exc + NB_SUBFRAME_SIZE].fill(0.0);

            // Pitch constraints.
            let pit_min = if submode.lbr_pitch != -1 {
                let margin = submode.lbr_pitch;
                if margin != 0 { (ol_pitch - margin + 1).max(NB_PITCH_START) } else { ol_pitch }
            } else {
                NB_PITCH_START
            };

            // Adaptive codebook contribution.
            let pitch = match submode.ltp_unquant {
                Some(LtpUnquant::Forced) => forced_pitch_unquant(
                    &mut st.exc_buf,
                    exc,
                    &mut exc32,
                    pit_min,
                    ol_pitch_coef,
                    NB_SUBFRAME_SIZE,
                    &mut pitch_gain,
                ),
                Some(LtpUnquant::ThreeTap(params)) => {
                    pitch_unquant_3tap(&st.exc_buf, exc, &mut exc32, pit_min, params, NB_SUBFRAME_SIZE, &mut pitch_gain, gb)
                }
                None => return Err(invalid("narrowband submode without a pitch predictor")),
            };

            sanitize_values(&mut exc32, -32000.0, 32000.0);

            let tmp = gain_3tap_to_1tap(&pitch_gain);
            if (tmp > best_pitch_gain
                && (2 * best_pitch - pitch).abs() >= 3
                && (3 * best_pitch - pitch).abs() >= 4
                && (4 * best_pitch - pitch).abs() >= 5)
                || (tmp > 0.6 * best_pitch_gain
                    && ((best_pitch - 2 * pitch).abs() < 3
                        || (best_pitch - 3 * pitch).abs() < 4
                        || (best_pitch - 4 * pitch).abs() < 5))
                || ((0.67 * tmp) > best_pitch_gain
                    && ((2 * best_pitch - pitch).abs() < 3
                        || (3 * best_pitch - pitch).abs() < 4
                        || (4 * best_pitch - pitch).abs() < 5))
            {
                best_pitch = pitch;
                if tmp > best_pitch_gain {
                    best_pitch_gain = tmp;
                }
            }

            innov.fill(0.0);

            // Sub-frame gain correction.
            let ener = match submode.have_subframe_gain {
                3 => EXC_GAIN_QUANT_SCAL3[gb.get_bits(3) as usize] * ol_gain,
                1 => EXC_GAIN_QUANT_SCAL1[gb.get_bits1() as usize] * ol_gain,
                _ => ol_gain,
            };

            // Fixed codebook contribution.
            let Some(innovation) = submode.innovation else {
                return Err(invalid("narrowband submode without an innovation codebook"));
            };
            innovation_unquant(innovation, &mut innov, NB_SUBFRAME_SIZE, gb, &mut st.seed);
            signal_mul(&mut innov, ener);

            // Second codebook (some modes only).
            if submode.double_codebook {
                let mut innov2 = [0.0f32; NB_SUBFRAME_SIZE];
                innovation_unquant(innovation, &mut innov2, NB_SUBFRAME_SIZE, gb, &mut st.seed);
                signal_mul(&mut innov2, 0.454545 * ener);
                for (a, b) in innov.iter_mut().zip(innov2) {
                    *a += b;
                }
            }
            for i in 0..NB_SUBFRAME_SIZE {
                st.exc_buf[exc + i] = exc32[i] + innov[i];
            }
            if let Some(save) = st.innov_save {
                out[save + offset..save + offset + NB_SUBFRAME_SIZE].copy_from_slice(&innov);
            }

            // Vocoder mode.
            if st.submode_id == 1 {
                let g = clipf(1.5 * (ol_pitch_coef - 0.2), 0.0, 1.0);

                st.exc_buf[exc..exc + NB_SUBFRAME_SIZE].fill(0.0);
                while st.voc_offset < NB_SUBFRAME_SIZE as i32 {
                    if st.voc_offset >= 0 {
                        st.exc_buf[exc + st.voc_offset as usize] = (2.0 * ol_pitch as f32).sqrt() * (g * ol_gain);
                    }
                    st.voc_offset += ol_pitch;
                }
                st.voc_offset -= NB_SUBFRAME_SIZE as i32;

                for (i, &inn) in innov.iter().enumerate() {
                    let exci = st.exc_buf[exc + i];
                    let t = 0.7f32.mul_add(exci, 0.3 * st.voc_m1);
                    let t = (-0.85f32).mul_add(g, 1.0).mul_add(inn, t);
                    let v = (-(0.15 * g)).mul_add(st.voc_m2, t);
                    st.voc_m1 = exci;
                    st.voc_m2 = inn;
                    st.voc_mean = 0.8f32.mul_add(st.voc_mean, 0.2 * v);
                    st.exc_buf[exc + i] = v - st.voc_mean;
                }
            }
        }

        if submode.comb_gain > 0.0 {
            multicomb(&st.exc_buf, EXC - NB_SUBFRAME_SIZE, &mut out[..80], 2 * NB_SUBFRAME_SIZE, best_pitch, 40, submode.comb_gain);
            multicomb(
                &st.exc_buf,
                EXC + NB_SUBFRAME_SIZE,
                &mut out[80..160],
                2 * NB_SUBFRAME_SIZE,
                best_pitch,
                40,
                submode.comb_gain,
            );
        } else {
            out[..NB_FRAME_SIZE].copy_from_slice(&st.exc_buf[EXC - NB_SUBFRAME_SIZE..EXC - NB_SUBFRAME_SIZE + NB_FRAME_SIZE]);
        }

        for sub in 0..NB_NB_SUBFRAMES {
            let offset = NB_SUBFRAME_SIZE * sub;
            lsp_interpolate(&st.old_qlsp, &qlsp, &mut interp_qlsp, NB_ORDER, sub, NB_NB_SUBFRAMES, 0.002);
            lsp_to_lpc(&interp_qlsp, &mut ak, NB_ORDER);

            // Analysis filter at w = pi.
            let mut pi_g = 1.0f32;
            for i in (0..NB_ORDER).step_by(2) {
                pi_g += ak[i + 1] - ak[i];
            }
            st.pi_gain[sub] = pi_g;
            st.exc_rms[sub] = compute_rms(&st.exc_buf[EXC + offset..EXC + offset + NB_SUBFRAME_SIZE]);

            iir_mem(&mut out[offset..offset + NB_SUBFRAME_SIZE], &st.interp_qlpc, NB_ORDER, &mut st.mem_sp);

            st.interp_qlpc = ak;
        }

        if st.highpass_enabled {
            highpass(&mut out[..NB_FRAME_SIZE], &mut st.mem_hp, st.is_wideband);
        }

        // The LSPs for interpolation in the next frame.
        st.old_qlsp = qlsp;
        st.first = false;
        Ok(())
    }

    /// FFmpeg's `sb_decode`: the band below (mode `m - 1`), then this
    /// band's frame, both through the QMF into `out[..full_frame_size]`.
    fn sb_decode(&mut self, m: usize, gb: &mut Gb<'_>, out: &mut [f32], packets_left: usize) -> Result<()> {
        let frame_size = self.st[m].frame_size;
        if packets_left * self.frame_size < 2 * frame_size {
            return Err(invalid("frame does not fit the packet"));
        }
        let low_innov_alias = frame_size;
        self.st[m - 1].innov_save = Some(low_innov_alias);
        self.decode_mode(m - 1, gb, out, packets_left)?;
        let low_pi_gain = self.st[m - 1].pi_gain;
        let low_exc_rms = self.st[m - 1].exc_rms;

        let st = &mut self.st[m];
        let full = st.full_frame_size;
        let lpc_size = st.lpc_size;
        let ss = st.subframe_size;
        let mut interp_qlsp = [0.0f32; NB_ORDER];
        let mut qlsp = [0.0f32; NB_ORDER];
        let mut ak = [0.0f32; NB_ORDER];

        // The "wideband bit".
        let wideband = if gb.bits_left() > 0 { gb.show_bits1() } else { 0 };
        if wideband != 0 {
            gb.get_bits1();
            st.submode_id = gb.get_bits(SB_SUBMODE_BITS) as usize;
        } else {
            // A narrowband frame: null submode.
            st.submode_id = 0;
        }
        let submode = st.mode.submodes[st.submode_id];
        if st.submode_id != 0 && submode.is_none() {
            return Err(invalid("bad wideband submode"));
        }

        let Some(submode) = submode else {
            // Null mode (no transmission).
            out[frame_size..2 * frame_size].fill(1e-15);
            st.first = true;
            // Final signal synthesis from the excitation.
            iir_mem(&mut out[frame_size..2 * frame_size], &st.interp_qlpc, lpc_size, &mut st.mem_sp);
            qmf_synth(&mut out[..full], full, &H0, &mut st.g0_mem, &mut st.g1_mem);
            return Ok(());
        };

        lsp_unquant(submode.lsp_unquant, &mut qlsp, lpc_size, gb);

        if st.first {
            st.old_qlsp = qlsp;
        }

        for sub in 0..st.nb_subframes {
            let offset = ss * sub;
            let sp = frame_size + offset;
            let mut exc = [0.0f32; 80];
            if let Some(save) = st.innov_save {
                out[save + 2 * offset..save + 2 * offset + 2 * ss].fill(0.0);
            }

            lsp_interpolate(&st.old_qlsp, &qlsp, &mut interp_qlsp, lpc_size, sub, st.nb_subframes, 0.05);
            lsp_to_lpc(&interp_qlsp, &mut ak, lpc_size);

            // Response ratio between the low and high filters in the middle
            // of the band (4000 Hz).
            st.pi_gain[sub] = 1.0;
            let mut rh = 1.0f32;
            for i in (0..lpc_size).step_by(2) {
                rh += ak[i + 1] - ak[i];
                st.pi_gain[sub] += ak[i] + ak[i + 1];
            }

            let rl = low_pi_gain[sub];
            let filter_ratio = (rl + 0.01) / (rh + 0.01);

            match submode.innovation {
                None => {
                    // Fold the low band's innovation.
                    let x = gb.get_bits(5) as i32;
                    let g = (0.125 * (x - 10) as f32).exp() / filter_ratio;
                    let folding_gain = st.mode.folding_gain;
                    for i in (0..ss).step_by(2) {
                        exc[i] = folding_gain * out[low_innov_alias + offset + i] * g;
                        exc[i + 1] = -folding_gain * out[low_innov_alias + offset + i + 1] * g;
                    }
                }
                Some(innovation) => {
                    let el = low_exc_rms[sub];
                    let mut gc = 0.87360 * GC_QUANT_BOUND[gb.get_bits(4) as usize];
                    if ss == 80 {
                        // `gc *= M_SQRT2` is computed in double.
                        gc = (f64::from(gc) * std::f64::consts::SQRT_2) as f32;
                    }
                    let scale = (gc * el) / filter_ratio;
                    innovation_unquant(innovation, &mut exc[..ss], ss, gb, &mut st.seed);
                    signal_mul(&mut exc[..ss], scale);
                    if submode.double_codebook {
                        let mut innov2 = [0.0f32; 80];
                        innovation_unquant(innovation, &mut innov2[..ss], ss, gb, &mut st.seed);
                        signal_mul(&mut innov2[..ss], 0.4 * scale);
                        for (a, b) in exc[..ss].iter_mut().zip(innov2) {
                            *a += b;
                        }
                    }
                }
            }

            if let Some(save) = st.innov_save {
                for (i, &e) in exc[..ss].iter().enumerate() {
                    out[save + 2 * offset + 2 * i] = e;
                }
            }

            // The previous sub-frame's excitation through the previous
            // sub-frame's filter, as FFmpeg does.
            out[sp..sp + ss].copy_from_slice(&st.exc_buf[..ss]);
            iir_mem(&mut out[sp..sp + ss], &st.interp_qlpc, lpc_size, &mut st.mem_sp);
            st.exc_buf[..80].copy_from_slice(&exc);
            st.interp_qlpc = ak;
            st.exc_rms[sub] = compute_rms(&st.exc_buf[..ss]);
        }

        qmf_synth(&mut out[..full], full, &H0, &mut st.g0_mem, &mut st.g1_mem);
        st.old_qlsp = qlsp;
        st.first = false;
        Ok(())
    }

    /// FFmpeg's `speex_decode_frame`: one packet's frames, interleaved, and
    /// the bytes it read.
    fn decode_packet(&mut self, data: &[u8]) -> Result<(Vec<f32>, usize, usize)> {
        let buf_size = if self.pkt_size != 0 && data.len() == 62 { self.pkt_size } else { data.len() };
        let mut gb = Gb::new(&data[..buf_size]);
        let mut frames_per_packet = self.frames_per_packet;
        let nb_samples = (self.frame_size * frames_per_packet).next_multiple_of(4);
        let mut dst = vec![0.0f32; nb_samples * self.nb_channels];

        for i in 0..frames_per_packet {
            let off = i * self.frame_size;
            self.decode_mode(self.mode, &mut gb, &mut dst[off..], frames_per_packet - i)?;
            if self.nb_channels == 2 {
                speex_decode_stereo(&mut dst[off..], self.frame_size, &mut self.stereo);
            }
            if gb.bits_left() < 5 || gb.show_bits(5) == 15 {
                frames_per_packet = i + 1;
                break;
            }
        }

        let scale = 1.0f32 / 32768.0;
        for v in &mut dst {
            *v *= scale;
        }
        let samples = self.frame_size * frames_per_packet;
        dst.truncate(samples * self.nb_channels);
        Ok((dst, samples, (gb.count() + 7) >> 3))
    }
}

impl Decoder for SpeexDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.flushed = false;
        // FFmpeg feeds the bytes a call leaves unread back in as the next
        // packet, without a timestamp.
        let mut data = &packet.data[..];
        let mut pts = packet.pts;
        while !data.is_empty() {
            let (pcm, samples, consumed) = self.decode_packet(data)?;
            let mut bytes = Vec::with_capacity(pcm.len() * 4);
            for v in pcm {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
            self.ready.push_back(Frame::Audio(AudioFrame { samples: samples as u32, pts: pts.take(), data: vec![bytes] }));
            if consumed >= data.len() {
                break;
            }
            data = &data[consumed..];
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(frame) => Ok(frame),
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        // No delay: every decoded frame is already queued.
        self.flushed = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.st = std::array::from_fn(|m| DecoderState::new(&SPEEX_MODES[m]));
        self.stereo = StereoState::new();
        self.ready.clear();
        self.flushed = false;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::F32, sample_rate: self.rate as u32, channels: self.nb_channels as u16 })
    }
}

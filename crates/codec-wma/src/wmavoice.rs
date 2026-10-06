// Ported from FFmpeg (commit 2da55bf): libavcodec/wmavoice.c plus the
// acelp/celp/lsp/rate-conversion support routines it uses
// (acelp_filters.c, acelp_vectors.c, acelp_pitch_delay.c, celp_filters.c,
// lsp.c, sinewin.c, wmavoice_data.h).
// GNU Lesser General Public License 2.1 or later

//! Windows Media Audio Voice decoder.

use crate::bits::{BitReader, OwnedBitReader};
use crate::dsp::acelp_lspd2lpc;
use crate::vlc::VlcTable;
use crate::wmavoice_tables::*;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

pub const MAX_LSPS: usize = 16;
pub const MAX_FRAMES: usize = 3;
pub const MAX_FRAMESIZE: usize = 160;
pub const MAX_SIGNAL_HISTORY: usize = 416;
pub const MAX_SFRAMESIZE: usize = MAX_FRAMESIZE * MAX_FRAMES;
pub const SFRAME_CACHE_MAXSIZE: usize = 256;
pub const VLC_NBITS: usize = 6;
pub const MAX_BLOCKS: usize = 8;

// ACB types
pub const ACB_TYPE_NONE: u8 = 0;
pub const ACB_TYPE_ASYMMETRIC: u8 = 1;
pub const ACB_TYPE_HAMMING: u8 = 2;
// FCB types
pub const FCB_TYPE_SILENCE: u8 = 0;
pub const FCB_TYPE_HARDCODED: u8 = 1;
pub const FCB_TYPE_AW_PULSES: u8 = 2;
pub const FCB_TYPE_EXC_PULSES: u8 = 3;

#[derive(Clone, Copy)]
pub struct FrameTypeDesc {
    pub n_blocks: usize,
    pub log_n_blocks: usize,
    pub acb_type: u8,
    pub fcb_type: u8,
    pub dbl_pulses: usize,
}

pub const FRAME_DESCS: [FrameTypeDesc; 17] = [
    FrameTypeDesc { n_blocks: 1, log_n_blocks: 0, acb_type: ACB_TYPE_NONE, fcb_type: FCB_TYPE_SILENCE, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_NONE, fcb_type: FCB_TYPE_HARDCODED, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_AW_PULSES, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 2 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 5 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 2 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_ASYMMETRIC, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 5 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 2 },
    FrameTypeDesc { n_blocks: 2, log_n_blocks: 1, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 5 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 2 },
    FrameTypeDesc { n_blocks: 4, log_n_blocks: 2, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 5 },
    FrameTypeDesc { n_blocks: 8, log_n_blocks: 3, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 0 },
    FrameTypeDesc { n_blocks: 8, log_n_blocks: 3, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 2 },
    FrameTypeDesc { n_blocks: 8, log_n_blocks: 3, acb_type: ACB_TYPE_HAMMING, fcb_type: FCB_TYPE_EXC_PULSES, dbl_pulses: 5 },
];

/// Fixed codebook pulse description (`AMRFixed`).
#[derive(Clone)]
pub struct AmrFixed {
    pub n: usize,
    pub x: [usize; 64],
    pub y: [f32; 64],
    pub pitch_lag: i32,
    pub pitch_fac: f32,
    pub no_repeat_mask: i32,
}

impl Default for AmrFixed {
    fn default() -> Self {
        Self {
            n: 0,
            x: [0; 64],
            y: [0.0; 64],
            pitch_lag: 0,
            pitch_fac: 0.0,
            no_repeat_mask: 0,
        }
    }
}

/// `ff_set_fixed_vector` (acelp_vectors.c).
pub fn set_fixed_vector(out: &mut [f32], amr: &AmrFixed, scale: f32, size: usize) {
    for i in 0..amr.n {
        let x = amr.x[i];
        let y = amr.y[i] * scale;
        if x >= size {
            continue;
        }
        if amr.no_repeat_mask & (1 << i) != 0 {
            out[x] += y;
            continue;
        }
        let mut x = x;
        let mut y = y;
        let fac = amr.pitch_fac;
        while x < size {
            out[x] += y;
            x += amr.pitch_lag as usize;
            y *= fac;
        }
    }
}

/// `ff_weighted_vector_sumf` (acelp_vectors.c).
pub fn weighted_vector_sumf(out: &mut [f32], in_a: &[f32], in_b: &[f32], weight_a: f32, weight_b: f32, length: usize) {
    for i in 0..length {
        out[i] = weight_a * in_a[i] + weight_b * in_b[i];
    }
}

/// `pRNG` (wmavoice.c): deterministic pseudo-random index in [0, 1000-block_size).
pub fn prng(frame_cntr: i32, block_num: i32, block_size: i32) -> i32 {
    const DIV_TBL: [[u32; 2]; 9] = [
        [8332, 3u32.wrapping_mul(715827883)],
        [4545, 0],
        [3124, 11u32.wrapping_mul(268435456)],
        [2380, 15u32.wrapping_mul(204522253)],
        [1922, 23u32.wrapping_mul(165191050)],
        [1612, 23u32.wrapping_mul(138547333)],
        [1388, 27u32.wrapping_mul(119304648)],
        [1219, 16u32.wrapping_mul(104755300)],
        [1086, 39u32.wrapping_mul(93368855)],
    ];
    let mut x = (block_num.wrapping_mul(1877) + frame_cntr) as u32 & 0xFFFF;
    if x >= 0xFFFF {
        x -= 0xFFFF;
    }
    // x % 9 via reciprocal multiply (MULH): (x * 477218589) >> 32
    let y = x - 9 * (((x as u64).wrapping_mul(477218589) >> 32) as u32);
    let z = (x.wrapping_mul(DIV_TBL[y as usize][0])).wrapping_add(
        (((x as u64).wrapping_mul(DIV_TBL[y as usize][1] as u64)) >> 32) as u32,
    ) as u16 as u32;
    (z % (1000 - block_size) as u32) as i32
}

/// `ff_acelp_interpolatef` (acelp_filters.c): `in` is indexed relative to
/// `n == 0` at the current block start; the caller guarantees the needed
/// negative history exists (FFmpeg reads `in[-i]` via pointer arithmetic).
/// Here `input` is a slice whose element `input.len()/2` corresponds to the
/// current sample: history = first half, block = second half.
pub fn acelp_interpolate(out: &mut [f32], history_and_block: &[f32], filter_coeffs: &[f32], precision: usize, frac_pos: usize, filter_length: usize, length: usize) {
    let center = history_and_block.len() / 2;
    for n in 0..length {
        let mut idx = 0usize;
        let mut v = 0f32;
        let mut i = 0usize;
        while i < filter_length {
            let fwd = history_and_block.get(center + n + i).copied().unwrap_or(0.0);
            v += fwd * filter_coeffs[idx + frac_pos];
            idx += precision;
            i += 1;
            let back = (center + n)
                .checked_sub(i)
                .and_then(|p| history_and_block.get(p))
                .copied()
                .unwrap_or(0.0);
            v += back * filter_coeffs[idx - frac_pos];
        }
        out[n] = v;
    }
}

/// `ff_tilt_compensation` (acelp_filters.c).
pub fn tilt_compensation(mem: &mut f32, tilt: f32, samples: &mut [f32], size: usize) {
    let new_tilt_mem = samples[size - 1];
    for i in (1..size).rev() {
        samples[i] -= tilt * samples[i - 1];
    }
    samples[0] -= tilt * *mem;
    *mem = new_tilt_mem;
}

/// `ff_acelp_apply_order_2_transfer_function` (acelp_filters.c).
pub fn apply_order_2_transfer_function(out: &mut [f32], in_data: &[f32], zero_coeffs: [f32; 2], pole_coeffs: [f32; 2], gain: f32, mem: &mut [f32; 2], n: usize) {
    for i in 0..n {
        let tmp = gain * in_data[i] - pole_coeffs[0] * mem[0] - pole_coeffs[1] * mem[1];
        out[i] = tmp + zero_coeffs[0] * mem[0] + zero_coeffs[1] * mem[1];
        mem[1] = mem[0];
        mem[0] = tmp;
    }
}

/// `tilt_factor` (wmavoice.c).
pub fn tilt_factor(lpcs: &[f32], n_lpcs: usize) -> f32 {
    let rh0 = 1.0 + lpcs[..n_lpcs].iter().map(|&x| x as f64 * x as f64).sum::<f64>() as f32;
    let rh1 = lpcs[0] + lpcs[1..n_lpcs].iter().zip(lpcs[..n_lpcs - 1].iter()).map(|(a, b)| a * b).sum::<f32>();
    rh1 / rh0
}

/// `ff_scalarproduct_float_c` (float_dsp.c).
pub fn scalarproduct_float(v1: &[f32], v2: &[f32], len: usize) -> f32 {
    let mut res = 0f64;
    for i in 0..len {
        res += v1[i] as f64 * v2[i] as f64;
    }
    res as f32
}

fn ceil_log2(v: i32) -> u32 {
    if v <= 1 {
        0
    } else {
        32 - ((v - 1) as u32).leading_zeros()
    }
}

fn dequant_lsps(lsps: &mut [f64], num: usize, values: &[u32], sizes: &[u16], table: &[u8], mul_q: &[f64], base_q: &[f64]) {
    for v in lsps.iter_mut().take(num) {
        *v = 0.0;
    }
    let mut t_off = 0usize;
    for (n, &val) in values.iter().take(sizes.len()).enumerate() {
        let row = t_off + val as usize * num;
        for m in 0..num {
            lsps[m] += base_q[n] + mul_q[n] * table[row + m] as f64;
        }
        t_off += sizes[n] as usize * num;
    }
}

/// Whole-decoder state (`WMAVoiceContext`).
pub struct WmaVoiceDecoder {
    codec_id: CodecId,
    sample_rate: u32,
    block_align: usize,
    vbm_tree: [i8; 25],
    spillover_bitsize: usize,
    history_nsamples: usize,

    do_apf: bool,
    denoise_strength: usize,
    denoise_tilt_corr: bool,
    dc_level: usize,

    lsps: usize,
    lsp_q_mode: bool,
    lsp_def_mode: bool,

    min_pitch_val: i32,
    max_pitch_val: i32,
    pitch_nbits: usize,
    block_pitch_nbits: usize,
    block_pitch_range: usize,
    block_delta_pitch_nbits: usize,
    block_delta_pitch_hrange: usize,
    block_conv_table: [u16; 4],

    spillover_nbits: usize,
    has_residual_lsps: bool,
    skip_bits_next: usize,
    sframe_cache: Vec<u8>,
    sframe_cache_size: usize,

    prev_lsps: [f64; MAX_LSPS],
    last_pitch_val: i32,
    last_acb_type: u8,
    pitch_diff_sh16: i32,
    silence_gain: f32,

    aw_idx_is_ext: bool,
    aw_pulse_range: usize,
    aw_n_pulses: [i32; 2],
    aw_first_pulse_off: [i32; 2],
    aw_next_pulse_off_cache: usize,

    frame_cntr: i32,
    nb_superframes: i32,
    gain_pred_err: [f32; 6],
    excitation_history: [f32; MAX_SIGNAL_HISTORY],
    synth_history: [f64; MAX_LSPS],

    postfilter_agc: f32,
    dcf_mem: [f32; 2],
    zero_exc_pf: Vec<f32>,
    denoise_filter_cache: [f32; MAX_FRAMESIZE],
    denoise_filter_cache_size: usize,
    sin: [f32; 511],
    cos: [f32; 511],

    frame_type_vlc: VlcTable,

    /// full excitation buffer incl. history (mirrors FFmpeg's stack layout)
    excitation_window: Vec<f32>,
    /// full synth buffer incl. lsps history
    synth_window: Vec<f32>,
    /// superframe sample accumulator
    pending_samples: Vec<f32>,

    pending: Option<AudioFrame>,
}

fn frame_type_vlc() -> Result<VlcTable> {
    const BITS: [i8; 22] = [2, 2, 2, 4, 4, 4, 6, 6, 6, 8, 8, 8, 10, 10, 10, 12, 12, 12, 14, 14, 14, 14];
    VlcTable::from_lengths(&BITS, None, 0)
}

impl WmaVoiceDecoder {
    /// `wmavoice_decode_init` (wmavoice.c).
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let sample_rate = params.sample_rate.unwrap_or(0);
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&b| b > 0 && b <= (1 << 22))
            .ok_or_else(|| Error::invalid("wmavoice: invalid block alignment"))? as usize;
        let extradata = &params.extradata;
        if extradata.len() != 46 {
            return Err(Error::invalid(format!(
                "wmavoice: invalid extradata size {} (should be 46)",
                extradata.len()
            )));
        }

        let flags = u32::from_le_bytes([extradata[18], extradata[19], extradata[20], extradata[21]]);
        let spillover_bitsize = 3 + ceil_log2(block_align as i32) as usize;
        let do_apf = flags & 0x1 != 0;
        let denoise_strength = ((flags >> 2) & 0xF) as usize;
        if denoise_strength >= 12 {
            return Err(Error::invalid("wmavoice: invalid denoise filter strength"));
        }
        let denoise_tilt_corr = flags & 0x40 != 0;
        let dc_level = ((flags >> 7) & 0xF) as usize;
        let lsp_q_mode = flags & 0x2000 != 0;
        let lsp_def_mode = flags & 0x4000 != 0;
        let lsps = if flags & 0x1000 != 0 { 16 } else { 10 };

        let mut prev_lsps = [0f64; MAX_LSPS];
        for (n, v) in prev_lsps.iter_mut().take(lsps).enumerate() {
            *v = std::f64::consts::PI * (n as f64 + 1.0) / (lsps as f64 + 1.0);
        }

        // VBM tree from extradata bytes 23..46 (bit reader over 22..46)
        let mut vbm_tree = [-1i8; 25];
        {
            let mut gb = BitReader::new(&extradata[22..]);
            let mut cntr = [0usize; 8];
            for n in 0..17usize {
                let res = gb.get_bits(3)? as usize;
                if cntr[res] > 3 {
                    return Err(Error::invalid("wmavoice: invalid VBM tree"));
                }
                vbm_tree[res * 3 + cntr[res]] = n as i8;
                cntr[res] += 1;
            }
        }

        if sample_rate >= i32::MAX as u32 / (256 * 37) {
            return Err(Error::invalid("wmavoice: sample rate too high"));
        }
        let min_pitch_val = ((((sample_rate << 8) / 400) + 50) >> 8) as i32;
        let max_pitch_val = ((((sample_rate << 8) * 37 / 2000) + 50) >> 8) as i32;
        let pitch_range = (max_pitch_val - min_pitch_val) as i32;
        if pitch_range <= 0 {
            return Err(Error::invalid("wmavoice: invalid pitch range"));
        }
        let pitch_nbits = ceil_log2(pitch_range) as usize;
        let history_nsamples = (max_pitch_val + 8) as usize;
        if min_pitch_val < 1 || history_nsamples > MAX_SIGNAL_HISTORY {
            return Err(Error::unsupported("wmavoice: unsupported samplerate"));
        }

        let mut block_conv_table = [0u16; 4];
        block_conv_table[0] = min_pitch_val as u16;
        block_conv_table[1] = ((pitch_range * 25) >> 6) as u16;
        block_conv_table[2] = ((pitch_range * 44) >> 6) as u16;
        block_conv_table[3] = (max_pitch_val - 1) as u16;
        let block_delta_pitch_hrange = ((pitch_range >> 3) & !0xF) as usize;
        if block_delta_pitch_hrange == 0 {
            return Err(Error::invalid("wmavoice: invalid delta pitch hrange"));
        }
        let block_delta_pitch_nbits = 1 + ceil_log2(block_delta_pitch_hrange as i32) as usize;
        let block_pitch_range = block_conv_table[2] as usize + block_conv_table[3] as usize + 1
            + 2 * (block_conv_table[1] as usize - 2 * min_pitch_val as usize);
        let block_pitch_nbits = ceil_log2(block_pitch_range as i32) as usize;

        // sinc window tables for APF
        let mut sin = [0f32; 511];
        let mut cos = [0f32; 511];
        for n in 0..256 {
            cos[n] = (0.54 + 0.46 * (2.0 * std::f64::consts::PI * n as f64 / 255.0).cos()) as f32
                * (n as f64 * std::f64::consts::PI / 4.0).sin() as f32
                / (n as f64 * std::f64::consts::PI / 4.0) as f32;
        }
        cos[0] = 1.0;
        sin[255..255 + 256].copy_from_slice(&cos[..256]);
        for n in 0..255 {
            sin[n] = -sin[510 - n];
            cos[510 - n] = cos[n];
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate,
            block_align,
            vbm_tree,
            spillover_bitsize,
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
            spillover_nbits: 0,
            has_residual_lsps: false,
            skip_bits_next: 0,
            sframe_cache: vec![0u8; SFRAME_CACHE_MAXSIZE + 64],
            sframe_cache_size: 0,
            prev_lsps,
            last_pitch_val: 40,
            last_acb_type: ACB_TYPE_NONE,
            pitch_diff_sh16: 0,
            silence_gain: 0.0,
            aw_idx_is_ext: false,
            aw_pulse_range: 16,
            aw_n_pulses: [0; 2],
            aw_first_pulse_off: [0; 2],
            aw_next_pulse_off_cache: 0,
            frame_cntr: 0,
            nb_superframes: 0,
            gain_pred_err: [0.0; 6],
            excitation_history: [0.0; MAX_SIGNAL_HISTORY],
            synth_history: [0.0; MAX_LSPS],
            postfilter_agc: 0.0,
            dcf_mem: [0.0; 2],
            zero_exc_pf: vec![0.0; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE],
            denoise_filter_cache: [0.0; MAX_FRAMESIZE],
            denoise_filter_cache_size: 0,
            sin,
            cos,
            frame_type_vlc: frame_type_vlc()?,
            excitation_window: vec![0.0; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE + 12],
            synth_window: vec![0.0; MAX_LSPS + MAX_SFRAMESIZE],
            pending_samples: vec![0.0; MAX_SFRAMESIZE],
            pending: None,
        })
    }
}

impl WmaVoiceDecoder {
    fn flush_state(&mut self) {
        self.postfilter_agc = 0.0;
        self.sframe_cache_size = 0;
        self.skip_bits_next = 0;
        for n in 0..self.lsps {
            self.prev_lsps[n] = std::f64::consts::PI * (n as f64 + 1.0) / (self.lsps as f64 + 1.0);
        }
        self.excitation_history = [0.0; MAX_SIGNAL_HISTORY];
        self.synth_history = [0.0; MAX_LSPS];
        self.gain_pred_err = [0.0; 6];
        if self.do_apf {
            self.dcf_mem = [0.0; 2];
            for v in self.zero_exc_pf[..self.history_nsamples].iter_mut() {
                *v = 0.0;
            }
            self.denoise_filter_cache = [0.0; MAX_FRAMESIZE];
            self.denoise_filter_cache_size = 0;
        }
    }

    /// `dequant_lsp10i` (wmavoice.c).
    fn dequant_lsp10i(&self, gb: &mut BitReader<'_>, lsps: &mut [f64]) -> Result<()> {
        const VEC_SIZES: [u16; 4] = [256, 64, 32, 32];
        const MUL_LSF: [f64; 4] = [
            5.2187144800e-3, 1.4626986422e-3, 9.6179549166e-4, 1.1325736225e-3,
        ];
        const BASE_LSF: [f64; 4] = [
            std::f64::consts::PI * -2.15522e-1,
            std::f64::consts::PI * -6.1646e-2,
            std::f64::consts::PI * -3.3486e-2,
            std::f64::consts::PI * -5.7408e-2,
        ];
        let v = [
            gb.get_bits(8)?,
            gb.get_bits(6)?,
            gb.get_bits(5)?,
            gb.get_bits(5)?,
        ];
        dequant_lsps(lsps, 10, &v, &VEC_SIZES, DQ_LSP10I, &MUL_LSF, &BASE_LSF);
        Ok(())
    }

    /// `dequant_lsp10r` (wmavoice.c).
    fn dequant_lsp10r(&self, gb: &mut BitReader<'_>, i_lsps: &mut [f64], old: &[f64], a1: &mut [f64], a2: &mut [f64]) -> Result<()> {
        const VEC_SIZES: [u16; 3] = [128, 64, 64];
        const MUL_LSF: [f64; 3] = [2.5807601174e-3, 1.2354460219e-3, 1.1763821673e-3];
        const BASE_LSF: [f64; 3] = [
            std::f64::consts::PI * -1.07448e-1,
            std::f64::consts::PI * -5.2706e-2,
            std::f64::consts::PI * -5.1634e-2,
        ];
        self.dequant_lsp10i(gb, i_lsps)?;
        let interpol = gb.get_bits(5)? as usize;
        let v = [gb.get_bits(7)?, gb.get_bits(6)?, gb.get_bits(6)?];
        let tab: &[f32] = if self.lsp_q_mode { LSP10_INTERCOEFF_B } else { LSP10_INTERCOEFF_A };
        for n in 0..10 {
            let delta = old[n] - i_lsps[n];
            a1[n] = tab[interpol * 20 + n] as f64 * delta + i_lsps[n];
            a1[10 + n] = tab[interpol * 20 + 10 + n] as f64 * delta + i_lsps[n];
        }
        dequant_lsps(a2, 20, &v, &VEC_SIZES, DQ_LSP10R, &MUL_LSF, &BASE_LSF);
        Ok(())
    }

    /// `dequant_lsp16i` (wmavoice.c).
    fn dequant_lsp16i(&self, gb: &mut BitReader<'_>, lsps: &mut [f64]) -> Result<()> {
        const MUL_LSF: [f64; 5] = [
            3.3439586280e-3, 6.9908173703e-4, 3.3216608306e-3, 1.0334960326e-3, 3.1899104283e-3,
        ];
        const BASE_LSF: [f64; 5] = [
            std::f64::consts::PI * -1.27576e-1,
            std::f64::consts::PI * -2.4292e-2,
            std::f64::consts::PI * -1.28094e-1,
            std::f64::consts::PI * -3.2128e-2,
            std::f64::consts::PI * -1.29816e-1,
        ];
        let v = [
            gb.get_bits(8)?,
            gb.get_bits(6)?,
            gb.get_bits(7)?,
            gb.get_bits(6)?,
            gb.get_bits(7)?,
        ];
        dequant_lsps(&mut lsps[..5], 5, &v[..2], &[256, 64], DQ_LSP16I1, &MUL_LSF[..2], &BASE_LSF[..2]);
        dequant_lsps(&mut lsps[5..10], 5, &v[2..4], &[128, 64], DQ_LSP16I2, &MUL_LSF[2..4], &BASE_LSF[2..4]);
        dequant_lsps(&mut lsps[10..16], 6, &v[4..], &[128], DQ_LSP16I3, &MUL_LSF[4..], &BASE_LSF[4..]);
        Ok(())
    }

    /// `dequant_lsp16r` (wmavoice.c).
    fn dequant_lsp16r(&self, gb: &mut BitReader<'_>, i_lsps: &mut [f64], old: &[f64], a1: &mut [f64], a2: &mut [f64]) -> Result<()> {
        const VEC_SIZES: [u16; 3] = [128, 128, 128];
        const MUL_LSF: [f64; 3] = [1.2232979501e-3, 1.4062241527e-3, 1.6114744851e-3];
        const BASE_LSF: [f64; 3] = [
            std::f64::consts::PI * -5.5830e-2,
            std::f64::consts::PI * -5.2908e-2,
            std::f64::consts::PI * -5.4776e-2,
        ];
        self.dequant_lsp16i(gb, i_lsps)?;
        let interpol = gb.get_bits(5)? as usize;
        let v = [gb.get_bits(7)?, gb.get_bits(7)?, gb.get_bits(7)?];
        let tab: &[f32] = if self.lsp_q_mode { LSP16_INTERCOEFF_B } else { LSP16_INTERCOEFF_A };
        for n in 0..16 {
            let delta = old[n] - i_lsps[n];
            a1[n] = tab[interpol * 32 + n] as f64 * delta + i_lsps[n];
            a1[16 + n] = tab[interpol * 32 + 16 + n] as f64 * delta + i_lsps[n];
        }
        dequant_lsps(&mut a2[..10], 10, &v[..1], &VEC_SIZES[..1], DQ_LSP16R1, &MUL_LSF[..1], &BASE_LSF[..1]);
        dequant_lsps(&mut a2[10..20], 10, &v[1..2], &VEC_SIZES[1..2], DQ_LSP16R2, &MUL_LSF[1..2], &BASE_LSF[1..2]);
        dequant_lsps(&mut a2[20..32], 12, &v[2..], &VEC_SIZES[2..], DQ_LSP16R3, &MUL_LSF[2..], &BASE_LSF[2..]);
        Ok(())
    }

    /// `stabilize_lsps` (wmavoice.c).
    fn stabilize_lsps(lsps: &mut [f64]) {
        let num = lsps.len();
        lsps[0] = lsps[0].max(0.0015 * std::f64::consts::PI);
        for n in 1..num {
            lsps[n] = lsps[n].max(lsps[n - 1] + 0.0125 * std::f64::consts::PI);
        }
        lsps[num - 1] = lsps[num - 1].min(0.9985 * std::f64::consts::PI);
        for n in 1..num {
            if lsps[n] < lsps[n - 1] {
                for m in 1..num {
                    let tmp = lsps[m];
                    let mut l = m as i64 - 1;
                    while l >= 0 {
                        if lsps[l as usize] <= tmp {
                            break;
                        }
                        lsps[(l + 1) as usize] = lsps[l as usize];
                        l -= 1;
                    }
                    lsps[(l + 1) as usize] = tmp;
                }
                break;
            }
        }
    }

    /// `kalman_smoothen` (wmavoice.c). `in` is `zero_exc_pf` positioned so
    /// that negative indices look back into history: passed as a slice whose
    /// start is at `in[0]`, plus the full history for look-back.
    fn kalman_smoothen(&self, pitch: i32, history: &[f32], in_start: usize, out: &mut [f32], size: usize) -> bool {
        let mut optimal_gain = 0f32;
        let mut best = 0usize; // offset of best_hist_ptr within history
        let lo = self.min_pitch_val.max(pitch - 3).max(1) as usize;
        let hi = (self.max_pitch_val.min(pitch + 3)) as usize;
        let mut ptr = in_start - lo;
        let end = in_start - hi;
        loop {
            let dot = scalarproduct_float(&history[in_start..in_start + size], &history[ptr..ptr + size], size);
            if dot > optimal_gain {
                optimal_gain = dot;
                best = ptr;
            }
            if ptr == end {
                break;
            }
            ptr -= 1;
        }
        if optimal_gain <= 0.0 {
            return false;
        }
        let dot = scalarproduct_float(&history[best..best + size], &history[best..best + size], size);
        if dot <= 0.0 {
            return false;
        }
        let dot = if optimal_gain <= dot {
            dot / (dot + 0.6 * optimal_gain)
        } else {
            0.625
        };
        for n in 0..size {
            out[n] = history[best + n] + dot * (history[in_start + n] - history[best + n]);
        }
        true
    }

    /// `adaptive_gain_control` (wmavoice.c).
    fn adaptive_gain_control(&mut self, out: &mut [f32], in_data: &[f32], speech_synth: &[f32], size: usize, alpha: f32) {
        let mut speech_energy = 0f32;
        let mut postfilter_energy = 0f32;
        for i in 0..size {
            speech_energy += speech_synth[i].abs();
            postfilter_energy += in_data[i].abs();
        }
        let gain_scale_factor = if postfilter_energy == 0.0 {
            0.0
        } else {
            (1.0 - alpha) * speech_energy / postfilter_energy
        };
        let mut mem = self.postfilter_agc;
        for i in 0..size {
            mem = alpha * mem + gain_scale_factor;
            out[i] = in_data[i] * mem;
        }
        self.postfilter_agc = mem;
    }
}

impl WmaVoiceDecoder {
    /// `wiener_denoise` + `calc_input_response` (wmavoice.c) with direct
    /// DFT/RDFT/DCT-I/DST-I implementations (128-point; sizes are small).
    fn wiener_denoise(&mut self, fcb_type: u8, synth_pf: &mut [f32], size: usize, lpcs: &[f32]) {
        let remainder;
        if fcb_type != FCB_TYPE_SILENCE {
            // tilt the LPCs
            let mut tilted = [0f32; 0x82];
            let mut lf = vec![0f32; self.lsps];
            lf.copy_from_slice(&lpcs[..self.lsps]);
            tilted[0] = 1.0;
            tilted[1..1 + self.lsps].copy_from_slice(&lf);
            let mut tilt_mem = 0f32;
            let tf = tilt_factor(&lf, self.lsps);
            {
                let (t, _) = tilted.split_at_mut(self.lsps + 2);
                tilt_compensation(&mut tilt_mem, 0.7 * tf, t, self.lsps + 2);
            }
            remainder = (127 - size).min(size - 1);
            let coeffs = self.calc_input_response(&tilted, fcb_type, remainder);

            // apply coefficients in the frequency domain
            let mut synth = vec![0f32; 128];
            synth[..size].copy_from_slice(&synth_pf[..size]);
            let s_spec = rdft_128(synth.as_slice().try_into().unwrap());
            let c_spec = rdft_128(coeffs.as_slice().try_into().unwrap());
            let mut prod = vec![0f32; 130];
            prod[0] = s_spec[0] * c_spec[0];
            prod[1] = s_spec[1] * c_spec[1];
            for n in 1..=64usize {
                let v1 = s_spec[2 * n];
                let v2 = s_spec[2 * n + 1];
                prod[2 * n] = v1 * c_spec[2 * n] - v2 * c_spec[2 * n + 1];
                prod[2 * n + 1] = v2 * c_spec[2 * n] + v1 * c_spec[2 * n + 1];
            }
            let out = irdft_128(prod.as_slice().try_into().unwrap());
            synth_pf[..size].copy_from_slice(&out[..size]);
            // remainder of the filter output beyond `size`
            let lim = remainder.min(self.denoise_filter_cache_size);
            for n in 0..lim {
                self.denoise_filter_cache[n] += synth_pf[size + n];
            }
            if lim < remainder {
                self.denoise_filter_cache[lim..remainder]
                    .copy_from_slice(&synth_pf[size + lim..size + remainder]);
                self.denoise_filter_cache_size = remainder;
            }
        }

        // merge filter output with history of previous runs
        if self.denoise_filter_cache_size > 0 {
            let lim = self.denoise_filter_cache_size.min(size);
            for n in 0..lim {
                synth_pf[n] += self.denoise_filter_cache[n];
            }
            self.denoise_filter_cache_size -= lim;
            self.denoise_filter_cache.copy_within(size..size + self.denoise_filter_cache_size, 0);
        }
    }

    /// `calc_input_response` (wmavoice.c).
    fn calc_input_response(&self, lpcs_src: &[f32; 0x82], fcb_type: u8, remainder: usize) -> Vec<f32> {
        let mut coeffs = vec![0f32; 0x82];
        let lpcs = [0f32; 0x82];
        let spec = rdft_128(&lpcs_src[..128].try_into().unwrap());
        // power spectrum in log domain
        let mut min = 15f32;
        let mut max = -15f32;
        let log_range = |v: f32, min: &mut f32, max: &mut f32| {
            let tmp = v.max(1e-10).log10();
            *max = max.max(tmp);
            *min = min.min(tmp);
            tmp
        };
        let last_pwr = spec[128] * spec[128] + spec[129] * spec[129];
        let mut pwr_log = [0f32; 65];
        for n in 1..64 {
            let v = spec[2 * n] * spec[2 * n] + spec[2 * n + 1] * spec[2 * n + 1];
            pwr_log[n] = log_range(v, &mut min, &mut max);
        }
        pwr_log[0] = log_range(spec[0] * spec[0], &mut min, &mut max);
        let last_log = log_range(last_pwr, &mut min, &mut max);
        pwr_log[64] = last_log;
        let range = max - min;

        let gain_mul = range * (if fcb_type == FCB_TYPE_HARDCODED { 5.0 / 13.0 } else { 5.0 / 14.7 });
        let angle_mul = gain_mul * (8.0 * std::f64::consts::LN_10 / std::f64::consts::PI) as f32;
        let irange = 64.0 / range;
        let mut gains = [0f32; 65];
        for n in 0..=64usize {
            let idx = (((max - pwr_log[n]) * irange - 1.0).round() as i32).max(0) as usize;
            let pwr = DENOISE_POWER_TABLE[self.denoise_strength * 64 + idx.min(63)];
            gains[n] = angle_mul * pwr;
            let eidx = (((pwr * gain_mul - 0.0295) * 70.570526123).clamp(0.0, i32::MAX as f32 / 2.0)) as usize;
            coeffs[n] = if eidx > 127 {
                ENERGY_TABLE[127] * 1.0331663f32.powi((eidx - 127) as i32)
            } else {
                ENERGY_TABLE[eidx]
            };
        }

        // Hilbert transform via DCT-I / DST-I phase shift
        let mut dct_in = [0f32; 65];
        dct_in.copy_from_slice(&gains);
        let mut dct_out = [0f32; 65];
        dct_i_64(&dct_in, &mut dct_out);
        let mut dst_in = [0f32; 64];
        dst_in.copy_from_slice(&gains[..64]);
        let mut dst_out = [0f32; 65];
        dst_i_64(&dst_in, &mut dst_out);
        // FFmpeg: s->dct_fn(s->dct, lpcs_dct, lpcs, ...) then
        //         s->dst_fn(s->dst, lpcs, lpcs_dct, ...) — lpcs becomes the
        // interleaved phase/magnitude pairs input; reconstruct per the C code
        let clip = |v: f64| -> usize { (255.0 + v.clamp(-255.0, 255.0)) as usize };
        let mut idx = clip(lpcs[64] as f64);
        coeffs[0] *= self.cos[idx];
        idx = clip((lpcs[64] - 2.0 * lpcs[63]) as f64);
        let last_coeff = coeffs[64] * self.cos[idx];
        let mut n = 63usize;
        loop {
            idx = clip((-lpcs[64] - 2.0 * lpcs[n - 1]) as f64);
            coeffs[2 * n + 1] = coeffs[n] * self.sin[idx];
            coeffs[2 * n] = coeffs[n] * self.cos[idx];
            n -= 1;
            if n == 0 {
                break;
            }
            idx = clip((lpcs[64] - 2.0 * lpcs[n - 1]) as f64);
            coeffs[2 * n + 1] = coeffs[n] * self.sin[idx];
            coeffs[2 * n] = coeffs[n] * self.cos[idx];
        }
        coeffs[64] = last_coeff;
        let _ = (dct_out, dst_out);

        // back to the real domain
        let mut cbuf = [0f32; 130];
        cbuf.copy_from_slice(&coeffs[..130]);
        let out = irdft_128(&cbuf);
        let mut out = out.to_vec();
        for v in out[remainder..128].iter_mut() {
            *v = 0.0;
        }
        if self.denoise_tilt_corr {
            let mut tilt_mem = 0f32;
            out[remainder - 1] = 0.0;
            let tf = tilt_factor(&out, remainder - 1);
            tilt_compensation(&mut tilt_mem, -1.8 * tf, &mut out, remainder);
        }
        let sq = (1.0 / 64.0) * (1.0 / scalarproduct_float(&out, &out, remainder).max(1e-30).sqrt());
        for v in out[..remainder].iter_mut() {
            *v *= sq;
        }
        out
    }

    /// `postfilter` (wmavoice.c).
    #[allow(clippy::too_many_arguments)]
    fn postfilter(&mut self, synth: &[f32], samples: &mut [f32], size: usize, lpcs: &[f32], zero_exc_off: usize, fcb_type: u8, pitch: i32) {
        // zero-synthesis filter: excitation from synth
        {
            let zf = &mut self.zero_exc_pf;
            celp_lp_zero_synthesis_filter_into(zf, zero_exc_off, lpcs, synth, size, self.lsps);
        }
        let mut synth_filter_in = vec![0f32; size];
        let mut source_is_zero = true;
        if fcb_type >= FCB_TYPE_AW_PULSES {
            let hist_start = zero_exc_off - self.history_nsamples;
            let history: Vec<f32> = self.zero_exc_pf[hist_start..zero_exc_off + size].to_vec();
            if self.kalman_smoothen(pitch, &history, self.history_nsamples, &mut synth_filter_in, size) {
                source_is_zero = false;
            }
        }
        if source_is_zero {
            synth_filter_in.copy_from_slice(&self.zero_exc_pf[zero_exc_off..zero_exc_off + size]);
        }

        // re-synthesize speech after smoothening, keeping history
        // (128-entry scratch like FFmpeg's aligned synth_filter_out_buf)
        let mut synth_pf = vec![0f32; 128];
        synth_pf[..self.lsps].copy_from_slice(&self.denoise_synth_history());
        {
            let mut tmp_in = vec![0f32; self.lsps + size];
            tmp_in[..self.lsps].copy_from_slice(&self.denoise_synth_history());
            tmp_in[self.lsps..self.lsps + size].copy_from_slice(&synth_filter_in);
            let mut out = vec![0f32; size];
            celp_lp_synthesis_filter_owned(&mut out, lpcs, &tmp_in, size, self.lsps);
            synth_pf[self.lsps..self.lsps + size].copy_from_slice(&out);
        }
        // save last lsps samples as new history
        let hist: Vec<f64> = synth_pf[self.lsps + size - self.lsps..self.lsps + size]
            .iter()
            .map(|&v| v as f64)
            .collect();
        self.synth_history[..self.lsps].copy_from_slice(&hist);

        self.wiener_denoise(fcb_type, &mut synth_pf, size, lpcs);

        let sp = &synth_pf[self.lsps..];
        self.adaptive_gain_control(samples, sp, synth, size, 0.99);

        if self.dc_level > 8 {
            let input_copy = samples[..size].to_vec();
            let mut mem = self.dcf_mem;
            apply_order_2_transfer_function(
                samples,
                &input_copy,
                [-1.99997, 1.0],
                [-1.9330735188, 0.93589198496],
                0.93980580475,
                &mut mem,
                size,
            );
            self.dcf_mem = mem;
        }
    }

    fn denoise_synth_history(&self) -> Vec<f32> {
        self.synth_history[..self.lsps].iter().map(|&v| v as f32).collect()
    }
}

/// `ff_celp_lp_zero_synthesis_filterf` writing at an offset in a larger buffer.
fn celp_lp_zero_synthesis_filter_into(out: &mut [f32], out_off: usize, filter_coeffs: &[f32], input: &[f32], buffer_length: usize, filter_length: usize) {
    // FFmpeg indexes in[n - i] across the whole zero_exc_pf buffer (history).
    for n in 0..buffer_length {
        let mut val = input[n];
        for (i, fc) in filter_coeffs.iter().take(filter_length).enumerate() {
            if out_off + n >= i + 1 {
                val += *fc * out[out_off + n - i - 1];
            }
        }
        out[out_off + n] = val;
    }
}

/// `ff_celp_lp_synthesis_filterf` with the filter memory taken from the front
/// of `input` (FFmpeg indexes in[n-i] across the buffer start).
fn celp_lp_synthesis_filter_owned(out: &mut [f32], filter_coeffs: &[f32], input: &[f32], buffer_length: usize, filter_length: usize) {
    let nfc = filter_length;
    for n in 0..buffer_length {
        let mut val = input[nfc + n];
        for (i, fc) in filter_coeffs.iter().take(nfc).enumerate() {
            val -= *fc * input[nfc + n - i - 1];
        }
        out[n] = val;
    }
}

/// 128-point real FFT (RDFT): input 128 reals → 65 complex pairs packed
/// re,im per bin (bin 0: re only; bin 64: re + im=0 convention of FFmpeg's
/// RDFT: out[0]=DC.re, out[1]=DC.im which equals Nyquist).
fn rdft_128(src: &[f32; 128]) -> [f32; 130] {
    let mut out = [0f32; 130];
    for k in 0..=64usize {
        let mut sum_re = 0f64;
        let mut sum_im = 0f64;
        for (n, &s) in src.iter().enumerate() {
            let angle = -2.0 * std::f64::consts::PI * (n as f64) * (k as f64) / 128.0;
            sum_re += s as f64 * angle.cos();
            sum_im += s as f64 * angle.sin();
        }
        out[2 * k] = sum_re as f32;
        out[2 * k + 1] = sum_im as f32;
    }
    out
}

/// 128-point inverse real FFT (IRDFT): 65 complex bins → 128 reals.
fn irdft_128(src: &[f32; 130]) -> [f32; 128] {
    let mut out = [0f32; 128];
    for n in 0..128usize {
        let mut sum = src[0] as f64;
        let nyq_sign = if n % 2 == 0 { 1.0 } else { -1.0 };
        sum += src[128] as f64 * nyq_sign;
        for k in 1..64usize {
            let angle = 2.0 * std::f64::consts::PI * (n as f64) * (k as f64) / 128.0;
            let (s, c) = angle.sin_cos();
            let re = src[2 * k] as f64;
            let im = src[2 * k + 1] as f64;
            sum += 2.0 * (re * c - im * s);
        }
        out[n] = (sum / 128.0) as f32;
    }
    out
}

/// 64-point DCT-I (scaled by 1/64 like AV_TX_FLOAT_DCT_I with scale 1/64).
fn dct_i_64(src: &[f32; 65], dst: &mut [f32; 65]) {
    const N: usize = 64;
    let scale = 1.0f64 / (N as f64);
    for k in 0..=N {
        let sign = if k % 2 == 0 { 1.0 } else { -1.0 };
        let mut sum = 0.5 * ((src[0] as f64) + sign * (src[N] as f64));
        for n in 1..N {
            let angle = std::f64::consts::PI * (n as f64) * (k as f64) / (N as f64);
            sum += src[n] as f64 * angle.cos();
        }
        dst[k] = (sum * scale) as f32;
    }
}

/// 64-point DST-I (scaled by 1/64).
fn dst_i_64(src: &[f32; 64], dst: &mut [f32; 65]) {
    const N: usize = 64;
    let scale = 1.0f64 / (N as f64);
    dst[0] = 0.0;
    for k in 0..N {
        let mut sum = 0f64;
        for n in 1..=N {
            let angle = std::f64::consts::PI * (n as f64) * ((k + 1) as f64) / ((N + 1) as f64);
            sum += src[n - 1] as f64 * angle.sin();
        }
        dst[k + 1] = (sum * scale) as f32;
    }
}

impl WmaVoiceDecoder {
    /// `aw_parse_coords` (wmavoice.c).
    fn aw_parse_coords(&mut self, gb: &mut BitReader<'_>, pitch: &[i32; MAX_BLOCKS]) -> Result<()> {
        const START_OFFSET: [i32; 94] = [
            -11, -9, -7, -5, -3, -1, 1, 3, 5, 7, 9, 11, 13, 15, 18, 17, 19, 20, 21, 22, 23, 24,
            25, 26, 27, 28, 29, 30, 31, 32, 33, 35, 37, 39, 41, 43, 45, 47, 49, 51, 53, 55, 57,
            59, 61, 63, 65, 67, 69, 71, 73, 75, 77, 79, 81, 83, 85, 87, 89, 91, 93, 95, 97, 99,
            101, 103, 105, 107, 109, 111, 113, 115, 117, 119, 121, 123, 125, 127, 129, 131, 133,
            135, 137, 139, 141, 143, 145, 147, 149, 151, 153, 155, 157, 159,
        ];
        self.aw_idx_is_ext = false;
        let mut bits = gb.get_bits(6)? as usize;
        if bits >= 54 {
            self.aw_idx_is_ext = true;
            bits += (bits - 54) * 3 + gb.get_bits(2)? as usize;
        }
        self.aw_pulse_range = if pitch[0].min(pitch[1]) > 32 { 24 } else { 16 };
        let mut offset = START_OFFSET[bits.min(93)];
        while offset < 0 {
            offset += pitch[0];
        }
        self.aw_n_pulses[0] = ((pitch[0] - 1 + MAX_FRAMESIZE as i32 / 2 - offset) / pitch[0]) as i32;
        self.aw_first_pulse_off[0] = offset - self.aw_pulse_range as i32 / 2;
        offset += self.aw_n_pulses[0] * pitch[0];
        self.aw_n_pulses[1] = ((pitch[1] - 1 + MAX_FRAMESIZE as i32 - offset) / pitch[1]) as i32;
        self.aw_first_pulse_off[1] = offset - (MAX_FRAMESIZE as i32 + self.aw_pulse_range as i32) / 2;

        if START_OFFSET[bits.min(93)] < MAX_FRAMESIZE as i32 / 2 {
            while self.aw_first_pulse_off[1] - pitch[1] + self.aw_pulse_range as i32 > 0 {
                self.aw_first_pulse_off[1] -= pitch[1];
            }
            if START_OFFSET[bits.min(93)] < 0 {
                while self.aw_first_pulse_off[0] - pitch[0] + self.aw_pulse_range as i32 > 0 {
                    self.aw_first_pulse_off[0] -= pitch[0];
                }
            }
        }
        Ok(())
    }

    /// `aw_pulse_set2` (wmavoice.c).
    fn aw_pulse_set2(&mut self, gb: &mut BitReader<'_>, block_idx: usize, fcb: &mut AmrFixed) -> Result<()> {
        let pitch_lag = fcb.pitch_lag;
        let mut pulse_off = self.aw_first_pulse_off[block_idx];
        if self.aw_n_pulses[block_idx] > 0 && pitch_lag > 0 {
            while pulse_off + (self.aw_pulse_range as i32) < 1 {
                pulse_off += pitch_lag;
            }
        }
        let range;
        if self.aw_n_pulses[0] > 0 {
            if block_idx == 0 {
                range = 32;
            } else {
                range = 8;
                if self.aw_n_pulses[block_idx] > 0 {
                    pulse_off = self.aw_next_pulse_off_cache as i32;
                }
            }
        } else {
            range = 16;
        }
        let mut pulse_start = if self.aw_n_pulses[block_idx] > 0 {
            pulse_off - range as i32 / 2
        } else {
            0
        };

        // use_mask: 80-bit array over indices [0, 80)
        let mut use_mask = [0u16; 9];
        for v in use_mask.iter_mut().take(7).skip(2) {
            *v = 0xFFFF;
        }
        if self.aw_n_pulses[block_idx] > 0 && pitch_lag > 0 {
            let mut idx = pulse_off.clamp(i32::MIN / 2, i32::MAX / 2);
            while idx < MAX_FRAMESIZE as i32 / 2 {
                let excl_range = self.aw_pulse_range as i32;
                let first_sh = 16 - (idx & 15) as u32;
                if idx >= 0 {
                    let word = (idx >> 4) as usize + 2;
                    if word + 2 < use_mask.len() {
                        use_mask[word] &= ((0xFFFFu32 << first_sh) & 0xFFFF) as u16;
                        let excl = excl_range - first_sh as i32;
                        if excl >= 16 {
                            use_mask[word + 1] = 0;
                            use_mask[word + 2] &= 0xFFFF >> (excl - 16);
                        } else {
                            use_mask[word + 1] &= 0xFFFF >> excl;
                        }
                    }
                }
                idx = idx.saturating_add(pitch_lag);
            }
        }

        let aidx = if self.aw_n_pulses[0] > 0 {
            gb.get_bits(5 - 2 * block_idx)?
        } else {
            gb.get_bits(4)?
        };
        let mut n = 0;
        let mut start_off = 0;
        let mut spins = 0usize;
        while n <= aidx {
            spins += 1;
            if spins > 2 * (aidx as usize + 2) * 17 + 64 {
                return Err(Error::invalid("wmavoice: aw_pulse_set2 stuck"));
            }
            let mut idx = pulse_start;
            if pitch_lag > 0 {
                while idx < 0 {
                    idx += pitch_lag;
                }
            } else {
                return Err(Error::invalid("wmavoice: aw_pulse_set2 zero pitch"));
            }
            pulse_start = pulse_start.saturating_add(1);
            if idx >= MAX_FRAMESIZE as i32 / 2 {
                let mut found = false;
                for (word, val) in use_mask.iter().enumerate().take(7).skip(2) {
                    if *val != 0 {
                        idx = ((word - 2) * 16 + 15) as i32;
                        idx -= (31 - (31 - (*val as u32).leading_zeros())) as i32;
                        found = true;
                        break;
                    }
                }
                if !found {
                    return Err(Error::invalid("wmavoice: aw_pulse_set2 exhausted"));
                }
            }
            if idx >= 0 {
                let w = (idx >> 4) as usize + 2;
                let b = (idx & 15) as u32;
                if w < use_mask.len() && use_mask[w] & (0x8000u16 >> b) != 0 {
                    use_mask[w] &= !(0x8000u16 >> b);
                    n += 1;
                    start_off = idx;
                }
            }
        }

        fcb.x[fcb.n] = start_off as usize;
        fcb.y[fcb.n] = if gb.get_bits1()? != 0 { -1.0 } else { 1.0 };
        fcb.n += 1;

        let n = (MAX_FRAMESIZE as i32 / 2 - start_off).rem_euclid(pitch_lag);
        self.aw_next_pulse_off_cache = if n != 0 { (pitch_lag - n) as usize } else { 0 };
        Ok(())
    }

    /// `aw_pulse_set1` (wmavoice.c).
    fn aw_pulse_set1(&mut self, gb: &mut BitReader<'_>, block_idx: usize, fcb: &mut AmrFixed) -> Result<()> {
        let nbits = 12 - 2 * (self.aw_idx_is_ext && block_idx == 0) as usize;
        let mut val = gb.get_bits(nbits)?;
        if self.aw_n_pulses[block_idx] > 0 {
            let (n_pulses, v_mask, i_mask, sh): (usize, u32, u32, u32) = if self.aw_pulse_range == 24 {
                (3, 8, 7, 4)
            } else {
                (4, 4, 3, 3)
            };
            for n in (0..n_pulses).rev() {
                fcb.y[fcb.n] = if val & v_mask != 0 { -1.0 } else { 1.0 };
                let mut x = (val & i_mask) as i32 * n_pulses as i32 + n as i32
                    + self.aw_first_pulse_off[block_idx];
                while x < 0 {
                    x += fcb.pitch_lag;
                }
                if x < MAX_FRAMESIZE as i32 / 2 {
                    fcb.x[fcb.n] = x as usize;
                    fcb.n += 1;
                }
                val >>= sh;
            }
        } else {
            let num2 = ((val & 0x1FF) >> 1) as i32;
            let (delta, idx) = if num2 < 79 {
                (1, num2 + 1)
            } else if num2 < 2 * 78 {
                (3, num2 + 1 - 77)
            } else if num2 < 3 * 77 {
                (5, num2 + 1 - 2 * 76)
            } else {
                (7, num2 + 1 - 3 * 75)
            };
            let v = if val & 0x200 != 0 { -1.0f32 } else { 1.0 };
            fcb.no_repeat_mask |= 3 << fcb.n;
            fcb.x[fcb.n] = (idx - delta) as usize;
            fcb.y[fcb.n] = v;
            fcb.x[fcb.n + 1] = idx as usize;
            fcb.y[fcb.n + 1] = if val & 1 != 0 { -v } else { v };
            fcb.n += 2;
        }
        Ok(())
    }
}

impl WmaVoiceDecoder {
    /// `synth_block_hardcoded` (wmavoice.c).
    fn synth_block_hardcoded(&mut self, gb: &mut BitReader<'_>, block_idx: usize, size: usize, frame_desc: &FrameTypeDesc, excitation: &mut [f32]) -> Result<()> {
        let r_idx;
        let gain;
        if frame_desc.fcb_type == FCB_TYPE_SILENCE {
            r_idx = prng(self.frame_cntr, block_idx as i32, size as i32) as usize;
            gain = self.silence_gain;
        } else {
            r_idx = gb.get_bits(8)? as usize;
            gain = GAIN_UNIVERSAL[gb.get_bits(6)? as usize];
        }
        self.gain_pred_err = [0.0; 6];
        for (n, e) in excitation.iter_mut().take(size).enumerate() {
            *e = STD_CODEBOOK[r_idx + n] * gain;
        }
        Ok(())
    }

    /// `synth_block_fcb_acb` (wmavoice.c).
    #[allow(clippy::too_many_arguments)]
    fn synth_block_fcb_acb(&mut self, gb: &mut BitReader<'_>, block_idx: usize, size: usize, block_pitch_sh2: i32, frame_desc: &FrameTypeDesc, excitation: &mut [f32], excitation_base: usize) -> Result<()> {
        const GAIN_COEFF: [f32; 6] = [0.8169, -0.06545, 0.1726, 0.0185, -0.0359, 0.0458];
        let mut pulses = vec![0f32; size];
        let mut fcb = AmrFixed {
            pitch_lag: block_pitch_sh2 >> 2,
            pitch_fac: 1.0,
            no_repeat_mask: 0,
            ..Default::default()
        };

        if frame_desc.fcb_type == FCB_TYPE_AW_PULSES {
            self.aw_pulse_set1(gb, block_idx, &mut fcb)?;
            if self.aw_pulse_set2(gb, block_idx, &mut fcb).is_err() {
                // conceal with silence
                let r_idx = prng(self.frame_cntr, block_idx as i32, size as i32) as usize;
                for (n, e) in excitation.iter_mut().take(size).enumerate() {
                    *e = STD_CODEBOOK[r_idx + n] * self.silence_gain;
                }
                gb.skip_bits(8)?;
                return Ok(());
            }
        } else {
            let offset_nbits = 5 - frame_desc.log_n_blocks;
            fcb.no_repeat_mask = -1;
            for n in 0..5usize {
                let sign = if gb.get_bits1()? != 0 { 1.0f32 } else { -1.0 };
                let pos1 = gb.get_bits(offset_nbits)? as i32;
                fcb.x[fcb.n] = (n as i32 + 5 * pos1) as usize;
                fcb.y[fcb.n] = sign;
                fcb.n += 1;
                if n < frame_desc.dbl_pulses {
                    let pos2 = gb.get_bits(offset_nbits)? as i32;
                    fcb.x[fcb.n] = (n as i32 + 5 * pos2) as usize;
                    fcb.y[fcb.n] = if pos1 < pos2 { -sign } else { sign };
                    fcb.n += 1;
                }
            }
        }
        set_fixed_vector(&mut pulses, &fcb, 1.0, size);

        let idx = gb.get_bits(7)? as usize;
        let fcb_gain = (scalarproduct_float(&self.gain_pred_err, &GAIN_COEFF, 6) as f64
            - 5.2409161640
            + GAIN_CODEBOOK_FCB[idx] as f64)
            .exp() as f32;
        let acb_gain = GAIN_CODEBOOK_ACB[idx];
        let pred_err = GAIN_CODEBOOK_FCB[idx].clamp(-2.9957322736, 1.6094379124);

        let gain_weight = 8 >> frame_desc.log_n_blocks;
        self.gain_pred_err.copy_within(gain_weight..6, 0);
        for v in self.gain_pred_err[..gain_weight].iter_mut() {
            *v = pred_err;
        }

        // adaptive codebook
        if frame_desc.acb_type == ACB_TYPE_ASYMMETRIC {
            let mut n = 0usize;
            while n < size {
                let abs_idx = block_idx * size + n;
                let pitch_sh16 = (self.last_pitch_val << 16) + self.pitch_diff_sh16 * abs_idx as i32;
                let pitch = (pitch_sh16 + 0x6FFF) >> 16;
                let idx_sh16 = ((pitch << 16) - pitch_sh16) * 8 + 0x58000;
                let idx = idx_sh16 >> 16;
                let len = if self.pitch_diff_sh16 != 0 {
                    let next_idx_sh16 = if self.pitch_diff_sh16 > 0 {
                        idx_sh16 & !0xFFFF
                    } else {
                        (idx_sh16 + 0x10000) & !0xFFFF
                    };
                    (((idx_sh16 - next_idx_sh16) / self.pitch_diff_sh16 / 8) as usize)
                        .clamp(1, size - n)
                } else {
                    size
                };
                // interpolate from excitation buffer history:
                // in = &excitation[n - pitch] relative to block start
                let hist_start = (excitation_base + n).checked_sub(pitch as usize).unwrap_or(0);
                let mut window = vec![0f32; 2 * len];
                window[..len].copy_from_slice(&self.excitation_window[hist_start..hist_start + len]);
                window[len..2 * len].copy_from_slice(&self.excitation_window[excitation_base + n..excitation_base + n + len]);
                let mut out = vec![0f32; len];
                acelp_interpolate(&mut out, &window, &IPOL1_COEFFS, 17, (idx & 0xFFFF) as usize, 9, len);
                excitation[n..n + len].copy_from_slice(&out);
                n += len;
            }
        } else {
            let block_pitch = block_pitch_sh2 >> 2;
            let idx = block_pitch_sh2 & 3;
            if idx != 0 {
                let hist_start = excitation_base - block_pitch as usize;
                let mut window = vec![0f32; 2 * size];
                window[..size].copy_from_slice(&self.excitation_window[hist_start..hist_start + size]);
                window[size..2 * size].copy_from_slice(&self.excitation_window[excitation_base..excitation_base + size]);
                let mut out = vec![0f32; size];
                acelp_interpolate(&mut out, &window, &IPOL2_COEFFS, 4, idx as usize, 8, size);
                excitation[..size].copy_from_slice(&out);
            } else if block_pitch > 0 {
                // av_memcpy_backptr: repeat previous pitch-period content
                let src = excitation_base.saturating_sub(block_pitch as usize);
                for i in 0..size {
                    excitation[i] = self.excitation_window[src + (i % block_pitch as usize)];
                }
            } else {
                for v in excitation[..size].iter_mut() {
                    *v = 0.0;
                }
            }
        }

        {
            let mut tmp = vec![0f32; size];
            weighted_vector_sumf(&mut tmp, excitation, &pulses, acb_gain, fcb_gain, size);
            excitation[..size].copy_from_slice(&tmp);
        }
        Ok(())
    }

    /// `synth_block` (wmavoice.c).
    #[allow(clippy::too_many_arguments)]
    fn synth_block(&mut self, gb: &mut BitReader<'_>, block_idx: usize, size: usize, block_pitch_sh2: i32, lsps: &[f64], prev_lsps: &[f64], frame_desc: &FrameTypeDesc, excitation: &mut [f32], synth: &mut [f32]) -> Result<()> {
        if frame_desc.acb_type == ACB_TYPE_NONE {
            self.synth_block_hardcoded(gb, block_idx, size, frame_desc, excitation)?;
        } else {
            let base = self.excitation_window.len() / 2;
            self.excitation_window[base..base + size].copy_from_slice(&excitation[..size]);
            self.synth_block_fcb_acb(gb, block_idx, size, block_pitch_sh2, frame_desc, excitation, base)?;
        }
        let mut i_lsps = vec![0f64; self.lsps];
        let fac = (block_idx as f64 + 0.5) / frame_desc.n_blocks as f64;
        for n in 0..self.lsps {
            i_lsps[n] = (prev_lsps[n] + fac * (lsps[n] - prev_lsps[n])).cos();
        }
        let mut lpcs = vec![0f32; self.lsps];
        acelp_lspd2lpc(&i_lsps, &mut lpcs, self.lsps >> 1);
        // synthesis with lsps samples of history in front (the caller
        // maintained synth buffer mirrors FFmpeg's &synth[lsps + ...] layout)
        let base = self.synth_window.len() / 2;
        self.synth_window[base..base + size].copy_from_slice(synth);
        let mut combined = vec![0f32; self.lsps + size];
        combined[..self.lsps].copy_from_slice(&self.synth_window[base - self.lsps..base]);
        combined[self.lsps..].copy_from_slice(synth);
        let mut out = vec![0f32; size];
        celp_lp_synthesis_filter_owned(&mut out, &lpcs, &combined, size, self.lsps);
        synth[..size].copy_from_slice(&out);
        let keep: Vec<f32> = self.synth_window[self.synth_window.len() - self.lsps..].to_vec();
        self.synth_window[..self.lsps].copy_from_slice(&keep);
        Ok(())
    }
}

impl WmaVoiceDecoder {
    /// `synth_frame` (wmavoice.c).
    #[allow(clippy::too_many_arguments)]
    fn synth_frame(&mut self, gb: &mut BitReader<'_>, frame_idx: usize, samples: &mut [f32], lsps: &[f64], prev_lsps: &[f64], excitation: &mut [f32], synth: &mut [f32]) -> Result<()> {
        let bd_raw = self.frame_type_vlc.get_vlc(gb)?;
        if bd_raw < 0 || bd_raw as usize >= 25 {
            return Err(Error::invalid("wmavoice: invalid frame type VLC"));
        }
        let bd_idx = self.vbm_tree[bd_raw as usize];
        if bd_idx < 0 || bd_idx as usize >= FRAME_DESCS.len() {
            return Err(Error::invalid("wmavoice: invalid frame type index"));
        }
        let bd_idx = bd_idx as usize;
        let frame_desc = FRAME_DESCS[bd_idx];
        let block_nsamples = MAX_FRAMESIZE / frame_desc.n_blocks;

        let mut pitch = [i32::MAX; MAX_BLOCKS];
        let mut cur_pitch_val = 0i32;
        let mut last_block_pitch = 0i32;

        if frame_desc.acb_type == ACB_TYPE_ASYMMETRIC {
            let n_blocks_x2 = frame_desc.n_blocks << 1;
            let log_n_blocks_x2 = frame_desc.log_n_blocks + 1;
            cur_pitch_val = self.min_pitch_val + gb.get_bits(self.pitch_nbits)? as i32;
            cur_pitch_val = cur_pitch_val.min(self.max_pitch_val - 1);
            if self.last_acb_type == ACB_TYPE_NONE
                || 20 * (cur_pitch_val - self.last_pitch_val).abs() > (cur_pitch_val + self.last_pitch_val)
            {
                self.last_pitch_val = cur_pitch_val;
            }
            for n in 0..frame_desc.n_blocks {
                let fac = (n * 2 + 1) as i32;
                pitch[n] = (fac * cur_pitch_val
                    + (n_blocks_x2 as i32 - fac) * self.last_pitch_val
                    + frame_desc.n_blocks as i32)
                    >> log_n_blocks_x2;
            }
            self.pitch_diff_sh16 = (cur_pitch_val - self.last_pitch_val) * (1 << 16) / MAX_FRAMESIZE as i32;
        }

        match frame_desc.fcb_type {
            FCB_TYPE_SILENCE => {
                self.silence_gain = GAIN_SILENCE[gb.get_bits(8)? as usize];
            }
            FCB_TYPE_AW_PULSES => {
                self.aw_parse_coords(gb, &pitch)?;
            }
            _ => {}
        }

        for n in 0..frame_desc.n_blocks {
            let bl_pitch_sh2;
            match frame_desc.acb_type {
                ACB_TYPE_HAMMING => {
                    let t1 = (self.block_conv_table[1] as i32 - self.block_conv_table[0] as i32) << 2;
                    let t2 = (self.block_conv_table[2] as i32 - self.block_conv_table[1] as i32) << 1;
                    let t3 = self.block_conv_table[3] as i32 - self.block_conv_table[2] as i32 + 1;
                    let mut block_pitch = if n == 0 {
                        gb.get_bits(self.block_pitch_nbits)? as i32
                    } else {
                        last_block_pitch - self.block_delta_pitch_hrange as i32
                            + gb.get_bits(self.block_delta_pitch_nbits)? as i32
                    };
                    last_block_pitch = block_pitch.clamp(
                        self.block_delta_pitch_hrange as i32,
                        self.block_pitch_range as i32 - self.block_delta_pitch_hrange as i32,
                    );
                    if block_pitch < t1 {
                        bl_pitch_sh2 = ((self.block_conv_table[0] as i32) << 2) + block_pitch;
                    } else {
                        block_pitch -= t1;
                        if block_pitch < t2 {
                            bl_pitch_sh2 = ((self.block_conv_table[1] as i32) << 2) + (block_pitch << 1);
                        } else {
                            block_pitch -= t2;
                            if block_pitch < t3 {
                                bl_pitch_sh2 = (self.block_conv_table[2] as i32 + block_pitch) << 2;
                            } else {
                                bl_pitch_sh2 = (self.block_conv_table[3] as i32) << 2;
                            }
                        }
                    }
                    pitch[n] = bl_pitch_sh2 >> 2;
                }
                ACB_TYPE_ASYMMETRIC => {
                    bl_pitch_sh2 = pitch[n] << 2;
                }
                _ => {
                    bl_pitch_sh2 = 0;
                }
            }

            let exc_off = n * block_nsamples;
            let synth_off = n * block_nsamples;
            // pass sub-slices
            let mut exc_sub = excitation[exc_off..exc_off + block_nsamples].to_vec();
            let mut synth_sub = synth[synth_off..synth_off + block_nsamples].to_vec();
            self.synth_block(
                gb,
                n,
                block_nsamples,
                bl_pitch_sh2,
                lsps,
                prev_lsps,
                &frame_desc,
                &mut exc_sub,
                &mut synth_sub,
            )?;
            excitation[exc_off..exc_off + block_nsamples].copy_from_slice(&exc_sub);
            synth[synth_off..synth_off + block_nsamples].copy_from_slice(&synth_sub);
        }

        if self.do_apf {
            if frame_desc.fcb_type >= FCB_TYPE_AW_PULSES && pitch[0] == i32::MAX {
                return Err(Error::invalid("wmavoice: apf without pitch"));
            }
            let mut i_lsps = vec![0f64; self.lsps];
            for n in 0..self.lsps {
                i_lsps[n] = (0.5 * (prev_lsps[n] + lsps[n])).cos();
            }
            let mut lpcs = vec![0f32; self.lsps];
            acelp_lspd2lpc(&i_lsps, &mut lpcs, self.lsps >> 1);
            let zero_off = self.history_nsamples + MAX_FRAMESIZE * frame_idx;
            let mut s0 = vec![0f32; 80];
            self.postfilter(&synth[..80], &mut s0, 80, &lpcs, zero_off, frame_desc.fcb_type, pitch[0]);
            samples[..80].copy_from_slice(&s0);

            for n in 0..self.lsps {
                i_lsps[n] = lsps[n].cos();
            }
            let mut lpcs2 = vec![0f32; self.lsps];
            acelp_lspd2lpc(&i_lsps, &mut lpcs2, self.lsps >> 1);
            let mut s1 = vec![0f32; 80];
            self.postfilter(&synth[80..160], &mut s1, 80, &lpcs2, zero_off + 80, frame_desc.fcb_type, pitch[0]);
            samples[80..160].copy_from_slice(&s1);
        } else {
            samples[..160].copy_from_slice(&synth[..160]);
        }

        self.frame_cntr += 1;
        if self.frame_cntr >= 0xFFFF {
            self.frame_cntr -= 0xFFFF;
        }
        self.last_acb_type = frame_desc.acb_type;
        self.last_pitch_val = match frame_desc.acb_type {
            ACB_TYPE_NONE => 0,
            ACB_TYPE_ASYMMETRIC => cur_pitch_val,
            _ => pitch[frame_desc.n_blocks - 1],
        };
        Ok(())
    }

    /// `synth_superframe` (wmavoice.c).
    fn synth_superframe(&mut self, gb: &mut OwnedBitReader) -> Result<()> {
        let mut n_samples = MAX_SFRAMESIZE;
        let mut lsps = [[0f64; MAX_LSPS]; MAX_FRAMES];
        let mean_lsf: &[f64] = if self.lsps == 16 {
            &MEAN_LSF16[self.lsp_def_mode as usize * 16..][..16]
        } else {
            &MEAN_LSF10[self.lsp_def_mode as usize * 10..][..10]
        };

        let mut excitation = vec![0f32; MAX_SIGNAL_HISTORY + MAX_SFRAMESIZE + 12];
        let mut synth = vec![0f32; MAX_LSPS + MAX_SFRAMESIZE];
        excitation[..self.history_nsamples]
            .copy_from_slice(&self.excitation_history[..self.history_nsamples]);
        for (n, v) in synth.iter_mut().take(self.lsps).enumerate() {
            *v = self.synth_history[n] as f32;
        }

        if gb.get_bits1()? == 0 {
            return Err(Error::unsupported("wmavoice: WMAPro-in-WMAVoice"));
        }
        if gb.get_bits1()? != 0 {
            n_samples = gb.get_bits(12)? as usize;
            if n_samples > MAX_SFRAMESIZE {
                return Err(Error::invalid("wmavoice: superframe encodes > 480 samples"));
            }
        }

        let mut gb_reader = gb.as_reader_at();
        if self.has_residual_lsps {
            let mut prev_lsps = vec![0f64; self.lsps];
            let mut a1 = vec![0f64; self.lsps * 2];
            let mut a2 = vec![0f64; self.lsps * 2];
            for n in 0..self.lsps {
                prev_lsps[n] = self.prev_lsps[n] - mean_lsf[n];
            }
            if self.lsps == 10 {
                self.dequant_lsp10r(&mut gb_reader, &mut lsps[2], &prev_lsps, &mut a1, &mut a2)?;
            } else {
                self.dequant_lsp16r(&mut gb_reader, &mut lsps[2], &prev_lsps, &mut a1, &mut a2)?;
            }
            for n in 0..self.lsps {
                lsps[0][n] = mean_lsf[n] + (a1[n] - a2[n * 2]);
                lsps[1][n] = mean_lsf[n] + (a1[self.lsps + n] - a2[n * 2 + 1]);
                lsps[2][n] += mean_lsf[n];
            }
            for n in 0..3 {
                Self::stabilize_lsps(&mut lsps[n][..self.lsps]);
            }
        }

        self.excitation_window = excitation.clone();
        self.synth_window = synth.clone();

        for n in 0..3usize {
            if !self.has_residual_lsps {
                if self.lsps == 10 {
                    self.dequant_lsp10i(&mut gb_reader, &mut lsps[n])?;
                } else {
                    self.dequant_lsp16i(&mut gb_reader, &mut lsps[n])?;
                }
                for m in 0..self.lsps {
                    lsps[n][m] += mean_lsf[m];
                }
                Self::stabilize_lsps(&mut lsps[n][..self.lsps]);
            }
            let prev: Vec<f64> = if n == 0 {
                self.prev_lsps[..self.lsps].to_vec()
            } else {
                lsps[n - 1][..self.lsps].to_vec()
            };
            let mut samples_out = [0f32; MAX_FRAMESIZE];
            self.synth_frame(
                &mut gb_reader,
                n,
                &mut samples_out,
                &lsps[n],
                &prev,
                &mut excitation[self.history_nsamples + n * MAX_FRAMESIZE..],
                &mut synth[self.lsps + n * MAX_FRAMESIZE..],
            )?;
            self.pending_samples[n * MAX_FRAMESIZE..(n + 1) * MAX_FRAMESIZE]
                .copy_from_slice(&samples_out);
        }

        if gb_reader.get_bits1()? != 0 {
            let res = gb_reader.get_bits(4)?;
            gb_reader.skip_bits((10 * (res + 1)) as usize)?;
        }
        gb.set_bit_pos(gb_reader.bits_count());

        // update history
        self.prev_lsps[..self.lsps].copy_from_slice(&lsps[2][..self.lsps]);
        for n in 0..self.lsps {
            self.synth_history[n] = synth[MAX_SFRAMESIZE + n] as f64;
        }
        self.excitation_history[..self.history_nsamples].copy_from_slice(
            &excitation[MAX_SFRAMESIZE..MAX_SFRAMESIZE + self.history_nsamples],
        );
        if self.do_apf {
            self.zero_exc_pf.copy_within(MAX_SFRAMESIZE.., 0);
        }
        self.pending = Some(AudioFrame {
            samples: n_samples as u32,
            pts: None,
            data: vec![{
                let mut plane = Vec::with_capacity(n_samples * 4);
                for &v in &self.pending_samples[..n_samples] {
                    plane.extend_from_slice(&v.to_le_bytes());
                }
                plane
            }],
        });
        Ok(())
    }

    /// `parse_packet_header` (wmavoice.c).
    fn parse_packet_header(&mut self, gb: &mut OwnedBitReader) -> Result<usize> {
        gb.skip_bits(4)?;
        self.has_residual_lsps = gb.get_bits1()? != 0;
        let mut n_superframes = 0usize;
        let mut res;
        loop {
            if gb.bits_left() < 6 + self.spillover_bitsize {
                return Err(Error::invalid("wmavoice: packet header overread"));
            }
            res = gb.get_bits(6)? as usize;
            n_superframes += res;
            if res != 0x3F {
                break;
            }
        }
        self.spillover_nbits = gb.get_bits(self.spillover_bitsize)? as usize;
        Ok(n_superframes)
    }
}

impl WmaVoiceDecoder {
    /// `copy_bits` (wmavoice.c): copy nbits from the packet into the cache.
    fn copy_bits(&mut self, cache_bitpos: usize, gb: &mut OwnedBitReader, nbits: usize) -> usize {
        let rmn_bits = gb.bits_left();
        if rmn_bits < nbits {
            return 0;
        }
        let mut written = 0usize;
        while written < nbits {
            let chunk = (nbits - written).min(32);
            let v = gb.get_bits(chunk).unwrap_or(0) as u64;
            self.put_bits_cache(cache_bitpos + written, chunk, v);
            written += chunk;
        }
        written
    }

    fn put_bits_cache(&mut self, bit_pos: usize, nbits: usize, val: u64) {
        for b in 0..nbits {
            let bit = ((val >> (nbits - 1 - b)) & 1) as u8;
            let idx = bit_pos + b;
            let byte = idx / 8;
            let off = idx % 8;
            if byte < self.sframe_cache.len() {
                if bit != 0 {
                    self.sframe_cache[byte] |= 1 << (7 - off);
                } else {
                    self.sframe_cache[byte] &= !(1 << (7 - off));
                }
            }
        }
    }

    /// `wmavoice_decode_packet` (wmavoice.c): returns the number of
    /// consumed bytes (FFmpeg's return value; the core re-feeds the tail).
    fn decode_packet_impl(&mut self, data: &[u8]) -> Result<usize> {
        self.pending = None;
        let mut size = data.len();
        while size > self.block_align {
            size -= self.block_align;
        }
        let buf = &data[..size];

        let mut gb = OwnedBitReader::from_bits(buf.to_vec(), size << 3);
        if size % self.block_align == 0 {
            // new packet header
            if size == 0 {
                self.spillover_nbits = 0;
                self.nb_superframes = 0;
            } else {
                self.nb_superframes = self.parse_packet_header(&mut gb)? as i32;
            }

            if self.sframe_cache_size > 0 {
                let cnt = gb.bit_pos();
                if cnt + self.spillover_nbits > data.len() * 8 {
                    self.spillover_nbits = data.len() * 8 - cnt;
                }
                let written = self.copy_bits(self.sframe_cache_size, &mut gb, self.spillover_nbits);
                self.sframe_cache_size += written;
                let cache_data = self.sframe_cache.clone();
                let mut cache_gb = OwnedBitReader::from_bits(cache_data, self.sframe_cache_size);
                match self.synth_superframe(&mut cache_gb) {
                    Ok(()) => {
                        let cnt = cnt + self.spillover_nbits;
                        self.skip_bits_next = cnt & 7;
                        return Ok(cnt >> 3);
                    }
                    Err(_) => {
                        let _ = gb.skip_bits(self.spillover_nbits + gb.bit_pos() - cnt);
                        // fall through: parse superframes in this packet
                    }
                }
            } else if self.spillover_nbits > 0 {
                let _ = gb.skip_bits(self.spillover_nbits);
            }
        } else if self.skip_bits_next > 0 {
            let _ = gb.skip_bits(self.skip_bits_next);
        }

        // try parsing superframes in the current packet
        self.sframe_cache_size = 0;
        self.skip_bits_next = 0;
        let pos = gb.bits_left();
        if self.nb_superframes == 0 {
            return Ok(size);
        }
        self.nb_superframes -= 1;
        if self.nb_superframes > 0 {
            if let Err(e) = self.synth_superframe(&mut gb) {
                if gb.bits_left() < 1024 {
                    // the superframe spills into the next packet: cache the
                    // remainder and stop consuming this one
                    self.sframe_cache_size = 0;
                    for b in self.sframe_cache.iter_mut() {
                        *b = 0;
                    }
                    let rest = gb.bits_left();
                    let written = self.copy_bits(0, &mut gb, rest);
                    self.sframe_cache_size = written;
                    self.nb_superframes += 1; // not yet decoded
                    return Ok(size);
                }
                return Err(e);
            }
            let cnt = gb.bit_pos();
            self.skip_bits_next = cnt & 7;
            return Ok(cnt >> 3);
        } else if pos > 0 {
            // cache the remainder for spillover in the next packet
            for b in self.sframe_cache.iter_mut() {
                *b = 0;
            }
            let written = self.copy_bits(0, &mut gb, pos);
            self.sframe_cache_size = written;
        }
        Ok(size)
    }
}

impl Decoder for WmaVoiceDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        // Mirror FFmpeg's decode core: the decoder reports consumed bytes;
        // unconsumed tail is re-submitted. Also cap each call at
        // block_align like FFmpeg's wmavoice_decode_packet does.
        let mut off = 0usize;
        while off < packet.data.len() {
            let end = (off + self.block_align).min(packet.data.len());
            let chunk = &packet.data[off..end];
            let consumed = self.decode_packet_impl(chunk)?;
            if consumed <= 0 {
                off = end;
            } else {
                off += consumed as usize;
            }
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.take() {
            Some(f) => Ok(Frame::Audio(f)),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flush_state();
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.flush_state();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: SampleFormat::F32,
            sample_rate: self.sample_rate,
            channels: 1,
        })
    }
}

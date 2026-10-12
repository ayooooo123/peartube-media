// AMR-NB decoder.
//
// Ported from FFmpeg libavcodec/amrnbdec.c and amr.h (commit 2da55bf),
// LGPL-2.1-or-later.

//! AMR narrowband (3GPP TS 26.090) as FFmpeg decodes it: the RFC 4867
//! storage-format frame (a mode byte, then the speech bits) per channel,
//! split-matrix quantized LSFs, adaptive and algebraic codebooks, gain
//! smoothing, anti-sparseness, LP synthesis, the post-filter and a
//! high-pass filter. 160 samples per frame at 8 kHz, planar float.
//! Arithmetic as in FFmpeg's C (see `celp`).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::amrnb_data::*;
use crate::celp::*;

pub const AMR_BLOCK_SIZE: usize = 160;
const AMR_SUBFRAME_SIZE: usize = 40;
const LP_FILTER_ORDER: usize = 10;
const AMR_SAMPLE_BOUND: f32 = 32768.0;
const AMR_SAMPLE_SCALE: f64 = 2.0 / 32768.0;
const PRED_FAC_MODE_12K2: f64 = 0.65;
const LSF_R_FAC: f64 = 8000.0 / 32768.0;
const MIN_LSF_SPACING: f64 = 50.0488 / 8000.0;
const PITCH_LAG_MIN_MODE_12K2: i32 = 18;
const MIN_ENERGY: f64 = -14.0;
const SHARP_MAX: f64 = 0.79449462890625;
const AMR_TILT_RESPONSE: usize = 22;
const AMR_TILT_GAMMA_T: f64 = 0.8;
const AMR_AGC_ALPHA: f64 = 0.9;

const MODE_4K75: usize = 0;
const MODE_5K15: usize = 1;
const MODE_5K9: usize = 2;
const MODE_6K7: usize = 3;
const MODE_7K4: usize = 4;
const MODE_7K95: usize = 5;
const MODE_10K2: usize = 6;
const MODE_12K2: usize = 7;
const MODE_DTX: usize = 8;
const N_MODES: usize = 9;

/// Where the current excitation starts in `excitation_buf`.
const EXC: usize = PITCH_DELAY_MAX as usize + LP_FILTER_ORDER + 1;

fn order_table(mode: usize) -> &'static [u8] {
    match mode {
        MODE_4K75 => &ORDER_MODE_4K75,
        MODE_5K15 => &ORDER_MODE_5K15,
        MODE_5K9 => &ORDER_MODE_5K9,
        MODE_6K7 => &ORDER_MODE_6K7,
        MODE_7K4 => &ORDER_MODE_7K4,
        MODE_7K95 => &ORDER_MODE_7K95,
        MODE_10K2 => &ORDER_MODE_10K2,
        _ => &ORDER_MODE_12K2,
    }
}

/// A table row, or an error for an index the bitstream should not give.
fn row<T, const N: usize>(table: &[[T; N]], index: usize) -> Result<&[T; N]> {
    table.get(index).ok_or_else(|| Error::invalid("amr_nb: table index out of range"))
}

/// `AMRContext`: one channel.
struct AMRContext {
    /// `AMRNBFrame` as 16-bit words: lsf[5], then the subframes.
    frame: [u16; FRAME_WORDS],
    cur_frame_mode: usize,
    prev_lsf_r: [i16; LP_FILTER_ORDER],
    lsp: [[f64; LP_FILTER_ORDER]; 4],
    prev_lsp_sub4: [f64; LP_FILTER_ORDER],
    lsf_q: [[f32; LP_FILTER_ORDER]; 4],
    lsf_avg: [f32; LP_FILTER_ORDER],
    lpc: [[f32; LP_FILTER_ORDER]; 4],
    pitch_lag_int: u8,
    excitation_buf: [f32; EXC + AMR_SUBFRAME_SIZE],
    pitch_vector: [f32; AMR_SUBFRAME_SIZE],
    fixed_vector: [f32; AMR_SUBFRAME_SIZE],
    prediction_error: [f32; 4],
    pitch_gain: [f32; 5],
    fixed_gain: [f32; 5],
    beta: f32,
    diff_count: u8,
    hang_count: u8,
    prev_sparse_fixed_gain: f32,
    prev_ir_filter_nr: u8,
    ir_filter_onset: u8,
    postfilter_mem: [f32; 10],
    tilt_mem: f32,
    postfilter_agc: f32,
    high_pass_mem: [f32; 2],
    samples_in: [f32; LP_FILTER_ORDER + AMR_SUBFRAME_SIZE],
}

impl AMRContext {
    /// `amrnb_decode_init` for one channel.
    fn new() -> Self {
        let mut p = Self {
            frame: [0; FRAME_WORDS],
            cur_frame_mode: 0,
            prev_lsf_r: [0; LP_FILTER_ORDER],
            lsp: [[0.0; LP_FILTER_ORDER]; 4],
            prev_lsp_sub4: [0.0; LP_FILTER_ORDER],
            lsf_q: [[0.0; LP_FILTER_ORDER]; 4],
            lsf_avg: [0.0; LP_FILTER_ORDER],
            lpc: [[0.0; LP_FILTER_ORDER]; 4],
            pitch_lag_int: 0,
            excitation_buf: [0.0; EXC + AMR_SUBFRAME_SIZE],
            pitch_vector: [0.0; AMR_SUBFRAME_SIZE],
            fixed_vector: [0.0; AMR_SUBFRAME_SIZE],
            prediction_error: [MIN_ENERGY as f32; 4],
            pitch_gain: [0.0; 5],
            fixed_gain: [0.0; 5],
            beta: 0.0,
            diff_count: 0,
            hang_count: 0,
            prev_sparse_fixed_gain: 0.0,
            prev_ir_filter_nr: 0,
            ir_filter_onset: 0,
            postfilter_mem: [0.0; 10],
            tilt_mem: 0.0,
            postfilter_agc: 0.0,
            high_pass_mem: [0.0; 2],
            samples_in: [0.0; LP_FILTER_ORDER + AMR_SUBFRAME_SIZE],
        };
        for i in 0..LP_FILTER_ORDER {
            p.prev_lsp_sub4[i] = f64::from((i32::from(LSP_SUB4_INIT[i]) * 1000) as f32 / 32768.0);
            let avg = f32::from(LSP_AVG_INIT[i]) / 32768.0;
            p.lsf_avg[i] = avg;
            p.lsf_q[3][i] = avg;
        }
        p
    }

    fn lsf_param(&self, i: usize) -> usize {
        usize::from(self.frame[i])
    }

    fn sub(&self, subframe: usize, field: usize) -> u16 {
        self.frame[SUBFRAME_BASE + subframe * SUBFRAME_WORDS + field]
    }

    fn pulses(&self, subframe: usize) -> [u16; 10] {
        let at = SUBFRAME_BASE + subframe * SUBFRAME_WORDS + 3;
        self.frame[at..at + 10].try_into().unwrap()
    }

    /// `unpack_bitstream`: the frame mode, `None` for NO_DATA.
    fn unpack_bitstream(&mut self, buf: &[u8]) -> Option<usize> {
        let mode = usize::from(buf[0] >> 3 & 0x0F);
        if mode >= N_MODES || buf.len() < usize::from(FRAME_SIZES_NB[mode]) + 1 {
            return None;
        }
        if mode < MODE_DTX {
            amr_bit_reorder(&mut self.frame, &buf[1..], order_table(mode));
        }
        Some(mode)
    }

    /// `interpolate_lsf`
    fn interpolate_lsf(&mut self, lsf_new: &[f32; LP_FILTER_ORDER]) {
        let last = self.lsf_q[3];
        for i in 0..4 {
            let (wa, wb) = ((0.25 * (3 - i) as f64) as f32, (0.25 * (i + 1) as f64) as f32);
            weighted_vector_sumf(&mut self.lsf_q[i], &last, lsf_new, wa, wb, LP_FILTER_ORDER);
        }
    }

    /// `lsf2lsp_for_mode12k2`
    fn lsf2lsp_for_mode12k2(
        &mut self,
        lsp_index: usize,
        lsf_no_r: &[f32; LP_FILTER_ORDER],
        quantizer: [&[i16; 4]; 5],
        offset: usize,
        sign: bool,
        update: bool,
    ) {
        let mut lsf_r = [0i16; LP_FILTER_ORDER];
        for i in 0..LP_FILTER_ORDER >> 1 {
            lsf_r[2 * i] = quantizer[i][offset];
            lsf_r[2 * i + 1] = quantizer[i][offset + 1];
        }
        if sign {
            lsf_r[4] = lsf_r[4].wrapping_neg();
            lsf_r[5] = lsf_r[5].wrapping_neg();
        }
        if update {
            self.prev_lsf_r = lsf_r;
        }
        let mut lsf_q = [0.0f32; LP_FILTER_ORDER];
        for i in 0..LP_FILTER_ORDER {
            lsf_q[i] = f64::from(lsf_r[i]).mul_add(LSF_R_FAC / 8000.0, f64::from(lsf_no_r[i]) * (1.0 / 8000.0)) as f32;
        }
        set_min_dist_lsf(&mut lsf_q, MIN_LSF_SPACING, LP_FILTER_ORDER);
        if update {
            self.interpolate_lsf(&lsf_q);
        }
        lsf2lspd(&mut self.lsp[lsp_index], &lsf_q, LP_FILTER_ORDER);
    }

    /// `lsf2lsp_5`
    fn lsf2lsp_5(&mut self) -> Result<()> {
        let quantizer = [
            row(&LSF_5_1, self.lsf_param(0))?,
            row(&LSF_5_2, self.lsf_param(1))?,
            row(&LSF_5_3, self.lsf_param(2) >> 1)?,
            row(&LSF_5_4, self.lsf_param(3))?,
            row(&LSF_5_5, self.lsf_param(4))?,
        ];
        let mut lsf_no_r = [0.0f32; LP_FILTER_ORDER];
        for i in 0..LP_FILTER_ORDER {
            lsf_no_r[i] =
                (f64::from(self.prev_lsf_r[i]) * LSF_R_FAC).mul_add(PRED_FAC_MODE_12K2, f64::from(LSF_5_MEAN[i])) as f32;
        }
        let sign = self.lsf_param(2) & 1 != 0;
        self.lsf2lsp_for_mode12k2(1, &lsf_no_r, quantizer, 0, sign, false);
        self.lsf2lsp_for_mode12k2(3, &lsf_no_r, quantizer, 2, sign, true);
        for i in 0..LP_FILTER_ORDER {
            self.lsp[0][i] = 0.5f64.mul_add(self.prev_lsp_sub4[i], 0.5 * self.lsp[1][i]);
            self.lsp[2][i] = 0.5f64.mul_add(self.lsp[1][i], 0.5 * self.lsp[3][i]);
        }
        Ok(())
    }

    /// `lsf2lsp_3`
    fn lsf2lsp_3(&mut self) -> Result<()> {
        let mode = self.cur_frame_mode;
        let mut lsf_r = [0i16; LP_FILTER_ORDER];
        let q1 = row(if mode == MODE_7K95 { &LSF_3_1_MODE_7K95[..] } else { &LSF_3_1[..] }, self.lsf_param(0))?;
        lsf_r[..3].copy_from_slice(q1);
        let q2 = row(&LSF_3_2, self.lsf_param(1) << usize::from(mode <= MODE_5K15))?;
        lsf_r[3..6].copy_from_slice(q2);
        let q3 = row(if mode <= MODE_5K15 { &LSF_3_3_MODE_5K15[..] } else { &LSF_3_3[..] }, self.lsf_param(2))?;
        lsf_r[6..].copy_from_slice(q3);

        let mut lsf_q = [0.0f32; LP_FILTER_ORDER];
        for i in 0..LP_FILTER_ORDER {
            let residual = f32::from(self.prev_lsf_r[i]).mul_add(PRED_FAC[i], f32::from(lsf_r[i]));
            lsf_q[i] =
                f64::from(residual).mul_add(LSF_R_FAC / 8000.0, f64::from(LSF_3_MEAN[i]) * (1.0 / 8000.0)) as f32;
        }
        set_min_dist_lsf(&mut lsf_q, MIN_LSF_SPACING, LP_FILTER_ORDER);
        self.interpolate_lsf(&lsf_q);
        self.prev_lsf_r = lsf_r;
        lsf2lspd(&mut self.lsp[3], &lsf_q, LP_FILTER_ORDER);
        for i in 1..=3 {
            for j in 0..LP_FILTER_ORDER {
                let prev = self.prev_lsp_sub4[j];
                self.lsp[i - 1][j] = ((self.lsp[3][j] - prev) * 0.25).mul_add(i as f64, prev);
            }
        }
        Ok(())
    }

    /// `decode_pitch_vector`
    fn decode_pitch_vector(&mut self, subframe: usize) {
        let mode = self.cur_frame_mode;
        let p_lag = i32::from(self.sub(subframe, 0));
        let prev = i32::from(self.pitch_lag_int);
        let (mut lag_int, mut lag_frac) = if mode == MODE_12K2 {
            decode_pitch_lag_1_6(p_lag, prev, subframe)
        } else {
            let resolution = if mode <= MODE_6K7 {
                4
            } else if mode == MODE_7K95 {
                5
            } else {
                6
            };
            let (lag_int, lag_frac) =
                decode_pitch_lag(p_lag, prev, subframe, mode != MODE_4K75 && mode != MODE_5K15, resolution);
            (lag_int, lag_frac * 2)
        };
        self.pitch_lag_int = lag_int as u8;
        lag_int += i32::from(lag_frac > 0);
        if lag_frac > 0 {
            lag_frac -= 6;
        }
        // FFmpeg reads before its buffer for a lag past the history it
        // keeps (a stream no encoder makes): zeros stand in for that.
        let in_at = EXC as i32 + 1 - lag_int;
        let frac = (lag_frac + 6) as usize;
        if in_at >= LP_FILTER_ORDER as i32 {
            interpolatef(&mut self.excitation_buf, EXC, in_at as usize, &B60_SINC, 6, frac, 10, AMR_SUBFRAME_SIZE);
        } else {
            let pad = (LP_FILTER_ORDER as i32 - in_at) as usize;
            let mut padded = vec![0.0f32; pad + self.excitation_buf.len()];
            padded[pad..].copy_from_slice(&self.excitation_buf);
            interpolatef(&mut padded, pad + EXC, (pad as i32 + in_at) as usize, &B60_SINC, 6, frac, 10, AMR_SUBFRAME_SIZE);
            self.excitation_buf.copy_from_slice(&padded[pad..]);
        }
        self.pitch_vector.copy_from_slice(&self.excitation_buf[EXC..EXC + AMR_SUBFRAME_SIZE]);
    }

    /// `pitch_sharpening`
    fn pitch_sharpening(&mut self, subframe: usize, fixed: &mut AmrFixed) {
        let mode = self.cur_frame_mode;
        if mode == MODE_12K2 {
            self.beta = f64::from(self.pitch_gain[4]).min_c(1.0) as f32;
        }
        fixed.pitch_lag = i32::from(self.pitch_lag_int);
        fixed.pitch_fac = self.beta;
        if mode != MODE_4K75 || subframe & 1 != 0 {
            self.beta = clipf(self.pitch_gain[4], 0.0, SHARP_MAX as f32);
        }
    }

    /// `fixed_gain_smooth`
    fn fixed_gain_smooth(&mut self, subframe: usize) -> f32 {
        let lsf = &self.lsf_q[subframe];
        let mut diff = 0.0f32;
        for i in 0..LP_FILTER_ORDER {
            diff = (f64::from(diff)
                + f64::from(self.lsf_avg[i] - lsf[i]).abs() / f64::from(self.lsf_avg[i])) as f32;
        }
        self.diff_count = self.diff_count.wrapping_add(1);
        if f64::from(diff) <= 0.65 {
            self.diff_count = 0;
        }
        if self.diff_count > 10 {
            self.hang_count = 0;
            self.diff_count -= 1;
        }
        let mode = self.cur_frame_mode;
        if self.hang_count < 40 {
            self.hang_count += 1;
        } else if mode < MODE_7K4 || mode == MODE_10K2 {
            let smoothing_factor = clipf(4.0f64.mul_add(f64::from(diff), -1.6) as f32, 0.0, 1.0);
            let g = &self.fixed_gain;
            let fixed_gain_mean = (f64::from(g[0] + g[1] + g[2] + g[3] + g[4]) * 0.2) as f32;
            return (1.0 - f64::from(smoothing_factor))
                .mul_add(f64::from(fixed_gain_mean), f64::from(smoothing_factor * g[4])) as f32;
        }
        self.fixed_gain[4]
    }

    /// `decode_gains`: the fixed gain factor; sets the pitch gain.
    fn decode_gains(&mut self, subframe: usize) -> Result<f32> {
        let mode = self.cur_frame_mode;
        let p_gain = usize::from(self.sub(subframe, 1));
        if mode == MODE_12K2 || mode == MODE_7K95 {
            let pit = *QUA_GAIN_PIT.get(p_gain).ok_or_else(|| Error::invalid("amr_nb: pitch gain index"))?;
            let code = *QUA_GAIN_CODE
                .get(usize::from(self.sub(subframe, 2)))
                .ok_or_else(|| Error::invalid("amr_nb: fixed gain index"))?;
            self.pitch_gain[4] = (f64::from(pit) * (1.0 / 16384.0)) as f32;
            Ok((f64::from(code) * (1.0 / 2048.0)) as f32)
        } else {
            let gains = if mode >= MODE_6K7 {
                row(&GAINS_HIGH, p_gain)?
            } else if mode >= MODE_5K15 {
                row(&GAINS_LOW, p_gain)?
            } else {
                row(&GAINS_MODE_4K75, (usize::from(self.sub(subframe & 2, 1)) << 1) + (subframe & 1))?
            };
            self.pitch_gain[4] = (f64::from(gains[0]) * (1.0 / 16384.0)) as f32;
            Ok((f64::from(gains[1]) * (1.0 / 4096.0)) as f32)
        }
    }

    /// `anti_sparseness`: true when the filtered vector in `out` replaces
    /// the fixed vector.
    fn anti_sparseness(&mut self, fixed: &AmrFixed, fixed_gain: f32, out: &mut [f32; AMR_SUBFRAME_SIZE]) -> bool {
        let pg = f64::from(self.pitch_gain[4]);
        let mut ir_filter_nr: u8 = if pg < 0.6 {
            0
        } else if pg < 0.9 {
            1
        } else {
            2
        };
        if f64::from(fixed_gain) > 2.0 * f64::from(self.prev_sparse_fixed_gain) {
            self.ir_filter_onset = 2;
        } else if self.ir_filter_onset != 0 {
            self.ir_filter_onset -= 1;
        }
        if self.ir_filter_onset == 0 {
            let count = self.pitch_gain.iter().filter(|&&g| f64::from(g) < 0.6).count();
            if count > 2 {
                ir_filter_nr = 0;
            }
            if u32::from(ir_filter_nr) > u32::from(self.prev_ir_filter_nr) + 1 {
                ir_filter_nr -= 1;
            }
        } else if ir_filter_nr < 2 {
            ir_filter_nr += 1;
        }
        if f64::from(fixed_gain) < 5.0 {
            ir_filter_nr = 2;
        }
        let mode = self.cur_frame_mode;
        let filtered = mode != MODE_7K4 && mode < MODE_10K2 && ir_filter_nr < 2;
        if filtered {
            let filter = match (mode == MODE_7K95, ir_filter_nr) {
                (true, 0) => &IR_FILTER_STRONG_MODE_7K95,
                (false, 0) => &IR_FILTER_STRONG,
                _ => &IR_FILTER_MEDIUM,
            };
            apply_ir_filter(out, fixed, filter);
        }
        self.prev_ir_filter_nr = ir_filter_nr;
        self.prev_sparse_fixed_gain = fixed_gain;
        filtered
    }

    /// `synthesis`: true on overflow.
    fn synthesis(&mut self, subframe: usize, fixed_gain: f32, fixed_vector: &[f32; AMR_SUBFRAME_SIZE], overflow: bool) -> bool {
        if overflow {
            for v in &mut self.pitch_vector {
                *v = (f64::from(*v) * 0.25) as f32;
            }
        }
        let mut excitation = [0.0f32; AMR_SUBFRAME_SIZE];
        weighted_vector_sumf(&mut excitation, &self.pitch_vector, fixed_vector, self.pitch_gain[4], fixed_gain, AMR_SUBFRAME_SIZE);
        let pg = self.pitch_gain[4];
        if f64::from(pg) > 0.5 && !overflow {
            let energy = dot(&excitation, &excitation, AMR_SUBFRAME_SIZE);
            let factor = if self.cur_frame_mode == MODE_12K2 {
                0.25 * f64::from(pg).min_c(1.0)
            } else {
                0.5 * f64::from(pg).min_c(SHARP_MAX)
            };
            let pitch_factor = (f64::from(pg) * factor) as f32;
            for i in 0..AMR_SUBFRAME_SIZE {
                excitation[i] = pitch_factor.mul_add(self.pitch_vector[i], excitation[i]);
            }
            scale_vector_to_given_sum_of_squares(&mut excitation, energy);
        }
        let lpc = self.lpc[subframe];
        lp_synthesis_filterf(&mut self.samples_in, LP_FILTER_ORDER, &lpc, &excitation, AMR_SUBFRAME_SIZE, LP_FILTER_ORDER);
        self.samples_in[LP_FILTER_ORDER..].iter().any(|s| s.abs() > AMR_SAMPLE_BOUND)
    }

    /// `update_state`
    fn update_state(&mut self) {
        self.prev_lsp_sub4 = self.lsp[3];
        self.excitation_buf.copy_within(AMR_SUBFRAME_SIZE.., 0);
        self.pitch_gain.copy_within(1.., 0);
        self.fixed_gain.copy_within(1.., 0);
        self.samples_in.copy_within(AMR_SUBFRAME_SIZE.., 0);
    }

    /// `postfilter`
    fn postfilter(&mut self, subframe: usize, buf_out: &mut [f32]) {
        let samples: [f32; AMR_SUBFRAME_SIZE] = self.samples_in[LP_FILTER_ORDER..].try_into().unwrap();
        let speech_gain = dot(&samples, &samples, AMR_SUBFRAME_SIZE);
        let (gamma_n, gamma_d) = if self.cur_frame_mode == MODE_12K2 || self.cur_frame_mode == MODE_10K2 {
            (&POW_0_7, &POW_0_75)
        } else {
            (&POW_0_55, &POW_0_7)
        };
        let lpc = &self.lpc[subframe];
        let mut lpc_n = [0.0f32; LP_FILTER_ORDER];
        let mut lpc_d = [0.0f32; LP_FILTER_ORDER];
        for i in 0..LP_FILTER_ORDER {
            lpc_n[i] = lpc[i] * gamma_n[i];
            lpc_d[i] = lpc[i] * gamma_d[i];
        }
        let mut pole_out = [0.0f32; AMR_SUBFRAME_SIZE + LP_FILTER_ORDER];
        pole_out[..LP_FILTER_ORDER].copy_from_slice(&self.postfilter_mem);
        lp_synthesis_filterf(&mut pole_out, LP_FILTER_ORDER, &lpc_d, &samples, AMR_SUBFRAME_SIZE, LP_FILTER_ORDER);
        self.postfilter_mem.copy_from_slice(&pole_out[AMR_SUBFRAME_SIZE..]);
        lp_zero_synthesis_filterf(buf_out, &lpc_n, &pole_out, LP_FILTER_ORDER, AMR_SUBFRAME_SIZE, LP_FILTER_ORDER);
        let tilt = tilt_factor(&lpc_n, &lpc_d);
        tilt_compensation(&mut self.tilt_mem, tilt, buf_out);
        adaptive_gain_control(buf_out, speech_gain, AMR_AGC_ALPHA as f32, &mut self.postfilter_agc);
    }

    /// One channel's frame of `amrnb_decode_frame`: the bytes it took.
    fn decode_channel_frame(&mut self, buf: &[u8], out: &mut [f32; AMR_BLOCK_SIZE]) -> Result<usize> {
        let mode = self.unpack_bitstream(buf).ok_or_else(|| Error::invalid("amr_nb: corrupt bitstream"))?;
        if mode == MODE_DTX {
            return Err(Error::unsupported("amr_nb: DTX frames"));
        }
        self.cur_frame_mode = mode;
        let channel_size = usize::from(FRAME_SIZES_NB[mode]) + 1;
        if mode == MODE_12K2 {
            self.lsf2lsp_5()?;
        } else {
            self.lsf2lsp_3()?;
        }
        for i in 0..4 {
            lspd2lpc(&self.lsp[i], &mut self.lpc[i], 5);
        }

        let mut fixed = AmrFixed::default();
        for subframe in 0..4 {
            self.decode_pitch_vector(subframe);
            decode_fixed_sparse(&mut fixed, &self.pulses(subframe), mode, subframe);
            let fixed_gain_factor = self.decode_gains(subframe)?;
            self.pitch_sharpening(subframe, &mut fixed);
            if fixed.pitch_lag == 0 {
                return Err(Error::invalid("amr_nb: pitch lag 0"));
            }
            set_fixed_vector(&mut self.fixed_vector, &fixed, 1.0, AMR_SUBFRAME_SIZE);
            let energy = dot(&self.fixed_vector, &self.fixed_vector, AMR_SUBFRAME_SIZE) / AMR_SUBFRAME_SIZE as f32;
            self.fixed_gain[4] =
                amr_set_fixed_gain(fixed_gain_factor, energy, &mut self.prediction_error, ENERGY_MEAN[mode], &ENERGY_PRED_FAC);

            let pg = self.pitch_gain[4];
            let exc = &mut self.excitation_buf[EXC..];
            for v in exc.iter_mut() {
                *v *= pg;
            }
            set_fixed_vector(exc, &fixed, self.fixed_gain[4], AMR_SUBFRAME_SIZE);
            for v in exc.iter_mut() {
                *v = v.trunc();
            }

            let synth_fixed_gain = self.fixed_gain_smooth(subframe);
            let mut spare = [0.0f32; AMR_SUBFRAME_SIZE];
            let fixed_vector = self.fixed_vector;
            let synth_fixed_vector =
                if self.anti_sparseness(&fixed, synth_fixed_gain, &mut spare) { spare } else { fixed_vector };
            if self.synthesis(subframe, synth_fixed_gain, &synth_fixed_vector, false) {
                self.synthesis(subframe, synth_fixed_gain, &synth_fixed_vector, true);
            }
            let at = subframe * AMR_SUBFRAME_SIZE;
            self.postfilter(subframe, &mut out[at..at + AMR_SUBFRAME_SIZE]);

            clear_fixed_vector(&mut self.fixed_vector, &fixed, AMR_SUBFRAME_SIZE);
            self.update_state();
        }

        let gain = (f64::from(HIGHPASS_GAIN) * AMR_SAMPLE_SCALE) as f32;
        apply_order_2_transfer_function(
            out,
            &[HIGHPASS_ZEROS[0], HIGHPASS_ZEROS[1]],
            &[HIGHPASS_POLES[0], HIGHPASS_POLES[1]],
            gain,
            &mut self.high_pass_mem,
        );
        let last = self.lsf_q[3];
        weighted_vector_sumf_in_place(&mut self.lsf_avg, &last, 0.84f64 as f32, 0.16f64 as f32, LP_FILTER_ORDER);
        Ok(channel_size)
    }
}

/// C's `FFMIN(a, b)` on doubles: `a > b ? b : a`.
trait MinC {
    fn min_c(self, b: f64) -> f64;
}

impl MinC for f64 {
    fn min_c(self, b: f64) -> f64 {
        if self > b { b } else { self }
    }
}

/// `av_clipf`
fn clipf(a: f32, min: f32, max: f32) -> f32 {
    if a < min {
        min
    } else if a > max {
        max
    } else {
        a
    }
}

/// `decode_pitch_lag_1_6`
fn decode_pitch_lag_1_6(pitch_index: i32, prev_lag_int: i32, subframe: usize) -> (i32, i32) {
    if subframe == 0 || subframe == 2 {
        if pitch_index < 463 {
            let lag_int = ((pitch_index + 107) * 10923) >> 16;
            (lag_int, pitch_index - lag_int * 6 + 105)
        } else {
            (pitch_index - 368, 0)
        }
    } else {
        let lag_int = (((pitch_index + 5) * 10923) >> 16) - 1;
        let lag_frac = pitch_index - lag_int * 6 - 3;
        (lag_int + (prev_lag_int - 5).clamp(PITCH_LAG_MIN_MODE_12K2, PITCH_DELAY_MAX - 9), lag_frac)
    }
}

/// `decode_10bit_pulse`
fn decode_10bit_pulse(code: u16, pulse_position: &mut [i32; 8], i1: usize, i2: usize, i3: usize) {
    let positions = &BASE_FIVE_TABLE[usize::from(code >> 3) & 127];
    pulse_position[i1] = (i32::from(positions[2]) << 1) + i32::from(code & 1);
    pulse_position[i2] = (i32::from(positions[1]) << 1) + i32::from((code >> 1) & 1);
    pulse_position[i3] = (i32::from(positions[0]) << 1) + i32::from((code >> 2) & 1);
}

/// `decode_8_pulses_31bits`
fn decode_8_pulses_31bits(fixed_index: &[u16; 10], fixed: &mut AmrFixed) {
    let mut pulse_position = [0i32; 8];
    decode_10bit_pulse(fixed_index[4], &mut pulse_position, 0, 4, 1);
    decode_10bit_pulse(fixed_index[5], &mut pulse_position, 2, 6, 5);
    let temp = ((i32::from(fixed_index[6]) >> 2) * 25 + 12) >> 5;
    pulse_position[3] = temp % 5;
    pulse_position[7] = temp / 5;
    if pulse_position[7] & 1 != 0 {
        pulse_position[3] = 4 - pulse_position[3];
    }
    pulse_position[3] = (pulse_position[3] << 1) + i32::from(fixed_index[6] & 1);
    pulse_position[7] = (pulse_position[7] << 1) + i32::from((fixed_index[6] >> 1) & 1);
    fixed.n = 8;
    for i in 0..4 {
        let pos1 = (pulse_position[i] << 2) + i as i32;
        let pos2 = (pulse_position[i + 4] << 2) + i as i32;
        let sign: f32 = if fixed_index[i] != 0 { -1.0 } else { 1.0 };
        fixed.x[i] = pos1;
        fixed.x[i + 4] = pos2;
        fixed.y[i] = sign;
        fixed.y[i + 4] = if pos2 < pos1 { -sign } else { sign };
    }
}

/// `decode_fixed_sparse`
fn decode_fixed_sparse(fixed: &mut AmrFixed, pulses: &[u16; 10], mode: usize, subframe: usize) {
    if mode == MODE_12K2 {
        decode_10_pulses_35bits(pulses, fixed, &GRAY_DECODE, 5, 3);
    } else if mode == MODE_10K2 {
        decode_8_pulses_31bits(pulses, fixed);
    } else {
        let fixed_index = i32::from(pulses[0]);
        let pos = &mut fixed.x;
        if mode <= MODE_5K15 {
            let subset = (((fixed_index >> 3) & 8) + ((subframe as i32) << 1)) as usize;
            pos[0] = (fixed_index & 7) * 5 + i32::from(TRACK_POSITION[subset]);
            pos[1] = ((fixed_index >> 3) & 7) * 5 + i32::from(TRACK_POSITION[subset + 1]);
            fixed.n = 2;
        } else if mode == MODE_5K9 {
            let subset = ((fixed_index & 1) << 1) + 1;
            pos[0] = ((fixed_index >> 1) & 7) * 5 + subset;
            let subset = (fixed_index >> 4) & 3;
            pos[1] = ((fixed_index >> 6) & 7) * 5 + subset + i32::from(subset == 3);
            fixed.n = if pos[0] == pos[1] { 1 } else { 2 };
        } else if mode == MODE_6K7 {
            pos[0] = (fixed_index & 7) * 5;
            let subset = (fixed_index >> 2) & 2;
            pos[1] = ((fixed_index >> 4) & 7) * 5 + subset + 1;
            let subset = (fixed_index >> 6) & 2;
            pos[2] = ((fixed_index >> 8) & 7) * 5 + subset + 2;
            fixed.n = 3;
        } else {
            let gray = |shift: i32| i32::from(GRAY_DECODE[((fixed_index >> shift) & 7) as usize]);
            pos[0] = gray(0);
            pos[1] = gray(3) + 1;
            pos[2] = gray(6) + 2;
            let subset = (fixed_index >> 9) & 1;
            pos[3] = gray(10) + subset + 3;
            fixed.n = 4;
        }
        for i in 0..fixed.n {
            fixed.y[i] = if (pulses[1] >> i) & 1 != 0 { 1.0 } else { -1.0 };
        }
    }
}

/// `apply_ir_filter`
fn apply_ir_filter(out: &mut [f32; AMR_SUBFRAME_SIZE], fixed: &AmrFixed, filter: &[f32; AMR_SUBFRAME_SIZE]) {
    let mut filter1 = [0.0f32; AMR_SUBFRAME_SIZE];
    let mut filter2 = [0.0f32; AMR_SUBFRAME_SIZE];
    let lag = fixed.pitch_lag;
    let fac = fixed.pitch_fac;
    let n = AMR_SUBFRAME_SIZE as i32;
    if lag < n {
        circ_addf(&mut filter1, filter, filter, lag as usize, fac, AMR_SUBFRAME_SIZE);
        if lag < n >> 1 {
            circ_addf(&mut filter2, filter, &filter1, lag as usize, fac, AMR_SUBFRAME_SIZE);
        }
    }
    out.fill(0.0);
    for i in 0..fixed.n {
        let x = fixed.x[i];
        let filterp = if x >= n - lag {
            filter
        } else if x >= n - (lag << 1) {
            &filter1
        } else {
            &filter2
        };
        circ_addf_in_place(out, filterp, x as usize, fixed.y[i], AMR_SUBFRAME_SIZE);
    }
}

/// `tilt_factor`
fn tilt_factor(lpc_n: &[f32; LP_FILTER_ORDER], lpc_d: &[f32; LP_FILTER_ORDER]) -> f32 {
    let mut impulse = [0.0f32; LP_FILTER_ORDER + AMR_TILT_RESPONSE];
    impulse[LP_FILTER_ORDER] = 1.0;
    impulse[LP_FILTER_ORDER + 1..LP_FILTER_ORDER + 1 + LP_FILTER_ORDER].copy_from_slice(lpc_n);
    // In place in FFmpeg: each block of samples is read before it is
    // written, so filtering a copy is the same.
    let input: [f32; AMR_TILT_RESPONSE] = impulse[LP_FILTER_ORDER..].try_into().unwrap();
    lp_synthesis_filterf(&mut impulse, LP_FILTER_ORDER, lpc_d, &input, AMR_TILT_RESPONSE, LP_FILTER_ORDER);
    let hf = &impulse[LP_FILTER_ORDER..];
    let rh0 = dot(hf, hf, AMR_TILT_RESPONSE);
    let rh1 = dot(hf, &hf[1..], AMR_TILT_RESPONSE - 1);
    if f64::from(rh1) >= 0.0 { (f64::from(rh1 / rh0) * AMR_TILT_GAMMA_T) as f32 } else { 0.0 }
}

pub struct AMRNBDecoder {
    codec_id: CodecId,
    channels: usize,
    ch: Vec<AMRContext>,
    queue: VecDeque<Frame>,
    pts_tracker: i64,
}

impl AMRNBDecoder {
    /// `amrnb_decode_init`: one or two channels (none given means one).
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = match params.channels.unwrap_or(0) {
            0 => 1,
            n @ 1..=2 => usize::from(n),
            n => return Err(Error::unsupported(format!("amr_nb: {n} channels"))),
        };
        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels,
            ch: (0..channels).map(|_| AMRContext::new()).collect(),
            queue: VecDeque::new(),
            pts_tracker: 0,
        })
    }
}

impl Decoder for AMRNBDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// `amrnb_decode_frame` as libavcodec calls it: each call decodes one
    /// frame per channel, in channel order, and the rest of the packet
    /// goes to the next call. Output is planar float.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = packet.data.as_slice();
        let mut cur_pts = packet.pts.unwrap_or(self.pts_tracker);
        while !data.is_empty() {
            let mut planes = Vec::with_capacity(self.channels);
            for ch in &mut self.ch {
                if data.is_empty() {
                    return Err(Error::invalid("amr_nb: packet ends before the frame of every channel"));
                }
                let mut pcm = [0.0f32; AMR_BLOCK_SIZE];
                let consumed = ch.decode_channel_frame(data, &mut pcm)?;
                data = data.get(consumed..).unwrap_or(&[]);
                planes.push(pcm.iter().flat_map(|s| s.to_le_bytes()).collect());
            }
            self.queue.push_back(Frame::Audio(AudioFrame {
                samples: AMR_BLOCK_SIZE as u32,
                pts: Some(cur_pts),
                data: planes,
            }));
            cur_pts += AMR_BLOCK_SIZE as i64;
        }
        self.pts_tracker = cur_pts;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.queue.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        for ch in &mut self.ch {
            *ch = AMRContext::new();
        }
        self.queue.clear();
        self.pts_tracker = 0;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::F32P, sample_rate: 8_000, channels: self.channels as u16 })
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(AMRNBDecoder::new(params)?))
}

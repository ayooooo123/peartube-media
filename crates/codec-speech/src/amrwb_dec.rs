// AMR-WB decoder.
//
// Ported from FFmpeg libavcodec/amrwbdec.c and amr.h (commit 2da55bf), and
// av_lfg_get from libavutil/lfg.h, LGPL-2.1-or-later.

//! AMR wideband (3GPP TS 26.190) as FFmpeg decodes it: per channel an RFC
//! 4867 storage-format frame, ISF vectors, adaptive and algebraic
//! codebooks, noise enhancement, anti-sparseness, 16th-order synthesis at
//! 12.8 kHz, de-emphasis and a high-pass filter, 5/4 upsampling to 16 kHz,
//! and a 6.4-7 kHz band of shaped noise. 320 samples per frame, planar
//! float. A frame marked bad or empty is silence, as in FFmpeg. Arithmetic
//! as in FFmpeg's C (see `celp`).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::amrwb_data::*;
use crate::celp::*;

const LP_ORDER: usize = 16;
const LP_ORDER_16K: usize = 20;
const HB_FIR_SIZE: usize = 30;
const UPS_FIR_SIZE: usize = 12;
const UPS_MEM_SIZE: usize = 2 * UPS_FIR_SIZE;
const MIN_ISF_SPACING: f64 = 128.0 / 32768.0;
const PRED_FACTOR: f64 = 1.0 / 3.0;
const MIN_ENERGY: f64 = -14.0;
const ENERGY_MEAN: f64 = 30.0;
const PREEMPH_FAC: f64 = 0.68;
const SFR: usize = 64;
const SFR16: usize = 80;
const P_DELAY_MAX: i32 = 231;
const P_DELAY_MIN: i32 = 34;
/// `1.0f / (1 << 15)`
const Q15: f32 = 1.0 / 32768.0;

const MODE_6K60: usize = 0;
const MODE_8K85: usize = 1;
const MODE_12K65: usize = 2;
const MODE_14K25: usize = 3;
const MODE_15K85: usize = 4;
const MODE_18K25: usize = 5;
const MODE_19K85: usize = 6;
const MODE_23K05: usize = 7;
const MODE_23K85: usize = 8;
const MODE_SID: usize = 9;
const NO_DATA: usize = 15;

/// Where the current excitation starts in `excitation_buf`.
const EXC: usize = P_DELAY_MAX as usize + LP_ORDER + 1;

fn order_table(mode: usize) -> &'static [u16] {
    match mode {
        MODE_6K60 => &ORDER_MODE_6K60,
        MODE_8K85 => &ORDER_MODE_8K85,
        MODE_12K65 => &ORDER_MODE_12K65,
        MODE_14K25 => &ORDER_MODE_14K25,
        MODE_15K85 => &ORDER_MODE_15K85,
        MODE_18K25 => &ORDER_MODE_18K25,
        MODE_19K85 => &ORDER_MODE_19K85,
        MODE_23K05 => &ORDER_MODE_23K05,
        _ => &ORDER_MODE_23K85,
    }
}

fn row<T, const N: usize>(table: &[[T; N]], index: u16) -> Result<&[T; N]> {
    table.get(usize::from(index)).ok_or_else(|| Error::invalid("amr_wb: table index out of range"))
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

/// `AVLFG`, seeded as `av_lfg_init(&prng, 1)`.
struct Lfg {
    state: [u32; 64],
    index: u32,
}

impl Lfg {
    fn new() -> Self {
        Self { state: LFG_SEED_1_STATE, index: 0 }
    }

    /// `av_lfg_get`
    fn get(&mut self) -> u32 {
        let i = self.index;
        let a = self.state[(i.wrapping_sub(24) & 63) as usize].wrapping_add(self.state[(i.wrapping_sub(55) & 63) as usize]);
        self.state[(i & 63) as usize] = a;
        self.index = i.wrapping_add(1);
        a
    }
}

/// `AMRWBContext`: one channel.
struct AMRWBContext {
    /// `AMRWBFrame` as 16-bit words: vad, isp_id[7], then the subframes.
    frame: [u16; FRAME_WORDS],
    fr_cur_mode: usize,
    isf_cur: [f32; LP_ORDER],
    isf_q_past: [f32; LP_ORDER],
    isf_past_final: [f32; LP_ORDER],
    isp: [[f64; LP_ORDER]; 4],
    isp_sub4_past: [f64; LP_ORDER],
    lp_coef: [[f32; LP_ORDER]; 4],
    base_pitch_lag: u8,
    pitch_lag_int: u8,
    excitation_buf: [f32; EXC + SFR + 1],
    pitch_vector: [f32; SFR],
    fixed_vector: [f32; SFR],
    prediction_error: [f32; 4],
    pitch_gain: [f32; 6],
    fixed_gain: [f32; 2],
    tilt_coef: f32,
    prev_ir_filter_nr: u8,
    prev_tr_gain: f32,
    samples_az: [f32; LP_ORDER + SFR],
    samples_up: [f32; UPS_MEM_SIZE + SFR],
    samples_hb: [f32; LP_ORDER_16K + SFR16],
    hpf_31_mem: [f32; 2],
    hpf_400_mem: [f32; 2],
    demph_mem: f32,
    bpf_6_7_mem: [f32; HB_FIR_SIZE],
    lpf_7_mem: [f32; HB_FIR_SIZE],
    prng: Lfg,
    first_frame: bool,
}

impl AMRWBContext {
    /// `amrwb_decode_init` for one channel.
    fn new() -> Self {
        let mut ctx = Self {
            frame: [0; FRAME_WORDS],
            fr_cur_mode: 0,
            isf_cur: [0.0; LP_ORDER],
            isf_q_past: [0.0; LP_ORDER],
            isf_past_final: [0.0; LP_ORDER],
            isp: [[0.0; LP_ORDER]; 4],
            isp_sub4_past: [0.0; LP_ORDER],
            lp_coef: [[0.0; LP_ORDER]; 4],
            base_pitch_lag: 0,
            pitch_lag_int: 0,
            excitation_buf: [0.0; EXC + SFR + 1],
            pitch_vector: [0.0; SFR],
            fixed_vector: [0.0; SFR],
            prediction_error: [MIN_ENERGY as f32; 4],
            pitch_gain: [0.0; 6],
            fixed_gain: [0.0; 2],
            tilt_coef: 0.0,
            prev_ir_filter_nr: 0,
            prev_tr_gain: 0.0,
            samples_az: [0.0; LP_ORDER + SFR],
            samples_up: [0.0; UPS_MEM_SIZE + SFR],
            samples_hb: [0.0; LP_ORDER_16K + SFR16],
            hpf_31_mem: [0.0; 2],
            hpf_400_mem: [0.0; 2],
            demph_mem: 0.0,
            bpf_6_7_mem: [0.0; HB_FIR_SIZE],
            lpf_7_mem: [0.0; HB_FIR_SIZE],
            prng: Lfg::new(),
            first_frame: true,
        };
        for i in 0..LP_ORDER {
            ctx.isf_past_final[i] = f32::from(ISF_INIT[i]) * Q15;
        }
        ctx
    }

    fn sub(&self, subframe: usize, field: usize) -> u16 {
        self.frame[SUBFRAME_BASE + subframe * SUBFRAME_WORDS + field]
    }

    fn pulses(&self, subframe: usize, high: bool) -> [u16; 4] {
        let at = SUBFRAME_BASE + subframe * SUBFRAME_WORDS + if high { 4 } else { 8 };
        self.frame[at..at + 4].try_into().unwrap()
    }

    /// `decode_isf_indices_36b` and `decode_isf_indices_46b`.
    fn decode_isf_indices(&mut self) -> Result<()> {
        let ind: [u16; 7] = self.frame[1..8].try_into().unwrap();
        let q = &mut self.isf_cur;
        let d1 = row(&DICO1_ISF, ind[0])?;
        let d2 = row(&DICO2_ISF, ind[1])?;
        for i in 0..9 {
            q[i] = f32::from(d1[i]) * Q15;
        }
        for i in 0..7 {
            q[i + 9] = f32::from(d2[i]) * Q15;
        }
        let mut add = |at: usize, values: &[i16]| {
            for (k, &v) in values.iter().enumerate() {
                q[at + k] = f32::from(v).mul_add(Q15, q[at + k]);
            }
        };
        if self.fr_cur_mode == MODE_6K60 {
            add(0, row(&DICO21_ISF_36B, ind[2])?);
            add(5, row(&DICO22_ISF_36B, ind[3])?);
            add(9, row(&DICO23_ISF_36B, ind[4])?);
        } else {
            add(0, row(&DICO21_ISF, ind[2])?);
            add(3, row(&DICO22_ISF, ind[3])?);
            add(6, row(&DICO23_ISF, ind[4])?);
            add(9, row(&DICO24_ISF, ind[5])?);
            add(12, row(&DICO25_ISF, ind[6])?);
        }
        Ok(())
    }

    /// `isf_add_mean_and_past`
    fn isf_add_mean_and_past(&mut self) {
        for i in 0..LP_ORDER {
            let tmp = self.isf_cur[i];
            let q = f32::from(ISF_MEAN[i]).mul_add(Q15, self.isf_cur[i]);
            self.isf_cur[i] = PRED_FACTOR.mul_add(f64::from(self.isf_q_past[i]), f64::from(q)) as f32;
            self.isf_q_past[i] = tmp;
        }
    }

    /// `decode_pitch_vector`
    fn decode_pitch_vector(&mut self, subframe: usize) {
        let mode = self.fr_cur_mode;
        let adap = i32::from(self.sub(subframe, 0));
        let (mut lag_int, lag_frac) = if mode <= MODE_8K85 {
            decode_pitch_lag_low(adap, &mut self.base_pitch_lag, subframe, mode)
        } else {
            decode_pitch_lag_high(adap, &mut self.base_pitch_lag, subframe)
        };
        self.pitch_lag_int = lag_int as u8;
        lag_int += i32::from(lag_frac > 0);
        let frac = (lag_frac + if lag_frac > 0 { 0 } else { 4 }) as usize;
        // The bitstream's lags stay inside the history FFmpeg keeps; zeros
        // stand in past it all the same.
        let in_at = EXC as i32 + 1 - lag_int;
        if in_at >= LP_ORDER as i32 {
            interpolatef(&mut self.excitation_buf, EXC, in_at as usize, &AC_INTER, 4, frac, LP_ORDER, SFR + 1);
        } else {
            let pad = (LP_ORDER as i32 - in_at) as usize;
            let mut padded = vec![0.0f32; pad + self.excitation_buf.len()];
            padded[pad..].copy_from_slice(&self.excitation_buf);
            interpolatef(&mut padded, pad + EXC, (pad as i32 + in_at) as usize, &AC_INTER, 4, frac, LP_ORDER, SFR + 1);
            self.excitation_buf.copy_from_slice(&padded[pad..]);
        }
        let ltp = self.sub(subframe, 1) != 0;
        let exc = &mut self.excitation_buf;
        if ltp {
            self.pitch_vector.copy_from_slice(&exc[EXC..EXC + SFR]);
        } else {
            for i in 0..SFR {
                let (a, b, c) = (f64::from(exc[EXC + i - 1]), f64::from(exc[EXC + i]), f64::from(exc[EXC + i + 1]));
                self.pitch_vector[i] = 0.18f64.mul_add(c, 0.18f64.mul_add(a, 0.64 * b)) as f32;
            }
            exc[EXC..EXC + SFR].copy_from_slice(&self.pitch_vector);
        }
    }

    /// `pitch_sharpening`
    fn pitch_sharpening(&mut self) {
        let fv = &mut self.fixed_vector;
        for i in (1..SFR).rev() {
            fv[i] = (-fv[i - 1]).mul_add(self.tilt_coef, fv[i]);
        }
        let lag = usize::from(self.pitch_lag_int);
        for i in lag..SFR {
            fv[i] = f64::from(fv[i - lag]).mul_add(0.85, f64::from(fv[i])) as f32;
        }
    }

    /// `anti_sparseness`: true when the filtered vector in `buf` replaces
    /// the fixed vector.
    fn anti_sparseness(&mut self, buf: &mut [f32; SFR]) -> bool {
        if self.fr_cur_mode > MODE_8K85 {
            return false;
        }
        let pg = f64::from(self.pitch_gain[0]);
        let mut ir_filter_nr: u8 = if pg < 0.6 {
            0
        } else if pg < 0.9 {
            1
        } else {
            2
        };
        if f64::from(self.fixed_gain[0]) > 3.0 * f64::from(self.fixed_gain[1]) {
            if ir_filter_nr < 2 {
                ir_filter_nr += 1;
            }
        } else {
            let count = self.pitch_gain.iter().filter(|&&g| f64::from(g) < 0.6).count();
            if count > 2 {
                ir_filter_nr = 0;
            }
            if u32::from(ir_filter_nr) > u32::from(self.prev_ir_filter_nr) + 1 {
                ir_filter_nr -= 1;
            }
        }
        self.prev_ir_filter_nr = ir_filter_nr;
        ir_filter_nr += u8::from(self.fr_cur_mode == MODE_8K85);
        if ir_filter_nr >= 2 {
            return false;
        }
        let coef = if ir_filter_nr == 0 { &IR_FILTER_STR } else { &IR_FILTER_MID };
        buf.fill(0.0);
        for i in 0..SFR {
            let v = self.fixed_vector[i];
            if v != 0.0 {
                circ_addf_in_place(buf, coef, i, v, SFR);
            }
        }
        true
    }

    /// `synthesis`
    fn synthesis(&mut self, subframe: usize, excitation: &mut [f32; SFR], fixed_gain: f32, fixed_vector: &[f32; SFR]) {
        let pg = self.pitch_gain[0];
        weighted_vector_sumf(excitation, &self.pitch_vector, fixed_vector, pg, fixed_gain, SFR);
        if f64::from(pg) > 0.5 && self.fr_cur_mode <= MODE_8K85 {
            let energy = dot(excitation, excitation, SFR);
            let pitch_factor = (0.25 * f64::from(pg) * f64::from(pg)) as f32;
            for i in 0..SFR {
                excitation[i] = pitch_factor.mul_add(self.pitch_vector[i], excitation[i]);
            }
            scale_vector_to_given_sum_of_squares(excitation, energy);
        }
        let lpc = self.lp_coef[subframe];
        lp_synthesis_filterf(&mut self.samples_az, LP_ORDER, &lpc, excitation, SFR, LP_ORDER);
    }

    /// `find_hb_gain`
    fn find_hb_gain(&self, synth: &[f32], hb_idx: u16, vad: u16) -> Result<f32> {
        if self.fr_cur_mode == MODE_23K85 {
            let g = QUA_HB_GAIN.get(usize::from(hb_idx)).ok_or_else(|| Error::invalid("amr_wb: high-band gain index"))?;
            return Ok(f32::from(*g) * (1.0 / 16384.0));
        }
        let wsp = f64::from(u8::from(vad > 0));
        let tmp = dot(synth, &synth[1..], SFR - 1);
        let tilt = if tmp > 0.0 { tmp / dot(synth, synth, SFR) } else { 0.0 };
        Ok(clipf(((1.0 - f64::from(tilt)) * (-0.25f64).mul_add(wsp, 1.25)) as f32, 0.1f64 as f32, 1.0))
    }

    /// `scaled_hb_excitation`
    fn scaled_hb_excitation(&mut self, hb_exc: &mut [f32; SFR16], synth_exc: &[f32; SFR], hb_gain: f32) {
        let energy = dot(synth_exc, synth_exc, SFR);
        for v in hb_exc.iter_mut() {
            *v = (32768.0 - f64::from(self.prng.get() as u16)) as f32;
        }
        scale_vector_to_given_sum_of_squares(hb_exc, energy * hb_gain * hb_gain);
    }

    /// `hb_synthesis`
    fn hb_synthesis(&mut self, subframe: usize, exc: &[f32; SFR16]) {
        let mut hb_lpc = [0.0f32; LP_ORDER_16K];
        let order = if self.fr_cur_mode == MODE_6K60 {
            let mut e_isf = [0.0f32; LP_ORDER_16K];
            let w = ISFP_INTER[subframe];
            weighted_vector_sumf(&mut e_isf, &self.isf_past_final, &self.isf_cur, w, (1.0 - f64::from(w)) as f32, LP_ORDER);
            extrapolate_isf(&mut e_isf);
            e_isf[LP_ORDER_16K - 1] = (f64::from(e_isf[LP_ORDER_16K - 1]) * 2.0) as f32;
            let mut e_isp = [0.0f64; LP_ORDER_16K];
            lsf2lspd(&mut e_isp, &e_isf, LP_ORDER_16K);
            amrwb_lsp2lpc(&e_isp, &mut hb_lpc, LP_ORDER_16K);
            let lpc = hb_lpc;
            lpc_weighting(&mut hb_lpc, &lpc, 0.9f64 as f32);
            LP_ORDER_16K
        } else {
            lpc_weighting(&mut hb_lpc[..LP_ORDER], &self.lp_coef[subframe], 0.6f64 as f32);
            LP_ORDER
        };
        lp_synthesis_filterf(&mut self.samples_hb, LP_ORDER_16K, &hb_lpc, exc, SFR16, order);
    }

    /// `update_sub_state`
    fn update_sub_state(&mut self) {
        self.excitation_buf.copy_within(SFR..SFR + EXC, 0);
        self.pitch_gain.copy_within(0..5, 1);
        self.fixed_gain[1] = self.fixed_gain[0];
        self.samples_az.copy_within(SFR.., 0);
        self.samples_up.copy_within(SFR.., 0);
        self.samples_hb.copy_within(SFR16.., 0);
    }

    /// One channel's frame of `amrwb_decode_frame`: the bytes it took.
    fn decode_frame(&mut self, buf: &[u8], out: &mut [f32; 4 * SFR16]) -> Result<usize> {
        // decode_mime_header
        let mode = usize::from(buf[0] >> 3 & 0x0F);
        let quality = buf[0] & 0x4 == 0x4;
        let expected_fr_size = ((usize::from(CF_SIZES_WB[mode]) + 7) >> 3) + 1;
        if mode == NO_DATA || !quality {
            out.fill(0.0);
            return Ok(expected_fr_size);
        }
        if mode > MODE_SID {
            return Err(Error::invalid(format!("amr_wb: invalid mode {mode}")));
        }
        if buf.len() < expected_fr_size {
            return Err(Error::invalid("amr_wb: frame too small"));
        }
        if mode == MODE_SID {
            return Err(Error::unsupported("amr_wb: SID frames"));
        }
        self.fr_cur_mode = mode;
        amr_bit_reorder(&mut self.frame, &buf[1..], order_table(mode));

        self.decode_isf_indices()?;
        self.isf_add_mean_and_past();
        set_min_dist_lsf(&mut self.isf_cur, MIN_ISF_SPACING, LP_ORDER - 1);
        let stab_fac = stability_factor(&self.isf_cur, &self.isf_past_final);
        self.isf_cur[LP_ORDER - 1] = (f64::from(self.isf_cur[LP_ORDER - 1]) * 2.0) as f32;
        lsf2lspd(&mut self.isp[3], &self.isf_cur, LP_ORDER);
        if self.first_frame {
            self.first_frame = false;
            self.isp_sub4_past = self.isp[3];
        }
        for k in 0..3 {
            let c = f64::from(ISFP_INTER[k]);
            for i in 0..LP_ORDER {
                self.isp[k][i] = (1.0 - c).mul_add(self.isp_sub4_past[i], c * self.isp[3][i]);
            }
        }
        for sub in 0..4 {
            amrwb_lsp2lpc(&self.isp[sub], &mut self.lp_coef[sub], LP_ORDER);
        }

        for sub in 0..4 {
            self.decode_pitch_vector(sub);
            let (pulse_hi, pulse_lo) = (self.pulses(sub, true), self.pulses(sub, false));
            decode_fixed_vector(&mut self.fixed_vector, &pulse_hi, &pulse_lo, mode);
            self.pitch_sharpening();

            let gains = if mode <= MODE_8K85 {
                row(&QUA_GAIN_6B, self.sub(sub, 2) & 0xFF)?
            } else {
                row(&QUA_GAIN_7B, self.sub(sub, 2) & 0xFF)?
            };
            self.pitch_gain[0] = f32::from(gains[0]) * (1.0 / 16384.0);
            let fixed_gain_factor = f32::from(gains[1]) * (1.0 / 2048.0);

            let energy = dot(&self.fixed_vector, &self.fixed_vector, SFR) / SFR as f32;
            self.fixed_gain[0] = amr_set_fixed_gain(
                fixed_gain_factor,
                energy,
                &mut self.prediction_error,
                ENERGY_MEAN as f32,
                &ENERGY_PRED_FAC,
            );

            let voice_fac = voice_factor(&self.pitch_vector, self.pitch_gain[0], &self.fixed_vector, self.fixed_gain[0]);
            self.tilt_coef = f64::from(voice_fac).mul_add(0.25, 0.25) as f32;

            let (pg, fg) = (self.pitch_gain[0], self.fixed_gain[0]);
            for i in 0..SFR {
                let e = &mut self.excitation_buf[EXC + i];
                *e *= pg;
                *e = fg.mul_add(self.fixed_vector[i], *e);
                *e = e.trunc();
            }

            let synth_fixed_gain = noise_enhancer(fg, &mut self.prev_tr_gain, voice_fac, stab_fac);
            let mut spare = [0.0f32; SFR];
            let mut synth_fixed_vector =
                if self.anti_sparseness(&mut spare) { spare } else { self.fixed_vector };
            pitch_enhancer(&mut synth_fixed_vector, voice_fac);

            let mut synth_exc = [0.0f32; SFR];
            self.synthesis(sub, &mut synth_exc, synth_fixed_gain, &synth_fixed_vector);

            // de_emphasis
            let m = PREEMPH_FAC as f32;
            let (az, up) = (&self.samples_az, &mut self.samples_up);
            up[UPS_MEM_SIZE] = m.mul_add(self.demph_mem, az[LP_ORDER]);
            for i in 1..SFR {
                up[UPS_MEM_SIZE + i] = up[UPS_MEM_SIZE + i - 1].mul_add(m, az[LP_ORDER + i]);
            }
            self.demph_mem = up[UPS_MEM_SIZE + SFR - 1];

            apply_order_2_transfer_function(
                &mut self.samples_up[UPS_MEM_SIZE..],
                &[HPF_ZEROS[0], HPF_ZEROS[1]],
                &[HPF_31_POLES[0], HPF_31_POLES[1]],
                HPF_31_GAIN,
                &mut self.hpf_31_mem,
            );

            let sub_buf: &mut [f32; SFR16] = (&mut out[sub * SFR16..(sub + 1) * SFR16]).try_into().unwrap();
            upsample_5_4(sub_buf, &self.samples_up);

            let mut hb_samples = [0.0f32; SFR16];
            hb_samples[..SFR].copy_from_slice(&self.samples_up[UPS_MEM_SIZE..]);
            apply_order_2_transfer_function(
                &mut hb_samples[..SFR],
                &[HPF_ZEROS[0], HPF_ZEROS[1]],
                &[HPF_400_POLES[0], HPF_400_POLES[1]],
                HPF_400_GAIN,
                &mut self.hpf_400_mem,
            );
            let hb_gain = self.find_hb_gain(&hb_samples[..SFR], self.sub(sub, 3), self.frame[0])?;

            let mut hb_exc = [0.0f32; SFR16];
            self.scaled_hb_excitation(&mut hb_exc, &synth_exc, hb_gain);
            self.hb_synthesis(sub, &hb_exc);

            let hb_in: [f32; SFR16] = self.samples_hb[LP_ORDER_16K..].try_into().unwrap();
            hb_fir_filter(&mut hb_samples, &BPF_6_7_COEF, &mut self.bpf_6_7_mem, &hb_in);
            if mode == MODE_23K85 {
                let input = hb_samples;
                hb_fir_filter(&mut hb_samples, &LPF_7_COEF, &mut self.lpf_7_mem, &input);
            }
            for i in 0..SFR16 {
                sub_buf[i] = (sub_buf[i] + hb_samples[i]) * Q15;
            }
            self.update_sub_state();
        }
        self.isp_sub4_past = self.isp[3];
        self.isf_past_final = self.isf_cur;
        Ok(expected_fr_size)
    }
}

/// `decode_pitch_lag_high`
fn decode_pitch_lag_high(pitch_index: i32, base_lag_int: &mut u8, subframe: usize) -> (i32, i32) {
    if subframe == 0 || subframe == 2 {
        let (lag_int, lag_frac) = if pitch_index < 376 {
            let lag_int = (pitch_index + 137) >> 2;
            (lag_int, pitch_index - (lag_int << 2) + 136)
        } else if pitch_index < 440 {
            let lag_int = (pitch_index + 257 - 376) >> 1;
            (lag_int, (pitch_index - (lag_int << 1) + 256 - 376) * 2)
        } else {
            (pitch_index - 280, 0)
        };
        *base_lag_int = (lag_int - 8 - i32::from(lag_frac < 0)).clamp(P_DELAY_MIN, P_DELAY_MAX - 15) as u8;
        (lag_int, lag_frac)
    } else {
        let lag_int = (pitch_index + 1) >> 2;
        let lag_frac = pitch_index - (lag_int << 2);
        (lag_int + i32::from(*base_lag_int), lag_frac)
    }
}

/// `decode_pitch_lag_low`
fn decode_pitch_lag_low(pitch_index: i32, base_lag_int: &mut u8, subframe: usize, mode: usize) -> (i32, i32) {
    if subframe == 0 || (subframe == 2 && mode != MODE_6K60) {
        let (lag_int, lag_frac) = if pitch_index < 116 {
            let lag_int = (pitch_index + 69) >> 1;
            (lag_int, (pitch_index - (lag_int << 1) + 68) * 2)
        } else {
            (pitch_index - 24, 0)
        };
        *base_lag_int = (lag_int - 8 - i32::from(lag_frac < 0)).clamp(P_DELAY_MIN, P_DELAY_MAX - 15) as u8;
        (lag_int, lag_frac)
    } else {
        let lag_int = (pitch_index + 1) >> 1;
        let lag_frac = (pitch_index - (lag_int << 1)) * 2;
        (lag_int + i32::from(*base_lag_int), lag_frac)
    }
}

/// `BIT_STR(x, lsb, len)`
fn bit_str(x: i32, lsb: i32, len: i32) -> i32 {
    ((x as u32 >> lsb) & ((1u32 << len) - 1)) as i32
}

/// `BIT_POS(x, p)`
fn bit_pos(x: i32, p: i32) -> i32 {
    (x >> p) & 1
}

fn decode_1p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let pos = bit_str(code, 0, m) + off;
    out[0] = if bit_pos(code, m) != 0 { -pos } else { pos };
}

fn decode_2p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let pos0 = bit_str(code, m, m) + off;
    let pos1 = bit_str(code, 0, m) + off;
    let neg = bit_pos(code, 2 * m) != 0;
    out[0] = if neg { -pos0 } else { pos0 };
    out[1] = if neg { -pos1 } else { pos1 };
    if pos0 > pos1 {
        out[1] = -out[1];
    }
}

fn decode_3p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let half_2p = bit_pos(code, 2 * m - 1) << (m - 1);
    decode_2p_track(out, bit_str(code, 0, 2 * m - 1), m - 1, off + half_2p);
    decode_1p_track(&mut out[2..], bit_str(code, 2 * m, m + 1), m, off);
}

fn decode_4p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let b_offset = 1 << (m - 1);
    match bit_str(code, 4 * m - 2, 2) {
        0 => {
            let half_4p = bit_pos(code, 4 * m - 3) << (m - 1);
            let subhalf_2p = bit_pos(code, 2 * m - 3) << (m - 2);
            decode_2p_track(out, bit_str(code, 0, 2 * m - 3), m - 2, off + half_4p + subhalf_2p);
            decode_2p_track(&mut out[2..], bit_str(code, 2 * m - 2, 2 * m - 1), m - 1, off + half_4p);
        }
        1 => {
            decode_1p_track(out, bit_str(code, 3 * m - 2, m), m - 1, off);
            decode_3p_track(&mut out[1..], bit_str(code, 0, 3 * m - 2), m - 1, off + b_offset);
        }
        2 => {
            decode_2p_track(out, bit_str(code, 2 * m - 1, 2 * m - 1), m - 1, off);
            decode_2p_track(&mut out[2..], bit_str(code, 0, 2 * m - 1), m - 1, off + b_offset);
        }
        _ => {
            decode_3p_track(out, bit_str(code, m, 3 * m - 2), m - 1, off);
            decode_1p_track(&mut out[3..], bit_str(code, 0, m), m - 1, off + b_offset);
        }
    }
}

fn decode_5p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let half_3p = bit_pos(code, 5 * m - 1) << (m - 1);
    decode_3p_track(out, bit_str(code, 2 * m + 1, 3 * m - 2), m - 1, off + half_3p);
    decode_2p_track(&mut out[3..], bit_str(code, 0, 2 * m + 1), m, off);
}

fn decode_6p_track(out: &mut [i32], code: i32, m: i32, off: i32) {
    let b_offset = 1 << (m - 1);
    let half_more = bit_pos(code, 6 * m - 5) << (m - 1);
    let half_other = b_offset - half_more;
    match bit_str(code, 6 * m - 4, 2) {
        0 => {
            decode_1p_track(out, bit_str(code, 0, m), m - 1, off + half_more);
            decode_5p_track(&mut out[1..], bit_str(code, m, 5 * m - 5), m - 1, off + half_more);
        }
        1 => {
            decode_1p_track(out, bit_str(code, 0, m), m - 1, off + half_other);
            decode_5p_track(&mut out[1..], bit_str(code, m, 5 * m - 5), m - 1, off + half_more);
        }
        2 => {
            decode_2p_track(out, bit_str(code, 0, 2 * m - 1), m - 1, off + half_other);
            decode_4p_track(&mut out[2..], bit_str(code, 2 * m - 1, 4 * m - 4), m - 1, off + half_more);
        }
        _ => {
            decode_3p_track(out, bit_str(code, 3 * m - 2, 3 * m - 2), m - 1, off);
            decode_3p_track(&mut out[3..], bit_str(code, 0, 3 * m - 2), m - 1, off + b_offset);
        }
    }
}

/// `decode_fixed_vector`
fn decode_fixed_vector(fixed_vector: &mut [f32; SFR], pulse_hi: &[u16; 4], pulse_lo: &[u16; 4], mode: usize) {
    let mut sig_pos = [[0i32; 6]; 4];
    let spacing = if mode == MODE_6K60 { 2 } else { 4 };
    let lo = |i: usize| i32::from(pulse_lo[i]);
    let hi = |i: usize| i32::from(pulse_hi[i]);
    match mode {
        MODE_6K60 => (0..2).for_each(|i| decode_1p_track(&mut sig_pos[i], lo(i), 5, 1)),
        MODE_8K85 => (0..4).for_each(|i| decode_1p_track(&mut sig_pos[i], lo(i), 4, 1)),
        MODE_12K65 => (0..4).for_each(|i| decode_2p_track(&mut sig_pos[i], lo(i), 4, 1)),
        MODE_14K25 => {
            (0..2).for_each(|i| decode_3p_track(&mut sig_pos[i], lo(i), 4, 1));
            (2..4).for_each(|i| decode_2p_track(&mut sig_pos[i], lo(i), 4, 1));
        }
        MODE_15K85 => (0..4).for_each(|i| decode_3p_track(&mut sig_pos[i], lo(i), 4, 1)),
        MODE_18K25 => (0..4).for_each(|i| decode_4p_track(&mut sig_pos[i], lo(i) + (hi(i) << 14), 4, 1)),
        MODE_19K85 => {
            (0..2).for_each(|i| decode_5p_track(&mut sig_pos[i], lo(i) + (hi(i) << 10), 4, 1));
            (2..4).for_each(|i| decode_4p_track(&mut sig_pos[i], lo(i) + (hi(i) << 14), 4, 1));
        }
        _ => (0..4).for_each(|i| decode_6p_track(&mut sig_pos[i], lo(i) + (hi(i) << 11), 4, 1)),
    }
    fixed_vector.fill(0.0);
    for i in 0..4 {
        for j in 0..usize::from(PULSES_NB_PER_MODE_TR[mode][i]) {
            let pos = (sig_pos[i][j].abs() - 1) * spacing + i as i32;
            if let Some(v) = usize::try_from(pos).ok().and_then(|p| fixed_vector.get_mut(p)) {
                *v += if sig_pos[i][j] < 0 { -1.0 } else { 1.0 };
            }
        }
    }
}

/// `voice_factor`
fn voice_factor(p_vector: &[f32; SFR], p_gain: f32, f_vector: &[f32; SFR], f_gain: f32) -> f32 {
    let p_ener = f64::from(dot(p_vector, p_vector, SFR)) * f64::from(p_gain) * f64::from(p_gain);
    let f_ener = f64::from(dot(f_vector, f_vector, SFR)) * f64::from(f_gain) * f64::from(f_gain);
    ((p_ener - f_ener) / (p_ener + f_ener + 0.01)) as f32
}

/// `stability_factor`
fn stability_factor(isf: &[f32; LP_ORDER], isf_past: &[f32; LP_ORDER]) -> f32 {
    let mut acc = 0.0f32;
    for i in 0..LP_ORDER - 1 {
        let d = isf[i] - isf_past[i];
        acc = d.mul_add(d, acc);
    }
    let v = (-(f64::from(acc) * 0.8)).mul_add(512.0, 1.25);
    (if 0.0 > v { 0.0 } else { v }) as f32
}

/// `noise_enhancer`
fn noise_enhancer(fixed_gain: f32, prev_tr_gain: &mut f32, voice_fac: f32, stab_fac: f32) -> f32 {
    let sm_fac = (0.5 * f64::from(1.0 - voice_fac) * f64::from(stab_fac)) as f32;
    let g0 = if fixed_gain < *prev_tr_gain {
        let up = fixed_gain.mul_add(6226.0 * Q15, fixed_gain);
        if *prev_tr_gain > up { up } else { *prev_tr_gain }
    } else {
        let down = fixed_gain * (27536.0 * Q15);
        if *prev_tr_gain > down { *prev_tr_gain } else { down }
    };
    *prev_tr_gain = g0;
    sm_fac.mul_add(g0, (1.0 - sm_fac) * fixed_gain)
}

/// `pitch_enhancer`
fn pitch_enhancer(fixed_vector: &mut [f32; SFR], voice_fac: f32) {
    let cpe = (0.125 * f64::from(1.0 + voice_fac)) as f32;
    let mut last = fixed_vector[0];
    fixed_vector[0] = (-cpe).mul_add(fixed_vector[1], fixed_vector[0]);
    for i in 1..SFR - 1 {
        let cur = fixed_vector[i];
        fixed_vector[i] = (-cpe).mul_add(last + fixed_vector[i + 1], fixed_vector[i]);
        last = cur;
    }
    fixed_vector[SFR - 1] = (-cpe).mul_add(last, fixed_vector[SFR - 1]);
}

/// `upsample_5_4`: 80 samples from the 64 at `samples_up[UPS_FIR_SIZE..]`
/// and the memory before them.
fn upsample_5_4(out: &mut [f32; SFR16], samples_up: &[f32]) {
    let input = UPS_FIR_SIZE;
    let in0 = input + 1 - UPS_FIR_SIZE;
    let mut i = 0;
    let mut int_part = 0;
    for _ in 0..SFR16 / 5 {
        out[i] = samples_up[input + int_part];
        let mut frac_part = 4;
        i += 1;
        for _ in 1..5 {
            out[i] = dot(&samples_up[in0 + int_part..], &UPSAMPLE_FIR[4 - frac_part], UPS_MEM_SIZE);
            int_part += 1;
            frac_part -= 1;
            i += 1;
        }
    }
}

/// `auto_correlation`
fn auto_correlation(diff_isf: &[f32], mean: f32, lag: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 7..LP_ORDER - 2 {
        let prod = (diff_isf[i] - mean) * (diff_isf[i - lag] - mean);
        sum = prod.mul_add(prod, sum);
    }
    sum
}

/// `extrapolate_isf`
fn extrapolate_isf(isf: &mut [f32; LP_ORDER_16K]) {
    let mut diff_isf = [0.0f32; LP_ORDER - 2];
    isf[LP_ORDER_16K - 1] = isf[LP_ORDER - 1];
    for i in 0..LP_ORDER - 2 {
        diff_isf[i] = isf[i + 1] - isf[i];
    }
    let mut diff_mean = 0.0f32;
    for &d in &diff_isf[2..LP_ORDER - 2] {
        diff_mean = d.mul_add(1.0f32 / (LP_ORDER - 4) as f32, diff_mean);
    }
    let mut corr_lag = [0.0f32; 3];
    let mut i_max_corr = 0;
    for i in 0..3 {
        corr_lag[i] = auto_correlation(&diff_isf, diff_mean, i + 2);
        if corr_lag[i] > corr_lag[i_max_corr] {
            i_max_corr = i;
        }
    }
    i_max_corr += 1;
    for i in LP_ORDER - 1..LP_ORDER_16K - 1 {
        isf[i] = isf[i - 1] + isf[i - 1 - i_max_corr] - isf[i - 2 - i_max_corr];
    }
    let est = (7965.0 + f64::from(isf[2] - isf[3] - isf[4]) / 6.0) as f32;
    let capped = if est > 7600.0 { 7600.0 } else { est };
    let scale = (0.5 * f64::from(capped - isf[LP_ORDER - 2]) / f64::from(isf[LP_ORDER_16K - 2] - isf[LP_ORDER - 2])) as f32;
    for (j, i) in (LP_ORDER - 1..LP_ORDER_16K - 1).enumerate() {
        diff_isf[j] = scale * (isf[i] - isf[i - 1]);
    }
    for i in 1..LP_ORDER_16K - LP_ORDER {
        if f64::from(diff_isf[i] + diff_isf[i - 1]) < 5.0 {
            if diff_isf[i] > diff_isf[i - 1] {
                diff_isf[i - 1] = (5.0 - f64::from(diff_isf[i])) as f32;
            } else {
                diff_isf[i] = (5.0 - f64::from(diff_isf[i - 1])) as f32;
            }
        }
    }
    for (j, i) in (LP_ORDER - 1..LP_ORDER_16K - 1).enumerate() {
        isf[i] = diff_isf[j].mul_add(Q15, isf[i - 1]);
    }
    for v in &mut isf[..LP_ORDER_16K - 1] {
        *v = (f64::from(*v) * 0.8) as f32;
    }
}

/// `lpc_weighting`
fn lpc_weighting(out: &mut [f32], lpc: &[f32], gamma: f32) {
    let mut fac = gamma;
    for (o, &l) in out.iter_mut().zip(lpc) {
        *o = l * fac;
        fac *= gamma;
    }
}

/// `hb_fir_filter`
fn hb_fir_filter(out: &mut [f32; SFR16], coef: &[f32; HB_FIR_SIZE + 1], mem: &mut [f32; HB_FIR_SIZE], input: &[f32; SFR16]) {
    let mut data = [0.0f32; SFR16 + HB_FIR_SIZE];
    data[..HB_FIR_SIZE].copy_from_slice(mem);
    data[HB_FIR_SIZE..].copy_from_slice(input);
    for i in 0..SFR16 {
        let mut v = 0.0f32;
        for j in 0..=HB_FIR_SIZE {
            v = data[i + j].mul_add(coef[j], v);
        }
        out[i] = v;
    }
    mem.copy_from_slice(&data[SFR16..]);
}

pub struct AMRWBDecoder {
    codec_id: CodecId,
    channels: usize,
    ch: Vec<AMRWBContext>,
    queue: VecDeque<Frame>,
    pts_tracker: i64,
}

impl AMRWBDecoder {
    /// `amrwb_decode_init`: one or two channels (none given means one).
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = match params.channels.unwrap_or(0) {
            0 => 1,
            n @ 1..=2 => usize::from(n),
            n => return Err(Error::unsupported(format!("amr_wb: {n} channels"))),
        };
        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels,
            ch: (0..channels).map(|_| AMRWBContext::new()).collect(),
            queue: VecDeque::new(),
            pts_tracker: 0,
        })
    }
}

impl Decoder for AMRWBDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// `amrwb_decode_frame` as libavcodec calls it: each call decodes one
    /// frame per channel, in channel order, and the rest of the packet
    /// goes to the next call. Output is planar float.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data = packet.data.as_slice();
        let mut cur_pts = packet.pts.unwrap_or(self.pts_tracker);
        while !data.is_empty() {
            let mut planes = Vec::with_capacity(self.channels);
            for ch in &mut self.ch {
                if data.is_empty() {
                    return Err(Error::invalid("amr_wb: packet ends before the frame of every channel"));
                }
                let mut pcm = [0.0f32; 4 * SFR16];
                let consumed = ch.decode_frame(data, &mut pcm)?;
                data = data.get(consumed..).unwrap_or(&[]);
                planes.push(pcm.iter().flat_map(|s| s.to_le_bytes()).collect());
            }
            self.queue.push_back(Frame::Audio(AudioFrame {
                samples: (4 * SFR16) as u32,
                pts: Some(cur_pts),
                data: planes,
            }));
            cur_pts += (4 * SFR16) as i64;
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
            *ch = AMRWBContext::new();
        }
        self.queue.clear();
        self.pts_tracker = 0;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::F32P, sample_rate: 16_000, channels: self.channels as u16 })
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(AMRWBDecoder::new(params)?))
}

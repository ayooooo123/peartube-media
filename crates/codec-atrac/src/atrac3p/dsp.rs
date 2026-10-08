// Port of FFmpeg's ATRAC3+ DSP routines (libavcodec/atrac3plusdsp.c, FFmpeg
// commit 2da55bf): sine-wave synthesis, power compensation, IMDCT
// windowing and the inverse PQF.
// Copyright (c) 2010-2013 Maxim Poliakovski; LGPL-2.1-or-later (see LICENSE).

use super::tables::*;
use super::{
    CH_UNIT_STEREO, ChanUnit, IpqfChannel, POWER_COMP_OFF, SUBBAND_SAMPLES, SUBBANDS, WaveEnvelope,
    WaveSynthParams, WavesData,
};
use crate::atrac1::sine_window;
use crate::tx::Imdct;

/// The tables `ff_atrac3p_init_dsp_static` builds, and the sine windows.
pub(crate) struct Tables {
    sine_table: Vec<f32>,
    hann_window: Vec<f32>,
    amp_sf_tab: [f32; 64],
    sine_64: Vec<f32>,
    sine_128: Vec<f32>,
}

impl Tables {
    pub(crate) fn new() -> Self {
        let two_pi = 2.0 * std::f64::consts::PI;
        let sine_table = (0..2048)
            .map(|i| (two_pi * f64::from(i) / 2048.0).sin() as f32)
            .collect();
        let hann_window = (0..256)
            .map(|i| ((1.0 - (two_pi * f64::from(i) / 256.0).cos()) * 0.5) as f32)
            .collect();
        let mut amp_sf_tab = [0f32; 64];
        for (i, v) in amp_sf_tab.iter_mut().enumerate() {
            *v = ((i as f32 - 3.0) / 4.0).exp2();
        }
        Self {
            sine_table,
            hann_window,
            amp_sf_tab,
            sine_64: sine_window(64),
            sine_128: sine_window(128),
        }
    }
}

/// `waves_synth`.
fn waves_synth(
    t: &Tables,
    synth: &WaveSynthParams,
    waves_info: &WavesData,
    envelope: &WaveEnvelope,
    invert_phase: bool,
    reg_offset: i32,
    out: &mut [f32; 128],
) {
    let start = waves_info.start_index.clamp(0, 48) as usize;
    let count = (waves_info.num_wavs.max(0) as usize).min(48 - start);
    for wave in &synth.waves[start..start + count] {
        // amplitude dequantization
        let amp_mul = if synth.amplitude_mode == 0 {
            (wave.amp_index + 1) as f32 / 15.13
        } else {
            1.0
        };
        let amp = f64::from(t.amp_sf_tab[(wave.amp_sf & 63) as usize] * amp_mul);
        let inc = wave.freq_index;
        let mut pos = (((wave.phase_index & 0x1F) << 6)
            .wrapping_sub((reg_offset ^ 128).wrapping_mul(inc)))
            & 2047;
        // waveform generation
        for o in out.iter_mut() {
            *o = (f64::from(*o) + f64::from(t.sine_table[pos as usize]) * amp) as f32;
            pos = pos.wrapping_add(inc) & 2047;
        }
    }

    if invert_phase {
        for o in out.iter_mut() {
            *o *= -1.0;
        }
    }

    // fade in with a steep Hann window
    if envelope.has_start_point {
        let pos = (envelope.start_pos << 2) - reg_offset;
        if pos > 0 && pos <= 128 {
            let pos = pos as usize;
            out[..pos].fill(0.0);
            if !envelope.has_stop_point || envelope.start_pos != envelope.stop_pos {
                for (k, h) in [0usize, 32, 64, 96].into_iter().enumerate() {
                    if let Some(o) = out.get_mut(pos + k) {
                        *o *= t.hann_window[h];
                    }
                }
            }
        }
    }

    // fade out with a steep Hann window
    if envelope.has_stop_point {
        let pos = ((envelope.stop_pos + 1) << 2) - reg_offset;
        if pos > 0 && pos <= 128 {
            let pos = pos as usize;
            for (k, h) in [96usize, 64, 32, 0].into_iter().enumerate() {
                if let Some(o) = (pos + k).checked_sub(4).and_then(|i| out.get_mut(i)) {
                    *o *= t.hann_window[h];
                }
            }
            out[pos..].fill(0.0);
        }
    }
}

/// `ff_atrac3p_generate_tones`: adds the tones of subband `sb` to `out`.
pub(super) fn generate_tones(
    t: &Tables,
    unit: &mut ChanUnit,
    ch_num: usize,
    sb: usize,
    out: &mut [f32],
) {
    let mut wavreg1 = [0f32; 128];
    let mut wavreg2 = [0f32; 128];
    let chan = &mut unit.channels[ch_num];
    let prev = 1 - chan.cur;
    let tones_now = chan.tones_info_hist[prev][sb];
    let mut tones_next = chan.tones_info_hist[chan.cur][sb];

    // the full envelopes of both overlapping regions from the truncated
    // bitstream data
    if tones_next.pend_env.has_start_point
        && tones_next.pend_env.start_pos < tones_next.pend_env.stop_pos
    {
        tones_next.curr_env.has_start_point = true;
        tones_next.curr_env.start_pos = tones_next.pend_env.start_pos + 32;
    } else if tones_now.pend_env.has_start_point {
        tones_next.curr_env.has_start_point = true;
        tones_next.curr_env.start_pos = tones_now.pend_env.start_pos;
    } else {
        tones_next.curr_env.has_start_point = false;
        tones_next.curr_env.start_pos = 0;
    }

    if tones_now.pend_env.has_stop_point
        && tones_now.pend_env.stop_pos >= tones_next.curr_env.start_pos
    {
        tones_next.curr_env.has_stop_point = true;
        tones_next.curr_env.stop_pos = tones_now.pend_env.stop_pos;
    } else if tones_next.pend_env.has_stop_point {
        tones_next.curr_env.has_stop_point = true;
        tones_next.curr_env.stop_pos = tones_next.pend_env.stop_pos + 32;
    } else {
        tones_next.curr_env.has_stop_point = false;
        tones_next.curr_env.stop_pos = 64;
    }
    chan.tones_info_hist[chan.cur][sb] = tones_next;

    // is the visible part of the envelope non-zero?
    let reg1_env_nonzero = tones_now.curr_env.stop_pos >= 32;
    let reg2_env_nonzero = tones_next.curr_env.start_pos < 32;

    // synthesize the waves of both overlapping regions
    let (info, info_prev) = (unit.waves_info(), unit.waves_info_prev());
    if tones_now.num_wavs != 0 && reg1_env_nonzero {
        let invert = info_prev.invert_phase[sb] & ch_num as u8 != 0;
        waves_synth(
            t,
            info_prev,
            &tones_now,
            &tones_now.curr_env,
            invert,
            128,
            &mut wavreg1,
        );
    }
    if tones_next.num_wavs != 0 && reg2_env_nonzero {
        let invert = info.invert_phase[sb] & ch_num as u8 != 0;
        waves_synth(
            t,
            info,
            &tones_next,
            &tones_next.curr_env,
            invert,
            0,
            &mut wavreg2,
        );
    }

    // Hann windowing of the waves without fades
    let hann = &t.hann_window;
    let apply = |reg: &mut [f32; 128], win: &[f32]| {
        for (r, &w) in reg.iter_mut().zip(win) {
            *r *= w;
        }
    };
    if tones_now.num_wavs != 0 && tones_next.num_wavs != 0 && reg1_env_nonzero && reg2_env_nonzero {
        apply(&mut wavreg1, &hann[128..]);
        apply(&mut wavreg2, &hann[..128]);
    } else {
        if tones_now.num_wavs != 0 && !tones_now.curr_env.has_stop_point {
            apply(&mut wavreg1, &hann[128..]);
        }
        if tones_next.num_wavs != 0 && !tones_next.curr_env.has_start_point {
            apply(&mut wavreg2, &hann[..128]);
        }
    }

    // overlap and add to the residual
    for i in 0..128 {
        out[i] += wavreg1[i] + wavreg2[i];
    }
}

/// `ff_atrac3p_power_compensation`.
pub(super) fn power_compensation(
    unit: &ChanUnit,
    ch_index: usize,
    sp: &mut [f32],
    rng_index: usize,
    sb: usize,
) {
    let swap_ch = usize::from(unit.unit_type == CH_UNIT_STEREO && unit.swap_channels[sb] != 0);
    let src = &unit.channels[ch_index ^ swap_ch];
    let level = src.power_levs[usize::from(SUBBAND_TO_POWGRP[sb])];
    if level == POWER_COMP_OFF {
        return;
    }

    // initial noise spectrum
    let mut pwcsp = [0f32; SUBBAND_SAMPLES];
    for (i, p) in pwcsp.iter_mut().enumerate() {
        *p = NOISE_TAB[(rng_index + i) & 0x3FF];
    }

    // gain control information
    let g1 = &src.gain_data()[sb];
    let g2 = &src.gain_data_prev()[sb];
    let gain_lev = if g1.num_points > 0 {
        6 - g1.lev_code[0]
    } else {
        0
    };
    let mut gcv = 0i32;
    for i in 0..(g2.num_points.max(0) as usize).min(7) {
        gcv = gcv.max(gain_lev - (g2.lev_code[i] - 6));
    }
    for i in 0..(g1.num_points.max(0) as usize).min(7) {
        gcv = gcv.max(6 - g1.lev_code[i]);
    }
    let grp_lev = PWC_LEVS[usize::from(level) & 15] / (1i64 << gcv.clamp(0, 62)) as f32;

    // the lowest two quant units (0...351 Hz) of subband 0 are skipped
    let chan = &unit.channels[ch_index];
    let first = usize::from(SUBBAND_TO_QU[sb]) + if sb == 0 { 2 } else { 0 };
    for qu in first..usize::from(SUBBAND_TO_QU[sb + 1]) {
        let wl = chan.qu_wordlen[qu];
        if wl <= 0 {
            continue;
        }
        let qu_lev = SF_TAB[chan.qu_sf_idx[qu] as usize & 63] * MANT_TAB[wl as usize & 7]
            / (1i32 << (wl & 7)) as f32
            * grp_lev;
        let start = usize::from(QU_TO_SPEC_POS[qu]);
        let nsp = usize::from(QU_TO_SPEC_POS[qu + 1]) - start;
        for (d, &p) in sp[start..start + nsp].iter_mut().zip(&pwcsp) {
            *d += p * qu_lev;
        }
    }
}

/// `ff_atrac3p_imdct`: inverse transform of one subband and its window.
pub(super) fn imdct(
    t: &Tables,
    mdct: &Imdct,
    input: &mut [f32],
    out: &mut [f32; 2 * SUBBAND_SAMPLES],
    wind_id: u8,
    sb: usize,
) {
    if sb & 1 != 0 {
        input[..SUBBAND_SAMPLES].reverse();
    }
    mdct.full(out, &input[..SUBBAND_SAMPLES]);

    // ATRAC3+ windows: the plain sine window of 256, or the sine window
    // of 128 wrapped into 32 zeros (start) and 32 ones (end)
    if wind_id & 2 != 0 {
        // first half: steep window
        out[..32].fill(0.0);
        for (o, &w) in out[32..96].iter_mut().zip(&t.sine_64) {
            *o *= w;
        }
    } else {
        // first half: simple sine window
        for (o, &w) in out[..128].iter_mut().zip(&t.sine_128) {
            *o *= w;
        }
    }
    if wind_id & 1 != 0 {
        // second half: steep window
        for (o, &w) in out[160..224].iter_mut().zip(t.sine_64.iter().rev()) {
            *o *= w;
        }
        out[224..].fill(0.0);
    } else {
        // second half: simple sine window
        for (o, &w) in out[128..].iter_mut().zip(t.sine_128.iter().rev()) {
            *o *= w;
        }
    }
}

/// `ff_atrac3p_ipqf`: the 16-band inverse PQF.
pub(super) fn ipqf(dct: &Imdct, hist: &mut IpqfChannel, input: &[f32], out: &mut [f32]) {
    out.fill(0.0);
    let mut idct_in = [0f32; SUBBANDS];
    let mut idct_out = [0f32; SUBBANDS];
    for s in 0..SUBBAND_SAMPLES {
        // one sample from each subband
        for (sb, v) in idct_in.iter_mut().enumerate() {
            *v = input[sb * SUBBAND_SAMPLES + s];
        }
        // the sine and cosine parts of the PQF through an IDCT-IV
        dct.half(&mut idct_out, &idct_in);

        // append the result to the history
        for i in 0..8 {
            hist.buf1[hist.pos][i] = idct_out[i + 8];
            hist.buf2[hist.pos][i] = idct_out[7 - i];
        }

        let mut pos_now = hist.pos;
        let mut pos_next = MOD23_LUT[pos_now + 2]; // (pos_now + 1) % 23
        for t in 0..super::PQF_FIR_LEN {
            for i in 0..8 {
                out[s * 16 + i] += hist.buf1[pos_now][i] * IPQF_COEFFS1[t][i]
                    + hist.buf2[pos_next][i] * IPQF_COEFFS2[t][i];
                out[s * 16 + i + 8] += hist.buf1[pos_now][7 - i] * IPQF_COEFFS1[t][i + 8]
                    + hist.buf2[pos_next][7 - i] * IPQF_COEFFS2[t][i + 8];
            }
            pos_now = MOD23_LUT[pos_next + 2]; // (pos_now + 2) % 23
            pos_next = MOD23_LUT[pos_now + 2]; // (pos_next + 2) % 23
        }
        hist.pos = MOD23_LUT[hist.pos]; // (pos - 1) % 23
    }
}

// QCELP decoder.
//
// Ported from FFmpeg libavcodec/qcelpdec.c (commit 2da55bf),
// LGPL-2.1-or-later.

//! QCELP-13K / PureVoice (TIA/EIA/IS-733) as FFmpeg decodes it: the rate
//! from the packet size (and its rate byte), the frame unpacked by the
//! rate's bitmap, codebook vectors, pitch synthesis and pre-filters, LSP
//! interpolation, formant synthesis and the post-filter; bad frames are
//! concealed as FFmpeg's erasure path does. 160 samples per packet at
//! 8 kHz, float. Arithmetic as in FFmpeg's C (see `celp`).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::celp::*;
use crate::qcelp_data::*;

const SFR_LEN: usize = 160;
/// `qcelp_packet_rate`
const I_F_Q: i32 = -1;
const SILENCE: i32 = 0;
const RATE_OCTAVE: i32 = 1;
const RATE_QUARTER: i32 = 2;
const RATE_HALF: i32 = 3;
const RATE_FULL: i32 = 4;

const QCELP_RATE_FULL_CODEBOOK_RATIO: f64 = 0.01;
const QCELP_RATE_HALF_CODEBOOK_RATIO: f64 = 0.5;
const QCELP_SQRT1887: f64 = 1.373681186;
const QCELP_LSP_SPREAD_FACTOR: f64 = 0.02;
const QCELP_BANDWIDTH_EXPANSION_COEFF: f64 = 0.9883;

fn lspvq(i: usize) -> &'static [[i16; 2]] {
    match i {
        0 => &LSPVQ1,
        1 => &LSPVQ2,
        2 => &LSPVQ3,
        3 => &LSPVQ4,
        _ => &LSPVQ5,
    }
}

fn bitmap(rate: i32) -> &'static [Bitmap] {
    match rate {
        RATE_OCTAVE => &RATE_OCTAVE_BITMAP,
        RATE_QUARTER => &RATE_QUARTER_BITMAP,
        RATE_HALF => &RATE_HALF_BITMAP,
        _ => &RATE_FULL_BITMAP,
    }
}

/// `buf_size2bitrate`
fn buf_size2bitrate(buf_size: usize) -> i32 {
    match buf_size {
        35 => RATE_FULL,
        17 => RATE_HALF,
        8 => RATE_QUARTER,
        4 => RATE_OCTAVE,
        1 => SILENCE,
        _ => I_F_Q,
    }
}

/// FFmpeg's default `GetBitContext`: most significant bit first; reads
/// past the data give zeros.
struct BitReader<'a> {
    data: &'a [u8],
    index: usize,
}

impl BitReader<'_> {
    fn get(&mut self, n: u32) -> u32 {
        let mut v = 0;
        for _ in 0..n {
            let byte = self.data.get(self.index >> 3).copied().unwrap_or(0);
            v = (v << 1) | u32::from((byte >> (7 - (self.index & 7))) & 1);
            self.index += 1;
        }
        v
    }
}

/// An index into a table that must hold it, or invalid data.
fn get<T: Copy>(table: &[T], index: usize) -> Result<T> {
    table.get(index).copied().ok_or_else(|| Error::invalid("qcelp: table index out of range"))
}

/// `QCELPContext`
struct QCELPContext {
    bitrate: i32,
    /// `QCELPFrame` as its bytes.
    frame: [u8; FRAME_BYTES],
    erasure_count: u8,
    octave_count: u8,
    prev_lspf: [f32; 10],
    predictor_lspf: [f32; 10],
    pitch_synthesis_filter_mem: [f32; 303],
    pitch_pre_filter_mem: [f32; 303],
    rnd_fir_filter_mem: [f32; 180],
    formant_mem: [f32; 170],
    last_codebook_gain: f32,
    prev_g1: [i32; 2],
    prev_bitrate: i32,
    pitch_gain: [f32; 4],
    pitch_lag: [u8; 4],
    first16bits: u16,
    postfilter_synth_mem: [f32; 10],
    postfilter_agc_mem: f32,
    postfilter_tilt_mem: f32,
}

impl QCELPContext {
    /// `qcelp_decode_init`
    fn new() -> Self {
        Self {
            bitrate: SILENCE,
            frame: [0; FRAME_BYTES],
            erasure_count: 0,
            octave_count: 0,
            prev_lspf: std::array::from_fn(|i| ((i + 1) as f64 / 11.0) as f32),
            predictor_lspf: [0.0; 10],
            pitch_synthesis_filter_mem: [0.0; 303],
            pitch_pre_filter_mem: [0.0; 303],
            rnd_fir_filter_mem: [0.0; 180],
            formant_mem: [0.0; 170],
            last_codebook_gain: 0.0,
            prev_g1: [0; 2],
            prev_bitrate: SILENCE,
            pitch_gain: [0.0; 4],
            pitch_lag: [0; 4],
            first16bits: 0,
            postfilter_synth_mem: [0.0; 10],
            postfilter_agc_mem: 0.0,
            postfilter_tilt_mem: 0.0,
        }
    }

    fn lspv(&self, i: usize) -> u8 {
        self.frame[LSPV + i]
    }

    /// `decode_lspf`: `Ok(false)` where FFmpeg returns -1 (a badly
    /// received packet).
    fn decode_lspf(&mut self, lspf: &mut [f32; 10]) -> Result<bool> {
        if self.bitrate == RATE_OCTAVE || self.bitrate == I_F_Q {
            let predictors = if self.prev_bitrate != RATE_OCTAVE && self.prev_bitrate != I_F_Q {
                self.prev_lspf
            } else {
                self.predictor_lspf
            };
            let smooth: f32;
            if self.bitrate == RATE_OCTAVE {
                self.octave_count = self.octave_count.wrapping_add(1);
                for i in 0..10 {
                    // QCELP_LSP_OCTAVE_PREDICTOR is `29.0/32` unbracketed:
                    // `pred * 29.0/32` divides the product by 32.
                    let spread = if self.lspv(i) != 0 { QCELP_LSP_SPREAD_FACTOR } else { -QCELP_LSP_SPREAD_FACTOR };
                    let v = ((i + 1) as f64)
                        .mul_add((1.0 - 29.0 / 32.0) / 11.0, spread + f64::from(predictors[i]) * 29.0 / 32.0);
                    lspf[i] = v as f32;
                    self.predictor_lspf[i] = lspf[i];
                }
                smooth = if self.octave_count < 10 { 0.875f64 as f32 } else { 0.1f64 as f32 };
            } else {
                let mut erasure_coeff = (29.0f64 / 32.0) as f32;
                if self.erasure_count > 1 {
                    erasure_coeff = (f64::from(erasure_coeff) * if self.erasure_count < 4 { 0.9 } else { 0.7 }) as f32;
                }
                for i in 0..10 {
                    let base = (i + 1) as f32 * (1.0 - erasure_coeff) / 11.0;
                    lspf[i] = erasure_coeff.mul_add(predictors[i], base);
                    self.predictor_lspf[i] = lspf[i];
                }
                smooth = 0.125;
            }
            // The stability of the LSP frequencies.
            let max = |a: f32, b: f64| if f64::from(a) > b { a } else { b as f32 };
            let min = |a: f32, b: f64| if f64::from(a) > b { b as f32 } else { a };
            lspf[0] = max(lspf[0], QCELP_LSP_SPREAD_FACTOR);
            for i in 1..10 {
                lspf[i] = max(lspf[i], f64::from(lspf[i - 1]) + QCELP_LSP_SPREAD_FACTOR);
            }
            lspf[9] = min(lspf[9], 1.0 - QCELP_LSP_SPREAD_FACTOR);
            for i in (1..10).rev() {
                lspf[i - 1] = min(lspf[i - 1], f64::from(lspf[i]) - QCELP_LSP_SPREAD_FACTOR);
            }
            weighted_vector_sumf_in_place(lspf, &self.prev_lspf, smooth, (1.0 - f64::from(smooth)) as f32, 10);
        } else {
            self.octave_count = 0;
            let mut tmp = 0.0f32;
            for i in 0..5 {
                let entry = get(lspvq(i), usize::from(self.lspv(i)))?;
                tmp = f64::from(entry[0]).mul_add(0.0001, f64::from(tmp)) as f32;
                lspf[2 * i] = tmp;
                tmp = f64::from(entry[1]).mul_add(0.0001, f64::from(tmp)) as f32;
                lspf[2 * i + 1] = tmp;
            }
            if self.bitrate == RATE_QUARTER {
                if f64::from(lspf[9]) <= 0.70 || f64::from(lspf[9]) >= 0.97 {
                    return Ok(false);
                }
                if (3..10).any(|i| f64::from(lspf[i] - lspf[i - 2]).abs() < 0.08) {
                    return Ok(false);
                }
            } else {
                if f64::from(lspf[9]) <= 0.66 || f64::from(lspf[9]) >= 0.985 {
                    return Ok(false);
                }
                if (4..10).any(|i| f64::from(lspf[i] - lspf[i - 4]).abs() < 0.0931) {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    /// `decode_gain_and_index`
    fn decode_gain_and_index(&mut self, gain: &mut [f32; 16]) -> Result<()> {
        let mut g1 = [0i32; 16];
        if self.bitrate >= RATE_QUARTER {
            let count = match self.bitrate {
                RATE_FULL => 16,
                RATE_HALF => 4,
                _ => 5,
            };
            for i in 0..count {
                g1[i] = 4 * i32::from(self.frame[CBGAIN + i]);
                if self.bitrate == RATE_FULL && (i + 1) & 3 == 0 {
                    g1[i] += ((g1[i - 1] + g1[i - 2] + g1[i - 3]) / 3 - 6).clamp(0, 32);
                }
                gain[i] = get(&G12GA, g1[i] as usize)?;
                if self.frame[CBSIGN + i] != 0 {
                    gain[i] = -gain[i];
                    self.frame[CINDEX + i] = (i32::from(self.frame[CINDEX + i]) - 89) as u8 & 127;
                }
            }
            self.prev_g1 = [g1[count - 2], g1[count - 1]];
            self.last_codebook_gain = get(&G12GA, g1[count - 1] as usize)?;
            if self.bitrate == RATE_QUARTER {
                let d = |x: f32| f64::from(x);
                gain[7] = gain[4];
                gain[6] = 0.4f64.mul_add(d(gain[3]), 0.6 * d(gain[4])) as f32;
                gain[5] = gain[3];
                gain[4] = 0.8f64.mul_add(d(gain[2]), 0.2 * d(gain[3])) as f32;
                gain[3] = 0.2f64.mul_add(d(gain[1]), 0.8 * d(gain[2])) as f32;
                gain[2] = gain[1];
                gain[1] = 0.6f64.mul_add(d(gain[0]), 0.4 * d(gain[1])) as f32;
            }
        } else if self.bitrate != SILENCE {
            let count = if self.bitrate == RATE_OCTAVE {
                g1[0] = 2 * i32::from(self.frame[CBGAIN]) + ((self.prev_g1[0] + self.prev_g1[1]) / 2 - 5).clamp(0, 54);
                8
            } else {
                g1[0] = self.prev_g1[1];
                match self.erasure_count {
                    1 => {}
                    2 => g1[0] -= 1,
                    3 => g1[0] -= 2,
                    _ => g1[0] -= 6,
                }
                g1[0] = g1[0].max(0);
                4
            };
            let slope = (0.5 * f64::from(get(&G12GA, g1[0] as usize)? - self.last_codebook_gain) / f64::from(count)) as f32;
            for i in 1..=count {
                gain[i as usize - 1] = slope.mul_add(i as f32, self.last_codebook_gain);
            }
            self.last_codebook_gain = gain[count as usize - 1];
            self.prev_g1 = [self.prev_g1[1], g1[0]];
        }
        Ok(())
    }

    /// `compute_svector`
    fn compute_svector(&mut self, gain: &[f32; 16], cdn: &mut [f32; SFR_LEN]) {
        let mut k = 0;
        match self.bitrate {
            RATE_FULL => {
                for i in 0..16 {
                    let tmp_gain = (f64::from(gain[i]) * QCELP_RATE_FULL_CODEBOOK_RATIO) as f32;
                    let mut cindex = (-i32::from(self.frame[CINDEX + i])) as u16;
                    for _ in 0..10 {
                        cdn[k] = tmp_gain * f32::from(RATE_FULL_CODEBOOK[usize::from(cindex & 127)]);
                        cindex = cindex.wrapping_add(1);
                        k += 1;
                    }
                }
            }
            RATE_HALF => {
                for i in 0..4 {
                    let tmp_gain = (f64::from(gain[i]) * QCELP_RATE_HALF_CODEBOOK_RATIO) as f32;
                    let mut cindex = (-i32::from(self.frame[CINDEX + i])) as u16;
                    for _ in 0..40 {
                        cdn[k] = tmp_gain * f32::from(RATE_HALF_CODEBOOK[usize::from(cindex & 127)]);
                        cindex = cindex.wrapping_add(1);
                        k += 1;
                    }
                }
            }
            RATE_QUARTER => {
                let l = |i: usize| i32::from(self.lspv(i));
                let mut cbseed = ((0x0003 & l(4)) << 14
                    | (0x003F & l(3)) << 8
                    | (0x0060 & l(2)) << 1
                    | (0x0007 & l(1)) << 3
                    | (0x0038 & l(0)) >> 3) as u16;
                let mem = &mut self.rnd_fir_filter_mem;
                let mut rnd = 20;
                for i in 0..8 {
                    let tmp_gain = (f64::from(gain[i]) * (QCELP_SQRT1887 / 32768.0)) as f32;
                    for _ in 0..20 {
                        cbseed = (521 * u32::from(cbseed) + 259) as u16;
                        mem[rnd] = f32::from(cbseed as i16);
                        let mut fir = 0.0f32;
                        for j in 0..10 {
                            fir = RND_FIR_COEFS[j].mul_add(f64::from(mem[rnd - j] + mem[rnd - 20 + j]), f64::from(fir)) as f32;
                        }
                        fir = RND_FIR_COEFS[10].mul_add(f64::from(mem[rnd - 10]), f64::from(fir)) as f32;
                        cdn[k] = tmp_gain * fir;
                        k += 1;
                        rnd += 1;
                    }
                }
                mem.copy_within(160..180, 0);
            }
            RATE_OCTAVE => {
                let mut cbseed = self.first16bits;
                for i in 0..8 {
                    let tmp_gain = (f64::from(gain[i]) * (QCELP_SQRT1887 / 32768.0)) as f32;
                    for _ in 0..20 {
                        cbseed = (521 * u32::from(cbseed) + 259) as u16;
                        cdn[k] = tmp_gain * f32::from(cbseed as i16);
                        k += 1;
                    }
                }
            }
            I_F_Q => {
                let mut cbseed = (-44i32) as u16;
                for i in 0..4 {
                    let tmp_gain = (f64::from(gain[i]) * QCELP_RATE_FULL_CODEBOOK_RATIO) as f32;
                    for _ in 0..40 {
                        cdn[k] = tmp_gain * f32::from(RATE_FULL_CODEBOOK[usize::from(cbseed & 127)]);
                        cbseed = cbseed.wrapping_add(1);
                        k += 1;
                    }
                }
            }
            _ => cdn.fill(0.0),
        }
    }

    /// `apply_pitch_filters`
    fn apply_pitch_filters(&mut self, cdn: &mut [f32; SFR_LEN]) {
        let rate = self.bitrate;
        if rate >= RATE_HALF || rate == SILENCE || (rate == I_F_Q && self.prev_bitrate >= RATE_HALF) {
            if rate >= RATE_HALF {
                for i in 0..4 {
                    let plag = self.frame[PLAG + i];
                    self.pitch_gain[i] =
                        if plag != 0 { (f64::from(i32::from(self.frame[PGAIN + i]) + 1) * 0.25) as f32 } else { 0.0 };
                    self.pitch_lag[i] = plag.wrapping_add(16);
                }
            } else {
                let max_pitch_gain = if rate == I_F_Q {
                    if self.erasure_count < 3 {
                        (-0.3f64).mul_add(f64::from(i32::from(self.erasure_count) - 1), 0.9) as f32
                    } else {
                        0.0
                    }
                } else {
                    1.0
                };
                for g in &mut self.pitch_gain {
                    if *g > max_pitch_gain {
                        *g = max_pitch_gain;
                    }
                }
                self.frame[PFRAC..PFRAC + 4].fill(0);
            }
            let pfrac: [u8; 4] = self.frame[PFRAC..PFRAC + 4].try_into().unwrap();
            let synthesis = do_pitchfilter(&mut self.pitch_synthesis_filter_mem, cdn, &self.pitch_gain, &self.pitch_lag, &pfrac);
            for g in &mut self.pitch_gain {
                let capped = if f64::from(*g) > 1.0 { 1.0 } else { f64::from(*g) };
                *g = (0.5 * capped) as f32;
            }
            let pre = do_pitchfilter(&mut self.pitch_pre_filter_mem, &synthesis, &self.pitch_gain, &self.pitch_lag, &pfrac);
            // apply_gain_ctrl
            for i in (0..SFR_LEN).step_by(40) {
                let res = dot(&synthesis[i..], &synthesis[i..], 40);
                cdn[i..i + 40].copy_from_slice(&pre[i..i + 40]);
                scale_vector_to_given_sum_of_squares(&mut cdn[i..i + 40], res);
            }
        } else {
            self.pitch_synthesis_filter_mem[..143].copy_from_slice(&cdn[17..]);
            self.pitch_pre_filter_mem[..143].copy_from_slice(&cdn[17..]);
            self.pitch_gain = [0.0; 4];
            self.pitch_lag = [0; 4];
        }
    }

    /// `interpolate_lpc`
    fn interpolate_lpc(&self, curr_lspf: &[f32; 10], lpc: &mut [f32; 10], subframe: usize) {
        let weight = if self.bitrate >= RATE_QUARTER {
            (0.25 * (subframe + 1) as f64) as f32
        } else if self.bitrate == RATE_OCTAVE && subframe == 0 {
            0.625
        } else {
            1.0
        };
        if weight != 1.0 {
            let mut interpolated = [0.0f32; 10];
            weighted_vector_sumf(&mut interpolated, curr_lspf, &self.prev_lspf, weight, (1.0 - f64::from(weight)) as f32, 10);
            lspf2lpc(&interpolated, lpc);
        } else if self.bitrate >= RATE_QUARTER || (self.bitrate == I_F_Q && subframe == 0) {
            lspf2lpc(curr_lspf, lpc);
        } else if self.bitrate == SILENCE && subframe == 0 {
            lspf2lpc(&self.prev_lspf, lpc);
        }
    }

    /// `postfilter`
    fn postfilter(&mut self, samples: &mut [f32; SFR_LEN], lpc: &[f32; 10]) {
        const POW_0_775: [f64; 10] = [0.775000, 0.600625, 0.465484, 0.360750, 0.279582, 0.216676, 0.167924, 0.130141, 0.100859, 0.078166];
        const POW_0_625: [f64; 10] = [0.625000, 0.390625, 0.244141, 0.152588, 0.095367, 0.059605, 0.037253, 0.023283, 0.014552, 0.009095];
        let mut lpc_s = [0.0f32; 10];
        let mut lpc_p = [0.0f32; 10];
        for n in 0..10 {
            lpc_s[n] = lpc[n] * POW_0_625[n] as f32;
            lpc_p[n] = lpc[n] * POW_0_775[n] as f32;
        }
        let mut zero_out = [0.0f32; SFR_LEN];
        lp_zero_synthesis_filterf(&mut zero_out, &lpc_s, &self.formant_mem, 10, SFR_LEN, 10);
        let mut pole_out = [0.0f32; 170];
        pole_out[..10].copy_from_slice(&self.postfilter_synth_mem);
        lp_synthesis_filterf(&mut pole_out, 10, &lpc_p, &zero_out, SFR_LEN, 10);
        self.postfilter_synth_mem.copy_from_slice(&pole_out[160..]);
        tilt_compensation(&mut self.postfilter_tilt_mem, 0.3f64 as f32, &mut pole_out[10..]);
        let energy = dot(&self.formant_mem[10..], &self.formant_mem[10..], SFR_LEN);
        adaptive_gain_control_from(samples, &pole_out[10..], energy, 0.9375, &mut self.postfilter_agc_mem);
    }

    /// The concealment `qcelp_decode_frame` jumps to (`erasure:`).
    fn erasure(&mut self, gain: &mut [f32; 16], out: &mut [f32; SFR_LEN], lspf: &mut [f32; 10]) -> Result<()> {
        self.bitrate = I_F_Q;
        self.erasure_count = self.erasure_count.wrapping_add(1);
        self.decode_gain_and_index(gain)?;
        self.compute_svector(gain, out);
        self.decode_lspf(lspf)?;
        self.apply_pitch_filters(out);
        Ok(())
    }

    /// `qcelp_decode_frame`
    fn decode_frame(&mut self, packet: &[u8], out: &mut [f32; SFR_LEN]) -> Result<()> {
        let mut gain = [0.0f32; 16];
        let mut lspf = [0.0f32; 10];
        let (bitrate, buf) = determine_bitrate(packet);
        self.bitrate = bitrate;
        let mut erase = bitrate == I_F_Q;
        if !erase && bitrate == RATE_OCTAVE {
            self.first16bits = u16::from_be_bytes([buf.first().copied().unwrap_or(0), buf.get(1).copied().unwrap_or(0)]);
            erase = self.first16bits == 0xFFFF;
        }
        if !erase && bitrate > SILENCE {
            // The reader spans the packet's size from where the frame starts
            // (one byte past the data when there is a rate byte): zeros.
            let mut gb = BitReader { data: buf, index: 0 };
            self.frame = [0; FRAME_BYTES];
            for bm in bitmap(bitrate) {
                let v = gb.get(u32::from(bm.bitlen)) << bm.bitpos;
                self.frame[usize::from(bm.index)] |= v as u8;
            }
            erase = self.frame[RESERVED] != 0
                || (bitrate == RATE_QUARTER && codebook_sanity_check_for_rate_quarter(&self.frame[CBGAIN..CBGAIN + 5]))
                || (bitrate >= RATE_HALF && (0..4).any(|i| self.frame[PFRAC + i] != 0 && self.frame[PLAG + i] >= 124));
        }
        if !erase {
            self.decode_gain_and_index(&mut gain)?;
            self.compute_svector(&gain, out);
            if self.decode_lspf(&mut lspf)? {
                self.apply_pitch_filters(out);
                self.erasure_count = 0;
            } else {
                erase = true;
            }
        }
        if erase {
            self.erasure(&mut gain, out, &mut lspf)?;
        }

        let mut lpc = [0.0f32; 10];
        for i in 0..4 {
            self.interpolate_lpc(&lspf, &mut lpc, i);
            let input: [f32; 40] = out[i * 40..i * 40 + 40].try_into().unwrap();
            lp_synthesis_filterf(&mut self.formant_mem, 10 + 40 * i, &lpc, &input, 40, 10);
        }
        self.postfilter(out, &lpc);
        self.formant_mem.copy_within(160..170, 0);
        self.prev_lspf = lspf;
        self.prev_bitrate = self.bitrate;
        Ok(())
    }
}

/// `determine_bitrate`: the rate and the frame's bytes (after the rate
/// byte when the packet has one).
fn determine_bitrate(packet: &[u8]) -> (i32, &[u8]) {
    let rate = buf_size2bitrate(packet.len());
    if rate >= 0 {
        let claimed = i32::from(packet[0]);
        if rate < claimed {
            return (I_F_Q, packet);
        }
        // A larger packet than the claimed rate needs decodes at the claim.
        (rate.min(claimed), &packet[1..])
    } else {
        let rate = buf_size2bitrate(packet.len() + 1);
        (rate, packet)
    }
}

/// `codebook_sanity_check_for_rate_quarter`: true when it fails.
fn codebook_sanity_check_for_rate_quarter(cbgain: &[u8]) -> bool {
    let mut prev_diff = 0;
    for i in 1..5 {
        let diff = i32::from(cbgain[i]) - i32::from(cbgain[i - 1]);
        if diff.abs() > 10 || (diff - prev_diff).abs() > 12 {
            return true;
        }
        prev_diff = diff;
    }
    false
}

/// `do_pitchfilter`: filters `v_in` through `memory` (143 samples of
/// history, then the 160 of output) and returns the output.
fn do_pitchfilter(memory: &mut [f32; 303], v_in: &[f32; SFR_LEN], gain: &[f32; 4], lag: &[u8; 4], pfrac: &[u8; 4]) -> [f32; SFR_LEN] {
    let mut v_out = 143;
    for i in 0..4 {
        let input = &v_in[40 * i..40 * i + 40];
        if gain[i] != 0.0 {
            let mut v_lag = 143 + 40 * i - usize::from(lag[i]);
            for &x in input {
                let mut v = if pfrac[i] != 0 {
                    let mut acc = 0.0f32;
                    for j in 0..4 {
                        acc = HAMMSINC_TABLE[j].mul_add(memory[v_lag + j - 4] + memory[v_lag + 3 - j], acc);
                    }
                    acc
                } else {
                    memory[v_lag]
                };
                v = gain[i].mul_add(v, x);
                memory[v_out] = v;
                v_lag += 1;
                v_out += 1;
            }
        } else {
            memory[v_out..v_out + 40].copy_from_slice(input);
            v_out += 40;
        }
    }
    memory.copy_within(160.., 0);
    memory[143..].try_into().unwrap()
}

/// `lspf2lpc`
fn lspf2lpc(lspf: &[f32; 10], lpc: &mut [f32; 10]) {
    let mut lsp = [0.0f64; 10];
    for i in 0..10 {
        lsp[i] = (std::f64::consts::PI * f64::from(lspf[i])).cos();
    }
    lspd2lpc(&lsp, lpc, 5);
    let mut coeff = QCELP_BANDWIDTH_EXPANSION_COEFF;
    for v in lpc.iter_mut() {
        *v = (f64::from(*v) * coeff) as f32;
        coeff *= QCELP_BANDWIDTH_EXPANSION_COEFF;
    }
}

pub struct QCELPDecoder {
    codec_id: CodecId,
    ctx: QCELPContext,
    queue: VecDeque<Frame>,
    pts_tracker: i64,
}

impl QCELPDecoder {
    /// `qcelp_decode_init`: FFmpeg sets mono whatever the container says.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        Ok(Self { codec_id: params.codec_id.clone(), ctx: QCELPContext::new(), queue: VecDeque::new(), pts_tracker: 0 })
    }
}

impl Decoder for QCELPDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// `qcelp_decode_frame`: one 160-sample frame per packet.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let pts = packet.pts.unwrap_or(self.pts_tracker);
        let mut out = [0.0f32; SFR_LEN];
        self.ctx.decode_frame(&packet.data, &mut out)?;
        self.queue.push_back(Frame::Audio(AudioFrame {
            samples: SFR_LEN as u32,
            pts: Some(pts),
            data: vec![out.iter().flat_map(|s| s.to_le_bytes()).collect()],
        }));
        self.pts_tracker = pts + SFR_LEN as i64;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.queue.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.ctx = QCELPContext::new();
        self.queue.clear();
        self.pts_tracker = 0;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::F32, sample_rate: 8_000, channels: 1 })
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(QCELPDecoder::new(params)?))
}

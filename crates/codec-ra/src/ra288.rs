//! RealAudio 2.0 (28.8K) decoder.
//! Ported faithfully from FFmpeg (libavcodec/ra288.c, g728_template.c, lpc_functions.h,
//! celp_filters.c ff_celp_lp_synthesis_filterf).
//! Commit: 2da55bf.
//! License: LGPL-2.1-or-later.
//!
//! The float arithmetic is FFmpeg 2da55bf's compiled code (arm64, clang -O3):
//! each `a*b ± c` in one C expression is a fused multiply-add, except in the
//! sums clang vectorizes (`crate::sums`), and the C's double expressions
//! stay double.

#![forbid(unsafe_code)]

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderLe;
use crate::ra288_tables::*;
use crate::sums::{scalarproduct_float, unfused_terms};

const RA288_BLOCK_SIZE: usize = 5;
const RA288_BLOCKS_PER_FRAME: usize = 32;
const BLOCK_ALIGN: usize = 38;

/// `10 * log10((1 << 24) / 5.) - 32` as FFmpeg's compiler folds it.
const GAIN_OFFSET: f64 = f64::from_bits(0x4040_a0f5_b977_7a46);

fn convolve(tgt: &mut [f32], work: &[f32], src_offset: usize, len: usize, order: usize) {
    for n in (0..=order).rev() {
        let src_a = &work[src_offset..src_offset + len];
        let src_b = &work[src_offset - n..src_offset - n + len];
        tgt[n] = scalarproduct_float(src_a, src_b, len);
    }
}

fn do_hybrid_window(
    order: usize,
    n: usize,
    non_rec: usize,
    out: &mut [f32],
    hist: &[f32],
    out2: &mut [f32],
    window: &[f32],
) {
    let total_len = order + n + non_rec;
    let mut work = vec![0.0f32; total_len];
    for i in 0..total_len {
        work[i] = window[i] * hist[i];
    }

    let mut buffer1 = vec![0.0f32; order + 1];
    let mut buffer2 = vec![0.0f32; order + 1];

    convolve(&mut buffer1, &work, order, n, order);
    convolve(&mut buffer2, &work, order + n, non_rec, order);

    // `ATTEN` is the double 0.5625: the update is double, then rounded.
    for i in 0..=order {
        out2[i] = (f64::from(out2[i]) * 0.5625 + f64::from(buffer1[i])) as f32;
        out[i] = out2[i] + buffer2[i];
    }

    out[0] *= 257.0 / 256.0;
}

fn compute_lpc_coefs(autoc: &[f32], max_order: usize, lpc: &mut [f32]) -> bool {
    let mut err = autoc[0];
    if autoc[max_order] == 0.0 || err <= 0.0 {
        return false;
    }

    for i in 0..max_order {
        let mut r = -autoc[i + 1];
        // `r -= lpc[j] * autoc[i - j - 1]` (autoc shifted by one): vectorized.
        let split = unfused_terms(i);
        for j in 0..split {
            r -= lpc[j] * autoc[i - j];
        }
        for j in split..i {
            r = (-lpc[j]).mul_add(autoc[i - j], r);
        }

        if err != 0.0 {
            r /= err;
        }
        // `LPC_FIXR(1.0) - r * r` is float.
        err *= (-r).mul_add(r, 1.0);

        lpc[i] = r;

        let half = (i + 1) / 2;
        for j in 0..half {
            let f = lpc[j];
            let b = lpc[i - 1 - j];
            lpc[j] = r.mul_add(b, f);
            lpc[i - 1 - j] = r.mul_add(f, b);
        }

        if err < 0.0 {
            return false;
        }
    }
    true
}

/// `ff_celp_lp_synthesis_filterf`: `out[o + n] = in[n] - Σ c[i-1] ·
/// out[o + n - i]`, the filter memory in `out[o - filter_length..o]`.
/// `filter_length` is even and at least 4. Four samples at a time as the C
/// does, then the remaining samples one by one, a sum FFmpeg's build
/// vectorizes (the coefficients do not overlap `out` here).
fn lp_synthesis_filterf(out: &mut [f32], o: usize, c: &[f32], input: &[f32], buffer_length: usize, filter_length: usize) {
    let a = c[0];
    let b = (-c[0]).mul_add(c[0], c[1]);
    let cc = (-c[0]).mul_add(b, (-c[1]).mul_add(c[0], c[2]));

    let mut old_out0 = out[o - 4];
    let mut old_out1 = out[o - 3];
    let mut old_out2 = out[o - 2];
    let mut old_out3 = out[o - 1];
    let mut n = 0;
    while n + 4 <= buffer_length {
        let p = o + n;
        let mut out0 = input[n];
        let mut out1 = input[n + 1];
        let mut out2 = input[n + 2];
        let mut out3 = input[n + 3];

        out0 = (-c[2]).mul_add(old_out1, out0);
        out1 = (-c[2]).mul_add(old_out2, out1);
        out2 = (-c[2]).mul_add(old_out3, out2);

        out0 = (-c[1]).mul_add(old_out2, out0);
        out1 = (-c[1]).mul_add(old_out3, out1);

        out0 = (-c[0]).mul_add(old_out3, out0);

        let mut val = c[3];
        out0 = (-val).mul_add(old_out0, out0);
        out1 = (-val).mul_add(old_out1, out1);
        out2 = (-val).mul_add(old_out2, out2);
        out3 = (-val).mul_add(old_out3, out3);

        let mut i = 5;
        while i < filter_length {
            old_out3 = out[p - i];
            val = c[i - 1];
            out0 = (-val).mul_add(old_out3, out0);
            out1 = (-val).mul_add(old_out0, out1);
            out2 = (-val).mul_add(old_out1, out2);
            out3 = (-val).mul_add(old_out2, out3);

            old_out2 = out[p - i - 1];
            val = c[i];
            out0 = (-val).mul_add(old_out2, out0);
            out1 = (-val).mul_add(old_out3, out1);
            out2 = (-val).mul_add(old_out0, out2);
            out3 = (-val).mul_add(old_out1, out3);

            core::mem::swap(&mut old_out0, &mut old_out2);
            old_out1 = old_out3;
            i += 2;
        }

        let (tmp0, tmp1, tmp2) = (out0, out1, out2);
        out3 = (-a).mul_add(tmp2, out3);
        out2 = (-a).mul_add(tmp1, out2);
        out1 = (-a).mul_add(tmp0, out1);
        out3 = (-b).mul_add(tmp1, out3);
        out2 = (-b).mul_add(tmp0, out2);
        out3 = (-cc).mul_add(tmp0, out3);

        out[p] = out0;
        out[p + 1] = out1;
        out[p + 2] = out2;
        out[p + 3] = out3;
        old_out0 = out0;
        old_out1 = out1;
        old_out2 = out2;
        old_out3 = out3;
        n += 4;
    }

    let split = unfused_terms(filter_length);
    while n < buffer_length {
        let mut v = input[n];
        for i in 1..=split {
            v -= c[i - 1] * out[o + n - i];
        }
        for i in split + 1..=filter_length {
            v = (-c[i - 1]).mul_add(out[o + n - i], v);
        }
        out[o + n] = v;
        n += 1;
    }
}

fn backward_filter(
    hist: &mut [f32],
    rec: &mut [f32],
    window: &[f32],
    lpc: &mut [f32],
    tab: &[f32],
    order: usize,
    n: usize,
    non_rec: usize,
    move_size: usize,
) {
    let mut temp = vec![0.0f32; order + 1];
    do_hybrid_window(order, n, non_rec, &mut temp, hist, rec, window);

    if compute_lpc_coefs(&temp, order, lpc) {
        for i in 0..order {
            lpc[i] *= tab[i];
        }
    }

    hist.copy_within(n..n + move_size, 0);
}

pub struct Ra288Decoder {
    codec_id: CodecId,
    sp_lpc: [f32; 36],
    gain_lpc: [f32; 10],
    sp_hist: [f32; 111],
    sp_rec: [f32; 37],
    gain_hist: [f32; 38],
    gain_rec: [f32; 11],
}

impl Ra288Decoder {
    pub fn new() -> Self {
        Self {
            codec_id: CodecId::new("ra_288"),
            sp_lpc: [0.0; 36],
            gain_lpc: [0.0; 10],
            sp_hist: [0.0; 111],
            sp_rec: [0.0; 37],
            gain_hist: [0.0; 38],
            gain_rec: [0.0; 11],
        }
    }

    fn decode_block(&mut self, gain: f32, cb_coef: usize) {
        self.sp_hist.copy_within(75..111, 70);

        // block 46 of G.728 spec (one fused multiply-subtract per term)
        let mut sum = 32.0f32;
        for i in 0..10 {
            sum = (-self.gain_hist[28 + 9 - i]).mul_add(self.gain_lpc[i], sum);
        }

        // block 47 of G.728 spec
        sum = sum.clamp(0.0, 60.0);

        // block 48 of G.728 spec
        let sumsum = (sum as f64 * 0.1151292546497).exp() * gain as f64 * (1.0 / 8388608.0);

        let mut buffer = [0.0f32; 5];
        for i in 0..5 {
            buffer[i] = (CODETABLE[cb_coef][i] as f64 * sumsum) as f32;
        }

        let energy_sum = scalarproduct_float(&buffer, &buffer, 5).max(5.0 / 16777216.0);

        self.gain_hist.copy_within(29..38, 28);
        // `10 * log10(sum) + (...)` is double, one fused multiply-add.
        self.gain_hist[37] = f64::from(energy_sum).log10().mul_add(10.0, GAIN_OFFSET) as f32;

        lp_synthesis_filterf(&mut self.sp_hist, 106, &self.sp_lpc, &buffer, RA288_BLOCK_SIZE, 36);
    }

    pub fn decode_frame_packet(&mut self, data: &[u8]) -> CoreResult<Vec<f32>> {
        if data.len() < BLOCK_ALIGN {
            return Err(CoreError::invalid("ra288: packet too small"));
        }

        let mut gb = BitReaderLe::new(&data[..BLOCK_ALIGN]);
        let mut samples = Vec::with_capacity(RA288_BLOCK_SIZE * RA288_BLOCKS_PER_FRAME);

        for i in 0..RA288_BLOCKS_PER_FRAME {
            let gain_idx = gb.read_bits(3).ok_or_else(|| CoreError::invalid("ra288: bitstream read failed"))? as usize;
            let cb_bits = 6 + (i & 1);
            let cb_coef = gb.read_bits(cb_bits).ok_or_else(|| CoreError::invalid("ra288: bitstream read failed"))? as usize;

            let gain = AMPTABLE[gain_idx];
            self.decode_block(gain, cb_coef);

            samples.extend_from_slice(&self.sp_hist[106..111]);

            if (i & 7) == 3 {
                backward_filter(
                    &mut self.sp_hist,
                    &mut self.sp_rec,
                    &SYN_WINDOW,
                    &mut self.sp_lpc,
                    &SYN_BW_TAB,
                    36,
                    40,
                    35,
                    70,
                );

                backward_filter(
                    &mut self.gain_hist,
                    &mut self.gain_rec,
                    &GAIN_WINDOW,
                    &mut self.gain_lpc,
                    &GAIN_BW_TAB,
                    10,
                    8,
                    20,
                    28,
                );
            }
        }

        Ok(samples)
    }
}

pub fn make_decoder(_params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    Ok(Box::new(Ra288DecoderWrapper {
        inner: Ra288Decoder::new(),
        pending_frames: Vec::new(),
    }))
}

struct Ra288DecoderWrapper {
    inner: Ra288Decoder,
    pending_frames: Vec<Frame>,
}

impl Decoder for Ra288DecoderWrapper {
    fn codec_id(&self) -> &CodecId {
        &self.inner.codec_id
    }
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::F32,
            sample_rate: 8000,
            channels: 1,
        })
    }


    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let mut offset = 0;
        let mut pts = packet.pts;
        while offset + BLOCK_ALIGN <= packet.data.len() {
            let samples = self.inner.decode_frame_packet(&packet.data[offset..offset + BLOCK_ALIGN])?;
            let mut byte_data = Vec::with_capacity(samples.len() * 4);
            for s in samples {
                byte_data.extend_from_slice(&s.to_le_bytes());
            }
            self.pending_frames.push(Frame::Audio(AudioFrame {
                samples: (RA288_BLOCK_SIZE * RA288_BLOCKS_PER_FRAME) as u32,
                pts,
                data: vec![byte_data],
            }));
            pts = None;
            offset += BLOCK_ALIGN;
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
        self.inner.sp_lpc = [0.0; 36];
        self.inner.gain_lpc = [0.0; 10];
        self.inner.sp_hist = [0.0; 111];
        self.inner.sp_rec = [0.0; 37];
        self.inner.gain_hist = [0.0; 38];
        self.inner.gain_rec = [0.0; 11];
        Ok(())
    }
}

//! RealAudio 2.0 (28.8K) decoder.
//! Ported faithfully from FFmpeg (libavcodec/ra288.c, g728_template.c, lpc_functions.h).
//! Commit: 2da55bf.
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderLe;
use crate::ra288_tables::*;

const RA288_BLOCK_SIZE: usize = 5;
const RA288_BLOCKS_PER_FRAME: usize = 32;
const BLOCK_ALIGN: usize = 38;
const ATTEN: f32 = 0.5625;

fn scalar_product(a: &[f32], b: &[f32], len: usize) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..len {
        sum += a[i] * b[i];
    }
    sum
}

fn convolve(tgt: &mut [f32], work: &[f32], src_offset: usize, len: usize, order: usize) {
    for n in (0..=order).rev() {
        let src_a = &work[src_offset..src_offset + len];
        let src_b = &work[src_offset - n..src_offset - n + len];
        tgt[n] = scalar_product(src_a, src_b, len);
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

    for i in 0..=order {
        out2[i] = out2[i] * ATTEN + buffer1[i];
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
        for j in 0..i {
            r -= lpc[j] * autoc[i - j];
        }

        if err != 0.0 {
            r /= err;
        }
        err *= 1.0 - r * r;

        lpc[i] = r;

        let half = (i + 1) / 2;
        for j in 0..half {
            let f = lpc[j];
            let b = lpc[i - 1 - j];
            lpc[j] = f + r * b;
            lpc[i - 1 - j] = b + r * f;
        }

        if err < 0.0 {
            return false;
        }
    }
    true
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

        // block 46 of G.728 spec
        let mut sum = 32.0f32;
        for i in 0..10 {
            sum -= self.gain_hist[28 + 9 - i] * self.gain_lpc[i];
        }

        // block 47 of G.728 spec
        sum = sum.clamp(0.0, 60.0);

        // block 48 of G.728 spec
        let sumsum = (sum as f64 * 0.1151292546497).exp() * gain as f64 * (1.0 / 8388608.0);

        let mut buffer = [0.0f32; 5];
        for i in 0..5 {
            buffer[i] = (CODETABLE[cb_coef][i] as f64 * sumsum) as f32;
        }

        let mut energy_sum = 0.0f32;
        for i in 0..5 {
            energy_sum += buffer[i] * buffer[i];
        }
        energy_sum = energy_sum.max(5.0 / 16777216.0);

        self.gain_hist.copy_within(29..38, 28);
        self.gain_hist[37] = 10.0 * energy_sum.log10() + (10.0 * (16777216.0f64 / 5.0).log10() as f32 - 32.0);

        // ff_celp_lp_synthesis_filterf on sp_hist[106..111]
        for n in 0..RA288_BLOCK_SIZE {
            let mut val = buffer[n];
            for i in 1..=36 {
                val -= self.sp_lpc[i - 1] * self.sp_hist[106 + n - i];
            }
            self.sp_hist[106 + n] = val;
        }
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

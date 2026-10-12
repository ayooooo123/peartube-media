// GSM 06.10 and Microsoft GSM decoders.
//
// Ported from FFmpeg (commit 2da55bf) libavcodec/gsmdec.c,
// gsmdec_template.c, msgsmdec.c, gsmdec_data.h and gsm.h, with get_bits.h's
// big-endian (GSM) and little-endian (Microsoft GSM) readers.
// Copyright (c) 2010 Reimar Döffinger <Reimar.Doeffinger@gmx.de>;
// LGPL-2.1-or-later (see LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat,
};

use crate::tables::{DEQUANT, LONG_TERM_GAIN, MODE_APCM_BITS, REQUANT};

/// Bytes per GSM block (`GSM_BLOCK_SIZE`).
pub const GSM_BLOCK_SIZE: usize = 33;
/// Bytes per Microsoft GSM block of two frames (`GSM_MS_BLOCK_SIZE`).
pub const GSM_MS_BLOCK_SIZE: usize = 65;
/// The smallest Microsoft block, MSN Audio at 8200 bit/s (`MSN_MIN_BLOCK_SIZE`).
pub const MSN_MIN_BLOCK_SIZE: usize = 41;
/// Samples per GSM frame (`GSM_FRAME_SIZE`).
pub const GSM_FRAME_SIZE: usize = 160;

/// get_bits.h's reader over one block: big-endian for GSM, little-endian
/// (`BITSTREAM_READER_LE`) for Microsoft GSM. Bits past the end read as
/// zero, as FFmpeg's packet padding does.
struct Bits<'a> {
    buf: &'a [u8],
    pos: usize,
    le: bool,
}

impl Bits<'_> {
    fn get(&mut self, n: u32) -> i32 {
        let mut v = 0u32;
        for i in 0..n as usize {
            let p = self.pos + i;
            let byte = u32::from(self.buf.get(p >> 3).copied().unwrap_or(0));
            if self.le {
                v |= (byte >> (p & 7) & 1) << i;
            } else {
                v = v << 1 | (byte >> (7 - (p & 7)) & 1);
            }
        }
        self.pos += n as usize;
        v as i32
    }
}

/// `GSMContext`
struct State {
    /// The last 120 excitation samples of the previous frame (read by the
    /// long-term synthesis at its lag), then the 160 of the current one.
    ref_buf: [i16; 280],
    v: [i32; 9],
    lar: [[i32; 8]; 2],
    lar_idx: usize,
    msr: i32,
}

impl State {
    fn new() -> Self {
        Self { ref_buf: [0; 280], v: [0; 9], lar: [[0; 8]; 2], lar_idx: 0, msr: 0 }
    }
}

/// `gsm_mult`: `(int)(a * (unsigned)b + (1 << 14)) >> 15`.
#[inline]
fn gsm_mult(a: i32, b: i32) -> i32 {
    ((a as u32).wrapping_mul(b as u32).wrapping_add(1 << 14) as i32) >> 15
}

/// `apcm_dequant_add`: the 13 RPE pulses of a sub-block, every third sample.
fn apcm_dequant_add(gb: &mut Bits, dst: &mut [i16], frame_bits: &[u8; 13]) {
    let maxidx = gb.get(6) as usize;
    let tab = &DEQUANT[maxidx];
    for (i, &bits) in frame_bits.iter().enumerate() {
        let val = gb.get(u32::from(bits)) as usize;
        let q = tab[usize::from(REQUANT[usize::from(bits)][val])];
        dst[3 * i] = dst[3 * i].wrapping_add(q);
    }
}

/// `long_term_synth`: 40 samples from `lag` samples back, scaled. `at` is
/// the sub-block's start in `ref_buf`; `lag` is at least 40, so the source
/// ends before the destination starts.
fn long_term_synth(ref_buf: &mut [i16; 280], at: usize, lag: usize, gain_idx: usize) {
    let gain = i32::from(LONG_TERM_GAIN[gain_idx]);
    for i in 0..40 {
        ref_buf[at + i] = gsm_mult(gain, i32::from(ref_buf[at - lag + i])) as i16;
    }
}

/// `decode_log_area`
#[inline]
fn decode_log_area(coded: i32, factor: i32, offset: i32) -> i32 {
    gsm_mult((coded << 10) - offset, factor) * 2
}

/// `get_rrp`: a log-area ratio to a reflection coefficient.
#[inline]
fn get_rrp(filtered: i32) -> i32 {
    let mut abs = filtered.wrapping_abs();
    if abs < 11059 {
        abs <<= 1;
    } else if abs < 20070 {
        abs += 11059;
    } else {
        abs = (abs >> 2) + 26112;
    }
    if filtered < 0 { -abs } else { abs }
}

/// `filter_value`: one sample through the short-term lattice filter.
#[inline]
fn filter_value(mut input: i32, rrp: &[i32; 8], v: &mut [i32; 9]) -> i32 {
    for i in (0..8).rev() {
        input = input.wrapping_sub(gsm_mult(rrp[i], v[i]));
        v[i + 1] = v[i].wrapping_add(gsm_mult(rrp[i], input));
    }
    v[0] = input;
    input
}

/// `short_term_synth`: the reflection coefficients move from the previous
/// frame's LARs to this frame's over samples 0-12, 13-26 and 27-39.
fn short_term_synth(st: &mut State, dst: &mut [i16]) {
    let lar = st.lar[st.lar_idx];
    let lar_prev = st.lar[st.lar_idx ^ 1];
    let src = &st.ref_buf[120..280];
    let mut rrp = [0i32; 8];
    for i in 0..8 {
        rrp[i] = get_rrp((lar_prev[i] >> 2) + (lar_prev[i] >> 1) + (lar[i] >> 2));
    }
    for i in 0..13 {
        dst[i] = filter_value(i32::from(src[i]), &rrp, &mut st.v) as i16;
    }
    for i in 0..8 {
        rrp[i] = get_rrp((lar_prev[i] >> 1) + (lar[i] >> 1));
    }
    for i in 13..27 {
        dst[i] = filter_value(i32::from(src[i]), &rrp, &mut st.v) as i16;
    }
    for i in 0..8 {
        rrp[i] = get_rrp((lar_prev[i] >> 2) + (lar[i] >> 1) + (lar[i] >> 2));
    }
    for i in 27..40 {
        dst[i] = filter_value(i32::from(src[i]), &rrp, &mut st.v) as i16;
    }
    for i in 0..8 {
        rrp[i] = get_rrp(lar[i]);
    }
    for i in 40..160 {
        dst[i] = filter_value(i32::from(src[i]), &rrp, &mut st.v) as i16;
    }
    st.lar_idx ^= 1;
}

#[inline]
fn clip_int16(v: i32) -> i32 {
    v.clamp(i32::from(i16::MIN), i32::from(i16::MAX))
}

/// `postprocess`: de-emphasis, then the 13-bit output of the standard.
fn postprocess(data: &mut [i16], mut msr: i32) -> i32 {
    for d in data.iter_mut().take(GSM_FRAME_SIZE) {
        msr = clip_int16(i32::from(*d) + gsm_mult(msr, 28180));
        *d = (clip_int16(msr * 2) & !7) as i16;
    }
    msr
}

/// `gsm_decode_block`: one 160-sample frame.
fn gsm_decode_block(st: &mut State, samples: &mut [i16], gb: &mut Bits, mode: usize) {
    let lar = &mut st.lar[st.lar_idx];
    lar[0] = decode_log_area(gb.get(6), 13107, 1 << 15);
    lar[1] = decode_log_area(gb.get(6), 13107, 1 << 15);
    lar[2] = decode_log_area(gb.get(5), 13107, (1 << 14) + 2048 * 2);
    lar[3] = decode_log_area(gb.get(5), 13107, (1 << 14) - 2560 * 2);
    lar[4] = decode_log_area(gb.get(4), 19223, (1 << 13) + 94 * 2);
    lar[5] = decode_log_area(gb.get(4), 17476, (1 << 13) - 1792 * 2);
    lar[6] = decode_log_area(gb.get(3), 31454, (1 << 12) - 341 * 2);
    lar[7] = decode_log_area(gb.get(3), 29708, (1 << 12) - 1144 * 2);

    let mut at = 120;
    for bits in MODE_APCM_BITS[mode] {
        let lag = gb.get(7).clamp(40, 120) as usize;
        let gain_idx = gb.get(2) as usize;
        let offset = gb.get(2) as usize;
        long_term_synth(&mut st.ref_buf, at, lag, gain_idx);
        apcm_dequant_add(gb, &mut st.ref_buf[at + offset..at + 40], bits);
        at += 40;
    }
    st.ref_buf.copy_within(160..280, 0);
    short_term_synth(st, samples);
    st.msr = postprocess(samples, st.msr);
}

/// The `gsm` and `gsm_ms` decoders: one block per call in FFmpeg
/// (`gsm_decode_frame` returns `block_align`); here every whole block of a
/// packet, in order.
pub struct GsmDecoder {
    codec_id: CodecId,
    ms: bool,
    block_align: usize,
    /// `(GSM_MS_BLOCK_SIZE - block_align) / 3` for Microsoft GSM, else 0.
    mode: usize,
    sample_rate: u32,
    state: State,
    ready: VecDeque<Frame>,
}

impl GsmDecoder {
    /// `gsm_init`. Microsoft GSM takes its block size from the container
    /// (option `block_align`), 65 when there is none; other sizes are the
    /// MSN Audio rates, 41 to 62 in steps of 3.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let ms = params.codec_id.as_str() == "gsm_ms";
        let block_align = if ms {
            let given = params.options.get("block_align").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
            if given == 0 {
                GSM_MS_BLOCK_SIZE
            } else if !(MSN_MIN_BLOCK_SIZE..=GSM_MS_BLOCK_SIZE).contains(&given)
                || (given - MSN_MIN_BLOCK_SIZE) % 3 != 0
            {
                return Err(Error::invalid(format!("gsm_ms: invalid block alignment {given}")));
            } else {
                given
            }
        } else {
            GSM_BLOCK_SIZE
        };
        Ok(Self {
            codec_id: params.codec_id.clone(),
            ms,
            block_align,
            mode: if ms { (GSM_MS_BLOCK_SIZE - block_align) / 3 } else { 0 },
            sample_rate: params.sample_rate.filter(|&r| r > 0).unwrap_or(8000),
            state: State::new(),
            ready: VecDeque::new(),
        })
    }

    fn frame_size(&self) -> usize {
        if self.ms { 2 * GSM_FRAME_SIZE } else { GSM_FRAME_SIZE }
    }

    /// One block into `samples` (`gsm_decode_frame`, `ff_msgsm_decode_block`).
    fn decode_block(&mut self, block: &[u8], samples: &mut [i16]) {
        if self.ms {
            // FFmpeg reads the block through a GSM_MS_BLOCK_SIZE reader; the
            // mode's bits end within block_align bytes.
            let mut gb = Bits { buf: &block[..block.len().min(GSM_MS_BLOCK_SIZE)], pos: 0, le: true };
            let (first, second) = samples.split_at_mut(GSM_FRAME_SIZE);
            gsm_decode_block(&mut self.state, first, &mut gb, self.mode);
            gsm_decode_block(&mut self.state, second, &mut gb, self.mode);
        } else {
            let mut gb = Bits { buf: block, pos: 0, le: false };
            // The 0xD magic: FFmpeg only warns when it is missing.
            gb.get(4);
            gsm_decode_block(&mut self.state, samples, &mut gb, 0);
        }
    }
}

impl Decoder for GsmDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::S16, sample_rate: self.sample_rate, channels: 1 })
    }

    /// Every whole block of the packet. Bytes left over that make no whole
    /// block are dropped, as FFmpeg drops them ("Packet is too small"); a
    /// packet with no whole block is an error.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() < self.block_align {
            return Err(Error::invalid("gsm: packet is too small"));
        }
        let frame_size = self.frame_size();
        let mut pts = packet.pts;
        let mut samples = vec![0i16; frame_size];
        for block in packet.data.chunks_exact(self.block_align) {
            self.decode_block(block, &mut samples);
            let bytes = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            self.ready.push_back(Frame::Audio(AudioFrame {
                samples: frame_size as u32,
                pts: pts.take(),
                data: vec![bytes],
            }));
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// `gsm_flush`: the whole context back to zero.
    fn reset(&mut self) -> Result<()> {
        self.state = State::new();
        self.ready.clear();
        Ok(())
    }
}

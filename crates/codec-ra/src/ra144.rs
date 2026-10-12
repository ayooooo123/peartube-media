//! RealAudio 1.0 (14.4K) decoder.
//! Ported faithfully from FFmpeg (libavcodec/ra144.c, ra144dec.c, celp_filters.c).
//! Commit: 2da55bf.
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderBe;
use crate::ra144_tables::*;

const NBLOCKS: usize = 4;
const BLOCKSIZE: usize = 40;
const BUFFERSIZE: usize = 146;
const FRAME_SIZE: usize = 20;
const LPC_ORDER: usize = 10;

#[inline]
fn av_log2_16bit(v: u32) -> i32 {
    let v16 = v as u16;
    if v16 == 0 {
        0
    } else {
        15 - v16.leading_zeros() as i32
    }
}

#[inline]
fn fastdiv(a: u32, b: u32) -> u32 {
    if b >= INVERSE_TAB.len() as u32 || b == 0 {
        return 0;
    }
    ((a as u64 * INVERSE_TAB[b as usize] as u64) >> 32) as u32
}

fn ff_sqrt(a: u32) -> u32 {
    let b = if a < 255 {
        (SQRT_TAB[a as usize + 1] as u32 - 1) >> 4
    } else if a < (1 << 12) {
        SQRT_TAB[(a >> 4) as usize] as u32 >> 2
    } else if a < (1 << 14) {
        SQRT_TAB[(a >> 6) as usize] as u32 >> 1
    } else if a < (1 << 16) {
        SQRT_TAB[(a >> 8) as usize] as u32
    } else {
        let s = av_log2_16bit(a >> 16) >> 1;
        let c = a >> (s + 2);
        let idx = (c >> (s + 8)) as usize;
        let tab_val = if idx < 256 { SQRT_TAB[idx] as u32 } else { 0 };
        fastdiv(c, tab_val) + (tab_val << s)
    };
    if a < b.wrapping_mul(b) {
        b.saturating_sub(1)
    } else {
        b
    }
}

fn ff_t_sqrt(mut x: u32) -> i32 {
    let mut s = 2;
    while x > 0xfff {
        s += 1;
        x >>= 2;
    }
    (ff_sqrt(x << 20) << s) as i32
}

fn ff_rms(data: &[i32; LPC_ORDER]) -> u32 {
    let mut res = 0x10000u32;
    let mut b = LPC_ORDER;

    for i in 0..LPC_ORDER {
        let term = (0x1000000 - data[i] * data[i]) >> 12;
        let prod = (term as i64 * res as i64) >> 12;
        if prod <= 0 {
            return 0;
        }
        res = prod as u32;

        while res <= 0x3fff {
            b += 1;
            res <<= 2;
        }
    }

    (ff_t_sqrt(res) >> b) as u32
}

fn ff_eval_coefs(coefs: &mut [i32; LPC_ORDER], refl: &[i32; LPC_ORDER]) {
    let mut buffer1 = [0i32; LPC_ORDER];
    let mut buffer2 = [0i32; LPC_ORDER];

    let mut b1_is_buf1 = true;

    for i in 0..LPC_ORDER {
        let (current_b1, current_b2) = if b1_is_buf1 {
            (&mut buffer1, &buffer2)
        } else {
            (&mut buffer2, &buffer1)
        };

        current_b1[i] = refl[i] * 16;
        for j in 0..i {
            let mult = (refl[i] as i64 * (current_b2[i - j - 1] as u32 as i64)) as i32;
            current_b1[j] = (mult >> 12) + current_b2[j];
        }

        b1_is_buf1 = !b1_is_buf1;
    }

    let final_buf = if b1_is_buf1 { &buffer2 } else { &buffer1 };
    for i in 0..LPC_ORDER {
        coefs[i] = final_buf[i] >> 4;
    }
}

fn ff_eval_refl(refl: &mut [i32; LPC_ORDER], coefs: &[i16; LPC_ORDER]) -> bool {
    let mut buffer1 = [0i32; LPC_ORDER];
    let mut buffer2 = [0i32; LPC_ORDER];

    for i in 0..LPC_ORDER {
        buffer2[i] = coefs[i] as i32;
    }

    refl[LPC_ORDER - 1] = buffer2[LPC_ORDER - 1];
    if (buffer2[LPC_ORDER - 1] as u32).wrapping_add(0x1000) > 0x1fff {
        return true; // overflow
    }

    let mut bp1_is_buf1 = true;

    for i in (0..LPC_ORDER - 1).rev() {
        let (cur_bp1, cur_bp2) = if bp1_is_buf1 {
            (&mut buffer1, &buffer2)
        } else {
            (&mut buffer2, &buffer1)
        };

        let mut b = 0x1000 - ((cur_bp2[i + 1] * cur_bp2[i + 1]) >> 12);
        if b == 0 {
            b = -2;
        }
        b = 0x1000000 / b;

        for j in 0..=i {
            let term = (refl[i + 1] as i64 * (cur_bp2[i - j] as u32 as i64)) as i32 >> 12;
            let sub = cur_bp2[j] - term;
            let mult = (sub as i64 * (b as u32 as i64)) as i32;
            cur_bp1[j] = mult >> 12;
        }

        if (cur_bp1[i] as u32).wrapping_add(0x1000) > 0x1fff {
            return true; // overflow
        }

        refl[i] = cur_bp1[i];
        bp1_is_buf1 = !bp1_is_buf1;
    }

    false
}

fn ff_copy_and_dup(target: &mut [i16; BLOCKSIZE], source: &[i16], offset: usize) {
    let src_start = BUFFERSIZE.saturating_sub(offset);
    let copy_len = BLOCKSIZE.min(offset);
    if src_start + copy_len <= source.len() {
        target[..copy_len].copy_from_slice(&source[src_start..src_start + copy_len]);
    }
    if offset < BLOCKSIZE {
        let rem = BLOCKSIZE - offset;
        if src_start + rem <= source.len() {
            target[offset..offset + rem].copy_from_slice(&source[src_start..src_start + rem]);
        }
    }
}

#[inline]
fn ff_rescale_rms(rms: u32, energy: u32) -> u32 {
    (rms * energy) >> 10
}

fn ff_irms(data: &[i16; BLOCKSIZE]) -> u32 {
    let mut sum = 0u32;
    for &x in data.iter() {
        sum = sum.wrapping_add((x as i32 * x as i32) as u32);
    }
    if sum == 0 {
        return 0;
    }
    let sqrt_val = ff_t_sqrt(sum) >> 8;
    if sqrt_val <= 0 {
        return 0;
    }
    0x20000000 / sqrt_val as u32
}

fn ff_interp(
    lpc_coef: &[[i32; LPC_ORDER]; 2],
    lpc_refl_rms: &[u32; 2],
    out: &mut [i16; LPC_ORDER],
    a: usize,
    copyold: usize,
    energy: u32,
) -> u32 {
    let mut work = [0i32; LPC_ORDER];
    let b = NBLOCKS - a;

    for i in 0..LPC_ORDER {
        out[i] = ((a as i32 * lpc_coef[0][i] + b as i32 * lpc_coef[1][i]) >> 2) as i16;
    }

    if ff_eval_refl(&mut work, out) {
        for i in 0..LPC_ORDER {
            out[i] = lpc_coef[copyold][i] as i16;
        }
        ff_rescale_rms(lpc_refl_rms[copyold], energy)
    } else {
        ff_rescale_rms(ff_rms(&work), energy)
    }
}

fn add_wav(
    dest: &mut [i16],
    n: usize,
    skip_first: bool,
    m: &[i32; 3],
    s1: Option<&[i16; BLOCKSIZE]>,
    s2: &[i8; 40],
    s3: &[i8; 40],
) {
    let mut v = [0i32; 3];
    let start = if skip_first { 1 } else { 0 };
    for i in start..3 {
        let mult = (GAIN_VAL_TAB[n][i] as i64 * (m[i] as u32 as i64)) as i32;
        v[i] = mult >> GAIN_EXP_TAB[n];
    }

    if let Some(s1_buf) = s1 {
        if v[0] != 0 {
            for i in 0..BLOCKSIZE {
                let term0 = (s1_buf[i] as i64 * (v[0] as u32 as i64)) as i32;
                let term1 = s2[i] as i32 * v[1];
                let term2 = s3[i] as i32 * v[2];
                dest[i] = ((term0 + term1 + term2) >> 12) as i16;
            }
            return;
        }
    }

    for i in 0..BLOCKSIZE {
        let term1 = s2[i] as i32 * v[1];
        let term2 = s3[i] as i32 * v[2];
        dest[i] = ((term1 + term2) >> 12) as i16;
    }
}

fn ff_celp_lp_synthesis_filter(
    out: &mut [i16],
    filter_coeffs: &[i16; LPC_ORDER],
    inp: &[i16; BLOCKSIZE],
    buffer_length: usize,
    filter_length: usize,
    stop_on_overflow: bool,
    shift: usize,
    rounder: i32,
) -> bool {
    // out buffer starts at offset LPC_ORDER, with LPC_ORDER history elements before it
    for n in 0..buffer_length {
        let mut sum = rounder;
        for i in 1..=filter_length {
            let hist_idx = LPC_ORDER + n - i;
            sum -= filter_coeffs[i - 1] as i32 * out[hist_idx] as i32;
        }

        let sum1 = ((sum >> 12) + inp[n] as i32) >> shift;
        let sum_clipped = sum1.clamp(-32768, 32767);

        if stop_on_overflow && sum_clipped != sum1 {
            return true;
        }

        out[LPC_ORDER + n] = sum_clipped as i16;
    }
    false
}

pub struct Ra144Decoder {
    codec_id: CodecId,
    old_energy: u32,
    lpc_tables: [[i32; LPC_ORDER]; 2],
    active_lpc_idx: usize,
    lpc_refl_rms: [u32; 2],
    curr_sblock: [i16; 50],
    adapt_cb: [i16; 148],
    buffer_a: [i16; BLOCKSIZE],
}

impl Ra144Decoder {
    pub fn new() -> Self {
        Self {
            codec_id: CodecId::new("ra_144"),
            old_energy: 0,
            lpc_tables: [[0; LPC_ORDER]; 2],
            active_lpc_idx: 0,
            lpc_refl_rms: [0; 2],
            curr_sblock: [0; 50],
            adapt_cb: [0; 148],
            buffer_a: [0; BLOCKSIZE],
        }
    }

    fn subblock_synthesis(
        &mut self,
        lpc_coefs: &[i16; LPC_ORDER],
        mut cba_idx: usize,
        cb1_idx: usize,
        cb2_idx: usize,
        gval: u32,
        gain: usize,
    ) {
        let mut m = [0i32; 3];

        let has_cba = cba_idx != 0;
        if has_cba {
            cba_idx += BLOCKSIZE / 2 - 1;
            ff_copy_and_dup(&mut self.buffer_a, &self.adapt_cb, cba_idx);
            let irms = ff_irms(&self.buffer_a);
            m[0] = ((irms as i64 * (gval as u32 as i64)) >> 12) as i32;
        }

        m[1] = ((CB1_BASE[cb1_idx] as u32 * gval) >> 8) as i32;
        m[2] = ((CB2_BASE[cb2_idx] as u32 * gval) >> 8) as i32;

        self.adapt_cb.copy_within(BLOCKSIZE..BUFFERSIZE, 0);

        let mut block = [0i16; BLOCKSIZE];
        let cb1_v = &CB1_VECTS[cb1_idx];
        let cb2_v = &CB2_VECTS[cb2_idx];
        let s1_arg = if has_cba { Some(&self.buffer_a) } else { None };
        add_wav(&mut block, gain, !has_cba, &m, s1_arg, cb1_v, cb2_v);

        let dest_start = BUFFERSIZE - BLOCKSIZE;
        self.adapt_cb[dest_start..dest_start + BLOCKSIZE].copy_from_slice(&block);

        self.curr_sblock.copy_within(BLOCKSIZE..BLOCKSIZE + LPC_ORDER, 0);

        let overflow = ff_celp_lp_synthesis_filter(
            &mut self.curr_sblock,
            lpc_coefs,
            &block,
            BLOCKSIZE,
            LPC_ORDER,
            true,
            0,
            0xfff,
        );

        if overflow {
            self.curr_sblock.fill(0);
        }
    }

    pub fn decode_packet(&mut self, data: &[u8]) -> CoreResult<Vec<i16>> {
        if data.len() < FRAME_SIZE {
            return Err(CoreError::invalid("ra144: frame too small"));
        }

        let sizes: [usize; LPC_ORDER] = [6, 5, 5, 4, 4, 3, 3, 3, 3, 2];
        let mut gb = BitReaderBe::new(&data[..FRAME_SIZE]);

        let mut lpc_refl = [0i32; LPC_ORDER];
        for i in 0..LPC_ORDER {
            let idx = gb.read_bits(sizes[i]).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;
            if idx >= LPC_REFL_CB[i].len() {
                return Err(CoreError::invalid("ra144: reflection table index out of range"));
            }
            lpc_refl[i] = LPC_REFL_CB[i][idx] as i32;
        }

        let active = self.active_lpc_idx;
        let inactive = 1 - active;

        ff_eval_coefs(&mut self.lpc_tables[active], &lpc_refl);
        self.lpc_refl_rms[active] = ff_rms(&lpc_refl);

        let energy_idx = gb.read_bits(5).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;
        let energy = ENERGY_TAB[energy_idx] as u32;

        let mut block_coefs = [[0i16; LPC_ORDER]; NBLOCKS];
        let mut refl_rms = [0u32; NBLOCKS];

        let lpc_pair = [self.lpc_tables[active], self.lpc_tables[inactive]];
        let rms_pair = [self.lpc_refl_rms[active], self.lpc_refl_rms[inactive]];

        refl_rms[0] = ff_interp(&lpc_pair, &rms_pair, &mut block_coefs[0], 1, 1, self.old_energy);
        let mid_energy = (ff_t_sqrt(energy * self.old_energy) >> 12) as u32;
        let copy_old_flag = if energy <= self.old_energy { 1 } else { 0 };
        refl_rms[1] = ff_interp(&lpc_pair, &rms_pair, &mut block_coefs[1], 2, copy_old_flag, mid_energy);
        refl_rms[2] = ff_interp(&lpc_pair, &rms_pair, &mut block_coefs[2], 3, 0, energy);
        refl_rms[3] = ff_rescale_rms(self.lpc_refl_rms[active], energy);

        for i in 0..LPC_ORDER {
            block_coefs[3][i] = self.lpc_tables[active][i] as i16;
        }

        let mut samples = Vec::with_capacity(NBLOCKS * BLOCKSIZE);

        for i in 0..NBLOCKS {
            let cba_idx = gb.read_bits(7).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;
            let gain = gb.read_bits(8).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;
            let cb1_idx = gb.read_bits(7).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;
            let cb2_idx = gb.read_bits(7).ok_or_else(|| CoreError::invalid("ra144: bitstream read failed"))? as usize;

            self.subblock_synthesis(&block_coefs[i], cba_idx, cb1_idx, cb2_idx, refl_rms[i], gain);

            for j in 0..BLOCKSIZE {
                let sample = (self.curr_sblock[j + 10] as i32 * 4).clamp(-32768, 32767) as i16;
                samples.push(sample);
            }
        }

        self.old_energy = energy;
        self.lpc_refl_rms[inactive] = self.lpc_refl_rms[active];
        self.active_lpc_idx = inactive;

        Ok(samples)
    }
}

pub fn make_decoder(_params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    Ok(Box::new(Ra144DecoderWrapper {
        inner: Ra144Decoder::new(),
        pending_frames: Vec::new(),
    }))
}

struct Ra144DecoderWrapper {
    inner: Ra144Decoder,
    pending_frames: Vec<Frame>,
}

impl Decoder for Ra144DecoderWrapper {
    fn codec_id(&self) -> &CodecId {
        &self.inner.codec_id
    }
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16,
            sample_rate: 8000,
            channels: 1,
        })
    }


    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        // One packet may contain multiple 20-byte frames
        let mut offset = 0;
        let mut pts = packet.pts;
        while offset + FRAME_SIZE <= packet.data.len() {
            let samples = self.inner.decode_packet(&packet.data[offset..offset + FRAME_SIZE])?;
            let mut byte_data = Vec::with_capacity(samples.len() * 2);
            for s in samples {
                byte_data.extend_from_slice(&s.to_le_bytes());
            }
            self.pending_frames.push(Frame::Audio(AudioFrame {
                samples: (NBLOCKS * BLOCKSIZE) as u32,
                pts,
                data: vec![byte_data],
            }));
            pts = None;
            offset += FRAME_SIZE;
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
        self.inner.old_energy = 0;
        self.inner.curr_sblock.fill(0);
        self.inner.adapt_cb.fill(0);
        self.inner.buffer_a.fill(0);
        self.inner.lpc_tables = [[0; LPC_ORDER]; 2];
        self.inner.lpc_refl_rms = [0; 2];
        Ok(())
    }
}

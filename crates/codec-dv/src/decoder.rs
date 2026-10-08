// Ported from FFmpeg (commit 2da55bf): libavcodec/dvdec.c
// (dv_init_weight_tables, dvvideo_decode_init, dv_decode_ac, bit_copy,
// put_block_8x4, dv100_idct_put_last_row_field_luma/_chroma,
// dv_decode_video_segment, dvvideo_decode_frame), with get_bits.h and
// put_bits.h for its bit readers and writers.
// License: LGPL-2.1-or-later

//! The `dvvideo` decoder: one DV frame per packet in, one picture out, in
//! the profile's pixel format (4:1:1, 4:2:0 or 4:2:2, 8-bit planar).

use std::collections::VecDeque;

use oxideav_core::{CodecId, CodecParameters, CodecTag, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame, VideoPlane};

use crate::idct::{simple_idct, simple_idct248_put, simple_idct_put};
use crate::profile::{frame_profile, mb_xy, work_chunks, DvProfile, WorkChunk, DV_MAX_BPM};
use crate::tables::{
    DV_IWEIGHT_1080_C, DV_IWEIGHT_1080_Y, DV_IWEIGHT_248, DV_IWEIGHT_720_C, DV_IWEIGHT_720_Y, DV_IWEIGHT_88, DV_QUANT_OFFSET,
    DV_QUANT_SHIFTS, DV_ZIGZAG248_DIRECT, ZIGZAG_DIRECT,
};
use crate::vlc::{RlVlc, DV_RL_VLC, TEX_VLC_BITS};

/// dv_iweight_bits.
const DV_IWEIGHT_BITS: u32 = 14;
/// MIN_CACHE_BITS.
const MIN_CACHE_BITS: i64 = 25;

/// A big-endian bit reader over `data` (zeros past its end, as FFmpeg's
/// padding) of `size` bits, as get_bits.h's.
struct Bits<'a> {
    data: &'a [u8],
    size: i64,
    index: i64,
}

impl<'a> Bits<'a> {
    fn new(data: &'a [u8], size: i64) -> Self {
        Self { data, size, index: 0 }
    }

    /// UPDATE_CACHE: the 32 bits from `index` (its byte's bits before it
    /// shifted out).
    fn cache(&self, index: i64) -> u32 {
        let Ok(index) = usize::try_from(index) else { return 0 };
        let byte = index >> 3;
        let word = (0..4).fold(0u32, |v, k| (v << 8) | u32::from(self.data.get(byte + k).copied().unwrap_or(0)));
        word << (index & 7)
    }

    /// get_bits(n), n in 1..=25.
    fn get_bits(&mut self, n: u32) -> u32 {
        let v = self.cache(self.index) >> (32 - n);
        self.index = (self.index + i64::from(n)).min(self.size + 8);
        v
    }

    /// get_sbits(n).
    fn get_sbits(&mut self, n: u32) -> i32 {
        let v = (self.cache(self.index) as i32) >> (32 - n);
        self.index = (self.index + i64::from(n)).min(self.size + 8);
        v
    }

    fn bits_left(&self) -> i64 {
        self.size - self.index
    }
}

/// A big-endian bit writer into `buf`, as put_bits.h's.
struct PutBits<'a> {
    buf: &'a mut [u8],
    acc: u64,
    acc_bits: u32,
    bytes: usize,
}

impl<'a> PutBits<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, acc: 0, acc_bits: 0, bytes: 0 }
    }

    /// put_bits(n, value), n in 0..=32; bits past the buffer are dropped.
    fn put(&mut self, n: u32, value: u32) {
        if n == 0 {
            return;
        }
        self.acc = (self.acc << n) | (u64::from(value) & ((1u64 << n) - 1));
        self.acc_bits += n;
        while self.acc_bits >= 8 {
            self.acc_bits -= 8;
            if let Some(b) = self.buf.get_mut(self.bytes) {
                *b = (self.acc >> self.acc_bits) as u8;
                self.bytes += 1;
            }
        }
    }

    /// put_bits_count.
    fn count(&self) -> i64 {
        (self.bytes * 8) as i64 + i64::from(self.acc_bits)
    }

    /// put_bits32(0) then flush_put_bits: the written bytes, the reader's
    /// zero padding after them.
    fn finish(mut self) -> &'a [u8] {
        self.put(32, 0);
        if self.acc_bits > 0 {
            if let Some(b) = self.buf.get_mut(self.bytes) {
                *b = (self.acc << (8 - self.acc_bits)) as u8;
                self.bytes += 1;
            }
        }
        let n = self.bytes.min(self.buf.len());
        let buf: &'a [u8] = self.buf;
        &buf[..n]
    }
}

/// bit_copy: the reader's remaining bits to the writer.
fn bit_copy(pb: &mut PutBits, gb: &mut Bits) {
    let mut bits_left = gb.bits_left();
    while bits_left >= MIN_CACHE_BITS {
        let v = gb.get_bits(MIN_CACHE_BITS as u32);
        pb.put(MIN_CACHE_BITS as u32, v);
        bits_left -= MIN_CACHE_BITS;
    }
    if bits_left > 0 {
        let v = gb.get_bits(bits_left as u32);
        pb.put(bits_left as u32, v);
    }
}

/// BlockInfo: where a block's AC decoding stands.
#[derive(Clone, Copy, Default)]
struct BlockInfo {
    /// Offset of its 64 dequantisation factors in `idct_factor`.
    factor: usize,
    /// The 2-4-8 scan and IDCT (an interlaced SD block).
    dct248: bool,
    /// Position in the block (uint8_t).
    pos: u8,
    partial_bit_count: u8,
    partial_bit_buffer: u32,
}

/// dv_decode_ac: AC coefficients until the reader's end or the block's.
fn decode_ac(gb: &mut Bits, mb: &mut BlockInfo, block: &mut [i16; 64], factor: &[u32], scan: &[u8; 64], vlc: &[RlVlc]) {
    let last_index = gb.size;
    let mut pos = i32::from(mb.pos);
    let partial_bit_count = u32::from(mb.partial_bit_count);
    let mut re_index = gb.index;
    let mut re_cache = gb.cache(re_index);
    // a partial VLC left from the last reader goes first
    if partial_bit_count > 0 {
        re_cache = (re_cache >> partial_bit_count) | mb.partial_bit_buffer;
        re_index -= i64::from(partial_bit_count);
        mb.partial_bit_count = 0;
    }
    loop {
        let mut index = (re_cache >> (32 - TEX_VLC_BITS)) as usize;
        let Some(first) = vlc.get(index) else { break };
        let mut vlc_len = i64::from(first.len8);
        if vlc_len < 0 {
            let extra = (-vlc_len) as u32;
            index = ((re_cache << TEX_VLC_BITS) >> (32 - extra)) as usize + first.level as usize;
            vlc_len = i64::from(TEX_VLC_BITS) - vlc_len;
        }
        let Some(&RlVlc { level, run, .. }) = vlc.get(index) else { break };
        // still within the reader?
        if re_index + vlc_len > last_index {
            let bits = (last_index - re_index) as u32;
            mb.partial_bit_count = bits as u8;
            mb.partial_bit_buffer = re_cache & !(u32::MAX.checked_shr(bits).unwrap_or(0));
            re_index = last_index;
            break;
        }
        re_index += vlc_len;
        pos += i32::from(run);
        if pos >= 64 {
            break;
        }
        let level = (level as i32 as u32).wrapping_mul(factor[pos as usize]).wrapping_add(1 << (DV_IWEIGHT_BITS - 1)) >> DV_IWEIGHT_BITS;
        block[usize::from(scan[pos as usize])] = level as i16;
        re_cache = gb.cache(re_index);
    }
    gb.index = re_index;
    mb.pos = pos as u8;
}

/// put_block_8x4: four rows of `block` (from `first`), clipped.
fn put_block_8x4(dest: &mut [u8], off: usize, stride: usize, block: &[i16; 64], first: usize) {
    let fits = stride.checked_mul(3).and_then(|r| r.checked_add(off + 8)).is_some_and(|end| end <= dest.len());
    if !fits {
        return;
    }
    for i in 0..4 {
        for j in 0..8 {
            dest[off + i * stride + j] = block[8 * (first + i) + j].clamp(0, 255) as u8;
        }
    }
}

/// The picture being decoded: Y, Cb and Cr planes and their strides.
struct Picture {
    planes: [Vec<u8>; 3],
    strides: [usize; 3],
}

/// The dvvideo decoder.
pub struct DvVideoDecoder {
    codec_id: CodecId,
    /// ff_dv_frame_profile's SL25 case: codec tag SL25, coded size 720x576.
    sl25_576: bool,
    sys: Option<&'static DvProfile>,
    work_chunks: Vec<WorkChunk>,
    idct_factor: Vec<u32>,
    ready: VecDeque<(VideoFrame, &'static DvProfile)>,
    /// The profile of the frame last returned.
    returned: Option<&'static DvProfile>,
    flushed: bool,
}

impl DvVideoDecoder {
    pub fn new(params: &CodecParameters) -> Self {
        let sl25_576 = params.tag == Some(CodecTag::fourcc(b"SL25")) && params.width == Some(720) && params.height == Some(576);
        Self {
            codec_id: CodecId::new("dvvideo"),
            sl25_576,
            sys: None,
            work_chunks: Vec::new(),
            idct_factor: vec![0; 2 * 4 * 16 * 64],
            ready: VecDeque::new(),
            returned: None,
            flushed: false,
        }
    }

    /// dv_init_weight_tables.
    fn init_weight_tables(&mut self, d: &DvProfile) {
        let factor = &mut self.idct_factor;
        let (mut f1, mut f2) = (0usize, if d.is_hd() { 4096 } else { 2816 });
        if d.is_hd() {
            // quantization quanta by QNO for DV100
            const DV100_QSTEP: [u32; 16] = [1, 1, 2, 3, 4, 5, 6, 7, 8, 16, 18, 20, 22, 24, 28, 52];
            let (w1, w2) = if d.height == 720 { (&DV_IWEIGHT_720_Y, &DV_IWEIGHT_720_C) } else { (&DV_IWEIGHT_1080_Y, &DV_IWEIGHT_1080_C) };
            for c in 0..4 {
                for &q in &DV100_QSTEP {
                    for i in 0..64 {
                        factor[f1] = (q << (c + 9)) * u32::from(w1[i]);
                        factor[f2] = (q << (c + 9)) * u32::from(w2[i]);
                        f1 += 1;
                        f2 += 1;
                    }
                }
            }
        } else {
            const DV_QUANT_AREAS: [usize; 4] = [6, 21, 43, 64];
            for weights in [&DV_IWEIGHT_88, &DV_IWEIGHT_248] {
                for shifts in &DV_QUANT_SHIFTS {
                    let mut i = 0;
                    for (c, &area) in DV_QUANT_AREAS.iter().enumerate() {
                        while i < area {
                            factor[f1] = u32::from(weights[i]) << (shifts[c] + 1);
                            factor[f2] = factor[f1] << 1;
                            f1 += 1;
                            f2 += 1;
                            i += 1;
                        }
                    }
                }
            }
        }
    }

    /// dvvideo_decode_frame.
    fn decode_frame(&mut self, buf: &[u8], pts: Option<i64>) -> Result<(VideoFrame, &'static DvProfile)> {
        let sys = frame_profile(self.sys, buf, buf.len(), self.sl25_576)
            .filter(|s| buf.len() >= s.frame_size)
            .ok_or_else(|| Error::invalid("dvvideo: could not find dv frame profile"))?;
        if !self.sys.is_some_and(|s| std::ptr::eq(s, sys)) {
            self.work_chunks = work_chunks(sys);
            self.init_weight_tables(sys);
            self.sys = Some(sys);
        }
        let (w, h) = (sys.width, sys.height);
        let cw = match sys.pix_fmt {
            PixelFormat::Yuv411P => w / 4,
            _ => w / 2,
        };
        let ch = if sys.pix_fmt == PixelFormat::Yuv420P { h / 2 } else { h };
        let mut pic = Picture { planes: [vec![0; w * h], vec![0; cw * ch], vec![0; cw * ch]], strides: [w, cw, cw] };
        let chunks = std::mem::take(&mut self.work_chunks);
        for chunk in &chunks {
            self.decode_video_segment(buf, sys, chunk, &mut pic);
        }
        self.work_chunks = chunks;
        let [y, u, v] = pic.planes;
        let planes = vec![
            VideoPlane { stride: w, data: y },
            VideoPlane { stride: cw, data: u },
            VideoPlane { stride: cw, data: v },
        ];
        Ok((VideoFrame { pts, planes }, sys))
    }

    /// dv_decode_video_segment: the five macroblocks of one video segment.
    fn decode_video_segment(&self, buf: &[u8], sys: &DvProfile, chunk: &WorkChunk, pic: &mut Picture) {
        let vlc = &DV_RL_VLC[..];
        let bpm = sys.bpm;
        let mut mb_data = [BlockInfo::default(); 5 * DV_MAX_BPM];
        let mut sblock = [[0i16; 64]; 5 * DV_MAX_BPM];
        let mut is_field_mode = [false; 5];
        let mut vs_bit_buffer_damaged = false;
        let mut mb_bit_buffer_damaged = [false; 5];
        let mut retried = false;
        let scans = [&ZIGZAG_DIRECT, &DV_ZIGZAG248_DIRECT];
        let seg = usize::from(chunk.buf_offset) * 80;
        if buf.len() < seg + 5 * 80 {
            return;
        }
        loop {
            for block in sblock.iter_mut() {
                block.fill(0);
            }
            // pass 1: read DC and AC coefficients in blocks
            let mut buf_ptr = seg;
            let mut vs_buffer = [0u8; 5 * 80];
            let mut vs_pb = PutBits::new(&mut vs_buffer);
            let mut sta = 0;
            for mb_index in 0..5 {
                let mb1 = mb_index * bpm;
                let header = buf[buf_ptr + 3];
                let quant = usize::from(header & 0x0f);
                // error concealment (on by default)
                if header >> 4 == 0x0E {
                    vs_bit_buffer_damaged = true;
                }
                if mb_index == 0 {
                    sta = header >> 4;
                } else if sta != header >> 4 {
                    vs_bit_buffer_damaged = true;
                }
                buf_ptr += 4;
                let mut mb_buffer = [0u8; 80];
                let mut pb = PutBits::new(&mut mb_buffer);
                is_field_mode[mb_index] = false;
                for j in 0..bpm {
                    let last_index = i64::from(sys.block_sizes[j]);
                    let mut gb = Bits::new(&buf[buf_ptr..], last_index);
                    // the DC
                    let dc = gb.get_sbits(9);
                    let dct_mode = gb.get_bits(1) as usize;
                    let class1 = gb.get_bits(2) as usize;
                    let mb = &mut mb_data[mb1 + j];
                    if sys.is_hd() {
                        mb.dct248 = false;
                        mb.factor = usize::from(j >= 4) * 4 * 16 * 64 + class1 * 16 * 64 + quant * 64;
                        is_field_mode[mb_index] |= j == 0 && dct_mode != 0;
                    } else {
                        mb.dct248 = dct_mode != 0;
                        mb.factor = usize::from(class1 == 3) * 2 * 22 * 64
                            + dct_mode * 22 * 64
                            + (quant + usize::from(DV_QUANT_OFFSET[class1])) * 64;
                    }
                    // unsigned: 128 is not added in the standard IDCT
                    sblock[mb1 + j][0] = (dc * 4 + 1024) as i16;
                    buf_ptr += (last_index >> 3) as usize;
                    mb.pos = 0;
                    mb.partial_bit_count = 0;
                    let scan = scans[usize::from(mb.dct248)];
                    decode_ac(&mut gb, mb, &mut sblock[mb1 + j], &self.idct_factor[mb.factor..mb.factor + 64], scan, vlc);
                    // the remaining bits go to a new buffer only if the block is finished
                    if mb.pos >= 64 {
                        bit_copy(&mut pb, &mut gb);
                    }
                    if mb.pos >= 64 && mb.pos < 127 {
                        vs_bit_buffer_damaged = true;
                        mb_bit_buffer_damaged[mb_index] = true;
                    }
                }
                if mb_bit_buffer_damaged[mb_index] {
                    continue;
                }
                // pass 2: within the macroblock
                let size = pb.count();
                let mut gb = Bits::new(pb.finish(), size);
                let mut j = 0;
                while j < bpm {
                    let mb = &mut mb_data[mb1 + j];
                    if mb.pos < 64 && gb.bits_left() > 0 {
                        let scan = scans[usize::from(mb.dct248)];
                        decode_ac(&mut gb, mb, &mut sblock[mb1 + j], &self.idct_factor[mb.factor..mb.factor + 64], scan, vlc);
                        // if still not finished, no need to parse other blocks
                        if mb.pos < 64 {
                            break;
                        }
                        if mb.pos < 127 {
                            vs_bit_buffer_damaged = true;
                            mb_bit_buffer_damaged[mb_index] = true;
                        }
                    }
                    j += 1;
                }
                // all blocks are finished: the extra bits serve the segment
                if j >= bpm {
                    bit_copy(&mut vs_pb, &mut gb);
                }
            }
            // pass 3: over the whole video segment
            let size = vs_pb.count();
            let mut gb = Bits::new(vs_pb.finish(), size);
            for (k, mb) in mb_data[..5 * bpm].iter_mut().enumerate() {
                if mb.pos < 64 && gb.bits_left() > 0 && !vs_bit_buffer_damaged {
                    let scan = scans[usize::from(mb.dct248)];
                    decode_ac(&mut gb, mb, &mut sblock[k], &self.idct_factor[mb.factor..mb.factor + 64], scan, vlc);
                }
                if mb.pos >= 64 && mb.pos < 127 {
                    // AC EOB marker is absent
                    vs_bit_buffer_damaged = true;
                }
            }
            if vs_bit_buffer_damaged && !retried {
                // concealing bitstream errors
                retried = true;
                continue;
            }
            break;
        }
        self.place(buf, sys, chunk, pic, &mb_data, &mut sblock, &is_field_mode);
    }

    /// The IDCT of each block, placed in the picture.
    #[allow(clippy::too_many_arguments)]
    fn place(
        &self,
        buf: &[u8],
        sys: &DvProfile,
        chunk: &WorkChunk,
        pic: &mut Picture,
        mb_data: &[BlockInfo; 5 * DV_MAX_BPM],
        sblock: &mut [[i16; 64]; 5 * DV_MAX_BPM],
        is_field_mode: &[bool; 5],
    ) {
        let put = |mb: &BlockInfo, dest: &mut [u8], off: usize, stride: usize, block: &mut [i16; 64]| {
            if mb.dct248 {
                simple_idct248_put(dest, off, stride, block);
            } else {
                simple_idct_put(dest, off, stride, block);
            }
        };
        let bpm = sys.bpm;
        let pix = sys.pix_fmt;
        let [ls_y, ls_c, _] = pic.strides;
        for mb_index in 0..5 {
            let (mb_x, mb_y) = mb_xy(sys, buf[1], chunk, mb_index);
            let field = is_field_mode[mb_index];
            let base = mb_index * bpm;
            // luminance
            let y_stride = if pix == PixelFormat::Yuv420P
                || (pix == PixelFormat::Yuv411P && mb_x >= 704 / 8)
                || (sys.height >= 720 && mb_y != 134)
            {
                ls_y << if field { 0 } else { 3 }
            } else {
                2 << 3
            };
            let y_off = (mb_y * ls_y + mb_x) << 3;
            let luma = &mut pic.planes[0];
            if mb_y == 134 && field {
                // dv100_idct_put_last_row_field_luma
                for b in &mut sblock[base..base + 4] {
                    simple_idct(b);
                }
                let s2 = ls_y << 1;
                put_block_8x4(luma, y_off, s2, &sblock[base], 0);
                put_block_8x4(luma, y_off + 16, s2, &sblock[base], 4);
                put_block_8x4(luma, y_off + 8, s2, &sblock[base + 1], 0);
                put_block_8x4(luma, y_off + 24, s2, &sblock[base + 1], 4);
                put_block_8x4(luma, y_off + ls_y, s2, &sblock[base + 2], 0);
                put_block_8x4(luma, y_off + 16 + ls_y, s2, &sblock[base + 2], 4);
                put_block_8x4(luma, y_off + 8 + ls_y, s2, &sblock[base + 3], 0);
                put_block_8x4(luma, y_off + 24 + ls_y, s2, &sblock[base + 3], 4);
            } else {
                let linesize = ls_y << usize::from(field);
                put(&mb_data[base], luma, y_off, linesize, &mut sblock[base]);
                if sys.video_stype == 4 {
                    // SD 4:2:2
                    put(&mb_data[base + 2], luma, y_off + 8, linesize, &mut sblock[base + 2]);
                } else {
                    put(&mb_data[base + 1], luma, y_off + 8, linesize, &mut sblock[base + 1]);
                    put(&mb_data[base + 2], luma, y_off + y_stride, linesize, &mut sblock[base + 2]);
                    put(&mb_data[base + 3], luma, y_off + 8 + y_stride, linesize, &mut sblock[base + 3]);
                }
            }
            // chrominance: Cr (plane 2), then Cb (plane 1)
            let mut k = base + 4;
            let c_off = ((mb_y >> usize::from(pix == PixelFormat::Yuv420P)) * ls_c
                + (mb_x >> if pix == PixelFormat::Yuv411P { 2 } else { 1 }))
                << 3;
            for plane in [2, 1] {
                let dest = &mut pic.planes[plane];
                if pix == PixelFormat::Yuv411P && mb_x >= 704 / 8 {
                    let mut pixels = [0u8; 64];
                    put(&mb_data[k], &mut pixels, 0, 8, &mut sblock[k]);
                    let fits = c_off + 15 * ls_c + 4 <= dest.len();
                    if fits {
                        for y in 0..8 {
                            for x in 0..4 {
                                dest[c_off + y * ls_c + x] = pixels[8 * y + x];
                                dest[c_off + (y + 8) * ls_c + x] = pixels[8 * y + 4 + x];
                            }
                        }
                    }
                    k += 1;
                } else {
                    let y_stride = if mb_y == 134 { 8 } else { ls_c << if field { 0 } else { 3 } };
                    if mb_y == 134 && field {
                        // dv100_idct_put_last_row_field_chroma
                        simple_idct(&mut sblock[k]);
                        simple_idct(&mut sblock[k + 1]);
                        let s2 = ls_c << 1;
                        put_block_8x4(dest, c_off, s2, &sblock[k], 0);
                        put_block_8x4(dest, c_off + 8, s2, &sblock[k], 4);
                        put_block_8x4(dest, c_off + ls_c, s2, &sblock[k + 1], 0);
                        put_block_8x4(dest, c_off + 8 + ls_c, s2, &sblock[k + 1], 4);
                        k += 2;
                    } else {
                        let linesize = ls_c << usize::from(field);
                        put(&mb_data[k], dest, c_off, linesize, &mut sblock[k]);
                        k += 1;
                        if bpm == 8 {
                            put(&mb_data[k], dest, c_off + y_stride, linesize, &mut sblock[k]);
                            k += 1;
                        }
                    }
                }
            }
        }
    }
}

impl Decoder for DvVideoDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// One frame per packet; a packet without a whole frame of a known
    /// profile is an error, as FFmpeg's.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let frame = self.decode_frame(&packet.data, packet.pts)?;
        self.ready.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some((frame, sys)) => {
                self.returned = Some(sys);
                Ok(Frame::Video(frame))
            }
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.flushed = false;
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.returned.map(|s| s.pix_fmt)
    }

    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        self.returned.map(|s| (s.width as u32, s.height as u32))
    }
}

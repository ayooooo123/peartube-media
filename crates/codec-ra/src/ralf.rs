//! RealAudio Lossless (RALF) decoder.
//! Ported faithfully from FFmpeg (libavcodec/ralf.c).
//! Commit: 2da55bf.
//! License: LGPL-2.1-or-later.

#![forbid(unsafe_code)]

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet,
    Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderBe;
use crate::ralf_tables::*;

const FILTER_NONE: usize = 0;
const FILTER_RAW: usize = 642;
const RALF_MAX_PKT_SIZE: usize = 8192;

#[derive(Clone, Debug)]
pub struct VlcTree {
    nodes: Vec<[u16; 2]>,
}

impl VlcTree {
    pub fn from_packed(data: &[u8], elems: usize) -> Self {
        let mut lens = vec![0u8; elems];
        let mut counts = [0usize; 17];
        let mut nb = 0;
        let mut data_idx = 0;

        for i in 0..elems {
            let byte = data[data_idx];
            let cur_len = (if nb != 0 { byte & 0x0f } else { byte >> 4 }) + 1;
            counts[cur_len as usize] += 1;
            lens[i] = cur_len;
            data_idx += nb;
            nb ^= 1;
        }

        let mut prefixes = [0u32; 18];
        for i in 1..=16 {
            prefixes[i + 1] = (prefixes[i] + counts[i] as u32) << 1;
        }

        let mut nodes: Vec<[u16; 2]> = vec![[0, 0]];

        for s in 0..elems {
            let len = lens[s] as usize;
            let code = prefixes[len];
            prefixes[len] += 1;

            let mut curr = 0;
            for bit_pos in (0..len).rev() {
                let bit = ((code >> bit_pos) & 1) as usize;
                if bit_pos == 0 {
                    nodes[curr][bit] = (s as u16) | 0x8000;
                } else {
                    let next = nodes[curr][bit];
                    if next == 0 {
                        let new_node = nodes.len() as u16;
                        nodes.push([0, 0]);
                        nodes[curr][bit] = new_node;
                        curr = new_node as usize;
                    } else {
                        curr = next as usize;
                    }
                }
            }
        }

        Self { nodes }
    }

    #[inline]
    pub fn decode(&self, gb: &mut BitReaderBe) -> Option<u16> {
        let mut curr = 0usize;
        loop {
            let bit = gb.read_bit()? as usize;
            let next = self.nodes.get(curr)?[bit];
            if next >= 0x8000 {
                return Some(next & 0x7fff);
            }
            if next == 0 {
                return None;
            }
            curr = next as usize;
        }
    }
}

pub struct VlcSet {
    filter_params: VlcTree,
    bias: VlcTree,
    coding_mode: VlcTree,
    filter_coeffs: Vec<Vec<VlcTree>>, // [10][11]
    short_codes: Vec<VlcTree>,        // [15]
    long_codes: Vec<VlcTree>,         // [125]
}

impl VlcSet {
    pub fn new(mode_idx: usize) -> Self {
        let filter_params = VlcTree::from_packed(&FILTER_PARAM_DEF[mode_idx], FILTERPARAM_ELEMENTS);
        let bias = VlcTree::from_packed(&BIAS_DEF[mode_idx], BIAS_ELEMENTS);
        let coding_mode = VlcTree::from_packed(&CODING_MODE_DEF[mode_idx], CODING_MODE_ELEMENTS);

        let mut filter_coeffs = Vec::with_capacity(10);
        for j in 0..10 {
            let mut row = Vec::with_capacity(11);
            for k in 0..11 {
                row.push(VlcTree::from_packed(&FILTER_COEFFS_DEF[mode_idx][j][k], FILTER_COEFFS_ELEMENTS));
            }
            filter_coeffs.push(row);
        }

        let mut short_codes = Vec::with_capacity(15);
        for j in 0..15 {
            short_codes.push(VlcTree::from_packed(&SHORT_CODES_DEF[mode_idx][j], SHORT_CODES_ELEMENTS));
        }

        let mut long_codes = Vec::with_capacity(125);
        for j in 0..125 {
            long_codes.push(VlcTree::from_packed(&LONG_CODES_DEF[mode_idx][j], LONG_CODES_ELEMENTS));
        }

        Self {
            filter_params,
            bias,
            coding_mode,
            filter_coeffs,
            short_codes,
            long_codes,
        }
    }
}

#[inline]
fn extend_code(gb: &mut BitReaderBe, mut val: i32, range: i32, bits: usize) -> Option<i32> {
    if val == 0 {
        val = -range - gb.read_ue_golomb()? as i32;
    } else if val == range * 2 {
        val = range + gb.read_ue_golomb()? as i32;
    } else {
        val -= range;
    }
    if bits > 0 {
        val = (val << bits) | (gb.read_bits(bits)? as i32);
    }
    Some(val)
}

#[inline]
fn av_log2(v: u32) -> i32 {
    if v == 0 {
        0
    } else {
        31 - v.leading_zeros() as i32
    }
}

pub struct RalfDecoder {
    codec_id: CodecId,
    channels: usize,
    sample_rate: u32,
    max_frame_size: usize,
    sets: [VlcSet; 3],
    channel_data: [[i32; 4096]; 2],
    filter_params: usize,
    filter_length: usize,
    filter_bits: usize,
    filter: [i32; 64],
    bias: [i32; 2],
    sample_offset: usize,
    block_size: [usize; 4096],
    block_pts: [usize; 4096],
    pkt_buf: Vec<u8>,
    has_pkt: bool,
}

impl RalfDecoder {
    pub fn new(extradata: &[u8]) -> CoreResult<Self> {
        if extradata.len() < 24 || &extradata[..4] != b"LSD:" {
            return Err(CoreError::invalid("ralf: invalid extradata header"));
        }

        let version = u16::from_be_bytes([extradata[4], extradata[5]]);
        if version != 0x103 {
            return Err(CoreError::unsupported(format!("ralf: unsupported version {version:#x}")));
        }

        let channels = u16::from_be_bytes([extradata[8], extradata[9]]) as usize;
        let sample_rate = u32::from_be_bytes([extradata[12], extradata[13], extradata[14], extradata[15]]);
        if !(1..=2).contains(&channels) || !(8000..=96000).contains(&sample_rate) {
            return Err(CoreError::invalid(format!("ralf: invalid channels {channels} or rate {sample_rate}")));
        }

        let raw_max_size = u32::from_be_bytes([extradata[16], extradata[17], extradata[18], extradata[19]]) as usize;
        if raw_max_size > (1 << 20) || raw_max_size == 0 {
            return Err(CoreError::invalid("ralf: invalid max_frame_size"));
        }
        let max_frame_size = raw_max_size.max(sample_rate as usize);

        let sets = [VlcSet::new(0), VlcSet::new(1), VlcSet::new(2)];

        Ok(Self {
            codec_id: CodecId::new("ralf"),
            channels,
            sample_rate,
            max_frame_size,
            sets,
            channel_data: [[0; 4096]; 2],
            filter_params: 0,
            filter_length: 0,
            filter_bits: 0,
            filter: [0; 64],
            bias: [0; 2],
            sample_offset: 0,
            block_size: [0; 4096],
            block_pts: [0; 4096],
            pkt_buf: Vec::new(),
            has_pkt: false,
        })
    }

    fn decode_channel(
        &mut self,
        gb: &mut BitReaderBe,
        ch: usize,
        length: usize,
        mode: usize,
        bits: usize,
    ) -> CoreResult<()> {
        let set = &self.sets[mode];

        self.filter_params = set
            .filter_params
            .decode(gb)
            .ok_or_else(|| CoreError::invalid("ralf: failed to decode filter_params"))? as usize;

        if self.filter_params > 1 {
            self.filter_bits = (self.filter_params - 2) >> 6;
            self.filter_length = self.filter_params - (self.filter_bits << 6) - 1;
        }

        if self.filter_params == FILTER_RAW {
            for i in 0..length {
                self.channel_data[ch][i] = gb
                    .read_bits(bits)
                    .ok_or_else(|| CoreError::invalid("ralf: failed to read raw bits"))? as i32;
            }
            self.bias[ch] = 0;
            return Ok(());
        }

        let raw_bias = set
            .bias
            .decode(gb)
            .ok_or_else(|| CoreError::invalid("ralf: failed to decode bias"))? as i32;
        self.bias[ch] = extend_code(gb, raw_bias, 127, 4)
            .ok_or_else(|| CoreError::invalid("ralf: failed to extend bias"))?;

        if self.filter_params == FILTER_NONE {
            self.channel_data[ch][..length].fill(0);
            return Ok(());
        }

        if self.filter_params > 1 {
            let mut cmode = 0i32;
            let mut coeff = 0i32;
            let add_bits = self.filter_bits;

            for i in 0..self.filter_length {
                let vlc_idx = (cmode + 5) as usize;
                let t_raw = set.filter_coeffs[self.filter_bits][vlc_idx]
                    .decode(gb)
                    .ok_or_else(|| CoreError::invalid("ralf: failed to decode filter coeff"))? as i32;
                let t = extend_code(gb, t_raw, 21, add_bits)
                    .ok_or_else(|| CoreError::invalid("ralf: failed to extend filter coeff"))?;

                if cmode == 0 {
                    coeff = coeff.wrapping_sub((12i32) << add_bits);
                }
                coeff = t.wrapping_sub(coeff);
                if i < 64 {
                    self.filter[i] = coeff;
                }

                cmode = coeff >> add_bits;
                if cmode < 0 {
                    cmode = -1 - av_log2((-cmode) as u32);
                    if cmode < -5 {
                        cmode = -5;
                    }
                } else if cmode > 0 {
                    cmode = 1 + av_log2(cmode as u32);
                    if cmode > 5 {
                        cmode = 5;
                    }
                }
            }
        }

        let code_params = set
            .coding_mode
            .decode(gb)
            .ok_or_else(|| CoreError::invalid("ralf: failed to decode coding_mode"))? as usize;

        let (add_bits, range, range2, code_vlc) = if code_params >= 15 {
            let mut ab = ((code_params / 5).saturating_sub(3) / 2).clamp(0, 10);
            if ab > 9 && (code_params % 5) != 2 {
                ab -= 1;
            }
            (ab, 10i32, 21i32, &set.long_codes[code_params - 15])
        } else {
            (0, 6i32, 13i32, &set.short_codes[code_params])
        };

        let mut i = 0;
        while i < length {
            let t = code_vlc
                .decode(gb)
                .ok_or_else(|| CoreError::invalid("ralf: failed to decode codebook"))? as i32;
            let code1 = t / range2;
            let code2 = t % range2;

            let ext1 = extend_code(gb, code1, range, 0)
                .ok_or_else(|| CoreError::invalid("ralf: failed to extend code1"))?;
            let ext2 = extend_code(gb, code2, range, 0)
                .ok_or_else(|| CoreError::invalid("ralf: failed to extend code2"))?;

            self.channel_data[ch][i] = ext1 * (1 << add_bits);
            self.channel_data[ch][i + 1] = ext2 * (1 << add_bits);

            if add_bits > 0 {
                let bits1 = gb
                    .read_bits(add_bits)
                    .ok_or_else(|| CoreError::invalid("ralf: failed to read extra bits"))? as i32;
                let bits2 = gb
                    .read_bits(add_bits)
                    .ok_or_else(|| CoreError::invalid("ralf: failed to read extra bits"))? as i32;
                self.channel_data[ch][i] |= bits1;
                self.channel_data[ch][i + 1] |= bits2;
            }

            i += 2;
        }

        Ok(())
    }

    fn apply_lpc(&mut self, ch: usize, length: usize, bits: usize) {
        let bias = 1i32 << (self.filter_bits - 1);
        let max_clip = (1i32 << bits) - 1;
        let min_clip = -max_clip - 1;

        for i in 1..length {
            let flen = self.filter_length.min(i);
            let mut acc = 0i64;
            for j in 0..flen {
                acc += (self.filter[j] as i64) * (self.channel_data[ch][i - j - 1] as i64);
            }
            let acc32 = if acc < 0 {
                let v = (acc + bias as i64 - 1) >> self.filter_bits;
                (v as i32).max(min_clip)
            } else {
                let v = (acc + bias as i64) >> self.filter_bits;
                (v as i32).min(max_clip)
            };
            self.channel_data[ch][i] += acc32;
        }
    }

    fn decode_block(
        &mut self,
        gb: &mut BitReaderBe,
        dst0: &mut [i16],
        dst1: &mut [i16],
    ) -> CoreResult<usize> {
        let unary = gb.read_unary(0, 6).ok_or_else(|| CoreError::invalid("ralf: read unary failed"))?;
        let mut len_pow = 12 - unary;
        if len_pow <= 7 {
            len_pow ^= 1;
        }
        let len = 1 << len_pow;

        if self.sample_offset + len > self.max_frame_size || len > 4096 {
            return Err(CoreError::invalid("ralf: too many samples"));
        }

        let dmode = if self.channels > 1 {
            (gb.read_bits(2).ok_or_else(|| CoreError::invalid("ralf: read dmode failed"))? + 1) as usize
        } else {
            0
        };

        let mode = [(dmode == 4) as usize, if dmode >= 2 { 2 } else { 0 }];
        let bits = [16usize, if mode[1] == 2 { 17 } else { 16 }];

        for ch in 0..self.channels {
            self.decode_channel(gb, ch, len, mode[ch], bits[ch])?;
            if self.filter_params > 1 && self.filter_params != FILTER_RAW {
                self.filter_bits += 3;
                self.apply_lpc(ch, len, bits[ch]);
            }
        }

        let off = self.sample_offset;
        match dmode {
            0 => {
                for i in 0..len {
                    dst0[off + i] = (self.channel_data[0][i] + self.bias[0]) as i16;
                }
            }
            1 => {
                for i in 0..len {
                    dst0[off + i] = (self.channel_data[0][i] + self.bias[0]) as i16;
                    dst1[off + i] = (self.channel_data[1][i] + self.bias[1]) as i16;
                }
            }
            2 => {
                for i in 0..len {
                    self.channel_data[0][i] += self.bias[0];
                    dst0[off + i] = self.channel_data[0][i] as i16;
                    dst1[off + i] = (self.channel_data[0][i] - (self.channel_data[1][i] + self.bias[1])) as i16;
                }
            }
            3 => {
                for i in 0..len {
                    let t = self.channel_data[0][i] + self.bias[0];
                    let t2 = self.channel_data[1][i] + self.bias[1];
                    dst0[off + i] = (t + t2) as i16;
                    dst1[off + i] = t as i16;
                }
            }
            4 => {
                for i in 0..len {
                    let t = self.channel_data[1][i] + self.bias[1];
                    let t2 = ((self.channel_data[0][i] + self.bias[0]) * 2) | (t & 1);
                    dst0[off + i] = ((t2 + t) / 2) as i16;
                    dst1[off + i] = ((t2 - t) / 2) as i16;
                }
            }
            _ => unreachable!(),
        }

        self.sample_offset += len;
        Ok(len)
    }

    pub fn decode_packet(&mut self, data: &[u8]) -> CoreResult<Option<Vec<Vec<i16>>>> {
        let mut assembled = Vec::new();
        let (src, src_len) = if self.has_pkt {
            self.has_pkt = false;
            let table_bytes = ((u16::from_be_bytes([data[0], data[1]]) as usize) + 7) >> 3;
            if table_bytes + 3 > data.len() || data.len() > RALF_MAX_PKT_SIZE {
                return Err(CoreError::invalid("ralf: wrong packet size"));
            }
            if self.pkt_buf.len() < 2 + table_bytes || &self.pkt_buf[..2 + table_bytes] != &data[..2 + table_bytes] {
                return Err(CoreError::invalid("ralf: packet header mismatch"));
            }
            self.pkt_buf.truncate(RALF_MAX_PKT_SIZE);
            self.pkt_buf.extend_from_slice(&data[2 + table_bytes..]);
            assembled = std::mem::take(&mut self.pkt_buf);
            (assembled.as_slice(), assembled.len())
        } else {
            if data.len() == RALF_MAX_PKT_SIZE {
                self.pkt_buf.clear();
                self.pkt_buf.extend_from_slice(data);
                self.has_pkt = true;
                return Ok(None);
            }
            (data, data.len())
        };
        if src_len < 5 {
            return Err(CoreError::invalid("ralf: packet too short"));
        }

        let table_size = u16::from_be_bytes([src[0], src[1]]) as usize;
        let table_bytes = (table_size + 7) >> 3;
        if src_len < table_bytes + 3 {
            return Err(CoreError::invalid("ralf: packet too short for table"));
        }

        let mut gb = BitReaderBe::with_bits(&src[2..2 + table_bytes], table_size);
        let mut num_blocks = 0;
        let block_bits = 13 + self.channels;

        while gb.bits_left() > 0 {
            if num_blocks >= self.block_size.len() {
                return Err(CoreError::invalid("ralf: too many blocks"));
            }
            self.block_size[num_blocks] = gb.read_bits(block_bits).ok_or_else(|| CoreError::invalid("ralf: read block size failed"))? as usize;
            if gb.read_bit().ok_or_else(|| CoreError::invalid("ralf: read block pts flag failed"))? != 0 {
                self.block_pts[num_blocks] = gb.read_bits(9).ok_or_else(|| CoreError::invalid("ralf: read block pts failed"))? as usize;
            } else {
                self.block_pts[num_blocks] = 0;
            }
            num_blocks += 1;
        }

        let mut dst0 = vec![0i16; self.max_frame_size];
        let mut dst1 = vec![0i16; self.max_frame_size];
        let mut block_pointer = 2 + table_bytes;
        let mut bytes_left = src_len - table_bytes - 2;

        self.sample_offset = 0;
        for i in 0..num_blocks {
            let bsize = self.block_size[i];
            if bytes_left < bsize {
                break;
            }
            let block_data = &src[block_pointer..block_pointer + bsize];
            let mut bgb = BitReaderBe::new(block_data);
            if self.decode_block(&mut bgb, &mut dst0, &mut dst1).is_err() {
                break;
            }
            block_pointer += bsize;
            bytes_left -= bsize;
        }

        if assembled.capacity() > 0 {
            self.pkt_buf = assembled;
            self.pkt_buf.clear();
        }

        let total_samples = self.sample_offset;
        if total_samples == 0 {
            return Ok(None);
        }

        dst0.truncate(total_samples);
        if self.channels > 1 {
            dst1.truncate(total_samples);
            Ok(Some(vec![dst0, dst1]))
        } else {
            Ok(Some(vec![dst0]))
        }
    }
}

pub fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    if params.extradata.is_empty() {
        return Err(CoreError::invalid("ralf: extradata required"));
    }
    let inner = RalfDecoder::new(&params.extradata)?;
    Ok(Box::new(RalfDecoderWrapper {
        inner,
        pending_frames: Vec::new(),
    }))
}

struct RalfDecoderWrapper {
    inner: RalfDecoder,
    pending_frames: Vec<Frame>,
}

impl Decoder for RalfDecoderWrapper {
    fn codec_id(&self) -> &CodecId {
        &self.inner.codec_id
    }
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::S16P,
            sample_rate: self.inner.sample_rate,
            channels: self.inner.channels as u16,
        })
    }


    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        if let Some(channels_data) = self.inner.decode_packet(&packet.data)? {
            let samples = channels_data[0].len() as u32;
            let mut planes = Vec::with_capacity(channels_data.len());
            for ch in channels_data {
                let mut b = Vec::with_capacity(ch.len() * 2);
                for s in ch {
                    b.extend_from_slice(&s.to_le_bytes());
                }
                planes.push(b);
            }
            self.pending_frames.push(Frame::Audio(AudioFrame {
                samples,
                pts: packet.pts,
                data: planes,
            }));
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
        self.inner.has_pkt = false;
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.pending_frames.clear();
        self.inner.has_pkt = false;
        self.inner.sample_offset = 0;
        Ok(())
    }
}

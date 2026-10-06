//! RealAudio Cook decoder.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/cook.c, libavcodec/cookdata.h.
//! License: LGPL-2.1-or-later.
#![forbid(unsafe_code)]

use std::sync::Arc;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet,
    Result as CoreResult, SampleFormat,
};

use crate::bitreader::BitReaderBe;
use crate::cook_tables::*;
use crate::vlc_len::LengthVlc;

const MONO: u32 = 0x1000001;
const STEREO: u32 = 0x1000002;
const JOINT_STEREO: u32 = 0x1000003;
const MC_COOK: u32 = 0x2000000;

const SUBBAND_SIZE: usize = 20;

pub struct COOKSubpacket {
    pub ch_idx: usize,
    pub size: usize,
    pub num_channels: usize,
    pub cookversion: u32,
    pub subbands: usize,
    pub js_subband_start: usize,
    pub js_vlc_bits: usize,
    pub samples_per_channel: usize,
    pub log2_numvector_size: usize,
    pub channel_mask: u32,
    pub channel_coupling: Option<LengthVlc>,
    pub joint_stereo: bool,
    pub bits_per_subpacket: usize,
    pub bits_per_subpdiv: usize,
    pub total_subbands: usize,
    pub numvector_size: usize,

    pub mono_previous_buffer1: Vec<f32>,
    pub mono_previous_buffer2: Vec<f32>,

    pub gains1_now: [i32; 9],
    pub gains1_previous: [i32; 9],
    pub gains2_now: [i32; 9],
    pub gains2_previous: [i32; 9],
}

pub struct CookEngine {
    pub samples_per_channel: usize,
    pub gain_size_factor: usize,
    pub gain_table: [f32; 31],
    pub mlt_window: Vec<f32>,
    pub mdct_tab_d: Vec<f32>,
    pub mdct_tab_u: Vec<f32>,
    pub envelope_quant_index: Vec<LengthVlc>,
    pub sqvh: Vec<LengthVlc>,
    pub cplscales: [&'static [f32]; 5],
}

impl CookEngine {
    pub fn new(samples_per_channel: usize) -> CoreResult<Self> {
        let gain_size_factor = samples_per_channel / 8;
        let mut gain_table = [0f32; 31];
        for i in 0..31 {
            gain_table[i] = POW2TAB[i + 48].powf(1.0 / gain_size_factor as f32);
        }

        let mut mlt_window = vec![0f32; samples_per_channel];
        let factor = (2.0f64 / samples_per_channel as f64).sqrt() as f32;
        for j in 0..samples_per_channel {
            let ang = (j as f64 + 0.5) * (std::f64::consts::PI / (2.0 * samples_per_channel as f64));
            mlt_window[j] = ang.sin() as f32 * factor;
        }

        let len = samples_per_channel;
        let half = len >> 1;
        let scale = 1.0f64 / 32768.0f64;
        let phase = std::f64::consts::PI / (4.0 * len as f64);
        let mut mdct_tab_d = Vec::with_capacity(half * len);
        let mut mdct_tab_u = Vec::with_capacity(half * len);
        for i in 0..half {
            let i_d = phase * (4.0 * half as f64 - 2.0 * i as f64 - 1.0);
            let i_u = phase * (3.0 * len as f64 + 2.0 * i as f64 + 1.0);
            for j in 0..len {
                let a = (2 * j + 1) as f64;
                mdct_tab_d.push(((a * i_d).cos() * scale) as f32);
                mdct_tab_u.push((-(a * i_u).cos() * scale) as f32);
            }
        }

        let mut envelope_quant_index = Vec::with_capacity(13);
        for i in 0..13 {
            let vlc = LengthVlc::from_counts(&ENVELOPE_HUFFCOUNTS[i], &ENVELOPE_HUFFSYMS[i], -12)
                .map_err(|e| Error::invalid(format!("cook: envelope vlc {i}: {e}")))?;
            envelope_quant_index.push(vlc);
        }

        let mut sqvh = Vec::with_capacity(7);
        let sqvh_counts = &CVH_HUFFCOUNTS;
        let v0 = LengthVlc::from_counts(&sqvh_counts[0], &CVH_HUFFSYMS_0, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 0: {e}")))?;
        let v1 = LengthVlc::from_counts(&sqvh_counts[1], &CVH_HUFFSYMS_1, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 1: {e}")))?;
        let v2 = LengthVlc::from_counts(&sqvh_counts[2], &CVH_HUFFSYMS_2, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 2: {e}")))?;
        let v3 = LengthVlc::from_counts(&sqvh_counts[3], &CVH_HUFFSYMS_3, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 3: {e}")))?;
        let v4 = LengthVlc::from_counts(&sqvh_counts[4], &CVH_HUFFSYMS_4, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 4: {e}")))?;
        let v5 = LengthVlc::from_counts(&sqvh_counts[5], &CVH_HUFFSYMS_5, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 5: {e}")))?;
        let v6 = LengthVlc::from_counts(&sqvh_counts[6], &CVH_HUFFSYMS_6, 0)
            .map_err(|e| Error::invalid(format!("cook: sqvh vlc 6: {e}")))?;
        sqvh.push(v0);
        sqvh.push(v1);
        sqvh.push(v2);
        sqvh.push(v3);
        sqvh.push(v4);
        sqvh.push(v5);
        sqvh.push(v6);

        let cplscales: [&'static [f32]; 5] = [
            &CPLSCALE_2,
            &CPLSCALE_3,
            &CPLSCALE_4,
            &CPLSCALE_5,
            &CPLSCALE_6,
        ];

        Ok(Self {
            samples_per_channel,
            gain_size_factor,
            gain_table,
            mlt_window,
            mdct_tab_d,
            mdct_tab_u,
            envelope_quant_index,
            sqvh,
            cplscales,
        })
    }

    pub fn run_immdct_full(&self, inbuffer: &[f32], out: &mut [f32]) {
        let s_len = self.samples_per_channel;
        let half = s_len >> 1;
        let len4 = s_len >> 1; // 512 (quarter of full 2048-point output)
        let len2 = s_len;      // 1024 (half of full 2048-point output)
        let full_len = s_len * 2; // 2048
        let mut dst = vec![0f32; s_len];
        for i in 0..half {
            let mut sd = 0f32;
            let mut su = 0f32;
            let row_off = i * s_len;
            for j in 0..s_len {
                let v = inbuffer[j];
                sd += self.mdct_tab_d[row_off + j] * v;
                su += self.mdct_tab_u[row_off + j] * v;
            }
            dst[i] = sd;
            dst[i + half] = su;
        }
        out[len4..len4 + s_len].copy_from_slice(&dst);
        for i in 0..len4 {
            out[i] = -out[len2 - i - 1];
            out[full_len - i - 1] = out[len2 + i];
        }
    }
}

pub struct CookDecoder {
    pub codec_id: CodecId,
    pub channels: usize,
    pub sample_rate: u32,
    pub block_align: usize,
    pub samples_per_channel: usize,
    pub discarded_packets: usize,
    pub random_state: AvLfg,
    pub engine: Arc<CookEngine>,
    pub subpackets: Vec<COOKSubpacket>,
}

impl CookDecoder {
    pub fn new(params: &CodecParameters) -> CoreResult<Self> {
        let ed = &params.extradata;
        if ed.len() < 8 {
            return Err(Error::invalid("cook: extradata < 8 bytes"));
        }
        let channels = params.channels.unwrap_or(1) as usize;
        if channels == 0 || channels > 64 {
            return Err(Error::invalid("cook: invalid channels"));
        }
        let block_align = 0;

        let mut subpackets = Vec::new();
        let mut offset = 0;
        let mut total_channels = 0;
        let mut samples_per_channel = 0;
        let mut channel_mask = 0u32;

        while offset + 14 <= ed.len() {
            let cookversion = u32::from_be_bytes([ed[offset], ed[offset + 1], ed[offset + 2], ed[offset + 3]]);
            let samples_per_frame = u16::from_be_bytes([ed[offset + 4], ed[offset + 5]]) as usize;
            let subbands = u16::from_be_bytes([ed[offset + 6], ed[offset + 7]]) as usize;
            // 4 bytes unused at offset+8..offset+12
            let js_subband_start = u16::from_be_bytes([ed[offset + 12], ed[offset + 13]]) as usize;
            offset += 14;

            let js_vlc_bits = if offset + 2 <= ed.len() {
                let v = u16::from_be_bytes([ed[offset], ed[offset + 1]]) as usize;
                offset += 2;
                v
            } else {
                0
            };

            let mut sp_samples_per_channel = samples_per_frame / channels;
            let bits_per_subpacket = 0;
            let mut log2_numvector_size = 5;
            let mut total_subbands = subbands;
            let mut num_channels = 1;
            let mut joint_stereo = false;
            let mut bits_per_subpdiv = 0;
            let mut sp_channel_mask = 0u32;

            match cookversion {
                MONO => {
                    if channels != 1 {
                        return Err(Error::invalid("cook: MONO version but container channels != 1"));
                    }
                }
                STEREO => {
                    if channels != 1 {
                        bits_per_subpdiv = 1;
                        num_channels = 2;
                    }
                }
                JOINT_STEREO => {
                    if ed.len() >= 16 {
                        total_subbands = subbands + js_subband_start;
                        joint_stereo = true;
                        num_channels = 2;
                    }
                    if sp_samples_per_channel > 256 {
                        log2_numvector_size = 6;
                    }
                    if sp_samples_per_channel > 512 {
                        log2_numvector_size = 7;
                    }
                }
                MC_COOK => {
                    if offset + 4 <= ed.len() {
                        sp_channel_mask = u32::from_be_bytes([ed[offset], ed[offset + 1], ed[offset + 2], ed[offset + 3]]);
                        offset += 4;
                    }
                    channel_mask |= sp_channel_mask;
                    if sp_channel_mask.count_ones() > 1 {
                        total_subbands = subbands + js_subband_start;
                        joint_stereo = true;
                        num_channels = 2;
                        sp_samples_per_channel = samples_per_frame >> 1;
                        if sp_samples_per_channel > 256 {
                            log2_numvector_size = 6;
                        }
                        if sp_samples_per_channel > 512 {
                            log2_numvector_size = 7;
                        }
                    } else {
                        sp_samples_per_channel = samples_per_frame;
                    }
                }
                _ => return Err(Error::invalid(format!("cook: unsupported cookversion 0x{cookversion:X}"))),
            }

            if samples_per_channel == 0 {
                samples_per_channel = sp_samples_per_channel;
            } else if samples_per_channel != sp_samples_per_channel {
                return Err(Error::invalid("cook: differing samples per channel in subpackets"));
            }

            if total_subbands > 53 {
                return Err(Error::invalid("cook: total_subbands > 53"));
            }
            if joint_stereo && js_subband_start > subbands {
                return Err(Error::invalid("cook: js_subband_start > subbands"));
            }
            if js_vlc_bits > 6 || (joint_stereo && js_vlc_bits < 2) {
                return Err(Error::invalid("cook: invalid js_vlc_bits"));
            }
            if subbands == 0 || subbands > 50 {
                return Err(Error::invalid("cook: invalid subbands count"));
            }

            let numvector_size = 1 << log2_numvector_size;

            let channel_coupling = if joint_stereo {
                let idx = js_vlc_bits.saturating_sub(2);
                let vlc = match idx {
                    0 => LengthVlc::from_counts(&CCPL_HUFFCOUNTS[0], &CCPL_HUFFSYMS_2, 0),
                    1 => LengthVlc::from_counts(&CCPL_HUFFCOUNTS[1], &CCPL_HUFFSYMS_3, 0),
                    2 => LengthVlc::from_counts(&CCPL_HUFFCOUNTS[2], &CCPL_HUFFSYMS_4, 0),
                    3 => LengthVlc::from_counts(&CCPL_HUFFCOUNTS[3], &CCPL_HUFFSYMS_5, 0),
                    4 => LengthVlc::from_counts(&CCPL_HUFFCOUNTS[4], &CCPL_HUFFSYMS_6, 0),
                    _ => return Err(Error::invalid("cook: bad js_vlc_bits")),
                }
                .map_err(|e| Error::invalid(format!("cook: coupling vlc: {e}")))?;
                Some(vlc)
            } else {
                None
            };

            subpackets.push(COOKSubpacket {
                ch_idx: total_channels,
                size: 0,
                num_channels,
                cookversion,
                subbands,
                js_subband_start,
                js_vlc_bits,
                samples_per_channel: sp_samples_per_channel,
                log2_numvector_size,
                channel_mask: sp_channel_mask,
                channel_coupling,
                joint_stereo,
                bits_per_subpacket,
                bits_per_subpdiv,
                total_subbands,
                numvector_size,
                mono_previous_buffer1: vec![0f32; sp_samples_per_channel],
                mono_previous_buffer2: vec![0f32; sp_samples_per_channel],
                gains1_now: [0; 9],
                gains1_previous: [0; 9],
                gains2_now: [0; 9],
                gains2_previous: [0; 9],
            });

            total_channels += num_channels;
            if total_channels > channels {
                return Err(Error::invalid("cook: subpacket channels exceed container channels"));
            }
        }
        if channel_mask != 0 && channel_mask.count_ones() as usize != total_channels {
            return Err(Error::invalid("cook: channel mask does not match subpacket channels"));
        }

        if samples_per_channel != 256 && samples_per_channel != 512 && samples_per_channel != 1024 {
            return Err(Error::invalid(format!("cook: unsupported samples_per_channel {samples_per_channel}")));
        }

        let engine = Arc::new(CookEngine::new(samples_per_channel)?);
        let sample_rate = params.sample_rate.unwrap_or(44100);

        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels,
            sample_rate,
            block_align,
            samples_per_channel,
            discarded_packets: 0,
            random_state: AvLfg::new(),
            engine,
            subpackets,
        })
    }

    pub fn decode_packet(&mut self, packet: &Packet) -> CoreResult<Option<Frame>> {
        let buf = &packet.data;
        if self.block_align == 0 {
            self.block_align = buf.len();
        }
        if buf.len() < self.block_align {
            return Ok(None);
        }

        self.subpackets[0].size = self.block_align;
        for i in 1..self.subpackets.len() {
            let idx = self.block_align - self.subpackets.len() + i;
            let sz = 2 * buf[idx] as usize;
            if self.subpackets[0].size < sz + 1 {
                return Err(Error::invalid("cook: subpacket size total exceeds block_align"));
            }
            self.subpackets[0].size -= sz + 1;
            self.subpackets[i].size = sz;
        }

        let produce_samples = self.discarded_packets >= 2;
        let mut planes = if produce_samples {
            Some(vec![vec![0f32; self.samples_per_channel]; self.channels])
        } else {
            None
        };

        let mut offset = 0;
        for i in 0..self.subpackets.len() {
            let sz = self.subpackets[i].size;
            let bits = (sz * 8) >> self.subpackets[i].bits_per_subpdiv;
            self.subpackets[i].bits_per_subpacket = bits;

            decode_subpacket(
                &self.engine,
                &mut self.subpackets[i],
                &mut self.random_state,
                &buf[offset..offset + sz],
                planes.as_deref_mut(),
            )?;
            offset += sz;
        }

        if self.discarded_packets < 2 {
            self.discarded_packets += 1;
            return Ok(None);
        }

        let planes = planes.unwrap();
        let mut data = Vec::with_capacity(self.channels);
        for plane in planes {
            let mut b = Vec::with_capacity(plane.len() * 4);
            for s in plane {
                b.extend_from_slice(&s.to_le_bytes());
            }
            data.push(b);
        }

        Ok(Some(Frame::Audio(AudioFrame {
            samples: self.samples_per_channel as u32,
            pts: packet.pts,
            data,
        })))
    }
}

fn decode_bytes(input: &[u8], bytes: usize) -> Vec<u8> {
    const XOR_KEY: [u8; 4] = [0x37, 0xc5, 0x11, 0xf2];
    let len = bytes.min(input.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        out.push(input[i] ^ XOR_KEY[i & 3]);
    }
    out
}

fn decode_gain_info(reader: &mut BitReaderBe<'_>, gaininfo: &mut [i32; 9]) {
    let mut n = reader.read_unary(0, reader.bits_left()).unwrap_or(0);
    let mut i = 0;
    while n > 0 {
        n -= 1;
        let index = reader.read_bits(3).unwrap_or(0) as usize;
        let gain = if reader.read_bit().unwrap_or(0) != 0 {
            reader.read_bits(4).unwrap_or(0) as i32 - 7
        } else {
            -1
        };
        while i <= index && i <= 8 {
            gaininfo[i] = gain;
            i += 1;
        }
    }
    while i <= 8 {
        gaininfo[i] = 0;
        i += 1;
    }
}

fn decode_envelope(
    reader: &mut BitReaderBe<'_>,
    subpacket: &COOKSubpacket,
    envelope_vlcs: &[LengthVlc],
    quant_index_table: &mut [i32],
) -> CoreResult<()> {
    quant_index_table[0] = reader.read_bits(6).ok_or_else(|| Error::invalid("cook: truncated envelope"))? as i32 - 6;
    for i in 1..subpacket.total_subbands {
        let mut vlc_index = i;
        if i >= subpacket.js_subband_start * 2 {
            vlc_index -= subpacket.js_subband_start;
        } else {
            vlc_index /= 2;
            if vlc_index < 1 {
                vlc_index = 1;
            }
        }
        if vlc_index > 13 {
            vlc_index = 13;
        }
        let j = envelope_vlcs[vlc_index - 1]
            .decode_signed(&mut || reader.read_bit())
            .ok_or_else(|| Error::invalid("cook: invalid envelope VLC code"))?;
        quant_index_table[i] = quant_index_table[i - 1] + j;
        if quant_index_table[i] > 63 || quant_index_table[i] < -63 {
            return Err(Error::invalid("cook: quantizer outside [-63, 63] range"));
        }
    }
    Ok(())
}

fn categorize(
    reader: &BitReaderBe<'_>,
    samples_per_channel: usize,
    subpacket: &COOKSubpacket,
    quant_index_table: &[i32],
    category: &mut [i32],
    category_index: &mut [i32],
) {
    let mut bits_left = subpacket.bits_per_subpacket as i32 - reader.bit_position() as i32;
    if bits_left > samples_per_channel as i32 {
        bits_left = samples_per_channel as i32 + ((bits_left - samples_per_channel as i32) * 5) / 8;
    }
    let mut bias = -32i32;
    let mut i = 32i32;
    while i > 0 {
        let mut num_bits = 0;
        let mut index = 0;
        for _ in (0..subpacket.total_subbands).rev() {
            let exp_idx = ((i - quant_index_table[index] + bias) / 2).clamp(0, 7) as usize;
            index += 1;
            num_bits += EXPBITS_TAB[exp_idx];
        }
        if num_bits >= bits_left - 32 {
            bias += i;
        }
        i /= 2;
    }

    let mut exp_index1 = [0i32; 102];
    let mut exp_index2 = [0i32; 102];
    let mut num_bits = 0;
    for i in 0..subpacket.total_subbands {
        let exp_idx = ((bias - quant_index_table[i]) / 2).clamp(0, 7) as usize;
        num_bits += EXPBITS_TAB[exp_idx];
        exp_index1[i] = exp_idx as i32;
        exp_index2[i] = exp_idx as i32;
    }
    let mut tmpbias1 = num_bits;
    let mut tmpbias2 = num_bits;

    let mut tmp_categorize_array = [0i32; 256];
    let mut tmp1_idx = subpacket.numvector_size;
    let mut tmp2_idx = subpacket.numvector_size;

    for _ in 1..subpacket.numvector_size {
        if tmpbias1 + tmpbias2 > 2 * bits_left {
            let mut max = -999999i32;
            let mut index = -1i32;
            for i in 0..subpacket.total_subbands {
                if exp_index1[i] < 7 {
                    let v = -2 * exp_index1[i] - quant_index_table[i] + bias;
                    if v >= max {
                        max = v;
                        index = i as i32;
                    }
                }
            }
            if index == -1 {
                break;
            }
            let idx = index as usize;
            tmp_categorize_array[tmp1_idx] = index;
            tmp1_idx += 1;
            tmpbias1 -= EXPBITS_TAB[exp_index1[idx] as usize]
                - EXPBITS_TAB[exp_index1[idx] as usize + 1];
            exp_index1[idx] += 1;
        } else {
            let mut min = 999999i32;
            let mut index = -1i32;
            for i in 0..subpacket.total_subbands {
                if exp_index2[i] > 0 {
                    let v = -2 * exp_index2[i] - quant_index_table[i] + bias;
                    if v < min {
                        min = v;
                        index = i as i32;
                    }
                }
            }
            if index == -1 {
                break;
            }
            let idx = index as usize;
            tmp2_idx -= 1;
            tmp_categorize_array[tmp2_idx] = index;
            tmpbias2 -= EXPBITS_TAB[exp_index2[idx] as usize]
                - EXPBITS_TAB[exp_index2[idx] as usize - 1];
            exp_index2[idx] -= 1;
        }
    }

    for i in 0..subpacket.total_subbands {
        category[i] = exp_index2[i];
    }
    for i in 0..subpacket.numvector_size.saturating_sub(1) {
        category_index[i] = tmp_categorize_array[tmp2_idx];
        tmp2_idx += 1;
    }
}

fn expand_category(num_vectors: usize, category: &mut [i32], category_index: &[i32]) {
    for i in 0..num_vectors {
        let idx = category_index[i] as usize;
        category[idx] += 1;
        if category[idx] >= DITHER_TAB.len() as i32 {
            category[idx] -= 1;
        }
    }
}

fn unpack_sqvh(
    reader: &mut BitReaderBe<'_>,
    subpacket: &COOKSubpacket,
    sqvh_vlcs: &[LengthVlc],
    category: usize,
    subband_coef_index: &mut [i32; 20],
    subband_coef_sign: &mut [i32; 20],
) -> bool {
    let vd = VD_TAB[category];
    let mut result = false;
    for i in 0..VPR_TAB[category] {
        let mut vlc = sqvh_vlcs[category]
            .decode(&mut || reader.read_bit())
            .unwrap_or(0) as i32;
        if subpacket.bits_per_subpacket < reader.bit_position() {
            vlc = 0;
            result = true;
        }
        for j in (0..vd).rev() {
            let tmp = ((vlc as i64 * INVRADIX_TAB[category] as i64) / 0x100000) as i32;
            subband_coef_index[vd * i + j] = vlc - tmp * (KMAX_TAB[category] + 1);
            vlc = tmp;
        }
        for j in 0..vd {
            let idx = i * vd + j;
            if subband_coef_index[idx] != 0 {
                if reader.bit_position() < subpacket.bits_per_subpacket {
                    subband_coef_sign[idx] = reader.read_bit().unwrap_or(0) as i32;
                } else {
                    result = true;
                    subband_coef_sign[idx] = 0;
                }
            } else {
                subband_coef_sign[idx] = 0;
            }
        }
    }
    result
}

fn scalar_dequant_float(
    prng: &mut AvLfg,
    index: usize,
    quant_index: i32,
    subband_coef_index: &[i32; 20],
    subband_coef_sign: &[i32; 20],
    mlt_p: &mut [f32],
) {
    let scale = ROOTPOW2TAB[(quant_index + 63).clamp(0, 126) as usize];
    for i in 0..SUBBAND_SIZE {
        let f1 = if subband_coef_index[i] != 0 {
            let c = QUANT_CENTROID_TAB[index][subband_coef_index[i] as usize];
            if subband_coef_sign[i] != 0 { -c } else { c }
        } else {
            let d = DITHER_TAB[index];
            if prng.get() < 0x8000_0000 { -d } else { d }
        };
        mlt_p[i] = f1 * scale;
    }
}

fn decode_vectors(
    reader: &mut BitReaderBe<'_>,
    subpacket: &COOKSubpacket,
    sqvh_vlcs: &[LengthVlc],
    prng: &mut AvLfg,
    category: &mut [i32],
    quant_index_table: &[i32],
    mlt_buffer: &mut [f32],
) {
    let mut subband_coef_index = [0i32; 20];
    let mut subband_coef_sign = [0i32; 20];

    for band in 0..subpacket.total_subbands {
        let mut index = category[band] as usize;
        if index < 7 {
            if unpack_sqvh(reader, subpacket, sqvh_vlcs, index, &mut subband_coef_index, &mut subband_coef_sign) {
                index = 7;
                for j in 0..subpacket.total_subbands {
                    category[band + j] = 7;
                }
            }
        }
        if index >= 7 {
            subband_coef_index = [0; 20];
            subband_coef_sign = [0; 20];
        }
        scalar_dequant_float(
            prng,
            index,
            quant_index_table[band],
            &subband_coef_index,
            &subband_coef_sign,
            &mut mlt_buffer[band * SUBBAND_SIZE..(band + 1) * SUBBAND_SIZE],
        );
    }
}

fn mono_decode(
    reader: &mut BitReaderBe<'_>,
    samples_per_channel: usize,
    subpacket: &COOKSubpacket,
    envelope_vlcs: &[LengthVlc],
    sqvh_vlcs: &[LengthVlc],
    prng: &mut AvLfg,
    mlt_buffer: &mut [f32],
) -> CoreResult<()> {
    let mut category_index = [0i32; 128];
    let mut category = [0i32; 128];
    let mut quant_index_table = [0i32; 102];

    decode_envelope(reader, subpacket, envelope_vlcs, &mut quant_index_table)?;
    let num_vectors = reader.read_bits(subpacket.log2_numvector_size).unwrap_or(0) as usize;
    categorize(reader, samples_per_channel, subpacket, &quant_index_table, &mut category, &mut category_index);
    expand_category(num_vectors, &mut category, &category_index);
    for i in 0..subpacket.total_subbands {
        if category[i] > 7 {
            return Err(Error::invalid("cook: category out of range"));
        }
    }
    decode_vectors(reader, subpacket, sqvh_vlcs, prng, &mut category, &quant_index_table, mlt_buffer);
    Ok(())
}

fn decouple_info(
    reader: &mut BitReaderBe<'_>,
    subpacket: &COOKSubpacket,
    coupling_vlc: Option<&LengthVlc>,
    decouple_tab: &mut [usize; 20],
) -> CoreResult<()> {
    let vlc = reader.read_bit().ok_or_else(|| Error::invalid("cook: truncated decouple"))? != 0;
    let start = CPLBAND[subpacket.js_subband_start];
    let end = CPLBAND[subpacket.subbands - 1];
    if start > end {
        return Ok(());
    }
    let length = end - start + 1;
    if vlc {
        let vlc_tab = coupling_vlc.ok_or_else(|| Error::invalid("cook: missing coupling VLC"))?;
        for i in 0..length {
            let val = vlc_tab.decode(&mut || reader.read_bit())
                .ok_or_else(|| Error::invalid("cook: invalid coupling VLC code"))?;
            decouple_tab[start + i] = val as usize;
        }
    } else {
        for i in 0..length {
            let v = reader.read_bits(subpacket.js_vlc_bits)
                .ok_or_else(|| Error::invalid("cook: truncated decouple bits"))? as usize;
            if v == (1 << subpacket.js_vlc_bits) - 1 {
                return Err(Error::invalid("cook: decouple value too large"));
            }
            decouple_tab[start + i] = v;
        }
    }
    Ok(())
}

fn joint_decode(
    reader: &mut BitReaderBe<'_>,
    samples_per_channel: usize,
    subpacket: &COOKSubpacket,
    envelope_vlcs: &[LengthVlc],
    sqvh_vlcs: &[LengthVlc],
    coupling_vlc: Option<&LengthVlc>,
    cplscales: [&'static [f32]; 5],
    prng: &mut AvLfg,
    mlt_buffer_left: &mut [f32],
    mlt_buffer_right: &mut [f32],
) -> CoreResult<()> {
    let mut decouple_tab = [0usize; 20];
    let mut decode_buffer = vec![0f32; 1060];

    mlt_buffer_left.fill(0.0);
    mlt_buffer_right.fill(0.0);

    decouple_info(reader, subpacket, coupling_vlc, &mut decouple_tab)?;
    mono_decode(reader, samples_per_channel, subpacket, envelope_vlcs, sqvh_vlcs, prng, &mut decode_buffer)?;

    for i in 0..subpacket.js_subband_start {
        for j in 0..SUBBAND_SIZE {
            mlt_buffer_left[i * SUBBAND_SIZE + j] = decode_buffer[i * 40 + j];
            mlt_buffer_right[i * SUBBAND_SIZE + j] = decode_buffer[i * 40 + SUBBAND_SIZE + j];
        }
    }

    let mut idx = (1 << subpacket.js_vlc_bits) - 1;
    for i in subpacket.js_subband_start..subpacket.subbands {
        let cpl_tmp = CPLBAND[i];
        idx -= decouple_tab[cpl_tmp];
        let cplscale = cplscales[subpacket.js_vlc_bits - 2];
        let f1 = cplscale[decouple_tab[cpl_tmp] + 1];
        let f2 = cplscale[idx];
        for j in 0..SUBBAND_SIZE {
            let tmp_idx = (subpacket.js_subband_start + i) * SUBBAND_SIZE + j;
            mlt_buffer_left[SUBBAND_SIZE * i + j] = f1 * decode_buffer[tmp_idx];
            mlt_buffer_right[SUBBAND_SIZE * i + j] = f2 * decode_buffer[tmp_idx];
        }
        idx = (1 << subpacket.js_vlc_bits) - 1;
    }
    Ok(())
}

fn imlt_window_float(
    samples_per_channel: usize,
    mlt_window: &[f32],
    buffer1: &mut [f32],
    gains_previous_0: i32,
    previous_buffer: &[f32],
) {
    let fc = POW2TAB[(gains_previous_0 + 63).clamp(0, 126) as usize];
    for i in 0..samples_per_channel {
        buffer1[i] = buffer1[i] * fc * mlt_window[i]
            - previous_buffer[i] * mlt_window[samples_per_channel - 1 - i];
    }
}

fn interpolate_float(
    gain_size_factor: usize,
    gain_table: &[f32; 31],
    buffer: &mut [f32],
    gain_index: i32,
    gain_index_next: i32,
) {
    let mut fc1 = POW2TAB[(gain_index + 63).clamp(0, 126) as usize];
    if gain_index == gain_index_next {
        for i in 0..gain_size_factor {
            buffer[i] *= fc1;
        }
    } else {
        let fc2 = gain_table[(15 + (gain_index_next - gain_index)).clamp(0, 30) as usize];
        for i in 0..gain_size_factor {
            buffer[i] *= fc1;
            fc1 *= fc2;
        }
    }
}

fn imlt_gain(
    engine: &CookEngine,
    inbuffer: &[f32],
    gains_now: &[i32; 9],
    gains_previous: &[i32; 9],
    previous_buffer: &mut [f32],
    mut out: Option<&mut [f32]>,
) {
    let mut mono_mdct_output = vec![0f32; engine.samples_per_channel * 2];
    engine.run_immdct_full(inbuffer, &mut mono_mdct_output);

    let (buffer0, buffer1) = mono_mdct_output.split_at_mut(engine.samples_per_channel);
    imlt_window_float(
        engine.samples_per_channel,
        &engine.mlt_window,
        buffer1,
        gains_previous[0],
        previous_buffer,
    );

    for i in 0..8 {
        if gains_now[i] != 0 || gains_now[i + 1] != 0 {
            interpolate_float(
                engine.gain_size_factor,
                &engine.gain_table,
                &mut buffer1[engine.gain_size_factor * i..],
                gains_now[i],
                gains_now[i + 1],
            );
        }
    }

    previous_buffer.copy_from_slice(buffer0);

    if let Some(out_buf) = &mut out {
        for (dst, &src) in out_buf.iter_mut().zip(buffer1.iter()) {
            *dst = src.clamp(-1.0, 1.0);
        }
    }
}

fn decode_subpacket(
    engine: &CookEngine,
    subpacket: &mut COOKSubpacket,
    prng: &mut AvLfg,
    inbuffer: &[u8],
    mut out_planes: Option<&mut [Vec<f32>]>,
) -> CoreResult<()> {
    let sub_packet_size = subpacket.size;
    let joint_stereo = subpacket.joint_stereo;
    let num_channels = subpacket.num_channels;
    let ch_idx = subpacket.ch_idx;
    let bits_per_subpacket = subpacket.bits_per_subpacket;

    let mut decode_buffer_1 = vec![0f32; 1024];
    let mut decode_buffer_2 = vec![0f32; 1024];

    // Decode bytes and gain for channel 1
    let decoded_bytes1 = decode_bytes(inbuffer, bits_per_subpacket / 8);
    let mut reader1 = BitReaderBe::with_bits(&decoded_bytes1, bits_per_subpacket);
    decode_gain_info(&mut reader1, &mut subpacket.gains1_now);
    std::mem::swap(&mut subpacket.gains1_now, &mut subpacket.gains1_previous);

    if joint_stereo {
        joint_decode(
            &mut reader1,
            engine.samples_per_channel,
            subpacket,
            &engine.envelope_quant_index,
            &engine.sqvh,
            subpacket.channel_coupling.as_ref(),
            engine.cplscales,
            prng,
            &mut decode_buffer_1,
            &mut decode_buffer_2,
        )?;
    } else {
        mono_decode(
            &mut reader1,
            engine.samples_per_channel,
            subpacket,
            &engine.envelope_quant_index,
            &engine.sqvh,
            prng,
            &mut decode_buffer_1,
        )?;

        if num_channels == 2 {
            let offset2 = sub_packet_size / 2;
            if offset2 < inbuffer.len() {
                let decoded_bytes2 = decode_bytes(&inbuffer[offset2..], bits_per_subpacket / 8);
                let mut reader2 = BitReaderBe::with_bits(&decoded_bytes2, bits_per_subpacket);
                decode_gain_info(&mut reader2, &mut subpacket.gains2_now);
                std::mem::swap(&mut subpacket.gains2_now, &mut subpacket.gains2_previous);
                mono_decode(
                    &mut reader2,
                    engine.samples_per_channel,
                    subpacket,
                    &engine.envelope_quant_index,
                    &engine.sqvh,
                    prng,
                    &mut decode_buffer_2,
                )?;
            }
        }
    }

    let mut out1 = vec![0f32; engine.samples_per_channel];
    let mut out2 = vec![0f32; engine.samples_per_channel];

    let has_out = out_planes.is_some();
    imlt_gain(
        engine,
        &decode_buffer_1,
        &subpacket.gains1_now,
        &subpacket.gains1_previous,
        &mut subpacket.mono_previous_buffer1,
        if has_out { Some(&mut out1) } else { None },
    );

    if num_channels == 2 {
        if joint_stereo {
            imlt_gain(
                engine,
                &decode_buffer_2,
                &subpacket.gains1_now,
                &subpacket.gains1_previous,
                &mut subpacket.mono_previous_buffer2,
                if has_out { Some(&mut out2) } else { None },
            );
        } else {
            imlt_gain(
                engine,
                &decode_buffer_2,
                &subpacket.gains2_now,
                &subpacket.gains2_previous,
                &mut subpacket.mono_previous_buffer2,
                if has_out { Some(&mut out2) } else { None },
            );
        }
    }

    if let Some(planes) = &mut out_planes {
        planes[ch_idx] = out1;
        if num_channels == 2 {
            planes[ch_idx + 1] = out2;
        }
    }

    Ok(())
}

pub struct CookDecoderWrapper {
    inner: CookDecoder,
    pending_frames: Vec<Frame>,
}

impl Decoder for CookDecoderWrapper {
    fn codec_id(&self) -> &CodecId {
        &self.inner.codec_id
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat {
            sample_format: SampleFormat::F32P,
            sample_rate: self.inner.sample_rate,
            channels: self.inner.channels as u16,
        })
    }

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        if let Some(frame) = self.inner.decode_packet(packet)? {
            self.pending_frames.push(frame);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        if self.pending_frames.is_empty() {
            Err(Error::NeedMore)
        } else {
            Ok(self.pending_frames.remove(0))
        }
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.pending_frames.clear();
        self.inner.discarded_packets = 0;
        self.inner.random_state = AvLfg::new();
        for sp in &mut self.inner.subpackets {
            sp.mono_previous_buffer1.fill(0.0);
            sp.mono_previous_buffer2.fill(0.0);
            sp.gains1_now = [0; 9];
            sp.gains1_previous = [0; 9];
            sp.gains2_now = [0; 9];
            sp.gains2_previous = [0; 9];
        }
        Ok(())
    }
}

pub fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    let inner = CookDecoder::new(params)?;
    Ok(Box::new(CookDecoderWrapper {
        inner,
        pending_frames: Vec::new(),
    }))
}

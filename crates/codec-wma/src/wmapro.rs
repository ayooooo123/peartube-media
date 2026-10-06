// Ported from FFmpeg (commit 2da55bf): libavcodec/wmaprodec.c, wmaprodata.h,
// plus wma_common.c (frame-length helper) and get_bits.h semantics.
// GNU Lesser General Public License 2.1 or later

//! WMA Pro (Windows Media Audio 9 Professional) decoder.

use crate::bits::BitReader;
use crate::dsp::vector_fmul_window;
use crate::fft::Imdct;
use crate::dsp::sine_window;
use crate::tables::CRITICAL_FREQS;
use crate::wma_common::wma_get_frame_len_bits;
use crate::wmapro_tables::*;
use crate::vlc::VlcTable;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

pub const WMAPRO_MAX_CHANNELS: usize = 8;
pub const MAX_SUBFRAMES: usize = 32;
pub const MAX_BANDS: usize = 29;
pub const MAX_FRAMESIZE: usize = 32768;
pub const WMAPRO_BLOCK_MIN_BITS: u32 = 6;
pub const WMAPRO_BLOCK_MAX_BITS: u32 = 13;
pub const WMAPRO_BLOCK_SIZES: usize = (WMAPRO_BLOCK_MAX_BITS - WMAPRO_BLOCK_MIN_BITS + 1) as usize;
pub const VLCBITS: usize = 9;
pub const SCALEVLCBITS: usize = 8;



pub const HUFF_SCALE_MAXBITS: u32 = 19;
pub const HUFF_SCALE_RL_MAXBITS: u32 = 21;
pub const HUFF_COEF0_MAXBITS: u32 = 21;
pub const HUFF_COEF1_MAXBITS: u32 = 22;
pub const HUFF_VEC4_MAXBITS: u32 = 14;
pub const HUFF_VEC2_MAXBITS: u32 = 12;
pub const HUFF_VEC1_MAXBITS: u32 = 11;

/// Build a VLC from (symbol, length) pairs (FFmpeg's
/// `VLC_INIT_FROM_LENGTHS`): codes are assigned in table order, MSB-first,
/// so the table order must be kept as is.
fn vlc_from_pairs(pairs: &[(u8, u8)], symbols_offset: i32) -> Result<VlcTable> {
    let lengths: Vec<i8> = pairs.iter().map(|&(_, l)| l as i8).collect();
    let symbols: Vec<i32> = pairs.iter().map(|&(s, _)| s as i32).collect();
    VlcTable::from_lengths(&lengths, Some(&symbols), symbols_offset)
}

fn vlc_from_lens_syms(lens: &[u8], syms: &[u16], symbols_offset: i32) -> Result<VlcTable> {
    let lengths: Vec<i8> = lens.iter().map(|&l| l as i8).collect();
    let symbols: Vec<i32> = syms.iter().map(|&s| s as i32).collect();
    VlcTable::from_lengths(&lengths, Some(&symbols), symbols_offset)
}

#[derive(Clone)]
struct ChannelCtx {
    prev_block_len: usize,
    transmit_coefs: bool,
    num_subframes: usize,
    subframe_len: [usize; MAX_SUBFRAMES],
    subframe_offset: [usize; MAX_SUBFRAMES],
    cur_subframe: usize,
    decoded_samples: usize,
    grouped: bool,
    quant_step: i32,
    reuse_sf: bool,
    scale_factor_step: i32,
    max_scale_factor: i32,
    saved_scale_factors: [[i32; MAX_BANDS]; 2],
    scale_factor_idx: usize,
    /// index into `saved_scale_factors` used as the decoding source
    table_idx: usize,
    num_vec_coeffs: usize,
    /// absolute position in `out[]` where the current subframe's coeffs start
    cur_out_pos: usize,
    coeffs: Vec<f32>,
    out: Vec<f32>,
}

impl ChannelCtx {
    fn new(samples_per_frame: usize) -> Self {
        let max = 1 << WMAPRO_BLOCK_MAX_BITS;
        Self {
            prev_block_len: samples_per_frame,
            coeffs: vec![0.0; max],
            out: vec![0.0; max + max / 2],
            ..Default::default()
        }
    }
}

impl Default for ChannelCtx {
    fn default() -> Self {
        let max = 1 << WMAPRO_BLOCK_MAX_BITS;
        Self {
            prev_block_len: 0,
            transmit_coefs: false,
            num_subframes: 0,
            subframe_len: [0; MAX_SUBFRAMES],
            subframe_offset: [0; MAX_SUBFRAMES],
            cur_subframe: 0,
            decoded_samples: 0,
            grouped: false,
            quant_step: 0,
            reuse_sf: false,
            scale_factor_step: 0,
            max_scale_factor: 0,
            saved_scale_factors: [[0; MAX_BANDS]; 2],
            scale_factor_idx: 0,
            table_idx: 0,
            num_vec_coeffs: 0,
            cur_out_pos: 0,
            coeffs: vec![0.0; max],
            out: vec![0.0; max + max / 2],
        }
    }
}

#[derive(Clone, Default)]
struct ChannelGrp {
    num_channels: usize,
    transform: bool,
    transform_band: [bool; MAX_BANDS],
    decorrelation_matrix: Vec<f32>,
    /// indexes (into `channel[]`) of the group's channels
    channel_data: Vec<usize>,
}

/// Whole-decoder state (`WMAProDecodeCtx`).
pub struct WmaProDecoder {
    codec_id: CodecId,
    channels: usize,
    sample_rate: u32,
    block_align_len: usize,

    #[allow(dead_code)]
    decode_flags: u32,
    len_prefix: bool,
    dynamic_range_compression: bool,
    bits_per_sample: u32,
    samples_per_frame: usize,
    log2_frame_size: u32,
    lfe_channel: i32,
    max_num_subframes: usize,
    subframe_len_bits: u32,
    max_subframe_len_bit: bool,
    min_samples_per_subframe: usize,
    num_sfb: [i32; WMAPRO_BLOCK_SIZES],
    sfb_offsets: [[i32; MAX_BANDS]; WMAPRO_BLOCK_SIZES],
    sf_offsets: [[[i8; MAX_BANDS]; WMAPRO_BLOCK_SIZES]; WMAPRO_BLOCK_SIZES],
    subwoofer_cutoffs: [i32; WMAPRO_BLOCK_SIZES],
    windows: [Vec<f32>; WMAPRO_BLOCK_SIZES],
    mdcts: Vec<Imdct>,
    tmp: Vec<f32>,

    sf_vlc: VlcTable,
    sf_rl_vlc: VlcTable,
    coef_vlc: [VlcTable; 2],
    vec4_vlc: VlcTable,
    #[allow(dead_code)]
    vec2_vlc: VlcTable,
    vec1_vlc: VlcTable,
    sin64: [f32; 33],

    // packet state
    frame_data: Vec<u8>,
    num_saved_bits: usize,
    frame_offset: usize,
    subframe_offset: usize,
    packet_loss: bool,
    packet_done: bool,
    eof_done: bool,
    skip_frame: bool,
    packet_sequence_number: u8,
    packet_offset: usize,
    next_packet_start: usize,
    trim_start: usize,
    trim_end: usize,

    // frame state
    /// bit reservoir over the assembled frame (`s->gb` in FFmpeg)
    gb: crate::bits::OwnedBitReader,
    buf_bit_size: usize,
    subframe_len: usize,
    channels_for_cur_subframe: usize,
    channel_indexes_for_cur_subframe: [usize; WMAPRO_MAX_CHANNELS],
    num_bands: usize,
    cur_sfb_offsets: Vec<i32>,
    table_idx: usize,
    esc_len: u32,
    transmit_num_vec_coeffs: bool,
    num_chgroups: usize,
    chgroup: Vec<ChannelGrp>,
    channel: Vec<ChannelCtx>,
    parsed_all_subframes: bool,
    pending: Vec<AudioFrame>,
    drc_gain: u8,
}

fn default_decorrelation(num_channels: usize) -> &'static [f32] {
    const OFFS: [usize; 7] = [usize::MAX, 0, 1, 5, 14, 30, 55];
    let start = OFFS[num_channels.min(6)];
    if start == usize::MAX || num_channels == 0 {
        return &[];
    }
    &DEFAULT_DECORRELATION_MATRICES[start..start + num_channels * num_channels]
}

impl WmaProDecoder {
    /// `decode_init` (wmaprodec.c), WMAPRO flavor only.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = params.channels.unwrap_or(0) as usize;
        let sample_rate = params.sample_rate.unwrap_or(0);
        // block_align travels through the codec options bag ("block_align"),
        // mirroring FFmpeg's required AVCodecContext::block_align.
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&b| b > 0)
            .ok_or_else(|| Error::invalid("wmapro: block_align is not set"))? as usize;
        let extradata = &params.extradata;
        if extradata.len() < 18 {
            return Err(Error::unsupported("wmapro: extradata too small"));
        }
        let rd16 = |p: usize| u16::from_le_bytes([extradata[p], extradata[p + 1]]) as u32;
        let rd32 =
            |p: usize| u32::from_le_bytes([extradata[p], extradata[p + 1], extradata[p + 2], extradata[p + 3]]);
        let decode_flags = rd16(14);
        let channel_mask = rd32(2);
        let bits_per_sample = rd16(0);
        let nb_channels = if channel_mask != 0 { channel_mask.count_ones() as usize } else { channels };
        if bits_per_sample > 32 || bits_per_sample < 1 {
            return Err(Error::unsupported(format!("wmapro: bits per sample is {bits_per_sample}")));
        }

        let log2_frame_size: u32 = (32 - (block_align as u32).leading_zeros() - 1) + 4;
        if log2_frame_size > 25 {
            return Err(Error::unsupported("wmapro: large block align"));
        }

        let len_prefix = decode_flags & 0x40 != 0;
        let bits = wma_get_frame_len_bits(sample_rate, 3, decode_flags);
        if bits > WMAPRO_BLOCK_MAX_BITS {
            return Err(Error::unsupported("wmapro: 14-bit block sizes"));
        }
        let samples_per_frame = 1usize << bits;

        let log2_max_num_subframes = ((decode_flags & 0x38) >> 3) as usize;
        let max_num_subframes = 1usize << log2_max_num_subframes;
        let max_subframe_len_bit = max_num_subframes == 16 || max_num_subframes == 4;
        // av_log2(log2_max_num_subframes) + 1, with av_log2(0) == 0.
        let subframe_len_bits = (log2_max_num_subframes as u32 | 1).ilog2() + 1;
        let num_possible_block_sizes = log2_max_num_subframes + 1;
        let min_samples_per_subframe = samples_per_frame / max_num_subframes;
        let dynamic_range_compression = decode_flags & 0x80 != 0;

        if max_num_subframes > MAX_SUBFRAMES {
            return Err(Error::invalid("wmapro: invalid number of subframes"));
        }
        if min_samples_per_subframe < (1 << WMAPRO_BLOCK_MIN_BITS) {
            return Err(Error::invalid("wmapro: min_samples_per_subframe too small"));
        }
        if nb_channels == 0 || nb_channels > WMAPRO_MAX_CHANNELS || nb_channels > channels {
            return Err(Error::unsupported("wmapro: invalid channel count"));
        }

        let mut lfe_channel: i32 = -1;
        if channel_mask & 8 != 0 {
            let mut mask = 1u32;
            while mask < 16 {
                if channel_mask & mask != 0 {
                    lfe_channel += 1;
                }
                mask <<= 1;
            }
        }

        let rate = sample_rate as i64;
        let mut num_sfb = [0i32; WMAPRO_BLOCK_SIZES];
        let mut sfb_offsets = [[0i32; MAX_BANDS]; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            let subframe_len = (samples_per_frame >> i) as i64;
            let mut band = 1usize;
            sfb_offsets[i][0] = 0;
            let mut x = 0usize;
            while x < MAX_BANDS - 1 && sfb_offsets[i][band - 1] < subframe_len as i32 {
                let offset = (subframe_len * 2 * CRITICAL_FREQS[x] as i64) / rate + 2;
                let offset = (offset & !3) as i32;
                if offset > sfb_offsets[i][band - 1] {
                    sfb_offsets[i][band] = offset;
                    band += 1;
                }
                if offset >= subframe_len as i32 {
                    break;
                }
                x += 1;
            }
            sfb_offsets[i][band - 1] = subframe_len as i32;
            num_sfb[i] = band as i32 - 1;
            if num_sfb[i] <= 0 {
                return Err(Error::invalid("wmapro: num_sfb invalid"));
            }
        }

        let mut sf_offsets = [[[0i8; MAX_BANDS]; WMAPRO_BLOCK_SIZES]; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            for b in 0..num_sfb[i] as usize {
                let offset = ((sfb_offsets[i][b] + sfb_offsets[i][b + 1] - 1) << i) >> 1;
                for x in 0..num_possible_block_sizes {
                    let mut v = 0usize;
                    while (sfb_offsets[x][v + 1] << x) < offset {
                        v += 1;
                        if v >= MAX_BANDS {
                            return Err(Error::invalid("wmapro: sfb table overflow"));
                        }
                    }
                    sf_offsets[i][x][b] = v as i8;
                }
            }
        }

        let mut subwoofer_cutoffs = [0i32; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            let block_size = (samples_per_frame >> i) as i64;
            let cutoff = (440 * block_size + 3 * (sample_rate as i64 >> 1) - 1) / sample_rate as i64;
            subwoofer_cutoffs[i] = cutoff.clamp(4, block_size) as i32;
        }

        let mut windows: [Vec<f32>; WMAPRO_BLOCK_SIZES] = Default::default();
        let mut mdcts = Vec::with_capacity(WMAPRO_BLOCK_SIZES);
        for i in 0..WMAPRO_BLOCK_SIZES {
            let win_idx = WMAPRO_BLOCK_MAX_BITS as usize - i;
            windows[WMAPRO_BLOCK_SIZES - i - 1] = sine_window(1 << win_idx);
            let mdct_len = 1usize << (WMAPRO_BLOCK_MIN_BITS + i as u32);
            let scale = 1.0f32
                / (1u32 << (WMAPRO_BLOCK_MIN_BITS + i as u32 - 1)) as f32
                / (1u32 << (bits_per_sample - 1)) as f32;
            mdcts.push(Imdct::new(mdct_len, scale as f64));
        }

        let sf_vlc = vlc_from_pairs(&SCALE_TABLE, -60)?;
        let sf_rl_vlc = vlc_from_pairs(&SCALE_RL_TABLE, 0)?;
        let coef0 = vlc_from_lens_syms(&COEF0_LENS, &COEF0_SYMS, 0)?;
        let coef1 = vlc_from_pairs(&COEF1_TABLE, 0)?;
        let vec4 = vlc_from_lens_syms(&VEC4_LENS, &VEC4_SYMS, -1)?;
        let vec2 = vlc_from_pairs(&VEC2_TABLE, -1)?;
        let vec1 = vlc_from_pairs(&VEC1_TABLE, 0)?;

        let mut sin64 = [0f32; 33];
        for (i, s) in sin64.iter_mut().enumerate() {
            *s = (i as f64 * std::f64::consts::PI / 64.0).sin() as f32;
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels: nb_channels,
            sample_rate,
            block_align_len: block_align,
            decode_flags,
            len_prefix,
            dynamic_range_compression,
            bits_per_sample,
            samples_per_frame,
            log2_frame_size,
            lfe_channel,
            max_num_subframes,
            subframe_len_bits,
            max_subframe_len_bit,
            min_samples_per_subframe,
            num_sfb,
            sfb_offsets,
            sf_offsets,
            subwoofer_cutoffs,
            windows,
            mdcts,
            tmp: vec![0.0; 1 << WMAPRO_BLOCK_MAX_BITS],
            sf_vlc,
            sf_rl_vlc,
            coef_vlc: [coef0, coef1],
            vec4_vlc: vec4,
            vec2_vlc: vec2,
            vec1_vlc: vec1,
            sin64,
            frame_data: vec![0u8; MAX_FRAMESIZE + 64],
            num_saved_bits: 0,
            frame_offset: 0,
            subframe_offset: 0,
            packet_loss: true,
            packet_done: false,
            eof_done: false,
            skip_frame: true,
            packet_sequence_number: 0,
            packet_offset: 0,
            next_packet_start: 0,
            trim_start: 0,
            trim_end: 0,
            gb: crate::bits::OwnedBitReader::new(),
            buf_bit_size: 0,
            subframe_len: 0,
            channels_for_cur_subframe: 0,
            channel_indexes_for_cur_subframe: [0; WMAPRO_MAX_CHANNELS],
            num_bands: 0,
            cur_sfb_offsets: Vec::new(),
            table_idx: 0,
            esc_len: 0,
            transmit_num_vec_coeffs: false,
            num_chgroups: 0,
            chgroup: (0..WMAPRO_MAX_CHANNELS).map(|_| ChannelGrp::default()).collect(),
            channel: (0..nb_channels).map(|_| ChannelCtx::new(samples_per_frame)).collect(),
            parsed_all_subframes: false,
            pending: Vec::new(),
            drc_gain: 0,
        })
    }

    /// `decode_subframe_length` (wmaprodec.c).
    fn decode_subframe_length(&mut self, offset: usize) -> Result<usize> {
        if offset == self.samples_per_frame - self.min_samples_per_subframe {
            return Ok(self.min_samples_per_subframe);
        }
        if self.gb.bits_left() < 1 {
            return Err(Error::invalid("wmapro: no bits for subframe length"));
        }
        let frame_len_shift = if self.max_subframe_len_bit {
            if self.gb.get_bits1()? != 0 {
                1 + self.gb.get_bits(self.subframe_len_bits as usize - 1)? as usize
            } else {
                0
            }
        } else {
            self.gb.get_bits(self.subframe_len_bits as usize)? as usize
        };
        let subframe_len = self.samples_per_frame >> frame_len_shift;
        if subframe_len < self.min_samples_per_subframe || subframe_len > self.samples_per_frame {
            return Err(Error::invalid("wmapro: broken frame: subframe_len"));
        }
        Ok(subframe_len)
    }

    /// `decode_tilehdr` (wmaprodec.c).
    fn decode_tilehdr(&mut self) -> Result<()> {
        let nb_channels = self.channels;
        let mut num_samples = [0usize; WMAPRO_MAX_CHANNELS];
        let mut contains_subframe = [false; WMAPRO_MAX_CHANNELS];
        let mut channels_for_cur_subframe = nb_channels;
        let mut fixed_channel_layout = false;
        let mut min_channel_len = 0usize;

        for c in 0..nb_channels {
            self.channel[c].num_subframes = 0;
        }

        if self.max_num_subframes == 1 || self.gb.get_bits1()? != 0 {
            fixed_channel_layout = true;
        }

        loop {
            for c in 0..nb_channels {
                if num_samples[c] == min_channel_len {
                    contains_subframe[c] = fixed_channel_layout
                        || channels_for_cur_subframe == 1
                        || min_channel_len == self.samples_per_frame - self.min_samples_per_subframe;
                    if !contains_subframe[c] {
                        contains_subframe[c] = self.gb.get_bits1()? != 0;
                    }
                } else {
                    contains_subframe[c] = false;
                }
            }

            let subframe_len = self.decode_subframe_length(min_channel_len)?;
            min_channel_len += subframe_len;
            for c in 0..nb_channels {
                if contains_subframe[c] {
                    let ch = &mut self.channel[c];
                    if ch.num_subframes >= MAX_SUBFRAMES {
                        return Err(Error::invalid("wmapro: num subframes > 31"));
                    }
                    ch.subframe_len[ch.num_subframes] = subframe_len;
                    num_samples[c] += subframe_len;
                    ch.num_subframes += 1;
                    if num_samples[c] > self.samples_per_frame {
                        return Err(Error::invalid("wmapro: channel len > samples_per_frame"));
                    }
                } else if num_samples[c] <= min_channel_len {
                    if num_samples[c] < min_channel_len {
                        channels_for_cur_subframe = 0;
                        min_channel_len = num_samples[c];
                    }
                    channels_for_cur_subframe += 1;
                }
            }
            if min_channel_len >= self.samples_per_frame {
                break;
            }
        }

        for c in 0..nb_channels {
            let mut offset = 0usize;
            for i in 0..self.channel[c].num_subframes {
                self.channel[c].subframe_offset[i] = offset;
                offset += self.channel[c].subframe_len[i];
            }
        }
        Ok(())
    }

    /// `decode_decorrelation_matrix` (wmaprodec.c).
    fn decode_decorrelation_matrix(&mut self, grp: usize) -> Result<()> {
        let num_channels = self.chgroup[grp].num_channels;
        let n = self.channels;
        self.chgroup[grp].decorrelation_matrix = vec![0.0; n * n];
        let count = num_channels * (num_channels - 1) >> 1;
        let mut rotation_offset = vec![0i32; count.max(1)];
        for item in rotation_offset.iter_mut().take(count) {
            *item = self.gb.get_bits(6)? as i32;
        }
        {
            let m = &mut self.chgroup[grp].decorrelation_matrix;
            for i in 0..num_channels {
                m[num_channels * i + i] = if self.gb.get_bits1()? != 0 { 1.0 } else { -1.0 };
            }
        }
        let mut offset = 0usize;
        for i in 1..num_channels {
            for x in 0..i {
                for y in 0..i + 1 {
                    let (v1, v2) = {
                        let m = &self.chgroup[grp].decorrelation_matrix;
                        (m[x * num_channels + y], m[i * num_channels + y])
                    };
                    let nn = rotation_offset[offset + x];
                    let (sinv, cosv) = if nn < 32 {
                        (self.sin64[nn as usize], self.sin64[(32 - nn) as usize])
                    } else {
                        (self.sin64[(64 - nn) as usize], -self.sin64[(nn - 32) as usize])
                    };
                    let m = &mut self.chgroup[grp].decorrelation_matrix;
                    m[y + x * num_channels] = v1 * sinv - v2 * cosv;
                    m[y + i * num_channels] = v1 * cosv + v2 * sinv;
                }
            }
            offset += i;
        }
        Ok(())
    }

    /// `decode_channel_transform` (wmaprodec.c).
    fn decode_channel_transform(&mut self) -> Result<()> {
        self.num_chgroups = 0;
        if self.channels > 1 {
            let mut remaining_channels = self.channels_for_cur_subframe;
            if self.gb.get_bits1()? != 0 {
                return Err(Error::unsupported("wmapro: channel transform bit"));
            }
            while remaining_channels > 0 && self.num_chgroups < self.channels_for_cur_subframe {
                let grp = self.num_chgroups;
                self.chgroup[grp].num_channels = 0;
                self.chgroup[grp].transform = false;
                self.chgroup[grp].channel_data.clear();

                if remaining_channels > 2 {
                    for i in 0..self.channels_for_cur_subframe {
                        let channel_idx = self.channel_indexes_for_cur_subframe[i];
                        if !self.channel[channel_idx].grouped && self.gb.get_bits1()? != 0 {
                            self.chgroup[grp].num_channels += 1;
                            self.channel[channel_idx].grouped = true;
                            self.chgroup[grp].channel_data.push(channel_idx);
                        }
                    }
                } else {
                    self.chgroup[grp].num_channels = remaining_channels;
                    for i in 0..self.channels_for_cur_subframe {
                        let channel_idx = self.channel_indexes_for_cur_subframe[i];
                        if !self.channel[channel_idx].grouped {
                            self.chgroup[grp].channel_data.push(channel_idx);
                        }
                        self.channel[channel_idx].grouped = true;
                    }
                }

                let num_channels = self.chgroup[grp].num_channels;
                if num_channels == 2 {
                    if self.gb.get_bits1()? != 0 {
                        if self.gb.get_bits1()? != 0 {
                            return Err(Error::unsupported("wmapro: unknown channel transform type"));
                        }
                    } else {
                        self.chgroup[grp].transform = true;
                        if self.channels == 2 {
                            self.chgroup[grp].decorrelation_matrix = vec![1.0, -1.0, 1.0, 1.0];
                        } else {
                            self.chgroup[grp].decorrelation_matrix =
                                vec![0.70703125, -0.70703125, 0.70703125, 0.70703125];
                        }
                    }
                } else if num_channels > 2 && self.gb.get_bits1()? != 0 {
                    self.chgroup[grp].transform = true;
                    if self.gb.get_bits1()? != 0 {
                        self.decode_decorrelation_matrix(grp)?;
                    } else {
                        if num_channels > 6 {
                            return Err(Error::unsupported("wmapro: coupled channels > 6"));
                        }
                        self.chgroup[grp].decorrelation_matrix =
                            default_decorrelation(num_channels).to_vec();
                    }
                }

                if self.chgroup[grp].transform {
                    if self.gb.get_bits1()? == 0 {
                        for i in 0..self.num_bands {
                            self.chgroup[grp].transform_band[i] = self.gb.get_bits1()? != 0;
                        }
                    } else {
                        for i in 0..self.num_bands {
                            self.chgroup[grp].transform_band[i] = true;
                        }
                    }
                }
                remaining_channels -= self.chgroup[grp].num_channels;
                self.num_chgroups += 1;
            }
        }
        Ok(())
    }

    /// `decode_coeffs` (wmaprodec.c): vector + run-level coefficient decoding.
    fn decode_coeffs(&mut self, c: usize) -> Result<()> {
        const FVAL_TAB: [u32; 16] = [
            0x00000000, 0x3f800000, 0x40000000, 0x40400000, 0x40800000, 0x40a00000, 0x40c00000,
            0x40e00000, 0x41000000, 0x41100000, 0x41200000, 0x41300000, 0x41400000, 0x41500000,
            0x41600000, 0x41700000,
        ];
        let vlctable = self.gb.get_bits1()? != 0;
        let subframe_len = self.subframe_len;
        let cur_coeff_limit = if vlctable { self.channel[c].num_vec_coeffs } else { usize::MAX };

        let mut rl_mode = false;
        let mut cur_coeff = 0usize;
        let mut num_zeros = 0usize;

        while (self.transmit_num_vec_coeffs || !rl_mode) && cur_coeff + 3 < cur_coeff_limit {
            let idx = self.coef_vlc_read(vlctable)?;
            let mut vals = [0u32; 4];
            if (idx as i32) < 0 {
                for i in (0..4).step_by(2) {
                    let idx2 = self.coef_vlc_read(vlctable)?;
                    if (idx2 as i32) < 0 {
                        let mut v0 = self.gb.get_vlc(&self.vec1_vlc)? as u32;
                        if v0 == (VEC1_TABLE.len() - 1) as u32 {
                            v0 = v0.wrapping_add(self.get_large_val()?);
                        }
                        let mut v1 = self.gb.get_vlc(&self.vec1_vlc)? as u32;
                        if v1 == (VEC1_TABLE.len() - 1) as u32 {
                            v1 = v1.wrapping_add(self.get_large_val()?);
                        }
                        vals[i] = v0;
                        vals[i + 1] = v1;
                    } else {
                        vals[i] = FVAL_TAB[(idx2 >> 4) as usize & 0xF];
                        vals[i + 1] = FVAL_TAB[(idx2 & 0xF) as usize];
                    }
                }
            } else {
                vals[0] = FVAL_TAB[(idx >> 12) as usize];
                vals[1] = FVAL_TAB[((idx >> 8) & 0xF) as usize];
                vals[2] = FVAL_TAB[((idx >> 4) & 0xF) as usize];
                vals[3] = FVAL_TAB[(idx & 0xF) as usize];
            }

            for i in 0..4 {
                if vals[i] != 0 {
                    let sign_bit = if self.gb.get_bits1()? == 0 { 1u32 << 31 } else { 0 };
                    self.channel[c].coeffs[cur_coeff] =
                        f32::from_bits(vals[i] ^ sign_bit);
                    num_zeros = 0;
                } else {
                    self.channel[c].coeffs[cur_coeff] = 0.0;
                    num_zeros += 1;
                    rl_mode |= num_zeros > self.subframe_len >> 8;
                }
                cur_coeff += 1;
            }
        }

        if cur_coeff < subframe_len {
            for cc in self.channel[c].coeffs[cur_coeff..subframe_len].iter_mut() {
                *cc = 0.0;
            }
            self.run_level_decode(vlctable, c, cur_coeff, subframe_len)?;
        }
        Ok(())
    }

    /// A coef-VLC read: for vec4/vec2 negative symbols come back as raw
    /// negative values; for coef0/coef1 the symbol indexes run/level tables.
    fn coef_vlc_read(&mut self, vlctable: bool) -> Result<u32> {
        let v = self.gb.get_vlc(&self.vec4_vlc)?;
        let _ = vlctable;
        Ok(v as u32)
    }

    /// `ff_wma_get_large_val` (wma.c).
    fn get_large_val(&mut self) -> Result<u32> {
        let mut n_bits = 8usize;
        if self.gb.get_bits1()? != 0 {
            n_bits += 8;
            if self.gb.get_bits1()? != 0 {
                n_bits += 8;
                if self.gb.get_bits1()? != 0 {
                    n_bits += 7;
                }
            }
        }
        self.gb.get_bits(n_bits)
    }

    /// `ff_wma_run_level_decode` (wma.c), version 1 (wmapro).
    fn run_level_decode(&mut self, vlctable: bool, c: usize, offset0: usize, num_coefs: usize) -> Result<()> {
        let block_len = self.subframe_len;
        let coef_mask = block_len - 1;
        let mut offset = offset0;
        while offset < num_coefs {
            let code = if vlctable {
                self.gb.get_vlc(&self.coef_vlc[1])?
            } else {
                self.gb.get_vlc(&self.coef_vlc[0])?
            };
            if code > 1 {
                let run = if vlctable { COEF1_RUN[code as usize] } else { COEF0_RUN[code as usize] };
                offset += run as usize;
                let sign = self.gb.get_bits1()? as i32 - 1;
                let level = if vlctable { COEF1_LEVEL[code as usize] } else { COEF0_LEVEL[code as usize] };
                self.channel[c].coeffs[offset & coef_mask] = if sign != 0 { -level } else { level };
            } else if code == 1 {
                break;
            } else {
                // escape: wmapro version
                let level = self.get_large_val()?;
                if self.gb.get_bits1()? != 0 {
                    if self.gb.get_bits1()? != 0 {
                        if self.gb.get_bits1()? != 0 {
                            return Err(Error::invalid("wmapro: broken escape sequence"));
                        }
                        offset += self.gb.get_bits(self.esc_len as usize)? as usize + 4;
                    } else {
                        offset += self.gb.get_bits(2)? as usize + 1;
                    }
                }
                let sign = self.gb.get_bits1()? as i32 - 1;
                let signed = if sign != 0 {
                    (level as i32).wrapping_neg()
                } else {
                    level as i32
                };
                self.channel[c].coeffs[offset & coef_mask] = signed as f32;
            }
        }
        if offset > num_coefs {
            return Err(Error::invalid("wmapro: overflow in spectral RLE"));
        }
        Ok(())
    }

    /// `decode_scale_factors` (wmaprodec.c).
    fn decode_scale_factors(&mut self) -> Result<()> {
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let num_bands = self.num_bands;
            let table_idx = self.table_idx;

            if self.channel[c].reuse_sf {
                let src_idx = self.channel[c].scale_factor_idx;
                let sf_offsets = &self.sf_offsets[table_idx][self.channel[c].table_idx];
                for b in 0..num_bands {
                    self.channel[c].saved_scale_factors[src_idx ^ 1][b] =
                        self.channel[c].saved_scale_factors[src_idx][sf_offsets[b] as usize];
                }
            }
            let src_idx = self.channel[c].scale_factor_idx;
            if self.channel[c].cur_subframe == 0 || self.gb.get_bits1()? != 0 {
                if !self.channel[c].reuse_sf {
                    let step = self.gb.get_bits(2)? as i32 + 1;
                    self.channel[c].scale_factor_step = step;
                    let mut val = 45 / step;
                    for b in 0..num_bands {
                        val += self.gb.get_vlc(&self.sf_vlc)?;
                        self.channel[c].saved_scale_factors[src_idx ^ 1][b] = val;
                    }
                } else {
                    let mut b = 0usize;
                    while b < num_bands {
                        let idx = self.gb.get_vlc(&self.sf_rl_vlc)?;
                        if idx == 0 {
                            let code = self.gb.get_bits(14)? as u32;
                            let val = (code >> 6) as i32;
                            let sign = (code & 1) as i32 - 1;
                            let skip = ((code & 0x3f) >> 1) as usize;
                            b += skip;
                            if b >= num_bands {
                                return Err(Error::invalid("wmapro: invalid scale factor coding"));
                            }
                            let delta = if sign != 0 { -val } else { val };
                            self.channel[c].saved_scale_factors[src_idx ^ 1][b] +=
                                delta;
                            b += 1;
                        } else if idx == 1 {
                            break;
                        } else {
                            let skip = SCALE_RL_RUN[idx as usize] as usize;
                            let val = SCALE_RL_LEVEL[idx as usize] as i32;
                            let sign = self.gb.get_bits1()? as i32 - 1;
                            b += skip;
                            if b >= num_bands {
                                return Err(Error::invalid("wmapro: invalid scale factor coding"));
                            }
                            let signed = if sign != 0 { -val } else { val };
                            self.channel[c].saved_scale_factors[src_idx ^ 1][b] += signed;
                            b += 1;
                        }
                    }
                }
                self.channel[c].scale_factor_idx ^= 1;
                self.channel[c].table_idx = table_idx;
                self.channel[c].reuse_sf = true;
            }

            let src = self.channel[c].scale_factor_idx;
            let mut max = self.channel[c].saved_scale_factors[src][0];
            for b in 1..num_bands {
                max = max.max(self.channel[c].saved_scale_factors[src][b]);
            }
            self.channel[c].max_scale_factor = max;
        }
        Ok(())
    }

    /// `inverse_channel_transform` (wmaprodec.c).
    fn inverse_channel_transform(&mut self) {
        for g in 0..self.num_chgroups {
            if !self.chgroup[g].transform {
                continue;
            }
            let num_channels = self.chgroup[g].num_channels;
            let ch_data = self.chgroup[g].channel_data.clone();
            let num_bands = self.num_bands;
            let subframe_len = self.subframe_len;
            for b in 0..num_bands {
                let sfb0 = self.cur_sfb_offsets[b] as usize;
                let sfb1 = self.cur_sfb_offsets.get(b + 1).copied().unwrap_or(subframe_len as i32) as usize;
                if self.chgroup[g].transform_band[b] {
                    let sfb1 = sfb1.min(subframe_len);
                    for y in sfb0..sfb1 {
                        let mat = self.chgroup[g].decorrelation_matrix.clone();
                        let mut data = vec![0f32; num_channels];
                        for (j, &ch) in ch_data.iter().enumerate().take(num_channels) {
                            data[j] = self.channel[ch].coeffs[y];
                        }
                        for (out_j, &ch) in ch_data.iter().enumerate().take(num_channels) {
                            let mut sum = 0f32;
                            for (row, &dv) in data.iter().enumerate() {
                                sum += dv * mat[out_j * num_channels + row];
                            }
                            self.channel[ch].coeffs[y] = sum;
                        }
                    }
                } else if self.channels == 2 {
                    let len = sfb1.min(subframe_len) - sfb0;
                    for ch in ch_data.iter().take(2) {
                        for y in sfb0..sfb0 + len {
                            self.channel[*ch].coeffs[y] *= 181.0 / 128.0;
                        }
                    }
                }
            }
        }
    }
}

impl WmaProDecoder {
    /// `wmapro_window` (wmaprodec.c): window + overlap-add via vector_fmul_window.
    fn wmapro_window(&mut self) {
        let subframe_len = self.subframe_len;
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let winlen = self.channel[c].prev_block_len;
            let mut start = self.channel[c].out.len() / 3; // placeholder, fixed below
            let _ = start;
            // `start` in FFmpeg points at coeffs - (winlen>>1) inside the
            // channel's `out` buffer (coeffs = &out[offset] with
            // offset = subframe position + samples_per_frame/2).
            // We reconstruct it from the subframe offset bookkeeping.
            start = 0;
            let _ = start;
            let mut winlen2 = winlen;
            if subframe_len < winlen2 {
                winlen2 = subframe_len;
            }
            let win_idx = 31 - (winlen2 as u32).leading_zeros() as usize - WMAPRO_BLOCK_MIN_BITS as usize;
            let window = &self.windows[win_idx];
            let half = winlen2 >> 1;
            // overlap-add into out at (cur position - half)
            let pos = self.channel[c].cur_out_pos - half;
            let (a, b) = {
                // src0 = out[pos..pos+half], src1 = out[pos+half..pos+2*half]
                let out = &self.channel[c].out;
                (
                    out[pos..pos + half].to_vec(),
                    out[pos + half..pos + 2 * half].to_vec(),
                )
            };
            let mut dst = vec![0f32; 2 * half];
            vector_fmul_window(&mut dst, &a, &b, window, half);
            let out = &mut self.channel[c].out;
            out[pos..pos + 2 * half].copy_from_slice(&dst);
            self.channel[c].prev_block_len = subframe_len;
        }
    }

    /// `decode_subframe` (wmaprodec.c).
    fn decode_subframe(&mut self) -> Result<()> {
        let mut offset = self.samples_per_frame;
        let mut subframe_len = self.samples_per_frame;
        let mut total_samples = self.samples_per_frame * self.channels;

        self.subframe_offset = self.gb.bits_count();

        for i in 0..self.channels {
            self.channel[i].grouped = false;
            if offset > self.channel[i].decoded_samples {
                offset = self.channel[i].decoded_samples;
                subframe_len = self.channel[i].subframe_len[self.channel[i].cur_subframe];
            }
        }

        self.channels_for_cur_subframe = 0;
        for i in 0..self.channels {
            let cur_subframe = self.channel[i].cur_subframe;
            total_samples -= self.channel[i].decoded_samples;
            if offset == self.channel[i].decoded_samples
                && subframe_len == self.channel[i].subframe_len[cur_subframe]
            {
                total_samples -= self.channel[i].subframe_len[cur_subframe];
                self.channel[i].decoded_samples += self.channel[i].subframe_len[cur_subframe];
                self.channel_indexes_for_cur_subframe[self.channels_for_cur_subframe] = i;
                self.channels_for_cur_subframe += 1;
            }
        }
        if total_samples == 0 {
            self.parsed_all_subframes = true;
        }

        self.table_idx = 31 - ((self.samples_per_frame / subframe_len) as u32).leading_zeros() as usize;
        self.num_bands = self.num_sfb[self.table_idx] as usize;
        self.cur_sfb_offsets = self.sfb_offsets[self.table_idx].to_vec();
        let cur_subwoofer_cutoff = self.subwoofer_cutoffs[self.table_idx] as usize;

        let offset = offset + (self.samples_per_frame >> 1);
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            self.channel[c].cur_out_pos = offset;
        }

        self.subframe_len = subframe_len;
        self.esc_len = 32 - ((subframe_len - 1) as u32).leading_zeros();

        // extended header
        if self.gb.get_bits1()? != 0 {
            let mut num_fill_bits = self.gb.get_bits(2)? as usize;
            if num_fill_bits == 0 {
                let len = self.gb.get_bits(4)? as usize;
                num_fill_bits = if len > 0 { self.gb.get_bits(len)? as usize + 1 } else { 1 };
            }
            if self.gb.bits_count() + num_fill_bits > self.num_saved_bits {
                return Err(Error::invalid("wmapro: invalid number of fill bits"));
            }
            self.gb.skip_bits(num_fill_bits)?;
        }

        if self.gb.get_bits1()? != 0 {
            return Err(Error::unsupported("wmapro: reserved bit set"));
        }

        self.decode_channel_transform()?;

        let mut transmit_coeffs = false;
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            self.channel[c].transmit_coefs = self.gb.get_bits1()? != 0;
            if self.channel[c].transmit_coefs {
                transmit_coeffs = true;
            }
        }

        if transmit_coeffs {
            let mut quant_step = 90 * self.bits_per_sample as i32 >> 4;
            self.transmit_num_vec_coeffs = self.gb.get_bits1()? != 0;
            if self.transmit_num_vec_coeffs {
                let num_bits = 32 - (((subframe_len + 3) / 4) as u32).leading_zeros();
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    let num_vec_coeffs = (self.gb.get_bits(num_bits as usize)? as usize) << 2;
                    if num_vec_coeffs > subframe_len {
                        return Err(Error::invalid("wmapro: num_vec_coeffs too large"));
                    }
                    self.channel[c].num_vec_coeffs = num_vec_coeffs;
                }
            } else {
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    self.channel[c].num_vec_coeffs = subframe_len;
                }
            }
            let step = self.gb.get_sbits(6)?;
            quant_step += step;
            if step == -32 || step == 31 {
                let sign = if step == 31 { -1i32 } else { 1 };
                let mut quant = 0i32;
                let mut s = step;
                while self.gb.bits_count() + 5 < self.num_saved_bits {
                    s = self.gb.get_bits(5)? as i32;
                    if s != 31 {
                        break;
                    }
                    quant += 31;
                }
                quant_step += ((quant + s) ^ sign) - sign;
            }

            if self.channels_for_cur_subframe == 1 {
                let c = self.channel_indexes_for_cur_subframe[0];
                self.channel[c].quant_step = quant_step;
            } else {
                let modifier_len = self.gb.get_bits(3)? as usize;
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    self.channel[c].quant_step = quant_step;
                    if self.gb.get_bits1()? != 0 {
                        if modifier_len > 0 {
                            self.channel[c].quant_step += self.gb.get_bits(modifier_len)? as i32 + 1;
                        } else {
                            self.channel[c].quant_step += 1;
                        }
                    }
                }
            }

            self.decode_scale_factors()?;
        }

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            if self.channel[c].transmit_coefs && self.gb.bits_count() < self.num_saved_bits {
                self.decode_coeffs(c)?;
            } else {
                for cc in self.channel[c].coeffs[..subframe_len].iter_mut() {
                    *cc = 0.0;
                }
            }
        }

        if transmit_coeffs {
            let tx_idx = (31 - (subframe_len as u32).leading_zeros()).saturating_sub(WMAPRO_BLOCK_MIN_BITS) as usize;
            // inverse channel transform
            self.inverse_channel_transform();
            for i in 0..self.channels_for_cur_subframe {
                let c = self.channel_indexes_for_cur_subframe[i];
                let sf_src = self.channel[c].scale_factor_idx;
                if c as i32 == self.lfe_channel {
                    for cc in self.tmp[cur_subwoofer_cutoff..subframe_len].iter_mut() {
                        *cc = 0.0;
                    }
                }
                for b in 0..self.num_bands {
                    let end = (self.cur_sfb_offsets[b + 1] as usize).min(subframe_len);
                    let exp = self.channel[c].quant_step
                        - (self.channel[c].max_scale_factor
                            - self.channel[c].saved_scale_factors[sf_src][b])
                            * self.channel[c].scale_factor_step;
                    let quant = 10f32.powf(exp as f32 / 20.0);
                    let start = self.cur_sfb_offsets[b] as usize;
                    for (j, cc) in self.tmp[start..end].iter_mut().enumerate() {
                        *cc = self.channel[c].coeffs[start + j] * quant;
                    }
                }
                // IMDCT into coeffs
                let mdct_len = 1usize << (WMAPRO_BLOCK_MIN_BITS + tx_idx as u32);
                let mut coeffs_copy = self.channel[c].coeffs.clone();
                let out = &mut self.channel[c].coeffs;
                self.mdcts[tx_idx].imdct_half(&mut coeffs_copy[..mdct_len], &self.tmp[..mdct_len]);
                // FFmpeg: tx_fn(tx, coeffs, tmp) — half MDCT output lands at
                // the first mdct_len floats of coeffs? For the non-FULL mdct
                // the output is mdct_len samples (the DCT-IV window).
                out[..mdct_len].copy_from_slice(&coeffs_copy[..mdct_len]);
            }
        }

        self.wmapro_window();

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            if self.channel[c].cur_subframe >= self.channel[c].num_subframes {
                return Err(Error::invalid("wmapro: broken subframe"));
            }
            self.channel[c].cur_subframe += 1;
        }
        Ok(())
    }

    /// `decode_frame` (wmaprodec.c). Returns `more_frames`.
    fn decode_frame(&mut self) -> Result<bool> {
        let more_frames;
        let mut len = 0usize;

        if self.len_prefix {
            len = self.gb.get_bits(self.log2_frame_size as usize)? as usize;
        }
        if self.decode_tilehdr().is_err() {
            self.packet_loss = true;
            return Ok(false);
        }

        if self.channels > 1 && self.gb.get_bits1()? != 0 && self.gb.get_bits1()? != 0 {
            for _ in 0..self.channels * self.channels {
                self.gb.skip_bits(4)?;
            }
        }

        if self.dynamic_range_compression {
            self.drc_gain = self.gb.get_bits(8)? as u8;
        }

        if self.gb.get_bits1()? != 0 {
            if self.gb.get_bits1()? != 0 {
                let bits = 32 - ((self.samples_per_frame * 2) as u32).leading_zeros() - 1;
                self.trim_start = self.gb.get_bits(bits as usize)? as usize;
            }
            if self.gb.get_bits1()? != 0 {
                let bits = 32 - ((self.samples_per_frame * 2) as u32).leading_zeros() - 1;
                self.trim_end = self.gb.get_bits(bits as usize)? as usize;
            }
        } else {
            self.trim_start = 0;
            self.trim_end = 0;
        }

        self.parsed_all_subframes = false;
        for i in 0..self.channels {
            self.channel[i].decoded_samples = 0;
            self.channel[i].cur_subframe = 0;
            self.channel[i].reuse_sf = false;
        }

        while !self.parsed_all_subframes {
            if self.decode_subframe().is_err() {
                self.packet_loss = true;
                return Ok(false);
            }
        }

        let mut frame = AudioFrame {
            samples: self.samples_per_frame as u32,
            pts: None,
            data: Vec::with_capacity(self.channels),
        };

        let start = self.trim_start;
        let end = self.samples_per_frame.saturating_sub(self.trim_end);
        self.trim_start = 0;
        self.trim_end = 0;

        if start < end {
            frame.samples = (end - start) as u32;
            for i in 0..self.channels {
                frame.data.push(
                    self.channel[i].out[start..end]
                        .iter()
                        .copied()
                        .collect::<Vec<f32>>()
                        .iter()
                        .flat_map(|f| f.to_le_bytes())
                        .collect::<Vec<u8>>(),
                );
            }
            if self.skip_frame {
                self.skip_frame = false;
            } else {
                self.pending.push(frame);
            }
        } else if self.skip_frame {
            self.skip_frame = false;
        }

        for i in 0..self.channels {
            let half = self.samples_per_frame / 2;
            let out = &mut self.channel[i].out;
            out.copy_within(self.samples_per_frame..self.samples_per_frame + half, 0);
        }

        if self.len_prefix {
            if len != (self.gb.bits_count() - self.frame_offset) + 2 {
                eprintln!("packet_loss: len mismatch: len={len} bits_count={} frame_offset={}", self.gb.bits_count(), self.frame_offset);
                self.packet_loss = true;
                return Ok(false);
            }
            let skip = len - (self.gb.bits_count() - self.frame_offset) - 1;
            if skip > 0 {
                self.gb.skip_bits(skip)?;
            }
        } else {
            while self.gb.bits_count() < self.num_saved_bits && self.gb.get_bits1()? == 0 {}
        }

        more_frames = self.gb.get_bits1()? != 0;
        Ok(more_frames)
    }

    /// `save_bits` (wmaprodec.c): copy `len` bits from the packet reader into
    /// the frame reservoir. `append` continues the previous frame; otherwise
    /// the reservoir restarts at the current byte-aligned offset.
    fn save_bits(&mut self, gb: &mut BitReader<'_>, len: usize, append: bool) {
        if len == 0 {
            return;
        }
        if !append {
            self.frame_offset = gb.bits_count() & 7;
            self.num_saved_bits = self.frame_offset;
            self.frame_data.clear();
        }
        let total = self.num_saved_bits + len;
        let bytes = ((total + 7) >> 3).max(1);
        if bytes > MAX_FRAMESIZE {
            self.packet_loss = true;
            self.num_saved_bits = 0;
            return;
        }
        if self.frame_data.len() < bytes {
            self.frame_data.resize(bytes, 0);
        }
        let start_bit = self.num_saved_bits;
        for i in 0..len {
            let bit = gb.get_bits1().unwrap_or(0);
            let idx = start_bit + i;
            let byte = idx / 8;
            let off = idx % 8;
            if bit != 0 {
                self.frame_data[byte] |= 1 << (7 - off);
            } else {
                self.frame_data[byte] &= !(1 << (7 - off));
            }
        }
        self.num_saved_bits = total;
        let data = self.frame_data.clone();
        self.gb = crate::bits::OwnedBitReader::from_bits(data, self.num_saved_bits);
        let _ = self.gb.skip_bits(self.frame_offset);
    }

    /// `decode_packet` (wmaprodec.c) for one block_align chunk.
    fn decode_packet_chunk(&mut self, cur_data: &[u8]) -> Result<usize> {
        let mut buf_size = cur_data.len();
        if buf_size == 0 {
            self.packet_done = false;
            if self.eof_done {
                return Ok(0);
            }
            let mut frame = AudioFrame {
                samples: self.samples_per_frame as u32,
                pts: None,
                data: Vec::with_capacity(self.channels),
            };
            for i in 0..self.channels {
                let half = self.samples_per_frame / 2;
                let mut samples = vec![0f32; self.samples_per_frame];
                samples[..half].copy_from_slice(&self.channel[i].out[..half]);
                frame.data.push(
                    samples.iter().flat_map(|f| f.to_le_bytes()).collect::<Vec<u8>>(),
                );
            }
            self.eof_done = true;
            self.packet_done = true;
            self.pending.push(frame);
            return Ok(0);
        }

        let mut gb;
        if self.packet_done || self.packet_loss {
            self.packet_done = false;
            if buf_size < block_align_of(self) {
                self.packet_loss = true;
                return Err(Error::invalid("wmapro: input packet too small"));
            }
            self.next_packet_start = buf_size - block_align_of(self);
            buf_size = block_align_of(self);
            self.buf_bit_size = buf_size << 3;

            gb = BitReader::with_bit_len(&cur_data[..buf_size], self.buf_bit_size);
            let packet_sequence_number = gb.get_bits(4)? as u8;
            gb.skip_bits(2)?;
            let num_bits_prev_frame = gb.get_bits(self.log2_frame_size as usize)? as usize;

            if !self.packet_loss
                && ((self.packet_sequence_number as u32 + 1) & 0xF) != packet_sequence_number as u32
            {
                eprintln!("packet_loss: seq mismatch: prev={} curr={}", self.packet_sequence_number, packet_sequence_number);
                self.packet_loss = true;
            }
            self.packet_sequence_number = packet_sequence_number;

            if num_bits_prev_frame > 0 {
                let remaining_packet_bits = self.buf_bit_size - gb.bits_count();
                let mut nb = num_bits_prev_frame;
                if nb >= remaining_packet_bits {
                    nb = remaining_packet_bits;
                    self.packet_done = true;
                }
                self.save_bits(&mut gb, nb, true);
                if !self.packet_loss {
                    self.decode_frame()?;
                }
            } else if self.num_saved_bits > self.frame_offset {
                // ignoring previously saved bits
            }

            if self.packet_loss {
                self.num_saved_bits = 0;
                self.packet_loss = false;
                self.frame_data.clear();
            }
        } else {
            if cur_data.len() < self.next_packet_start {
                self.packet_loss = true;
                return Err(Error::invalid("wmapro: packet too small"));
            }
            self.buf_bit_size = (cur_data.len() - self.next_packet_start) << 3;
            gb = BitReader::with_bit_len(&cur_data[self.next_packet_start..], self.buf_bit_size);
            gb.skip_bits(self.packet_offset)?;

            let remaining = self.buf_bit_size.saturating_sub(gb.bits_count());
            if self.len_prefix && remaining > self.log2_frame_size as usize {
                let frame_size = gb.show_bits(self.log2_frame_size as usize)? as usize;
                if frame_size > 0 && frame_size <= remaining {
                    self.save_bits(&mut gb, frame_size, false);
                    if !self.packet_loss {
                        let more = self.decode_frame()?;
                        self.packet_done = !more;
                    }
                } else {
                    self.packet_done = true;
                }
            } else if !self.len_prefix && self.num_saved_bits > self.gb.bit_pos() {
                let more = self.decode_frame()?;
                self.packet_done = !more;
            } else {
                self.packet_done = true;
            }
        }

        let remaining = self.buf_bit_size as i64 - gb.bits_count() as i64;
        if remaining < 0 {
            eprintln!("packet_loss: remaining < 0: {remaining}");
            self.packet_loss = true;
        }

        if self.packet_done && !self.packet_loss && remaining > 0 {
            self.save_bits(&mut gb, remaining as usize, false);
        }

        self.packet_offset = gb.bits_count() & 7;
        if self.packet_loss {
            return Err(Error::invalid("wmapro: packet loss"));
        }
        let consumed = (gb.bits_count() >> 3) + self.next_packet_start;
        self.next_packet_start = 0;
        Ok(consumed)
    }

    fn decode_packet_impl(&mut self, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            self.decode_packet_chunk(&[])?;
            return Ok(());
        }
        let mut cur = data;
        while !cur.is_empty() {
            let consumed = self.decode_packet_chunk(cur)?;
            if consumed >= cur.len() {
                break;
            }
            if consumed > 0 {
                cur = &cur[consumed..];
            } else if self.packet_done {
                break;
            }
        }
        Ok(())
    }
}

fn block_align_of(s: &WmaProDecoder) -> usize {
    s.block_align_len
}

impl Decoder for WmaProDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.decode_packet_impl(&packet.data)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if !self.pending.is_empty() {
            return Ok(Frame::Audio(self.pending.remove(0)));
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        for ch in &mut self.channel {
            for v in ch.out.iter_mut() {
                *v = 0.0;
            }
        }
        self.packet_loss = true;
        self.eof_done = false;
        self.skip_frame = true;
        self.pending.clear();
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.flush()
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: SampleFormat::F32P,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}

// Ported from FFmpeg (commit 2da55bf): libavcodec/wmalosslessdec.c and
// libavcodec/lossless_audiodsp.c (scalarproduct_and_madd_*), with
// wma_common.c's frame-length helper.
// GNU Lesser General Public License 2.1 or later

//! WMA Lossless decoder.

use crate::bits::{BitReader, OwnedBitReader};
use crate::wma_common::wma_get_frame_len_bits;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

pub const WMALL_MAX_CHANNELS: usize = 8;
pub const MAX_SUBFRAMES: usize = 32;
pub const MAX_BANDS: usize = 29;
pub const MAX_FRAMESIZE: usize = 32768;
pub const MAX_ORDER: usize = 256;
pub const WMALL_BLOCK_MIN_BITS: u32 = 6;
pub const WMALL_BLOCK_MAX_BITS: u32 = 14;
pub const WMALL_BLOCK_MAX_SIZE: usize = 1 << WMALL_BLOCK_MAX_BITS;
pub const WMALL_COEFF_PAD: usize = 8; // pad entries (int16) for the LMS arrays

#[inline]
fn wmasign(x: i32) -> i32 {
    if x > 0 {
        1
    } else if x < 0 {
        -1
    } else {
        0
    }
}

#[inline]
fn clip_i32(x: i32, lo: i32, hi: i32) -> i32 {
    x.max(lo).min(hi)
}

#[derive(Clone)]
struct ChannelCtx {
    #[allow(dead_code)]
    prev_block_len: usize,
    num_subframes: usize,
    subframe_len: [usize; MAX_SUBFRAMES],
    subframe_offsets: [usize; MAX_SUBFRAMES],
    cur_subframe: usize,
    decoded_samples: usize,
    transient_counter: i32,
}

impl Default for ChannelCtx {
    fn default() -> Self {
        Self {
            prev_block_len: 0,
            num_subframes: 0,
            subframe_len: [0; MAX_SUBFRAMES],
            subframe_offsets: [0; MAX_SUBFRAMES],
            cur_subframe: 0,
            decoded_samples: 0,
            transient_counter: 0,
        }
    }
}

#[derive(Clone)]
struct Cdlms {
    order: usize,
    scaling: i32,
    coefsend: usize,
    bitsend: i32,
    coefs: [i16; MAX_ORDER + WMALL_COEFF_PAD],
    lms_prevvalues: [i32; MAX_ORDER * 2 + WMALL_COEFF_PAD],
    lms_updates: [i16; MAX_ORDER * 2 + WMALL_COEFF_PAD],
    recent: usize,
}

/// Whole-decoder state (`WmallDecodeCtx`).
pub struct WmaLosslessDecoder {
    codec_id: CodecId,
    channels: usize,
    sample_rate: u32,

    #[allow(dead_code)]
    decode_flags: u32,
    len_prefix: bool,
    dynamic_range_compression: bool,
    bits_per_sample: u32,
    samples_per_frame: usize,
    log2_frame_size: u32,
    #[allow(dead_code)]
    lfe_channel: i32,
    max_num_subframes: usize,
    #[allow(dead_code)]
    subframe_len_bits: u32,
    #[allow(dead_code)]
    max_subframe_len_bit: bool,
    min_samples_per_subframe: usize,

    max_frame_size: usize,
    block_align_stored: usize,
    frame_data: Vec<u8>,
    num_saved_bits: usize,
    frame_offset: usize,
    subframe_offset: usize,
    packet_loss: bool,
    packet_done: bool,
    packet_sequence_number: u8,
    packet_offset: usize,
    next_packet_start: usize,
    trim_end: usize,
    buf_bit_size: usize,

    gb: OwnedBitReader,
    drc_gain: u8,
    skip_frame: bool,
    parsed_all_subframes: bool,
    #[allow(dead_code)]
    subframe_len: usize,
    channels_for_cur_subframe: usize,
    channel_indexes_for_cur_subframe: [usize; WMALL_MAX_CHANNELS],
    channel: Vec<ChannelCtx>,
    out: Vec<Vec<i32>>, // per-channel 32-bit accumulation buffer for the current frame

    do_arith_coding: bool,
    do_ac_filter: bool,
    do_inter_ch_decorr: bool,
    do_mclms: bool,
    do_lpc: bool,

    acfilter_order: usize,
    acfilter_scaling: i32,
    acfilter_coeffs: [i16; 16],
    acfilter_prevvalues: [[i32; 16]; WMALL_MAX_CHANNELS],

    mclms_order: usize,
    mclms_scaling: i32,
    mclms_coeffs: Vec<i16>,
    mclms_coeffs_cur: Vec<i16>,
    mclms_prevvalues: Vec<i32>,
    mclms_updates: Vec<i16>,
    mclms_recent: usize,

    movave_scaling: i32,
    quant_stepsize: i32,

    cdlms: Vec<[Cdlms; 9]>,
    cdlms_ttl: [usize; WMALL_MAX_CHANNELS],

    b_v3_rtm: bool,
    is_channel_coded: [bool; WMALL_MAX_CHANNELS],
    update_speed: [i32; WMALL_MAX_CHANNELS],

    transient: [bool; WMALL_MAX_CHANNELS],
    transient_pos: [usize; WMALL_MAX_CHANNELS],
    seekable_tile: bool,

    ave_sum: [u32; WMALL_MAX_CHANNELS],
    channel_residues: Vec<[i32; WMALL_BLOCK_MAX_SIZE]>,

    lpc_coefs: [[i32; 40]; WMALL_MAX_CHANNELS],
    lpc_order: usize,
    lpc_scaling: i32,
    lpc_intbits: i32,

    pending: Vec<AudioFrame>,
}

impl WmaLosslessDecoder {
    /// `decode_init` (wmalosslessdec.c).
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = params.channels.unwrap_or(0) as usize;
        let sample_rate = params.sample_rate.unwrap_or(0);
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|&b| b > 0 && b <= (1 << 21))
            .ok_or_else(|| Error::invalid("wmall: block_align is not set or invalid"))? as usize;
        let extradata = &params.extradata;
        if extradata.len() < 18 {
            return Err(Error::unsupported("wmall: unsupported extradata size"));
        }
        let rd16 = |p: usize| u16::from_le_bytes([extradata[p], extradata[p + 1]]) as u32;
        let rd32 =
            |p: usize| u32::from_le_bytes([extradata[p], extradata[p + 1], extradata[p + 2], extradata[p + 3]]);
        let decode_flags = rd16(14);
        let channel_mask = rd32(2);
        let bits_per_sample = rd16(0);
        if bits_per_sample != 16 && bits_per_sample != 24 {
            return Err(Error::invalid(format!("wmall: unknown bit-depth {bits_per_sample}")));
        }
        if channels == 0 || channels > WMALL_MAX_CHANNELS {
            return Err(Error::unsupported("wmall: more than 8 channels"));
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

        let max_frame_size = MAX_FRAMESIZE * channels;
        let log2_frame_size: u32 = (32 - (block_align as u32).leading_zeros() - 1) + 4;
        let len_prefix = decode_flags & 0x40 != 0;
        let samples_per_frame = 1usize << wma_get_frame_len_bits(sample_rate, 3, decode_flags);

        let log2_max_num_subframes = ((decode_flags & 0x38) >> 3) as usize;
        let max_num_subframes = 1usize << log2_max_num_subframes;
        let subframe_len_bits = 32 - (log2_max_num_subframes as u32).leading_zeros();
        let min_samples_per_subframe = samples_per_frame / max_num_subframes;
        let b_v3_rtm = decode_flags & 0x100 != 0;

        if max_num_subframes > MAX_SUBFRAMES {
            return Err(Error::invalid("wmall: invalid number of subframes"));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            channels,
            sample_rate,
            decode_flags,
            len_prefix,
            dynamic_range_compression: decode_flags & 0x80 != 0,
            bits_per_sample,
            samples_per_frame,
            log2_frame_size,
            lfe_channel,
            max_num_subframes,
            subframe_len_bits,
            max_subframe_len_bit: false,
            min_samples_per_subframe,
            max_frame_size,
            block_align_stored: block_align,
            frame_data: vec![0u8; max_frame_size + 64],
            num_saved_bits: 0,
            frame_offset: 0,
            subframe_offset: 0,
            packet_loss: true,
            packet_done: false,
            packet_sequence_number: 0,
            packet_offset: 0,
            next_packet_start: 0,
            trim_end: 0,
            buf_bit_size: 0,
            gb: OwnedBitReader::new(),
            drc_gain: 0,
            skip_frame: true,
            parsed_all_subframes: false,
            subframe_len: 0,
            channels_for_cur_subframe: 0,
            channel_indexes_for_cur_subframe: [0; WMALL_MAX_CHANNELS],
            channel: (0..channels)
                .map(|_| ChannelCtx {
                    prev_block_len: samples_per_frame,
                    ..Default::default()
                })
                .collect(),
            out: (0..channels).map(|_| vec![0i32; samples_per_frame]).collect(),
            do_arith_coding: false,
            do_ac_filter: false,
            do_inter_ch_decorr: false,
            do_mclms: false,
            do_lpc: false,
            acfilter_order: 0,
            acfilter_scaling: 0,
            acfilter_coeffs: [0; 16],
            acfilter_prevvalues: [[0; 16]; WMALL_MAX_CHANNELS],
            mclms_order: 0,
            mclms_scaling: 0,
            mclms_coeffs: vec![0; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS * 32],
            mclms_coeffs_cur: vec![0; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS],
            mclms_prevvalues: vec![0; WMALL_MAX_CHANNELS * 2 * 32],
            mclms_updates: vec![0; WMALL_MAX_CHANNELS * 2 * 32],
            mclms_recent: 0,
            movave_scaling: 0,
            quant_stepsize: 1,
            cdlms: (0..WMALL_MAX_CHANNELS).map(|_| Default::default()).collect(),
            cdlms_ttl: [0; WMALL_MAX_CHANNELS],
            b_v3_rtm,
            is_channel_coded: [false; WMALL_MAX_CHANNELS],
            update_speed: [8; WMALL_MAX_CHANNELS],
            transient: [false; WMALL_MAX_CHANNELS],
            transient_pos: [0; WMALL_MAX_CHANNELS],
            seekable_tile: false,
            ave_sum: [0; WMALL_MAX_CHANNELS],
            channel_residues: (0..WMALL_MAX_CHANNELS).map(|_| [0i32; WMALL_BLOCK_MAX_SIZE]).collect(),
            lpc_coefs: [[0; 40]; WMALL_MAX_CHANNELS],
            lpc_order: 0,
            lpc_scaling: 0,
            lpc_intbits: 0,
            pending: Vec::new(),
        })
    }
}

impl Default for Cdlms {
    fn default() -> Self {
        Self {
            order: 0,
            scaling: 0,
            coefsend: 0,
            bitsend: 0,
            coefs: [0; MAX_ORDER + WMALL_COEFF_PAD],
            lms_prevvalues: [0; MAX_ORDER * 2 + WMALL_COEFF_PAD],
            lms_updates: [0; MAX_ORDER * 2 + WMALL_COEFF_PAD],
            recent: 0,
        }
    }
}

impl WmaLosslessDecoder {
    /// `decode_subframe_length` (wmalosslessdec.c).
    fn decode_subframe_length(&mut self, offset: usize) -> Result<usize> {
        if offset == self.samples_per_frame - self.min_samples_per_subframe {
            return Ok(self.min_samples_per_subframe);
        }
        let len = 32 - ((self.max_num_subframes - 1) as u32).leading_zeros() as usize;
        let frame_len_ratio = self.gb.get_bits(len)? as usize;
        let subframe_len = self.min_samples_per_subframe * (frame_len_ratio + 1);
        if subframe_len < self.min_samples_per_subframe || subframe_len > self.samples_per_frame {
            return Err(Error::invalid("wmall: broken frame: subframe_len"));
        }
        Ok(subframe_len)
    }

    /// `decode_tilehdr` (wmalosslessdec.c).
    fn decode_tilehdr(&mut self) -> Result<()> {
        let nch = self.channels;
        let mut num_samples = [0usize; WMALL_MAX_CHANNELS];
        let mut contains_subframe = [false; WMALL_MAX_CHANNELS];
        let mut channels_for_cur_subframe = nch;
        let mut fixed_channel_layout = false;
        let mut min_channel_len = 0usize;

        for c in 0..nch {
            self.channel[c].num_subframes = 0;
        }

        let tile_aligned = self.gb.get_bits1()? != 0;
        if self.max_num_subframes == 1 || tile_aligned {
            fixed_channel_layout = true;
        }

        loop {
            let mut in_use = false;
            for c in 0..nch {
                if num_samples[c] == min_channel_len {
                    contains_subframe[c] = fixed_channel_layout
                        || channels_for_cur_subframe == 1
                        || min_channel_len == self.samples_per_frame - self.min_samples_per_subframe;
                    if !contains_subframe[c] {
                        contains_subframe[c] = self.gb.get_bits1()? != 0;
                    }
                    in_use |= contains_subframe[c];
                } else {
                    contains_subframe[c] = false;
                }
            }
            if !in_use {
                return Err(Error::invalid("wmall: found empty subframe"));
            }
            let subframe_len = self.decode_subframe_length(min_channel_len)?;
            min_channel_len += subframe_len;
            for c in 0..nch {
                if contains_subframe[c] {
                    let ch = &mut self.channel[c];
                    if ch.num_subframes >= MAX_SUBFRAMES {
                        return Err(Error::invalid("wmall: num subframes > 31"));
                    }
                    ch.subframe_len[ch.num_subframes] = subframe_len;
                    num_samples[c] += subframe_len;
                    ch.num_subframes += 1;
                    if num_samples[c] > self.samples_per_frame {
                        return Err(Error::invalid("wmall: channel len > samples_per_frame"));
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

        for c in 0..nch {
            let mut offset = 0usize;
            for i in 0..self.channel[c].num_subframes {
                self.channel[c].subframe_offsets[i] = offset;
                offset += self.channel[c].subframe_len[i];
            }
        }
        Ok(())
    }

    /// `decode_ac_filter` (wmalosslessdec.c).
    fn decode_ac_filter(&mut self) -> Result<()> {
        self.acfilter_order = self.gb.get_bits(4)? as usize + 1;
        self.acfilter_scaling = self.gb.get_bits(4)? as i32;
        for i in 0..self.acfilter_order {
            self.acfilter_coeffs[i] = (self.gb.get_bits(self.acfilter_scaling as usize)? + 1) as i16;
        }
        Ok(())
    }

    /// `decode_mclms` (wmalosslessdec.c).
    fn decode_mclms(&mut self) -> Result<()> {
        self.mclms_order = ((self.gb.get_bits(4)? + 1) * 2) as usize;
        self.mclms_scaling = self.gb.get_bits(4)? as i32;
        if self.gb.get_bits1()? != 0 {
            let mut cbits = 32 - ((self.mclms_scaling + 1) as u32).leading_zeros() - 1;
            if (1 << cbits) < self.mclms_scaling + 1 {
                cbits += 1;
            }
            let send_coef_bits = self.gb.get_bits(cbits as usize)? + 2;
            let n = self.mclms_order * self.channels * self.channels;
            for item in self.mclms_coeffs.iter_mut().take(n) {
                *item = self.gb.get_bits(send_coef_bits as usize)? as i16;
            }
            for i in 0..self.channels {
                for c in 0..i {
                    self.mclms_coeffs_cur[i * self.channels + c] =
                        self.gb.get_bits(send_coef_bits as usize)? as i16;
                }
            }
        }
        Ok(())
    }

    /// `decode_cdlms` (wmalosslessdec.c).
    fn decode_cdlms(&mut self) -> Result<()> {
        let cdlms_send_coef = self.gb.get_bits1()? != 0;
        for c in 0..self.channels {
            self.cdlms_ttl[c] = self.gb.get_bits(3)? as usize + 1;
            for i in 0..self.cdlms_ttl[c] {
                self.cdlms[c][i].order = ((self.gb.get_bits(7)? + 1) * 8) as usize;
                if self.cdlms[c][i].order > MAX_ORDER {
                    self.cdlms[0][0].order = 0;
                    return Err(Error::invalid("wmall: cdlms order > max"));
                }
            }
            for i in 0..self.cdlms_ttl[c] {
                self.cdlms[c][i].scaling = self.gb.get_bits(4)? as i32;
            }
            if cdlms_send_coef {
                for i in 0..self.cdlms_ttl[c] {
                    let order = self.cdlms[c][i].order;
                    let mut cbits = 32 - (order as u32).leading_zeros() - 1;
                    if (1 << cbits) < order as i32 {
                        cbits += 1;
                    }
                    self.cdlms[c][i].coefsend = self.gb.get_bits(cbits as usize)? as usize + 1;

                    let mut cbits = 32 - ((self.cdlms[c][i].scaling + 1) as u32).leading_zeros() - 1;
                    if (1 << cbits) < self.cdlms[c][i].scaling + 1 {
                        cbits += 1;
                    }
                    self.cdlms[c][i].bitsend = self.gb.get_bits(cbits as usize)? as i32 + 2;
                    let shift_l = 32 - self.cdlms[c][i].bitsend;
                    let shift_r = 32 - self.cdlms[c][i].scaling - 2;
                    for j in 0..self.cdlms[c][i].coefsend {
                        let v = self.gb.get_bits(self.cdlms[c][i].bitsend as usize)? as u32;
                        self.cdlms[c][i].coefs[j] = (((v << shift_l) >> shift_r) as i32) as i16;
                    }
                }
            }
        }
        Ok(())
    }

    /// `decode_channel_residues` (wmalosslessdec.c).
    fn decode_channel_residues(&mut self, ch: usize, tile_size: usize) -> Result<()> {
        let mut i = 0usize;
        self.transient[ch] = self.gb.get_bits1()? != 0;
        if self.transient[ch] {
            self.transient_pos[ch] = self.gb.get_bits((32 - (tile_size as u32).leading_zeros() - 1) as usize)? as usize;
            if self.transient_pos[ch] != 0 {
                self.transient[ch] = false;
            }
            self.channel[ch].transient_counter =
                self.channel[ch].transient_counter.max(self.samples_per_frame as i32 / 2);
        } else if self.channel[ch].transient_counter != 0 {
            self.transient[ch] = true;
        }

        if self.seekable_tile {
            let ave_mean = self.gb.get_bits(self.bits_per_sample as usize)?;
            self.ave_sum[ch] = ave_mean << (self.movave_scaling + 1);
            let first_bits = if self.do_inter_ch_decorr {
                self.bits_per_sample as usize + 1
            } else {
                self.bits_per_sample as usize
            };
            self.channel_residues[ch][0] = self.gb.get_bits(first_bits)? as i32;
            i = 1;
        }
        while i < tile_size {
            let mut quo = 0u32;
            while self.gb.get_bits1()? != 0 {
                quo += 1;
                if self.gb.bits_left() == 0 {
                    return Err(Error::invalid("wmall: residue overread"));
                }
            }
            if quo >= 32 {
                let extra_bits = self.gb.get_bits(5)? + 1;
                quo += self.gb.get_bits(extra_bits as usize)?;
            }
            let ave_mean = (self.ave_sum[ch] + (1 << self.movave_scaling)) >> (self.movave_scaling + 1);
            let residue: u32 = if ave_mean <= 1 {
                quo
            } else {
                let rem_bits = 32 - (ave_mean as u32).leading_zeros();
                let rem = self.gb.get_bits(rem_bits as usize)?;
                (quo << rem_bits) + rem
            };
            self.ave_sum[ch] = self.ave_sum[ch]
                .wrapping_add(residue)
                .wrapping_sub(self.ave_sum[ch] >> self.movave_scaling);
            let signed_res = ((residue >> 1) as i32) ^ -((residue & 1) as i32);
            self.channel_residues[ch][i] = signed_res;
            i += 1;
        }
        Ok(())
    }

    /// `decode_lpc` (wmalosslessdec.c).
    fn decode_lpc(&mut self) -> Result<()> {
        self.lpc_order = self.gb.get_bits(5)? as usize + 1;
        self.lpc_scaling = self.gb.get_bits(4)? as i32;
        self.lpc_intbits = self.gb.get_bits(3)? as i32 + 1;
        let cbits = (self.lpc_scaling + self.lpc_intbits) as usize;
        for ch in 0..self.channels {
            for i in 0..self.lpc_order {
                self.lpc_coefs[ch][i] = self.gb.get_sbits(cbits)?;
            }
        }
        Ok(())
    }

    /// `clear_codec_buffers` (wmalosslessdec.c).
    fn clear_codec_buffers(&mut self) {
        self.acfilter_coeffs = [0; 16];
        self.acfilter_prevvalues = [[0; 16]; WMALL_MAX_CHANNELS];
        self.lpc_coefs = [[0; 40]; WMALL_MAX_CHANNELS];
        self.mclms_coeffs.iter_mut().for_each(|v| *v = 0);
        self.mclms_coeffs_cur.iter_mut().for_each(|v| *v = 0);
        self.mclms_prevvalues.iter_mut().for_each(|v| *v = 0);
        self.mclms_updates.iter_mut().for_each(|v| *v = 0);
        for ich in 0..self.channels {
            for ilms in 0..self.cdlms_ttl[ich] {
                self.cdlms[ich][ilms].coefs = [0; MAX_ORDER + WMALL_COEFF_PAD];
                self.cdlms[ich][ilms].lms_prevvalues = [0; MAX_ORDER * 2 + WMALL_COEFF_PAD];
                self.cdlms[ich][ilms].lms_updates = [0; MAX_ORDER * 2 + WMALL_COEFF_PAD];
            }
            self.ave_sum[ich] = 0;
        }
    }

    /// `reset_codec` (wmalosslessdec.c).
    fn reset_codec(&mut self) {
        self.mclms_recent = self.mclms_order * self.channels;
        for ich in 0..self.channels {
            for ilms in 0..self.cdlms_ttl[ich] {
                self.cdlms[ich][ilms].recent = self.cdlms[ich][ilms].order;
            }
            self.channel[ich].transient_counter = self.samples_per_frame as i32;
            self.transient[ich] = true;
            self.transient_pos[ich] = 0;
        }
    }

    /// `mclms_update` (wmalosslessdec.c).
    fn mclms_update(&mut self, icoef: usize, pred: &[i32; WMALL_MAX_CHANNELS]) {
        let order = self.mclms_order;
        let num_channels = self.channels;
        let range = 1i32 << (self.bits_per_sample - 1);

        for ich in 0..num_channels {
            let pred_error = self.channel_residues[ich][icoef].wrapping_sub(pred[ich]);
            if pred_error > 0 {
                for i in 0..order * num_channels {
                    self.mclms_coeffs[i + ich * order * num_channels] += self.mclms_updates[self.mclms_recent + i];
                }
                for j in 0..ich {
                    self.mclms_coeffs_cur[ich * num_channels + j] +=
                        wmasign(self.channel_residues[j][icoef]) as i16;
                }
            } else if pred_error < 0 {
                for i in 0..order * num_channels {
                    self.mclms_coeffs[i + ich * order * num_channels] -= self.mclms_updates[self.mclms_recent + i];
                }
                for j in 0..ich {
                    self.mclms_coeffs_cur[ich * num_channels + j] -=
                        wmasign(self.channel_residues[j][icoef]) as i16;
                }
            }
        }

        for ich in (0..num_channels).rev() {
            self.mclms_recent -= 1;
            self.mclms_prevvalues[self.mclms_recent] =
                clip_i32(self.channel_residues[ich][icoef], -range, range - 1);
            self.mclms_updates[self.mclms_recent] = wmasign(self.channel_residues[ich][icoef]) as i16;
        }

        if self.mclms_recent == 0 {
            let n = order * num_channels;
            for i in 0..n {
                self.mclms_prevvalues[n + i] = self.mclms_prevvalues[i];
                self.mclms_updates[n + i] = self.mclms_updates[i];
            }
            self.mclms_recent = num_channels * order;
        }
    }

    /// `mclms_predict` (wmalosslessdec.c).
    fn mclms_predict(&mut self, icoef: usize, pred: &mut [i32; WMALL_MAX_CHANNELS]) {
        let order = self.mclms_order;
        let num_channels = self.channels;
        for ich in 0..num_channels {
            pred[ich] = 0;
            if !self.is_channel_coded[ich] {
                continue;
            }
            for i in 0..order * num_channels {
                pred[ich] = pred[ich]
                    .wrapping_add(
                        self.mclms_prevvalues[i + self.mclms_recent]
                            .wrapping_mul(self.mclms_coeffs[i + order * num_channels * ich] as i32),
                    );
            }
            for i in 0..ich {
                pred[ich] = pred[ich].wrapping_add(
                    self.channel_residues[i][icoef]
                        .wrapping_mul(self.mclms_coeffs_cur[i + num_channels * ich] as i32),
                );
            }
            pred[ich] = pred[ich].wrapping_add((1i32 << self.mclms_scaling) >> 1);
            pred[ich] >>= self.mclms_scaling;
            self.channel_residues[ich][icoef] = self.channel_residues[ich][icoef].wrapping_add(pred[ich]);
        }
    }

    /// `revert_mclms` (wmalosslessdec.c).
    fn revert_mclms(&mut self, tile_size: usize) {
        let mut pred = [0i32; WMALL_MAX_CHANNELS];
        for icoef in 0..tile_size {
            self.mclms_predict(icoef, &mut pred);
            self.mclms_update(icoef, &pred);
        }
    }

    /// `use_high_update_speed` (wmalosslessdec.c).
    fn use_high_update_speed(&mut self, ich: usize) {
        for ilms in (0..self.cdlms_ttl[ich]).rev() {
            if self.update_speed[ich] == 16 {
                continue;
            }
            let recent = self.cdlms[ich][ilms].recent;
            if self.b_v3_rtm {
                for icoef in 0..self.cdlms[ich][ilms].order {
                    self.cdlms[ich][ilms].lms_updates[icoef + recent] =
                        self.cdlms[ich][ilms].lms_updates[icoef + recent].wrapping_mul(2);
                }
            } else {
                for icoef in 0..self.cdlms[ich][ilms].order {
                    self.cdlms[ich][ilms].lms_updates[icoef] =
                        self.cdlms[ich][ilms].lms_updates[icoef].wrapping_mul(2);
                }
            }
        }
        self.update_speed[ich] = 16;
    }

    /// `use_normal_update_speed` (wmalosslessdec.c).
    fn use_normal_update_speed(&mut self, ich: usize) {
        for ilms in (0..self.cdlms_ttl[ich]).rev() {
            if self.update_speed[ich] == 8 {
                continue;
            }
            let recent = self.cdlms[ich][ilms].recent;
            if self.b_v3_rtm {
                for icoef in 0..self.cdlms[ich][ilms].order {
                    self.cdlms[ich][ilms].lms_updates[icoef + recent] =
                        self.cdlms[ich][ilms].lms_updates[icoef + recent] / 2;
                }
            } else {
                for icoef in 0..self.cdlms[ich][ilms].order {
                    self.cdlms[ich][ilms].lms_updates[icoef] =
                        self.cdlms[ich][ilms].lms_updates[icoef] / 2;
                }
            }
        }
        self.update_speed[ich] = 8;
    }

    /// `lms_update` (wmalosslessdec.c CD_LMS macro, 32-bit prevvalues path).
    fn lms_update(&mut self, ich: usize, ilms: usize, input: i32) {
        let range = 1i32 << (self.bits_per_sample - 1);
        let order = self.cdlms[ich][ilms].order;
        let mut recent = self.cdlms[ich][ilms].recent;
        if recent != 0 {
            recent -= 1;
        } else {
            for i in 0..order {
                self.cdlms[ich][ilms].lms_prevvalues[order + i] = self.cdlms[ich][ilms].lms_prevvalues[i];
                self.cdlms[ich][ilms].lms_updates[order + i] = self.cdlms[ich][ilms].lms_updates[i];
            }
            recent = order - 1;
        }
        self.cdlms[ich][ilms].lms_prevvalues[recent] = clip_i32(input, -range, range - 1);
        self.cdlms[ich][ilms].lms_updates[recent] =
            (wmasign(input) * self.update_speed[ich]) as i16;
        self.cdlms[ich][ilms].lms_updates[recent + (order >> 4)] >>= 2;
        self.cdlms[ich][ilms].lms_updates[recent + (order >> 3)] >>= 1;
        self.cdlms[ich][ilms].recent = recent;
        for item in self.cdlms[ich][ilms].lms_updates[recent + order..].iter_mut() {
            *item = 0;
        }
    }

    /// `revert_cdlms` (wmalosslessdec.c, 32-bit variant; the 16-bit variant
    /// differs only in the prevvalues element type).
    fn revert_cdlms(&mut self, ch: usize, coef_begin: usize, coef_end: usize) {
        let num_lms = self.cdlms_ttl[ch];
        for ilms in (0..num_lms).rev() {
            for icoef in coef_begin..coef_end {
                let scaling = self.cdlms[ch][ilms].scaling;
                let order = self.cdlms[ch][ilms].order;
                let recent = self.cdlms[ch][ilms].recent;
                let residue = self.channel_residues[ch][icoef];
                // scalarproduct_and_madd_int32_c: res += v1[k]*v2[k];
                // v1[k] += mul*v3[k], over FFALIGN(order, 8) elements (the
                // arrays are zero-padded so the padded reads are zeros that
                // also get updated).
                let len = (order + 7) & !7;
                let mut pred = (1i32 << scaling) >> 1;
                let mul = wmasign(residue);
                {
                    let g = &mut self.cdlms[ch][ilms];
                    let mut res = 0i32;
                    for k in 0..len {
                        res = res.wrapping_add((g.coefs[k] as i32).wrapping_mul(g.lms_prevvalues[recent + k]));
                        g.coefs[k] = (g.coefs[k] as i32).wrapping_add(mul * g.lms_updates[recent + k] as i32) as i16;
                    }
                    pred = pred.wrapping_add(res);
                }
                let input = residue.wrapping_add(pred >> scaling);
                self.lms_update(ch, ilms, input);
                self.channel_residues[ch][icoef] = input;
            }
        }
    }
}

impl WmaLosslessDecoder {
    /// `revert_inter_ch_decorr` (wmalosslessdec.c).
    fn revert_inter_ch_decorr(&mut self, tile_size: usize) {
        if self.channels != 2 {
            return;
        }
        if self.is_channel_coded[0] || self.is_channel_coded[1] {
            for icoef in 0..tile_size {
                let r1 = self.channel_residues[1][icoef];
                let r0 = self.channel_residues[0][icoef];
                self.channel_residues[0][icoef] = r0.wrapping_sub(r1 >> 1);
                self.channel_residues[1][icoef] = r1.wrapping_add(self.channel_residues[0][icoef]);
            }
        }
    }

    /// `revert_acfilter` (wmalosslessdec.c).
    fn revert_acfilter(&mut self, tile_size: usize) {
        let scaling = self.acfilter_scaling;
        let order = self.acfilter_order;
        for ich in 0..self.channels {
            for i in 0..order {
                let mut pred = 0i32;
                for j in 0..order {
                    if i <= j {
                        pred = pred.wrapping_add(
                            self.acfilter_coeffs[j] as i32
                                * self.acfilter_prevvalues[ich][j - i],
                        );
                    } else {
                        pred = pred.wrapping_add(
                            self.channel_residues[ich][i - j - 1].wrapping_mul(self.acfilter_coeffs[j] as i32),
                        );
                    }
                }
                pred >>= scaling;
                self.channel_residues[ich][i] = self.channel_residues[ich][i].wrapping_add(pred);
            }
            for i in order..tile_size {
                let mut pred = 0i32;
                for j in 0..order {
                    pred = pred.wrapping_add(
                        self.channel_residues[ich][i - j - 1].wrapping_mul(self.acfilter_coeffs[j] as i32),
                    );
                }
                pred >>= scaling;
                self.channel_residues[ich][i] = self.channel_residues[ich][i].wrapping_add(pred);
            }
            for j in (0..order).rev() {
                let pv = &mut self.acfilter_prevvalues[ich];
                if tile_size <= j {
                    pv[j] = pv[j - tile_size];
                } else {
                    pv[j] = self.channel_residues[ich][tile_size - j - 1];
                }
            }
        }
    }

    /// `decode_subframe` (wmalosslessdec.c).
    fn decode_subframe(&mut self) -> Result<bool> {
        let mut offset = self.samples_per_frame;
        let mut subframe_len = self.samples_per_frame;
        let mut total_samples = self.samples_per_frame * self.channels;

        self.subframe_offset = self.gb.bit_pos();

        for i in 0..self.channels {
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

        self.seekable_tile = self.gb.get_bits1()? != 0;
        if self.seekable_tile {
            self.clear_codec_buffers();
            self.do_arith_coding = self.gb.get_bits1()? != 0;
            if self.do_arith_coding {
                return Err(Error::unsupported("wmall: arithmetic coding"));
            }
            self.do_ac_filter = self.gb.get_bits1()? != 0;
            self.do_inter_ch_decorr = self.gb.get_bits1()? != 0;
            self.do_mclms = self.gb.get_bits1()? != 0;

            if self.do_ac_filter {
                self.decode_ac_filter()?;
            }
            if self.do_mclms {
                self.decode_mclms()?;
            }
            self.decode_cdlms()?;
            self.movave_scaling = self.gb.get_bits(3)? as i32;
            self.quant_stepsize = self.gb.get_bits(8)? as i32 + 1;
            self.reset_codec();
        }

        let rawpcm_tile = self.gb.get_bits1()? != 0;
        if !rawpcm_tile && self.cdlms[0][0].order == 0 {
            // waiting for seekable tile: FFmpeg returns an error and drops
            // the frame; the stream resyncs at the next seekable tile.
            return Err(Error::invalid("wmall: waiting for seekable tile"));
        }

        for i in 0..self.channels {
            self.is_channel_coded[i] = true;
        }

        if !rawpcm_tile {
            for i in 0..self.channels {
                self.is_channel_coded[i] = self.gb.get_bits1()? != 0;
            }
            self.do_lpc = false;
            if self.b_v3_rtm {
                self.do_lpc = self.gb.get_bits1()? != 0;
                if self.do_lpc {
                    self.decode_lpc()?;
                }
            }
        }

        if self.gb.bits_left() < 1 {
            return Err(Error::invalid("wmall: no bits left"));
        }

        let padding_zeroes = if self.gb.get_bits1()? != 0 {
            self.gb.get_bits(5)? as u32
        } else {
            0
        };

        if rawpcm_tile {
            let bits = self.bits_per_sample.saturating_sub(padding_zeroes);
            if bits == 0 {
                return Err(Error::invalid("wmall: invalid padding bits in raw PCM tile"));
            }
            for i in 0..self.channels {
                for j in 0..subframe_len {
                    self.channel_residues[i][j] = self.gb.get_sbits(bits as usize)?;
                }
            }
        } else {
            if self.bits_per_sample < padding_zeroes {
                return Err(Error::invalid("wmall: padding_zeroes > bits_per_sample"));
            }
            for i in 0..self.channels {
                if self.is_channel_coded[i] {
                    self.decode_channel_residues(i, subframe_len)?;
                    if self.seekable_tile {
                        self.use_high_update_speed(i);
                    } else {
                        self.use_normal_update_speed(i);
                    }
                    self.revert_cdlms(i, 0, subframe_len);
                } else {
                    for v in self.channel_residues[i][..subframe_len].iter_mut() {
                        *v = 0;
                    }
                }
            }

            if self.do_mclms {
                self.revert_mclms(subframe_len);
            }
            if self.do_inter_ch_decorr {
                self.revert_inter_ch_decorr(subframe_len);
            }
            if self.do_ac_filter {
                self.revert_acfilter(subframe_len);
            }

            if self.quant_stepsize != 1 {
                for i in 0..self.channels {
                    for v in self.channel_residues[i][..subframe_len].iter_mut() {
                        *v = v.wrapping_mul(self.quant_stepsize);
                    }
                }
            }
        }

        // Write to the output buffer depending on bit depth
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let sl = self.channel[c].subframe_len[self.channel[c].cur_subframe];
            let base = self.channel[c].subframe_offsets[self.channel[c].cur_subframe];
            for j in 0..sl {
                let v = if self.bits_per_sample == 16 {
                    ((self.channel_residues[c][j] as i16 as i32) << padding_zeroes) as i32
                } else {
                    self.channel_residues[c][j].wrapping_mul((256u32 << padding_zeroes) as i32)
                };
                self.out[c][base + j] = v;
            }
        }

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            if self.channel[c].cur_subframe >= self.channel[c].num_subframes {
                return Err(Error::invalid("wmall: broken subframe"));
            }
            self.channel[c].cur_subframe += 1;
        }
        Ok(true)
    }

    /// `decode_frame` (wmalosslessdec.c). Returns `more_frames`.
    fn decode_frame(&mut self) -> Result<bool> {
        let mut len = 0usize;

        if self.len_prefix {
            len = self.gb.get_bits(self.log2_frame_size as usize)? as usize;
        }

        match self.decode_tilehdr() {
            Ok(()) => {}
            Err(_) => {
                self.packet_loss = true;
            }
        }

        if self.dynamic_range_compression {
            self.drc_gain = self.gb.get_bits(8)? as u8;
        }

        if self.gb.get_bits1()? != 0 {
            let bits = 32 - ((self.samples_per_frame * 2) as u32).leading_zeros() - 1;
            if self.gb.get_bits1()? != 0 {
                let _start_skip = self.gb.get_bits(bits as usize)?;
            }
            if self.gb.get_bits1()? != 0 {
                let end_skip = self.gb.get_bits(bits as usize)? as usize;
                if end_skip >= self.samples_per_frame {
                    return Err(Error::invalid("wmall: end skip >= frame"));
                }
                // handled at output time below
                self.trim_end = end_skip;
            }
        }

        self.parsed_all_subframes = false;
        for i in 0..self.channels {
            self.channel[i].decoded_samples = 0;
            self.channel[i].cur_subframe = 0;
        }

        while !self.parsed_all_subframes {
            match self.decode_subframe() {
                Ok(true) => {}
                Ok(false) => {
                    return Ok(false);
                }
                Err(_) => {
                    self.packet_loss = true;
                }
            }
        }
        self.skip_frame = false;

        if self.len_prefix {
            if len != (self.gb.bit_pos() - self.frame_offset) + 2 {
                self.packet_loss = true;
            }
            let skip = len - (self.gb.bit_pos() - self.frame_offset) - 1;
            if skip > 0 {
                let _ = self.gb.skip_bits(skip);
            }
        }

        // decode trailer bit (more_frames)
        let more_frames = self.gb.get_bits1()? != 0;

        // assemble the output frame (planar)
        let trim_end = self.trim_end.min(self.samples_per_frame);
        let mut frame = AudioFrame {
            samples: (self.samples_per_frame - trim_end) as u32,
            pts: None,
            data: Vec::with_capacity(self.channels),
        };
        for c in 0..self.channels {
            let mut plane = Vec::with_capacity((self.samples_per_frame - trim_end) * 4);
            for &v in &self.out[c][..self.samples_per_frame - trim_end] {
                plane.extend_from_slice(&v.to_le_bytes());
            }
            frame.data.push(plane);
        }
        if self.skip_frame {
            // consumed by packet logic; frame not emitted
        } else {
            self.pending.push(frame);
        }
        Ok(more_frames)
    }

    /// Bit reservoir: append-only. `gb` keeps the read cursor across saves;
    /// consumed prefix bytes are compacted when large.
    fn save_bits(&mut self, gb: &mut BitReader<'_>, len: usize, append: bool) {
        if len == 0 {
            return;
        }
        if !append {
            self.frame_offset = gb.bits_count() & 7;
            self.num_saved_bits = self.frame_offset;
            self.frame_data.clear();
        }
        let buflen = (self.num_saved_bits + len + 8) >> 3;
        if buflen > self.max_frame_size {
            self.packet_loss = true;
            self.num_saved_bits = 0;
            return;
        }
        let start_bit = self.num_saved_bits;
        let new_bytes = ((start_bit + len + 7) >> 3).max(1);
        if self.frame_data.len() < new_bytes {
            self.frame_data.resize(new_bytes, 0);
        }
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
        self.num_saved_bits += len;
        self.gb = OwnedBitReader::from_bits(self.frame_data.clone(), self.num_saved_bits);
        let _ = self.gb.skip_bits(self.frame_offset);
    }


    /// `decode_packet` (wmalosslessdec.c). Each ASF packet carries exactly
    /// one codec packet of `block_align` bytes with its own 23-bit header:
    /// seq, splicing flag, and the number of bits of the previous frame
    /// spilled into this packet. The reservoir is append-only; `gb` keeps
    /// the read cursor so frames decode sequentially across packets.
    fn decode_packet_chunk(&mut self, cur_data: &[u8]) -> Result<usize> {
        let mut buf_size = cur_data.len();
        if buf_size == 0 {
            self.packet_done = false;
            if self.num_saved_bits <= self.gb.bit_pos() {
                return Ok(0);
            }
            if !self.decode_frame()? {
                self.num_saved_bits = 0;
            }
            return Ok(0);
        }

        let mut gb;
        if self.packet_done || self.packet_loss {
            self.packet_done = false;

            self.next_packet_start = buf_size - block_align(self).min(buf_size);
            buf_size = block_align(self).min(buf_size);
            self.buf_bit_size = buf_size << 3;

            gb = BitReader::with_bit_len(&cur_data[..buf_size], self.buf_bit_size);
            let packet_sequence_number = gb.get_bits(4)? as u8;
            gb.skip_bits(1)?; // seekable_frame_in_packet
            let spliced_packet = gb.get_bits1()? != 0;
            if spliced_packet {
                return Err(Error::unsupported("wmall: bitstream splicing"));
            }

            let num_bits_prev_frame = gb.get_bits(self.log2_frame_size as usize)? as usize;

            if !self.packet_loss
                && ((self.packet_sequence_number as u32 + 1) & 0xF) != packet_sequence_number as u32
            {
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
                if nb < remaining_packet_bits && !self.packet_loss {
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
                return Err(Error::invalid("wmall: packet too small"));
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
            self.packet_loss = true;
        }

        if self.packet_done && !self.packet_loss && remaining > 0 {
            self.save_bits(&mut gb, remaining as usize, false);
        }

        self.packet_offset = gb.bits_count() & 7;
        if self.packet_loss {
            return Err(Error::invalid("wmall: packet loss"));
        }
        let consumed = (gb.bits_count() >> 3) + self.next_packet_start;
        self.next_packet_start = 0;
        Ok(consumed)
    }

    fn decode_packet_impl(&mut self, data: &[u8]) -> Result<()> {
        let mut cur = data;
        while !cur.is_empty() {
            let consumed = self.decode_packet_chunk(cur)?;
            if consumed == 0 || consumed >= cur.len() {
                break;
            }
            cur = &cur[consumed..];
        }
        Ok(())
    }

}

fn block_align(s: &WmaLosslessDecoder) -> usize {
    s.block_align_stored
}

impl Decoder for WmaLosslessDecoder {
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
        self.packet_loss = true;
        self.packet_done = false;
        self.num_saved_bits = 0;
        self.frame_offset = 0;
        self.next_packet_start = 0;
        self.cdlms[0][0].order = 0;
        self.pending.clear();
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.flush()
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        let sample_format = if self.bits_per_sample == 16 {
            SampleFormat::S16P
        } else {
            SampleFormat::S32P
        };
        Some(oxideav_core::AudioFormat {
            sample_format,
            sample_rate: self.sample_rate,
            channels: self.channels as u16,
        })
    }
}

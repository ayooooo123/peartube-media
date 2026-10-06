// Ported from FFmpeg (commit 2da55bf): libavcodec/wmalosslessdec.c and
// libavcodec/lossless_audiodsp.c (scalarproduct_and_madd_int16/32_c), with
// wma_common.c's frame-length helper.
// GNU Lesser General Public License 2.1 or later.

//! WMA Lossless decoder (`wmalossless`).
//!
//! The bitstream side follows FFmpeg's checked reader: reads past a frame's
//! saved bits return whatever the reservoir buffer holds beyond them, and
//! the position stops 8 bits past the end. A truncated final frame thus
//! decodes from the reservoir's leftover bytes exactly as FFmpeg does, so
//! the reservoir is one persistent buffer that only `save_bits` writes.

use crate::decode_loop::{Call, DecodeCallback, DecodeLoop};
use crate::getbits::{GetBits, GetBitsState, PutBits};
use crate::wma_common::{av_ceil_log2, av_log2, wma_get_frame_len_bits};
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

const WMALL_MAX_CHANNELS: usize = 8;
const MAX_SUBFRAMES: usize = 32;
const MAX_FRAMESIZE: usize = 32768;
const MAX_ORDER: usize = 256;
const WMALL_BLOCK_MAX_BITS: u32 = 14;
const WMALL_BLOCK_MAX_SIZE: usize = 1 << WMALL_BLOCK_MAX_BITS;
/// `WMALL_COEFF_PAD_SIZE` (16 bytes) in int16 elements.
const COEFF_PAD: usize = 8;
const AV_INPUT_BUFFER_PADDING_SIZE: usize = 64;

/// `WMASIGN`.
#[inline]
fn wmasign(x: i32) -> i32 {
    (x > 0) as i32 - (x < 0) as i32
}

/// `av_clip`.
#[inline]
fn av_clip(a: i32, amin: i32, amax: i32) -> i32 {
    if a < amin {
        amin
    } else if a > amax {
        amax
    } else {
        a
    }
}

/// `FFALIGN`.
#[inline]
fn ffalign(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// `WmallChannelCtx`.
#[derive(Clone, Copy, Default)]
struct ChannelCtx {
    num_subframes: usize,
    subframe_len: [u16; MAX_SUBFRAMES],
    subframe_offsets: [u16; MAX_SUBFRAMES],
    cur_subframe: usize,
    decoded_samples: usize,
    transient_counter: i32,
}

impl ChannelCtx {
    /// `subframe_len[i]`, 0 past the array (FFmpeg reads the next field).
    fn len_at(&self, i: usize) -> usize {
        self.subframe_len.get(i).copied().unwrap_or(0) as usize
    }
}

/// One cascaded LMS filter (`WmallDecodeCtx.cdlms[ch][i]`).
#[derive(Clone)]
struct Cdlms {
    order: usize,
    scaling: u32,
    coefs: [i16; MAX_ORDER + COEFF_PAD],
    lms_prevvalues: [i32; MAX_ORDER * 2 + COEFF_PAD],
    lms_updates: [i16; MAX_ORDER * 2 + COEFF_PAD],
    recent: usize,
}

impl Default for Cdlms {
    fn default() -> Self {
        Self {
            order: 0,
            scaling: 0,
            coefs: [0; MAX_ORDER + COEFF_PAD],
            lms_prevvalues: [0; MAX_ORDER * 2 + COEFF_PAD],
            lms_updates: [0; MAX_ORDER * 2 + COEFF_PAD],
            recent: 0,
        }
    }
}

/// WMA Lossless decoder (FFmpeg's `wmalossless`, `WmallDecodeCtx`).
pub struct WmaLosslessDecoder {
    codec_id: CodecId,
    sample_rate: u32,
    block_align: usize,

    // frame size dependent frame information (set during initialization)
    len_prefix: bool,
    dynamic_range_compression: bool,
    bits_per_sample: u32,
    samples_per_frame: usize,
    log2_frame_size: u32,
    num_channels: usize,
    max_num_subframes: usize,
    min_samples_per_subframe: usize,
    max_frame_size: usize,
    frame_data: Vec<u8>,
    pb: PutBits,

    // packet decode state
    /// `s->pgb`: position of the packet reader, kept for draining calls.
    pgb: GetBitsState,
    next_packet_start: usize,
    packet_offset: usize,
    packet_sequence_number: u32,
    num_saved_bits: usize,
    frame_offset: usize,
    packet_loss: bool,
    packet_done: bool,

    // frame decode state
    /// `s->gb`: the reader over the reservoir in `frame_data`.
    gb: GetBitsState,
    buf_bit_size: i64,
    parsed_all_subframes: bool,

    // subframe/block decode state
    channels_for_cur_subframe: usize,
    channel_indexes_for_cur_subframe: [usize; WMALL_MAX_CHANNELS],
    channel: [ChannelCtx; WMALL_MAX_CHANNELS],

    do_ac_filter: bool,
    do_inter_ch_decorr: bool,
    do_mclms: bool,

    acfilter_order: usize,
    acfilter_scaling: u32,
    acfilter_coeffs: [i16; 16],
    acfilter_prevvalues: [[i32; 16]; WMALL_MAX_CHANNELS],

    mclms_order: usize,
    mclms_scaling: u32,
    mclms_coeffs: [i16; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS * 32],
    mclms_coeffs_cur: [i16; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS],
    mclms_prevvalues: [i32; WMALL_MAX_CHANNELS * 2 * 32],
    mclms_updates: [i32; WMALL_MAX_CHANNELS * 2 * 32],
    mclms_recent: usize,

    movave_scaling: u32,
    quant_stepsize: u32,

    cdlms: Vec<[Cdlms; 9]>,
    cdlms_ttl: [usize; WMALL_MAX_CHANNELS],

    b_v3_rtm: bool,

    is_channel_coded: [bool; WMALL_MAX_CHANNELS],
    update_speed: [i32; WMALL_MAX_CHANNELS],

    transient: [bool; WMALL_MAX_CHANNELS],
    transient_pos: [u32; WMALL_MAX_CHANNELS],
    seekable_tile: bool,

    ave_sum: [u32; WMALL_MAX_CHANNELS],

    channel_residues: Vec<[i32; WMALL_BLOCK_MAX_SIZE]>,

    lpc_coefs: [[i32; 40]; WMALL_MAX_CHANNELS],
    lpc_order: usize,

    // `s->frame`: the output frame being decoded
    out: Vec<Vec<i32>>,
    out_pos: [usize; WMALL_MAX_CHANNELS],
    nb_samples: i64,

    lp: DecodeLoop,
}

impl WmaLosslessDecoder {
    /// `decode_init`.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|&b| b > 0 && b <= 1 << 21)
            .ok_or_else(|| Error::invalid("wmalossless: block_align is not set or invalid"))?
            as usize;

        let extradata = &params.extradata;
        if extradata.len() < 18 {
            return Err(Error::unsupported("wmalossless: unsupported extradata size"));
        }
        let rl16 = |p: usize| u16::from_le_bytes([extradata[p], extradata[p + 1]]) as u32;
        let decode_flags = rl16(14);
        let channel_mask =
            u32::from_le_bytes([extradata[2], extradata[3], extradata[4], extradata[5]]);
        let bits_per_sample = rl16(0);
        if bits_per_sample != 16 && bits_per_sample != 24 {
            return Err(Error::invalid(format!("wmalossless: unknown bit-depth {bits_per_sample}")));
        }

        let num_channels =
            if channel_mask != 0 { channel_mask.count_ones() as usize } else { params.channels.unwrap_or(0) as usize };
        if num_channels > WMALL_MAX_CHANNELS {
            return Err(Error::unsupported("wmalossless: more than 8 channels"));
        }
        if num_channels == 0 {
            return Err(Error::invalid("wmalossless: no channels"));
        }

        let max_frame_size = MAX_FRAMESIZE * num_channels;
        let sample_rate = params.sample_rate.unwrap_or(0);
        let samples_per_frame = 1usize << wma_get_frame_len_bits(sample_rate, 3, decode_flags);
        let log2_max_num_subframes = (decode_flags & 0x38) >> 3;
        let max_num_subframes = 1usize << log2_max_num_subframes;
        if max_num_subframes > MAX_SUBFRAMES {
            return Err(Error::invalid(format!("wmalossless: invalid number of subframes {max_num_subframes}")));
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate,
            block_align,
            len_prefix: decode_flags & 0x40 != 0,
            dynamic_range_compression: decode_flags & 0x80 != 0,
            bits_per_sample,
            samples_per_frame,
            log2_frame_size: av_log2(block_align as u32) + 4,
            num_channels,
            max_num_subframes,
            min_samples_per_subframe: samples_per_frame / max_num_subframes,
            max_frame_size,
            frame_data: vec![0; max_frame_size + AV_INPUT_BUFFER_PADDING_SIZE],
            pb: PutBits::default(),
            pgb: GetBitsState::default(),
            next_packet_start: 0,
            packet_offset: 0,
            packet_sequence_number: 0,
            num_saved_bits: 0,
            frame_offset: 0,
            packet_loss: true,
            packet_done: false,
            gb: GetBitsState::default(),
            buf_bit_size: 0,
            parsed_all_subframes: false,
            channels_for_cur_subframe: 0,
            channel_indexes_for_cur_subframe: [0; WMALL_MAX_CHANNELS],
            channel: [ChannelCtx::default(); WMALL_MAX_CHANNELS],
            do_ac_filter: false,
            do_inter_ch_decorr: false,
            do_mclms: false,
            acfilter_order: 0,
            acfilter_scaling: 0,
            acfilter_coeffs: [0; 16],
            acfilter_prevvalues: [[0; 16]; WMALL_MAX_CHANNELS],
            mclms_order: 0,
            mclms_scaling: 0,
            mclms_coeffs: [0; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS * 32],
            mclms_coeffs_cur: [0; WMALL_MAX_CHANNELS * WMALL_MAX_CHANNELS],
            mclms_prevvalues: [0; WMALL_MAX_CHANNELS * 2 * 32],
            mclms_updates: [0; WMALL_MAX_CHANNELS * 2 * 32],
            mclms_recent: 0,
            movave_scaling: 0,
            quant_stepsize: 0,
            cdlms: vec![Default::default(); WMALL_MAX_CHANNELS],
            cdlms_ttl: [0; WMALL_MAX_CHANNELS],
            b_v3_rtm: decode_flags & 0x100 != 0,
            is_channel_coded: [false; WMALL_MAX_CHANNELS],
            update_speed: [0; WMALL_MAX_CHANNELS],
            transient: [false; WMALL_MAX_CHANNELS],
            transient_pos: [0; WMALL_MAX_CHANNELS],
            seekable_tile: false,
            ave_sum: [0; WMALL_MAX_CHANNELS],
            channel_residues: vec![[0; WMALL_BLOCK_MAX_SIZE]; WMALL_MAX_CHANNELS],
            lpc_coefs: [[0; 40]; WMALL_MAX_CHANNELS],
            lpc_order: 0,
            out: vec![vec![0; samples_per_frame]; num_channels],
            out_pos: [0; WMALL_MAX_CHANNELS],
            nb_samples: 0,
            // AV_CODEC_CAP_DELAY: frames wait in the reservoir
            lp: DecodeLoop::new(true),
        })
    }

    /// `decode_subframe_length`.
    fn decode_subframe_length(&self, gb: &mut GetBits<'_>, offset: usize) -> Option<usize> {
        // no need to read from the bitstream when only one length is possible
        if offset == self.samples_per_frame - self.min_samples_per_subframe {
            return Some(self.min_samples_per_subframe);
        }

        let len = av_log2(self.max_num_subframes as u32 - 1) + 1;
        let frame_len_ratio = gb.get_bits(len) as usize;
        let subframe_len = self.min_samples_per_subframe * (frame_len_ratio + 1);

        // sanity check the length
        (subframe_len >= self.min_samples_per_subframe && subframe_len <= self.samples_per_frame)
            .then_some(subframe_len)
    }

    /// `decode_tilehdr`: how the frame splits into subframes per channel.
    fn decode_tilehdr(&mut self, gb: &mut GetBits<'_>) -> Option<()> {
        let nch = self.num_channels;
        let mut num_samples = [0usize; WMALL_MAX_CHANNELS];
        let mut contains_subframe = [false; WMALL_MAX_CHANNELS];
        let mut channels_for_cur_subframe = nch;
        let mut min_channel_len = 0;

        // reset tiling information
        for ch in self.channel[..nch].iter_mut() {
            ch.num_subframes = 0;
        }

        let tile_aligned = gb.get_bits1() != 0;
        let fixed_channel_layout = self.max_num_subframes == 1 || tile_aligned;

        // loop until the frame data is split between the subframes
        loop {
            let mut in_use = false;

            // check which channels contain the subframe
            for c in 0..nch {
                if num_samples[c] == min_channel_len {
                    contains_subframe[c] = if fixed_channel_layout
                        || channels_for_cur_subframe == 1
                        || min_channel_len == self.samples_per_frame - self.min_samples_per_subframe
                    {
                        true
                    } else {
                        gb.get_bits1() != 0
                    };
                    in_use |= contains_subframe[c];
                } else {
                    contains_subframe[c] = false;
                }
            }

            if !in_use {
                return None;
            }

            // get subframe length, subframe_len == 0 is not allowed
            let subframe_len = self.decode_subframe_length(gb, min_channel_len)?;
            // add subframes to the individual channels and find new
            // min_channel_len
            min_channel_len += subframe_len;
            for c in 0..nch {
                let chan = &mut self.channel[c];
                if contains_subframe[c] {
                    if chan.num_subframes >= MAX_SUBFRAMES {
                        return None;
                    }
                    chan.subframe_len[chan.num_subframes] = subframe_len as u16;
                    num_samples[c] += subframe_len;
                    chan.num_subframes += 1;
                    if num_samples[c] > self.samples_per_frame {
                        return None;
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

        for chan in self.channel[..nch].iter_mut() {
            let mut offset = 0u16;
            for i in 0..chan.num_subframes {
                chan.subframe_offsets[i] = offset;
                offset += chan.subframe_len[i];
            }
        }

        Some(())
    }

    /// `decode_ac_filter`.
    fn decode_ac_filter(&mut self, gb: &mut GetBits<'_>) {
        self.acfilter_order = gb.get_bits(4) as usize + 1;
        self.acfilter_scaling = gb.get_bits(4);

        for i in 0..self.acfilter_order {
            self.acfilter_coeffs[i] = (gb.get_bitsz(self.acfilter_scaling) + 1) as i16;
        }
    }

    /// `decode_mclms`.
    fn decode_mclms(&mut self, gb: &mut GetBits<'_>) {
        self.mclms_order = (gb.get_bits(4) as usize + 1) * 2;
        self.mclms_scaling = gb.get_bits(4);
        if gb.get_bits1() != 0 {
            let mut cbits = av_log2(self.mclms_scaling + 1);
            if 1 << cbits < self.mclms_scaling + 1 {
                cbits += 1;
            }

            let send_coef_bits = gb.get_bitsz(cbits) + 2;

            let nch = self.num_channels;
            for c in self.mclms_coeffs[..self.mclms_order * nch * nch].iter_mut() {
                *c = gb.get_bits(send_coef_bits) as i16;
            }

            for i in 0..nch {
                for c in 0..i {
                    self.mclms_coeffs_cur[i * nch + c] = gb.get_bits(send_coef_bits) as i16;
                }
            }
        }
    }

    /// `decode_cdlms`.
    fn decode_cdlms(&mut self, gb: &mut GetBits<'_>) -> Option<()> {
        let cdlms_send_coef = gb.get_bits1() != 0;

        for c in 0..self.num_channels {
            self.cdlms_ttl[c] = gb.get_bits(3) as usize + 1;
            for i in 0..self.cdlms_ttl[c] {
                self.cdlms[c][i].order = (gb.get_bits(7) as usize + 1) * 8;
                if self.cdlms[c][i].order > MAX_ORDER {
                    self.cdlms[0][0].order = 0;
                    return None;
                }
            }

            for i in 0..self.cdlms_ttl[c] {
                self.cdlms[c][i].scaling = gb.get_bits(4);
            }

            if cdlms_send_coef {
                for i in 0..self.cdlms_ttl[c] {
                    let lms = &mut self.cdlms[c][i];
                    let mut cbits = av_log2(lms.order as u32);
                    if 1 << cbits < lms.order {
                        cbits += 1;
                    }
                    let coefsend = gb.get_bits(cbits) as usize + 1;

                    let mut cbits = av_log2(lms.scaling + 1);
                    if 1 << cbits < lms.scaling + 1 {
                        cbits += 1;
                    }

                    let bitsend = gb.get_bitsz(cbits) + 2;
                    let shift_l = 32 - bitsend;
                    let shift_r = 32 - lms.scaling - 2;
                    for coef in lms.coefs[..coefsend].iter_mut() {
                        *coef = ((gb.get_bits(bitsend) << shift_l) >> shift_r) as i16;
                    }
                }
            }

            for i in 0..self.cdlms_ttl[c] {
                let lms = &mut self.cdlms[c][i];
                let order = lms.order;
                lms.coefs[order..order + COEFF_PAD].fill(0);
            }
        }

        Some(())
    }

    /// `decode_channel_residues`; false when the bits run out.
    fn decode_channel_residues(&mut self, gb: &mut GetBits<'_>, ch: usize, tile_size: usize) -> bool {
        let mut i = 0;
        self.transient[ch] = gb.get_bits1() != 0;
        if self.transient[ch] {
            self.transient_pos[ch] = gb.get_bits(av_log2(tile_size as u32));
            if self.transient_pos[ch] != 0 {
                self.transient[ch] = false;
            }
            self.channel[ch].transient_counter =
                self.channel[ch].transient_counter.max(self.samples_per_frame as i32 / 2);
        } else if self.channel[ch].transient_counter != 0 {
            self.transient[ch] = true;
        }

        if self.seekable_tile {
            let ave_mean = gb.get_bits(self.bits_per_sample);
            self.ave_sum[ch] = ave_mean << (self.movave_scaling + 1);
        }

        if self.seekable_tile {
            let bits = if self.do_inter_ch_decorr { self.bits_per_sample + 1 } else { self.bits_per_sample };
            self.channel_residues[ch][0] = gb.get_sbits(bits);
            i += 1;
        }
        while i < tile_size {
            let mut quo = 0u32;
            while gb.get_bits1() != 0 {
                quo = quo.wrapping_add(1);
                if gb.bits_left() <= 0 {
                    return false;
                }
            }
            if quo >= 32 {
                let n = gb.get_bits(5) + 1;
                quo = quo.wrapping_add(gb.get_bits(n));
            }

            let ave_mean =
                self.ave_sum[ch].wrapping_add(1 << self.movave_scaling) >> (self.movave_scaling + 1);
            let residue = if ave_mean <= 1 {
                quo
            } else {
                let rem_bits = av_ceil_log2(ave_mean);
                let rem = gb.get_bits(rem_bits);
                (quo << rem_bits).wrapping_add(rem)
            };

            self.ave_sum[ch] =
                residue.wrapping_add(self.ave_sum[ch]).wrapping_sub(self.ave_sum[ch] >> self.movave_scaling);

            self.channel_residues[ch][i] = ((residue >> 1) ^ (residue & 1).wrapping_neg()) as i32;
            i += 1;
        }

        true
    }

    /// `decode_lpc` (FFmpeg reads but does not apply the coefficients).
    fn decode_lpc(&mut self, gb: &mut GetBits<'_>) {
        self.lpc_order = gb.get_bits(5) as usize + 1;
        let lpc_scaling = gb.get_bits(4);
        let lpc_intbits = gb.get_bits(3) + 1;
        let cbits = lpc_scaling + lpc_intbits;
        for ch in 0..self.num_channels {
            for i in 0..self.lpc_order {
                self.lpc_coefs[ch][i] = gb.get_sbits(cbits);
            }
        }
    }

    /// `clear_codec_buffers`.
    fn clear_codec_buffers(&mut self) {
        self.acfilter_coeffs = [0; 16];
        self.acfilter_prevvalues = [[0; 16]; WMALL_MAX_CHANNELS];
        self.lpc_coefs = [[0; 40]; WMALL_MAX_CHANNELS];

        self.mclms_coeffs.fill(0);
        self.mclms_coeffs_cur.fill(0);
        self.mclms_prevvalues.fill(0);
        self.mclms_updates.fill(0);

        for ich in 0..self.num_channels {
            for lms in self.cdlms[ich][..self.cdlms_ttl[ich]].iter_mut() {
                lms.coefs.fill(0);
                lms.lms_prevvalues.fill(0);
                lms.lms_updates.fill(0);
            }
            self.ave_sum[ich] = 0;
        }
    }

    /// `reset_codec`: filter parameters and transient area at a new
    /// seekable tile.
    fn reset_codec(&mut self) {
        self.mclms_recent = self.mclms_order * self.num_channels;
        for ich in 0..self.num_channels {
            for lms in self.cdlms[ich][..self.cdlms_ttl[ich]].iter_mut() {
                lms.recent = lms.order;
            }
            // first sample of a seekable subframe is considered as the
            // starting of a transient area which is samples_per_frame
            // samples long
            self.channel[ich].transient_counter = self.samples_per_frame as i32;
            self.transient[ich] = true;
            self.transient_pos[ich] = 0;
        }
    }

    /// `mclms_update`.
    fn mclms_update(&mut self, icoef: usize, pred: &[i32; WMALL_MAX_CHANNELS]) {
        let order = self.mclms_order;
        let nch = self.num_channels;
        let range = 1i32 << (self.bits_per_sample - 1);
        let n = order * nch;

        for ich in 0..nch {
            let pred_error = self.channel_residues[ich][icoef].wrapping_sub(pred[ich]);
            if pred_error > 0 {
                for i in 0..n {
                    let c = &mut self.mclms_coeffs[i + ich * n];
                    *c = (*c as i32).wrapping_add(self.mclms_updates[self.mclms_recent + i]) as i16;
                }
                for j in 0..ich {
                    let c = &mut self.mclms_coeffs_cur[ich * nch + j];
                    *c = (*c as i32 + wmasign(self.channel_residues[j][icoef])) as i16;
                }
            } else if pred_error < 0 {
                for i in 0..n {
                    let c = &mut self.mclms_coeffs[i + ich * n];
                    *c = (*c as i32).wrapping_sub(self.mclms_updates[self.mclms_recent + i]) as i16;
                }
                for j in 0..ich {
                    let c = &mut self.mclms_coeffs_cur[ich * nch + j];
                    *c = (*c as i32 - wmasign(self.channel_residues[j][icoef])) as i16;
                }
            }
        }

        for ich in (0..nch).rev() {
            self.mclms_recent -= 1;
            self.mclms_prevvalues[self.mclms_recent] = av_clip(self.channel_residues[ich][icoef], -range, range - 1);
            self.mclms_updates[self.mclms_recent] = wmasign(self.channel_residues[ich][icoef]);
        }

        if self.mclms_recent == 0 {
            self.mclms_prevvalues.copy_within(..n, n);
            self.mclms_updates.copy_within(..n, n);
            self.mclms_recent = n;
        }
    }

    /// `mclms_predict`.
    fn mclms_predict(&mut self, icoef: usize, pred: &mut [i32; WMALL_MAX_CHANNELS]) {
        let order = self.mclms_order;
        let nch = self.num_channels;
        let n = order * nch;

        for ich in 0..nch {
            pred[ich] = 0;
            if !self.is_channel_coded[ich] {
                continue;
            }
            let mut p = 0u32;
            for i in 0..n {
                p = p.wrapping_add(
                    (self.mclms_prevvalues[i + self.mclms_recent] as u32)
                        .wrapping_mul(self.mclms_coeffs[i + n * ich] as i32 as u32),
                );
            }
            for i in 0..ich {
                p = p.wrapping_add(
                    (self.channel_residues[i][icoef] as u32)
                        .wrapping_mul(self.mclms_coeffs_cur[i + nch * ich] as i32 as u32),
                );
            }
            p = p.wrapping_add((1u32 << self.mclms_scaling) >> 1);
            pred[ich] = (p as i32) >> self.mclms_scaling;
            self.channel_residues[ich][icoef] = self.channel_residues[ich][icoef].wrapping_add(pred[ich]);
        }
    }

    /// `revert_mclms`.
    fn revert_mclms(&mut self, tile_size: usize) {
        let mut pred = [0i32; WMALL_MAX_CHANNELS];
        for icoef in 0..tile_size {
            self.mclms_predict(icoef, &mut pred);
            self.mclms_update(icoef, &pred);
        }
    }

    /// `use_high_update_speed`.
    fn use_high_update_speed(&mut self, ich: usize) {
        for ilms in (0..self.cdlms_ttl[ich]).rev() {
            let lms = &mut self.cdlms[ich][ilms];
            let recent = lms.recent;
            if self.update_speed[ich] == 16 {
                continue;
            }
            let base = if self.b_v3_rtm { recent } else { 0 };
            for u in lms.lms_updates[base..base + lms.order].iter_mut() {
                *u = u.wrapping_mul(2);
            }
        }
        self.update_speed[ich] = 16;
    }

    /// `use_normal_update_speed`.
    fn use_normal_update_speed(&mut self, ich: usize) {
        for ilms in (0..self.cdlms_ttl[ich]).rev() {
            let lms = &mut self.cdlms[ich][ilms];
            let recent = lms.recent;
            if self.update_speed[ich] == 8 {
                continue;
            }
            let base = if self.b_v3_rtm { recent } else { 0 };
            for u in lms.lms_updates[base..base + lms.order].iter_mut() {
                *u /= 2;
            }
        }
        self.update_speed[ich] = 8;
    }

    /// `lms_update16` / `lms_update32` (the 16-bit path's prevvalues fit
    /// in int16 after the clip, so one element type serves both).
    fn lms_update(&mut self, ich: usize, ilms: usize, input: i32) {
        let range = 1i32 << (self.bits_per_sample - 1);
        let speed = self.update_speed[ich];
        let lms = &mut self.cdlms[ich][ilms];
        let order = lms.order;
        let mut recent = lms.recent;

        if recent != 0 {
            recent -= 1;
        } else {
            lms.lms_prevvalues.copy_within(..order, order);
            lms.lms_updates.copy_within(..order, order);
            recent = order - 1;
        }

        lms.lms_prevvalues[recent] = av_clip(input, -range, range - 1);
        lms.lms_updates[recent] = (wmasign(input) * speed) as i16;

        lms.lms_updates[recent + (order >> 4)] >>= 2;
        lms.lms_updates[recent + (order >> 3)] >>= 1;
        lms.recent = recent;
        lms.lms_updates[recent + order..].fill(0);
    }

    /// `revert_cdlms16` / `revert_cdlms32`, with
    /// `scalarproduct_and_madd_int16/32_c` over `FFALIGN(order, 16)` /
    /// `FFALIGN(order, 8)` taps.
    fn revert_cdlms(&mut self, ch: usize, coef_begin: usize, coef_end: usize) {
        let round = if self.bits_per_sample > 16 { 8 } else { 16 };
        for ilms in (0..self.cdlms_ttl[ch]).rev() {
            for icoef in coef_begin..coef_end {
                let residue = self.channel_residues[ch][icoef];
                let lms = &mut self.cdlms[ch][ilms];
                let recent = lms.recent;
                let len = ffalign(lms.order, round);
                let mul = wmasign(residue);
                let mut pred = (1u32 << lms.scaling) >> 1;
                let mut res = 0u32;
                for k in 0..len {
                    res = res
                        .wrapping_add((lms.coefs[k] as i32 as u32).wrapping_mul(lms.lms_prevvalues[recent + k] as u32));
                    lms.coefs[k] = (lms.coefs[k] as i32 + mul * lms.lms_updates[recent + k] as i32) as i16;
                }
                pred = pred.wrapping_add(res);
                let input = residue.wrapping_add((pred as i32) >> lms.scaling);
                self.lms_update(ch, ilms, input);
                self.channel_residues[ch][icoef] = input;
            }
        }
    }

    /// `revert_inter_ch_decorr`.
    fn revert_inter_ch_decorr(&mut self, tile_size: usize) {
        if self.num_channels != 2 {
            return;
        }
        if self.is_channel_coded[0] || self.is_channel_coded[1] {
            let (r0, r1) = self.channel_residues.split_at_mut(1);
            for (a, b) in r0[0][..tile_size].iter_mut().zip(r1[0][..tile_size].iter_mut()) {
                *a = a.wrapping_sub(*b >> 1);
                *b = b.wrapping_add(*a);
            }
        }
    }

    /// `revert_acfilter`.
    fn revert_acfilter(&mut self, tile_size: usize) {
        let filter_coeffs = self.acfilter_coeffs;
        let scaling = self.acfilter_scaling;
        let order = self.acfilter_order;

        for ich in 0..self.num_channels {
            let prevvalues = &mut self.acfilter_prevvalues[ich];
            let res = &mut self.channel_residues[ich];
            for i in 0..order {
                let mut pred = 0u32;
                for j in 0..order {
                    let term = if i <= j {
                        (filter_coeffs[j] as i32 as u32).wrapping_mul(prevvalues[j - i] as u32)
                    } else {
                        (res[i - j - 1] as u32).wrapping_mul(filter_coeffs[j] as i32 as u32)
                    };
                    pred = pred.wrapping_add(term);
                }
                res[i] = res[i].wrapping_add((pred as i32) >> scaling);
            }
            for i in order..tile_size {
                let mut pred = 0u32;
                for j in 0..order {
                    pred = pred.wrapping_add((res[i - j - 1] as u32).wrapping_mul(filter_coeffs[j] as i32 as u32));
                }
                res[i] = res[i].wrapping_add((pred as i32) >> scaling);
            }
            for j in (0..order).rev() {
                prevvalues[j] = if tile_size <= j { prevvalues[j - tile_size] } else { res[tile_size - j - 1] };
            }
        }
    }

    /// `decode_subframe`; `None` is FFmpeg's negative return.
    fn decode_subframe(&mut self, gb: &mut GetBits<'_>) -> Option<()> {
        let nch = self.num_channels;
        let mut offset = self.samples_per_frame;
        let mut subframe_len = self.samples_per_frame;
        let mut total_samples = (self.samples_per_frame * nch) as i64;

        // find the next block offset and size: the next block of the
        // channel with the smallest number of decoded samples
        for ch in &self.channel[..nch] {
            if offset > ch.decoded_samples {
                offset = ch.decoded_samples;
                subframe_len = ch.len_at(ch.cur_subframe);
            }
        }

        // get a list of all channels that contain the estimated block
        self.channels_for_cur_subframe = 0;
        for i in 0..nch {
            let ch = &mut self.channel[i];
            // subtract already processed samples
            total_samples -= ch.decoded_samples as i64;

            // and count if there are multiple subframes that match our profile
            if offset == ch.decoded_samples && subframe_len == ch.len_at(ch.cur_subframe) {
                let len = ch.len_at(ch.cur_subframe);
                total_samples -= len as i64;
                ch.decoded_samples += len;
                self.channel_indexes_for_cur_subframe[self.channels_for_cur_subframe] = i;
                self.channels_for_cur_subframe += 1;
            }
        }

        // check if the frame will be complete after processing the
        // estimated block
        if total_samples == 0 {
            self.parsed_all_subframes = true;
        }

        self.seekable_tile = gb.get_bits1() != 0;
        if self.seekable_tile {
            self.clear_codec_buffers();

            if gb.get_bits1() != 0 {
                // arithmetic coding: FFmpeg asks for a sample
                return None;
            }
            self.do_ac_filter = gb.get_bits1() != 0;
            self.do_inter_ch_decorr = gb.get_bits1() != 0;
            self.do_mclms = gb.get_bits1() != 0;

            if self.do_ac_filter {
                self.decode_ac_filter(gb);
            }

            if self.do_mclms {
                self.decode_mclms(gb);
            }

            self.decode_cdlms(gb)?;
            self.movave_scaling = gb.get_bits(3);
            self.quant_stepsize = gb.get_bits(8) + 1;

            self.reset_codec();
        }

        let rawpcm_tile = gb.get_bits1() != 0;

        if !rawpcm_tile && self.cdlms[0][0].order == 0 {
            // waiting for seekable tile
            self.nb_samples = 0;
            return None;
        }

        self.is_channel_coded[..nch].fill(true);

        if !rawpcm_tile {
            for coded in self.is_channel_coded[..nch].iter_mut() {
                *coded = gb.get_bits1() != 0;
            }

            if self.b_v3_rtm && gb.get_bits1() != 0 {
                self.decode_lpc(gb);
            }
        }

        if gb.bits_left() < 1 {
            return None;
        }

        let padding_zeroes = if gb.get_bits1() != 0 { gb.get_bits(5) } else { 0 };

        if rawpcm_tile {
            let bits = self.bits_per_sample as i32 - padding_zeroes as i32;
            if bits <= 0 {
                return None;
            }
            for i in 0..nch {
                for j in 0..subframe_len {
                    self.channel_residues[i][j] = gb.get_sbits(bits as u32);
                }
            }
        } else {
            if self.bits_per_sample < padding_zeroes {
                return None;
            }
            for i in 0..nch {
                if self.is_channel_coded[i] {
                    // FFmpeg ignores running out of bits here
                    self.decode_channel_residues(gb, i, subframe_len);
                    if self.seekable_tile {
                        self.use_high_update_speed(i);
                    } else {
                        self.use_normal_update_speed(i);
                    }
                    self.revert_cdlms(i, 0, subframe_len);
                } else {
                    self.channel_residues[i][..subframe_len].fill(0);
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

            // Dequantize
            if self.quant_stepsize != 1 {
                for res in self.channel_residues[..nch].iter_mut() {
                    for v in res[..subframe_len].iter_mut() {
                        *v = (*v as u32).wrapping_mul(self.quant_stepsize) as i32;
                    }
                }
            }
        }

        // Write to proper output buffer depending on bit-depth
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let len = self.channel[c].len_at(self.channel[c].cur_subframe);
            let out = &mut self.out[c];
            for j in 0..len {
                let v = if self.bits_per_sample == 16 {
                    (self.channel_residues[c][j] as i16 as i32).wrapping_mul(1i32 << padding_zeroes)
                } else {
                    (self.channel_residues[c][j] as u32).wrapping_mul(256u32 << padding_zeroes) as i32
                };
                if let Some(o) = out.get_mut(self.out_pos[c]) {
                    *o = v;
                }
                self.out_pos[c] += 1;
            }
        }

        // handled one subframe
        for i in 0..self.channels_for_cur_subframe {
            let ch = &mut self.channel[self.channel_indexes_for_cur_subframe[i]];
            if ch.cur_subframe >= ch.num_subframes {
                return None;
            }
            ch.cur_subframe += 1;
        }
        Some(())
    }

    /// `decode_frame` on the reservoir reader: 1 when more frames follow,
    /// 0 for the last frame or a damaged subframe, negative on errors.
    fn decode_frame(&mut self) -> i32 {
        let data = std::mem::take(&mut self.frame_data);
        let mut gb = GetBits::with_state(&data, self.gb);
        let ret = self.decode_frame_inner(&mut gb);
        self.gb = gb.state();
        self.frame_data = data;
        ret
    }

    fn decode_frame_inner(&mut self, gb: &mut GetBits<'_>) -> i32 {
        let spf = self.samples_per_frame;
        // ff_get_buffer
        self.nb_samples = spf as i64;
        for out in self.out.iter_mut() {
            out.fill(0);
        }
        self.out_pos = [0; WMALL_MAX_CHANNELS];

        // get frame length
        let len = if self.len_prefix { gb.get_bits(self.log2_frame_size) as i64 } else { 0 };

        // decode tile information
        if self.decode_tilehdr(gb).is_none() {
            self.packet_loss = true;
            self.nb_samples = 0;
            return -1;
        }

        // read drc info
        if self.dynamic_range_compression {
            gb.get_bits(8);
        }

        // skip counts at the start (usually the first frame) and end
        // (sometimes the last frame) of the stream
        if gb.get_bits1() != 0 {
            if gb.get_bits1() != 0 {
                gb.get_bits(av_log2(spf as u32 * 2));
            }
            if gb.get_bits1() != 0 {
                let skip = gb.get_bits(av_log2(spf as u32 * 2));
                self.nb_samples -= skip as i64;
                if self.nb_samples <= 0 {
                    return -1;
                }
            }
        }

        // reset subframe states
        self.parsed_all_subframes = false;
        for ch in self.channel[..self.num_channels].iter_mut() {
            ch.decoded_samples = 0;
            ch.cur_subframe = 0;
        }

        // decode all subframes
        while !self.parsed_all_subframes {
            let decoded_samples = self.channel[0].decoded_samples;
            if self.decode_subframe(gb).is_none() {
                self.packet_loss = true;
                if self.nb_samples != 0 {
                    self.nb_samples = decoded_samples as i64;
                }
                return 0;
            }
        }

        if self.len_prefix {
            let read = gb.bits_count() as i64 - self.frame_offset as i64;
            if len != read + 2 {
                self.packet_loss = true;
                return 0;
            }

            // skip the rest of the frame data
            gb.skip_bits_long(len - read - 1);
        }

        // decode trailer bit
        gb.get_bits1() as i32
    }

    /// `remaining_bits` of the packet reader.
    fn remaining_bits(&self, gb: &GetBits<'_>) -> i64 {
        self.buf_bit_size - gb.bits_count() as i64
    }

    /// `save_bits`: fill the bit reservoir with a (partial) frame.
    fn save_bits(&mut self, gb: &mut GetBits<'_>, len: i64, append: bool) {
        let mut len = len;
        // when the frame data does not need to be concatenated, the input
        // buffer is reset and additional bits from the previous frame are
        // copied and skipped later so that a fast byte copy is possible
        if !append {
            self.frame_offset = gb.bits_count() & 7;
            self.num_saved_bits = self.frame_offset;
            self.pb.reset();
        }

        let buflen = (self.num_saved_bits as i64 + len + 8) >> 3;

        if len <= 0 || buflen > self.max_frame_size as i64 {
            self.packet_loss = true;
            self.num_saved_bits = 0;
            return;
        }

        self.num_saved_bits += len as usize;
        if !append {
            let src = gb.buffer().get(gb.bits_count() >> 3..).unwrap_or(&[]);
            self.pb.copy_bits(&mut self.frame_data, src, self.num_saved_bits);
        } else {
            let align = (8 - (gb.bits_count() & 7) as i64).min(len);
            let v = gb.get_bits(align as u32);
            self.pb.put_bits(&mut self.frame_data, align as u32, v);
            len -= align;
            let src = gb.buffer().get(gb.bits_count() >> 3..).unwrap_or(&[]);
            self.pb.copy_bits(&mut self.frame_data, src, len as usize);
        }
        gb.skip_bits_long(len);

        self.pb.flush(&mut self.frame_data);

        let mut fgb = GetBits::new(&self.frame_data, self.num_saved_bits);
        fgb.skip_bits(self.frame_offset as u32);
        self.gb = fgb.state();
    }

    /// `decode_packet` on the unread rest `buf` of a demuxer packet (empty
    /// when draining): the bytes consumed, or `None` for FFmpeg's error
    /// return, which drops the frame decoded in the call. The frame, if
    /// any, is left in `self.out` / `self.nb_samples`.
    fn decode_packet(&mut self, buf: &[u8]) -> Option<usize> {
        self.nb_samples = 0;

        let mut gb;
        if buf.is_empty() {
            self.packet_done = false;
            if self.num_saved_bits <= self.gb.bits_count() {
                return Some(0);
            }
            if self.decode_frame() == 0 {
                self.num_saved_bits = 0;
            }
            // the packet reader keeps its position from the last packet
            gb = GetBits::with_state(&[], self.pgb);
        } else if self.packet_done || self.packet_loss {
            self.packet_done = false;

            let buf_size = self.block_align.min(buf.len());
            self.next_packet_start = buf.len() - buf_size;
            self.buf_bit_size = (buf_size << 3) as i64;

            // parse packet header
            gb = GetBits::new(buf, buf_size << 3);
            let packet_sequence_number = gb.get_bits(4);
            gb.skip_bits(1); // seekable_frame_in_packet, currently unused
            gb.get_bits1(); // spliced packet: FFmpeg asks for a sample and goes on

            // get number of bits that need to be added to the previous frame
            let mut num_bits_prev_frame = gb.get_bits(self.log2_frame_size) as i64;

            // check for packet loss
            if !self.packet_loss && (self.packet_sequence_number + 1) & 0xF != packet_sequence_number {
                self.packet_loss = true;
            }
            self.packet_sequence_number = packet_sequence_number;

            if num_bits_prev_frame > 0 {
                let remaining_packet_bits = self.buf_bit_size - gb.bits_count() as i64;
                if num_bits_prev_frame >= remaining_packet_bits {
                    num_bits_prev_frame = remaining_packet_bits;
                    self.packet_done = true;
                }

                // Append the previous frame data to the remaining data from
                // the previous packet to create a full frame.
                self.save_bits(&mut gb, num_bits_prev_frame, true);

                // decode the cross packet frame if it is valid
                if num_bits_prev_frame < remaining_packet_bits && !self.packet_loss {
                    self.decode_frame();
                }
            }

            if self.packet_loss {
                // Reset number of saved bits so that the decoder does not
                // start to decode incomplete frames in the len_prefix == 0
                // case.
                self.num_saved_bits = 0;
                self.packet_loss = false;
                self.pb.reset();
            }
        } else {
            let size = buf.len() as i64 - self.next_packet_start as i64;
            self.buf_bit_size = size.max(0) << 3;
            gb = GetBits::new(buf, self.buf_bit_size as usize);
            gb.skip_bits(self.packet_offset as u32);

            let remaining = self.remaining_bits(&gb);
            let frame_size = if self.len_prefix && remaining > self.log2_frame_size as i64 {
                gb.show_bits(self.log2_frame_size) as i64
            } else {
                0
            };
            if frame_size != 0 && frame_size <= remaining {
                self.save_bits(&mut gb, frame_size, false);

                if !self.packet_loss {
                    self.packet_done = self.decode_frame() == 0;
                }
            } else if !self.len_prefix && self.num_saved_bits > self.gb.bits_count() {
                // Without length prefixes the frame lengths are unknown, but
                // the part of a new packet that belongs to the previous frame
                // is: save the packet first and append the "previous frame"
                // data from the next packet, so the buffer holds only full
                // frames.
                self.packet_done = self.decode_frame() == 0;
            } else {
                self.packet_done = true;
            }
        }

        if self.remaining_bits(&gb) < 0 {
            self.packet_loss = true;
        }

        if self.packet_done && !self.packet_loss && self.remaining_bits(&gb) > 0 {
            // save the rest of the data so that it can be decoded with the
            // next packet
            let rest = self.remaining_bits(&gb);
            self.save_bits(&mut gb, rest, false);
        }

        self.pgb = gb.state();
        self.packet_offset = gb.bits_count() & 7;

        (!self.packet_loss).then_some(gb.bits_count() >> 3)
    }

    /// The frame `decode_packet` left behind, if it has samples.
    fn take_frame(&mut self) -> Option<AudioFrame> {
        if self.nb_samples <= 0 {
            return None;
        }
        let n = (self.nb_samples as usize).min(self.samples_per_frame);
        self.nb_samples = 0;
        let data = self
            .out
            .iter()
            .map(|out| {
                if self.bits_per_sample == 16 {
                    out[..n].iter().flat_map(|&v| (v as i16).to_le_bytes()).collect()
                } else {
                    out[..n].iter().flat_map(|&v| v.to_le_bytes()).collect()
                }
            })
            .collect();
        Some(AudioFrame { samples: n as u32, pts: None, data })
    }

    /// FFmpeg's `flush` (seek).
    fn flush_state(&mut self) {
        self.packet_loss = true;
        self.packet_done = false;
        self.num_saved_bits = 0;
        self.frame_offset = 0;
        self.next_packet_start = 0;
        self.cdlms[0][0].order = 0;
        self.nb_samples = 0;
        self.pb.reset();
    }
}

impl DecodeCallback for WmaLosslessDecoder {
    fn decode(&mut self, data: &[u8]) -> Call {
        let consumed = self.decode_packet(data);
        // an error return drops the call's frame
        let frame = consumed.and_then(|_| self.take_frame());
        Call { consumed, frame }
    }
}

impl Decoder for WmaLosslessDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut lp = std::mem::take(&mut self.lp);
        lp.send(self, &packet.data);
        self.lp = lp;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let mut lp = std::mem::take(&mut self.lp);
        let frame = lp.receive(self);
        self.lp = lp;
        frame
    }

    /// End of stream: drain the frames still in the reservoir.
    fn flush(&mut self) -> Result<()> {
        self.lp.flush();
        Ok(())
    }

    /// Seek: FFmpeg's `flush`.
    fn reset(&mut self) -> Result<()> {
        self.flush_state();
        self.lp.reset();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: if self.bits_per_sample == 16 { SampleFormat::S16P } else { SampleFormat::S32P },
            sample_rate: self.sample_rate,
            channels: self.num_channels as u16,
        })
    }
}

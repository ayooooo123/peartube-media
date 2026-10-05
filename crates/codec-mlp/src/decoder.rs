// Ported from FFmpeg libavcodec/mlpdec.c (commit 2da55bf).
// Licensed under LGPL-2.1-or-later.

//! MLP / TrueHD decoder: access unit parsing, restart headers, decoding
//! parameter blocks, Huffman residual reading, prediction filtering,
//! rematrixing and lossless packing.

use crate::bitreader::BitReader;
use crate::checksum::{mlp_calculate_parity, mlp_restart_checksum, xor_32_to_8};
use crate::common::*;
use crate::crc;
use crate::dsp::{mlp_filter_channel, mlp_rematrix_channel, msb_mask, pack_output};

use crate::parse::{read_major_sync, MlpHeaderInfo};
use crate::tables::{HUFF_LUTS, NOISE_TABLE, THD_CHANNEL_ORDER};

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error as CoreError, Frame, Packet};

/// Number of bits used for the VLC lookup — longest Huffman code is 9.
const VLC_BITS: u32 = 9;

/// Per-substream decoding state (`SubStream` in FFmpeg).
#[derive(Clone, Debug)]
struct SubStream {
    restart_seen: bool,
    end_of_stream: bool,

    // restart header data
    noise_type: u16,
    min_channel: usize,
    max_channel: usize,
    coded_channels: u64,
    max_matrix_channel: usize,
    /// For each channel output by the matrix, the output channel that maps
    /// to it (ch_assign[out] = matrix channel).
    ch_assign: [u8; MAX_CHANNELS],
    mask: u64,

    channel_params: [ChannelParams; MAX_CHANNELS],

    noise_shift: u8,
    noisegen_seed: u32,

    data_check_present: bool,
    param_presence_flags: u8,

    // matrix data
    num_primitive_matrices: usize,
    matrix_out_ch: [usize; MAX_MATRICES],
    lsb_bypass: [bool; MAX_MATRICES],
    /// 2.14 fixed point.
    matrix_coeff: [[i32; MAX_CHANNELS]; MAX_MATRICES],
    matrix_noise_shift: [u8; MAX_MATRICES],

    quant_step_size: [u8; MAX_CHANNELS],
    blocksize: usize,
    blockpos: usize,
    output_shift: [i8; MAX_CHANNELS],
    lossless_check_data: i32,
}

impl Default for SubStream {
    fn default() -> Self {
        Self {
            restart_seen: false,
            end_of_stream: false,
            noise_type: 0,
            min_channel: 0,
            max_channel: 0,
            coded_channels: 0,
            max_matrix_channel: 0,
            ch_assign: [0; MAX_CHANNELS],
            mask: 0,
            channel_params: [ChannelParams::default(); MAX_CHANNELS],
            noise_shift: 0,
            noisegen_seed: 0,
            data_check_present: false,
            param_presence_flags: 0xff,
            num_primitive_matrices: 0,
            matrix_out_ch: [0; MAX_MATRICES],
            lsb_bypass: [false; MAX_MATRICES],
            matrix_coeff: [[0; MAX_CHANNELS]; MAX_MATRICES],
            matrix_noise_shift: [0; MAX_MATRICES],
            quant_step_size: [0; MAX_CHANNELS],
            blocksize: 8,
            blockpos: 0,
            output_shift: [0; MAX_CHANNELS],
            lossless_check_data: -1,
        }
    }
}

/// Whole-decoder state (`MLPDecodeContext`).
pub struct MlpDecoder {
    codec_is_mlp: bool,
    /// The codec id the decoder was created for (`mlp` or `truehd`).
    codec_id: CodecId,
    /// Buffered output frame between `send_packet` and `receive_frame`.
    pending: Option<AudioFrame>,

    is_major_sync_unit: bool,
    major_sync_header_size: usize,
    params_valid: bool,
    num_substreams: usize,
    extended_substream_info: u32,
    substream_info: u32,
    /// Index of the last substream to decode — further substreams skipped.
    max_decoded_substream: usize,
    needs_reordering: bool,

    access_unit_size: usize,
    access_unit_size_pow2: usize,

    substream: [SubStream; MAX_SUBSTREAMS],

    matrix_changed: i32,
    filter_changed: [[i32; NUM_FILTERS]; MAX_CHANNELS],

    noise_buffer: [i8; MAX_BLOCKSIZE_POW2],
    bypassed_lsbs: [[u8; MAX_CHANNELS]; MAX_BLOCKSIZE],
    sample_buffer: [[i32; MAX_CHANNELS]; MAX_BLOCKSIZE],

    // Output formatting
    out_channels: u16,
    out_sample_rate: u32,
    is32: bool,
}

fn invalid(msg: impl Into<String>) -> CoreError {
    CoreError::InvalidData(format!("mlp: {}", msg.into()))
}

impl MlpDecoder {
    /// `mlp_decode_init`. `codec_is_mlp` selects the MLP vs TrueHD rules.
    pub fn new(params: &CodecParameters, codec_is_mlp: bool) -> Self {
        let _ = params;
        Self {
            codec_is_mlp,
            codec_id: params.codec_id.clone(),
            pending: None,
            is_major_sync_unit: false,
            major_sync_header_size: 0,
            params_valid: false,
            num_substreams: 0,
            extended_substream_info: 0,
            substream_info: 0,
            max_decoded_substream: 0,
            needs_reordering: false,
            access_unit_size: 0,
            access_unit_size_pow2: 0,
            substream: Default::default(),
            matrix_changed: 0,
            filter_changed: [[0; NUM_FILTERS]; MAX_CHANNELS],
            noise_buffer: [0; MAX_BLOCKSIZE_POW2],
            bypassed_lsbs: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
            sample_buffer: [[0; MAX_CHANNELS]; MAX_BLOCKSIZE],
            out_channels: 0,
            out_sample_rate: 0,
            is32: false,
        }
    }

    /// `read_major_sync` (decoder side): validate and apply the header.
    fn apply_major_sync(
        &mut self,
        buf: &[u8],
        gb: &mut BitReader,
    ) -> oxideav_core::Result<MlpHeaderInfo> {
        let mh = read_major_sync(buf, gb).map_err(|e| invalid(format!("{e}")))?;

        if mh.group1_bits == 0 {
            return Err(invalid("invalid/unknown bits per sample"));
        }
        if mh.group2_bits > mh.group1_bits {
            return Err(invalid(
                "Channel group 2 cannot have more bits per sample than group 1.",
            ));
        }
        if mh.group2_samplerate != 0 && mh.group2_samplerate != mh.group1_samplerate {
            return Err(invalid(
                "Channel groups with differing sample rates are not currently supported.",
            ));
        }
        if mh.group1_samplerate == 0 {
            return Err(invalid("invalid/unknown sampling rate"));
        }
        if mh.group1_samplerate > MAX_SAMPLERATE {
            return Err(invalid(format!(
                "Sampling rate {} is greater than the supported maximum ({}).",
                mh.group1_samplerate, MAX_SAMPLERATE
            )));
        }
        if mh.access_unit_size as usize > MAX_BLOCKSIZE {
            return Err(invalid(format!(
                "Block size {} is greater than the supported maximum ({}).",
                mh.access_unit_size, MAX_BLOCKSIZE
            )));
        }
        if mh.access_unit_size_pow2 as usize > MAX_BLOCKSIZE_POW2 {
            return Err(invalid(format!(
                "Block size pow2 {} is greater than the supported maximum ({}).",
                mh.access_unit_size_pow2, MAX_BLOCKSIZE_POW2
            )));
        }

        if mh.num_substreams == 0 {
            return Err(invalid("zero substreams"));
        }
        if self.codec_is_mlp && mh.num_substreams > 2 {
            return Err(invalid("MLP only supports up to 2 substreams."));
        }
        if mh.num_substreams as usize > MAX_SUBSTREAMS {
            return Err(invalid(format!(
                "{} substreams (more than the maximum supported by the decoder)",
                mh.num_substreams
            )));
        }

        self.major_sync_header_size = mh.header_size;
        self.access_unit_size = mh.access_unit_size as usize;
        self.access_unit_size_pow2 = mh.access_unit_size_pow2 as usize;
        self.num_substreams = mh.num_substreams as usize;
        self.extended_substream_info = mh.extended_substream_info;
        self.substream_info = mh.substream_info;

        // A 4th substream with the MSB of substream_info set means a
        // 16-channel spatial presentation (Atmos in TrueHD).
        let _atmos = !self.codec_is_mlp
            && self.num_substreams == 4
            && self.substream_info >> 7 == 1;

        // Limit to decoding 3 substreams: the 4th carries Atmos non-audio data.
        self.max_decoded_substream = (self.num_substreams - 1).min(2);

        self.out_sample_rate = mh.group1_samplerate;
        self.is32 = mh.group1_bits > 16;

        self.params_valid = true;
        for s in self.substream.iter_mut() {
            s.restart_seen = false;
        }

        // Set the layout for each substream. When there's more than one,
        // the first substream is Stereo; later substreams' layouts are in
        // the major sync.
        if self.codec_is_mlp {
            if mh.stream_type != SYNC_MLP {
                return Err(invalid(format!(
                    "unexpected stream_type {:X} in MLP",
                    mh.stream_type
                )));
            }
            // More than one substream: the first is Stereo, the given layout
            // belongs to substream 1.
            let layout_substream = if self.num_substreams > 1 { 1 } else { 0 };
            self.substream[layout_substream].mask = mh.channel_layout_mlp;
        } else {
            if mh.stream_type != SYNC_TRUEHD {
                return Err(invalid(format!(
                    "unexpected stream_type {:X} in !MLP",
                    mh.stream_type
                )));
            }
            self.substream[1].mask = mh.channel_layout_thd_stream1;
            if mh.channels_thd_stream1 == 2 && mh.channels_thd_stream2 == 2 {
                self.substream[0].mask = 0x3; // STEREO
            }
            if self.num_substreams > 1 {
                self.substream[0].mask = 0x3; // STEREO
            }
            if self.num_substreams == 1
                && mh.channels_thd_stream1 == 1
                && mh.channels_thd_stream2 == 1
            {
                self.substream[0].mask = 0x4; // MONO
            }
            if self.num_substreams > 2 {
                if mh.channel_layout_thd_stream2 != 0 {
                    self.substream[2].mask = mh.channel_layout_thd_stream2;
                } else {
                    self.substream[2].mask = mh.channel_layout_thd_stream1;
                }
            }
            if self.num_substreams == 2 {
                self.substream[1].mask = mh.channel_layout_thd_stream2;
            }
        }

        self.needs_reordering = mh.channel_arrangement >= 18 && mh.channel_arrangement <= 20;

        Ok(mh)
    }

    /// `read_restart_header`.
    fn read_restart_header(
        &mut self,
        gbp: &mut BitReader,
        buf: &[u8],
        substr: usize,
        restart_bit_offset: usize,
    ) -> oxideav_core::Result<()> {
        let std_max_matrix_channel = if self.codec_is_mlp {
            MAX_MATRIX_CHANNEL_MLP
        } else {
            MAX_MATRIX_CHANNEL_TRUEHD
        };

        let sync_word = gbp.get_bits(13);
        if sync_word != 0x31ea >> 1 {
            return Err(invalid(format!(
                "restart header sync incorrect (got 0x{sync_word:04x})"
            )));
        }

        let noise_type = gbp.get_bits(1);
        if self.codec_is_mlp && noise_type != 0 {
            return Err(invalid("MLP must have 0x31ea sync word."));
        }

        gbp.skip(16); // Output timestamp

        let min_channel = gbp.get_bits(4) as usize;
        let max_channel = gbp.get_bits(4) as usize;
        let max_matrix_channel = gbp.get_bits(4) as usize;

        if max_matrix_channel > std_max_matrix_channel {
            return Err(invalid(format!(
                "Max matrix channel cannot be greater than {std_max_matrix_channel}."
            )));
        }
        if max_matrix_channel > MAX_MATRIX_CHANNEL_MLP && noise_type == 0 {
            return Err(invalid(format!(
                "{} channels (more than the maximum supported by the decoder)",
                max_channel + 2
            )));
        }
        // FFmpeg: `max_channel + 1 > MAX_CHANNELS || max_channel + 1 < min_channel`
        // — the second comparison is against min_channel, not min_channel + 1
        // (this exact quirk is what FATE's ticket-1726-monocut exercises).
        if max_channel + 1 > MAX_CHANNELS || max_channel + 1 < min_channel {
            return Err(invalid("bad channel range"));
        }

        {
            let s = &mut self.substream[substr];
            s.min_channel = min_channel;
            s.max_channel = max_channel;
            // FFmpeg: ((1LL << (max - min + 1)) - 1) << min; the width can be
            // zero (max == min - 1 passes FFmpeg's range check) or negative in
            // principle — compute in i64 so a negative shift yields 0, not a
            // panic.
            let width = max_channel as i64 - min_channel as i64 + 1;
            let ones = if width >= 64 {
                u64::MAX
            } else if width <= 0 {
                0u64
            } else {
                (1u64 << width) - 1
            };
            s.coded_channels = ones << min_channel;
            s.max_matrix_channel = max_matrix_channel;
            s.noise_type = noise_type as u16;

            s.noise_shift = gbp.get_bits(4) as u8;
            s.noisegen_seed = gbp.get_bits(23);

            gbp.skip(19);

            s.data_check_present = gbp.get_bits(1) != 0;
            let lossless_check = gbp.get_bits(8) as u8;
            if substr == self.max_decoded_substream && s.lossless_check_data != -1 {
                let tmp = xor_32_to_8(s.lossless_check_data as u32);
                if tmp != lossless_check {
                    // Warning in FFmpeg; not fatal.
                }
            }

            gbp.skip(16);

            s.ch_assign = [0; MAX_CHANNELS];

            let mask = s.mask;
            for ch in 0..=s.max_matrix_channel {
                let mut ch_assign = gbp.get_bits(6) as usize;
                if !self.codec_is_mlp {
                    let channel = thd_channel_layout_extract_channel(mask, ch_assign);
                    // av_channel_layout_index_from_channel: index of the
                    // channel within the mask in LSB order.
                    match channel {
                        Some(chan) => {
                            ch_assign = mask_channel_index(mask, usize::from(chan)).ok_or_else(|| {
                                invalid(format!(
                                    "Assignment of matrix channel {ch} to invalid output channel {ch_assign}"
                                ))
                            })?;
                        }
                        None => {
                            return Err(invalid(format!(
                                "Assignment of matrix channel {ch} to invalid output channel {ch_assign}"
                            )));
                        }
                    }
                }
                if ch_assign > s.max_matrix_channel {
                    return Err(invalid(format!(
                        "Assignment of matrix channel {ch} to invalid output channel {ch_assign}"
                    )));
                }
                s.ch_assign[ch_assign] = ch as u8;
            }
        }

        // Checksum over `restart_bit_offset .. gbp.bits_read()` bits of `buf`
        // (the substream base). FFmpeg's restart header always starts 2 bits
        // into buf[0] for the first block; the caller passes the offset.
        let bit_size = gbp.bits_read() - restart_bit_offset;
        let checksum = self.restart_checksum_at(buf, restart_bit_offset, bit_size);
        let read = gbp.get_bits(8) as u8;
        if checksum != read {
            // FFmpeg logs an error but continues decoding.
        }

        // Set default decoding parameters.
        let s = &mut self.substream[substr];
        s.param_presence_flags = 0xff;
        s.num_primitive_matrices = 0;
        s.blocksize = 8;
        s.lossless_check_data = 0;

        s.output_shift = [0; MAX_CHANNELS];
        s.quant_step_size = [0; MAX_CHANNELS];

        let (min_channel, max_channel) = (s.min_channel, s.max_channel);
        for ch in min_channel..=max_channel {
            s.channel_params[ch].reset_to_defaults();
        }

        if substr == self.max_decoded_substream {
            // Output layout = substream mask; FFmpeg replaces ch_layout.
            self.out_channels = mask_popcount(s.mask) as u16;
            if self.out_channels == 0
                || self.out_channels as usize > s.max_matrix_channel + 1
            {
                // Keep count consistent with the matrix channels.
                self.out_channels = (s.max_matrix_channel + 1) as u16;
            }

            if self.codec_is_mlp && self.needs_reordering {
                // MLP (not TrueHD) channel reorder for the legacy layouts.
                let s = &mut self.substream[substr];
                if s.mask == (0x33 | 0x8) || s.mask == 0x37 {
                    // AV_CH_LAYOUT_QUAD|LFE or AV_CH_LAYOUT_5POINT0_BACK:
                    // rotate ch_assign[2..5].
                    let i = s.ch_assign[4];
                    s.ch_assign[4] = s.ch_assign[3];
                    s.ch_assign[3] = s.ch_assign[2];
                    s.ch_assign[2] = i;
                } else if s.mask == 0x3f {
                    // AV_CH_LAYOUT_5POINT1_BACK: swap LFE/BC pairs.
                    s.ch_assign.swap(2, 4);
                    s.ch_assign.swap(3, 5);
                }
            }
        }

        Ok(())
    }

    /// `ff_mlp_restart_checksum` over a bit range that may start mid-byte.
    fn restart_checksum_at(&self, buf: &[u8], bit_offset: usize, bit_size: usize) -> u8 {
        if bit_offset % 8 == 0 {
            return mlp_restart_checksum(&buf[bit_offset / 8..], bit_size);
        }
        // The restart header is only ever read from the first block, which
        // starts byte-aligned (bit_offset == 2 handled below). FFmpeg's
        // checksum assumes the header starts exactly two bits into buf[0]
        // and that bit_offset == 2. Reproduce that exact behaviour:
        debug_assert_eq!(bit_offset, 2, "restart header bit offset");
        mlp_restart_checksum(buf, bit_size)
    }

    /// `read_filter_params`.
    fn read_filter_params(
        &mut self,
        gbp: &mut BitReader,
        substr: usize,
        channel: usize,
        filter: usize,
    ) -> oxideav_core::Result<()> {
        let max_order = if filter != FIR { MAX_IIR_ORDER } else { MAX_FIR_ORDER };

        if self.filter_changed[channel][filter] > 1 {
            return Err(invalid("Filters may change only once per access unit."));
        }
        self.filter_changed[channel][filter] += 1;

        let order = gbp.get_bits(4) as usize;
        if order > max_order {
            return Err(invalid(format!(
                "{}IR filter order {order} is greater than maximum {max_order}.",
                if filter != FIR { 'I' } else { 'F' }
            )));
        }
        let mut fp = self.substream[substr].channel_params[channel].filter_params[filter];
        fp.order = order as u8;

        if order > 0 {
            fp.shift = gbp.get_bits(4) as u8;

            let coeff_bits = gbp.get_bits(5);
            let coeff_shift = gbp.get_bits(3);
            if coeff_bits < 1 || coeff_bits > 16 {
                return Err(invalid(format!(
                    "{}IR filter coeff_bits must be between 1 and 16.",
                    if filter != FIR { 'I' } else { 'F' }
                )));
            }
            if coeff_bits + coeff_shift > 16 {
                return Err(invalid(format!(
                    "Sum of coeff_bits and coeff_shift for {}IR filter must be 16 or less.",
                    if filter != FIR { 'I' } else { 'F' }
                )));
            }

            for i in 0..order {
                let coeff = self.substream[substr].channel_params[channel].coeff[filter];
                let _ = coeff;
                let v = gbp.get_sbits(coeff_bits) * (1 << coeff_shift);
                self.substream[substr].channel_params[channel].coeff[filter][i] = v;
            }

            if gbp.get_bits(1) != 0 {
                if filter == FIR {
                    return Err(invalid("FIR filter has state data specified."));
                }
                let state_bits = gbp.get_bits(4);
                let state_shift = gbp.get_bits(4);
                for i in 0..order {
                    fp.state[i] = if state_bits != 0 {
                        gbp.get_sbits(state_bits) * (1 << state_shift)
                    } else {
                        0
                    };
                }
            }
        }
        self.substream[substr].channel_params[channel].filter_params[filter] = fp;

        Ok(())
    }

    /// `read_matrix_params`.
    fn read_matrix_params(
        &mut self,
        substr: usize,
        gbp: &mut BitReader,
    ) -> oxideav_core::Result<()> {
        if self.matrix_changed > 1 {
            return Err(invalid("Matrices may change only once per access unit."));
        }
        self.matrix_changed += 1;

        let max_primitive_matrices = if self.codec_is_mlp {
            MAX_MATRICES_MLP
        } else {
            MAX_MATRICES_TRUEHD
        };
        let s = &mut self.substream[substr];

        s.num_primitive_matrices = gbp.get_bits(4) as usize;
        if s.num_primitive_matrices > max_primitive_matrices {
            s.num_primitive_matrices = 0;
            s.matrix_out_ch = [0; MAX_MATRICES];
            return Err(invalid(
                "Number of primitive matrices cannot be greater than the maximum.",
            ));
        }

        for mat in 0..s.num_primitive_matrices {
            s.matrix_out_ch[mat] = gbp.get_bits(4) as usize;
            let frac_bits = gbp.get_bits(4);
            s.lsb_bypass[mat] = gbp.get_bits(1) != 0;

            if s.matrix_out_ch[mat] > s.max_matrix_channel {
                s.num_primitive_matrices = 0;
                s.matrix_out_ch = [0; MAX_MATRICES];
                return Err(invalid("Invalid channel specified as output from matrix."));
            }
            if frac_bits > 14 {
                s.num_primitive_matrices = 0;
                s.matrix_out_ch = [0; MAX_MATRICES];
                return Err(invalid("Too many fractional bits specified."));
            }

            let mut max_chan = s.max_matrix_channel;
            if s.noise_type == 0 {
                max_chan += 2;
            }

            for ch in 0..=max_chan {
                let coeff_val = if gbp.get_bits(1) != 0 {
                    gbp.get_sbits(frac_bits + 2)
                } else {
                    0
                };
                s.matrix_coeff[mat][ch] = coeff_val * (1 << (14 - frac_bits));
            }

            if s.noise_type != 0 {
                s.matrix_noise_shift[mat] = gbp.get_bits(4) as u8;
            } else {
                s.matrix_noise_shift[mat] = 0;
            }
        }

        Ok(())
    }

    /// `read_channel_params`.
    fn read_channel_params(
        &mut self,
        substr: usize,
        gbp: &mut BitReader,
        ch: usize,
    ) -> oxideav_core::Result<()> {
        let param_presence_flags = self.substream[substr].param_presence_flags;

        if param_presence_flags & PARAM_FIR != 0 && gbp.get_bits(1) != 0 {
            self.read_filter_params(gbp, substr, ch, FIR)?;
        }
        if param_presence_flags & PARAM_IIR != 0 && gbp.get_bits(1) != 0 {
            self.read_filter_params(gbp, substr, ch, IIR)?;
        }

        {
            let s = &self.substream[substr];
            let fir = &s.channel_params[ch].filter_params[FIR];
            let iir = &s.channel_params[ch].filter_params[IIR];
            if usize::from(fir.order) + usize::from(iir.order) > 8 {
                return Err(invalid("Total filter orders too high."));
            }
            if fir.order > 0 && iir.order > 0 && fir.shift != iir.shift {
                return Err(invalid("FIR and IIR filters must use the same precision."));
            }
            if fir.order == 0 && iir.order > 0 {
                let shift = iir.shift;
                self.substream[substr].channel_params[ch].filter_params[FIR].shift = shift;
            }
        }

        if param_presence_flags & PARAM_HUFFOFFSET != 0 && gbp.get_bits(1) != 0 {
            let v = gbp.get_sbits(15) as i16;
            self.substream[substr].channel_params[ch].huff_offset = v;
        }

        let codebook = gbp.get_bits(2) as u8;
        let huff_lsbs = gbp.get_bits(5) as u8;
        let cp = &mut self.substream[substr].channel_params[ch];
        cp.codebook = codebook;
        cp.huff_lsbs = huff_lsbs;

        if cp.codebook > 0 && cp.huff_lsbs > 24 {
            cp.huff_lsbs = 0;
            return Err(invalid("Invalid huff_lsbs."));
        }

        Ok(())
    }

    /// `read_decoding_params`.
    fn read_decoding_params(
        &mut self,
        substr: usize,
        gbp: &mut BitReader,
    ) -> oxideav_core::Result<()> {
        let s = &mut self.substream[substr];
        let mut recompute_sho: u32 = 0;
        let mut ret: oxideav_core::Result<()> = Ok(());

        if s.param_presence_flags & PARAM_PRESENCE != 0 && gbp.get_bits(1) != 0 {
            s.param_presence_flags = gbp.get_bits(8) as u8;
        }

        let pflags = self.substream[substr].param_presence_flags;

        if pflags & PARAM_BLOCKSIZE != 0 && gbp.get_bits(1) != 0 {
            let blocksize = gbp.get_bits(9) as usize;
            if blocksize < 8 || blocksize > self.access_unit_size {
                self.substream[substr].blocksize = 0;
                return Err(invalid("Invalid blocksize."));
            }
            self.substream[substr].blocksize = blocksize;
        }

        if pflags & PARAM_MATRIX != 0 && gbp.get_bits(1) != 0 {
            if let Err(e) = self.read_matrix_params(substr, gbp) {
                ret = Err(e);
            }
        }

        if ret.is_ok() && pflags & PARAM_OUTSHIFT != 0 && gbp.get_bits(1) != 0 {
            let s = &mut self.substream[substr];
            for ch in 0..=s.max_matrix_channel {
                let shift = gbp.get_sbits(4);
                s.output_shift[ch] = if shift < 0 {
                    0 // FFmpeg warns: negative output_shift unsupported
                } else {
                    shift as i8
                };
            }
        }

        if ret.is_ok() && pflags & PARAM_QUANTSTEP != 0 && gbp.get_bits(1) != 0 {
            let s = &mut self.substream[substr];
            for ch in 0..=s.max_channel {
                s.quant_step_size[ch] = gbp.get_bits(4) as u8;
                recompute_sho |= 1 << ch;
            }
        }

        if ret.is_ok() {
            let (min_channel, max_channel) = {
                let s = &self.substream[substr];
                (s.min_channel, s.max_channel)
            };
            for ch in min_channel..=max_channel {
                if gbp.get_bits(1) != 0 {
                    recompute_sho |= 1 << ch;
                    if let Err(e) = self.read_channel_params(substr, gbp, ch) {
                        ret = Err(e);
                        break;
                    }
                }
            }
        }

        // fail: label — recompute sign huff offsets for touched channels.
        // (calculate_sign_hoff reads only this substream's state, inlined
        // to keep the borrow checker happy.)
        {
            let s = &mut self.substream[substr];
            for ch in 0..=s.max_channel {
                if recompute_sho & (1 << ch) != 0 {
                    let cp = &s.channel_params[ch];
                    let lsb_bits = i32::from(cp.huff_lsbs) - i32::from(s.quant_step_size[ch]);
                    let sign_shift = lsb_bits + if cp.codebook > 0 {
                        2 - i32::from(cp.codebook)
                    } else {
                        -1
                    };
                    let mut sign_huff_offset = i32::from(cp.huff_offset);
                    // FFmpeg shifts unconditionally here and lets the value
                    // wrap (UB in C, garbage in practice) when the erroring
                    // packet leaves lsb_bits negative; the frame is about to
                    // fail anyway. Wrap instead of panicking.
                    if cp.codebook > 0 {
                        sign_huff_offset -= match lsb_bits {
                            0..=30 => 7i32 << lsb_bits,
                            31 => (7i32 << 30) << 1,
                            _ => 0,
                        };
                    }
                    if (0..=30).contains(&sign_shift) {
                        sign_huff_offset -= 1i32 << sign_shift;
                    }
                    if cp.codebook > 0 && cp.huff_lsbs < s.quant_step_size[ch] {
                        if ret.is_ok() {
                            ret = Err(invalid("quant_step_size larger than huff_lsbs"));
                        }
                        s.quant_step_size[ch] = 0;
                    }
                    s.channel_params[ch].sign_huff_offset = sign_huff_offset;
                }
            }
        }

        ret
    }

    /// `read_huff_channels` for one sample position.
    fn read_huff_channels(
        &mut self,
        gbp: &mut BitReader,
        substr: usize,
        pos: usize,
    ) -> oxideav_core::Result<()> {
        let (num_primitive_matrices, min_channel, max_channel, blockpos) = {
            let s = &self.substream[substr];
            (
                s.num_primitive_matrices,
                s.min_channel,
                s.max_channel,
                s.blockpos,
            )
        };

        for mat in 0..num_primitive_matrices {
            if self.substream[substr].lsb_bypass[mat] {
                let bit = gbp.get_bits(1);
                self.bypassed_lsbs[pos + blockpos][mat] = bit as u8;
            }
        }

        for channel in min_channel..=max_channel {
            let cp = self.substream[substr].channel_params[channel];
            let quant_step_size = self.substream[substr].quant_step_size[channel];
            let codebook = cp.codebook;
            let lsb_bits = i32::from(cp.huff_lsbs) - i32::from(quant_step_size);
            let mut result: i32 = 0;

            if codebook > 0 {
                result = self.get_vlc(gbp, (codebook - 1) as usize);
            }

            if lsb_bits > 0 {
                result = (result << lsb_bits) + gbp.get_bits(lsb_bits as u32) as i32;
            }

            result += cp.sign_huff_offset;
            result *= 1 << quant_step_size;

            self.sample_buffer[pos + blockpos][channel] = result;
        }

        Ok(())
    }

    /// `get_vlc2` over the static 9-bit Huffman LUTs.
    #[inline]
    fn get_vlc(&self, gbp: &mut BitReader, table: usize) -> i32 {
        let lut = HUFF_LUTS[table];
        let peek = gbp.show_bits(VLC_BITS) as usize;
        let entry = lut[peek];
        gbp.skip(u32::from(entry.bits));
        i32::from(entry.sym)
    }

    /// `read_block_data`.
    fn read_block_data(
        &mut self,
        gbp: &mut BitReader,
        substr: usize,
    ) -> oxideav_core::Result<()> {
        let (data_check_present, blocksize, blockpos, access_unit_size, min_channel, max_channel) = {
            let s = &self.substream[substr];
            (
                s.data_check_present,
                s.blocksize,
                s.blockpos,
                self.access_unit_size,
                s.min_channel,
                s.max_channel,
            )
        };

        let mut expected_stream_pos = 0usize;
        if data_check_present {
            expected_stream_pos = gbp.bits_read();
            expected_stream_pos += gbp.get_bits(16) as usize;
            // FFmpeg: avpriv_request_sample — VLC block size check info.
            // The length check below still runs.
        }

        if blockpos + blocksize > access_unit_size {
            return Err(invalid("too many audio samples in frame"));
        }

        {
            let row = blockpos;
            for mat in 0..self.substream[substr].num_primitive_matrices {
                for i in 0..blocksize {
                    self.bypassed_lsbs[row + i][mat] = 0;
                }
            }
        }

        for i in 0..blocksize {
            self.read_huff_channels(gbp, substr, i)?;
        }

        for ch in min_channel..=max_channel {
            self.filter_channel(gbp, substr, ch);
        }

        let s = &mut self.substream[substr];
        s.blockpos += s.blocksize;

        if data_check_present {
            if gbp.bits_read() != expected_stream_pos {
                // FFmpeg logs "block data length mismatch" and continues.
            }
            gbp.skip(8);
        }

        Ok(())
    }

    /// `filter_channel`: run one channel's decoded residuals through the
    /// prediction filters.
    fn filter_channel(&mut self, _gbp: &mut BitReader, substr: usize, channel: usize) {
        // Copy the channel's filter state into the rolling state windows.
        let (fir_order, iir_order, filter_shift, mask, blocksize, blockpos) = {
            let s = &self.substream[substr];
            let cp = &s.channel_params[channel];
            let fir = &cp.filter_params[FIR];
            let iir = &cp.filter_params[IIR];
            (
                usize::from(fir.order),
                usize::from(iir.order),
                // FFmpeg uses the FIR shift (which read_channel_params
                // synchronised to the IIR shift when only IIR is present).
                u32::from(fir.shift),
                msb_mask(u32::from(s.quant_step_size[channel])),
                s.blocksize,
                s.blockpos,
            )
        };

        // firbuf: [MAX_FIR_ORDER] newest-first window seeded from state;
        // iirbuf likewise with MAX_IIR_ORDER entries (kept in the same
        // MAX_FIR_ORDER-wide array, unused tail zero).
        let mut firbuf = [0i32; MAX_FIR_ORDER];
        let mut iirbuf = [0i32; MAX_FIR_ORDER];
        let cp = &self.substream[substr].channel_params[channel];
        let fir_state = cp.filter_params[FIR].state;
        let iir_state = cp.filter_params[IIR].state;
        firbuf.copy_from_slice(&fir_state);
        iirbuf[..MAX_IIR_ORDER].copy_from_slice(&iir_state[..MAX_IIR_ORDER]);

        let fircoeff = cp.coeff[FIR];
        let iircoeff = cp.coeff[IIR];

        mlp_filter_channel(
            &mut firbuf,
            &mut iirbuf,
            &fircoeff,
            &iircoeff,
            fir_order,
            iir_order,
            filter_shift,
            mask,
            blocksize,
            &mut self.sample_buffer,
            channel,
            blockpos,
        );

        // Save state back: FFmpeg copies firbuf - blocksize .., i.e. the
        // newest-first window (state[0] = last sample's output).
        let cp = &mut self.substream[substr].channel_params[channel];
        cp.filter_params[FIR].state = firbuf;
        cp.filter_params[IIR].state[..MAX_IIR_ORDER]
            .copy_from_slice(&iirbuf[..MAX_IIR_ORDER]);
    }

    /// `generate_2_noise_channels`.
    fn generate_2_noise_channels(&mut self, substr: usize) {
        let (blockpos, maxchan, noise_shift) = {
            let s = &self.substream[substr];
            (s.blockpos, s.max_matrix_channel, s.noise_shift)
        };
        let mut seed = self.substream[substr].noisegen_seed;

        for i in 0..blockpos {
            // FFmpeg: uint16_t seed_shr7 = seed >> 7 (truncated to 16 bits).
            let seed_shr7 = (seed >> 7) as u16;
            self.sample_buffer[i][maxchan + 1] =
                (((seed >> 15) as u8) as i8 as i32) * (1 << noise_shift);
            self.sample_buffer[i][maxchan + 2] =
                ((seed_shr7 as u8) as i8 as i32) * (1 << noise_shift);

            let s32 = u32::from(seed_shr7);
            seed = (seed << 16) ^ s32 ^ (s32 << 5);
        }

        self.substream[substr].noisegen_seed = seed;
    }

    /// `fill_noise_buffer`.
    fn fill_noise_buffer(&mut self, substr: usize) {
        let mut seed = self.substream[substr].noisegen_seed;

        for item in self.noise_buffer.iter_mut().take(self.access_unit_size_pow2) {
            // FFmpeg truncates to uint8_t.
            let seed_shr15 = (seed >> 15) as u8;
            *item = NOISE_TABLE[seed_shr15 as usize];
            let s32 = u32::from(seed_shr15);
            seed = (seed << 8) ^ s32 ^ (s32 << 5);
        }

        self.substream[substr].noisegen_seed = seed;
    }

    /// `output_data`: rematrix, pack and return the frame's samples.
    fn output_data(&mut self, substr: usize) -> oxideav_core::Result<(Vec<Vec<u8>>, usize, i64)> {
        let (out_channels, noise_type, blockpos0, max_matrix_channel) = {
            let s = &self.substream[substr];
            (
                self.out_channels as usize,
                s.noise_type,
                s.blockpos,
                s.max_matrix_channel,
            )
        };

        if out_channels != max_matrix_channel + 1 {
            return Err(invalid("channel count mismatch"));
        }

        if blockpos0 == 0 {
            return Err(invalid("No samples to output."));
        }

        let maxchan;
        if noise_type == 0 {
            self.generate_2_noise_channels(substr);
            maxchan = max_matrix_channel + 2;
        } else {
            self.fill_noise_buffer(substr);
            maxchan = max_matrix_channel;
        }

        // Apply the channel matrices in turn.
        let (num_primitive_matrices, blockpos, access_unit_size_pow2) = (
            self.substream[substr].num_primitive_matrices,
            self.substream[substr].blockpos,
            self.access_unit_size_pow2,
        );
        for mat in 0..num_primitive_matrices {
            let dest_ch = self.substream[substr].matrix_out_ch[mat];
            let coeffs = self.substream[substr].matrix_coeff[mat];
            let noise_shift = self.substream[substr].matrix_noise_shift[mat];
            let mask = msb_mask(u32::from(
                self.substream[substr].quant_step_size[dest_ch],
            ));
            let index = num_primitive_matrices - mat;
            mlp_rematrix_channel(
                &mut self.sample_buffer,
                &coeffs,
                &self.bypassed_lsbs,
                mat,
                &self.noise_buffer,
                index,
                dest_ch,
                blockpos,
                maxchan,
                noise_shift,
                access_unit_size_pow2,
                mask,
            );
        }

        // Pack into interleaved PCM.
        let bytes_per_sample = if self.is32 { 4 } else { 2 };
        let mut data = vec![0u8; blockpos * out_channels * bytes_per_sample];
        let s = &self.substream[substr];
        let lc = pack_output(
            s.lossless_check_data,
            blockpos,
            &self.sample_buffer,
            &mut data,
            &s.ch_assign,
            &s.output_shift,
            s.max_matrix_channel,
            self.is32,
        );
        let s = &mut self.substream[substr];
        s.lossless_check_data = lc;

        Ok((vec![data], blockpos, lc as i64))
    }

    /// `read_access_unit`.
    pub fn decode_access_unit(&mut self, pkt: &Packet) -> oxideav_core::Result<Option<(AudioFrame, usize)>> {
        let buf = &pkt.data[..];
        let buf_size = buf.len();
        if buf_size < 4 {
            return Err(invalid("packet smaller than the access unit header"));
        }

        let length = ((u16::from_be_bytes([buf[0], buf[1]]) & 0xfff) * 2) as usize;
        if length < 4 || length > buf_size {
            return Err(invalid("access unit length out of range"));
        }

        let mut gb = BitReader::new(&buf[4..length]);
        self.is_major_sync_unit = false;
        if gb.show_bits(31) == 0xf8726fba >> 1 {
            let mh = self
                .apply_major_sync(&buf[4..length], &mut gb)
                .map_err(|e| {
                    self.params_valid = false;
                    e
                })?;
            let _ = mh;
            self.is_major_sync_unit = true;
        }

        if !self.params_valid {
            // FFmpeg logs a warning and skips the frame.
            return Ok(None);
        }

        // FFmpeg: header_size = 4, bumped by the major sync size only when
        // this access unit carries one.
        let header_size = 4 + if self.is_major_sync_unit {
            self.major_sync_header_size
        } else {
            0
        };
        let mut substr_header_size = 0usize;
        let mut substream_parity_present = [false; MAX_SUBSTREAMS];
        let mut substream_data_len = [0usize; MAX_SUBSTREAMS];
        let mut substream_start = 0usize;

        for substr in 0..self.num_substreams {
            let extraword_present = gb.get_bits(1) != 0;
            let nonrestart_substr = gb.get_bits(1) != 0;
            let checkdata_present = gb.get_bits(1) != 0;
            gb.skip(1);

            let end = (gb.get_bits(12) as usize) * 2;
            substr_header_size += 2;

            if extraword_present {
                if self.codec_is_mlp {
                    self.params_valid = false;
                    return Err(invalid("There must be no extraword for MLP."));
                }
                gb.skip(16);
                substr_header_size += 2;
            }

            if length < header_size + substr_header_size {
                self.params_valid = false;
                return Err(invalid("Insufficient data for headers"));
            }

            if !(nonrestart_substr ^ self.is_major_sync_unit) {
                self.params_valid = false;
                return Err(invalid("Invalid nonrestart_substr."));
            }

            let mut end = end;
            if end + header_size + substr_header_size > length {
                end = length - header_size - substr_header_size;
            }

            if end < substream_start {
                self.params_valid = false;
                return Err(invalid(format!(
                    "Indicated end offset of substream {substr} data is smaller than calculated start offset."
                )));
            }

            if substr > self.max_decoded_substream {
                continue;
            }

            substream_parity_present[substr] = checkdata_present;
            substream_data_len[substr] = end - substream_start;
            substream_start = end;
        }

        // Parity check over the AU header + substream headers.
        {
            let mut parity_bits = mlp_calculate_parity(&buf[..4]);
            parity_bits ^= mlp_calculate_parity(&buf[header_size..header_size + substr_header_size]);
            if ((parity_bits >> 4) ^ parity_bits) & 0xF != 0xF {
                self.params_valid = false;
                return Err(invalid("Parity check failed."));
            }
        }

        let mut offset = header_size + substr_header_size;

        for substr in 0..=self.max_decoded_substream {
            let sub_len = substream_data_len[substr];
            let mut gb = BitReader::new(&buf[offset..offset + sub_len]);
            let sub_start_bit = 0usize;

            self.matrix_changed = 0;
            self.filter_changed = [[0; NUM_FILTERS]; MAX_CHANNELS];

            self.substream[substr].blockpos = 0;

            let mut outcome = BlockOutcome::Continue;
            loop {
                if gb.get_bits(1) != 0 {
                    if gb.get_bits(1) != 0 {
                        // A restart header should be present.
                        if let Err(e) = self.read_restart_header(
                            &mut gb,
                            &buf[offset..],
                            substr,
                            sub_start_bit + 2,
                        ) {
                            outcome = BlockOutcome::NextSubstream;
                            let _ = e;
                            break;
                        }
                        self.substream[substr].restart_seen = true;
                    }

                    if !self.substream[substr].restart_seen {
                        outcome = BlockOutcome::NextSubstream;
                        break;
                    }
                    if let Err(e) = self.read_decoding_params(substr, &mut gb) {
                        outcome = BlockOutcome::NextSubstream;
                        let _ = e;
                        break;
                    }
                }

                if !self.substream[substr].restart_seen {
                    outcome = BlockOutcome::NextSubstream;
                    break;
                }

                // Channel-overlap heuristics from FFmpeg.
                let channels = self.out_channels;
                let substream_info = self.substream_info;
                if (channels == 6 && (substream_info >> 2) & 0x3 != 0x3)
                    || (channels == 8
                        && (substream_info >> 4) & 0x7 != 0x7
                        && (substream_info >> 4) & 0x7 != 0x6
                        && (substream_info >> 4) & 0x7 != 0x3)
                {
                    let overlaps_prev = substr > 0
                        && substr < self.max_decoded_substream
                        && self.substream[substr].min_channel
                            <= self.substream[substr - 1].max_channel;
                    if overlaps_prev {
                        outcome = BlockOutcome::NextSubstream;
                        break;
                    }
                }

                if substr != self.max_decoded_substream
                    && (self.substream[substr].coded_channels
                        & self.substream[self.max_decoded_substream].coded_channels)
                        != 0
                {
                    outcome = BlockOutcome::NextSubstream;
                    break;
                }

                if let Err(e) = self.read_block_data(&mut gb, substr) {
                    // FFmpeg returns the error straight out of the decoder.
                    return Err(e);
                }

                if gb.bits_read() >= sub_len * 8 {
                    outcome = BlockOutcome::LengthMismatch;
                    break;
                }

                if gb.get_bits(1) != 0 {
                    break;
                }
            }

            match outcome {
                BlockOutcome::Continue | BlockOutcome::NextSubstream => {}
                BlockOutcome::LengthMismatch => {
                    return Err(invalid(format!("substream {substr} length mismatch")));
                }
            }

            if outcome == BlockOutcome::Continue {
                // Only when the block loop ended normally do we read the
                // end-of-stream and check data.
                gb.align_16();

                if sub_len * 8 - gb.bits_read() >= 32 {
                    if gb.get_bits(16) != 0xD234 {
                        return Err(invalid("bad end-of-stream marker"));
                    }

                    let shorten_by = gb.get_bits(16);
                    let s = &mut self.substream[substr];
                    if !self.codec_is_mlp && shorten_by & 0x2000 != 0 {
                        let cut = (shorten_by & 0x1FFF) as usize;
                        s.blockpos -= cut.min(s.blockpos);
                    } else if self.codec_is_mlp && shorten_by != 0xD234 {
                        return Err(invalid("bad MLP shorten_by"));
                    }

                    s.end_of_stream = true;
                }

                if substream_parity_present[substr] {
                    if sub_len * 8 - gb.bits_read() != 16 {
                        return Err(invalid(format!("substream {substr} length mismatch")));
                    }

                    let parity = mlp_calculate_parity(&buf[offset..offset + sub_len - 2]);
                    let checksum = crc::checksum8(&buf[offset..offset + sub_len - 2]);

                    let read_parity = gb.get_bits(8);
                    if (read_parity ^ u32::from(parity)) & 0xff != 0xa9 {
                        // FFmpeg logs; not fatal.
                    }
                    let read_checksum = gb.get_bits(8);
                    if read_checksum != u32::from(checksum) {
                        // FFmpeg logs; not fatal.
                    }
                }

                if sub_len * 8 != gb.bits_read() {
                    return Err(invalid(format!("substream {substr} length mismatch")));
                }
            }

            offset += sub_len;
        }

        let (data, samples, _lc) = self.output_data(self.max_decoded_substream)?;

        // End-of-stream handling after successful output.
        for substr in 0..=self.max_decoded_substream {
            let s = &mut self.substream[substr];
            if s.end_of_stream {
                s.lossless_check_data = -1;
                s.end_of_stream = false;
                self.params_valid = false;
            }
        }

        let frame = AudioFrame {
            samples: samples as u32,
            pts: pkt.pts,
            data,
        };
        Ok(Some((frame, length)))
    }
}

/// Block-loop outcome (`goto next_substr` / length-mismatch / normal).
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockOutcome {
    Continue,
    NextSubstream,
    LengthMismatch,
}

/// `thd_channel_layout_extract_channel`: the `index`-th channel of `mask`
/// in `thd_channel_order`.
fn thd_channel_layout_extract_channel(mask: u64, mut index: usize) -> Option<u8> {
    if u64::from(mask_popcount(mask)) <= index as u64 {
        return None;
    }
    for &order in THD_CHANNEL_ORDER.iter() {
        let chan = order as usize;
        if mask & (1u64 << chan) != 0 {
            if index == 0 {
                return Some(chan as u8);
            }
            index -= 1;
        }
    }
    None
}

/// Index (LSB order) of channel `chan` within `mask`.
fn mask_channel_index(mask: u64, chan: usize) -> Option<usize> {
    if mask & (1u64 << chan) == 0 {
        return None;
    }
    let mut idx = 0;
    for bit in 0..chan {
        if mask & (1u64 << bit) != 0 {
            idx += 1;
        }
    }
    Some(idx)
}

/// Popcount of a channel mask.
fn mask_popcount(mask: u64) -> u32 {
    mask.count_ones()
}

impl Decoder for MlpDecoder {
    fn codec_id(&self) -> &CodecId {
        // The registry constructed us from one of these two ids; keep the
        // one `new` received so `codec_id()` matches the registration.
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        match self.decode_access_unit(packet) {
            Ok(Some((frame, _consumed))) => {
                self.pending = Some(frame);
                Ok(())
            }
            Ok(None) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        match self.pending.take() {
            Some(frame) => Ok(Frame::Audio(frame)),
            None => Err(CoreError::NeedMore),
        }
    }

    /// `mlp_decode_flush`: drop stream parameters; the next major sync
    /// re-establishes them. `lossless_check_data` restarts.
    fn flush(&mut self) -> oxideav_core::Result<()> {
        self.params_valid = false;
        for s in self.substream.iter_mut().take(self.max_decoded_substream + 1) {
            s.lossless_check_data = -1;
            s.end_of_stream = false;
        }
        self.pending = None;
        Ok(())
    }

    fn reset(&mut self) -> oxideav_core::Result<()> {
        self.flush()
    }
}

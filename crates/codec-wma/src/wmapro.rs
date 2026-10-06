// Ported from FFmpeg (commit 2da55bf): libavcodec/wmaprodec.c (the WMA Pro
// decoder; the XMA variants are not included), wmaprodata.h, wma_common.c
// (ff_wma_get_frame_len_bits) and wma.c (ff_wma_run_level_decode,
// ff_wma_get_large_val).
// GNU Lesser General Public License 2.1 or later

//! WMA Pro (Windows Media Audio 9 Professional) decoder.
//!
//! The bitstream is split into packets of `block_align` bytes; frames may
//! cross packet boundaries and are reassembled in a bit reservoir. Every
//! frame is split into subframes per channel, which are decoded (vector and
//! run-level coded coefficients, scale factors, channel transforms),
//! inverse quantized, transformed with an IMDCT and overlap-added with a
//! sine window.

use std::collections::VecDeque;

use crate::fft::{sine_window, vector_fmul_window_inplace, Imdct};
use crate::getbits::{GetBits, GetBitsState, PutBits};
use crate::vlc::VlcTable;
use crate::wma_common::{av_log2, wma_get_frame_len_bits};
use crate::wmapro_tables::*;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, SampleFormat};

const WMAPRO_MAX_CHANNELS: usize = 8;
const MAX_SUBFRAMES: usize = 32;
const MAX_BANDS: usize = 29;
const MAX_FRAMESIZE: usize = 32768;
const WMAPRO_BLOCK_MIN_BITS: u32 = 6;
const WMAPRO_BLOCK_MAX_BITS: u32 = 13;
const WMAPRO_BLOCK_MIN_SIZE: usize = 1 << WMAPRO_BLOCK_MIN_BITS;
const WMAPRO_BLOCK_MAX_SIZE: usize = 1 << WMAPRO_BLOCK_MAX_BITS;
const WMAPRO_BLOCK_SIZES: usize = (WMAPRO_BLOCK_MAX_BITS - WMAPRO_BLOCK_MIN_BITS + 1) as usize;
const HUFF_VEC1_SIZE: u32 = 101;
/// `AV_INPUT_BUFFER_PADDING_SIZE` after the reservoir.
const PADDING: usize = 64;

/// `VLC_INIT_FROM_LENGTHS` over `(symbol, length)` pairs: codes are
/// assigned in table order, so the order must be kept as is.
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

/// `ff_exp10` (libavutil/ffmath.h).
fn ff_exp10(x: f64) -> f64 {
    (std::f64::consts::LOG2_10 * x).exp2()
}

/// `ff_wma_get_large_val` (wma.c).
fn get_large_val(gb: &mut GetBits<'_>) -> u32 {
    // consumes up to 34 bits
    let mut n_bits = 8;
    if gb.get_bits1() != 0 {
        n_bits += 8;
        if gb.get_bits1() != 0 {
            n_bits += 8;
            if gb.get_bits1() != 0 {
                n_bits += 7;
            }
        }
    }
    gb.get_bits(n_bits)
}

/// Static VLC tables (`decode_init_static`).
struct Vlcs {
    sf: VlcTable,
    sf_rl: VlcTable,
    coef: [VlcTable; 2],
    vec4: VlcTable,
    vec2: VlcTable,
    vec1: VlcTable,
}

impl Vlcs {
    fn new() -> Result<Self> {
        Ok(Self {
            sf: vlc_from_pairs(&SCALE_TABLE, -60)?,
            sf_rl: vlc_from_pairs(&SCALE_RL_TABLE, 0)?,
            coef: [vlc_from_lens_syms(&COEF0_LENS, &COEF0_SYMS, 0)?, vlc_from_pairs(&COEF1_TABLE, 0)?],
            vec4: vlc_from_lens_syms(&VEC4_LENS, &VEC4_SYMS, -1)?,
            vec2: vlc_from_pairs(&VEC2_TABLE, -1)?,
            vec1: vlc_from_pairs(&VEC1_TABLE, 0)?,
        })
    }
}

/// Frame-specific decoder context for a single channel (`WMAProChannelCtx`).
#[derive(Clone)]
struct ChannelCtx {
    prev_block_len: usize,
    transmit_coefs: bool,
    num_subframes: usize,
    subframe_len: [usize; MAX_SUBFRAMES],
    cur_subframe: usize,
    decoded_samples: usize,
    grouped: bool,
    quant_step: i32,
    reuse_sf: bool,
    scale_factor_step: i32,
    max_scale_factor: i32,
    saved_scale_factors: [[i32; MAX_BANDS]; 2],
    scale_factor_idx: usize,
    /// The `saved_scale_factors` row `scale_factors` points at.
    scale_factors: usize,
    table_idx: usize,
    /// Offset of the subframe decode buffer (`coeffs`) in `out`.
    coeffs: usize,
    num_vec_coeffs: usize,
    out: Vec<f32>,
}

impl ChannelCtx {
    fn new() -> Self {
        Self {
            prev_block_len: 0,
            transmit_coefs: false,
            num_subframes: 0,
            subframe_len: [0; MAX_SUBFRAMES],
            cur_subframe: 0,
            decoded_samples: 0,
            grouped: false,
            quant_step: 0,
            reuse_sf: false,
            scale_factor_step: 0,
            max_scale_factor: 0,
            saved_scale_factors: [[0; MAX_BANDS]; 2],
            scale_factor_idx: 0,
            scale_factors: 0,
            table_idx: 0,
            coeffs: 0,
            num_vec_coeffs: 0,
            out: vec![0.0; WMAPRO_BLOCK_MAX_SIZE + WMAPRO_BLOCK_MAX_SIZE / 2],
        }
    }

    /// `subframe_len[cur_subframe]`; past the last subframe FFmpeg reads
    /// the neighbouring field, which is zero here.
    fn cur_subframe_len(&self) -> usize {
        self.subframe_len.get(self.cur_subframe).copied().unwrap_or(0)
    }
}

/// Channel group for channel transformations (`WMAProChannelGrp`).
#[derive(Clone)]
struct ChannelGrp {
    num_channels: usize,
    transform: bool,
    transform_band: [bool; MAX_BANDS],
    decorrelation_matrix: [f32; WMAPRO_MAX_CHANNELS * WMAPRO_MAX_CHANNELS],
    /// Channel indexes whose coefficients the transform reads and writes.
    channel_data: [usize; WMAPRO_MAX_CHANNELS],
}

impl Default for ChannelGrp {
    fn default() -> Self {
        Self {
            num_channels: 0,
            transform: false,
            transform_band: [false; MAX_BANDS],
            decorrelation_matrix: [0.0; WMAPRO_MAX_CHANNELS * WMAPRO_MAX_CHANNELS],
            channel_data: [0; WMAPRO_MAX_CHANNELS],
        }
    }
}

/// Main decoder context (`WMAProDecodeCtx`).
pub struct WmaProDecoder {
    codec_id: CodecId,
    sample_rate: u32,
    block_align: usize,
    vlcs: Vlcs,
    sin64: [f32; 33],
    frame_data: Vec<u8>,
    pb: PutBits,
    tx: Vec<Imdct>,
    tmp: Vec<f32>,
    windows: Vec<Vec<f32>>,

    // frame size dependent frame information (set during initialization)
    len_prefix: bool,
    dynamic_range_compression: bool,
    bits_per_sample: u32,
    samples_per_frame: usize,
    trim_start: usize,
    trim_end: usize,
    log2_frame_size: u32,
    lfe_channel: i32,
    max_num_subframes: usize,
    subframe_len_bits: u32,
    max_subframe_len_bit: bool,
    min_samples_per_subframe: usize,
    num_sfb: [usize; WMAPRO_BLOCK_SIZES],
    sfb_offsets: [[usize; MAX_BANDS]; WMAPRO_BLOCK_SIZES],
    sf_offsets: [[[usize; MAX_BANDS]; WMAPRO_BLOCK_SIZES]; WMAPRO_BLOCK_SIZES],
    subwoofer_cutoffs: [usize; WMAPRO_BLOCK_SIZES],

    // packet decode state
    next_packet_start: usize,
    packet_offset: u32,
    packet_sequence_number: u8,
    num_saved_bits: usize,
    frame_offset: usize,
    packet_loss: bool,
    packet_done: bool,
    /// Set by `flush` (end of stream), cleared by `reset`.
    eof: bool,

    // frame decode state
    /// `s->gb`: the reader over the reservoir in `frame_data`.
    gb: GetBitsState,
    buf_bit_size: usize,
    skip_frame: bool,
    parsed_all_subframes: bool,

    // subframe/block decode state
    subframe_len: usize,
    nb_channels: usize,
    channels_for_cur_subframe: usize,
    channel_indexes_for_cur_subframe: [usize; WMAPRO_MAX_CHANNELS],
    num_bands: usize,
    transmit_num_vec_coeffs: bool,
    table_idx: usize,
    esc_len: u32,

    num_chgroups: usize,
    chgroup: [ChannelGrp; WMAPRO_MAX_CHANNELS],
    channel: Vec<ChannelCtx>,

    pending: VecDeque<AudioFrame>,
}

impl WmaProDecoder {
    /// `wmapro_decode_init` / `decode_init` (wmaprodec.c), WMA Pro only.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let channels = params.channels.unwrap_or(0) as usize;
        let sample_rate = params.sample_rate.unwrap_or(0);
        // FFmpeg's AVCodecContext::block_align travels in the codec options.
        let block_align = params
            .options
            .get("block_align")
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&b| b > 0)
            .ok_or_else(|| Error::invalid("wmapro: block_align is not set"))?;
        if sample_rate == 0 {
            return Err(Error::invalid("wmapro: sample rate is not set"));
        }

        let edata = &params.extradata;
        if edata.len() < 18 {
            return Err(Error::unsupported("wmapro: unknown extradata size"));
        }
        let decode_flags = u16::from_le_bytes([edata[14], edata[15]]) as u32;
        let channel_mask = u32::from_le_bytes([edata[2], edata[3], edata[4], edata[5]]);
        let bits_per_sample = u16::from_le_bytes([edata[0], edata[1]]) as u32;
        let nb_channels = if channel_mask != 0 { channel_mask.count_ones() as usize } else { channels };
        if !(1..=32).contains(&bits_per_sample) {
            return Err(Error::unsupported(format!("wmapro: bits per sample is {bits_per_sample}")));
        }

        // generic init
        let log2_frame_size = av_log2(block_align as u32) + 4;
        if log2_frame_size > 25 {
            return Err(Error::unsupported("wmapro: large block align"));
        }
        let len_prefix = decode_flags & 0x40 != 0;

        // get frame len
        let bits = wma_get_frame_len_bits(sample_rate, 3, decode_flags);
        if bits > WMAPRO_BLOCK_MAX_BITS {
            return Err(Error::unsupported("wmapro: 14-bit block sizes"));
        }
        let samples_per_frame = 1usize << bits;

        // subframe info
        let log2_max_num_subframes = ((decode_flags & 0x38) >> 3) as usize;
        let max_num_subframes = 1usize << log2_max_num_subframes;
        let max_subframe_len_bit = max_num_subframes == 16 || max_num_subframes == 4;
        let subframe_len_bits = av_log2(log2_max_num_subframes as u32) + 1;
        let num_possible_block_sizes = log2_max_num_subframes + 1;
        let min_samples_per_subframe = samples_per_frame / max_num_subframes;
        let dynamic_range_compression = decode_flags & 0x80 != 0;

        if max_num_subframes > MAX_SUBFRAMES {
            return Err(Error::invalid(format!("wmapro: invalid number of subframes {max_num_subframes}")));
        }
        if min_samples_per_subframe < WMAPRO_BLOCK_MIN_SIZE {
            return Err(Error::invalid(format!(
                "wmapro: min_samples_per_subframe of {min_samples_per_subframe} too small"
            )));
        }
        if nb_channels == 0 {
            return Err(Error::invalid("wmapro: invalid number of channels"));
        }
        if nb_channels > WMAPRO_MAX_CHANNELS || nb_channels > channels {
            return Err(Error::unsupported(format!("wmapro: {nb_channels} channels")));
        }

        // extract lfe channel position
        let mut lfe_channel = -1i32;
        if channel_mask & 8 != 0 {
            let mut mask = 1u32;
            while mask < 16 {
                if channel_mask & mask != 0 {
                    lfe_channel += 1;
                }
                mask <<= 1;
            }
        }

        // calculate number of scale factor bands and their offsets for
        // every possible block size
        let rate = sample_rate as i64;
        let mut num_sfb = [0usize; WMAPRO_BLOCK_SIZES];
        let mut sfb_offsets = [[0usize; MAX_BANDS]; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            let subframe_len = (samples_per_frame >> i) as i64;
            let mut band = 1usize;
            sfb_offsets[i][0] = 0;
            let mut x = 0usize;
            while x < MAX_BANDS - 1 && (sfb_offsets[i][band - 1] as i64) < subframe_len {
                let offset = ((subframe_len * 2 * CRITICAL_FREQ_WMAPRO[x] as i64) / rate + 2) & !3;
                if offset > sfb_offsets[i][band - 1] as i64 {
                    sfb_offsets[i][band] = offset as usize;
                    band += 1;
                }
                if offset >= subframe_len {
                    break;
                }
                x += 1;
            }
            sfb_offsets[i][band - 1] = subframe_len as usize;
            if band < 2 {
                return Err(Error::invalid("wmapro: num_sfb invalid"));
            }
            num_sfb[i] = band - 1;
        }

        // Scale factors can be shared between blocks of different size as
        // every block has a different scale factor band layout. The matrix
        // sf_offsets is needed to find the correct scale factor.
        let mut sf_offsets = [[[0usize; MAX_BANDS]; WMAPRO_BLOCK_SIZES]; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            for b in 0..num_sfb[i] {
                let offset = ((sfb_offsets[i][b] + sfb_offsets[i][b + 1] - 1) << i) >> 1;
                for x in 0..num_possible_block_sizes {
                    let mut v = 0usize;
                    while (sfb_offsets[x][v + 1] << x) < offset {
                        v += 1;
                        if v + 1 >= MAX_BANDS {
                            return Err(Error::invalid("wmapro: scale factor resample overflow"));
                        }
                    }
                    sf_offsets[i][x][b] = v;
                }
            }
        }

        // init MDCT, FIXME: only init needed sizes
        let mut tx = Vec::with_capacity(WMAPRO_BLOCK_SIZES);
        for i in 0..WMAPRO_BLOCK_SIZES {
            let scale = (1.0 / (1u64 << (WMAPRO_BLOCK_MIN_BITS as usize + i - 1)) as f64
                / (1u64 << (bits_per_sample - 1)) as f64) as f32;
            tx.push(Imdct::new(1 << (WMAPRO_BLOCK_MIN_BITS as usize + i), scale as f64));
        }

        // init MDCT windows: simple sine window
        let windows = (0..WMAPRO_BLOCK_SIZES).map(|i| sine_window(1 << (WMAPRO_BLOCK_MIN_BITS as usize + i))).collect();

        // calculate subwoofer cutoff values
        let mut subwoofer_cutoffs = [0usize; WMAPRO_BLOCK_SIZES];
        for i in 0..num_possible_block_sizes {
            let block_size = (samples_per_frame >> i) as i64;
            let cutoff = (440 * block_size + 3 * (sample_rate as i64 >> 1) - 1) / sample_rate as i64;
            subwoofer_cutoffs[i] = cutoff.clamp(4, block_size) as usize;
        }

        // calculate sine values for the decorrelation matrix
        let mut sin64 = [0f32; 33];
        for (i, s) in sin64.iter_mut().enumerate() {
            *s = (i as f64 * std::f64::consts::PI / 64.0).sin() as f32;
        }

        let mut channel = vec![ChannelCtx::new(); nb_channels];
        // init previous block len
        for ch in channel.iter_mut() {
            ch.prev_block_len = samples_per_frame;
        }

        Ok(Self {
            codec_id: params.codec_id.clone(),
            sample_rate,
            block_align,
            vlcs: Vlcs::new()?,
            sin64,
            frame_data: vec![0u8; MAX_FRAMESIZE + PADDING],
            pb: PutBits::default(),
            tx,
            tmp: vec![0.0; WMAPRO_BLOCK_MAX_SIZE],
            windows,
            len_prefix,
            dynamic_range_compression,
            bits_per_sample,
            samples_per_frame,
            trim_start: 0,
            trim_end: 0,
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
            next_packet_start: 0,
            packet_offset: 0,
            packet_sequence_number: 0,
            num_saved_bits: 0,
            frame_offset: 0,
            // frame info
            packet_loss: true,
            packet_done: false,
            eof: false,
            gb: GetBitsState::default(),
            buf_bit_size: 0,
            // skip first frame
            skip_frame: true,
            parsed_all_subframes: false,
            subframe_len: 0,
            nb_channels,
            channels_for_cur_subframe: 0,
            channel_indexes_for_cur_subframe: [0; WMAPRO_MAX_CHANNELS],
            num_bands: 0,
            transmit_num_vec_coeffs: false,
            table_idx: 0,
            esc_len: 0,
            num_chgroups: 0,
            chgroup: Default::default(),
            channel,
            pending: VecDeque::new(),
        })
    }

    /// `decode_subframe_length`: the subframe length in samples.
    fn decode_subframe_length(&self, gb: &mut GetBits<'_>, offset: usize) -> Result<usize> {
        // no need to read from the bitstream when only one length is possible
        if offset == self.samples_per_frame - self.min_samples_per_subframe {
            return Ok(self.min_samples_per_subframe);
        }
        if gb.bits_left() < 1 {
            return Err(Error::invalid("wmapro: no bits for the subframe length"));
        }

        // 1 bit indicates if the subframe is of maximum length
        let mut frame_len_shift = 0;
        if self.max_subframe_len_bit {
            if gb.get_bits1() != 0 {
                frame_len_shift = 1 + gb.get_bits(self.subframe_len_bits - 1);
            }
        } else {
            frame_len_shift = gb.get_bits(self.subframe_len_bits);
        }

        let subframe_len = self.samples_per_frame >> frame_len_shift;

        // sanity check the length
        if subframe_len < self.min_samples_per_subframe || subframe_len > self.samples_per_frame {
            return Err(Error::invalid(format!("wmapro: broken frame: subframe_len {subframe_len}")));
        }
        Ok(subframe_len)
    }

    /// `decode_tilehdr`: how the frame is split into subframes per channel.
    fn decode_tilehdr(&mut self, gb: &mut GetBits<'_>) -> Result<()> {
        // sum of samples for all currently known subframes of a channel
        let mut num_samples = [0usize; WMAPRO_MAX_CHANNELS];
        // flag indicating if a channel contains the current subframe
        let mut contains_subframe = [false; WMAPRO_MAX_CHANNELS];
        // number of channels that contain the current subframe
        let mut channels_for_cur_subframe = self.nb_channels;
        // flag indicating that all channels use the same subframe offsets and sizes
        let mut fixed_channel_layout = false;
        // smallest sum of samples (channels with this length will be processed first)
        let mut min_channel_len = 0usize;

        // reset tiling information
        for c in 0..self.nb_channels {
            self.channel[c].num_subframes = 0;
        }

        if self.max_num_subframes == 1 || gb.get_bits1() != 0 {
            fixed_channel_layout = true;
        }

        // loop until the frame data is split between the subframes
        loop {
            // check which channels contain the subframe
            for c in 0..self.nb_channels {
                if num_samples[c] == min_channel_len {
                    contains_subframe[c] = if fixed_channel_layout
                        || channels_for_cur_subframe == 1
                        || min_channel_len == self.samples_per_frame - self.min_samples_per_subframe
                    {
                        true
                    } else {
                        gb.get_bits1() != 0
                    };
                } else {
                    contains_subframe[c] = false;
                }
            }

            // get subframe length, subframe_len == 0 is not allowed
            let subframe_len = self.decode_subframe_length(gb, min_channel_len)?;

            // add subframes to the individual channels and find new min_channel_len
            min_channel_len += subframe_len;
            for c in 0..self.nb_channels {
                let chan = &mut self.channel[c];
                if contains_subframe[c] {
                    if chan.num_subframes >= MAX_SUBFRAMES {
                        return Err(Error::invalid("wmapro: broken frame: num subframes > 31"));
                    }
                    chan.subframe_len[chan.num_subframes] = subframe_len;
                    num_samples[c] += subframe_len;
                    chan.num_subframes += 1;
                    if num_samples[c] > self.samples_per_frame {
                        return Err(Error::invalid("wmapro: broken frame: channel len > samples_per_frame"));
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
        // (FFmpeg also records each subframe's offset here, for logging only.)
        Ok(())
    }

    /// `decode_decorrelation_matrix`.
    fn decode_decorrelation_matrix(&mut self, gb: &mut GetBits<'_>, g: usize) {
        let nb_channels = self.nb_channels;
        let sin64 = self.sin64;
        let chgroup = &mut self.chgroup[g];
        let n = chgroup.num_channels;
        let mut rotation_offset = [0i32; WMAPRO_MAX_CHANNELS * WMAPRO_MAX_CHANNELS];
        chgroup.decorrelation_matrix[..nb_channels * nb_channels].fill(0.0);

        for r in rotation_offset.iter_mut().take(n * (n - 1) >> 1) {
            *r = gb.get_bits(6) as i32;
        }

        for i in 0..n {
            chgroup.decorrelation_matrix[n * i + i] = if gb.get_bits1() != 0 { 1.0 } else { -1.0 };
        }

        let mut offset = 0;
        let m = &mut chgroup.decorrelation_matrix;
        for i in 1..n {
            for x in 0..i {
                for y in 0..i + 1 {
                    let v1 = m[x * n + y];
                    let v2 = m[i * n + y];
                    let rot = rotation_offset[offset + x] as usize;
                    let (sinv, cosv) = if rot < 32 {
                        (sin64[rot], sin64[32 - rot])
                    } else {
                        (sin64[64 - rot], -sin64[rot - 32])
                    };
                    m[y + x * n] = (v1 * sinv) - (v2 * cosv);
                    m[y + i * n] = (v1 * cosv) + (v2 * sinv);
                }
            }
            offset += i;
        }
    }

    /// `decode_channel_transform`: channel transformation parameters.
    fn decode_channel_transform(&mut self, gb: &mut GetBits<'_>) -> Result<()> {
        // in the one channel case channel transforms are pointless
        self.num_chgroups = 0;
        if self.nb_channels > 1 {
            let mut remaining_channels = self.channels_for_cur_subframe;

            if gb.get_bits1() != 0 {
                return Err(Error::unsupported("wmapro: channel transform bit"));
            }

            while remaining_channels > 0 && self.num_chgroups < self.channels_for_cur_subframe {
                let g = self.num_chgroups;
                let mut num_channels = 0usize;
                let mut data_count = 0usize;
                self.chgroup[g].transform = false;

                // decode channel mask
                if remaining_channels > 2 {
                    for i in 0..self.channels_for_cur_subframe {
                        let channel_idx = self.channel_indexes_for_cur_subframe[i];
                        if !self.channel[channel_idx].grouped && gb.get_bits1() != 0 {
                            num_channels += 1;
                            self.channel[channel_idx].grouped = true;
                            self.chgroup[g].channel_data[data_count] = channel_idx;
                            data_count += 1;
                        }
                    }
                } else {
                    num_channels = remaining_channels;
                    for i in 0..self.channels_for_cur_subframe {
                        let channel_idx = self.channel_indexes_for_cur_subframe[i];
                        if !self.channel[channel_idx].grouped {
                            self.chgroup[g].channel_data[data_count] = channel_idx;
                            data_count += 1;
                        }
                        self.channel[channel_idx].grouped = true;
                    }
                }
                self.chgroup[g].num_channels = num_channels;

                // decode transform type
                if num_channels == 2 {
                    if gb.get_bits1() != 0 {
                        if gb.get_bits1() != 0 {
                            return Err(Error::unsupported("wmapro: unknown channel transform type"));
                        }
                    } else {
                        self.chgroup[g].transform = true;
                        let m = &mut self.chgroup[g].decorrelation_matrix;
                        if self.nb_channels == 2 {
                            m[..4].copy_from_slice(&[1.0, -1.0, 1.0, 1.0]);
                        } else {
                            // cos(pi/4)
                            m[..4].copy_from_slice(&[0.70703125, -0.70703125, 0.70703125, 0.70703125]);
                        }
                    }
                } else if num_channels > 2 && gb.get_bits1() != 0 {
                    self.chgroup[g].transform = true;
                    if gb.get_bits1() != 0 {
                        self.decode_decorrelation_matrix(gb, g);
                    } else if num_channels <= 6 {
                        // FIXME: more than 6 coupled channels not supported
                        const OFFSETS: [usize; 7] = [0, 0, 1, 5, 14, 30, 55];
                        let start = OFFSETS[num_channels];
                        let n2 = num_channels * num_channels;
                        self.chgroup[g].decorrelation_matrix[..n2]
                            .copy_from_slice(&DEFAULT_DECORRELATION_MATRICES[start..start + n2]);
                    }
                }

                // decode transform on / off
                if self.chgroup[g].transform {
                    if gb.get_bits1() == 0 {
                        // transform can be enabled for individual bands
                        for i in 0..self.num_bands {
                            self.chgroup[g].transform_band[i] = gb.get_bits1() != 0;
                        }
                    } else {
                        self.chgroup[g].transform_band[..self.num_bands].fill(true);
                    }
                }
                remaining_channels = remaining_channels.saturating_sub(num_channels);
                self.num_chgroups += 1;
            }
        }
        Ok(())
    }

    /// `decode_coeffs`: extract the coefficients of channel `c`.
    fn decode_coeffs(&mut self, gb: &mut GetBits<'_>, c: usize) -> Result<()> {
        // Integers 0..15 as single-precision floats.
        const FVAL_TAB: [u32; 16] = [
            0x00000000, 0x3f800000, 0x40000000, 0x40400000, 0x40800000, 0x40a00000, 0x40c00000, 0x40e00000,
            0x41000000, 0x41100000, 0x41200000, 0x41300000, 0x41400000, 0x41500000, 0x41600000, 0x41700000,
        ];
        let vlcs = &self.vlcs;
        let subframe_len = self.subframe_len;
        let transmit_num_vec_coeffs = self.transmit_num_vec_coeffs;
        let ci = &mut self.channel[c];
        let mut rl_mode = false;
        let mut cur_coeff = 0usize;
        let mut num_zeros = 0usize;

        let vlctable = gb.get_bits1() as usize;
        let (run, level): (&[u16], &[f32]) =
            if vlctable != 0 { (&COEF1_RUN, &COEF1_LEVEL) } else { (&COEF0_RUN, &COEF0_LEVEL) };

        // decode vector coefficients (consumes up to 167 bits per iteration
        // for 4 vector coded large values)
        while (transmit_num_vec_coeffs || !rl_mode) && cur_coeff + 3 < ci.num_vec_coeffs {
            let mut vals = [0u32; 4];
            let idx = gb.get_vlc(&vlcs.vec4);
            if idx < 0 {
                for i in (0..4).step_by(2) {
                    let idx = gb.get_vlc(&vlcs.vec2);
                    if idx < 0 {
                        let mut v0 = gb.get_vlc(&vlcs.vec1) as u32;
                        if v0 == HUFF_VEC1_SIZE - 1 {
                            v0 = v0.wrapping_add(get_large_val(gb));
                        }
                        let mut v1 = gb.get_vlc(&vlcs.vec1) as u32;
                        if v1 == HUFF_VEC1_SIZE - 1 {
                            v1 = v1.wrapping_add(get_large_val(gb));
                        }
                        vals[i] = (v0 as f32).to_bits();
                        vals[i + 1] = (v1 as f32).to_bits();
                    } else {
                        vals[i] = FVAL_TAB[(idx >> 4) as usize & 0xF];
                        vals[i + 1] = FVAL_TAB[(idx & 0xF) as usize];
                    }
                }
            } else {
                vals[0] = FVAL_TAB[(idx >> 12) as usize & 0xF];
                vals[1] = FVAL_TAB[((idx >> 8) & 0xF) as usize];
                vals[2] = FVAL_TAB[((idx >> 4) & 0xF) as usize];
                vals[3] = FVAL_TAB[(idx & 0xF) as usize];
            }

            // decode sign
            for &v in &vals {
                let slot = ci.coeffs + cur_coeff;
                if v != 0 {
                    let sign = gb.get_bits1().wrapping_sub(1);
                    ci.out[slot] = f32::from_bits(v ^ (sign << 31));
                    num_zeros = 0;
                } else {
                    ci.out[slot] = 0.0;
                    // switch to run level mode when subframe_len / 128 zeros
                    // were found in a row
                    num_zeros += 1;
                    rl_mode |= num_zeros > subframe_len >> 8;
                }
                cur_coeff += 1;
            }
        }

        // decode run level coded coefficients
        if cur_coeff < subframe_len {
            let coeffs = &mut ci.out[ci.coeffs..ci.coeffs + subframe_len];
            coeffs[cur_coeff..].fill(0.0);
            run_level_decode(gb, &vlcs.coef[vlctable], level, run, coeffs, cur_coeff, subframe_len, self.esc_len)?;
        }
        Ok(())
    }

    /// `decode_scale_factors`.
    fn decode_scale_factors(&mut self, gb: &mut GetBits<'_>) -> Result<()> {
        // should never consume more than 5344 bits
        // MAX_CHANNELS * (1 +  MAX_BANDS * 23)
        let num_bands = self.num_bands;
        let table_idx = self.table_idx;
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let sf_offsets = self.sf_offsets[table_idx][self.channel[c].table_idx];
            let ch = &mut self.channel[c];
            let sf = ch.scale_factor_idx ^ 1;
            ch.scale_factors = sf;

            // resample scale factors for the new block size as the scale
            // factors might need to be resampled several times before some
            // new values are transmitted, a backup of the last transmitted
            // scale factors is kept in saved_scale_factors
            if ch.reuse_sf {
                for b in 0..num_bands {
                    ch.saved_scale_factors[sf][b] = ch.saved_scale_factors[ch.scale_factor_idx][sf_offsets[b]];
                }
            }

            if ch.cur_subframe == 0 || gb.get_bits1() != 0 {
                if !ch.reuse_sf {
                    // decode DPCM coded scale factors
                    ch.scale_factor_step = gb.get_bits(2) as i32 + 1;
                    let mut val = 45 / ch.scale_factor_step;
                    for b in 0..num_bands {
                        val += gb.get_vlc(&self.vlcs.sf);
                        ch.saved_scale_factors[sf][b] = val;
                    }
                } else {
                    // run level decode differences to the resampled factors
                    let mut b = 0usize;
                    while b < num_bands {
                        let idx = gb.get_vlc(&self.vlcs.sf_rl);
                        let (skip, val, sign);
                        if idx == 0 {
                            let code = gb.get_bits(14);
                            val = (code >> 6) as i32;
                            sign = (code & 1) as i32 - 1;
                            skip = ((code & 0x3f) >> 1) as usize;
                        } else if idx == 1 {
                            break;
                        } else {
                            let idx = usize::try_from(idx).map_err(|_| Error::invalid("wmapro: invalid scale factor code"))?;
                            skip = SCALE_RL_RUN[idx] as usize;
                            val = SCALE_RL_LEVEL[idx] as i32;
                            sign = gb.get_bits1() as i32 - 1;
                        }

                        b += skip;
                        if b >= num_bands {
                            return Err(Error::invalid("wmapro: invalid scale factor coding"));
                        }
                        ch.saved_scale_factors[sf][b] += (val ^ sign) - sign;
                        b += 1;
                    }
                }
                // swap buffers
                ch.scale_factor_idx ^= 1;
                ch.table_idx = table_idx;
                ch.reuse_sf = true;
            }

            // calculate new scale factor maximum
            let factors = &ch.saved_scale_factors[ch.scale_factors][..num_bands.max(1)];
            ch.max_scale_factor = factors.iter().copied().max().unwrap_or(0);
        }
        Ok(())
    }

    /// `inverse_channel_transform`: reconstruct the individual channel data.
    fn inverse_channel_transform(&mut self) {
        let subframe_len = self.subframe_len;
        let sfb = self.sfb_offsets[self.table_idx];
        for i in 0..self.num_chgroups {
            let g = &self.chgroup[i];
            if !g.transform {
                continue;
            }
            let num_channels = g.num_channels;
            let mut data = [0f32; WMAPRO_MAX_CHANNELS];
            for b in 0..self.num_bands {
                let (start, end) = (sfb[b], sfb[b + 1].min(subframe_len));
                if g.transform_band[b] {
                    // multiply values with the decorrelation_matrix
                    for y in start..end {
                        for (k, &ch) in g.channel_data[..num_channels].iter().enumerate() {
                            let chan = &self.channel[ch];
                            data[k] = chan.out[chan.coeffs + y];
                        }
                        let mut mat = g.decorrelation_matrix.iter();
                        for &ch in &g.channel_data[..num_channels] {
                            let mut sum = 0f32;
                            for &d in &data[..num_channels] {
                                sum += d * mat.next().copied().unwrap_or(0.0);
                            }
                            let chan = &mut self.channel[ch];
                            let pos = chan.coeffs + y;
                            chan.out[pos] = sum;
                        }
                    }
                } else if self.nb_channels == 2 && end > start {
                    for &ch in &g.channel_data[..2] {
                        let chan = &mut self.channel[ch];
                        let base = chan.coeffs;
                        for v in &mut chan.out[base + start..base + end] {
                            *v *= (181.0 / 128.0) as f32;
                        }
                    }
                }
            }
        }
    }

    /// `wmapro_window`: apply the sine window and reconstruct the output.
    fn wmapro_window(&mut self) {
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let chan = &mut self.channel[c];
            let mut winlen = chan.prev_block_len;
            let mut start = chan.coeffs - (winlen >> 1);

            if self.subframe_len < winlen {
                start += (winlen - self.subframe_len) >> 1;
                winlen = self.subframe_len;
            }

            let window = &self.windows[av_log2(winlen as u32) as usize - WMAPRO_BLOCK_MIN_BITS as usize];
            winlen >>= 1;
            vector_fmul_window_inplace(&mut chan.out[start..start + 2 * winlen], window, winlen);

            chan.prev_block_len = self.subframe_len;
        }
    }

    /// `decode_subframe`: decode a single subframe (block).
    fn decode_subframe(&mut self, gb: &mut GetBits<'_>) -> Result<()> {
        let mut offset = self.samples_per_frame;
        let mut subframe_len = self.samples_per_frame;
        let mut total_samples = (self.samples_per_frame * self.nb_channels) as i64;
        let mut transmit_coeffs = false;

        // reset channel context and find the next block offset and size
        // == the next block of the channel with the smallest number of
        // decoded samples
        for i in 0..self.nb_channels {
            let chan = &mut self.channel[i];
            chan.grouped = false;
            if offset > chan.decoded_samples {
                offset = chan.decoded_samples;
                subframe_len = chan.cur_subframe_len();
            }
        }

        // get a list of all channels that contain the estimated block
        self.channels_for_cur_subframe = 0;
        for i in 0..self.nb_channels {
            let chan = &mut self.channel[i];
            // subtract already processed samples
            total_samples -= chan.decoded_samples as i64;

            // and count if there are multiple subframes that match our profile
            if offset == chan.decoded_samples && subframe_len == chan.cur_subframe_len() {
                total_samples -= subframe_len as i64;
                chan.decoded_samples += subframe_len;
                self.channel_indexes_for_cur_subframe[self.channels_for_cur_subframe] = i;
                self.channels_for_cur_subframe += 1;
            }
        }

        // check if the frame will be complete after processing the
        // estimated block
        if total_samples == 0 {
            self.parsed_all_subframes = true;
        }
        if subframe_len == 0 {
            return Err(Error::invalid("wmapro: empty subframe"));
        }

        // calculate number of scale factor bands and their offsets
        self.table_idx = av_log2((self.samples_per_frame / subframe_len) as u32) as usize;
        if self.table_idx >= WMAPRO_BLOCK_SIZES {
            return Err(Error::invalid("wmapro: subframe too short"));
        }
        self.num_bands = self.num_sfb[self.table_idx];
        let cur_subwoofer_cutoff = self.subwoofer_cutoffs[self.table_idx];

        // configure the decoder for the current subframe
        offset += self.samples_per_frame >> 1;

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            self.channel[c].coeffs = offset;
        }

        self.subframe_len = subframe_len;
        self.esc_len = av_log2(subframe_len as u32 - 1) + 1;

        // skip extended header if any
        if gb.get_bits1() != 0 {
            let mut num_fill_bits = gb.get_bits(2) as usize;
            if num_fill_bits == 0 {
                let len = gb.get_bits(4);
                num_fill_bits = gb.get_bitsz(len) as usize + 1;
            }

            if gb.bits_count() + num_fill_bits > self.num_saved_bits {
                return Err(Error::invalid("wmapro: invalid number of fill bits"));
            }

            gb.skip_bits_long(num_fill_bits as i64);
        }

        // no idea for what the following bit is used
        if gb.get_bits1() != 0 {
            return Err(Error::unsupported("wmapro: reserved bit"));
        }

        self.decode_channel_transform(gb)?;

        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            self.channel[c].transmit_coefs = gb.get_bits1() != 0;
            if self.channel[c].transmit_coefs {
                transmit_coeffs = true;
            }
        }

        if transmit_coeffs {
            let mut quant_step = (90 * self.bits_per_sample as i32) >> 4;

            // decode number of vector coded coefficients
            self.transmit_num_vec_coeffs = gb.get_bits1() != 0;
            if self.transmit_num_vec_coeffs {
                let num_bits = av_log2(((subframe_len + 3) / 4) as u32) + 1;
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    let num_vec_coeffs = (gb.get_bits(num_bits) as usize) << 2;
                    if num_vec_coeffs > subframe_len {
                        return Err(Error::invalid(format!("wmapro: num_vec_coeffs {num_vec_coeffs} is too large")));
                    }
                    self.channel[c].num_vec_coeffs = num_vec_coeffs;
                }
            } else {
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    self.channel[c].num_vec_coeffs = subframe_len;
                }
            }

            // decode quantization step
            let mut step = gb.get_sbits(6);
            quant_step += step;
            if step == -32 || step == 31 {
                let sign = (step == 31) as i32 - 1;
                let mut quant = 0i32;
                while gb.bits_count() + 5 < self.num_saved_bits {
                    step = gb.get_bits(5) as i32;
                    if step != 31 {
                        break;
                    }
                    quant += 31;
                }
                quant_step += ((quant + step) ^ sign) - sign;
            }

            // decode quantization step modifiers for every channel
            if self.channels_for_cur_subframe == 1 {
                let c = self.channel_indexes_for_cur_subframe[0];
                self.channel[c].quant_step = quant_step;
            } else {
                let modifier_len = gb.get_bits(3);
                for i in 0..self.channels_for_cur_subframe {
                    let c = self.channel_indexes_for_cur_subframe[i];
                    self.channel[c].quant_step = quant_step;
                    if gb.get_bits1() != 0 {
                        if modifier_len != 0 {
                            self.channel[c].quant_step += gb.get_bits(modifier_len) as i32 + 1;
                        } else {
                            self.channel[c].quant_step += 1;
                        }
                    }
                }
            }

            // decode scale factors
            self.decode_scale_factors(gb)?;
        }

        // parse coefficients
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            if self.channel[c].transmit_coefs && gb.bits_count() < self.num_saved_bits {
                // FFmpeg ignores coefficient decoding errors here.
                let _ = self.decode_coeffs(gb, c);
            } else {
                let chan = &mut self.channel[c];
                let base = chan.coeffs;
                chan.out[base..base + subframe_len].fill(0.0);
            }
        }

        if transmit_coeffs {
            let tx_idx = av_log2(subframe_len as u32) as usize - WMAPRO_BLOCK_MIN_BITS as usize;
            // reconstruct the per channel data
            self.inverse_channel_transform();
            let sfb = self.sfb_offsets[self.table_idx];
            for i in 0..self.channels_for_cur_subframe {
                let c = self.channel_indexes_for_cur_subframe[i];
                let chan = &mut self.channel[c];
                let sf = &chan.saved_scale_factors[chan.scale_factors];

                if c as i32 == self.lfe_channel && cur_subwoofer_cutoff < subframe_len {
                    self.tmp[cur_subwoofer_cutoff..subframe_len].fill(0.0);
                }

                // inverse quantization and rescaling
                for b in 0..self.num_bands {
                    let end = sfb[b + 1].min(subframe_len);
                    let exp = chan.quant_step - (chan.max_scale_factor - sf[b]) * chan.scale_factor_step;
                    let quant = ff_exp10(exp as f64 / 20.0) as f32;
                    let start = sfb[b];
                    for k in start..end {
                        self.tmp[k] = chan.out[chan.coeffs + k] * quant;
                    }
                }

                // apply imdct (imdct_half == DCTIV with reverse)
                let base = chan.coeffs;
                self.tx[tx_idx].imdct_half(&mut chan.out[base..base + subframe_len], &self.tmp);
            }
        }

        // window and overlapp-add
        self.wmapro_window();

        // handled one subframe
        for i in 0..self.channels_for_cur_subframe {
            let c = self.channel_indexes_for_cur_subframe[i];
            let chan = &mut self.channel[c];
            if chan.cur_subframe >= chan.num_subframes {
                return Err(Error::invalid("wmapro: broken subframe"));
            }
            chan.cur_subframe += 1;
        }
        Ok(())
    }

    /// `decode_frame`: decode one WMA frame from the reservoir. Returns
    /// whether the trailer bit announces more frames; sets `frame` when the
    /// frame is output.
    fn decode_frame(&mut self, frame: &mut Option<AudioFrame>) -> bool {
        let data = std::mem::take(&mut self.frame_data);
        let mut gb = GetBits::with_state(&data, self.gb);
        let more_frames = self.decode_frame_inner(&mut gb, frame);
        self.gb = gb.state();
        self.frame_data = data;
        more_frames
    }

    fn decode_frame_inner(&mut self, gb: &mut GetBits<'_>, frame: &mut Option<AudioFrame>) -> bool {
        let mut len = 0usize;

        // get frame length
        if self.len_prefix {
            len = gb.get_bits(self.log2_frame_size) as usize;
        }

        // decode tile information
        if self.decode_tilehdr(gb).is_err() {
            self.packet_loss = true;
            return false;
        }

        // read postproc transform
        if self.nb_channels > 1 && gb.get_bits1() != 0 && gb.get_bits1() != 0 {
            for _ in 0..self.nb_channels * self.nb_channels {
                gb.skip_bits(4);
            }
        }

        // read drc info
        if self.dynamic_range_compression {
            // drc_gain: read but not applied, as in FFmpeg
            gb.skip_bits(8);
        }

        if gb.get_bits1() != 0 {
            let bits = av_log2((self.samples_per_frame * 2) as u32);
            if gb.get_bits1() != 0 {
                self.trim_start = gb.get_bits(bits) as usize;
            }
            if gb.get_bits1() != 0 {
                self.trim_end = gb.get_bits(bits) as usize;
            }
        } else {
            self.trim_start = 0;
            self.trim_end = 0;
        }

        // reset subframe states
        self.parsed_all_subframes = false;
        for chan in self.channel.iter_mut() {
            chan.decoded_samples = 0;
            chan.cur_subframe = 0;
            chan.reuse_sf = false;
        }

        // decode all subframes
        while !self.parsed_all_subframes {
            if self.decode_subframe(gb).is_err() {
                self.packet_loss = true;
                return false;
            }
        }

        // copy samples to the output buffer
        let spf = self.samples_per_frame;
        let out = AudioFrame {
            samples: spf as u32,
            pts: None,
            data: self.channel.iter().map(|ch| ch.out[..spf].iter().flat_map(|v| v.to_le_bytes()).collect()).collect(),
        };

        for chan in self.channel.iter_mut() {
            // reuse second half of the IMDCT output for the next frame
            chan.out.copy_within(spf..spf + spf / 2, 0);
        }

        if self.skip_frame {
            self.skip_frame = false;
            *frame = None;
        } else {
            *frame = Some(out);
        }

        if self.len_prefix {
            let consumed = gb.bits_count() - self.frame_offset;
            if len != consumed + 2 {
                // FIXME: not sure if this is always an error
                self.packet_loss = true;
                return false;
            }
            // skip the rest of the frame data
            gb.skip_bits_long(len as i64 - consumed as i64 - 1);
        } else {
            while gb.bits_count() < self.num_saved_bits && gb.get_bits1() == 0 {}
        }

        // decode trailer bit
        gb.get_bits1() != 0
    }

    /// `remaining_bits`: remaining packet input in bits.
    fn remaining_bits(&self, gb: &GetBits<'_>) -> i64 {
        self.buf_bit_size as i64 - gb.bits_count() as i64
    }

    /// `save_bits`: fill the bit reservoir with a (partial) frame. `append`
    /// continues the reservoir; otherwise it restarts at the current byte so
    /// a byte copy is possible and the leading bits are skipped later.
    fn save_bits(&mut self, gb: &mut GetBits<'_>, len: i64, append: bool) {
        let buflen;
        if !append {
            self.frame_offset = gb.bits_count() & 7;
            self.num_saved_bits = self.frame_offset;
            self.pb.reset();
            buflen = (self.num_saved_bits as i64 + len + 7) >> 3;
        } else {
            buflen = (self.pb.count() as i64 + len + 7) >> 3;
        }

        if len <= 0 || buflen > MAX_FRAMESIZE as i64 {
            // "Too small input buffer"
            self.packet_loss = true;
            return;
        }
        let mut len = len as usize;

        self.num_saved_bits += len;
        if !append {
            let src = gb.buffer().get(gb.bits_count() >> 3..).unwrap_or(&[]);
            self.pb.copy_bits(&mut self.frame_data, src, self.num_saved_bits);
        } else {
            let align = (8 - (gb.bits_count() & 7)).min(len);
            let v = gb.get_bits(align as u32);
            self.pb.put_bits(&mut self.frame_data, align as u32, v);
            len -= align;
            let src = gb.buffer().get(gb.bits_count() >> 3..).unwrap_or(&[]);
            self.pb.copy_bits(&mut self.frame_data, src, len);
        }
        gb.skip_bits_long(len as i64);

        self.pb.flush(&mut self.frame_data);

        let mut sgb = GetBits::new(&self.frame_data, self.num_saved_bits);
        sgb.skip_bits(self.frame_offset as u32);
        self.gb = sgb.state();
    }

    /// `decode_packet` on the unread, non-empty part `data` of the current
    /// packet. Returns the bytes consumed, or `None` for FFmpeg's error
    /// return; `frame` is the output, which FFmpeg delivers even alongside
    /// an error. (FFmpeg's end-of-stream branch is unreachable for WMA Pro:
    /// the decoder lacks `AV_CODEC_CAP_DELAY`, so it is never drained.)
    fn decode_packet(&mut self, data: &[u8], frame: &mut Option<AudioFrame>) -> Option<usize> {
        *frame = None;

        let mut gb;
        if self.packet_done || self.packet_loss {
            self.packet_done = false;

            // sanity check for the buffer length
            if data.len() < self.block_align {
                // "Input packet too small"
                self.packet_loss = true;
                return None;
            }

            self.next_packet_start = data.len() - self.block_align;
            let buf_size = self.block_align;
            self.buf_bit_size = buf_size << 3;

            // parse packet header
            gb = GetBits::new(data, buf_size * 8);
            let packet_sequence_number = gb.get_bits(4) as u8;
            gb.skip_bits(2);

            // get number of bits that need to be added to the previous frame
            let mut num_bits_prev_frame = gb.get_bits(self.log2_frame_size) as i64;

            // check for packet loss
            if !self.packet_loss && (self.packet_sequence_number.wrapping_add(1) & 0xF) != packet_sequence_number {
                self.packet_loss = true;
            }
            self.packet_sequence_number = packet_sequence_number;

            if num_bits_prev_frame > 0 {
                let remaining_packet_bits = self.buf_bit_size as i64 - gb.bits_count() as i64;
                if num_bits_prev_frame >= remaining_packet_bits {
                    num_bits_prev_frame = remaining_packet_bits;
                    self.packet_done = true;
                }

                // append the previous frame data to the remaining data from
                // the previous packet to create a full frame
                self.save_bits(&mut gb, num_bits_prev_frame, true);

                // decode the cross packet frame if it is valid
                if !self.packet_loss {
                    self.decode_frame(frame);
                }
            }

            if self.packet_loss {
                // reset number of saved bits so that the decoder does not
                // start to decode incomplete frames in the len_prefix == 0
                // case
                self.num_saved_bits = 0;
                self.packet_loss = false;
            }
        } else {
            if data.len() < self.next_packet_start {
                self.packet_loss = true;
                return None;
            }

            let size = data.len() - self.next_packet_start;
            self.buf_bit_size = size << 3;
            gb = GetBits::new(data, size * 8);
            gb.skip_bits(self.packet_offset);
            let remaining = self.remaining_bits(&gb);
            let frame_size = if self.len_prefix && remaining > self.log2_frame_size as i64 {
                gb.show_bits(self.log2_frame_size) as i64
            } else {
                0
            };
            if self.len_prefix && frame_size != 0 && frame_size <= remaining {
                self.save_bits(&mut gb, frame_size, false);
                if !self.packet_loss {
                    self.packet_done = !self.decode_frame(frame);
                }
            } else if !self.len_prefix && self.num_saved_bits > self.gb.bits_count() {
                // when the frames do not have a length prefix, we don't
                // know the compressed length of the individual frames
                // however, we know what part of a new packet belongs to the
                // previous frame therefore we save the incoming packet
                // first, then we append the "previous frame" data from the
                // next packet so that we get a buffer that only contains
                // full frames
                self.packet_done = !self.decode_frame(frame);
            } else {
                self.packet_done = true;
            }
        }

        if self.remaining_bits(&gb) < 0 {
            // "Overread"
            self.packet_loss = true;
        }

        if self.packet_done && !self.packet_loss && self.remaining_bits(&gb) > 0 {
            // save the rest of the data so that it can be decoded with the
            // next packet
            let rest = self.remaining_bits(&gb);
            self.save_bits(&mut gb, rest, false);
        }

        self.packet_offset = (gb.bits_count() & 7) as u32;
        if self.packet_loss {
            return None;
        }

        let mut nb_samples = self.samples_per_frame;
        if self.trim_start != 0 {
            if self.trim_start < nb_samples {
                if let Some(f) = frame.as_mut() {
                    for plane in f.data.iter_mut() {
                        plane.drain(..self.trim_start * 4);
                    }
                    f.samples -= self.trim_start as u32;
                }
                nb_samples -= self.trim_start;
            } else {
                *frame = None;
            }
            self.trim_start = 0;
        }

        if self.trim_end != 0 {
            if self.trim_end < nb_samples {
                if let Some(f) = frame.as_mut() {
                    let keep = (nb_samples - self.trim_end) * 4;
                    for plane in f.data.iter_mut() {
                        plane.truncate(keep);
                    }
                    f.samples -= self.trim_end as u32;
                }
            } else {
                *frame = None;
            }
            self.trim_end = 0;
        }

        Some(gb.bits_count() >> 3)
    }

    /// FFmpeg's decode loop over one packet: `decode_packet` runs on the
    /// unread rest until it consumes everything or fails (the rest is then
    /// dropped). Frames come out even alongside an error, as with FFmpeg.
    fn decode_packet_loop(&mut self, data: &[u8]) {
        let mut data = data;
        let mut stalls = 0;
        while !data.is_empty() {
            let mut frame = None;
            let res = self.decode_packet(data, &mut frame);
            if let Some(f) = frame {
                self.pending.push_back(f);
            }
            let Some(consumed) = res else { break };
            if consumed >= data.len() {
                break;
            }
            // a decoder that keeps consuming nothing would loop forever
            stalls = if consumed == 0 { stalls + 1 } else { 0 };
            if stalls > 2 {
                break;
            }
            data = &data[consumed..];
        }
    }

    /// `flush`: clear decoder buffers (for seeking).
    fn flush_state(&mut self) {
        // reset output buffer as a part of it is used during the windowing
        // of a new frame
        for chan in self.channel.iter_mut() {
            chan.out[..self.samples_per_frame].fill(0.0);
        }
        self.packet_loss = true;
        self.skip_frame = true;
    }
}

/// `ff_wma_run_level_decode` (wma.c), version 1 (WMA Pro): `ptr` is the
/// subframe's coefficients, decoding starts at `offset`.
#[allow(clippy::too_many_arguments)]
fn run_level_decode(
    gb: &mut GetBits<'_>,
    vlc: &VlcTable,
    level_table: &[f32],
    run_table: &[u16],
    ptr: &mut [f32],
    mut offset: usize,
    num_coefs: usize,
    frame_len_bits: u32,
) -> Result<()> {
    let coef_mask = ptr.len() - 1;
    while offset < num_coefs {
        let code = gb.get_vlc(vlc);
        if code > 1 {
            // normal code
            offset += run_table[code as usize] as usize;
            let level = level_table[code as usize];
            ptr[offset & coef_mask] = if gb.get_bits1() == 0 { -level } else { level };
        } else if code == 1 {
            // EOB
            break;
        } else {
            // escape
            let level = get_large_val(gb) as i32;
            // escape decode
            if gb.get_bits1() != 0 {
                if gb.get_bits1() != 0 {
                    if gb.get_bits1() != 0 {
                        return Err(Error::invalid("wmapro: broken escape sequence"));
                    }
                    offset += gb.get_bits(frame_len_bits) as usize + 4;
                } else {
                    offset += gb.get_bits(2) as usize + 1;
                }
            }
            let sign = gb.get_bits1() as i32 - 1;
            ptr[offset & coef_mask] = ((level ^ sign).wrapping_sub(sign)) as f32;
        }
        offset += 1;
    }
    // NOTE: EOB can be omitted
    if offset > num_coefs {
        return Err(Error::invalid("wmapro: overflow in spectral RLE"));
    }
    Ok(())
}

impl Decoder for WmaProDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.decode_packet_loop(&packet.data);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.pending.pop_front() {
            Some(f) => Ok(Frame::Audio(f)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    /// End of stream. WMA Pro has no decoder delay in FFmpeg (no
    /// `AV_CODEC_CAP_DELAY`): the overlap tail is not output.
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    /// Seek: FFmpeg's `wmapro_flush`.
    fn reset(&mut self) -> Result<()> {
        self.flush_state();
        self.eof = false;
        self.pending.clear();
        Ok(())
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        Some(oxideav_core::AudioFormat {
            sample_format: SampleFormat::F32P,
            sample_rate: self.sample_rate,
            channels: self.nb_channels as u16,
        })
    }
}

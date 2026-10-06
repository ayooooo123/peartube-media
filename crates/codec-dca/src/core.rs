// Ported from FFmpeg libavcodec/dca_core.c and dca_core.h (commit
// 2da55bf), LGPL-2.1-or-later.

//! DTS Coherent Acoustics core decoder: frame header, coding header,
//! subframe parsing (audio data, VQ, ADPCM, joint intensity), the XCH,
//! XXCH, X96 and XBR extensions, and the float / fixed-point synthesis
//! filter paths.

use crate::bitreader::BitReader;
use crate::dca::{self, amode, ext_audio_type, lfe_flag, mask as spk, speaker, CoreFrameHeader, ParseError};
use crate::data::*;
use crate::dsp;
use crate::exss::ExssAsset;
use crate::huffman::{BITALLOC_MAXBITS, DCA_CODE_BOOKS, FF_DCA_BITALLOC_OFFSETS, FF_DCA_BITALLOC_SIZES, FF_DCA_VLC_SRC_TABLES};
use crate::math::{clip23, core_dequantize, dcaadpcm_predict, mul16, mul17, mul23, mul31};
use crate::vlc::{build_src, Vlc};

/// Whether the current packet carries XLL (set by the top-level decoder).
pub fn decoder_packet_xll(s: &CoreDecoder) -> bool {
    s.packet & crate::dca::decoder_packets::DCA_PACKET_XLL != 0
}

/// `DCA_ABITS_MAX` (dca_core.h).
const DCA_ABITS_MAX: i32 = crate::dca::DCA_ABITS_MAX;

/// `dca->packet & DCA_PACKET_XLL` (dcadec.h flags live in `dca::decoder_packets`).
fn packet_xll(s: &CoreDecoder) -> i32 {
    s.packet & crate::dca::decoder_packets::DCA_PACKET_XLL
}

/// Header types (dca_core.c `enum HeaderType`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HeaderType {
    Core,
    Xch,
    Xxch,
}

/// Huffman codebooks, built once per decoder (FFmpeg's `ff_dca_init_vlcs`).
pub struct VlcSet {
    /// `ff_dca_vlc_quant_index[DCA_CODE_BOOKS][7]` — books beyond a code
    /// book's group size alias the last valid one (FFmpeg leaves those
    /// slots uninitialized but guarded by `quant_index_sel < group_size`;
    /// aliasing keeps indexing panic-free on malformed data).
    pub quant_index: Vec<Vec<Vlc>>,
    pub bit_allocation: [Vlc; 5],
    pub scale_factor: [Vlc; 5],
    pub transition_mode: [Vlc; 4],
}

/// The core 12-entry bit allocation / scale factor / transition mode VLCs
/// share one source table region after the quant_index books.
fn core_vlc_tables() -> VlcSet {
    let mut pos = 0usize;
    let mut quant_index: Vec<Vec<Vlc>> = Vec::with_capacity(DCA_CODE_BOOKS);
    for i in 0..DCA_CODE_BOOKS {
        let mut row: Vec<Vlc> = Vec::with_capacity(7);
        for j in 0..FF_DCA_QUANT_INDEX_GROUP_SIZE[i] as usize {
            let n = FF_DCA_BITALLOC_SIZES[i] as usize;
            row.push(build_src(
                &FF_DCA_VLC_SRC_TABLES[pos..pos + n],
                BITALLOC_MAXBITS[i][j] as u32,
                i32::from(FF_DCA_BITALLOC_OFFSETS[i]),
                false,
            ));
            pos += n;
        }
        quant_index.push(row);
    }
    // bit_allocation[5]: 12 entries each, offset 1
    let mut bit_allocation = Vec::with_capacity(5);
    for i in 0..5 {
        bit_allocation.push(build_src(
            &FF_DCA_VLC_SRC_TABLES[pos..pos + 12],
            u32::from(crate::huffman::BITALLOC_12_VLC_BITS[i]),
            1,
            false,
        ));
        pos += 12;
    }
    // scale_factor[5]: 129 entries each, offset -64
    let mut scale_factor = Vec::with_capacity(5);
    for _ in 0..5 {
        scale_factor.push(build_src(&FF_DCA_VLC_SRC_TABLES[pos..pos + 129], 9, -64, false));
        pos += 129;
    }
    // transition_mode[4]: 4 entries each, offset 0
    let mut transition_mode = Vec::with_capacity(4);
    for _ in 0..4 {
        transition_mode.push(build_src(&FF_DCA_VLC_SRC_TABLES[pos..pos + 4], 3, 0, false));
        pos += 4;
    }
    VlcSet {
        quant_index,
        bit_allocation: bit_allocation.try_into().ok().unwrap(),
        scale_factor: scale_factor.try_into().ok().unwrap(),
        transition_mode: transition_mode.try_into().ok().unwrap(),
    }
}

/// `DCADSPData`: FIR history for one channel.
#[derive(Clone)]
pub struct DspData {
    pub hist1: [f32; 1024],
    pub hist2: [f32; 64],
    pub hist1_fix: [i32; 1024],
    pub hist2_fix: [i32; 64],
    pub offset: i32,
}

impl Default for DspData {
    fn default() -> Self {
        Self {
            hist1: [0.0; 1024],
            hist2: [0.0; 64],
            hist1_fix: [0; 1024],
            hist2_fix: [0; 64],
            offset: 0,
        }
    }
}

/// Subband sample buffers: `[channel][band]` rows of
/// `DCA_ADPCM_COEFFS + npcmblocks` samples (the leading 4 samples are the
/// ADPCM history FFmpeg's pointer arithmetic reaches).
struct SubbandBuffer {
    nchsamples: usize,
    nbands: usize,
    data: Vec<i32>,
}

impl SubbandBuffer {
    fn new(nbands: usize) -> Self {
        Self {
            nchsamples: 0,
            nbands,
            data: Vec::new(),
        }
    }

    /// `alloc_sample_buffer` / `alloc_x96_sample_buffer`. Like FFmpeg's
    /// `av_fast_mallocz`, the buffer is zeroed only when (re)allocated;
    /// its contents persist across frames so the ADPCM predictor sees the
    /// previous frame's tail samples (the 4-sample history region is
    /// refreshed by `parse_frame_data`, `erase_adpcm_history` clears it
    /// when `predictor_history` is off).
    fn alloc(&mut self, nchsamples: usize, nchannels: usize, _nbands: usize) {
        let nframesamples = nchsamples * nchannels * self.nbands;
        if self.data.len() != nframesamples {
            self.data = vec![0i32; nframesamples];
            self.nchsamples = nchsamples;
        }
    }

    /// Flat row index for channel/band.
    fn row_base(&self, ch: usize, band: usize) -> usize {
        (ch * self.nbands + band) * self.nchsamples
    }
}


// ───────────────────────── decoder struct ─────────────────────────

const BLOCK_CODE_NBITS: [usize; 7] = [7, 10, 12, 13, 15, 17, 19];

/// Speaker→speaker mapping tables (dca_core.c statics).
const PRM_CH_TO_SPKR_MAP: [[i32; 5]; amode::COUNT] = [
    [speaker::C as i32, -1, -1, -1, -1],
    [speaker::L as i32, speaker::R as i32, -1, -1, -1],
    [speaker::L as i32, speaker::R as i32, -1, -1, -1],
    [speaker::L as i32, speaker::R as i32, -1, -1, -1],
    [speaker::L as i32, speaker::R as i32, -1, -1, -1],
    [speaker::C as i32, speaker::L as i32, speaker::R as i32, -1, -1],
    [speaker::L as i32, speaker::R as i32, speaker::CS as i32, -1, -1],
    [speaker::C as i32, speaker::L as i32, speaker::R as i32, speaker::CS as i32, -1],
    [speaker::L as i32, speaker::R as i32, speaker::LS as i32, speaker::RS as i32, -1],
    [speaker::C as i32, speaker::L as i32, speaker::R as i32, speaker::LS as i32, speaker::RS as i32],
];

const AUDIO_MODE_CH_MASK: [u32; amode::COUNT] = [
    spk::MONO,
    spk::STEREO,
    spk::STEREO,
    spk::STEREO,
    spk::STEREO,
    spk::THREE_0,
    spk::TWO_1,
    spk::THREE_1,
    spk::TWO_2,
    spk::FIVE_POINT0,
];

/// `DCACoreDecoder`. The sample buffers use FFmpeg's flat layout with
/// `DCA_ADPCM_COEFFS` leading history samples per row.
pub struct CoreDecoder {
    pub vlcs: VlcSet,

    // Bit stream header
    pub crc_present: bool,
    pub npcmblocks: usize,
    pub frame_size: usize,
    pub audio_mode: usize,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub drc_present: bool,
    pub ts_present: bool,
    pub aux_present: bool,
    pub ext_audio_type: usize,
    pub ext_audio_present: bool,
    pub sync_ssf: bool,
    pub lfe_present: i32,
    pub predictor_history: bool,
    pub filter_perfect: bool,
    pub source_pcm_res: u32,
    pub es_format: i32,
    pub sumdiff_front: bool,
    pub sumdiff_surround: bool,

    // Primary audio coding header
    pub nsubframes: usize,
    pub nchannels: usize,
    pub ch_mask: u32,
    pub nsubbands: [usize; dca::DCA_CHANNELS],
    pub subband_vq_start: [usize; dca::DCA_CHANNELS],
    pub joint_intensity_index: [usize; dca::DCA_CHANNELS],
    pub transition_mode_sel: [usize; dca::DCA_CHANNELS],
    pub scale_factor_sel: [usize; dca::DCA_CHANNELS],
    pub bit_allocation_sel: [usize; dca::DCA_CHANNELS],
    pub quant_index_sel: [[usize; DCA_CODE_BOOKS]; dca::DCA_CHANNELS],
    pub scale_factor_adj: [[i32; DCA_CODE_BOOKS]; dca::DCA_CHANNELS],

    // Primary audio coding side information
    pub nsubsubframes: [usize; dca::DCA_SUBFRAMES],
    pub prediction_mode: [[i32; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
    pub prediction_vq_index: [[usize; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
    pub bit_allocation: [[i32; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
    pub transition_mode: [[[usize; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS]; dca::DCA_SUBFRAMES],
    pub scale_factors: [[[i32; 2]; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS],
    pub joint_scale_sel: [usize; dca::DCA_CHANNELS],
    pub joint_scale_factors: [[i32; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],

    // Auxiliary data
    pub prim_dmix_embedded: bool,
    pub prim_dmix_type: usize,
    pub prim_dmix_coeff: [i32; dca::DCA_DMIX_CHANNELS_MAX * dca::DCA_CORE_CHANNELS_MAX],

    // Core extensions
    pub ext_audio_mask: i32,

    // XCH extension data
    pub xch_pos: usize,

    // XXCH extension data
    pub xxch_crc_present: bool,
    pub xxch_mask_nbits: usize,
    pub xxch_core_mask: u32,
    pub xxch_spkr_mask: u32,
    pub xxch_dmix_embedded: bool,
    pub xxch_dmix_scale_inv: i32,
    pub xxch_dmix_mask: [u32; dca::DCA_XXCH_CHANNELS_MAX],
    pub xxch_dmix_coeff: [i32; dca::DCA_XXCH_CHANNELS_MAX * dca::DCA_CORE_CHANNELS_MAX],
    pub xxch_pos: usize,

    // X96 extension data
    pub x96_rev_no: usize,
    pub x96_crc_present: bool,
    pub x96_nchannels: usize,
    pub x96_high_res: bool,
    pub x96_subband_start: usize,
    pub x96_rand: u32,
    pub x96_pos: usize,

    // Sample buffers
    subband: SubbandBuffer,
    x96_subband: SubbandBuffer,
    pub lfe_samples: [i32; dca::DCA_LFE_HISTORY + dca::DCA_PCMBLOCK_SAMPLES * dca::DCA_SUBBANDS / 2 + 16],

    // DSP contexts
    pub dcadsp_data: [DspData; dca::DCA_CHANNELS],

    // PCM output data (fixed mode: per-speaker planes)
    pub output: Vec<i32>,
    pub output_plane_len: usize,
    pub output_history_lfe_fixed: i32,
    pub output_history_lfe_float: f32,

    /// FFmpeg's `s->imdct[0]` / `s->imdct[1]`: AV_TX_FLOAT_MDCT inverse,
    /// len 32 and 64, scale 1.0 (dca_core.c ff_dca_core_init).
    pub imdct32: crate::avtx::MdctInv,
    pub imdct64: crate::avtx::MdctInv,

    pub ch_remap: [usize; dca::DCA_SPEAKER_COUNT],
    pub request_mask: u32,
    pub request_channel_layout: u32,

    pub npcmsamples: usize,
    pub output_rate: u32,
    pub filter_mode: i32,

    /// Frame buffer with padding for the bitstream conversion path.
    pub buffer: Vec<u8>,
    pub packet: i32,
    pub core_only: bool,
}

impl CoreDecoder {
    pub fn new() -> Self {
        Self {
            vlcs: core_vlc_tables(),
            crc_present: false,
            npcmblocks: 0,
            frame_size: 0,
            audio_mode: 0,
            sample_rate: 0,
            bit_rate: 0,
            drc_present: false,
            ts_present: false,
            aux_present: false,
            ext_audio_type: 0,
            ext_audio_present: false,
            sync_ssf: false,
            lfe_present: 0,
            predictor_history: false,
            filter_perfect: false,
            source_pcm_res: 0,
            es_format: 0,
            sumdiff_front: false,
            sumdiff_surround: false,
            nsubframes: 0,
            nchannels: 0,
            ch_mask: 0,
            nsubbands: [0; dca::DCA_CHANNELS],
            subband_vq_start: [0; dca::DCA_CHANNELS],
            joint_intensity_index: [0; dca::DCA_CHANNELS],
            transition_mode_sel: [0; dca::DCA_CHANNELS],
            scale_factor_sel: [0; dca::DCA_CHANNELS],
            bit_allocation_sel: [0; dca::DCA_CHANNELS],
            quant_index_sel: [[0; DCA_CODE_BOOKS]; dca::DCA_CHANNELS],
            scale_factor_adj: [[0; DCA_CODE_BOOKS]; dca::DCA_CHANNELS],
            nsubsubframes: [0; dca::DCA_SUBFRAMES],
            prediction_mode: [[0; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
            prediction_vq_index: [[0; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
            bit_allocation: [[0; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
            transition_mode: [[[0; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS]; dca::DCA_SUBFRAMES],
            scale_factors: [[[0; 2]; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS],
            joint_scale_sel: [0; dca::DCA_CHANNELS],
            joint_scale_factors: [[0; dca::DCA_SUBBANDS_X96]; dca::DCA_CHANNELS],
            prim_dmix_embedded: false,
            prim_dmix_type: 0,
            prim_dmix_coeff: [0; dca::DCA_DMIX_CHANNELS_MAX * dca::DCA_CORE_CHANNELS_MAX],
            ext_audio_mask: 0,
            xch_pos: 0,
            xxch_crc_present: false,
            xxch_mask_nbits: 0,
            xxch_core_mask: 0,
            xxch_spkr_mask: 0,
            xxch_dmix_embedded: false,
            xxch_dmix_scale_inv: 0,
            xxch_dmix_mask: [0; dca::DCA_XXCH_CHANNELS_MAX],
            xxch_dmix_coeff: [0; dca::DCA_XXCH_CHANNELS_MAX * dca::DCA_CORE_CHANNELS_MAX],
            xxch_pos: 0,
            x96_rev_no: 0,
            x96_crc_present: false,
            x96_nchannels: 0,
            x96_high_res: false,
            x96_subband_start: 0,
            x96_rand: 1,
            x96_pos: 0,
            subband: SubbandBuffer::new(dca::DCA_SUBBANDS),
            x96_subband: SubbandBuffer::new(dca::DCA_SUBBANDS_X96),
            lfe_samples: [0; dca::DCA_LFE_HISTORY + dca::DCA_PCMBLOCK_SAMPLES * dca::DCA_SUBBANDS / 2 + 16],
            dcadsp_data: Default::default(),
            output: Vec::new(),
            output_plane_len: 0,
            output_history_lfe_fixed: 0,
            output_history_lfe_float: 0.0,
            imdct32: crate::avtx::MdctInv::new(32, 1.0),
            imdct64: crate::avtx::MdctInv::new(64, 1.0),
            ch_remap: [0; dca::DCA_SPEAKER_COUNT],
            request_mask: 0,
            request_channel_layout: 0,
            npcmsamples: 0,
            output_rate: 0,
            filter_mode: 0,
            buffer: Vec::new(),
            packet: 0,
            core_only: false,
        }
    }

    /// `ff_dca_core_map_spkr`.
    pub fn map_spkr(&self, spkr: usize) -> i32 {
        if self.ch_mask & (1u32 << spkr) != 0 {
            return spkr as i32;
        }
        if spkr == speaker::LSS && (self.ch_mask & spk::LS) != 0 {
            return speaker::LS as i32;
        }
        if spkr == speaker::RSS && (self.ch_mask & spk::RS) != 0 {
            return speaker::RS as i32;
        }
        -1
    }

    /// ADPCM history window before row start (4 samples + `j`).
    /// The 4 subband samples preceding absolute data position
    /// `ADPCM_COEFFS + ofs + j` — the ADPCM prediction window.
    fn adpcm_hist(&self, ch: usize, band: usize, x96: bool, ofs: usize, j: usize) -> [i32; 4] {
        let buf = if x96 { &self.x96_subband } else { &self.subband };
        let base = buf.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + ofs + j - 4;
        [
            buf.data[base],
            buf.data[base + 1],
            buf.data[base + 2],
            buf.data[base + 3],
        ]
    }
}

impl CoreDecoder {
    // ───────────── 5.3.1 bit stream header ─────────────

    fn parse_frame_header(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        let mut h = CoreFrameHeader::default();
        let err = dca::parse_core_frame_header(&mut h, gb);

        if let Err(e) = err {
            return match e {
                ParseError::DeficitSamples => Err("deficit samples are not supported"),
                ParseError::PcmBlocks => Err("unsupported number of PCM sample blocks"),
                ParseError::FrameSize => Err("invalid core frame size"),
                ParseError::Amode => Err("unsupported audio channel arrangement"),
                ParseError::SampleRate => Err("invalid core audio sampling frequency"),
                ParseError::ReservedBit => Err("reserved bit set"),
                ParseError::LfeFlag => Err("invalid low frequency effects flag"),
                ParseError::PcmRes => Err("invalid source PCM resolution"),
                ParseError::SyncWord => Err("invalid core frame header sync word"),
            };
        }

        self.crc_present = h.crc_present != 0;
        self.npcmblocks = h.npcmblocks as usize;
        self.frame_size = h.frame_size as usize;
        self.audio_mode = h.audio_mode as usize;
        self.sample_rate = FF_DCA_SAMPLE_RATES[h.sr_code as usize];
        self.bit_rate = FF_DCA_BIT_RATES[h.br_code as usize];
        self.drc_present = h.drc_present != 0;
        self.ts_present = h.ts_present != 0;
        self.aux_present = h.aux_present != 0;
        self.ext_audio_type = h.ext_audio_type as usize;
        self.ext_audio_present = h.ext_audio_present != 0;
        self.sync_ssf = h.sync_ssf != 0;
        self.lfe_present = h.lfe_present as i32;
        self.predictor_history = h.predictor_history != 0;
        self.filter_perfect = h.filter_perfect != 0;
        self.source_pcm_res = u32::from(FF_DCA_BITS_PER_SAMPLE[h.pcmr_code as usize]);
        self.es_format = i32::from(h.pcmr_code & 1);
        self.sumdiff_front = h.sumdiff_front != 0;
        self.sumdiff_surround = h.sumdiff_surround != 0;

        Ok(())
    }

    // ───────────── 5.3.2 primary audio coding header ─────────────

    fn parse_coding_header(&mut self, gb: &mut BitReader, header: HeaderType, xch_base: usize) -> Result<(), &'static str> {
        let header_pos = gb.bits_read();
        let mut xxch_header_end = 0usize;

        match header {
            HeaderType::Core => {
                // Number of subframes
                self.nsubframes = gb.get_bits(4) as usize + 1;

                // Number of primary audio channels
                self.nchannels = gb.get_bits(3) as usize + 1;
                if self.nchannels != FF_DCA_CHANNELS[self.audio_mode] as usize {
                    return Err("invalid number of primary audio channels for arrangement");
                }

                self.ch_mask = u32::from(AUDIO_MODE_CH_MASK[self.audio_mode]);

                // Add LFE channel if present
                if self.lfe_present != 0 {
                    self.ch_mask |= spk::LFE1;
                }
            }
            HeaderType::Xch => {
                self.nchannels = FF_DCA_CHANNELS[self.audio_mode] as usize + 1;
                self.ch_mask |= spk::CS;
            }
            HeaderType::Xxch => {
                // Channel set header length
                let header_size = gb.get_bits(7) as usize + 1;

                // Number of channels in a channel set
                let nchannels = gb.get_bits(3) as usize + 1;
                if nchannels > dca::DCA_XXCH_CHANNELS_MAX {
                    return Err("too many XXCH channels");
                }
                self.nchannels = FF_DCA_CHANNELS[self.audio_mode] as usize + nchannels;
                if self.nchannels > dca::DCA_CHANNELS {
                    return Err("too many XXCH channels for the channel set");
                }

                // Loudspeaker layout mask
                let m = self.xxch_mask_nbits - speaker::CS;
                let mword = gb.get_bits(m as u32);
                self.xxch_spkr_mask = mword << speaker::CS;

                if self.xxch_spkr_mask.count_ones() as usize != nchannels {
                    return Err("invalid XXCH speaker layout mask");
                }

                if self.xxch_core_mask & self.xxch_spkr_mask != 0 {
                    return Err("XXCH speaker layout mask overlaps with core");
                }

                // Combine core and XXCH masks together
                self.ch_mask = self.xxch_core_mask | self.xxch_spkr_mask;

                // Downmix coefficients present in stream
                if gb.get_bits(1) != 0 {
                    let mut coeff_idx = 0usize;

                    // Downmix already performed by encoder
                    self.xxch_dmix_embedded = gb.get_bits(1) != 0;

                    // Downmix scale factor
                    let index = gb.get_bits(6) as i32 * 4 - FF_DCA_DMIXTABLE_OFFSET as i32 - 3;
                    if index < 0 || index as usize >= FF_DCA_INV_DMIXTABLE_SIZE {
                        return Err("invalid XXCH downmix scale index");
                    }
                    self.xxch_dmix_scale_inv = FF_DCA_INV_DMIXTABLE[index as usize] as i32;

                    // Downmix channel mapping mask
                    for ch in 0..nchannels {
                        let mword = gb.get_bits_long(self.xxch_mask_nbits as u32);
                        if mword & self.xxch_core_mask != mword {
                            return Err("invalid XXCH downmix channel mapping mask");
                        }
                        self.xxch_dmix_mask[ch] = mword;
                    }

                    // Downmix coefficients
                    for ch in 0..nchannels {
                        for n in 0..self.xxch_mask_nbits {
                            if self.xxch_dmix_mask[ch] & (1u32 << n) != 0 {
                                let mut code = gb.get_bits(7);
                                let sign = (code >> 6) as i32 - 1;
                                code &= 63;
                                if code != 0 {
                                    let index = code as usize * 4 - 3;
                                    if index >= FF_DCA_DMIXTABLE_SIZE {
                                        return Err("invalid XXCH downmix coefficient index");
                                    }
                                    let c = i32::from(FF_DCA_DMIXTABLE[index]);
                                    self.xxch_dmix_coeff[coeff_idx] = ((c as u32 ^ sign as u32).wrapping_sub(sign as u32)) as i32;
                                    coeff_idx += 1;
                                } else {
                                    self.xxch_dmix_coeff[coeff_idx] = 0;
                                    coeff_idx += 1;
                                }
                            }
                        }
                    }
                } else {
                    self.xxch_dmix_embedded = false;
                }

                // CRC unchecked (FFmpeg default: no AV_EF_CRCCHECK)
                xxch_header_end = header_pos + header_size * 8;
            }
        }

        // Subband activity count
        for ch in xch_base..self.nchannels {
            self.nsubbands[ch] = gb.get_bits(5) as usize + 2;
            if self.nsubbands[ch] > dca::DCA_SUBBANDS {
                return Err("invalid subband activity count");
            }
        }

        // High frequency VQ start subband
        for ch in xch_base..self.nchannels {
            self.subband_vq_start[ch] = gb.get_bits(5) as usize + 1;
        }

        // Joint intensity coding index
        for ch in xch_base..self.nchannels {
            let mut n = gb.get_bits(3) as usize;
            if n != 0 && header == HeaderType::Xxch {
                n += xch_base.wrapping_sub(1);
            }
            if n > self.nchannels {
                return Err("invalid joint intensity coding index");
            }
            self.joint_intensity_index[ch] = n;
        }

        // Transient mode code book
        for ch in xch_base..self.nchannels {
            self.transition_mode_sel[ch] = gb.get_bits(2) as usize;
        }

        // Scale factor code book
        for ch in xch_base..self.nchannels {
            self.scale_factor_sel[ch] = gb.get_bits(3) as usize;
            if self.scale_factor_sel[ch] == 7 {
                return Err("invalid scale factor code book");
            }
        }

        // Bit allocation quantizer select
        for ch in xch_base..self.nchannels {
            self.bit_allocation_sel[ch] = gb.get_bits(3) as usize;
            if self.bit_allocation_sel[ch] == 7 {
                return Err("invalid bit allocation quantizer select");
            }
        }

        // Quantization index codebook select
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                self.quant_index_sel[ch][n] = gb.get_bits(u32::from(FF_DCA_QUANT_INDEX_SEL_NBITS[n])) as usize;
            }
        }

        // Scale factor adjustment index
        for n in 0..DCA_CODE_BOOKS {
            for ch in xch_base..self.nchannels {
                if self.quant_index_sel[ch][n] < FF_DCA_QUANT_INDEX_GROUP_SIZE[n] as usize {
                    self.scale_factor_adj[ch][n] = FF_DCA_SCALE_FACTOR_ADJ[gb.get_bits(2) as usize] as i32;
                }
            }
        }

        if header == HeaderType::Xxch {
            // Reserved, byte align, CRC16 of channel set header
            if !gb.seek_bits(xxch_header_end) {
                return Err("read past end of XXCH channel set header");
            }
        } else {
            // Audio header CRC check word
            if self.crc_present {
                gb.skip(16);
            }
        }

        Ok(())
    }
}


impl CoreDecoder {
    // ───────────── scale factors ─────────────

    fn parse_scale(&self, gb: &mut BitReader, scale_index: &mut usize, sel: usize) -> Result<i32, &'static str> {
        // Select the root square table
        let (table, size): (&[u32], usize) = if sel > 5 {
            (&FF_DCA_SCALE_FACTOR_QUANT7, FF_DCA_SCALE_FACTOR_QUANT7.len())
        } else {
            (&FF_DCA_SCALE_FACTOR_QUANT6, FF_DCA_SCALE_FACTOR_QUANT6.len())
        };

        // If Huffman code was used, the difference of scales was encoded
        if sel < 5 {
            let v = self.vlcs.scale_factor[sel].get(gb, 2);
            *scale_index = (*scale_index as i32 + v) as usize;
        } else {
            *scale_index = gb.get_bits(sel as u32 + 1) as usize;
        }

        // Look up scale factor from the root square table
        if *scale_index >= size {
            return Err("invalid scale factor index");
        }
        Ok(table[*scale_index] as i32)
    }

    fn parse_joint_scale(&self, gb: &mut BitReader, sel: usize) -> Result<i32, &'static str> {
        // Absolute value was encoded even when Huffman code was used
        let mut scale_index = if sel < 5 {
            self.vlcs.scale_factor[sel].get(gb, 2)
        } else {
            gb.get_bits(sel as u32 + 1) as i32
        };

        // Bias by 64
        scale_index += 64;

        // Look up joint scale factor
        if scale_index < 0 || scale_index as usize >= FF_DCA_JOINT_SCALE_FACTORS.len() {
            return Err("invalid joint scale factor index");
        }
        Ok(FF_DCA_JOINT_SCALE_FACTORS[scale_index as usize] as i32)
    }

    // ───────────── block codes / huffman / audio extraction ─────────────

    fn decode_blockcodes(code1: i32, code2: i32, levels: usize, audio: &mut [i32]) -> i32 {
        let offset = (levels as i32 - 1) / 2;
        let levels = levels as i32;
        let mut code1 = code1;
        let mut code2 = code2;

        for n in 0..dca::DCA_SUBBAND_SAMPLES / 2 {
            let div = code1 / levels; // FASTDIV
            audio[n] = code1 - div * levels - offset;
            code1 = div;
        }
        for n in dca::DCA_SUBBAND_SAMPLES / 2..dca::DCA_SUBBAND_SAMPLES {
            let div = code2 / levels;
            audio[n] = code2 - div * levels - offset;
            code2 = div;
        }

        code1 | code2
    }

    fn parse_block_codes(&self, gb: &mut BitReader, audio: &mut [i32], abits: usize) -> Result<(), &'static str> {
        // Extract block code indices from the bit stream
        let code1 = gb.get_bits(BLOCK_CODE_NBITS[abits - 1] as u32) as i32;
        let code2 = gb.get_bits(BLOCK_CODE_NBITS[abits - 1] as u32) as i32;
        let levels = FF_DCA_QUANT_LEVELS[abits] as usize;

        // Look up samples from the block code book
        if Self::decode_blockcodes(code1, code2, levels, audio) != 0 {
            return Err("failed to decode block code(s)");
        }
        Ok(())
    }

    fn parse_huffman_codes(&self, gb: &mut BitReader, audio: &mut [i32], abits: usize, sel: usize) -> Result<usize, &'static str> {
        // Extract Huffman codes from the bit stream
        let book = &self.vlcs.quant_index[abits - 1][sel];
        for slot in audio.iter_mut().take(dca::DCA_SUBBAND_SAMPLES) {
            *slot = book.get(gb, 2);
        }
        Ok(1)
    }

    /// `extract_audio`. Returns whether Huffman codes were used (>0).
    fn extract_audio(&self, gb: &mut BitReader, audio: &mut [i32], abits: usize, ch: usize) -> Result<usize, &'static str> {
        if abits == 0 {
            // No bits allocated
            audio.iter_mut().take(dca::DCA_SUBBAND_SAMPLES).for_each(|v| *v = 0);
            return Ok(0);
        }

        if abits <= DCA_CODE_BOOKS {
            let sel = self.quant_index_sel[ch][abits - 1];
            if sel < FF_DCA_QUANT_INDEX_GROUP_SIZE[abits - 1] as usize {
                // Huffman codes
                return self.parse_huffman_codes(gb, audio, abits, sel);
            }
            if abits <= 7 {
                // Block codes
                return self.parse_block_codes(gb, audio, abits).map(|()| 0);
            }
        }

        // No further encoding
        for slot in audio.iter_mut().take(dca::DCA_SUBBAND_SAMPLES) {
            *slot = gb.get_sbits(abits as u32 - 3);
        }
        Ok(0)
    }

    // ───────────── 5.4.1 primary audio coding side information ─────────────

    fn parse_subframe_header(&mut self, gb: &mut BitReader, sf: usize, header: HeaderType, xch_base: usize) -> Result<(), &'static str> {
        if header == HeaderType::Core {
            // Subsubframe count
            self.nsubsubframes[sf] = gb.get_bits(2) as usize + 1;

            // Partial subsubframe sample count
            gb.skip(3);
        }

        // Prediction mode
        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = gb.get_bits(1) as i32;
            }
        }

        // Prediction coefficients VQ address
        for ch in xch_base..self.nchannels {
            for band in 0..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] != 0 {
                    self.prediction_vq_index[ch][band] = gb.get_bits(12) as usize;
                }
            }
        }

        // Bit allocation index
        for ch in xch_base..self.nchannels {
            let sel = self.bit_allocation_sel[ch];

            for band in 0..self.subband_vq_start[ch] {
                let abits = if sel < 5 {
                    self.vlcs.bit_allocation[sel].get(gb, 2)
                } else {
                    gb.get_bits(sel as u32 - 1) as i32
                };

                if abits > DCA_ABITS_MAX {
                    return Err("invalid bit allocation index");
                }
                self.bit_allocation[ch][band] = abits;
            }
        }

        // Transition mode
        for ch in xch_base..self.nchannels {
            // Clear transition mode for all subbands
            self.transition_mode[sf][ch] = [0; dca::DCA_SUBBANDS];

            // Transient possible only if more than one subsubframe
            if self.nsubsubframes[sf] > 1 {
                let sel = self.transition_mode_sel[ch];
                for band in 0..self.subband_vq_start[ch] {
                    if self.bit_allocation[ch][band] != 0 {
                        self.transition_mode[sf][ch][band] =
                            self.vlcs.transition_mode[sel].get(gb, 1).max(0) as usize;
                    }
                }
            }
        }

        // Scale factors
        for ch in xch_base..self.nchannels {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index: usize = 0;

            // Extract scales for subbands up to VQ
            for band in 0..self.subband_vq_start[ch] {
                if self.bit_allocation[ch][band] != 0 {
                    let s = self.parse_scale(gb, &mut scale_index, sel)?;
                    self.scale_factors[ch][band][0] = s;
                    if self.transition_mode[sf][ch][band] != 0 {
                        let s = self.parse_scale(gb, &mut scale_index, sel)?;
                        self.scale_factors[ch][band][1] = s;
                    }
                } else {
                    self.scale_factors[ch][band][0] = 0;
                }
            }

            // High frequency VQ subbands
            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                let s = self.parse_scale(gb, &mut scale_index, sel)?;
                self.scale_factors[ch][band][0] = s;
            }
        }

        // Joint subband codebook select
        for ch in xch_base..self.nchannels {
            if self.joint_intensity_index[ch] != 0 {
                self.joint_scale_sel[ch] = gb.get_bits(3) as usize;
                if self.joint_scale_sel[ch] == 7 {
                    return Err("invalid joint scale factor code book");
                }
            }
        }

        // Scale factors for joint subband coding
        for ch in xch_base..self.nchannels {
            let src_ch = self.joint_intensity_index[ch].wrapping_sub(1);
            if self.joint_intensity_index[ch] != 0 && src_ch < dca::DCA_CHANNELS {
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    let s = self.parse_joint_scale(gb, sel)?;
                    self.joint_scale_factors[ch][band] = s;
                }
            }
        }

        // Dynamic range coefficient
        if self.drc_present && header == HeaderType::Core {
            gb.skip(8);
        }

        // Side information CRC check word
        if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    // ───────────── inverse ADPCM / joint ─────────────

    fn inverse_adpcm(&mut self, x96: bool, vq_index_ch: usize, sb_start: usize, sb_end: usize, ofs: usize, len: usize) {
        // Gather per-band prediction data first (borrow split).
        struct BandJob {
            band: usize,
            pred_id: usize,
        }
        let mut jobs: Vec<BandJob> = Vec::new();
        for band in sb_start..sb_end {
            if self.prediction_mode[vq_index_ch][band] != 0 {
                jobs.push(BandJob {
                    band,
                    pred_id: self.prediction_vq_index[vq_index_ch][band],
                });
            }
        }
        for job in jobs {
            for j in 0..len {
                let hist = self.adpcm_hist(vq_index_ch, job.band, x96, ofs, j);
                let x = dcaadpcm_predict(job.pred_id, &hist);
                let buf = if x96 {
                    &mut self.x96_subband
                } else {
                    &mut self.subband
                };
                let idx = buf.row_base(vq_index_ch, job.band) + crate::data::DCA_ADPCM_COEFFS + ofs + j;
                buf.data[idx] = clip23(buf.data[idx] + x);
            }
        }
    }

    // ───────────── 5.5 primary audio data arrays ─────────────

    fn parse_subframe_audio(&mut self, gb: &mut BitReader, sf: usize, header: HeaderType, xch_base: usize, sub_pos: &mut usize, lfe_pos: &mut usize) -> Result<(), &'static str> {
        let mut audio = [0i32; 16];
        let nsamples = self.nsubsubframes[sf] * dca::DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err("subband sample buffer overflow");
        }

        // VQ encoded subbands
        for ch in xch_base..self.nchannels {
            let mut vq_index = [0i32; dca::DCA_SUBBANDS];

            for band in self.subband_vq_start[ch]..self.nsubbands[ch] {
                // Extract the VQ address from the bit stream
                vq_index[band] = gb.get_bits(10) as i32;
            }

            if self.subband_vq_start[ch] < self.nsubbands[ch] {
                // decode_hf writes directly into the subband rows
                let scales: Vec<[i32; 2]> = (0..dca::DCA_SUBBANDS).map(|b| self.scale_factors[ch][b]).collect();
                let start = self.subband_vq_start[ch];
                let end = self.nsubbands[ch];
                for band in start..end {
                    let coeff = &FF_DCA_HIGH_FREQ_VQ[vq_index[band] as usize];
                    let scale = scales[band][0];
                    let base = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos;
                    for j in 0..nsamples {
                        self.subband.data[base + j] = clip23((i32::from(coeff[j]) * scale + (1 << 3)) >> 4);
                    }
                            }
            }
        }

        // Low frequency effect data
        if self.lfe_present != 0 && header == HeaderType::Core {
            // Determine number of LFE samples in this subframe
            let nlfesamples = 2 * self.lfe_present as usize * self.nsubsubframes[sf];

            // Extract LFE samples from the bit stream
            for slot in audio.iter_mut().take(nlfesamples) {
                *slot = gb.get_sbits(8);
            }

            // Extract scale factor index from the bit stream
            let index = gb.get_bits(8) as usize;
            if index >= FF_DCA_SCALE_FACTOR_QUANT7.len() {
                return Err("invalid LFE scale factor index");
            }

            // Look up the 7-bit root square quantization table
            let mut scale = FF_DCA_SCALE_FACTOR_QUANT7[index] as i32;

            // Account for quantizer step size which is 0.035
            scale = mul23(4697620, scale); // 0.035 * (1 << 27)

            // Scale and take the LFE samples
            for (n, &a) in audio.iter().enumerate().take(nlfesamples) {
                self.lfe_samples[*lfe_pos + n] = clip23(a * scale >> 4);
            }

            // Advance LFE sample pointer for the next subframe
            *lfe_pos += nlfesamples;
        }

        // Audio data
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in xch_base..self.nchannels {
                for band in 0..self.subband_vq_start[ch] {
                    let abits = self.bit_allocation[ch][band];
                    let abitsu = abits.max(0) as usize;

                    // Extract bits from the bit stream
                    let huffman = self.extract_audio(gb, &mut audio, abitsu, ch)?;

                    // Select quantization step size table and look up step size
                    let step_size = if self.bit_rate == 3 {
                        FF_DCA_LOSSLESS_QUANT[abitsu]
                    } else {
                        FF_DCA_LOSSY_QUANT[abitsu]
                    } as i32;

                    // Identify transient location
                    let trans_ssf = self.transition_mode[sf][ch][band];

                    // Determine proper scale factor
                    let scale = if trans_ssf == 0 || ssf < trans_ssf {
                        self.scale_factors[ch][band][0]
                    } else {
                        self.scale_factors[ch][band][1]
                    };

                    // Adjust scale factor when SEL indicates Huffman code
                    let scale = if huffman > 0 {
                        let adj = self.scale_factor_adj[ch][abitsu - 1];
                        clip23(((i64::from(adj) * i64::from(scale)) >> 22) as i32)
                    } else {
                        scale
                    };

                                // Dequantize into the subband row
                    let base = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + ofs;
                    let (out, _rest) = self.subband.data[base..base + dca::DCA_SUBBAND_SAMPLES].split_at_mut(dca::DCA_SUBBAND_SAMPLES);
                    core_dequantize(out, &audio, step_size, scale, false);
                }
            }

            // DSYNC
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && gb.get_bits(16) != 0xffff {
                return Err("DSYNC check failed");
            }

            ofs += dca::DCA_SUBBAND_SAMPLES;
        }

        // Inverse ADPCM
        for ch in xch_base..self.nchannels {
            self.inverse_adpcm(false, ch, 0, self.nsubbands[ch], *sub_pos, nsamples);
                    }

        // Joint subband coding (reads/writes at the subframe start, like
        // FFmpeg's decode_joint(.., *sub_pos, nsamples))
        for ch in xch_base..self.nchannels {
            let src_ch = self.joint_intensity_index[ch].wrapping_sub(1);
            if self.joint_intensity_index[ch] != 0 && src_ch < dca::DCA_CHANNELS {
                let scales: Vec<i32> = (self.nsubbands[ch]..self.nsubbands[src_ch])
                    .map(|band| self.joint_scale_factors[ch][band])
                    .collect();
                let start = self.nsubbands[ch];
                let end = self.nsubbands[src_ch];
                for (bi, band) in (start..end).enumerate() {
                    for j in 0..nsamples {
                        let sv = self.subband.data[self.subband.row_base(src_ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos + j];
                        let di = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos + j;
                        self.subband.data[di] = clip23(mul17(sv, scales[bi]));
                    }
                            }
            }
        }

        // Advance subband sample pointer for the next subframe
        *sub_pos = ofs;
        Ok(())
    }
}

impl CoreDecoder {
    // ───────────── sample buffers & frame data ─────────────

    fn erase_adpcm_history(&mut self) {
        for ch in 0..dca::DCA_CHANNELS {
            for band in 0..dca::DCA_SUBBANDS {
                let n = self.subband.nchsamples;
                if n == 0 {
                    return;
                }
                let base = self.subband.row_base(ch, band);
                for v in &mut self.subband.data[base..base + crate::data::DCA_ADPCM_COEFFS] {
                    *v = 0;
                }
            }
        }
    }

    fn erase_x96_adpcm_history(&mut self) {
        for ch in 0..dca::DCA_CHANNELS {
            for band in 0..dca::DCA_SUBBANDS_X96 {
                let n = self.x96_subband.nchsamples;
                if n == 0 {
                    return;
                }
                let base = self.x96_subband.row_base(ch, band);
                for v in &mut self.x96_subband.data[base..base + crate::data::DCA_ADPCM_COEFFS] {
                    *v = 0;
                }
            }
        }
    }

    fn alloc_sample_buffer(&mut self) {
        let nchsamples = crate::data::DCA_ADPCM_COEFFS + self.npcmblocks;
        self.subband.alloc(nchsamples, dca::DCA_CHANNELS, dca::DCA_SUBBANDS);
        if !self.predictor_history {
            self.erase_adpcm_history();
        }
    }

    fn alloc_x96_sample_buffer(&mut self) {
        let nchsamples = crate::data::DCA_ADPCM_COEFFS + self.npcmblocks;
        self.x96_subband.alloc(nchsamples, dca::DCA_CHANNELS, dca::DCA_SUBBANDS_X96);
        if !self.predictor_history {
            self.erase_x96_adpcm_history();
        }
    }

    fn parse_frame_data(&mut self, gb: &mut BitReader, header: HeaderType, xch_base: usize) -> Result<(), &'static str> {
        self.parse_coding_header(gb, header, xch_base)?;

        let mut sub_pos = 0usize;
        let mut lfe_pos = dca::DCA_LFE_HISTORY;
        for sf in 0..self.nsubframes {
            self.parse_subframe_header(gb, sf, header, xch_base)?;
            self.parse_subframe_audio(gb, sf, header, xch_base, &mut sub_pos, &mut lfe_pos)?;
        }

        for ch in xch_base..self.nchannels {
            // Determine number of active subbands for this channel
            let mut nsubbands = self.nsubbands[ch];
            let jidx = self.joint_intensity_index[ch];
            if jidx != 0 && jidx.wrapping_sub(1) < dca::DCA_CHANNELS {
                nsubbands = nsubbands.max(self.nsubbands[jidx - 1]);
            }

            // Update history for ADPCM
            for band in 0..nsubbands {
                let base = self.subband.row_base(ch, band);
                // AV_COPY128(samples, samples + npcmblocks): copy the last
                // 4 samples of the frame (samples region ends at
                // base + COEFFS + npcmblocks) into the 4-sample history.
                for k in 0..crate::data::DCA_ADPCM_COEFFS {
                    self.subband.data[base + k] =
                        self.subband.data[base + crate::data::DCA_ADPCM_COEFFS + self.npcmblocks - 4 + k];
                }
            }

            // Clear inactive subbands
            for band in nsubbands..dca::DCA_SUBBANDS {
                let base = self.subband.row_base(ch, band);
                let n = self.subband.nchsamples;
                for v in &mut self.subband.data[base..base + n] {
                    *v = 0;
                }
            }
        }

        Ok(())
    }
}


impl CoreDecoder {
    // ───────────── XCH / XXCH / XBR / X96 extensions ─────────────

    fn parse_xch_frame(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        if self.ch_mask & spk::CS != 0 {
            return Err("XCH with Cs speaker already present");
        }

        self.parse_frame_data(gb, HeaderType::Xch, self.nchannels)?;

        // Seek to the end of core frame, don't trust XCH frame size
        if !gb.seek_bits(self.frame_size * 8) {
            return Err("read past end of XCH frame");
        }
        Ok(())
    }

    fn parse_xxch_frame(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        let header_pos = gb.bits_read();

        // XXCH sync word
        if gb.get_bits_long(32) != dca::DCA_SYNCWORD_XXCH {
            return Err("invalid XXCH sync word");
        }

        // XXCH frame header length
        let header_size = gb.get_bits(6) as usize + 1;

        // CRC unchecked (FFmpeg default: no AV_EF_CRCCHECK)

        // CRC presence flag for channel set header
        self.xxch_crc_present = gb.get_bits(1) != 0;

        // Number of bits for loudspeaker mask
        self.xxch_mask_nbits = gb.get_bits(5) as usize + 1;
        if self.xxch_mask_nbits <= speaker::CS {
            return Err("invalid number of bits for XXCH speaker mask");
        }

        // Number of channel sets
        let xxch_nchsets = gb.get_bits(2) as usize + 1;
        if xxch_nchsets > 1 {
            return Err("multiple XXCH channel sets not supported");
        }

        // Channel set 0 data byte size
        let xxch_frame_size = gb.get_bits(14) as usize + 1;

        // Core loudspeaker activity mask
        let m = gb.get_bits_long(self.xxch_mask_nbits as u32);
        self.xxch_core_mask = m;

        // Validate the core mask
        let mut m2 = self.ch_mask;

        if (m2 & spk::LS) != 0 && (self.xxch_core_mask & spk::LSS) != 0 {
            m2 = (m2 & !spk::LS) | spk::LSS;
        }

        if (m2 & spk::RS) != 0 && (self.xxch_core_mask & spk::RSS) != 0 {
            m2 = (m2 & !spk::RS) | spk::RSS;
        }

        if m2 != self.xxch_core_mask {
            return Err("XXCH core speaker activity mask disagrees with core");
        }

        // Reserved, byte align, CRC16 of XXCH frame header
        if !gb.seek_bits(header_pos + header_size * 8) {
            return Err("read past end of XXCH frame header");
        }

        // Parse XXCH channel set 0
        self.parse_frame_data(gb, HeaderType::Xxch, self.nchannels)?;

        if !gb.seek_bits(header_pos + header_size * 8 + xxch_frame_size * 8) {
            return Err("read past end of XXCH channel set");
        }

        Ok(())
    }

    fn parse_xbr_subframe(
        &mut self,
        gb: &mut BitReader,
        xbr_base_ch: usize,
        xbr_nchannels: usize,
        xbr_nsubbands: &[usize],
        xbr_transition_mode: bool,
        sf: usize,
        sub_pos: &mut usize,
    ) -> Result<(), &'static str> {
        let mut xbr_nabits = [0usize; dca::DCA_CHANNELS];
        let mut xbr_bit_allocation = [[0usize; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS];
        let mut xbr_scale_nbits = [0usize; dca::DCA_CHANNELS];
        let mut xbr_scale_factors = [[[0i32; 2]; dca::DCA_SUBBANDS]; dca::DCA_CHANNELS];

        // Check number of subband samples in this subframe
        if *sub_pos + self.nsubsubframes[sf] * dca::DCA_SUBBAND_SAMPLES > self.npcmblocks {
            return Err("subband sample buffer overflow");
        }

        // Number of bits for XBR bit allocation index
        for ch in xbr_base_ch..xbr_nchannels {
            xbr_nabits[ch] = gb.get_bits(2) as usize + 2;
        }

        // XBR bit allocation index
        for ch in xbr_base_ch..xbr_nchannels {
            for band in 0..xbr_nsubbands[ch] {
                xbr_bit_allocation[ch][band] = gb.get_bits(xbr_nabits[ch] as u32) as usize;
                if xbr_bit_allocation[ch][band] as i32 > DCA_ABITS_MAX {
                    return Err("invalid XBR bit allocation index");
                }
            }
        }

        // Number of bits for scale indices
        for ch in xbr_base_ch..xbr_nchannels {
            xbr_scale_nbits[ch] = gb.get_bits(3) as usize;
            if xbr_scale_nbits[ch] == 0 {
                return Err("invalid number of bits for XBR scale factor index");
            }
        }

        // XBR scale factors
        for ch in xbr_base_ch..xbr_nchannels {
            // Select the root square table
            let (table, size): (&[u32], usize) = if self.scale_factor_sel[ch] > 5 {
                (&FF_DCA_SCALE_FACTOR_QUANT7, FF_DCA_SCALE_FACTOR_QUANT7.len())
            } else {
                (&FF_DCA_SCALE_FACTOR_QUANT6, FF_DCA_SCALE_FACTOR_QUANT6.len())
            };

            // Parse scale factor indices and look up scale factors
            for band in 0..xbr_nsubbands[ch] {
                if xbr_bit_allocation[ch][band] != 0 {
                    let mut scale_index = gb.get_bits(xbr_scale_nbits[ch] as u32) as usize;
                    if scale_index >= size {
                        return Err("invalid XBR scale factor index");
                    }
                    xbr_scale_factors[ch][band][0] = table[scale_index] as i32;
                    if xbr_transition_mode && self.transition_mode[sf][ch][band] != 0 {
                        scale_index = gb.get_bits(xbr_scale_nbits[ch] as u32) as usize;
                        if scale_index >= size {
                            return Err("invalid XBR scale factor index");
                        }
                        xbr_scale_factors[ch][band][1] = table[scale_index] as i32;
                    }
                }
            }
        }

        // Audio data
        let mut audio = [0i32; dca::DCA_SUBBAND_SAMPLES];
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in xbr_base_ch..xbr_nchannels {
                for band in 0..xbr_nsubbands[ch] {
                    let trans_ssf = if xbr_transition_mode {
                        self.transition_mode[sf][ch][band]
                    } else {
                        0
                    };
                    let abits = xbr_bit_allocation[ch][band];
            
                    // Extract bits from the bit stream
                    if abits > 7 {
                        for slot in audio.iter_mut() {
                            *slot = gb.get_sbits(abits as u32 - 3);
                        }
                    } else if abits > 0 {
                        self.parse_block_codes(gb, &mut audio, abits)?;
                    } else {
                        // No bits allocated
                        continue;
                    }

                    // Look up quantization step size
                    let step_size = FF_DCA_LOSSLESS_QUANT[abits] as i32;

                    // Determine proper scale factor
                    let scale = if trans_ssf == 0 || ssf < trans_ssf {
                        xbr_scale_factors[ch][band][0]
                    } else {
                        xbr_scale_factors[ch][band][1]
                    };

                    let base = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + ofs;
                    let out = &mut self.subband.data[base..base + dca::DCA_SUBBAND_SAMPLES];
                    core_dequantize(out, &audio, step_size, scale, true);
                            }
            }

            // DSYNC
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && gb.get_bits(16) != 0xffff {
                return Err("XBR-DSYNC check failed");
            }

            ofs += dca::DCA_SUBBAND_SAMPLES;
        }

        // Advance subband sample pointer for the next subframe
        *sub_pos = ofs;
        Ok(())
    }

    fn parse_xbr_frame(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        let mut xbr_frame_size = [0usize; dca::DCA_EXSS_CHSETS_MAX];
        let mut xbr_nchannels = [0usize; dca::DCA_EXSS_CHSETS_MAX];
        let mut xbr_nsubbands = [0usize; dca::DCA_EXSS_CHSETS_MAX * dca::DCA_EXSS_CHANNELS_MAX];
        let header_pos = gb.bits_read();

        // XBR sync word
        if gb.get_bits_long(32) != dca::DCA_SYNCWORD_XBR {
            return Err("invalid XBR sync word");
        }

        // XBR frame header length
        let header_size = gb.get_bits(6) as usize + 1;

        // Number of channel sets
        let xbr_nchsets = gb.get_bits(2) as usize + 1;

        // Channel set data byte size
        for size in xbr_frame_size.iter_mut().take(xbr_nchsets) {
            *size = gb.get_bits(14) as usize + 1;
        }

        // Transition mode flag
        let xbr_transition_mode = gb.get_bits(1) != 0;

        // Channel set headers
        let mut ch2 = 0usize;
        for i in 0..xbr_nchsets {
            xbr_nchannels[i] = gb.get_bits(3) as usize + 1;
            let xbr_band_nbits = gb.get_bits(2) as usize + 5;
            for _ in 0..xbr_nchannels[i] {
                xbr_nsubbands[ch2] = gb.get_bits(xbr_band_nbits as u32) as usize + 1;
                if xbr_nsubbands[ch2] > dca::DCA_SUBBANDS {
                    return Err("invalid number of active XBR subbands");
                }
                ch2 += 1;
            }
        }

        // Reserved, byte align, CRC16 of XBR frame header
        if !gb.seek_bits(header_pos + header_size * 8) {
            return Err("read past end of XBR frame header");
        }

        // Channel set data
        let mut xbr_base_ch = 0usize;
        for i in 0..xbr_nchsets {
            let header_pos = gb.bits_read();

            if xbr_base_ch + xbr_nchannels[i] <= self.nchannels {
                let mut sub_pos = 0usize;
                for sf in 0..self.nsubframes {
                    self.parse_xbr_subframe(
                        gb,
                        xbr_base_ch,
                        xbr_base_ch + xbr_nchannels[i],
                        &xbr_nsubbands,
                        xbr_transition_mode,
                        sf,
                        &mut sub_pos,
                    )?;
                }
            }

            xbr_base_ch += xbr_nchannels[i];

            if !gb.seek_bits(header_pos + xbr_frame_size[i] * 8) {
                return Err("read past end of XBR channel set");
            }
        }

        Ok(())
    }

    /// `rand_x96`: LCG in [-2^30, 2^30 - 1].
    fn rand_x96(&mut self) -> i32 {
        self.x96_rand = 1103515245u32
            .wrapping_mul(self.x96_rand)
            .wrapping_add(12345);
        (self.x96_rand & 0x7fffffff) as i32 - 0x40000000
    }
}

impl CoreDecoder {
    // ───────────── X96 ─────────────

    fn parse_x96_subframe_audio(&mut self, gb: &mut BitReader, sf: usize, xch_base: usize, sub_pos: &mut usize) -> Result<(), &'static str> {
        let nsamples = self.nsubsubframes[sf] * dca::DCA_SUBBAND_SAMPLES;
        if *sub_pos + nsamples > self.npcmblocks {
            return Err("subband sample buffer overflow");
        }

        // VQ encoded or unallocated subbands
        for ch in xch_base..self.x96_nchannels {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                let scale = self.scale_factors[ch][band >> 1][band & 1];
                let base = self.x96_subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos;
                let ba = self.bit_allocation[ch][band];

                match ba {
                    0 => {
                        // No bits allocated for subband
                        if scale <= 1 {
                            let row = &mut self.x96_subband.data[base..base + nsamples];
                            row.iter_mut().for_each(|v| *v = 0);
                        } else {
                            let rand_vals: Vec<i32> =
                                (0..nsamples).map(|_| mul31(self.rand_x96(), scale)).collect();
                            let row = &mut self.x96_subband.data[base..base + nsamples];
                            row.copy_from_slice(&rand_vals);
                        }
                    }
                    1 => {
                        // VQ encoded subband: read VQ addresses first, then
                        // write (splits the gb/buffer borrows).
                        let row = &mut self.x96_subband.data[base..base + nsamples];
                        let ssfs = (self.nsubsubframes[sf] + 1) / 2;
                        let mut chunk_vals: Vec<Vec<i32>> = Vec::with_capacity(ssfs);
                        let mut chunk_lens: Vec<usize> = Vec::with_capacity(ssfs);
                        for ssf in 0..ssfs {
                            let vq_samples = &FF_DCA_HIGH_FREQ_VQ[gb.get_bits(10) as usize];
                            let cnt = (nsamples - ssf * 16).min(16);
                            chunk_vals.push((0..cnt).map(|k| clip23((i32::from(vq_samples[k]) * scale + (1 << 3)) >> 4)).collect());
                            chunk_lens.push(cnt);
                        }
                        let mut pos = 0usize;
                        for (vals, cnt) in chunk_vals.iter().zip(chunk_lens.iter()) {
                            row[pos..pos + cnt].copy_from_slice(&vals[..*cnt]);
                            pos += cnt;
                        }
                    }
                    _ => {}
                }
            }
        }

        // Audio data
        let mut audio = [0i32; dca::DCA_SUBBAND_SAMPLES];
        let mut ofs = *sub_pos;
        for ssf in 0..self.nsubsubframes[sf] {
            for ch in xch_base..self.x96_nchannels {
                for band in self.x96_subband_start..self.nsubbands[ch] {
                    let abits = self.bit_allocation[ch][band] - 1;

                    // Not VQ encoded or unallocated subbands
                    if abits < 1 {
                        continue;
                    }
                    let abitsu = abits as usize;

                    // Extract bits from the bit stream
                    self.extract_audio(gb, &mut audio, abitsu, ch)?;

                    // Select quantization step size table
                    let step_size = if self.bit_rate == 3 {
                        FF_DCA_LOSSLESS_QUANT[abitsu]
                    } else {
                        FF_DCA_LOSSY_QUANT[abitsu]
                    } as i32;

                    // Get the scale factor
                    let scale = self.scale_factors[ch][band >> 1][band & 1];

                    let base = self.x96_subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + ofs;
                    let out = &mut self.x96_subband.data[base..base + dca::DCA_SUBBAND_SAMPLES];
                    core_dequantize(out, &audio, step_size, scale, false);
                }
            }

            // DSYNC
            if (ssf == self.nsubsubframes[sf] - 1 || self.sync_ssf) && gb.get_bits(16) != 0xffff {
                return Err("X96-DSYNC check failed");
            }

            ofs += dca::DCA_SUBBAND_SAMPLES;
        }

        // Inverse ADPCM
        for ch in xch_base..self.x96_nchannels {
            self.inverse_adpcm(true, ch, self.x96_subband_start, self.nsubbands[ch], *sub_pos, nsamples);
        }

        // Joint subband coding (reads/writes at the subframe start, like
        // FFmpeg's decode_joint(.., *sub_pos, nsamples))
        for ch in xch_base..self.x96_nchannels {
            let src_ch = self.joint_intensity_index[ch].wrapping_sub(1);
            if self.joint_intensity_index[ch] != 0 && src_ch < dca::DCA_CHANNELS {
                let scales: Vec<i32> = (self.nsubbands[ch]..self.nsubbands[src_ch])
                    .map(|band| self.joint_scale_factors[ch][band])
                    .collect();
                let start = self.nsubbands[ch];
                let end = self.nsubbands[src_ch];
                for (bi, band) in (start..end).enumerate() {
                    for j in 0..nsamples {
                        let sv = self.x96_subband.data[self.x96_subband.row_base(src_ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos + j];
                        let di = self.x96_subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS + *sub_pos + j;
                        self.x96_subband.data[di] = clip23(mul17(sv, scales[bi]));
                    }
                }
            }
        }

        // Advance subband sample pointer for the next subframe
        *sub_pos = ofs;
        Ok(())
    }

    fn parse_x96_subframe_header(&mut self, gb: &mut BitReader, xch_base: usize) -> Result<(), &'static str> {
        // Prediction mode
        for ch in xch_base..self.x96_nchannels {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                self.prediction_mode[ch][band] = gb.get_bits(1) as i32;
            }
        }

        // Prediction coefficients VQ address
        for ch in xch_base..self.x96_nchannels {
            for band in self.x96_subband_start..self.nsubbands[ch] {
                if self.prediction_mode[ch][band] != 0 {
                    self.prediction_vq_index[ch][band] = gb.get_bits(12) as usize;
                }
            }
        }

        // Bit allocation index
        for ch in xch_base..self.x96_nchannels {
            let sel = self.bit_allocation_sel[ch];
            let mut abits: i32 = 0;

            for band in self.x96_subband_start..self.nsubbands[ch] {
                // If Huffman code was used, the difference of abits was encoded
                if sel < 7 {
                    let book = &self.vlcs.quant_index[5 + 2 * usize::from(self.x96_high_res)][sel];
                    abits += book.get(gb, 2);
                } else {
                    abits = gb.get_bits(3 + u32::from(self.x96_high_res)) as i32;
                }

                let cap = 7 + 8 * i32::from(self.x96_high_res);
                if abits < 0 || abits > cap {
                    return Err("invalid X96 bit allocation index");
                }

                self.bit_allocation[ch][band] = abits;
            }
        }

        // Scale factors
        for ch in xch_base..self.x96_nchannels {
            let sel = self.scale_factor_sel[ch];
            let mut scale_index: usize = 0;

            // Extract scales for subbands transmitted even for unallocated
            for band in self.x96_subband_start..self.nsubbands[ch] {
                let s = self.parse_scale(gb, &mut scale_index, sel)?;
                self.scale_factors[ch][band >> 1][band & 1] = s;
            }
        }

        // Joint subband codebook select
        for ch in xch_base..self.x96_nchannels {
            if self.joint_intensity_index[ch] != 0 {
                self.joint_scale_sel[ch] = gb.get_bits(3) as usize;
                if self.joint_scale_sel[ch] == 7 {
                    return Err("invalid X96 joint scale factor code book");
                }
            }
        }

        // Scale factors for joint subband coding
        for ch in xch_base..self.x96_nchannels {
            let src_ch = self.joint_intensity_index[ch].wrapping_sub(1);
            if self.joint_intensity_index[ch] != 0 && src_ch < dca::DCA_CHANNELS {
                let sel = self.joint_scale_sel[ch];
                for band in self.nsubbands[ch]..self.nsubbands[src_ch] {
                    let s = self.parse_joint_scale(gb, sel)?;
                    self.joint_scale_factors[ch][band] = s;
                }
            }
        }

        // Side information CRC check word
        if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_x96_coding_header(&mut self, gb: &mut BitReader, exss: bool, xch_base: usize) -> Result<(), &'static str> {
        let header_pos = gb.bits_read();
        let mut header_size = 0usize;

        if exss {
            // Channel set header length
            header_size = gb.get_bits(7) as usize + 1;
        }

        // High resolution flag
        self.x96_high_res = gb.get_bits(1) != 0;

        // First encoded subband
        if self.x96_rev_no < 8 {
            self.x96_subband_start = gb.get_bits(5) as usize;
            if self.x96_subband_start > 27 {
                return Err("invalid X96 subband start index");
            }
        } else {
            self.x96_subband_start = dca::DCA_SUBBANDS;
        }

        // Subband activity count
        for ch in xch_base..self.x96_nchannels {
            self.nsubbands[ch] = gb.get_bits(6) as usize + 1;
            if self.nsubbands[ch] < dca::DCA_SUBBANDS {
                return Err("invalid X96 subband activity count");
            }
        }

        // Joint intensity coding index
        for ch in xch_base..self.x96_nchannels {
            let mut n = gb.get_bits(3) as usize;
            if n != 0 && xch_base != 0 {
                n += xch_base - 1;
            }
            if n > self.x96_nchannels {
                return Err("invalid X96 joint intensity coding index");
            }
            self.joint_intensity_index[ch] = n;
        }

        // Scale factor code book
        for ch in xch_base..self.x96_nchannels {
            self.scale_factor_sel[ch] = gb.get_bits(3) as usize;
            if self.scale_factor_sel[ch] >= 6 {
                return Err("invalid X96 scale factor code book");
            }
        }

        // Bit allocation quantizer select
        for ch in xch_base..self.x96_nchannels {
            self.bit_allocation_sel[ch] = gb.get_bits(3) as usize;
        }

        // Quantization index codebook select
        for n in 0..6 + 4 * usize::from(self.x96_high_res) {
            for ch in xch_base..self.x96_nchannels {
                self.quant_index_sel[ch][n] = gb.get_bits(u32::from(FF_DCA_QUANT_INDEX_SEL_NBITS[n])) as usize;
            }
        }

        if exss {
            // Reserved, byte align, CRC16 of channel set header
            if !gb.seek_bits(header_pos + header_size * 8) {
                return Err("read past end of X96 channel set header");
            }
        } else if self.crc_present {
            gb.skip(16);
        }

        Ok(())
    }

    fn parse_x96_frame_data(&mut self, gb: &mut BitReader, exss: bool, xch_base: usize) -> Result<(), &'static str> {
        self.parse_x96_coding_header(gb, exss, xch_base)?;

        let mut sub_pos = 0usize;
        for sf in 0..self.nsubframes {
            self.parse_x96_subframe_header(gb, xch_base)?;
            self.parse_x96_subframe_audio(gb, sf, xch_base, &mut sub_pos)?;
        }

        for ch in xch_base..self.x96_nchannels {
            // Determine number of active subbands for this channel
            let mut nsubbands = self.nsubbands[ch];
            let jidx = self.joint_intensity_index[ch];
            if jidx != 0 && jidx.wrapping_sub(1) < dca::DCA_CHANNELS {
                nsubbands = nsubbands.max(self.nsubbands[jidx - 1]);
            }

            // Update history for ADPCM and clear inactive subbands
            for band in 0..dca::DCA_SUBBANDS_X96 {
                let base = self.x96_subband.row_base(ch, band);
                if band >= self.x96_subband_start && band < nsubbands {
                    for k in 0..crate::data::DCA_ADPCM_COEFFS {
                        self.x96_subband.data[base + k] =
                            self.x96_subband.data[base + crate::data::DCA_ADPCM_COEFFS + self.npcmblocks - 4 + k];
                    }
                } else {
                    let n = self.x96_subband.nchsamples;
                    for v in &mut self.x96_subband.data[base..base + n] {
                        *v = 0;
                    }
                }
            }
        }

        Ok(())
    }

    fn parse_x96_frame(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        // Revision number
        self.x96_rev_no = gb.get_bits(4) as usize;
        if !(1..=8).contains(&self.x96_rev_no) {
            return Err("invalid X96 revision");
        }

        self.x96_crc_present = false;
        self.x96_nchannels = self.nchannels;

        self.alloc_x96_sample_buffer();
        self.parse_x96_frame_data(gb, false, 0)?;

        // Seek to the end of core frame
        if !gb.seek_bits(self.frame_size * 8) {
            return Err("read past end of X96 frame");
        }

        Ok(())
    }

    fn parse_x96_frame_exss(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        let mut x96_frame_size = [0usize; dca::DCA_EXSS_CHSETS_MAX];
        let mut x96_nchannels = [0usize; dca::DCA_EXSS_CHSETS_MAX];
        let header_pos = gb.bits_read();

        // X96 sync word
        if gb.get_bits_long(32) != dca::DCA_SYNCWORD_X96 {
            return Err("invalid X96 sync word");
        }

        // X96 frame header length
        let header_size = gb.get_bits(6) as usize + 1;

        // Revision number
        self.x96_rev_no = gb.get_bits(4) as usize;
        if !(1..=8).contains(&self.x96_rev_no) {
            return Err("invalid X96 revision");
        }

        // CRC presence flag for channel set header
        self.x96_crc_present = gb.get_bits(1) != 0;

        // Number of channel sets
        let x96_nchsets = gb.get_bits(2) as usize + 1;

        // Channel set data byte size
        for size in x96_frame_size.iter_mut().take(x96_nchsets) {
            *size = gb.get_bits(12) as usize + 1;
        }

        // Number of channels in channel set
        for n in x96_nchannels.iter_mut().take(x96_nchsets) {
            *n = gb.get_bits(3) as usize + 1;
        }

        // Reserved, byte align, CRC16 of X96 frame header
        if !gb.seek_bits(header_pos + header_size * 8) {
            return Err("read past end of X96 frame header");
        }

        self.alloc_x96_sample_buffer();

        // Channel set data
        self.x96_nchannels = 0;
        let mut x96_base_ch = 0usize;
        for i in 0..x96_nchsets {
            let header_pos = gb.bits_read();

            if x96_base_ch + x96_nchannels[i] <= self.nchannels {
                self.x96_nchannels = x96_base_ch + x96_nchannels[i];
                self.parse_x96_frame_data(gb, true, x96_base_ch)?;
            }

            x96_base_ch += x96_nchannels[i];

            if !gb.seek_bits(header_pos + x96_frame_size[i] * 8) {
                return Err("read past end of X96 channel set");
            }
        }

        Ok(())
    }
}

impl CoreDecoder {
    // ───────────── aux data & optional info ─────────────

    fn parse_aux_data(&mut self, gb: &mut BitReader) -> Result<(), &'static str> {
        // Auxiliary data byte count (can't be trusted)
        gb.skip(6);

        // 4-byte align: skip_bits_long(&s->gb, -get_bits_count(&s->gb) & 31)
        let pad = (32 - gb.bits_read() % 32) % 32;
        gb.skip(pad as u32);

        // Auxiliary data sync word
        if gb.get_bits_long(32) != dca::DCA_SYNCWORD_REV1AUX {
            return Err("invalid auxiliary data sync word");
        }

        // Auxiliary decode time stamp flag
        if gb.get_bits(1) != 0 {
            gb.skip(47);
        }

        // Auxiliary dynamic downmix flag
        self.prim_dmix_embedded = gb.get_bits(1) != 0;
        if self.prim_dmix_embedded {
            // Auxiliary primary channel downmix type
            self.prim_dmix_type = gb.get_bits(3) as usize;
            if self.prim_dmix_type >= crate::dca::dmix_type::COUNT {
                return Err("invalid primary channel set downmix type");
            }

            // Size of downmix coefficients matrix
            let m = FF_DCA_DMIX_PRIMARY_NCH[self.prim_dmix_type] as usize;
            let n = FF_DCA_CHANNELS[self.audio_mode] as usize + usize::from(self.lfe_present != 0);

            // Dynamic downmix code coefficients
            for i in 0..m * n {
                let code = gb.get_bits(9) as usize;
                let sign = (code >> 8) as i32 - 1;
                let index = code & 0xff;
                if index >= FF_DCA_DMIXTABLE_SIZE {
                    return Err("invalid downmix coefficient index");
                }
                let c = i32::from(FF_DCA_DMIXTABLE[index]);
                self.prim_dmix_coeff[i] = ((c as u32 ^ sign as u32).wrapping_sub(sign as u32)) as i32;
            }
        }

        // Byte align
        let pad = (8 - gb.bits_read() % 8) % 8;
        gb.skip(pad as u32);

        // CRC16 of auxiliary data
        gb.skip(16);

        // CRC unchecked (FFmpeg default)
        Ok(())
    }

    fn parse_optional_info(&mut self, gb: &mut BitReader, buf: &[u8]) -> Result<(), &'static str> {
        // Time code stamp
        if self.ts_present {
            gb.skip(32);
        }

        // Auxiliary data (errors tolerated like FFmpeg without EXPLODE)
        let mut aux_failed = false;
        if self.aux_present {
            aux_failed = self.parse_aux_data(gb).is_err();
        }
        if aux_failed {
            self.prim_dmix_embedded = false;
            // Re-sync to frame_size end below via the caller's seek.
            // FFmpeg continues parsing extensions after a failed aux.
        }

        // Core extensions
        if self.ext_audio_present && !self.core_only {
            let sync_pos_limit = self.frame_size / 4;
            let last_pos = gb.bits_read() / 32;

            // Search for extension sync words aligned on 4-byte boundary,
            // backwards from the end of the core frame.
            let mut w2: u32 = 0;
            let mut found = false;
            match self.ext_audio_type {
                t if t == ext_audio_type::XCH as usize => {
                    if self.request_channel_layout == 0 {
                        let mut sync_pos = sync_pos_limit.saturating_sub(1);
                        while sync_pos >= last_pos && sync_pos * 4 + 4 <= buf.len() {
                            let w1 = u32::from_be_bytes([buf[sync_pos * 4], buf[sync_pos * 4 + 1], buf[sync_pos * 4 + 2], buf[sync_pos * 4 + 3]]);
                            if w1 == dca::DCA_SYNCWORD_XCH {
                                let size = ((w2 >> 22) as usize) + 1;
                                let dist = self.frame_size - sync_pos * 4;
                                if size >= 96 && (size == dist || size - 1 == dist) && (w2 >> 15 & 0x7f) == 0x08 {
                                    self.xch_pos = sync_pos * 32 + 49;
                                    found = true;
                                    break;
                                }
                            }
                            w2 = w1;
                            if sync_pos == 0 {
                                break;
                            }
                            sync_pos -= 1;
                        }
                    }
                    if !found {
                        self.xch_pos = 0;
                    }
                }
                t if t == ext_audio_type::X96 as usize => {
                    let mut sync_pos = sync_pos_limit.saturating_sub(1);
                    while sync_pos >= last_pos && sync_pos * 4 + 4 <= buf.len() {
                        let w1 = u32::from_be_bytes([buf[sync_pos * 4], buf[sync_pos * 4 + 1], buf[sync_pos * 4 + 2], buf[sync_pos * 4 + 3]]);
                        if w1 == dca::DCA_SYNCWORD_X96 {
                            let size = ((w2 >> 20) as usize) + 1;
                            let dist = self.frame_size - sync_pos * 4;
                            if size >= 96 && size == dist {
                                self.x96_pos = sync_pos * 32 + 44;
                                found = true;
                                break;
                            }
                        }
                        w2 = w1;
                        if sync_pos == 0 {
                            break;
                        }
                        sync_pos -= 1;
                    }
                    if !found {
                        self.x96_pos = 0;
                    }
                }
                t if t == ext_audio_type::XXCH as usize => {
                    if self.request_channel_layout == 0 {
                        let mut sync_pos = sync_pos_limit.saturating_sub(1);
                        while sync_pos >= last_pos && sync_pos * 4 + 4 <= buf.len() {
                            let w1 = u32::from_be_bytes([buf[sync_pos * 4], buf[sync_pos * 4 + 1], buf[sync_pos * 4 + 2], buf[sync_pos * 4 + 3]]);
                            if w1 == dca::DCA_SYNCWORD_XXCH {
                                let size = ((w2 >> 26) as usize) + 1;
                                let dist = buf.len() - sync_pos * 4;
                                if size >= 11 && size <= dist {
                                    // CRC must be valid (this one the C always checks)
                                    let crc_off = (sync_pos + 1) * 4;
                                    if crc_off + size - 4 <= buf.len()
                                        && crate::crc16::av_crc16_ccitt(0xffff, &buf[crc_off..crc_off + size - 4]) == 0
                                    {
                                        self.xxch_pos = sync_pos * 32;
                                        found = true;
                                        break;
                                    }
                                }
                            }
                            w2 = w1;
                            if sync_pos == 0 {
                                break;
                            }
                            sync_pos -= 1;
                        }
                    }
                    if !found {
                        self.xxch_pos = 0;
                    }
                }
                _ => {}
            }
        }

        Ok(())
    }

    // ───────────── ff_dca_core_parse / parse_exss ─────────────

    pub fn core_parse(&mut self, data: &[u8]) -> Result<(), &'static str> {
        self.ext_audio_mask = 0;
        self.xch_pos = 0;
        self.xxch_pos = 0;
        self.x96_pos = 0;

        let mut gb = BitReader::new(data);

        self.parse_frame_header(&mut gb)?;
        self.alloc_sample_buffer();
        self.parse_frame_data(&mut gb, HeaderType::Core, 0)?;
        self.parse_optional_info(&mut gb, data)?;

        // Workaround for DTS in WAV
        if self.frame_size > data.len() {
            self.frame_size = data.len();
        }

        // Read past end of core frame is tolerated (FFmpeg without EXPLODE)
        let _ = gb.seek_bits(self.frame_size * 8);

        Ok(())
    }

    pub fn core_parse_exss(&mut self, data: &[u8], asset: Option<&ExssAsset>) -> Result<(), &'static str> {
        let exss_mask = asset.map_or(0, |a| a.extension_mask);
        let mut failed = false;

        // Parse (X)XCH unless downmixing
        if self.request_channel_layout == 0 {
            let mut ext = 0i32;
            if exss_mask & crate::dca::exss_mask::EXSS_XXCH != 0 {
                let asset = asset.unwrap();
                let sub = &data[asset.xxch_offset..(asset.xxch_offset + asset.xxch_size).min(data.len())];
                let mut gb = BitReader::new(sub);
                let r = self.parse_xxch_frame(&mut gb);
                ext = crate::dca::exss_mask::EXSS_XXCH;
                if r.is_err() {
                    failed = true;
                }
            } else if self.xxch_pos != 0 {
                let mut gb = BitReader::new(data);
                gb.skip_long(self.xxch_pos as i32);
                let r = self.parse_xxch_frame(&mut gb);
                ext = crate::dca::css_mask::XXCH;
                if r.is_err() {
                    failed = true;
                }
            } else if self.xch_pos != 0 {
                let mut gb = BitReader::new(data);
                gb.skip_long(self.xch_pos as i32);
                let r = self.parse_xch_frame(&mut gb);
                ext = crate::dca::css_mask::XCH;
                if r.is_err() {
                    failed = true;
                }
            }

            // Revert to primary channel set in case (X)XCH parsing fails
            if failed {
                self.nchannels = FF_DCA_CHANNELS[self.audio_mode] as usize;
                self.ch_mask = u32::from(AUDIO_MODE_CH_MASK[self.audio_mode]);
                if self.lfe_present != 0 {
                    self.ch_mask |= spk::LFE1;
                }
            } else {
                self.ext_audio_mask |= ext;
            }
        }

        // Parse XBR
        if exss_mask & crate::dca::exss_mask::XBR != 0 {
            let asset = asset.unwrap();
            let sub = &data[asset.xbr_offset..(asset.xbr_offset + asset.xbr_size).min(data.len())];
            let mut gb = BitReader::new(sub);
            match self.parse_xbr_frame(&mut gb) {
                Err(_) => {}
                Ok(()) => self.ext_audio_mask |= crate::dca::exss_mask::XBR,
            }
        }


        // Parse X96 unless decoding XLL
        if self.packet & packet_xll(self) == 0 {
            if exss_mask & crate::dca::exss_mask::EXSS_X96 != 0 {
                let asset = asset.unwrap();
                let sub = &data[asset.x96_offset..(asset.x96_offset + asset.x96_size).min(data.len())];
                let mut gb = BitReader::new(sub);
                match self.parse_x96_frame_exss(&mut gb) {
                    Ok(()) => self.ext_audio_mask |= crate::dca::exss_mask::EXSS_X96,
                    Err(_) => {}
                }
            } else if self.x96_pos != 0 {
                let mut gb = BitReader::new(data);
                gb.skip_long(self.x96_pos as i32);
                match self.parse_x96_frame(&mut gb) {
                    Ok(()) => self.ext_audio_mask |= crate::dca::css_mask::X96,
                    Err(_) => {}
                }
            }
        }

        Ok(())
    }

    // ───────────── flush / DSP history ─────────────

    fn erase_dsp_history(&mut self) {
        for d in &mut self.dcadsp_data {
            *d = DspData::default();
        }
        self.output_history_lfe_fixed = 0;
        self.output_history_lfe_float = 0.0;
    }

    fn set_filter_mode(&mut self, mode: i32) {
        if self.filter_mode != mode {
            self.erase_dsp_history();
            self.filter_mode = mode;
        }
    }

    /// `ff_dca_core_flush`.
    pub fn core_flush(&mut self) {
        if !self.subband.data.is_empty() {
            self.erase_adpcm_history();
            for v in self.lfe_samples.iter_mut().take(dca::DCA_LFE_HISTORY) {
                *v = 0;
            }
        }
        if !self.x96_subband.data.is_empty() {
            self.erase_x96_adpcm_history();
        }
        self.erase_dsp_history();
    }
}

impl CoreDecoder {
    // ───────────── channel mapping & filtering ─────────────

    fn map_prm_ch_to_spkr(&self, ch: usize) -> Result<usize, &'static str> {
        // Try to map this channel to core first
        let pos = FF_DCA_CHANNELS[self.audio_mode] as usize;
        if ch < pos {
            let spkr = PRM_CH_TO_SPKR_MAP[self.audio_mode][ch];
            if self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::exss_mask::EXSS_XXCH) != 0 {
                if self.xxch_core_mask & (1u32 << spkr) != 0 {
                    return Ok(spkr as usize);
                }
                if spkr == speaker::LS as i32 && (self.xxch_core_mask & spk::LSS) != 0 {
                    return Ok(speaker::LSS);
                }
                if spkr == speaker::RS as i32 && (self.xxch_core_mask & spk::RSS) != 0 {
                    return Ok(speaker::RSS);
                }
                return Err("channel has no speaker mapping (XXCH)");
            }
            return Ok(spkr as usize);
        }

        // Then XCH
        if (self.ext_audio_mask & crate::dca::css_mask::XCH) != 0 && ch == pos {
            return Ok(speaker::CS);
        }

        // Then XXCH
        if self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::exss_mask::EXSS_XXCH) != 0 {
            let mut pos2 = pos;
            for spkr in speaker::CS..self.xxch_mask_nbits {
                if self.xxch_spkr_mask & (1u32 << spkr) != 0 {
                    if pos2 == ch {
                        return Ok(spkr);
                    }
                    pos2 += 1;
                }
            }
        }

        Err("no speaker mapping for channel")
    }

    /// `ff_dca_core_filter_fixed`. Fills `self.output` with per-speaker
    /// planes of `nsamples` int32 samples. `x96_synth`: -1 = automatic.
    pub fn core_filter_fixed(&mut self, x96_synth_in: i32) -> Result<(), &'static str> {
        let mut x96_nchannels = 0usize;
        let mut x96_synth = x96_synth_in;

        // Externally set x96_synth flag implies that X96 synthesis should be
        // enabled, yet actual X96 subband data should be discarded.
        if x96_synth == 0 && (self.ext_audio_mask & (crate::dca::css_mask::X96 | crate::dca::exss_mask::EXSS_X96)) != 0 {
            x96_nchannels = self.x96_nchannels;
            x96_synth = 1;
        }
        if x96_synth < 0 {
            x96_synth = 0;
        }
        let x96 = x96_synth != 0;

        self.output_rate = self.sample_rate << x96_synth;
        let nsamples = (self.npcmblocks * dca::DCA_PCMBLOCK_SAMPLES) << x96_synth;
        self.npcmsamples = nsamples;

        // Reallocate PCM output buffer (one plane per active speaker)
        let nplanes = self.ch_mask.count_ones() as usize;
        self.output = vec![0i32; nsamples * nplanes];
        self.output_plane_len = nsamples;

        // Handle change of filtering mode
        self.set_filter_mode(i32::from(x96) | dca::DCA_FILTER_MODE_FIXED);

        // Select filter
        let filter_coeff: &[i32] = if x96 {
            &FF_DCA_FIR_64BANDS_FIXED
        } else if self.filter_perfect {
            &FF_DCA_FIR_32BANDS_PERFECT_FIXED
        } else {
            &FF_DCA_FIR_32BANDS_NONPERFECT_FIXED
        };

        // Filter primary channels
        for ch in 0..self.nchannels {
            let spkr = self.map_prm_ch_to_spkr(ch)?;

            // Filter bank reconstruction (sub_qmf_fixed[x96_synth])
            let poff = match self.speaker_plane_off(spkr as usize, nsamples) {
                Some(p) => p,
                None => return Err("speaker plane unavailable"),
            };
            let hist = &mut self.dcadsp_data[ch];

            // Build per-subband rows as slices of the subband buffer.
            let lo_rows: Vec<&[i32]> = (0..dca::DCA_SUBBANDS)
                .map(|band| {
                    let base = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS;
                    &self.subband.data[base..base + self.npcmblocks]
                })
                .collect();
            let hi_rows: Option<Vec<&[i32]>> = if x96 && ch < x96_nchannels {
                Some(
                    (0..dca::DCA_SUBBANDS_X96)
                        .map(|band| {
                            let base = self.x96_subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS;
                            &self.x96_subband.data[base..base + self.npcmblocks]
                        })
                        .collect(),
                )
            } else {
                None
            };

            sub_qmf_fixed(
                &mut self.output[poff..poff + nsamples],
                &lo_rows,
                hi_rows.as_deref(),
                &mut hist.hist1_fix,
                &mut hist.offset,
                &mut hist.hist2_fix,
                filter_coeff,
                self.npcmblocks,
                x96,
            );
        }

        // Filter LFE channel
        if self.lfe_present != 0 {
            let nlfesamples = self.npcmblocks >> 1;

            // Check LFF
            if self.lfe_present == lfe_flag::FLAG_128 {
                return Err("fixed point mode doesn't support LFF=1");
            }

            // Offset intermediate buffer for X96
            let lfe_plane = self.speaker_plane_off(speaker::LFE1, nsamples).ok_or("no LFE plane")?;
            let samples_off = lfe_plane + if x96 { nsamples / 2 } else { 0 };

            // Interpolate LFE channel. The C passes `lfe_samples +
            // DCA_LFE_HISTORY` (start of new samples) and indexes [-k]
            // back into the 8-sample history region.
            let lfe_hist_end = dca::DCA_LFE_HISTORY + self.npcmblocks / 2;
            let lfe_in: Vec<i32> = self.lfe_samples[..lfe_hist_end].to_vec();
            let pcm_len = if x96 { nsamples / 2 } else { nsamples };
            dsp::lfe_fir_fixed_ext(
                &mut self.output[samples_off..samples_off + pcm_len],
                &lfe_in,
                &FF_DCA_LFE_FIR_64_FIXED,
                self.npcmblocks,
                dca::DCA_LFE_HISTORY,
            );

            if x96 {
                // Filter 96 kHz oversampled LFE PCM
                let src: Vec<i32> = self.output[samples_off..samples_off + nsamples / 2].to_vec();
                let mut hist = self.output_history_lfe_fixed;
                let full = self.speaker_plane_off(speaker::LFE1, nsamples).unwrap();
                dsp::lfe_x96_fixed(&mut self.output[full..full + nsamples], &src, &mut hist, nsamples / 2);
                self.output_history_lfe_fixed = hist;
            }

            // Update LFE history
            for n in (0..dca::DCA_LFE_HISTORY).rev() {
                self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
            }
        }

        Ok(())
    }

    /// Public wrapper for the fixed-path plane offset (used by xll.rs).
    pub fn speaker_plane_off_pub(&self, spkr: usize, nsamples: usize) -> usize {
        let idx = (0..spkr).filter(|&s| self.ch_mask & (1u32 << s) != 0).count();
        idx * nsamples
    }

    fn speaker_plane_off(&self, spkr: usize, nsamples: usize) -> Option<usize> {
        if self.ch_mask & (1u32 << spkr) == 0 {
            return None;
        }
        let idx = (0..spkr).filter(|&s| self.ch_mask & (1u32 << s) != 0).count();
        Some(idx * nsamples)
    }
}

/// `sub_qmf32/64_fixed_c` — fixed-point synthesis over `npcmblocks`
/// subband columns. `hi` borrows rows when the 64-band filter runs.
#[allow(clippy::too_many_arguments)]
fn sub_qmf_fixed(
    pcm: &mut [i32],
    lo: &[&[i32]],
    hi: Option<&[&[i32]]>,
    hist1: &mut [i32; 1024],
    offset: &mut i32,
    hist2: &mut [i32; 64],
    filter_coeff: &[i32],
    npcmblocks: usize,
    x96: bool,
) {
    let mut input = [0i32; 64];
    let mut pcm_pos = 0usize;

    for j in 0..npcmblocks {
        // Load in one sample from each subband
        if let Some(hi) = hi {
            // Full 64 subbands, first 32 are residual coded
            for i in 0..32 {
                input[i] = lo[i][j] + hi[i][j];
            }
            for i in 32..64 {
                input[i] = hi[i][j];
            }
        } else {
            for i in 0..32 {
                input[i] = lo[i][j];
            }
        }

        // One subband sample generates 32 or 64 interpolated ones
        if x96 {
            dsp::synth_filter_fixed_64(hist1, offset, hist2, filter_coeff, &mut pcm[pcm_pos..pcm_pos + 64], &input);
            pcm_pos += 64;
        } else {
            let mut input32 = [0i32; 32];
            input32.copy_from_slice(&input[..32]);
            let hist2_32: &mut [i32; 32] = (&mut hist2[..32]).try_into().unwrap();
            let hist1_1024: &mut [i32; 1024] = (&mut hist1[..1024]).try_into().unwrap();
            dsp::synth_filter_fixed(hist1_1024, offset, hist2_32, filter_coeff, &mut pcm[pcm_pos..pcm_pos + 32], &input32);
            pcm_pos += 32;
        }
    }
}

impl CoreDecoder {
    // ───────────── output assembly (fixed point) ─────────────

    /// `filter_frame_fixed` up to plane extraction: applies the embedded
    /// downmix undos, sum/diff decoding and stereo downmix, and writes the
    /// active output channels as S32 planes into `out` (one per remapped
    /// channel, in order), scaled like FFmpeg (`clip23(x) << 8`).
    pub fn filter_frame_fixed(&mut self, packet_xll: bool) -> Result<(u32, Vec<Vec<i32>>), &'static str> {
        // Handle downmixing to stereo request: none in this port —
        // request_mask = ch_mask (FFmpeg with request_channel_layout = 0).
        self.request_mask = self.ch_mask;

        // Don't filter twice when falling back from XLL
        if !packet_xll {
            self.core_filter_fixed(0)?;
        }

        let nsamples = self.npcmsamples;

        // Undo embedded XCH downmix
        if self.es_format != 0
            && (self.ext_audio_mask & crate::dca::css_mask::XCH) != 0
            && self.audio_mode >= amode::AMODE_2F2R
        {
            let ls = self.speaker_plane_off(speaker::LS, nsamples).unwrap();
            let rs = self.speaker_plane_off(speaker::RS, nsamples).unwrap();
            let cs = self.speaker_plane_off(speaker::CS, nsamples).unwrap();
            let (o, csrc) = (self.output.clone(), self.output.clone());
            let mut ls_v = o[ls..ls + nsamples].to_vec();
            let mut rs_v = o[rs..rs + nsamples].to_vec();
            dsp::dmix_sub_xch(&mut ls_v, &mut rs_v, &csrc[cs..cs + nsamples], nsamples);
            self.output[ls..ls + nsamples].copy_from_slice(&ls_v);
            self.output[rs..rs + nsamples].copy_from_slice(&rs_v);
        }

        // Undo embedded XXCH downmix
        if (self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::exss_mask::EXSS_XXCH)) != 0
            && self.xxch_dmix_embedded
        {
            let scale_inv = self.xxch_dmix_scale_inv;
            let xch_base = FF_DCA_CHANNELS[self.audio_mode] as usize;

            // Undo embedded core downmix pre-scaling
            for spkr in 0..self.xxch_mask_nbits {
                if self.xxch_core_mask & (1u32 << spkr) != 0 {
                    if let Some(off) = self.speaker_plane_off(spkr, nsamples) {
                        dsp::dmix_scale_inv(&mut self.output[off..off + nsamples], scale_inv, nsamples);
                    }
                }
            }

            // Undo downmix
            let mut coeff_idx = 0usize;
            for ch in xch_base..self.nchannels {
                let src_spkr = self.map_prm_ch_to_spkr(ch)?;
                for spkr in 0..self.xxch_mask_nbits {
                    if self.xxch_dmix_mask[ch - xch_base] & (1u32 << spkr) != 0 {
                        let coeff = mul16(self.xxch_dmix_coeff[coeff_idx], scale_inv);
                        coeff_idx += 1;
                        if coeff != 0 {
                            if let (Some(d), Some(s)) = (
                                self.speaker_plane_off(spkr, nsamples),
                                self.speaker_plane_off(src_spkr as usize, nsamples),
                            ) {
                                let src = self.output[s..s + nsamples].to_vec();
                                dsp::dmix_sub(&mut self.output[d..d + nsamples], &src, coeff, nsamples);
                            }
                        }
                    }
                }
            }
        }

        if (self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::css_mask::XCH | crate::dca::exss_mask::EXSS_XXCH)) == 0 {
            // Front sum/difference decoding
            if (self.sumdiff_front && self.audio_mode > amode::MONO)
                || self.audio_mode == amode::STEREO_SUMDIFF
            {
                let l = self.speaker_plane_off(speaker::L, nsamples).unwrap();
                let r = self.speaker_plane_off(speaker::R, nsamples).unwrap();
                let mut lv = self.output[l..l + nsamples].to_vec();
                let mut rv = self.output[r..r + nsamples].to_vec();
                dsp::butterflies_fixed(&mut lv, &mut rv, nsamples);
                self.output[l..l + nsamples].copy_from_slice(&lv);
                self.output[r..r + nsamples].copy_from_slice(&rv);
            }

            // Surround sum/difference decoding
            if self.sumdiff_surround && self.audio_mode >= amode::AMODE_2F2R {
                let l = self.speaker_plane_off(speaker::LS, nsamples).unwrap();
                let r = self.speaker_plane_off(speaker::RS, nsamples).unwrap();
                let mut lv = self.output[l..l + nsamples].to_vec();
                let mut rv = self.output[r..r + nsamples].to_vec();
                dsp::butterflies_fixed(&mut lv, &mut rv, nsamples);
                self.output[l..l + nsamples].copy_from_slice(&lv);
                self.output[r..r + nsamples].copy_from_slice(&rv);
            }
        }

        // Downmix primary channel set to stereo
        if self.request_mask != self.ch_mask {
            self.downmix_to_stereo_fixed(nsamples)?;
        }

        // Extract the remapped channel planes, scaled to 24-bit in 32.
        let remap: Vec<usize> = self.active_remap();
        let planes = remap
            .iter()
            .map(|&spkr| {
                let off = self.speaker_plane_off(spkr, nsamples).unwrap();
                self.output[off..off + nsamples]
                    .iter()
                    .map(|&v| clip23(v) * (1 << 8))
                    .collect::<Vec<i32>>()
            })
            .collect();

        Ok((self.output_rate, planes))
    }

    /// Channel list in output order (`ch_remap` of ff_dca_set_channel_layout
    /// with the default channel order).
    pub fn active_remap(&self) -> Vec<usize> {
        let dca_mask = self.request_mask;
        let dca2wav: &[usize; 28] = if dca_mask == spk::SEVEN_POINT0_WIDE || dca_mask == spk::SEVEN_POINT1_WIDE {
            &DCA2WAV_WIDE
        } else {
            &DCA2WAV_NORM
        };
        // WAV channel order (AV_CH bitmask bit positions).
        let wav_positions: [(usize, u8); 18] = WAV_CHANNELS;
        let mut wav_mask = 0u32;
        let mut wav_map = [0usize; 18];
        for dca_ch in 0..28 {
            if dca_mask & (1 << dca_ch) != 0 {
                let wav_ch = dca2wav[dca_ch];
                if wav_mask & (1 << wav_ch) == 0 {
                    wav_map[wav_ch] = dca_ch;
                    wav_mask |= 1 << wav_ch;
                }
            }
        }
        let mut remap = Vec::new();
        for &(bit, _pos) in &wav_positions {
            if wav_mask & (1 << bit) != 0 {
                remap.push(wav_map[bit]);
            }
        }
        remap
    }

    fn downmix_to_stereo_fixed(&mut self, nsamples: usize) -> Result<(), &'static str> {
        // ff_dca_downmix_to_stereo_fixed
        let coeff_l = self.prim_dmix_coeff;
        let ncoeff = self.ch_mask.count_ones() as usize;
        let coeff_r_off = ncoeff;
        let pos = usize::from(self.ch_mask & spk::C != 0);

        let l = self.speaker_plane_off(speaker::L, nsamples).ok_or("no L plane")?;
        let r = self.speaker_plane_off(speaker::R, nsamples).ok_or("no R plane")?;
        dsp::dmix_scale(&mut self.output[l..l + nsamples], coeff_l[pos], nsamples);
        dsp::dmix_scale(&mut self.output[r..r + nsamples], coeff_l[coeff_r_off + pos + 1], nsamples);

        let max_spkr = 31 - self.ch_mask.leading_zeros() as usize;
        let mut ci = 0usize;
        for spkr in 0..=max_spkr {
            if self.ch_mask & (1u32 << spkr) == 0 {
                continue;
            }
            let cl = coeff_l[ci];
            let cr = coeff_l[coeff_r_off + ci];
            if cl != 0 && spkr != speaker::L {
                if let Some(off) = self.speaker_plane_off(spkr, nsamples) {
                    let src = self.output[off..off + nsamples].to_vec();
                    dsp::dmix_add(&mut self.output[l..l + nsamples], &src, cl, nsamples);
                }
            }
            if cr != 0 && spkr != speaker::R {
                if let Some(off) = self.speaker_plane_off(spkr, nsamples) {
                    let src = self.output[off..off + nsamples].to_vec();
                    dsp::dmix_add(&mut self.output[r..r + nsamples], &src, cr, nsamples);
                }
            }
            ci += 1;
        }
        Ok(())
    }
}

/// `dca2wav_norm` (dcadec.c): DCA speaker → WAV channel bit position.
const DCA2WAV_NORM: [usize; 28] = [
    2, 0, 1, 9, 10, 3, 8, 4, 5, 9, 10, 6, 7, 12, 13, 14, 3, 6, 7, 11, 12, 14, 16, 15, 17, 8, 4, 5,
];

/// `dca2wav_wide` (dcadec.c).
const DCA2WAV_WIDE: [usize; 28] = [
    2, 0, 1, 4, 5, 3, 8, 4, 5, 9, 10, 6, 7, 12, 13, 14, 3, 6, 7, 11, 12, 14, 16, 15, 17, 8, 4, 5,
];

/// WAV channel bit positions in AV_CH order (index = AVChannel value).
const WAV_CHANNELS: [(usize, u8); 18] = [
    (0, 0),  // FL
    (1, 1),  // FR
    (2, 2),  // FC
    (3, 3),  // LFE
    (4, 4),  // BL
    (5, 5),  // BR
    (6, 6),  // FLC
    (7, 7),  // FRC
    (8, 8),  // BC
    (9, 9),  // SL
    (10, 10), // SR
    (11, 11), // TC
    (12, 12), // TFL
    (13, 13), // TFC
    (14, 14), // TFR
    (15, 15), // TBL
    (16, 16), // TBC
    (17, 17), // TBR
];

impl CoreDecoder {
    // ───────────── float output path ─────────────

    /// `filter_frame_float` → f32 planes. Mirrors FFmpeg's float path:
    /// synth filter with scale `1/(1 << (17 - x96_synth))`, LFE, embedded
    /// downmix undos, sum/diff and the optional stereo downmix.
    pub fn filter_frame_float(&mut self) -> Result<(u32, Vec<Vec<f32>>), &'static str> {
        self.request_mask = self.ch_mask;

        let x96_nchannels;
        let x96_synth;
        if (self.ext_audio_mask & (crate::dca::css_mask::X96 | crate::dca::exss_mask::EXSS_X96)) != 0 {
            x96_nchannels = self.x96_nchannels;
            x96_synth = 1;
        } else {
            x96_nchannels = 0;
            x96_synth = 0;
        }
        let x96 = x96_synth != 0;

        let nsamples = (self.npcmblocks * dca::DCA_PCMBLOCK_SAMPLES) << x96_synth;

        // Build output planes: one per active speaker in ch_mask order.
        let nplanes = self.ch_mask.count_ones() as usize;
        let mut out = vec![0f32; nsamples * nplanes];

        self.set_filter_mode(x96_synth);

        // Select filter
        let filter_coeff: &[f32] = if x96 {
            &FF_DCA_FIR_64BANDS
        } else if self.filter_perfect {
            &FF_DCA_FIR_32BANDS_PERFECT
        } else {
            &FF_DCA_FIR_32BANDS_NONPERFECT
        };
        let _ = filter_coeff;

        // Filter primary channels
        for ch in 0..self.nchannels {
            let spkr = self.map_prm_ch_to_spkr(ch)?;
                        // Filter bank reconstruction (sub_qmf_float[x96_synth])
            let lo_rows: Vec<Vec<f32>> = (0..dca::DCA_SUBBANDS)
                .map(|band| {
                    let base = self.subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS;
                    self.subband.data[base..base + self.npcmblocks].iter().map(|&v| v as f32).collect()
                })
                .collect();
            let hi_rows: Option<Vec<Vec<f32>>> = if x96 && ch < x96_nchannels {
                Some(
                    (0..dca::DCA_SUBBANDS_X96)
                        .map(|band| {
                            let base = self.x96_subband.row_base(ch, band) + crate::data::DCA_ADPCM_COEFFS;
                            self.x96_subband.data[base..base + self.npcmblocks].iter().map(|&v| v as f32).collect()
                        })
                        .collect(),
                )
            } else {
                None
            };

            let poff = self.plane_off(out.len(), nsamples, spkr as usize);
            let hist = &mut self.dcadsp_data[ch];
            sub_qmf_float(
                &mut out[poff..poff + nsamples],
                &lo_rows,
                hi_rows.as_ref(),
                &mut hist.hist1,
                &mut hist.offset,
                &mut hist.hist2,
                filter_coeff,
                self.npcmblocks,
                x96,
                1.0f32 / (1 << (17 - x96_synth)) as f32,
                if x96 { &self.imdct64 } else { &self.imdct32 },
            );
        }

        // Filter LFE channel
        if self.lfe_present != 0 {
            let dec_select = usize::from(self.lfe_present == lfe_flag::FLAG_128);
            let nlfesamples = self.npcmblocks >> (dec_select + 1);

            let lfe_plane = self.plane_off(out.len(), nsamples, speaker::LFE1);
            let samples_off = lfe_plane + if x96 { nsamples / 2 } else { 0 };

            // Select filter
            let coeff: &[f32] = if dec_select == 1 { &FF_DCA_LFE_FIR_128 } else { &FF_DCA_LFE_FIR_64 };

            // Interpolate LFE channel (history-extended input, as above).
            let lfe_hist_end = dca::DCA_LFE_HISTORY + self.npcmblocks / 2;
            let lfe_in: Vec<i32> = self.lfe_samples[..lfe_hist_end].to_vec();
            let pcm_len = if x96 { nsamples / 2 } else { nsamples };
            dsp::lfe_fir_float_ext(
                &mut out[samples_off..samples_off + pcm_len],
                &lfe_in,
                coeff,
                self.npcmblocks,
                dec_select,
                dca::DCA_LFE_HISTORY,
            );

            if x96 {
                // Filter 96 kHz oversampled LFE PCM
                let src: Vec<f32> = out[samples_off..samples_off + nsamples / 2].to_vec();
                let mut hist = self.output_history_lfe_float;
                dsp::lfe_x96_float(&mut out[lfe_plane..lfe_plane + nsamples], &src, &mut hist, nsamples / 2);
                self.output_history_lfe_float = hist;
            }

            // Update LFE history
            for n in (0..dca::DCA_LFE_HISTORY).rev() {
                self.lfe_samples[n] = self.lfe_samples[nlfesamples + n];
            }
        }

        // Undo embedded XCH downmix
        if self.es_format != 0
            && (self.ext_audio_mask & crate::dca::css_mask::XCH) != 0
            && self.audio_mode >= amode::AMODE_2F2R
        {
            let ls = self.plane_off(out.len(), nsamples, speaker::LS);
            let rs = self.plane_off(out.len(), nsamples, speaker::RS);
            let cs = self.plane_off(out.len(), nsamples, speaker::CS);
            let csrc = out[cs..cs + nsamples].to_vec();
            for i in 0..nsamples {
                let v = -csrc[i] * std::f32::consts::FRAC_1_SQRT_2;
                out[ls + i] += v;
                out[rs + i] += v;
            }
        }

        // Undo embedded XXCH downmix
        if (self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::exss_mask::EXSS_XXCH)) != 0
            && self.xxch_dmix_embedded
        {
            let scale_inv = self.xxch_dmix_scale_inv as f32 * (1.0 / (1 << 16) as f32);
            let xch_base = FF_DCA_CHANNELS[self.audio_mode] as usize;

            // Undo downmix
            let mut coeff_idx = 0usize;
            for ch in xch_base..self.nchannels {
                let src_spkr = self.map_prm_ch_to_spkr(ch)?;
                for spkr in 0..self.xxch_mask_nbits {
                    if self.xxch_dmix_mask[ch - xch_base] & (1u32 << spkr) != 0 {
                        let coeff = self.xxch_dmix_coeff[coeff_idx];
                        coeff_idx += 1;
                        if coeff != 0 {
                            let d = self.plane_off(out.len(), nsamples, spkr);
                            let s = self.plane_off(out.len(), nsamples, src_spkr as usize);
                            let src = out[s..s + nsamples].to_vec();
                            dsp::vector_fmac_scalar(&mut out[d..d + nsamples], &src, coeff as f32 * (-1.0f32 / (1 << 15) as f32), nsamples);
                        }
                    }
                }
            }

            // Undo embedded core downmix pre-scaling
            for spkr in 0..self.xxch_mask_nbits {
                if self.xxch_core_mask & (1u32 << spkr) != 0 {
                    let d = self.plane_off(out.len(), nsamples, spkr);
                    let src = out[d..d + nsamples].to_vec();
                    dsp::vector_fmul_scalar(&mut out[d..d + nsamples], &src, scale_inv, nsamples);
                }
            }
        }

        if (self.ext_audio_mask & (crate::dca::css_mask::XXCH | crate::dca::css_mask::XCH | crate::dca::exss_mask::EXSS_XXCH)) == 0 {
            // Front sum/difference decoding
            if (self.sumdiff_front && self.audio_mode > amode::MONO)
                || self.audio_mode == amode::STEREO_SUMDIFF
            {
                let l = self.plane_off(out.len(), nsamples, speaker::L);
                let r = self.plane_off(out.len(), nsamples, speaker::R);
                let mut lv = out[l..l + nsamples].to_vec();
                let mut rv = out[r..r + nsamples].to_vec();
                dsp::butterflies_float(&mut lv, &mut rv, nsamples);
                out[l..l + nsamples].copy_from_slice(&lv);
                out[r..r + nsamples].copy_from_slice(&rv);
            }

            // Surround sum/difference decoding
            if self.sumdiff_surround && self.audio_mode >= amode::AMODE_2F2R {
                let l = self.plane_off(out.len(), nsamples, speaker::LS);
                let r = self.plane_off(out.len(), nsamples, speaker::RS);
                let mut lv = out[l..l + nsamples].to_vec();
                let mut rv = out[r..r + nsamples].to_vec();
                dsp::butterflies_float(&mut lv, &mut rv, nsamples);
                out[l..l + nsamples].copy_from_slice(&lv);
                out[r..r + nsamples].copy_from_slice(&rv);
            }
        }

        // Downmix primary channel set to stereo
        if self.request_mask != self.ch_mask {
            self.downmix_to_stereo_float(&mut out, nsamples)?;
        }

        // Extract planes in output order (f32 conversion is exact — the
        // float path is already float).
        let remap = self.active_remap();
        let planes = remap
            .iter()
            .map(|&spkr| {
                let off = self.plane_off(out.len(), nsamples, spkr);
                out[off..off + nsamples].to_vec()
            })
            .collect();

        Ok((self.sample_rate << x96_synth, planes))
    }

    fn plane_off(&self, total: usize, nsamples: usize, spkr: usize) -> usize {
        debug_assert_eq!(total % nsamples, 0);
        let idx = (0..spkr).filter(|&s| self.ch_mask & (1u32 << s) != 0).count();
        idx * nsamples
    }

    fn downmix_to_stereo_float(&mut self, out: &mut [f32], nsamples: usize) -> Result<(), &'static str> {
        let coeff_l: Vec<i32> = self.prim_dmix_coeff.to_vec();
        let ncoeff = self.ch_mask.count_ones() as usize;
        let coeff_r_off = ncoeff;
        let pos = usize::from(self.ch_mask & spk::C != 0);
        let scale = 1.0f32 / (1 << 15) as f32;

        let l = self.plane_off(out.len(), nsamples, speaker::L);
        let r = self.plane_off(out.len(), nsamples, speaker::R);
        let lv = out[l..l + nsamples].to_vec();
        let rv = out[r..r + nsamples].to_vec();
        dsp::vector_fmul_scalar(&mut out[l..l + nsamples], &lv, coeff_l[pos] as f32 * scale, nsamples);
        dsp::vector_fmul_scalar(&mut out[r..r + nsamples], &rv, coeff_l[coeff_r_off + pos + 1] as f32 * scale, nsamples);

        let max_spkr = 31 - self.ch_mask.leading_zeros() as usize;
        let mut ci = 0usize;
        for spkr in 0..=max_spkr {
            if self.ch_mask & (1u32 << spkr) == 0 {
                continue;
            }
            let cl = coeff_l[ci];
            let cr = coeff_l[coeff_r_off + ci];
            if cl != 0 && spkr != speaker::L {
                let off = self.plane_off(out.len(), nsamples, spkr);
                let src = out[off..off + nsamples].to_vec();
                dsp::vector_fmac_scalar(&mut out[l..l + nsamples], &src, cl as f32 * scale, nsamples);
            }
            if cr != 0 && spkr != speaker::R {
                let off = self.plane_off(out.len(), nsamples, spkr);
                let src = out[off..off + nsamples].to_vec();
                dsp::vector_fmac_scalar(&mut out[r..r + nsamples], &src, cr as f32 * scale, nsamples);
            }
            ci += 1;
        }
        Ok(())
    }
}

/// `sub_qmf32/64_float_c`.
#[allow(clippy::too_many_arguments)]
fn sub_qmf_float(
    pcm: &mut [f32],
    lo: &[Vec<f32>],
    hi: Option<&Vec<Vec<f32>>>,
    hist1: &mut [f32; 1024],
    offset: &mut i32,
    hist2: &mut [f32; 64],
    filter_coeff: &[f32],
    npcmblocks: usize,
    x96: bool,
    scale: f32,
    imdct: &crate::avtx::MdctInv,
) {

    let mut input = [0f32; 64];
    let mut pcm_pos = 0usize;

    for j in 0..npcmblocks {
        if let Some(hi) = hi {
            // Full 64 subbands, first 32 are residual coded
            for i in 0..32 {
                input[i] = if (i as i64 - 1) & 2 != 0 { -(lo[i][j] + hi[i][j]) } else { lo[i][j] + hi[i][j] };
            }
            for i in 32..64 {
                input[i] = if (i as i64 - 1) & 2 != 0 { -hi[i][j] } else { hi[i][j] };
            }
        } else {
            for i in 0..32 {
                input[i] = if (i as i64 - 1) & 2 != 0 { -lo[i][j] } else { lo[i][j] };
            }
        }

        if x96 {
            let hist2_64: &mut [f32; 64] = (&mut hist2[..64]).try_into().unwrap();
            let hist1_1024: &mut [f32; 1024] = (&mut hist1[..1024]).try_into().unwrap();
            let mut out64 = [0f32; 64];
            dsp::synth_filter_float_64(&|i, o| imdct.run(i, o), hist1_1024, offset, hist2_64, filter_coeff, &mut out64, &input[..64], scale);
            pcm[pcm_pos..pcm_pos + 64].copy_from_slice(&out64);
            pcm_pos += 64;
        } else {
            let mut input32 = [0f32; 32];
            input32.copy_from_slice(&input[..32]);
            let mut out32 = [0f32; 32];
            let hist2_32: &mut [f32; 32] = (&mut hist2[..32]).try_into().unwrap();
            let hist1_1024: &mut [f32; 1024] = (&mut hist1[..1024]).try_into().unwrap();
            dsp::synth_filter_float(&|i, o| imdct.run(i, o), hist1_1024, offset, hist2_32, filter_coeff, &mut out32, &input32, scale);
            pcm[pcm_pos..pcm_pos + 32].copy_from_slice(&out32);
            pcm_pos += 32;
        }
    }
}

/// Generic half-IMDCT used by the float synth filter: FFmpeg's
/// `av_tx_init(AV_TX_FLOAT_MDCT, inv, len=N, scale=1.0)`. Matches
/// `ff_tx_mdct_naive_inv` with `s->len = N`: N inputs in, N outputs out
/// (`dst[i] = sum_d`, `dst[i + N/2] = -sum_u`, sums over all N inputs,
/// `phase = pi / (4N)`).
pub fn imdct_half_float<const N: usize>(input: &[f32], out: &mut [f32]) {
    let len = N; // s->len
    let half = len >> 1;
    let phase = std::f64::consts::PI / (4.0 * len as f64);
    debug_assert_eq!(input.len(), len);
    for i in 0..half {
        let mut sum_d = 0.0f64;
        let mut sum_u = 0.0f64;
        let i_d = phase * ((2 * len - 2 * i - 1) as f64);
        let i_u = phase * ((3 * len + 2 * i + 1) as f64);
        for (j, &val) in input.iter().take(len).enumerate() {
            // The naive inverse MDCT indexes the input as (2j+1).
            let a = (2 * j + 1) as f64;
            sum_d += (a * i_d).cos() * val as f64;
            sum_u += (a * i_u).cos() * val as f64;
        }
        out[i] = (sum_d * 1.0) as f32;
        out[i + half] = (-sum_u * 1.0) as f32;
    }
}

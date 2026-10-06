// Ported from FFmpeg libavcodec/dca_xll.c and dca_xll.h (commit 2da55bf),
// LGPL-2.1-or-later.

//! XLL (DTS-HD Master Audio) lossless decoder: channel set headers, NAVI
//! segment table, Rice/linear/hybrid residual decoding, adaptive and
//! fixed prediction, pairwise decorrelation, MSB/LSB assembly, frequency
//! band assembly and hierarchical downmix undoing. Lossless output is
//! bit-exact with FFmpeg's decoder by construction (pure integer path).

use crate::bitreader::BitReader;
use crate::data::{FF_DCA_DMIXTABLE_OFFSET, FF_DCA_DMIXTABLE_SIZE, FF_DCA_INV_DMIXTABLE_SIZE, FF_DCA_INV_DMIXTABLE, FF_DCA_DMIXTABLE, FF_DCA_SAMPLING_FREQS, FF_DCA_XLL_REFL_COEFF, FF_DCA_XLL_BAND_COEFF, FF_DCA_DMIX_PRIMARY_NCH};
use crate::dca::{self, dmix_type, mask as spk, speaker};
use crate::dsp;
use crate::exss::ExssAsset;
use crate::math::{clip23, mul15, mul16, norm16};

pub const DCA_XLL_CHSETS_MAX: usize = 3;
pub const DCA_XLL_CHANNELS_MAX: usize = 8;
pub const DCA_XLL_BANDS_MAX: usize = 2;
pub const DCA_XLL_ADAPT_PRED_ORDER_MAX: usize = 16;
pub const DCA_XLL_DECI_HISTORY_MAX: usize = 8;
pub const DCA_XLL_DMIX_SCALES_MAX: usize = (DCA_XLL_CHSETS_MAX - 1) * DCA_XLL_CHANNELS_MAX;
pub const DCA_XLL_DMIX_COEFFS_MAX: usize = DCA_XLL_DMIX_SCALES_MAX * DCA_XLL_CHANNELS_MAX;
pub const DCA_XLL_PBR_BUFFER_MAX: usize = 240 << 10;
pub const DCA_XLL_SAMPLE_BUFFERS_MAX: usize = 3;

/// `DCAXllBand` (sample buffers live in the chset's flat arena).
#[derive(Clone, Debug, Default)]
pub struct XllBand {
    pub decor_enabled: bool,
    pub orig_order: [usize; DCA_XLL_CHANNELS_MAX],
    pub decor_coeff: [i32; DCA_XLL_CHANNELS_MAX / 2],

    pub adapt_pred_order: [usize; DCA_XLL_CHANNELS_MAX],
    pub highest_pred_order: usize,
    pub fixed_pred_order: [usize; DCA_XLL_CHANNELS_MAX],
    pub adapt_refl_coeff: [[i32; DCA_XLL_ADAPT_PRED_ORDER_MAX]; DCA_XLL_CHANNELS_MAX],

    pub dmix_embedded: bool,

    pub lsb_section_size: usize,
    pub nscalablelsbs: [usize; DCA_XLL_CHANNELS_MAX],
    pub bit_width_adjust: [usize; DCA_XLL_CHANNELS_MAX],
}

/// `DCAXllChSet`.
impl Default for XllChSet {
    fn default() -> Self {
        Self {
            nchannels: 0,
            residual_encode: 0,
            pcm_bit_res: 0,
            storage_bit_res: 0,
            freq: 0,
            primary_chset: false,
            dmix_coeffs_present: false,
            dmix_embedded: false,
            dmix_type: 0,
            hier_chset: false,
            hier_ofs: 0,
            dmix_coeff: [0; DCA_XLL_DMIX_COEFFS_MAX],
            dmix_scale: [0; DCA_XLL_DMIX_SCALES_MAX],
            dmix_scale_inv: [0; DCA_XLL_DMIX_SCALES_MAX],
            ch_mask: 0,
            ch_remap: [0; DCA_XLL_CHANNELS_MAX],
            nfreqbands: 0,
            nabits: 0,
            bands: Default::default(),
            deci_history: [[0; DCA_XLL_DECI_HISTORY_MAX]; DCA_XLL_CHANNELS_MAX],
            seg_common_scratch: false,
            rice_code_flag: [false; DCA_XLL_CHANNELS_MAX],
            bitalloc_hybrid_linear: [0; DCA_XLL_CHANNELS_MAX],
            bitalloc_part_a: [0; DCA_XLL_CHANNELS_MAX],
            bitalloc_part_b: [0; DCA_XLL_CHANNELS_MAX],
            nsamples_part_a: [0; DCA_XLL_CHANNELS_MAX],
        }
    }
}

#[derive(Clone, Debug)]
pub struct XllChSet {
    // Channel set header
    pub nchannels: usize,
    pub residual_encode: u32,
    pub pcm_bit_res: usize,
    pub storage_bit_res: usize,
    pub freq: u32,

    pub primary_chset: bool,
    pub dmix_coeffs_present: bool,
    pub dmix_embedded: bool,
    pub dmix_type: usize,
    pub hier_chset: bool,
    pub hier_ofs: usize,
    pub dmix_coeff: [i32; DCA_XLL_DMIX_COEFFS_MAX],
    pub dmix_scale: [i32; DCA_XLL_DMIX_SCALES_MAX],
    pub dmix_scale_inv: [i32; DCA_XLL_DMIX_SCALES_MAX],
    pub ch_mask: u32,
    pub ch_remap: [usize; DCA_XLL_CHANNELS_MAX],

    pub nfreqbands: usize,
    pub nabits: usize,

    pub bands: [XllBand; DCA_XLL_BANDS_MAX],

    // Decimator history
    pub deci_history: [[i32; DCA_XLL_DECI_HISTORY_MAX]; DCA_XLL_CHANNELS_MAX],

    // Frequency band coding parameters (per-frame scratch, DCAXllChSet in C)
    pub seg_common_scratch: bool,
    pub rice_code_flag: [bool; DCA_XLL_CHANNELS_MAX],
    pub bitalloc_hybrid_linear: [usize; DCA_XLL_CHANNELS_MAX],
    pub bitalloc_part_a: [usize; DCA_XLL_CHANNELS_MAX],
    pub bitalloc_part_b: [usize; DCA_XLL_CHANNELS_MAX],
    pub nsamples_part_a: [usize; DCA_XLL_CHANNELS_MAX],
}

type XllResult<T> = Result<T, XllError>;

/// XLL errors that mirror FFmpeg's return codes and their handling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum XllError {
    /// `AVERROR(EAGAIN)` — no sync word; PBR smoothing may recover.
    Again,
    /// `AVERROR_INVALIDDATA`.
    InvalidData,
    /// `AVERROR_PATCHWELCOME`.
    Unsupported,
    /// `AVERROR(EINVAL)`.
    Invalid,
}

/// `DCAXllDecoder`.
pub struct XllDecoder {
    // Common header
    pub frame_size: usize,
    pub nchsets: usize,
    pub nframesegs: usize,
    pub nsegsamples_log2: usize,
    pub nsegsamples: usize,
    pub nframesamples_log2: usize,
    pub nframesamples: usize,
    pub seg_size_nbits: usize,
    pub band_crc_present: usize,
    pub scalable_lsbs: bool,
    pub ch_mask_nbits: usize,
    pub fixed_lsb_width: usize,

    pub chset: [XllChSet; DCA_XLL_CHSETS_MAX],

    pub navi: Vec<usize>,

    pub nfreqbands: usize,
    pub nchannels: usize,
    pub nreschsets: usize,
    pub nactivechsets: usize,

    pub hd_stream_id: i32,

    pub pbr_buffer: Vec<u8>,
    pub pbr_length: usize,
    pub pbr_delay: usize,

    pub x_syncword_present: bool,
    pub x_imax_syncword_present: bool,

    pub output_mask: u32,
    /// Flattened output planes per speaker (fixed-point residual output).
    pub output_samples: Vec<Vec<i32>>,
    pub frame_nsamples: usize,
}

// get_linear / get_rice helpers (dca_xll.c statics).

fn get_linear(gb: &mut BitReader, n: u32) -> i32 {
    let v = gb.get_bits_long(n);
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn get_rice_un(gb: &mut BitReader, k: u32) -> u32 {
    // get_unary(gb, 1, get_bits_left(gb)): stop bit is 1, so count the
    // leading ZERO bits before the terminating 1.
    let len = gb.bits_left().max(0) as u32;
    let mut v = 0u32;
    while v < len && gb.get_bits(1) != 1 {
        v += 1;
    }
    (v << k) | gb.get_bits_long(k)
}

fn get_rice(gb: &mut BitReader, k: u32) -> i32 {
    let v = get_rice_un(gb, k);
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn get_linear_array(gb: &mut BitReader, array: &mut [i32], n: u32) {
    if n == 0 {
        array.iter_mut().for_each(|v| *v = 0);
    } else {
        for v in array.iter_mut() {
            *v = get_linear(gb, n);
        }
    }
}

fn get_rice_array(gb: &mut BitReader, array: &mut [i32], k: u32) -> bool {
    let size = array.len();
    let mut i = 0usize;
    while i < size && gb.bits_left() > k as i32 {
        array[i] = get_rice(gb, k);
        i += 1;
    }
    i == size
}

impl XllChSet {
    pub fn rice_code_flag_store(&mut self, i: usize, v: bool) {
        self.rice_code_flag[i] = v;
    }
    pub fn rice_code_flag_get(&self, i: usize) -> bool {
        self.rice_code_flag[i]
    }
    pub fn bitalloc_hybrid_linear_store(&mut self, i: usize, v: usize) {
        self.bitalloc_hybrid_linear[i] = v;
    }
    pub fn bitalloc_hybrid_linear_get(&self, i: usize) -> usize {
        self.bitalloc_hybrid_linear[i]
    }
    pub fn bitalloc_part_a_store(&mut self, i: usize, v: usize) {
        self.bitalloc_part_a[i] = v;
    }
    pub fn bitalloc_part_a_get(&self, i: usize) -> usize {
        self.bitalloc_part_a[i]
    }
    pub fn bitalloc_part_b_store(&mut self, i: usize, v: usize) {
        self.bitalloc_part_b[i] = v;
    }
    pub fn bitalloc_part_b_get(&self, i: usize) -> usize {
        self.bitalloc_part_b[i]
    }
    pub fn nsamples_part_a_store(&mut self, i: usize, v: usize) {
        self.nsamples_part_a[i] = v;
    }
    pub fn nsamples_part_a_get(&self, i: usize) -> usize {
        self.nsamples_part_a[i]
    }
}

impl Default for XllDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl XllDecoder {
    pub fn new() -> Self {
        Self {
            frame_size: 0,
            nchsets: 0,
            nframesegs: 0,
            nsegsamples_log2: 0,
            nsegsamples: 0,
            nframesamples_log2: 0,
            nframesamples: 0,
            seg_size_nbits: 0,
            band_crc_present: 0,
            scalable_lsbs: false,
            ch_mask_nbits: 0,
            fixed_lsb_width: 0,
            chset: Default::default(),
            navi: Vec::new(),
            nfreqbands: 0,
            nchannels: 0,
            nreschsets: 0,
            nactivechsets: 0,
            hd_stream_id: -1,
            pbr_buffer: Vec::new(),
            pbr_length: 0,
            pbr_delay: 0,
            x_syncword_present: false,
            x_imax_syncword_present: false,
            output_mask: 0,
            output_samples: vec![Vec::new(); dca::DCA_SPEAKER_COUNT],
            frame_nsamples: 0,
        }
    }

    // ───────────── channel set header ─────────────

    fn parse_dmix_coeffs(&mut self, gb: &mut BitReader, c: usize) -> XllResult<()> {
        let chset = &self.chset[c];
        // Size of downmix coefficient matrix
        let m = if chset.primary_chset {
            FF_DCA_DMIX_PRIMARY_NCH[chset.dmix_type] as usize
        } else {
            chset.hier_ofs
        };

        let mut coeff_idx = 0usize;
        for i in 0..m {
            #[allow(unused_assignments)]
            let (mut scale, mut scale_inv) = (0i32, 0i32);

            // Downmix scale (only for non-primary channel sets)
            if !self.chset[c].primary_chset {
                let code = gb.get_bits(9) as usize;
                let sign = (code >> 8) as i32 - 1;
                let index = (code & 0xff).wrapping_sub(FF_DCA_DMIXTABLE_OFFSET);
                if index >= FF_DCA_INV_DMIXTABLE_SIZE {
                    return Err(XllError::InvalidData);
                }
                scale = i32::from(FF_DCA_DMIXTABLE[index + FF_DCA_DMIXTABLE_OFFSET]);
                scale_inv = FF_DCA_INV_DMIXTABLE[index] as i32;
                let chset = &mut self.chset[c];
                chset.dmix_scale[i] = ((scale as u32) ^ (sign as u32)).wrapping_sub(sign as u32) as i32;
                chset.dmix_scale_inv[i] = ((scale_inv as u32) ^ (sign as u32)).wrapping_sub(sign as u32) as i32;
            }

            // Downmix coefficients
            for _ in 0..self.chset[c].nchannels {
                let code = gb.get_bits(9) as usize;
                let sign = (code >> 8) as i32 - 1;
                let index = code & 0xff;
                if index >= FF_DCA_DMIXTABLE_SIZE {
                    return Err(XllError::InvalidData);
                }
                let mut coeff = i32::from(FF_DCA_DMIXTABLE[index]);
                if !self.chset[c].primary_chset {
                    // Multiply by |InvDmixScale| to get |UndoDmixScale|
                    coeff = mul16(scale_inv, coeff);
                }
                if coeff_idx < DCA_XLL_DMIX_COEFFS_MAX {
                    self.chset[c].dmix_coeff[coeff_idx] =
                        ((coeff as u32) ^ (sign as u32)).wrapping_sub(sign as u32) as i32;
                }
                coeff_idx += 1;
            }
        }

        Ok(())
    }

    fn chs_parse_header(&mut self, gb: &mut BitReader, c: usize, asset: &ExssAsset) -> XllResult<()> {
        let header_pos = gb.bits_read();

        // Size of channel set sub-header
        let header_size = gb.get_bits(10) as usize + 1;

        // Number of channels in the channel set
        self.chset[c].nchannels = gb.get_bits(4) as usize + 1;
        if self.chset[c].nchannels > DCA_XLL_CHANNELS_MAX {
            return Err(XllError::Unsupported);
        }

        // Residual type
        self.chset[c].residual_encode = gb.get_bits(self.chset[c].nchannels as u32);

        // PCM bit resolution
        self.chset[c].pcm_bit_res = gb.get_bits(5) as usize + 1;

        // Storage unit width
        self.chset[c].storage_bit_res = gb.get_bits(5) as usize + 1;
        if self.chset[c].storage_bit_res != 16 && self.chset[c].storage_bit_res != 20 && self.chset[c].storage_bit_res != 24 {
            return Err(XllError::Unsupported);
        }

        if self.chset[c].pcm_bit_res > self.chset[c].storage_bit_res {
            return Err(XllError::InvalidData);
        }

        // Original sampling frequency
        self.chset[c].freq = FF_DCA_SAMPLING_FREQS[gb.get_bits(4) as usize];
        if self.chset[c].freq > 192000 {
            return Err(XllError::Unsupported);
        }

        // Sampling frequency modifier
        if gb.get_bits(2) != 0 {
            return Err(XllError::Unsupported);
        }

        // Which replacement set this channel set is member of
        if gb.get_bits(2) != 0 {
            return Err(XllError::Unsupported);
        }

        if asset.one_to_one_map_ch_to_spkr {
            // Primary channel set flag
            self.chset[c].primary_chset = gb.get_bits(1) != 0;
            if self.chset[c].primary_chset != (c == 0) {
                return Err(XllError::InvalidData);
            }

            // Downmix coefficients present in stream
            self.chset[c].dmix_coeffs_present = gb.get_bits(1) != 0;

            // Downmix already performed by encoder
            self.chset[c].dmix_embedded = self.chset[c].dmix_coeffs_present && gb.get_bits(1) != 0;

            // Downmix type
            if self.chset[c].dmix_coeffs_present && self.chset[c].primary_chset {
                self.chset[c].dmix_type = gb.get_bits(3) as usize;
                if self.chset[c].dmix_type >= dmix_type::COUNT {
                    return Err(XllError::InvalidData);
                }
            }

            // Whether the channel set is part of a hierarchy
            self.chset[c].hier_chset = gb.get_bits(1) != 0;
            if !self.chset[c].hier_chset && self.nchsets != 1 {
                return Err(XllError::Unsupported);
            }

            // Downmix coefficients
            if self.chset[c].dmix_coeffs_present {
                self.parse_dmix_coeffs(gb, c)?;
            }

            // Channel mask enabled
            if gb.get_bits(1) == 0 {
                return Err(XllError::Unsupported);
            }

            // Channel mask for set
            self.chset[c].ch_mask = gb.get_bits_long(self.ch_mask_nbits as u32);
            if self.chset[c].ch_mask.count_ones() as usize != self.chset[c].nchannels {
                return Err(XllError::InvalidData);
            }

            // Build the channel to speaker map
            let (nbits, chmask) = (self.ch_mask_nbits, self.chset[c].ch_mask);
            let mut j = 0usize;
            for i in 0..nbits {
                if chmask & (1u32 << i) != 0 {
                    self.chset[c].ch_remap[j] = i;
                    j += 1;
                }
            }
        } else {
            // Mapping coeffs present flag
            if self.chset[c].nchannels != 2 || self.nchsets != 1 || gb.get_bits(1) != 0 {
                return Err(XllError::Unsupported);
            }

            // Setup for LtRt decoding
            self.chset[c].primary_chset = true;
            self.chset[c].dmix_coeffs_present = false;
            self.chset[c].dmix_embedded = false;
            self.chset[c].hier_chset = false;
            self.chset[c].ch_mask = spk::STEREO;
            self.chset[c].ch_remap[0] = speaker::L;
            self.chset[c].ch_remap[1] = speaker::R;
        }

        if self.chset[c].freq > 96000 {
            // Extra frequency bands flag
            if gb.get_bits(1) != 0 {
                return Err(XllError::Unsupported);
            }
            self.chset[c].nfreqbands = 2;
        } else {
            self.chset[c].nfreqbands = 1;
        }

        // Set the sampling frequency to that of the first frequency band.
        self.chset[c].freq >>= (self.chset[c].nfreqbands - 1) as u32;

        // Verify that all channel sets have the same audio characteristics
        if c != 0 {
            let (pc, cc) = (&self.chset[0], &self.chset[c]);
            if cc.nfreqbands != pc.nfreqbands || cc.freq != pc.freq
                || cc.pcm_bit_res != pc.pcm_bit_res
                || cc.storage_bit_res != pc.storage_bit_res
            {
                return Err(XllError::Unsupported);
            }
        }

        // Determine number of bits to read bit allocation coding parameter
        self.chset[c].nabits = if self.chset[c].storage_bit_res > 16 {
            5
        } else if self.chset[c].storage_bit_res > 8 {
            4
        } else {
            3
        };

        // Account for embedded downmix and decimator saturation
        if (self.nchsets > 1 || self.chset[c].nfreqbands > 1) && self.chset[c].nabits < 5 {
            self.chset[c].nabits += 1;
        }

        let nchannels = self.chset[c].nchannels;
        let nabits = self.chset[c].nabits;
        let nsegsamples = self.nsegsamples;
        let scalable_lsbs = self.scalable_lsbs;
        let band_crc_present = self.band_crc_present;
        let frame_size = self.frame_size;
        let seg_size_nbits = self.seg_size_nbits;

        for band in 0..self.chset[c].nfreqbands {
            // Pairwise channel decorrelation
            self.chset[c].bands[band].decor_enabled = gb.get_bits(1) != 0;
            if self.chset[c].bands[band].decor_enabled && nchannels > 1 {
                let ch_nbits = 32 - (nchannels as u32 - 1).leading_zeros(); // av_ceil_log2

                // Original channel order
                for i in 0..nchannels {
                    self.chset[c].bands[band].orig_order[i] = gb.get_bits(ch_nbits) as usize;
                    if self.chset[c].bands[band].orig_order[i] >= nchannels {
                        return Err(XllError::InvalidData);
                    }
                }

                // Pairwise channel coefficients
                for i in 0..nchannels / 2 {
                    self.chset[c].bands[band].decor_coeff[i] =
                        if gb.get_bits(1) != 0 { get_linear(gb, 7) } else { 0 };
                }
            } else {
                for i in 0..nchannels {
                    self.chset[c].bands[band].orig_order[i] = i;
                }
                for i in 0..nchannels / 2 {
                    self.chset[c].bands[band].decor_coeff[i] = 0;
                }
            }

            // Adaptive predictor order
            self.chset[c].bands[band].highest_pred_order = 0;
            for i in 0..nchannels {
                self.chset[c].bands[band].adapt_pred_order[i] = gb.get_bits(4) as usize;
                if self.chset[c].bands[band].adapt_pred_order[i] > self.chset[c].bands[band].highest_pred_order {
                    self.chset[c].bands[band].highest_pred_order = self.chset[c].bands[band].adapt_pred_order[i];
                }
            }
            if self.chset[c].bands[band].highest_pred_order > nsegsamples {
                return Err(XllError::InvalidData);
            }

            // Fixed predictor order
            for i in 0..nchannels {
                self.chset[c].bands[band].fixed_pred_order[i] = if self.chset[c].bands[band].adapt_pred_order[i] != 0 {
                    0
                } else {
                    gb.get_bits(2) as usize
                };
            }

            // Adaptive predictor quantized reflection coefficients
            for i in 0..nchannels {
                for j in 0..self.chset[c].bands[band].adapt_pred_order[i] {
                    let k = get_linear(gb, 8);
                    if k == -128 {
                        return Err(XllError::InvalidData);
                    }
                    let rc = if k < 0 {
                        -(FF_DCA_XLL_REFL_COEFF[(-k) as usize] as i32)
                    } else {
                        FF_DCA_XLL_REFL_COEFF[k as usize] as i32
                    };
                    self.chset[c].bands[band].adapt_refl_coeff[i][j] = rc;
                }
            }

            // Downmix performed by encoder in extension frequency band
            self.chset[c].bands[band].dmix_embedded =
                self.chset[c].dmix_embedded && (band == 0 || gb.get_bits(1) != 0);

            // MSB/LSB split flag in extension frequency band
            if (band == 0 && scalable_lsbs) || (band != 0 && gb.get_bits(1) != 0) {
                // Size of LSB section in any segment
                self.chset[c].bands[band].lsb_section_size = gb.get_bits_long(seg_size_nbits as u32) as usize;
                if self.chset[c].bands[band].lsb_section_size > frame_size {
                    return Err(XllError::InvalidData);
                }

                // Account for optional CRC bytes after LSB section
                if self.chset[c].bands[band].lsb_section_size != 0
                    && (band_crc_present > 2 || (band == 0 && band_crc_present > 1))
                {
                    self.chset[c].bands[band].lsb_section_size += 2;
                }

                // Number of bits to represent the samples in LSB part
                for i in 0..nchannels {
                    self.chset[c].bands[band].nscalablelsbs[i] = gb.get_bits(4) as usize;
                    if self.chset[c].bands[band].nscalablelsbs[i] != 0 && self.chset[c].bands[band].lsb_section_size == 0 {
                        return Err(XllError::InvalidData);
                    }
                }
            } else {
                self.chset[c].bands[band].lsb_section_size = 0;
                for i in 0..nchannels {
                    self.chset[c].bands[band].nscalablelsbs[i] = 0;
                }
            }

            // Scalable resolution flag in extension frequency band
            if (band == 0 && scalable_lsbs) || (band != 0 && gb.get_bits(1) != 0) {
                // Number of bits discarded by authoring
                for i in 0..nchannels {
                    self.chset[c].bands[band].bit_width_adjust[i] = gb.get_bits(4) as usize;
                }
            } else {
                for i in 0..nchannels {
                    self.chset[c].bands[band].bit_width_adjust[i] = 0;
                }
            }
        }
        let _ = nabits;

        // Reserved, byte align, CRC16 of channel set sub-header
        if !gb.seek_bits(header_pos + header_size * 8) {
            return Err(XllError::InvalidData);
        }

        Ok(())
    }
}

/// Per-chset MSB/LSB sample buffers, indexed like FFmpeg's
/// `msb_sample_buffer[ch]` / `lsb_sample_buffer[ch]` rows.
#[derive(Default)]
pub struct ChsBuffers {
    /// Band 0..nfreqbands, per channel: `nframesamples + ndecisamples`.
    msb: Vec<Vec<i32>>,
    /// Only for bands with `lsb_section_size`; `nframesamples` per channel.
    lsb: Vec<Option<Vec<i32>>>,
}

impl XllDecoder {
    // ───────────── band data ─────────────

    fn chs_parse_band_data(&mut self, gb: &mut BitReader, c: usize, band: usize, seg: usize, band_data_end: usize, bufs: &mut ChsBuffers) -> XllResult<()> {
        let seg_common;
        // Start unpacking MSB portion of the segment
        if seg == 0 || gb.get_bits(1) == 0 {
            // Unpack segment type
            self.chset[c].bands[band].decor_enabled = self.chset[c].bands[band].decor_enabled;
            seg_common = gb.get_bits(1) != 0;
            self.seg_common_store(c, seg_common);

            // Determine number of coding parameters encoded in segment
            let k = if seg_common { 1 } else { self.chset[c].nchannels };

            // Unpack Rice coding parameters
            for i in 0..k {
                // Unpack Rice coding flag: 0 - linear code, 1 - Rice code
                self.chset[c].rice_code_flag_store(i, gb.get_bits(1) != 0);
                // Unpack Hybrid Rice coding flag
                if !seg_common && self.chset[c].rice_code_flag_get(i) && gb.get_bits(1) != 0 {
                    // Unpack binary code length for isolated samples
                    self.chset[c].bitalloc_hybrid_linear_store(i, gb.get_bits(self.chset[c].nabits as u32) as usize + 1);
                } else {
                    self.chset[c].bitalloc_hybrid_linear_store(i, 0);
                }
            }

            // Unpack coding parameters
            for i in 0..k {
                if seg == 0 {
                    // Unpack coding parameter for part A of segment 0
                    let mut part_a = gb.get_bits(self.chset[c].nabits as u32) as usize;

                    // Adjust for the linear code
                    if !self.chset[c].rice_code_flag_get(i) && part_a != 0 {
                        part_a += 1;
                    }

                    self.chset[c].bitalloc_part_a_store(i, part_a);
                    let ns = if !seg_common {
                        self.chset[c].bands[band].adapt_pred_order[i]
                    } else {
                        self.chset[c].bands[band].highest_pred_order
                    };
                    self.chset[c].nsamples_part_a_store(i, ns);
                } else {
                    self.chset[c].bitalloc_part_a_store(i, 0);
                    self.chset[c].nsamples_part_a_store(i, 0);
                }

                // Unpack coding parameter for part B of segment
                let mut part_b = gb.get_bits(self.chset[c].nabits as u32) as usize;

                // Adjust for the linear code
                if !self.chset[c].rice_code_flag_get(i) && part_b != 0 {
                    part_b += 1;
                }
                self.chset[c].bitalloc_part_b_store(i, part_b);
            }
        } else {
            seg_common = self.seg_common_get(c);
        }

        // Unpack entropy codes
        for i in 0..self.chset[c].nchannels {
            // Select index of coding parameters
            let k = if seg_common { 0 } else { i };

            // Slice the segment into parts A and B
            let nsamples_part_a = self.chset[c].nsamples_part_a_get(k);
            let part_a_len = nsamples_part_a;
            let part_b_len = self.nsegsamples - nsamples_part_a;

            if gb.bits_left() < 0 {
                return Err(XllError::InvalidData);
            }

            let nseg = self.nsegsamples;
            let msb = &mut bufs.msb[i];
            let msb_off = seg * nseg;

            if !self.chset[c].rice_code_flag_get(k) {
                // Linear codes
                let ba = self.chset[c].bitalloc_part_a_get(k) as u32;
                get_linear_array(gb, &mut msb[msb_off..msb_off + part_a_len], ba);
                let bb = self.chset[c].bitalloc_part_b_get(k) as u32;
                get_linear_array(gb, &mut msb[msb_off + part_a_len..msb_off + part_a_len + part_b_len], bb);
            } else {
                // Rice codes: part A
                let baa = self.chset[c].bitalloc_part_a_get(k) as u32;
                if !get_rice_array(gb, &mut msb[msb_off..msb_off + part_a_len], baa) {
                    return Err(XllError::InvalidData);
                }

                if self.chset[c].bitalloc_hybrid_linear_get(k) != 0 {
                    // Hybrid Rice codes
                    // Unpack the number of isolated samples
                    let nisosamples = gb.get_bits(self.nsegsamples_log2 as u32) as usize;

                    // Set all locations to 0
                    for v in msb[msb_off + part_a_len..msb_off + part_a_len + part_b_len].iter_mut() {
                        *v = 0;
                    }

                    // Extract the locations of isolated samples, flagged -1
                    for _ in 0..nisosamples {
                        let loc = gb.get_bits(self.nsegsamples_log2 as u32) as usize;
                        if loc >= part_b_len {
                            return Err(XllError::InvalidData);
                        }
                        msb[msb_off + part_a_len + loc] = -1;
                    }

                    // Unpack all residuals of part B
                    let bw = self.chset[c].bitalloc_hybrid_linear_get(k) as u32;
                    let bkb = self.chset[c].bitalloc_part_b_get(k) as u32;
                    for j in 0..part_b_len {
                        msb[msb_off + part_a_len + j] = if msb[msb_off + part_a_len + j] != 0 {
                            get_linear(gb, bw)
                        } else {
                            get_rice(gb, bkb)
                        };
                    }
                } else {
                    // Rice codes: part B
                    let bbb = self.chset[c].bitalloc_part_b_get(k) as u32;
                    if !get_rice_array(gb, &mut msb[msb_off + part_a_len..msb_off + part_a_len + part_b_len], bbb) {
                        return Err(XllError::InvalidData);
                    }
                }
            }
        }

        // Unpack decimator history for frequency band 1
        if seg == 0 && band == 1 {
            let nbits = gb.get_bits(5) + 1;
            for i in 0..self.chset[c].nchannels {
                for j in 1..DCA_XLL_DECI_HISTORY_MAX {
                    self.chset[c].deci_history[i][j] = gb.get_sbits_long(nbits);
                }
            }
        }

        // Start unpacking LSB portion of the segment
        if self.chset[c].bands[band].lsb_section_size != 0 {
            // Skip to the start of LSB portion
            let Some(lsb_start) =
                band_data_end.checked_sub(self.chset[c].bands[band].lsb_section_size * 8)
            else {
                return Err(XllError::InvalidData);
            };
            if !gb.seek_bits(lsb_start) {
                return Err(XllError::InvalidData);
            }

            // Unpack all LSB parts of residuals of this segment
            for i in 0..self.chset[c].nchannels {
                if self.chset[c].bands[band].nscalablelsbs[i] != 0 {
                    if let Some(lsb) = bufs.lsb[i].as_mut() {
                        let nseg = self.nsegsamples;
                        let off = seg * nseg;
                        for v in lsb[off..off + nseg].iter_mut() {
                            *v = gb.get_bits(self.chset[c].bands[band].nscalablelsbs[i] as u32) as i32;
                        }
                    }
                }
            }
        }

        // Skip to the end of band data
        if !gb.seek_bits(band_data_end) {
            return Err(XllError::InvalidData);
        }

        Ok(())
    }

    // Segment/coding parameter storage on the chset (FFmpeg keeps these on
    // DCAXllChSet as flat fields, not per-band).
    fn seg_common_store(&mut self, c: usize, v: bool) {
        self.chset[c].seg_common_scratch = v;
    }
    fn seg_common_get(&self, c: usize) -> bool {
        self.chset[c].seg_common_scratch
    }
}

impl XllDecoder {
    // ───────────── prediction / decorrelation / assembly ─────────────

    fn chs_filter_band_data(&mut self, c: usize, band: usize, bufs: &mut ChsBuffers) {
        let nsamples = self.nframesamples;

        // Inverse adaptive or fixed prediction
        for i in 0..self.chset[c].nchannels {
            let order = self.chset[c].bands[band].adapt_pred_order[i];
            if order > 0 {
                // Conversion from reflection coefficients to direct form
                let mut coeff = [0i32; DCA_XLL_ADAPT_PRED_ORDER_MAX];
                for j in 0..order {
                    let rc = self.chset[c].bands[band].adapt_refl_coeff[i][j];
                    for k in 0..(j + 1) / 2 {
                        let tmp1 = coeff[k];
                        let tmp2 = coeff[j - k - 1];
                        coeff[k] = tmp1 + mul16(rc, tmp2);
                        coeff[j - k - 1] = tmp2 + mul16(rc, tmp1);
                    }
                    coeff[j] = rc;
                }
                // Inverse adaptive prediction
                for j in 0..nsamples - order {
                    let mut err: i64 = 0;
                    for k in 0..order {
                        err += i64::from(bufs.msb[i][j + k]) * i64::from(coeff[order - k - 1]);
                    }
                    bufs.msb[i][j + order] = bufs.msb[i][j + order].wrapping_sub(clip23(norm16(err)));
                }
            } else {
                // Inverse fixed coefficient prediction
                for _j in 0..self.chset[c].bands[band].fixed_pred_order[i] {
                    for k in 1..nsamples {
                        bufs.msb[i][k] = bufs.msb[i][k].wrapping_add(bufs.msb[i][k - 1]);
                    }
                }
            }
        }

        // Inverse pairwise channel decorrelation
        if self.chset[c].bands[band].decor_enabled {
            for i in 0..self.chset[c].nchannels / 2 {
                let coeff = self.chset[c].bands[band].decor_coeff[i];
                if coeff != 0 {
                    // decor(dst, src, coeff): dst[i] += (src[i]*coeff + 4) >> 3
                    let src: Vec<i32> = bufs.msb[i * 2].to_vec();
                    dsp::decor(&mut bufs.msb[i * 2 + 1], &src, coeff, nsamples);
                }
            }

            // Reorder channel buffers to the original order (pointer swap
            // in C; here swap the Vecs).
            let mut tmp: Vec<Vec<i32>> = (0..self.chset[c].nchannels).map(|i| std::mem::take(&mut bufs.msb[i])).collect();
            for i in 0..self.chset[c].nchannels {
                bufs.msb[self.chset[c].bands[band].orig_order[i]] = std::mem::take(&mut tmp[i]);
            }
        }

        // Map output channel buffers for frequency band 0
        if self.chset[c].nfreqbands == 1 {
            for i in 0..self.chset[c].nchannels {
                let spkr = self.chset[c].ch_remap[i];
                self.output_samples[spkr] = bufs.msb[i].clone();
            }
        }
    }

    fn chs_get_lsb_width(&self, c: usize, band: usize, ch: usize) -> usize {
        let adj = self.chset[c].bands[band].bit_width_adjust[ch];
        let mut shift = self.chset[c].bands[band].nscalablelsbs[ch];

        if self.fixed_lsb_width != 0 {
            shift = self.fixed_lsb_width;
        } else if shift != 0 && adj != 0 {
            shift += adj - 1;
        } else {
            shift += adj;
        }

        shift
    }

    fn chs_assemble_msbs_lsbs(&mut self, c: usize, band: usize, bufs: &mut ChsBuffers) {
        let nsamples = self.nframesamples;

        for ch in 0..self.chset[c].nchannels {
            let shift = self.chs_get_lsb_width(c, band, ch);
            if shift != 0 {
                if self.chset[c].bands[band].nscalablelsbs[ch] != 0 {
                    let adj = self.chset[c].bands[band].bit_width_adjust[ch];
                    if let Some(lsb) = bufs.lsb[ch].as_mut() {
                        for n in 0..nsamples {
                            bufs.msb[ch][n] = (bufs.msb[ch][n] as i64 * (1i64 << shift) + ((lsb[n] as i64) << adj)) as i32;
                        }
                    }
                } else {
                    for n in 0..nsamples {
                        bufs.msb[ch][n] = (bufs.msb[ch][n] as i64 * (1i64 << shift)) as i32;
                    }
                }
            }
        }
    }
}

impl XllDecoder {
    fn chs_assemble_freq_bands(&mut self, c: usize, bufs: &mut ChsBuffers) {
        let nsamples = self.nframesamples;

        // Assemble frequency bands 0 and 1
        let nch = self.chset[c].nchannels;
        for ch in 0..nch {
            // Build history-extended windows: band0 gets the decimator
            // history copied in front (8 samples); bufs.msb is band-major
            // (band 0 channels, then band 1 channels).
            let mut src0 = vec![0i32; DCA_XLL_DECI_HISTORY_MAX + nsamples];
            let mut src1 = vec![0i32; DCA_XLL_DECI_HISTORY_MAX + nsamples];
            src0[DCA_XLL_DECI_HISTORY_MAX..].copy_from_slice(&bufs.msb[ch][..nsamples]);
            // Copy decimator history
            for (d, &s) in self.chset[c].deci_history[ch].iter().enumerate() {
                src0[d] = s;
            }
            src1[DCA_XLL_DECI_HISTORY_MAX..].copy_from_slice(&bufs.msb[nch + ch][..nsamples]);

            let mut dst = vec![0i32; 2 * nsamples];
            dsp::assemble_freq_bands(&mut dst, &mut src0, &mut src1, &FF_DCA_XLL_BAND_COEFF, nsamples);

            // Remap output channel buffer to assembly buffer
            let spkr = self.chset[c].ch_remap[ch];
            self.output_samples[spkr] = dst;
        }
    }

    // ───────────── frame-level parse ─────────────

    fn parse_common_header(&mut self, gb: &mut BitReader) -> XllResult<()> {
        // XLL extension sync word
        if gb.get_bits_long(32) != dca::DCA_SYNCWORD_XLL {
            return Err(XllError::Again);
        }

        // Version number
        let stream_ver = gb.get_bits(4) + 1;
        if stream_ver > 1 {
            return Err(XllError::Unsupported);
        }

        // Lossless frame header length
        let header_size = gb.get_bits(8) as usize + 1;

        // Number of bits used to read frame size
        let frame_size_nbits = gb.get_bits(5) + 1;

        // Number of bytes in a lossless frame
        let frame_size = gb.get_bits_long(frame_size_nbits) as usize;
        if frame_size >= DCA_XLL_PBR_BUFFER_MAX {
            return Err(XllError::InvalidData);
        }
        self.frame_size = frame_size + 1;

        // Number of channels sets per frame
        self.nchsets = gb.get_bits(4) as usize + 1;
        if self.nchsets > DCA_XLL_CHSETS_MAX {
            return Err(XllError::Unsupported);
        }

        // Number of segments per frame
        let nframesegs_log2 = gb.get_bits(4) as usize;
        self.nframesegs = 1 << nframesegs_log2;
        if self.nframesegs > 1024 {
            return Err(XllError::InvalidData);
        }

        // Samples in segment per one frequency band for the first channel set
        self.nsegsamples_log2 = gb.get_bits(4) as usize;
        if self.nsegsamples_log2 == 0 {
            return Err(XllError::InvalidData);
        }
        self.nsegsamples = 1 << self.nsegsamples_log2;
        if self.nsegsamples > 512 {
            return Err(XllError::InvalidData);
        }

        // Samples in frame per one frequency band for the first channel set
        self.nframesamples_log2 = self.nsegsamples_log2 + nframesegs_log2;
        self.nframesamples = 1 << self.nframesamples_log2;
        if self.nframesamples > 65536 {
            return Err(XllError::InvalidData);
        }

        // Number of bits used to read segment size
        self.seg_size_nbits = gb.get_bits(5) as usize + 1;

        // Presence of CRC16 within each frequency band
        self.band_crc_present = gb.get_bits(2) as usize;

        // MSB/LSB split flag
        self.scalable_lsbs = gb.get_bits(1) != 0;

        // Channel position mask
        self.ch_mask_nbits = gb.get_bits(5) as usize + 1;

        // Fixed LSB width
        self.fixed_lsb_width = if self.scalable_lsbs { gb.get_bits(4) as usize } else { 0 };

        // Reserved, byte align, header CRC16 protection
        if !gb.seek_bits(header_size * 8) {
            return Err(XllError::InvalidData);
        }

        Ok(())
    }

    fn is_hier_dmix_chset(c: &XllChSet) -> bool {
        !c.primary_chset && c.dmix_embedded && c.hier_chset
    }

    fn find_next_hier_dmix_chset(&self, c: usize) -> Option<usize> {
        if !self.chset[c].hier_chset {
            return None;
        }
        (c + 1..self.nchsets).find(|&i| Self::is_hier_dmix_chset(&self.chset[i]))
    }

    fn prescale_down_mix(&mut self, c: usize, o: usize) {
        let mut coeff_idx = 0usize;
        let hier_ofs = self.chset[c].hier_ofs;
        for i in 0..hier_ofs {
            let scale = self.chset[o].dmix_scale[i];
            let scale_inv = self.chset[o].dmix_scale_inv[i];
            let o_scale: Vec<i32> = self.chset[o].dmix_scale.to_vec();
            let (ds, dsi, dmc, nch) = {
                let cs = &mut self.chset[c];
                cs.dmix_scale[i] = mul15(cs.dmix_scale[i], scale);
                cs.dmix_scale_inv[i] = mul16(cs.dmix_scale_inv[i], scale_inv);
                (cs.dmix_scale.as_mut_ptr(), cs.dmix_scale_inv.as_mut_ptr(), cs.dmix_coeff.as_mut_ptr(), cs.nchannels)
            };
            unsafe {
                *ds.add(i) = mul15(*ds.add(i), scale);
                *dsi.add(i) = mul16(*dsi.add(i), scale_inv);
                for j in 0..nch {
                    let coeff = mul16(*dmc.add(coeff_idx), scale_inv);
                    *dmc.add(coeff_idx) = mul15(coeff, o_scale[hier_ofs + j]);
                    coeff_idx += 1;
                }
            }
        }
    }

    fn parse_sub_headers(&mut self, gb: &mut BitReader, asset: &ExssAsset) -> XllResult<()> {
        // Parse channel set headers
        self.nfreqbands = 0;
        self.nchannels = 0;
        self.nreschsets = 0;
        for i in 0..self.nchsets {
            self.chset[i].hier_ofs = self.nchannels;
            self.chs_parse_header(gb, i, asset)?;
            if self.chset[i].nfreqbands > self.nfreqbands {
                self.nfreqbands = self.chset[i].nfreqbands;
            }
            if self.chset[i].hier_chset {
                self.nchannels += self.chset[i].nchannels;
            }
            if self.chset[i].residual_encode != (1u32 << self.chset[i].nchannels) - 1 {
                self.nreschsets += 1;
            }
        }

        // Pre-scale downmixing coefficients for all non-primary channel sets
        for i in (1..self.nchsets).rev() {
            if Self::is_hier_dmix_chset(&self.chset[i]) {
                if let Some(o) = self.find_next_hier_dmix_chset(i) {
                    self.prescale_down_mix(i, o);
                }
            }
        }

        // Determine number of active channel sets to decode
        // (request_channel_layout: none in this port — full decode).
        self.nactivechsets = self.nchsets;

        Ok(())
    }
}

impl XllDecoder {
    fn parse_navi_table(&mut self, gb: &mut BitReader) -> XllResult<()> {
        // Determine size of NAVI table
        let navi_nb = self.nfreqbands * self.nframesegs * self.nchsets;
        if navi_nb > 1024 {
            return Err(XllError::InvalidData);
        }

        // Parse NAVI
        self.navi.clear();
        for _band in 0..self.nfreqbands {
            for _seg in 0..self.nframesegs {
                for chs in 0..self.nchsets {
                    let mut size = 0usize;
                    if self.chset[chs].nfreqbands > 0 {
                        let v = gb.get_bits_long(self.seg_size_nbits as u32) as usize;
                        if v >= self.frame_size {
                            return Err(XllError::InvalidData);
                        }
                        size = v + 1;
                    }
                    self.navi.push(size);
                }
            }
        }

        // Byte align, CRC16
        let pad = (8 - gb.bits_read() % 8) % 8;
        gb.skip(pad as u32);
        gb.skip(16);

        // CRC unchecked here in the sense that FFmpeg verifies; our
        // check_crc is advisory (see crc16.rs), matching default options.
        Ok(())
    }

    fn chs_alloc_band_data(&self, c: usize) -> ChsBuffers {
        let ndecisamples = if self.chset[c].nfreqbands > 1 { DCA_XLL_DECI_HISTORY_MAX } else { 0 };
        let nchsamples = self.nframesamples + ndecisamples;

        let msb: Vec<Vec<i32>> = (0..self.chset[c].nfreqbands * self.chset[c].nchannels)
            .map(|_| vec![0i32; nchsamples])
            .collect();

        let lsb: Vec<Option<Vec<i32>>> = (0..self.chset[c].nfreqbands)
            .flat_map(|band| {
                (0..self.chset[c].nchannels).map(move | _ | {
                    if self.chset[c].bands[band].lsb_section_size != 0 {
                        Some(vec![0i32; self.nframesamples])
                    } else {
                        None
                    }
                })
            })
            .collect();

        // msb and lsb are band-major; chs_parse_band_data indexes msb[i]
        // with i = band * nchannels + ch.
        ChsBuffers { msb, lsb }
    }

    fn parse_band_data(&mut self, gb: &mut BitReader) -> XllResult<Vec<ChsBuffers>> {
        let mut all_bufs: Vec<ChsBuffers> = Vec::new();

        for chs in 0..self.nactivechsets {
            all_bufs.push(self.chs_alloc_band_data(chs));
        }

        let mut navi_pos = gb.bits_read();
        let mut navi_idx = 0usize;
        for band in 0..self.nfreqbands {
            for seg in 0..self.nframesegs {
                for chs in 0..self.nchsets {
                    if self.chset[chs].nfreqbands > band {
                        navi_pos += self.navi[navi_idx] * 8;
                        if navi_pos > gb.len_bits() {
                            return Err(XllError::InvalidData);
                        }
                        if chs < self.nactivechsets {
                            let mut bufs = std::mem::take(&mut all_bufs[chs]);
                            let r = self.chs_parse_band_data(gb, chs, band, seg, navi_pos, &mut bufs);
                            all_bufs[chs] = bufs;
                            if r.is_err() {
                                // chs_clear_band_data
                                self.chs_clear_band_data(chs, band, seg as i32, &mut all_bufs[chs]);
                            }
                        }
                        let cur = gb.bits_read();
                        if (navi_pos as i64 - cur as i64) > 0 {
                            gb.skip((navi_pos - cur) as u32);
                        } else if navi_pos < cur {
                            // FFmpeg's skip_bits_long accepts negative; our
                            // reader clamps at 0 — mirror going back.
                            gb.skip_long((navi_pos as i64 - cur as i64) as i32);
                        }
                    }
                    navi_idx += 1;
                }
            }
        }

        Ok(all_bufs)
    }

    /// `chs_clear_band_data`: zero the band segment (seg >= 0) or the whole
    /// band (seg = -1 → whole band here).
    fn chs_clear_band_data(&mut self, c: usize, band: usize, seg: i32, bufs: &mut ChsBuffers) {
        let (offset, nsamples) = if seg < 0 {
            (0, self.nframesamples)
        } else {
            let s = seg as usize;
            (s * self.nsegsamples, self.nsegsamples)
        };

        let band_off = band * self.chset[c].nchannels;
        for i in 0..self.chset[c].nchannels {
            for v in bufs.msb[band_off + i][offset..offset + nsamples].iter_mut() {
                *v = 0;
            }
            if self.chset[c].bands[band].lsb_section_size != 0 {
                if let Some(lsb) = bufs.lsb[band_off + i].as_mut() {
                    for v in lsb[offset..offset + nsamples].iter_mut() {
                        *v = 0;
                    }
                }
            }
        }

        if seg <= 0 && band != 0 {
            self.chset[c].deci_history = [[0; DCA_XLL_DECI_HISTORY_MAX]; DCA_XLL_CHANNELS_MAX];
        }
    }

    fn parse_frame(&mut self, gb: &mut BitReader, asset: &ExssAsset) -> XllResult<(Vec<ChsBuffers>, usize)> {
        self.parse_common_header(gb)?;
        self.parse_sub_headers(gb, asset)?;
        self.parse_navi_table(gb)?;
        let bufs = self.parse_band_data(gb)?;

        if self.frame_size * 8 > ((gb.bits_read() + 31) & !31) {
            // Align to dword
            let pad = (32 - gb.bits_read() % 32) % 32;
            gb.skip(pad as u32);

            let extradata_syncword = gb.show_bits(32);

            if extradata_syncword == dca::DCA_SYNCWORD_XLL_X {
                self.x_syncword_present = true;
            } else if (extradata_syncword >> 1) == (dca::DCA_SYNCWORD_XLL_X_IMAX >> 1) {
                self.x_imax_syncword_present = true;
            }
        }

        if !gb.seek_bits(self.frame_size * 8) {
            return Err(XllError::InvalidData);
        }
        Ok((bufs, self.frame_size))
    }
}

impl XllDecoder {
    // ───────────── PBR smoothing ─────────────

    fn clear_pbr(&mut self) {
        self.pbr_length = 0;
        self.pbr_delay = 0;
    }

    fn copy_to_pbr(&mut self, data: &[u8], delay: usize) -> XllResult<()> {
        if data.len() > DCA_XLL_PBR_BUFFER_MAX {
            return Err(XllError::Invalid); // ENOSPC → treated as hard error
        }
        self.pbr_buffer.clear();
        self.pbr_buffer.extend_from_slice(data);
        self.pbr_length = data.len();
        self.pbr_delay = delay;
        Ok(())
    }

    fn parse_frame_no_pbr(&mut self, data: &[u8], asset: &ExssAsset) -> XllResult<(Vec<ChsBuffers>, usize)> {
        let mut gb = BitReader::new(data);
        let res = self.parse_frame(&mut gb, asset);

        // If XLL packet data didn't start with a sync word, we must have
        // jumped right into the middle of a PBR smoothing period.
        let again = matches!(res, Err(XllError::Again));
        if again && asset.xll_sync_present && asset.xll_sync_offset < data.len() {
            // Skip to the next sync word in this packet
            let data = &data[asset.xll_sync_offset..];

            // If decoding delay is set, put the frame into PBR buffer
            if asset.xll_delay_nframes > 0 {
                self.copy_to_pbr(data, asset.xll_delay_nframes as usize)?;
                return Err(XllError::Again);
            }

            // No decoding delay, just parse the frame in place
            let mut gb = BitReader::new(data);
            return self.parse_frame(&mut gb, asset);
        }

        res?;
        if self.frame_size > data.len() {
            return Err(XllError::Invalid);
        }

        // If the XLL decoder didn't consume full packet, start PBR smoothing
        if self.frame_size < data.len() {
            self.copy_to_pbr(&data[self.frame_size..], 0)?;
        }

        // Note: the buffers from the first successful parse are returned by
        // parse_frame's Ok value; we re-parse only on the Again path.
        let mut gb = BitReader::new(data);
        self.parse_frame(&mut gb, asset)
    }

    fn parse_frame_pbr(&mut self, data: &[u8], asset: &ExssAsset) -> XllResult<(Vec<ChsBuffers>, usize)> {
        if data.len() > DCA_XLL_PBR_BUFFER_MAX - self.pbr_length {
            self.clear_pbr();
            return Err(XllError::Invalid);
        }

        self.pbr_buffer.extend_from_slice(data);
        self.pbr_length += data.len();

        // Respect decoding delay after synchronization error
        if self.pbr_delay > 0 {
            self.pbr_delay -= 1;
            if self.pbr_delay > 0 {
                return Err(XllError::Again);
            }
        }

        let pbr_copy = self.pbr_buffer[..self.pbr_length].to_vec();
        let mut gb = BitReader::new(&pbr_copy);
        let (bufs, frame_size) = match self.parse_frame(&mut gb, asset) {
            Ok(v) => v,
            Err(e) => {
                // For now, throw out all PBR state on failure.
                self.clear_pbr();
                return Err(e);
            }
        };

        if frame_size > self.pbr_length {
            self.clear_pbr();
            return Err(XllError::Invalid);
        }

        if frame_size == self.pbr_length {
            // End of PBR smoothing period
            self.clear_pbr();
        } else {
            self.pbr_length -= frame_size;
            self.pbr_buffer.drain(..frame_size);
        }

        Ok((bufs, frame_size))
    }

    /// `ff_dca_xll_parse`. Returns decoded band buffers on success.
    pub fn xll_parse(&mut self, data: &[u8], asset: &ExssAsset) -> XllResult<Vec<ChsBuffers>> {
        if self.hd_stream_id != asset.hd_stream_id {
            self.clear_pbr();
            self.hd_stream_id = asset.hd_stream_id;
        }

        let sub = &data[asset.xll_offset.min(data.len())..(asset.xll_offset + asset.xll_size).min(data.len())];

        if self.pbr_length != 0 {
            self.parse_frame_pbr(sub, asset).map(|(b, _)| b)
        } else {
            self.parse_frame_no_pbr(sub, asset).map(|(b, _)| b)
        }
    }

    /// `ff_dca_xll_flush`.
    pub fn xll_flush(&mut self) {
        self.clear_pbr();
    }
}

impl XllDecoder {
    // ───────────── filter_frame ─────────────

    fn undo_down_mix(&mut self, o: usize, band: usize, all_bufs: &mut [ChsBuffers]) {
        let mut nchannels = 0usize;
        let mut coeff_idx = 0usize;

        for i in 0..self.nactivechsets {
            if !self.chset[i].hier_chset {
                continue;
            }

            for j in 0..self.chset[i].nchannels {
                for k in 0..self.chset[o].nchannels {
                    let coeff = self.chset[o].dmix_coeff[coeff_idx];
                    coeff_idx += 1;
                    if coeff != 0 {
                        let src: Vec<i32> = all_bufs[o].msb[band * self.chset[o].nchannels + k].clone();
                        dsp::dmix_sub(
                            &mut all_bufs[i].msb[band * self.chset[i].nchannels + j],
                            &src,
                            coeff,
                            self.nframesamples,
                        );
                        if band != 0 {
                            let src_d: Vec<i32> = self.chset[o].deci_history[k].to_vec();
                            let mut dst_d = self.chset[i].deci_history[j];
                            dsp::dmix_sub(&mut dst_d, &src_d, coeff, DCA_XLL_DECI_HISTORY_MAX);
                            self.chset[i].deci_history[j] = dst_d;
                        }
                    }
                }
            }

            nchannels += self.chset[i].nchannels;
            if nchannels >= self.chset[o].hier_ofs {
                break;
            }
        }
    }

    fn scale_down_mix(&mut self, o: usize, band: usize, all_bufs: &mut [ChsBuffers]) {
        let mut nchannels = 0usize;

        for i in 0..self.nactivechsets {
            if !self.chset[i].hier_chset {
                continue;
            }

            for j in 0..self.chset[i].nchannels {
                let scale = self.chset[o].dmix_scale[nchannels];
                nchannels += 1;
                if scale != 1 << 15 {
                    dsp::dmix_scale(
                        &mut all_bufs[i].msb[band * self.chset[i].nchannels + j],
                        scale,
                        self.nframesamples,
                    );
                    if band != 0 {
                        let mut dst_d = self.chset[i].deci_history[j];
                        dsp::dmix_scale(&mut dst_d, scale, DCA_XLL_DECI_HISTORY_MAX);
                        self.chset[i].deci_history[j] = dst_d;
                    }
                }
            }

            if nchannels >= self.chset[o].hier_ofs {
                break;
            }
        }
    }

    /// Combine the lossy core output with residual-encoded channels.
    /// `core_planes` maps core speaker → plane.
    fn combine_residual_frame(
        &mut self,
        c: usize,
        all_bufs: &mut [ChsBuffers],
        core: &crate::core::CoreDecoder,
        next_hier: Option<usize>,
    ) -> XllResult<()> {
        // Verify that core is compatible
        if core.packet & crate::dca::decoder_packets::DCA_PACKET_CORE == 0 {
            return Err(XllError::Invalid);
        }

        if self.chset[c].freq != core.output_rate {
            return Err(XllError::InvalidData);
        }

        let nsamples = self.nframesamples;
        if nsamples != core.npcmsamples {
            return Err(XllError::InvalidData);
        }

        // Reduce core bit width and combine with residual
        for ch in 0..self.chset[c].nchannels {
            if self.chset[c].residual_encode & (1u32 << ch) != 0 {
                continue;
            }

            // Map this channel to core speaker
            let spkr = core.map_spkr(self.chset[c].ch_remap[ch]);
            if spkr < 0 {
                return Err(XllError::InvalidData);
            }

            // Account for LSB width
            let shift = 24 - self.chset[c].pcm_bit_res + self.chs_get_lsb_width(c, 0, ch);
            if shift > 24 {
                return Err(XllError::InvalidData);
            }
            let shift = shift as u32;
            let round: i64 = if shift > 0 { 1i64 << (shift - 1) } else { 0 };

            if core.ch_mask & (1u32 << spkr as usize) == 0 || core.output.is_empty() {
                return Err(XllError::InvalidData);
            }
            let plane_off = (0..spkr as usize)
                .filter(|&s| core.ch_mask & (1u32 << s) != 0)
                .count()
                * nsamples;
            if plane_off + nsamples > core.output.len() {
                return Err(XllError::InvalidData);
            }
            let src = &core.output[plane_off..plane_off + nsamples];
            if all_bufs[c].msb.len() <= ch || all_bufs[c].msb[ch].len() < nsamples {
                return Err(XllError::InvalidData);
            }
            let dst = &mut all_bufs[c].msb[ch];

            if let Some(o) = next_hier {
                // Undo embedded core downmix pre-scaling
                let scale_inv = self.chset[o].dmix_scale_inv[self.chset[c].hier_ofs + ch];
                for n in 0..nsamples {
                    dst[n] = dst[n].wrapping_add(clip23(((i64::from(mul16(src[n], scale_inv)) + round) >> shift) as i32));
                }
            } else {
                // No downmix scaling
                for n in 0..nsamples {
                    dst[n] = dst[n].wrapping_add(((i64::from(src[n]) + round) >> shift) as i32);
                }
            }
        }

        Ok(())
    }

    /// `ff_dca_xll_filter_frame`. Returns (sample_rate, S32 planes in
    /// output order) — 24-bit samples scaled `<< 8` like FFmpeg, or 16-bit
    /// samples when the primary channel set stores 16 bits.
    pub fn xll_filter_frame(
        &mut self,
        all_bufs: &mut Vec<ChsBuffers>,
        core: &mut crate::core::CoreDecoder,
    ) -> XllResult<(u32, Vec<Vec<i32>>)> {
        let asset_hd = self.hd_stream_id; // for reference only

        // Force lossy downmixed output during recovery
        if core.packet & crate::dca::decoder_packets::DCA_PACKET_RECOVERY != 0 {
            for i in 0..self.nchsets {
                if i < self.nactivechsets {
                    self.force_lossy_output(i, all_bufs, core);
                }
                if !self.chset[i].primary_chset {
                    self.chset[i].dmix_embedded = false;
                }
            }
            self.scalable_lsbs = false;
            self.fixed_lsb_width = 0;
        }

        // Filter frequency bands for active channel sets
        self.output_mask = 0;
        for i in 0..self.nactivechsets {
            self.chs_filter_band_data(i, 0, &mut all_bufs[i]);

            let full_residual = self.chset[i].residual_encode == (1u32 << self.chset[i].nchannels) - 1;
            if !full_residual {
                let next_hier = self.find_next_hier_dmix_chset(i);
                self.combine_residual_frame(i, all_bufs, core, next_hier)?;
            }

            if self.scalable_lsbs {
                self.chs_assemble_msbs_lsbs(i, 0, &mut all_bufs[i]);
            }

            if self.chset[i].nfreqbands > 1 {
                self.chs_filter_band_data(i, 1, &mut all_bufs[i]);
                self.chs_assemble_msbs_lsbs(i, 1, &mut all_bufs[i]);
            }

            // Refresh the mapped output buffers: combine/assembly above
            // mutated the band buffers after chs_filter_band_data's clone.
            if self.chset[i].nfreqbands == 1 {
                for j in 0..self.chset[i].nchannels {
                    let spkr = self.chset[i].ch_remap[j];
                    self.output_samples[spkr] = all_bufs[i].msb[j].clone();
                }
            }

            self.output_mask |= self.chset[i].ch_mask;
        }

        // Undo hierarchical downmix and/or apply scaling
        for i in 1..self.nchsets {
            if !Self::is_hier_dmix_chset(&self.chset[i]) {
                continue;
            }

            if i >= self.nactivechsets {
                for j in 0..self.chset[i].nfreqbands {
                    if self.chset[i].bands[j].dmix_embedded {
                        self.scale_down_mix(i, j, all_bufs);
                    }
                }
                break;
            }

            for j in 0..self.chset[i].nfreqbands {
                if self.chset[i].bands[j].dmix_embedded {
                    self.undo_down_mix(i, j, all_bufs);
                }
            }
        }

        // Assemble frequency bands for active channel sets
        if self.nfreqbands > 1 {
            for i in 0..self.nactivechsets {
                self.chs_assemble_freq_bands(i, &mut all_bufs[i]);
            }
        }

        let p_freq = self.chset[0].freq;
        let p_storage = self.chset[0].storage_bit_res;
        let p_pcm_res = self.chset[0].pcm_bit_res;
        let p_dmix_embedded = self.chset[0].dmix_embedded;
        let p_dmix_type = self.chset[0].dmix_type;
        let p_dmix_coeff: Vec<i32> = self.chset[0].dmix_coeff.to_vec();
        let p_ch_mask = self.chset[0].ch_mask;

        // Handle downmixing to stereo request: not supported in this port
        // (request_channel_layout == 0), so request_mask == output_mask.
        let request_mask = self.output_mask;
        let remap = self.active_remap(request_mask);

        let sample_rate = p_freq << (self.nfreqbands - 1) as u32;

        let shift: u32 = match p_storage {
            16 => 16 - p_pcm_res as u32,
            20 | 24 => 24 - p_pcm_res as u32,
            _ => return Err(XllError::Invalid),
        };

        let nsamples = self.nframesamples << (self.nfreqbands - 1) as u32;
        self.frame_nsamples = nsamples as usize;

        // Downmix primary channel set to stereo
        if request_mask != self.output_mask {
            self.downmix_to_stereo_fixed(&p_dmix_coeff, nsamples as usize);
        }

        let mut planes = Vec::new();
        for &spkr in &remap {
            let samples = self.output_samples[spkr].clone();
            let plane: Vec<i32> = if p_storage == 16 {
                samples.iter().map(|&v| (v.wrapping_mul(1i32 << shift)).clamp(i16::MIN as i32, i16::MAX as i32)).collect()
            } else {
                samples.iter().map(|&v| clip23(v.wrapping_mul(1i32 << shift)) * (1 << 8)).collect()
            };
            planes.push(plane);
        }
        let _ = (p_dmix_embedded, p_dmix_type, p_dmix_coeff, p_ch_mask, asset_hd);

        Ok((sample_rate, planes))
    }

    /// `force_lossy_output`: clear band data and drop residual flag for
    /// channels with no lossy core counterpart.
    fn force_lossy_output(&mut self, c: usize, all_bufs: &mut [ChsBuffers], core: &crate::core::CoreDecoder) {
        for band in 0..self.chset[c].nfreqbands {
            self.chs_clear_band_data(c, band, -1, &mut all_bufs[c]);
        }

        for ch in 0..self.chset[c].nchannels {
            if self.chset[c].residual_encode & (1u32 << ch) == 0 {
                continue;
            }
            if core.map_spkr(self.chset[c].ch_remap[ch]) < 0 {
                continue;
            }
            self.chset[c].residual_encode &= !(1u32 << ch);
        }
    }

    /// Output channel order (default WAV order) for a DCA mask.
    fn active_remap(&self, dca_mask: u32) -> Vec<usize> {
        let dca2wav: &[usize; 28] = if dca_mask == spk::SEVEN_POINT0_WIDE || dca_mask == spk::SEVEN_POINT1_WIDE {
            &DCA2WAV_WIDE_XLL
        } else {
            &DCA2WAV_NORM_XLL
        };
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
        for bit in 0..18 {
            if wav_mask & (1 << bit) != 0 {
                remap.push(wav_map[bit]);
            }
        }
        remap
    }

    fn downmix_to_stereo_fixed(&mut self, coeff: &[i32], nsamples: usize) {
        // ff_dca_downmix_to_stereo_fixed over output_samples planes
        let ncoeff = self.output_mask.count_ones() as usize;
        let coeff_r_off = ncoeff;
        let pos = usize::from(self.output_mask & spk::C != 0);

        let l = self.output_mask_plane(speaker::L);
        let r = self.output_mask_plane(speaker::R);
        if let (Some(l), Some(r)) = (l, r) {
            dsp::dmix_scale(&mut self.output_samples[l], coeff[pos], nsamples);
            dsp::dmix_scale(&mut self.output_samples[r], coeff[coeff_r_off + pos + 1], nsamples);

            let max_spkr = 31 - self.output_mask.leading_zeros() as usize;
            let mut ci = 0usize;
            for spkr in 0..=max_spkr {
                if self.output_mask & (1u32 << spkr) == 0 {
                    continue;
                }
                let cl = coeff[ci];
                let cr = coeff[coeff_r_off + ci];
                if cl != 0 && spkr != speaker::L {
                    if let Some(off) = self.output_mask_plane(spkr) {
                        let src = self.output_samples[off].clone();
                        dsp::dmix_add(&mut self.output_samples[l], &src, cl, nsamples);
                    }
                }
                if cr != 0 && spkr != speaker::R {
                    if let Some(off) = self.output_mask_plane(spkr) {
                        let src = self.output_samples[off].clone();
                        dsp::dmix_add(&mut self.output_samples[r], &src, cr, nsamples);
                    }
                }
                ci += 1;
            }
        }
    }

    fn output_mask_plane(&self, spkr: usize) -> Option<usize> {
        if self.output_mask & (1u32 << spkr) == 0 {
            return None;
        }
        Some((0..spkr).filter(|&s| self.output_mask & (1u32 << s) != 0).count())
    }
}

const DCA2WAV_NORM_XLL: [usize; 28] = [
    2, 0, 1, 9, 10, 3, 8, 4, 5, 9, 10, 6, 7, 12, 13, 14, 3, 6, 7, 11, 12, 14, 16, 15, 17, 8, 4, 5,
];
const DCA2WAV_WIDE_XLL: [usize; 28] = [
    2, 0, 1, 4, 5, 3, 8, 4, 5, 9, 10, 6, 7, 12, 13, 14, 3, 6, 7, 11, 12, 14, 16, 15, 17, 8, 4, 5,
];

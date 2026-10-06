// Ported from FFmpeg libavcodec/dca_lbr.c and dca_lbr.h (commit 2da55bf),
// LGPL-2.1-or-later.

//! LBR (DTS Express) decoder: chunked lossy substream with tonal
//! synthesis, grid scale factors, time-sample entropy coding, LPC
//! prediction and the hybrid filterbank/MDCT reconstruction.

use crate::bitreader_le::LeBitReader;
use crate::data::{
    FF_DCA_AVG_G3_FREQS, FF_DCA_SAMPLING_FREQS, FF_DCA_BANK_COEFF, FF_DCA_CORR_CF, FF_DCA_FREQ_RANGES, FF_DCA_FREQ_TO_SB,
    FF_DCA_FST_AMP, FF_DCA_GRID_1_TO_SCF, FF_DCA_GRID_1_WEIGHTS, FF_DCA_GRID_2_TO_SCF,
    FF_DCA_LFE_DELTA_INDEX_16, FF_DCA_LFE_DELTA_INDEX_24, FF_DCA_LFE_IIR, FF_DCA_LFE_STEP_SIZE_16,
    FF_DCA_LFE_STEP_SIZE_24, FF_DCA_LONG_WINDOW, FF_DCA_PH0_SHIFT, FF_DCA_QUANT_AMP, FF_DCA_RSD_LEVEL_16,
    FF_DCA_RSD_LEVEL_2A, FF_DCA_RSD_LEVEL_2B, FF_DCA_RSD_LEVEL_3, FF_DCA_RSD_LEVEL_5, FF_DCA_RSD_LEVEL_8,
    FF_DCA_RSD_PACK_3_IN_7, FF_DCA_RSD_PACK_5_IN_8, FF_DCA_SCF_TO_GRID_1, FF_DCA_SCF_TO_GRID_2,
    FF_DCA_SB_REORDER, FF_DCA_ST_COEFF, FF_DCA_SYNTH_ENV,
};
use crate::dca::{self, speaker, speaker_pair as spair};
use crate::dsp;
use crate::exss::ExssAsset;
use crate::huffman::{DCA_DAMP_VLC_BITS, DCA_AVG_G3_VLC_BITS, DCA_DPH_VLC_BITS, DCA_FST_RSD_VLC_BITS, DCA_GRID_VLC_BITS, DCA_RSD_AMP_VLC_BITS, DCA_RSD_APPRX_VLC_BITS, DCA_RSD_VLC_BITS, DCA_ST_GRID_VLC_BITS, DCA_TNL_GRP_VLC_BITS, DCA_TNL_SCF_VLC_BITS};
use crate::vlc::{self as vlc_mod, Vlc};

pub const DCA_LBR_CHANNELS: usize = 6;
pub const DCA_LBR_CHANNELS_TOTAL: usize = 32;
pub const DCA_LBR_SUBBANDS: usize = 32;
pub const DCA_LBR_TONES: usize = 512;

pub const DCA_LBR_TIME_SAMPLES: usize = 128;
pub const DCA_LBR_TIME_HISTORY: usize = 8;

pub const DCA_LBR_HEADER_SYNC_ONLY: u8 = 1;
pub const DCA_LBR_HEADER_DECODER_INIT: u8 = 2;

const AMP_MAX: usize = 56;

pub mod lbr_flags {
    pub const FLAG_24_BIT: u8 = 0x01;
    pub const FLAG_LFE_PRESENT: u8 = 0x02;
    pub const BAND_LIMIT_2_3: u8 = 0x04;
    pub const BAND_LIMIT_1_2: u8 = 0x08;
    pub const BAND_LIMIT_1_3: u8 = 0x0c;
    pub const BAND_LIMIT_1_4: u8 = 0x10;
    pub const BAND_LIMIT_1_8: u8 = 0x18;
    pub const BAND_LIMIT_NONE: u8 = 0x14;
    pub const BAND_LIMIT_MASK: u8 = 0x1c;
    pub const DMIX_STEREO: u8 = 0x20;
    pub const DMIX_MULTI_CH: u8 = 0x40;
}

pub mod chunk {
    pub const NULL: u8 = 0x00;
    pub const PAD: u8 = 0x01;
    pub const FRAME: u8 = 0x04;
    pub const FRAME_NO_CSUM: u8 = 0x06;
    pub const LFE: u8 = 0x0a;
    pub const ECS: u8 = 0x0b;
    pub const RESERVED_1: u8 = 0x0c;
    pub const RESERVED_2: u8 = 0x0d;
    pub const SCF: u8 = 0x0e;
    pub const TONAL: u8 = 0x10;
    pub const TONAL_GRP_1: u8 = 0x11;
    pub const TONAL_GRP_5: u8 = 0x15;
    pub const TONAL_SCF: u8 = 0x16;
    pub const TONAL_SCF_GRP_1: u8 = 0x17;
    pub const TONAL_SCF_GRP_5: u8 = 0x1b;
    pub const RES_GRID_LR: u8 = 0x30;
    pub const RES_GRID_HR: u8 = 0x40;
    pub const RES_TS_1: u8 = 0x50;
    pub const RES_TS_2: u8 = 0x60;
    pub const EXTENSION: u8 = 0x7f;
}

#[derive(Clone, Copy, Debug)]
pub struct LbrChunk<'a> {
    pub id: u8,
    pub len: usize,
    pub data: &'a [u8],
}

/// `DCALbrTone`.
#[derive(Clone, Copy, Debug, Default)]
pub struct LbrTone {
    pub x_freq: u8,
    pub f_delt: u8,
    pub ph_rot: u8,
    pub pad: u8,
    pub amp: [u8; DCA_LBR_CHANNELS],
    pub phs: [u8; DCA_LBR_CHANNELS],
}

/// LBR VLC books (LE tables).
pub struct LbrVlcs {
    pub tnl_grp: Vec<Vlc>,
    pub tnl_scf: Vlc,
    pub damp: Vlc,
    pub dph: Vlc,
    pub fst_rsd_amp: Vlc,
    pub rsd_apprx: Vlc,
    pub rsd_amp: Vlc,
    pub avg_g3: Vlc,
    pub st_grid: Vlc,
    pub grid_2: Vlc,
    pub grid_3: Vlc,
    pub rsd: Vlc,
}

/// Build the LBR books from `FF_DCA_VLC_SRC_TABLES` following the core
/// books in `ff_dca_init_vlcs` (LE flag set).
pub fn lbr_vlcs() -> LbrVlcs {
    use crate::huffman::{FF_DCA_VLC_SRC_TABLES, TNL_GRP_SIZES};
    let mut pos = core_table_end();

    let mut tnl_grp = Vec::with_capacity(5);
    for i in 0..5 {
        let n = TNL_GRP_SIZES[i] as usize;
        tnl_grp.push(vlc_mod::init(&FF_DCA_VLC_SRC_TABLES[pos..pos + n], DCA_TNL_GRP_VLC_BITS as u32, -1, true));
        pos += n;
    }
    macro_rules! one {
        ($n:expr, $bits:expr) => {{
            let v = vlc_mod::init(&FF_DCA_VLC_SRC_TABLES[pos..pos + $n], $bits as u32, -1, true);
            pos += $n;
            v
        }};
    }
    let tnl_scf = one!(20, DCA_TNL_SCF_VLC_BITS);
    let damp = one!(7, DCA_DAMP_VLC_BITS);
    let dph = one!(9, DCA_DPH_VLC_BITS);
    let fst_rsd_amp = one!(24, DCA_FST_RSD_VLC_BITS);
    let rsd_apprx = one!(6, DCA_RSD_APPRX_VLC_BITS);
    let rsd_amp = one!(33, DCA_RSD_AMP_VLC_BITS);
    let avg_g3 = one!(18, DCA_AVG_G3_VLC_BITS);
    let st_grid = one!(22, DCA_ST_GRID_VLC_BITS);
    let grid_2 = one!(20, DCA_GRID_VLC_BITS);
    let grid_3 = one!(13, DCA_GRID_VLC_BITS);
    let rsd = vlc_mod::init(&FF_DCA_VLC_SRC_TABLES[pos..pos + 9], DCA_RSD_VLC_BITS as u32, 0, true);

    LbrVlcs {
        tnl_grp,
        tnl_scf,
        damp,
        dph,
        fst_rsd_amp,
        rsd_apprx,
        rsd_amp,
        avg_g3,
        st_grid,
        grid_2,
        grid_3,
        rsd,
    }
}

/// Total entries consumed by the core (BE) books — see core.rs's
/// `core_vlc_tables`: quant_index books, bit_allocation, scale_factor,
/// transition_mode.
pub fn core_table_end() -> usize {
    use crate::data::FF_DCA_QUANT_INDEX_GROUP_SIZE;
    use crate::huffman::{BITALLOC_12_VLC_BITS, FF_DCA_BITALLOC_SIZES};
    let mut pos = 0usize;
    for i in 0..crate::huffman::DCA_CODE_BOOKS {
        pos += FF_DCA_BITALLOC_SIZES[i] as usize * FF_DCA_QUANT_INDEX_GROUP_SIZE[i] as usize;
    }
    for b in BITALLOC_12_VLC_BITS.iter() {
        let _ = b;
        pos += 12;
    }
    pos += 5 * 129; // scale_factor
    pos += 4 * 4; // transition_mode
    pos
}

/// `cos_tab`: `cos(M_PI * i / 128)`.
static COS_TAB: LazyLock<Vec<f32>> = LazyLock::new(|| {
    (0..256)
        .map(|i| (std::f64::consts::PI * f64::from(i) / 128.0).cos() as f32)
        .collect()
});

/// `lpc_tab[i] = sin((i - 8) * (M_PI / ((i < 8) ? 17 : 15)))`.
const LPC_TAB: [f32; 16] = [
    -0.995734176295034521871191178905, -0.961825643172819070408796290732,
    -0.895163291355062322067016499754, -0.798017227280239503332805112796,
    -0.673695643646557211712691912426, -0.526432162877355800244607799141,
    -0.361241666187152948744714596184, -0.183749517816570331574408839621,
    0.0, 0.207911690817759337101742284405,
    0.406736643075800207753985990341, 0.587785252292473129168705954639,
    0.743144825477394235014697048974, 0.866025403784438646763723170753,
    0.951056516295153572116439333379, 0.994521895368273336922691944981,
];

const CHANNEL_REORDER_NOLFE: [[i8; 5]; 7] = [
    [0, -1, -1, -1, -1],
    [0, 1, -1, -1, -1],
    [0, 1, 2, -1, -1],
    [0, 1, -1, -1, -1],
    [1, 2, 0, -1, -1],
    [0, 1, 2, 3, -1],
    [0, 1, 3, 4, 2],
];

const CHANNEL_REORDER_LFE: [[i8; 5]; 7] = [
    [0, -1, -1, -1, -1],
    [0, 1, -1, -1, -1],
    [0, 1, 2, -1, -1],
    [1, 2, -1, -1, -1],
    [2, 3, 0, -1, -1],
    [0, 1, 3, 4, -1],
    [0, 1, 4, 5, 2],
];

const LFE_INDEX: [usize; 7] = [1, 2, 3, 0, 1, 2, 3];

/// `channel_layouts[ch_conf]` as DCA speaker masks.
const CHANNEL_LAYOUTS: [u32; 7] = [
    spk_and(speaker::C),
    spk_and(speaker::L) | spk_and(speaker::R),
    spk_and(speaker::L) | spk_and(speaker::R) | spk_and(speaker::C),
    spk_and(speaker::LS) | spk_and(speaker::RS),
    spk_and(speaker::C) | spk_and(speaker::LS) | spk_and(speaker::RS),
    spk_and(speaker::L) | spk_and(speaker::R) | spk_and(speaker::LS) | spk_and(speaker::RS),
    spk_and(speaker::L)
        | spk_and(speaker::R)
        | spk_and(speaker::C)
        | spk_and(speaker::LS)
        | spk_and(speaker::RS),
];

const fn spk_and(bit: usize) -> u32 {
    1u32 << bit
}

use std::sync::LazyLock;

/// `DCALbrDecoder`.
pub struct LbrDecoder {
    pub vlcs: LbrVlcs,

    pub sample_rate: u32,
    pub ch_mask: u32,
    pub flags: u8,
    pub bit_rate_orig: u32,
    pub bit_rate_scaled: u32,

    pub nchannels: usize,
    pub nchannels_total: usize,
    pub freq_range: usize,
    pub band_limit: usize,
    pub limited_rate: u32,
    pub limited_range: i32,
    pub res_profile: usize,
    pub nsubbands: usize,
    pub g3_avg_only_start_sb: usize,
    pub min_mono_subband: usize,
    pub max_mono_subband: usize,

    pub framenum: usize,
    pub lbr_rand: u32,
    pub warned: u8,

    pub quant_levels: [[u8; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
    pub sb_indices: [u8; DCA_LBR_SUBBANDS],

    pub sec_ch_sbms: [[u8; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
    pub sec_ch_lrms: [[u8; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
    pub ch_pres: [u32; DCA_LBR_CHANNELS],

    pub grid_1_scf: [[[u8; 8]; 12]; DCA_LBR_CHANNELS],
    pub grid_2_scf: [[[u8; 64]; 3]; DCA_LBR_CHANNELS],

    pub grid_3_avg: [[i8; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS],
    pub grid_3_scf: [[[i8; 8]; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS],
    pub grid_3_pres: [u32; DCA_LBR_CHANNELS],

    pub high_res_scf: [[[u8; 8]; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS],

    pub part_stereo: [[[u8; 5]; DCA_LBR_SUBBANDS / 4]; DCA_LBR_CHANNELS],
    pub part_stereo_pres: u8,

    pub lpc_coeff: [[[[[f32; 8]; 2]; 3]; DCA_LBR_CHANNELS]; 2],

    pub sb_scf: [f32; DCA_LBR_SUBBANDS],

    /// Time samples: `[channel][subband][DCA_LBR_TIME_SAMPLES + 2*HIST]`,
    /// the leading `DCA_LBR_TIME_HISTORY` samples are history.
    pub ts_buffer: Vec<f32>,
    pub nchsamples_ts: usize,

    pub history: [[f32; DCA_LBR_SUBBANDS * 4]; DCA_LBR_CHANNELS],
    pub window: Vec<f32>,

    pub lfe_data: [f32; 64],
    pub lfe_history: [[f32; 2]; 5],
    pub lfe_scale: f32,

    pub tonal_scf: [u8; 6],
    pub tonal_bounds: [[[usize; 2]; 32]; 5],
    pub tones: [LbrTone; DCA_LBR_TONES],
    pub ntones: usize,

    /// IMDCT length (`1 << (freq_range + 5)`).
    pub imdct_len: usize,
}

impl LbrDecoder {
    pub fn new() -> Self {
        Self {
            vlcs: lbr_vlcs(),
            sample_rate: 0,
            ch_mask: 0,
            flags: 0,
            bit_rate_orig: 0,
            bit_rate_scaled: 0,
            nchannels: 0,
            nchannels_total: 0,
            freq_range: 0,
            band_limit: 0,
            limited_rate: 0,
            limited_range: 0,
            res_profile: 0,
            nsubbands: 0,
            g3_avg_only_start_sb: 0,
            min_mono_subband: 0,
            max_mono_subband: 0,
            framenum: 0,
            lbr_rand: 1,
            warned: 0,
            quant_levels: [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
            sb_indices: [0xff; DCA_LBR_SUBBANDS],
            sec_ch_sbms: [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
            sec_ch_lrms: [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2],
            ch_pres: [0; DCA_LBR_CHANNELS],
            grid_1_scf: [[[0; 8]; 12]; DCA_LBR_CHANNELS],
            grid_2_scf: [[[0; 64]; 3]; DCA_LBR_CHANNELS],
            grid_3_avg: [[0; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS],
            grid_3_scf: [[[0; 8]; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS],
            grid_3_pres: [0; DCA_LBR_CHANNELS],
            high_res_scf: [[[0; 8]; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS],
            part_stereo: [[[16; 5]; DCA_LBR_SUBBANDS / 4]; DCA_LBR_CHANNELS],
            part_stereo_pres: 0,
            lpc_coeff: [[[[[0.0; 8]; 2]; 3]; DCA_LBR_CHANNELS]; 2],
            sb_scf: [0.0; DCA_LBR_SUBBANDS],
            ts_buffer: Vec::new(),
            nchsamples_ts: 0,
            history: [[0.0; DCA_LBR_SUBBANDS * 4]; DCA_LBR_CHANNELS],
            window: Vec::new(),
            lfe_data: [0.0; 64],
            lfe_history: [[0.0; 2]; 5],
            lfe_scale: 0.0,
            tonal_scf: [0; 6],
            tonal_bounds: [[[0; 2]; 32]; 5],
            tones: [LbrTone::default(); DCA_LBR_TONES],
            ntones: 0,
            imdct_len: 0,
        }
    }

    /// Row into `ts_buffer` for channel/subband (after history).
    fn ts(&self, ch: usize, sb: usize) -> &[f32] {
        let n = self.nchsamples_ts;
        let start = (ch * self.nsubbands.max(1) + sb) * n + DCA_LBR_TIME_HISTORY;
        &self.ts_buffer[start..start + n - DCA_LBR_TIME_HISTORY]
    }

    fn ts_mut(&mut self, ch: usize, sb: usize) -> &mut [f32] {
        let n = self.nchsamples_ts;
        let start = (ch * self.nsubbands.max(1) + sb) * n + DCA_LBR_TIME_HISTORY;
        let end = start + n - DCA_LBR_TIME_HISTORY;
        &mut self.ts_buffer[start..end]
    }

    /// `lbr_rand`.
    fn lbr_rand(&mut self, sb: usize) -> f32 {
        self.lbr_rand = 1103515245u32.wrapping_mul(self.lbr_rand).wrapping_add(12345);
        self.lbr_rand as f32 * self.sb_scf[sb]
    }

    /// `parse_vlc` with the "rare value" fallback.
    fn parse_vlc(&self, gb: &mut LeBitReader, vlc: &Vlc, _nb_bits: u8, max_depth: u32) -> i32 {
        let v = vlc.get_le(gb, max_depth);
        if v >= 0 {
            return v;
        }
        // Rare value
        let n = gb.get_bits(3) + 1;
        gb.get_bits(n) as i32
    }

    // ───────────── LFE ─────────────

    fn parse_lfe_24(&mut self, gb: &mut LeBitReader) -> Result<(), &'static str> {
        let step_max = FF_DCA_LFE_STEP_SIZE_24.len() - 1;

        let ps = gb.get_bits(24) as i32;
        let si = ps >> 23;
        let mut value = ((((ps & 0x7fffff) ^ -si) + si) as f32) * (1.0 / 0x7fffff as f32);

        let mut step_i = gb.get_bits(8) as i32;
        if step_i as usize > step_max {
            return Err("invalid LFE step size index");
        }

        let mut step = FF_DCA_LFE_STEP_SIZE_24[step_i as usize];

        for i in 0..64 {
            let code = gb.get_bits(6);

            let mut delta = step * 0.03125;
            if code & 16 != 0 {
                delta += step;
            }
            if code & 8 != 0 {
                delta += step * 0.5;
            }
            if code & 4 != 0 {
                delta += step * 0.25;
            }
            if code & 2 != 0 {
                delta += step * 0.125;
            }
            if code & 1 != 0 {
                delta += step * 0.0625;
            }

            if code & 32 != 0 {
                value -= delta;
                if value < -3.0 {
                    value = -3.0;
                }
            } else {
                value += delta;
                if value > 3.0 {
                    value = 3.0;
                }
            }

            step_i += i32::from(FF_DCA_LFE_DELTA_INDEX_24[(code & 31) as usize]);
            step_i = step_i.clamp(0, step_max as i32);

            step = FF_DCA_LFE_STEP_SIZE_24[step_i as usize];
            self.lfe_data[i] = value * self.lfe_scale;
        }

        Ok(())
    }

    fn parse_lfe_16(&mut self, gb: &mut LeBitReader) -> Result<(), &'static str> {
        let step_max = FF_DCA_LFE_STEP_SIZE_16.len() - 1;

        let ps = gb.get_bits(16) as i32;
        let si = ps >> 15;
        let mut value = ((((ps & 0x7fff) ^ -si) + si) as f32) * (1.0 / 0x7fff as f32);

        let mut step_i = gb.get_bits(8) as i32;
        if step_i as usize > step_max {
            return Err("invalid LFE step size index");
        }

        let mut step = FF_DCA_LFE_STEP_SIZE_16[step_i as usize];

        for i in 0..64 {
            let code = gb.get_bits(4);

            let mut delta = step * 0.125;
            if code & 4 != 0 {
                delta += step;
            }
            if code & 2 != 0 {
                delta += step * 0.5;
            }
            if code & 1 != 0 {
                delta += step * 0.25;
            }

            if code & 8 != 0 {
                value -= delta;
                if value < -3.0 {
                    value = -3.0;
                }
            } else {
                value += delta;
                if value > 3.0 {
                    value = 3.0;
                }
            }

            step_i += i32::from(FF_DCA_LFE_DELTA_INDEX_16[(code & 7) as usize]);
            step_i = step_i.clamp(0, step_max as i32);

            step = FF_DCA_LFE_STEP_SIZE_16[step_i as usize];
            self.lfe_data[i] = value * self.lfe_scale;
        }

        Ok(())
    }

    fn parse_lfe_chunk(&mut self, chunk: &LbrChunk) -> Result<(), &'static str> {
        if self.flags & lbr_flags::FLAG_LFE_PRESENT == 0 {
            return Ok(());
        }

        if chunk.len == 0 {
            return Ok(());
        }

        let mut gb = LeBitReader::new(chunk.data);

        // Determine bit depth from chunk size
        if chunk.len >= 52 {
            return self.parse_lfe_24(&mut gb);
        }
        if chunk.len >= 35 {
            return self.parse_lfe_16(&mut gb);
        }

        Err("LFE chunk too short")
    }
}

impl LbrDecoder {
    // ───────────── tonal chunks ─────────────

    fn parse_tonal(&mut self, gb: &mut LeBitReader, group: usize) -> Result<(), &'static str> {
        let ch_nbits = 32 - ((self.nchannels_total as u32) - 1).leading_zeros(); // av_ceil_log2
        let mut amp = [0u32; DCA_LBR_CHANNELS_TOTAL];
        let mut phs = [0u32; DCA_LBR_CHANNELS_TOTAL];

        // Parse subframes for this group
        let mut sf = 0usize;
        let mut diff;
        while sf < 1 << group {
            let sf_idx = ((self.framenum << group) + sf) & 31;
            self.tonal_bounds[group][sf_idx][0] = self.ntones;

            // Parse tones for this subframe
            let mut freq = 1usize;
            loop {
                if gb.bits_left() < 1 {
                    return Err("tonal group chunk too short");
                }

                let d = self.parse_vlc(gb, &self.vlcs.tnl_grp[group], DCA_TNL_GRP_VLC_BITS, 2);
                if d < 0 || d as usize >= FF_DCA_FST_AMP.len() {
                    return Err("invalid tonal frequency diff");
                }
                let d = d as usize;

                diff = gb.get_bits((d >> 2) as u32) as usize + FF_DCA_FST_AMP[d] as usize;
                if diff <= 1 {
                    break; // End of subframe
                }

                freq += diff - 2;
                if freq >> (5 - group) > self.nsubbands * 4 - 6 {
                    return Err("invalid spectral line offset");
                }

                // Main channel
                let main_ch = gb.get_bits(ch_nbits) as usize;
                let main_amp = self.parse_vlc(gb, &self.vlcs.tnl_scf, DCA_TNL_SCF_VLC_BITS, 2)
                    + i32::from(
                        self.tonal_scf
                            [FF_DCA_FREQ_TO_SB[(freq >> (7 - group)) as usize] as usize],
                    )
                    + self.limited_range as i32
                    - 2;
                amp[main_ch] = if main_amp >= 0 && (main_amp as usize) < AMP_MAX {
                    main_amp as u32
                } else {
                    0
                };
                phs[main_ch] = u32::from(gb.get_bits(3));

                // Secondary channels
                for ch in 0..self.nchannels_total {
                    if ch == main_ch {
                        continue;
                    }
                    if gb.get_bits(1) != 0 {
                        let d = self.parse_vlc(gb, &self.vlcs.damp, DCA_DAMP_VLC_BITS, 1);
                        amp[ch] = (amp[main_ch] as i32 - d) as u32;
                        let d = self.parse_vlc(gb, &self.vlcs.dph, DCA_DPH_VLC_BITS, 1);
                        phs[ch] = (phs[main_ch] as i32 - d) as u32;
                    } else {
                        amp[ch] = 0;
                        phs[ch] = 0;
                    }
                }

                if amp[main_ch] != 0 {
                    // Allocate new tone
                    let idx = self.ntones;
                    self.ntones = (self.ntones + 1) & (DCA_LBR_TONES - 1);

                    let t = &mut self.tones[idx];
                    t.x_freq = (freq >> (5 - group)) as u8;
                    t.f_delt = ((freq & ((1 << (5 - group)) - 1)) << group) as u8;
                    t.ph_rot = (256 - u32::from(t.x_freq & 1) * 128 - u32::from(t.f_delt) * 4) as u8;

                    let shift = i32::from(
                        FF_DCA_PH0_SHIFT[(usize::from(t.x_freq & 3)) * 2 + (freq & 1)],
                    ) - (((i32::from(t.ph_rot) << (5 - group)) - i32::from(t.ph_rot)));

                    for ch in 0..self.nchannels {
                        t.amp[ch] = if amp[ch] < AMP_MAX as u32 { amp[ch] as u8 } else { 0 };
                        t.phs[ch] = (128 - i32::from(phs[ch] as u8) * 32 + shift) as u8;
                    }
                }
            }

            self.tonal_bounds[group][sf_idx][1] = self.ntones;

            sf += if diff != 0 { 8 } else { 1 };
        }

        Ok(())
    }

    fn parse_tonal_chunk(&mut self, chunk: &LbrChunk) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }

        let mut gb = LeBitReader::new(chunk.data);

        // Scale factors
        if chunk.id == chunk::SCF || chunk.id == chunk::TONAL_SCF {
            if gb.bits_left() < 36 {
                return Err("tonal scale factor chunk too short");
            }
            for sb in 0..6 {
                self.tonal_scf[sb] = gb.get_bits(6) as u8;
            }
        }

        // Tonal groups
        if chunk.id == chunk::TONAL || chunk.id == chunk::TONAL_SCF {
            for group in 0..5 {
                self.parse_tonal(&mut gb, group)?;
            }
        }

        Ok(())
    }

    fn parse_tonal_group(&mut self, chunk: &LbrChunk) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }

        let mut gb = LeBitReader::new(chunk.data);
        self.parse_tonal(&mut gb, chunk.id as usize)
    }

    // ───────────── scale factors ─────────────

    /// `ensure_bits`: returns Ok(false) when out of bits (caller stops).
    fn ensure_bits(gb: &mut LeBitReader, n: i32) -> Result<bool, &'static str> {
        let left = gb.bits_left();
        if left < 0 {
            return Err("read past end");
        }
        if left < n {
            gb.skip(left.max(0) as u32);
            return Ok(false);
        }
        Ok(true)
    }

    fn parse_scale_factors(&mut self, gb: &mut LeBitReader, scf: &mut [u8; 8]) -> Result<(), &'static str> {
        // Truncated scale factors remain zero
        if !Self::ensure_bits(gb, 20)? {
            return Ok(());
        }

        // Initial scale factor
        let mut prev = self.parse_vlc(gb, &self.vlcs.fst_rsd_amp, DCA_FST_RSD_VLC_BITS, 2);
        let mut next = prev;

        let mut sf = 0usize;
        while sf < 7 {
            scf[sf] = prev as u8; // Store previous value

            if !Self::ensure_bits(gb, 20)? {
                return Ok(());
            }

            // Interpolation distance
            let dist = self.parse_vlc(gb, &self.vlcs.rsd_apprx, DCA_RSD_APPRX_VLC_BITS, 1) + 1;
            if dist as usize > 7 - sf {
                return Err("invalid scale factor distance");
            }

            if !Self::ensure_bits(gb, 20)? {
                return Ok(());
            }

            // Final interpolation point
            next = self.parse_vlc(gb, &self.vlcs.rsd_amp, DCA_RSD_AMP_VLC_BITS, 2);

            if next & 1 != 0 {
                next = prev + ((next + 1) >> 1);
            } else {
                next = prev - (next >> 1);
            }

            // Interpolate
            match dist {
                2 => {
                    if next > prev {
                        scf[sf + 1] = (prev + ((next - prev) >> 1)) as u8;
                    } else {
                        scf[sf + 1] = (prev - ((prev - next) >> 1)) as u8;
                    }
                }
                4 => {
                    if next > prev {
                        scf[sf + 1] = (prev + ((next - prev) >> 2)) as u8;
                        scf[sf + 2] = (prev + ((next - prev) >> 1)) as u8;
                        scf[sf + 3] = (prev + (((next - prev) * 3) >> 2)) as u8;
                    } else {
                        scf[sf + 1] = (prev - ((prev - next) >> 2)) as u8;
                        scf[sf + 2] = (prev - ((prev - next) >> 1)) as u8;
                        scf[sf + 3] = (prev - (((prev - next) * 3) >> 2)) as u8;
                    }
                }
                _ => {
                    for i in 1..dist as usize {
                        scf[sf + i] = (prev + (next - prev) * i as i32 / dist) as u8;
                    }
                }
            }

            prev = next;
            sf += dist as usize;
        }

        scf[sf] = next as u8; // Store final value

        Ok(())
    }

    fn parse_st_code(&self, gb: &mut LeBitReader, min_v: i32) -> i32 {
        let v = self.parse_vlc(gb, &self.vlcs.st_grid, DCA_ST_GRID_VLC_BITS, 2) + min_v;

        let v = if v & 1 != 0 {
            16 + (v >> 1)
        } else {
            16 - (v >> 1)
        };

        if v as usize >= FF_DCA_ST_COEFF.len() {
            16
        } else {
            v
        }
    }
}

impl LbrDecoder {
    // ───────────── grid chunks ─────────────

    fn parse_grid_1_chunk(&mut self, chunk: &LbrChunk, ch1: usize, ch2: usize) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }

        let mut gb = LeBitReader::new(chunk.data);

        // Scale factors
        let nsubbands = usize::from(FF_DCA_SCF_TO_GRID_1[self.nsubbands - 1]) + 1;
        for sb in 2..nsubbands {
            let mut scf = self.grid_1_scf[ch1][sb];
            self.parse_scale_factors(&mut gb, &mut scf)?;
            self.grid_1_scf[ch1][sb] = scf;
            if ch1 != ch2 && usize::from(FF_DCA_GRID_1_TO_SCF[sb]) < self.min_mono_subband {
                let mut scf2 = self.grid_1_scf[ch2][sb];
                self.parse_scale_factors(&mut gb, &mut scf2)?;
                self.grid_1_scf[ch2][sb] = scf2;
            }
        }

        if gb.bits_left() < 1 {
            return Ok(()); // Should not happen, but a sample exists that proves otherwise
        }

        // Average values for third grid
        for sb in 0..self.nsubbands - 4 {
            self.grid_3_avg[ch1][sb] =
                (self.parse_vlc(&mut gb, &self.vlcs.avg_g3, DCA_AVG_G3_VLC_BITS, 2) - 16) as i8;
            if ch1 != ch2 {
                if sb + 4 < self.min_mono_subband {
                    self.grid_3_avg[ch2][sb] =
                        (self.parse_vlc(&mut gb, &self.vlcs.avg_g3, DCA_AVG_G3_VLC_BITS, 2) - 16) as i8;
                } else {
                    self.grid_3_avg[ch2][sb] = self.grid_3_avg[ch1][sb];
                }
            }
        }

        if gb.bits_left() < 0 {
            return Err("first grid chunk too short");
        }

        // Stereo image for partial mono mode
        if ch1 != ch2 {
            if !Self::ensure_bits(&mut gb, 8)? {
                return Ok(());
            }

            let min_v = [gb.get_bits(4) as i32, gb.get_bits(4) as i32];

            let nsubbands = (self.nsubbands - self.min_mono_subband + 3) / 4;
            for sb in 0..nsubbands {
                for ch in ch1..=ch2 {
                    for sf in 1..=4 {
                        self.part_stereo[ch][sb][sf] =
                            self.parse_st_code(&mut gb, min_v[ch - ch1]) as u8;
                    }
                }
            }

            if gb.bits_left() >= 0 {
                self.part_stereo_pres |= 1 << ch1;
            }
        }

        // Low resolution spatial information is not decoded
        Ok(())
    }

    fn parse_grid_1_sec_ch(&mut self, gb: &mut LeBitReader, ch2: usize) -> Result<(), &'static str> {
        // Scale factors
        let nsubbands = usize::from(FF_DCA_SCF_TO_GRID_1[self.nsubbands - 1]) + 1;
        for sb in 2..nsubbands {
            if usize::from(FF_DCA_GRID_1_TO_SCF[sb]) >= self.min_mono_subband {
                let mut scf2 = self.grid_1_scf[ch2][sb];
                self.parse_scale_factors(gb, &mut scf2)?;
                self.grid_1_scf[ch2][sb] = scf2;
            }
        }

        // Average values for third grid
        for sb in 0..self.nsubbands - 4 {
            if sb + 4 >= self.min_mono_subband {
                if !Self::ensure_bits(gb, 20)? {
                    return Ok(());
                }
                self.grid_3_avg[ch2][sb] =
                    (self.parse_vlc(gb, &self.vlcs.avg_g3, DCA_AVG_G3_VLC_BITS, 2) - 16) as i8;
            }
        }

        Ok(())
    }

    fn parse_grid_3(&mut self, gb: &mut LeBitReader, ch1: usize, ch2: usize, sb: usize, flag: bool) {
        for ch in ch1..=ch2 {
            if (ch != ch1 && sb + 4 >= self.min_mono_subband) != flag {
                continue;
            }

            if self.grid_3_pres[ch] & (1u32 << sb) != 0 {
                continue; // Already parsed
            }

            for i in 0..8 {
                if !Self::ensure_bits(gb, 20).unwrap_or(false) {
                    return;
                }
                self.grid_3_scf[ch][sb][i] =
                    (self.parse_vlc(gb, &self.vlcs.grid_3, DCA_GRID_VLC_BITS, 2) - 16) as i8;
            }

            // Flag scale factors for this subband parsed
            self.grid_3_pres[ch] |= 1u32 << sb;
        }
    }

    // ───────────── time samples ─────────────

    fn parse_ch(&mut self, gb: &mut LeBitReader, ch: usize, sb: usize, quant_level: usize, flag: bool) {
        let mut i = 0usize;

        if !Self::ensure_bits(gb, 20).unwrap_or(false) {
            return; // Too few bits left
        }

        let coding_method = gb.get_bits(1) != 0;

        let mut new_samples: Vec<f32> = Vec::with_capacity(DCA_LBR_TIME_SAMPLES);

        match quant_level {
            1 => {
                let nblocks = (gb.bits_left() as usize / 8).min(DCA_LBR_TIME_SAMPLES / 8);
                for _ in 0..nblocks {
                    let code = gb.get_bits(8);
                    for j in 0..8 {
                        new_samples.push(FF_DCA_RSD_LEVEL_2A[((code >> j) & 1) as usize]);
                    }
                }
                i = nblocks * 8;
            }
            2 => {
                if coding_method {
                    while i < DCA_LBR_TIME_SAMPLES && gb.bits_left() >= 2 {
                        if gb.get_bits(1) != 0 {
                            new_samples.push(FF_DCA_RSD_LEVEL_2B[gb.get_bits(1) as usize]);
                        } else {
                            new_samples.push(0.0);
                        }
                        i += 1;
                    }
                } else {
                    let nblocks = (gb.bits_left() as usize / 8).min((DCA_LBR_TIME_SAMPLES + 4) / 5);
                    for _ in 0..nblocks {
                        let code = usize::from(FF_DCA_RSD_PACK_5_IN_8[gb.get_bits(8) as usize]);
                        for j in 0..5 {
                            new_samples.push(FF_DCA_RSD_LEVEL_3[(code >> (j * 2)) & 3]);
                        }
                    }
                    i = nblocks * 5;
                }
            }
            3 => {
                let nblocks = (gb.bits_left() as usize / 7).min((DCA_LBR_TIME_SAMPLES + 2) / 3);
                for _ in 0..nblocks {
                    let code = gb.get_bits(7) as usize;
                    for j in 0..3 {
                        new_samples.push(FF_DCA_RSD_LEVEL_5[usize::from(FF_DCA_RSD_PACK_3_IN_7[code][j])]);
                    }
                }
                i = nblocks * 3;
            }
            4 => {
                while i < DCA_LBR_TIME_SAMPLES && gb.bits_left() >= 6 {
                    let v = self.vlcs.rsd.get_le(gb, 1);
                    new_samples.push(FF_DCA_RSD_LEVEL_8[v.max(0) as usize]);
                    i += 1;
                }
            }
            5 => {
                let nblocks = (gb.bits_left() as usize / 4).min(DCA_LBR_TIME_SAMPLES);
                for _ in 0..nblocks {
                    new_samples.push(FF_DCA_RSD_LEVEL_16[gb.get_bits(4) as usize]);
                }
                i = nblocks;
            }
            _ => unreachable!("quant_level"),
        }

        if flag && gb.bits_left() < 20 {
            return; // Skip incomplete mono subband
        }

        // Truncate to i then fill with randomness
        new_samples.truncate(i);
        let mut rand_vals: Vec<f32> = Vec::with_capacity(DCA_LBR_TIME_SAMPLES - new_samples.len());
        for _ in new_samples.len()..DCA_LBR_TIME_SAMPLES {
            rand_vals.push(self.lbr_rand(sb));
        }
        new_samples.extend_from_slice(&rand_vals);

        let row = self.ts_mut(ch, sb);
        let len = row.len().min(new_samples.len());
        row[..len].copy_from_slice(&new_samples[..len]);

        self.ch_pres[ch] |= 1u32 << sb;
    }

    fn parse_ts(&mut self, gb: &mut LeBitReader, ch1: usize, ch2: usize, start_sb: usize, end_sb: usize, flag: bool) -> Result<(), &'static str> {
        let mut sb = start_sb;
        while sb < end_sb {
            // Subband number before reordering
            let sb_reorder;
            if sb < 6 {
                sb_reorder = sb;
            } else if flag && sb < self.max_mono_subband {
                sb_reorder = usize::from(self.sb_indices[sb]);
            } else {
                if !Self::ensure_bits(gb, 28)? {
                    break;
                }
                let mut v = gb.get_bits(self.limited_range as u32 + 3) as usize;
                if v < 6 {
                    v = 6;
                }
                self.sb_indices[sb] = v as u8;
                sb_reorder = v;
            }
            if sb_reorder >= self.nsubbands {
                return Err("subband reorder out of range");
            }

            // Third grid scale factors
            if sb == 12 {
                for sb_g3 in 0..self.g3_avg_only_start_sb.saturating_sub(4) {
                    self.parse_grid_3(gb, ch1, ch2, sb_g3, flag);
                }
            } else if sb < 12 && sb_reorder >= 4 {
                self.parse_grid_3(gb, ch1, ch2, sb_reorder - 4, flag);
            }

            // Secondary channel flags
            if ch1 != ch2 {
                if !Self::ensure_bits(gb, 20)? {
                    break;
                }
                if !flag || sb_reorder >= self.max_mono_subband {
                    self.sec_ch_sbms[ch1 / 2][sb_reorder] = gb.get_bits(8) as u8;
                }
                if flag && sb_reorder >= self.min_mono_subband {
                    self.sec_ch_lrms[ch1 / 2][sb_reorder] = gb.get_bits(8) as u8;
                }
            }

            let quant_level = usize::from(self.quant_levels[ch1 / 2][sb_reorder]);
            if quant_level == 0 {
                return Err("invalid quantization level");
            }

            // Time samples for one or both channels
            if sb < self.max_mono_subband && sb_reorder >= self.min_mono_subband {
                if !flag {
                    self.parse_ch(gb, ch1, sb_reorder, quant_level, false);
                } else if ch1 != ch2 {
                    self.parse_ch(gb, ch2, sb_reorder, quant_level, true);
                }
            } else {
                self.parse_ch(gb, ch1, sb_reorder, quant_level, false);
                if ch1 != ch2 {
                    self.parse_ch(gb, ch2, sb_reorder, quant_level, false);
                }
            }

            sb += 1;
        }

        Ok(())
    }

    // ───────────── LPC ─────────────

    fn convert_lpc(&self, coeff: &mut [f32; 8], codes: &[usize]) {
        for i in 0..8 {
            let rc = LPC_TAB[codes[i]];
            for j in 0..(i + 1) / 2 {
                let tmp1 = coeff[j];
                let tmp2 = coeff[i - j - 1];
                coeff[j] = tmp1 + rc * tmp2;
                coeff[i - j - 1] = tmp2 + rc * tmp1;
            }
            coeff[i] = rc;
        }
    }

    fn parse_lpc(&mut self, gb: &mut LeBitReader, ch1: usize, ch2: usize, start_sb: usize, end_sb: usize) -> Result<(), &'static str> {
        let f = self.framenum & 1;
        let mut codes = [0usize; 16];

        // First two subbands have two sets of coefficients, third has one
        for sb in start_sb..end_sb {
            let ncodes = 8 * (1 + usize::from(sb < 2));
            for ch in ch1..=ch2 {
                if !Self::ensure_bits(gb, (4 * ncodes) as i32)? {
                    return Ok(());
                }
                for c in codes.iter_mut().take(ncodes) {
                    *c = gb.get_bits(4) as usize;
                }
                for i in 0..ncodes / 8 {
                    let mut coeff = self.lpc_coeff[f][ch][sb][i];
                    self.convert_lpc(&mut coeff, &codes[i * 8..i * 8 + 8]);
                    self.lpc_coeff[f][ch][sb][i] = coeff;
                }
            }
        }

        Ok(())
    }
}

impl LbrDecoder {
    // ───────────── high-res grid / grid 2 / TS chunks ─────────────

    fn parse_high_res_grid(&mut self, chunk: &LbrChunk, ch1: usize, ch2: usize) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }

        let mut gb = LeBitReader::new(chunk.data);

        // Quantizer profile
        let profile = gb.get_bits(8);
        // Overall level
        let ol = (profile >> 3) & 7;
        // Steepness
        let st = profile >> 6;
        // Max energy subband
        let max_sb = profile & 7;

        // Calculate quantization levels
        let mut quant_levels = [0u8; DCA_LBR_SUBBANDS];
        for sb in 0..self.nsubbands {
            let f = sb as u64 * u64::from(self.limited_rate) / self.nsubbands as u64;
            let a = 18000 / (12 * f / 1000 + 100 + 40 * u64::from(st)) + 20 * u64::from(ol);
            quant_levels[sb] = if a <= 95 {
                1
            } else if a <= 140 {
                2
            } else if a <= 180 {
                3
            } else if a <= 230 {
                4
            } else {
                5
            };
        }

        // Reorder quantization levels for lower subbands
        for sb in 0..8 {
            self.quant_levels[ch1 / 2][sb] = quant_levels[usize::from(FF_DCA_SB_REORDER[max_sb as usize][sb])];
        }
        for sb in 8..self.nsubbands {
            self.quant_levels[ch1 / 2][sb] = quant_levels[sb];
        }

        // LPC for the first two subbands
        self.parse_lpc(&mut gb, ch1, ch2, 0, 2)?;

        // Time-samples for the first two subbands of main channel
        self.parse_ts(&mut gb, ch1, ch2, 0, 2, false)?;

        // First two bands of the first grid
        for sb in 0..2 {
            for ch in ch1..=ch2 {
                let mut scf = self.grid_1_scf[ch][sb];
                self.parse_scale_factors(&mut gb, &mut scf)?;
                self.grid_1_scf[ch][sb] = scf;
            }
        }

        Ok(())
    }

    fn parse_grid_2(&mut self, gb: &mut LeBitReader, ch1: usize, ch2: usize, start_sb: usize, end_sb: usize, flag: bool) {
        let mut end_sb = end_sb;
        let nsubbands = usize::from(FF_DCA_SCF_TO_GRID_2[self.nsubbands - 1]) + 1;
        if end_sb > nsubbands {
            end_sb = nsubbands;
        }

        for sb in start_sb..end_sb {
            for ch in ch1..=ch2 {
                let same = (ch != ch1 && usize::from(FF_DCA_GRID_2_TO_SCF[sb]) >= self.min_mono_subband) != flag;
                if same {
                    if !flag {
                        let src = self.grid_2_scf[ch1][sb];
                        self.grid_2_scf[ch][sb] = src;
                    }
                    continue;
                }

                // Scale factors in groups of 8
                for i in 0..8 {
                    let base = i * 8;
                    if gb.bits_left() < 1 {
                        for v in self.grid_2_scf[ch][sb][base..64].iter_mut() {
                            *v = 0;
                        }
                        break;
                    }
                    // Bit indicating if whole group has zero values
                    if gb.get_bits(1) != 0 {
                        for j in 0..8 {
                            if !Self::ensure_bits(gb, 20).unwrap_or(false) {
                                break;
                            }
                            self.grid_2_scf[ch][sb][base + j] =
                                self.parse_vlc(gb, &self.vlcs.grid_2, DCA_GRID_VLC_BITS, 2) as u8;
                        }
                    } else {
                        for v in self.grid_2_scf[ch][sb][base..base + 8].iter_mut() {
                            *v = 0;
                        }
                    }
                }
            }
        }
    }

    fn parse_ts1_chunk(&mut self, chunk: &LbrChunk, ch1: usize, ch2: usize) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }
        let mut gb = LeBitReader::new(chunk.data);
        self.parse_lpc(&mut gb, ch1, ch2, 2, 3)?;
        self.parse_ts(&mut gb, ch1, ch2, 2, 4, false)?;
        self.parse_grid_2(&mut gb, ch1, ch2, 0, 1, false);
        self.parse_ts(&mut gb, ch1, ch2, 4, 6, false)?;
        Ok(())
    }

    fn parse_ts2_chunk(&mut self, chunk: &LbrChunk, ch1: usize, ch2: usize) -> Result<(), &'static str> {
        if chunk.len == 0 {
            return Ok(());
        }
        let mut gb = LeBitReader::new(chunk.data);
        self.parse_grid_2(&mut gb, ch1, ch2, 1, 3, false);
        self.parse_ts(&mut gb, ch1, ch2, 6, self.max_mono_subband, false)?;
        if ch1 != ch2 {
            self.parse_grid_1_sec_ch(&mut gb, ch2)?;
            self.parse_grid_2(&mut gb, ch1, ch2, 0, 3, true);
        }
        self.parse_ts(&mut gb, ch1, ch2, self.min_mono_subband, self.nsubbands, true)?;
        Ok(())
    }
}

impl LbrDecoder {
    // ───────────── init_sample_rate / alloc / decoder_init ─────────────

    fn init_sample_rate(&mut self) -> Result<(), &'static str> {
        let _init_scale = (-1.0 / f64::from(1 << 17)) * (2f64.powi(2 - self.limited_range)).sqrt();
        let mut scale: f64;
        let br_per_ch = self.bit_rate_scaled / self.nchannels_total as u32;

        self.imdct_len = 1usize << (self.freq_range + 5);

        self.window = (0..32 << self.freq_range)
            .map(|i| FF_DCA_LONG_WINDOW[i << (2 - self.freq_range)])
            .collect();

        if br_per_ch < 14000 {
            scale = 0.85;
        } else if br_per_ch < 32000 {
            scale = f64::from(br_per_ch - 14000) * (1.0 / 120000.0) + 0.85;
        } else {
            scale = 1.0;
        }

        scale *= 1.0 / u32::MAX as f64;

        for i in 0..self.nsubbands {
            if i < 2 {
                self.sb_scf[i] = 0.0; // The first two subbands are always zero
            } else if i < 5 {
                self.sb_scf[i] = ((i - 1) as f32) * 0.25 * 0.785 * scale as f32;
            } else {
                self.sb_scf[i] = 0.785 * scale as f32;
            }
        }

        self.lfe_scale = ((16 << self.freq_range) as f32) * 0.000_007_826_589_4;

        Ok(())
    }

    fn alloc_sample_buffer(&mut self) {
        // Reserve space for history and padding
        let nchsamples = DCA_LBR_TIME_SAMPLES + DCA_LBR_TIME_HISTORY * 2;
        let nsamples = nchsamples * self.nchannels * self.nsubbands;
        self.ts_buffer = vec![0.0f32; nsamples];
        self.nchsamples_ts = nchsamples;
    }

    /// `parse_decoder_init`.
    fn parse_decoder_init<R: std::io::Read>(&mut self, gb: &mut R) -> Result<(), &'static str> {
        let old_rate = self.sample_rate;
        let old_band_limit = self.band_limit;
        let old_nchannels = self.nchannels;

        fn read_byte<R: std::io::Read>(gb: &mut R, b: &mut [u8]) -> Result<(), &'static str> {
            std::io::Read::read_exact(gb, b).map_err(|_| "truncated LBR decoder init")
        }
        let mut byte = [0u8; 1];

        // Sample rate of LBR audio
        read_byte(gb, &mut byte)?;
        let sr_code = byte[0] as u32;
        if sr_code as usize >= FF_DCA_SAMPLING_FREQS.len() {
            return Err("invalid LBR sample rate");
        }
        self.sample_rate = FF_DCA_SAMPLING_FREQS[sr_code as usize];
        if self.sample_rate > 48000 {
            return Err("unsupported LBR sample rate");
        }

        // LBR speaker mask
        let mut two = [0u8; 2];
        read_byte(gb, &mut two)?;
        self.ch_mask = u16::from_le_bytes(two) as u32;
        if self.ch_mask & 0x7 == 0 {
            return Err("unsupported LBR channel mask");
        }

        // LBR bitstream version
        read_byte(gb, &mut two)?;
        let version = u16::from_le_bytes(two);
        if version & 0xff00 != 0x0800 {
            return Err("unsupported LBR stream version");
        }

        // Flags for LBR decoder initialization
        read_byte(gb, &mut byte)?;
        self.flags = byte[0];
        if self.flags & lbr_flags::DMIX_MULTI_CH != 0 {
            return Err("unsupported LBR multi-channel downmix");
        }
        if (self.flags & lbr_flags::FLAG_LFE_PRESENT) != 0 && self.sample_rate != 48000 {
            self.flags &= !lbr_flags::FLAG_LFE_PRESENT;
        }

        // Most significant bit rate nibbles
        read_byte(gb, &mut byte)?;
        let bit_rate_hi = byte[0] as u32;

        // Least significant original bit rate word
        read_byte(gb, &mut two)?;
        self.bit_rate_orig = u16::from_le_bytes(two) as u32 | ((bit_rate_hi & 0x0F) << 16);

        // Least significant scaled bit rate word
        read_byte(gb, &mut two)?;
        self.bit_rate_scaled = u16::from_le_bytes(two) as u32 | ((bit_rate_hi & 0xF0) << 12);

        // Setup number of fullband channels
        self.nchannels_total =
            dca::count_chs_for_mask(self.ch_mask & !u32::from(spair::LFE1)) as usize;
        self.nchannels = self.nchannels_total.min(DCA_LBR_CHANNELS);

        // Setup band limit
        self.band_limit = match self.flags & lbr_flags::BAND_LIMIT_MASK {
            lbr_flags::BAND_LIMIT_NONE => 0,
            lbr_flags::BAND_LIMIT_1_2 => 1,
            lbr_flags::BAND_LIMIT_1_4 => 2,
            _ => return Err("unsupported LBR band limit"),
        };

        // Setup frequency range
        self.freq_range = usize::from(FF_DCA_FREQ_RANGES[sr_code as usize]);

        // Setup resolution profile
        self.res_profile = if self.bit_rate_orig >= 44000 * (self.nchannels_total as u32 + 2) {
            2
        } else if self.bit_rate_orig >= 25000 * (self.nchannels_total as u32 + 2) {
            1
        } else {
            0
        };

        // Setup limited sample rate, number of subbands, etc
        self.limited_rate = self.sample_rate >> self.band_limit;
        self.limited_range = self.freq_range as i32 - self.band_limit as i32;
        if self.limited_range < 0 {
            return Err("invalid LBR band limit for frequency range");
        }

        self.nsubbands = 8 << self.limited_range as usize;

        self.g3_avg_only_start_sb = self.nsubbands * usize::from(FF_DCA_AVG_G3_FREQS[self.res_profile])
            / (self.limited_rate / 2) as usize;
        if self.g3_avg_only_start_sb > self.nsubbands {
            self.g3_avg_only_start_sb = self.nsubbands;
        }

        self.min_mono_subband = self.nsubbands * 2000 / (self.limited_rate / 2) as usize;
        if self.min_mono_subband > self.nsubbands {
            self.min_mono_subband = self.nsubbands;
        }

        self.max_mono_subband = self.nsubbands * 14000 / (self.limited_rate / 2) as usize;
        if self.max_mono_subband > self.nsubbands {
            self.max_mono_subband = self.nsubbands;
        }

        // Handle change of sample rate
        if (old_rate != self.sample_rate || old_band_limit != self.band_limit) && self.init_sample_rate().is_err() {
            return Err("LBR sample rate init failed");
        }

        // Setup stereo downmix
        if self.flags & lbr_flags::DMIX_STEREO != 0 {
            if self.nchannels_total < 3 || self.nchannels_total > DCA_LBR_CHANNELS_TOTAL - 2 {
                return Err("invalid number of channels for LBR stereo downmix");
            }

            // Account for extra downmixed channel pair
            self.nchannels_total += 2;
            self.nchannels = 2;
            self.ch_mask = u32::from(spair::LR);
            self.flags &= !lbr_flags::FLAG_LFE_PRESENT;
        }

        // Handle change of sample rate or number of channels
        if old_rate != self.sample_rate
            || old_band_limit != self.band_limit
            || old_nchannels != self.nchannels
        {
            self.alloc_sample_buffer();
            self.lbr_flush();
        }

        Ok(())
    }
}

impl LbrDecoder {
    // ───────────── ff_dca_lbr_parse ─────────────

    pub fn lbr_parse(&mut self, data: &[u8], asset: &ExssAsset) -> Result<(), &'static str> {
        let sub = &data[asset.lbr_offset.min(data.len())..(asset.lbr_offset + asset.lbr_size).min(data.len())];
        let mut pos = 0usize;

        // LBR sync word
        if pos + 4 > sub.len() {
            return Err("truncated LBR stream");
        }
        let sync = u32::from_be_bytes(sub[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if sync != dca::DCA_SYNCWORD_LBR {
            return Err("invalid LBR sync word");
        }

        // LBR header type
        if pos + 1 > sub.len() {
            return Err("truncated LBR stream");
        }
        match sub[pos] {

            DCA_LBR_HEADER_SYNC_ONLY => {
                pos += 1;
                if self.sample_rate == 0 {
                    return Err("LBR decoder not initialized");
                }
            }
            DCA_LBR_HEADER_DECODER_INIT => {
                pos += 1;
                let rest = &sub[pos..];
                let mut cursor = std::io::Cursor::new(rest);
                if let Err(e) = self.parse_decoder_init(&mut cursor) {
                    self.sample_rate = 0;
                    return Err(e);
                }
                pos += cursor.position() as usize;
            }
            _ => return Err("invalid LBR header type"),
        }

        // LBR frame chunk header
        if pos + 1 > sub.len() {
            return Err("truncated LBR stream");
        }
        let chunk_id = sub[pos];
        pos += 1;
        let chunk_len = if chunk_id & 0x80 != 0 {
            if pos + 2 > sub.len() {
                return Err("truncated LBR stream");
            }
            let l = u16::from_be_bytes([sub[pos], sub[pos + 1]]) as usize;
            pos += 2;
            l
        } else {
            if pos + 1 > sub.len() {
                return Err("truncated LBR stream");
            }
            let l = usize::from(sub[pos]);
            pos += 1;
            l
        };

        let left = sub.len() - pos;
        let chunk_len = chunk_len.min(left);

        let frame = &sub[pos..pos + chunk_len];
        let _pos_after_frame = pos + chunk_len;

        match chunk_id & 0x7f {
            chunk::FRAME => {
                // Checksum skipped (FFmpeg default: no AV_EF_CRCCHECK)
            }
            chunk::FRAME_NO_CSUM => {}
            _ => return Err("invalid LBR frame chunk ID"),
        }

        // Clear current frame
        self.quant_levels = [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2];
        self.sb_indices = [0xff; DCA_LBR_SUBBANDS];
        self.sec_ch_sbms = [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2];
        self.sec_ch_lrms = [[0; DCA_LBR_SUBBANDS]; DCA_LBR_CHANNELS / 2];
        self.ch_pres = [0; DCA_LBR_CHANNELS];
        self.grid_1_scf = [[[0; 8]; 12]; DCA_LBR_CHANNELS];
        self.grid_2_scf = [[[0; 64]; 3]; DCA_LBR_CHANNELS];
        self.grid_3_avg = [[0; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS];
        self.grid_3_scf = [[[0; 8]; DCA_LBR_SUBBANDS - 4]; DCA_LBR_CHANNELS];
        self.grid_3_pres = [0; DCA_LBR_CHANNELS];
        self.tonal_scf = [0; 6];
        self.lfe_data = [0.0; 64];
        self.part_stereo_pres = 0;
        self.framenum = (self.framenum + 1) & 31;

        for ch in 0..self.nchannels {
            for sb in 0..self.nsubbands / 4 {
                self.part_stereo[ch][sb][0] = self.part_stereo[ch][sb][4];
                self.part_stereo[ch][sb][4] = 16;
            }
        }

        self.lpc_coeff[self.framenum & 1] = [[[[0.0; 8]; 2]; 3]; DCA_LBR_CHANNELS];

        for group in 0..5 {
            for sf in 0..1 << group {
                let sf_idx = ((self.framenum << group) + sf) & 31;
                self.tonal_bounds[group][sf_idx][0] = self.ntones;
                self.tonal_bounds[group][sf_idx][1] = self.ntones;
            }
        }

        // Parse chunk headers
        let mut chunks = Chunks::default();
        let mut fpos = 0usize;
        while fpos < frame.len() {
            let id = frame[fpos];
            fpos += 1;
            let len = if id & 0x80 != 0 {
                if fpos + 2 > frame.len() {
                    break;
                }
                let l = u16::from_be_bytes([frame[fpos], frame[fpos + 1]]) as usize;
                fpos += 2;
                l
            } else {
                if fpos + 1 > frame.len() {
                    break;
                }
                let l = usize::from(frame[fpos]);
                fpos += 1;
                l
            };
            let id = id & 0x7f;
            let len = len.min(frame.len() - fpos);
            let payload = &frame[fpos..fpos + len];
            fpos += len;

            chunks.classify(id, len, payload);
        }

        // Parse the chunks
        let mut ret: i32 = 0;
        ret |= match self.parse_lfe_chunk(&chunks.lfe) {
            Ok(()) => 0,
            Err(_) => -1,
        };
        ret |= match self.parse_tonal_chunk(&chunks.tonal) {
            Ok(()) => 0,
            Err(_) => -1,
        };
        for i in 0..5 {
            ret |= match self.parse_tonal_group(chunks.tonal_grp[i].as_ref().unwrap_or(&EMPTY_CHUNK)) {
                Ok(()) => 0,
                Err(_) => -1,
            };
        }

        for i in 0..(self.nchannels + 1) / 2 {
            let ch1 = i * 2;
            let ch2 = (ch1 + 1).min(self.nchannels - 1);

            let g1 = self.parse_grid_1_chunk(chunks.grid1[i].as_ref().unwrap_or(&EMPTY_CHUNK), ch1, ch2);
            let hr = self.parse_high_res_grid(chunks.hr_grid[i].as_ref().unwrap_or(&EMPTY_CHUNK), ch1, ch2);
            if g1.is_err() || hr.is_err() {
                ret = -1;
                continue;
            }

            // TS chunks depend on both grids. TS_2 depends on TS_1.
            let g1c = chunks.grid1[i].as_ref().unwrap_or(&EMPTY_CHUNK);
            let hrc = chunks.hr_grid[i].as_ref().unwrap_or(&EMPTY_CHUNK);
            let t1c = chunks.ts1[i].as_ref().unwrap_or(&EMPTY_CHUNK);
            if g1c.len == 0 || hrc.len == 0 || t1c.len == 0 {
                continue;
            }

            let t1 = self.parse_ts1_chunk(chunks.ts1[i].as_ref().unwrap_or(&EMPTY_CHUNK), ch1, ch2);
            let t2 = self.parse_ts2_chunk(chunks.ts2[i].as_ref().unwrap_or(&EMPTY_CHUNK), ch1, ch2);
            if t1.is_err() || t2.is_err() {
                ret = -1;
            }
        }

        if ret < 0 {
            // FFmpeg without EXPLODE continues; frame may be partly decoded.
        }

        Ok(())
    }
}

/// Chunk collector (FFmpeg's local `chunk` struct).
struct Chunks<'a> {
    lfe: LbrChunk<'a>,
    tonal: LbrChunk<'a>,
    tonal_grp: [Option<LbrChunk<'a>>; 5],
    grid1: [Option<LbrChunk<'a>>; DCA_LBR_CHANNELS / 2],
    hr_grid: [Option<LbrChunk<'a>>; DCA_LBR_CHANNELS / 2],
    ts1: [Option<LbrChunk<'a>>; DCA_LBR_CHANNELS / 2],
    ts2: [Option<LbrChunk<'a>>; DCA_LBR_CHANNELS / 2],
}

pub static EMPTY_CHUNK: LbrChunk<'static> = LbrChunk { id: 0, len: 0, data: &[] };

impl<'a> Default for Chunks<'a> {
    fn default() -> Self {
        Self {
            lfe: EMPTY_CHUNK,
            tonal: EMPTY_CHUNK,
            tonal_grp: [None; 5],
            grid1: [None; DCA_LBR_CHANNELS / 2],
            hr_grid: [None; DCA_LBR_CHANNELS / 2],
            ts1: [None; DCA_LBR_CHANNELS / 2],
            ts2: [None; DCA_LBR_CHANNELS / 2],
        }
    }
}

impl<'a> Chunks<'a> {
    fn classify(&mut self, id: u8, len: usize, data: &'a [u8]) {
        let mk = || LbrChunk { id, len, data };
        match id {
            chunk::LFE => self.lfe = mk(),
            chunk::SCF | chunk::TONAL | chunk::TONAL_SCF => self.tonal = mk(),
            i if (chunk::TONAL_GRP_1..=chunk::TONAL_GRP_5).contains(&i) => {
                self.tonal_grp[usize::from(chunk::TONAL_GRP_5 - i)] = Some(mk())
            }
            i if (chunk::TONAL_SCF_GRP_1..=chunk::TONAL_SCF_GRP_5).contains(&i) => {
                self.tonal_grp[usize::from(chunk::TONAL_SCF_GRP_5 - i)] = Some(mk())
            }
            i if (chunk::RES_GRID_LR..=chunk::RES_GRID_LR + 2).contains(&i) => {
                self.grid1[usize::from(i - chunk::RES_GRID_LR)] = Some(mk())
            }
            i if (chunk::RES_GRID_HR..=chunk::RES_GRID_HR + 2).contains(&i) => {
                self.hr_grid[usize::from(i - chunk::RES_GRID_HR)] = Some(mk())
            }
            i if (chunk::RES_TS_1..=chunk::RES_TS_1 + 2).contains(&i) => {
                self.ts1[usize::from(i - chunk::RES_TS_1)] = Some(mk())
            }
            i if (chunk::RES_TS_2..=chunk::RES_TS_2 + 2).contains(&i) => {
                self.ts2[usize::from(i - chunk::RES_TS_2)] = Some(mk())
            }
            _ => {}
        }
    }
}

impl LbrDecoder {
    // ───────────── filter_frame ─────────────

    fn decode_grid(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in 0..self.nsubbands {
                let g1_sb = usize::from(FF_DCA_SCF_TO_GRID_1[sb]);

                let g1_scf_a = self.grid_1_scf[ch][g1_sb];
                let g1_scf_b = self.grid_1_scf[ch][g1_sb + 1];

                let w1 = usize::from(FF_DCA_GRID_1_WEIGHTS[g1_sb][sb]);
                let w2 = usize::from(FF_DCA_GRID_1_WEIGHTS[g1_sb + 1][sb]);

                if sb < 4 {
                    for i in 0..8 {
                        let scf = w1 * usize::from(g1_scf_a[i]) + w2 * usize::from(g1_scf_b[i]);
                        self.high_res_scf[ch][sb][i] = (scf >> 7) as u8;
                    }
                } else {
                    let g3_scf = self.grid_3_scf[ch][sb - 4];
                    let g3_avg = i32::from(self.grid_3_avg[ch][sb - 4]);

                    for i in 0..8 {
                        let scf = w1 * usize::from(g1_scf_a[i]) + w2 * usize::from(g1_scf_b[i]);
                        self.high_res_scf[ch][sb][i] =
                            ((scf >> 7) as i32 - g3_avg - i32::from(g3_scf[i])) as u8;
                    }
                }
            }
        }
    }

    /// Fill unallocated subbands with randomness.
    fn random_ts(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in 0..self.nsubbands {
                if self.ch_pres[ch] & (1u32 << sb) != 0 {
                    continue; // Skip allocated subband
                }

                if sb < 2 {
                    // The first two subbands are always zero
                    let row = self.ts_mut(ch, sb);
                    row.iter_mut().for_each(|v| *v = 0.0);
                } else if sb < 10 {
                    let mut vals = Vec::with_capacity(DCA_LBR_TIME_SAMPLES);
                    for _ in 0..DCA_LBR_TIME_SAMPLES {
                        vals.push(self.lbr_rand(sb));
                    }
                    let row = self.ts_mut(ch, sb);
                    row.copy_from_slice(&vals[..row.len()]);
                } else {
                    // Modulate by subbands 2-5 in blocks of 8
                    for i in 0..DCA_LBR_TIME_SAMPLES / 8 {
                        let mut accum = [0f32; 8];
                        for k in 2..6 {
                            let other: Vec<f32> = self.ts(ch, k)[i * 8..i * 8 + 8].to_vec();
                            for j in 0..8 {
                                accum[j] += other[j].abs();
                            }
                        }

                        let mut vals = [0f32; 8];
                        for j in 0..8 {
                            vals[j] = (accum[j] * 0.25 + 0.5) * self.lbr_rand(sb);
                        }
                        let row = self.ts_mut(ch, sb);
                        row[i * 8..i * 8 + 8].copy_from_slice(&vals);
                    }
                }
            }
        }
    }

    fn synth_lpc(&mut self, ch1: usize, ch2: usize, sb: usize) {
        let f = self.framenum & 1;
        for ch in ch1..=ch2 {
            if self.ch_pres[ch] & (1u32 << sb) == 0 {
                continue;
            }

            let coeffs = self.lpc_coeff;
            if sb < 2 {
                {
                    let mut row = self.ts(ch, sb).to_vec();
                    predict_free(&mut row, &coeffs[f ^ 1][ch][sb][1], 16);
                    let r = self.ts_mut(ch, sb);
                    r[..16].copy_from_slice(&row[..16]);
                }
                {
                    let mut row16 = self.ts(ch, sb)[16..16 + 64].to_vec();
                    predict_free(&mut row16, &coeffs[f][ch][sb][0], 64);
                    let row = self.ts_mut(ch, sb);
                    row[16..16 + 64].copy_from_slice(&row16);
                }
                {
                    let mut row48 = self.ts(ch, sb)[80..80 + 48].to_vec();
                    predict_free(&mut row48, &coeffs[f][ch][sb][1], 48);
                    let row = self.ts_mut(ch, sb);
                    row[80..80 + 48].copy_from_slice(&row48);
                }
            } else {
                {
                    let mut row = self.ts(ch, sb).to_vec();
                    predict_free(&mut row, &coeffs[f ^ 1][ch][sb][0], 16);
                    let r = self.ts_mut(ch, sb);
                    r[..16].copy_from_slice(&row[..16]);
                }
                {
                    let mut row112 = self.ts(ch, sb)[16..16 + 112].to_vec();
                    predict_free(&mut row112, &coeffs[f][ch][sb][0], 112);
                    let row = self.ts_mut(ch, sb);
                    row[16..16 + 112].copy_from_slice(&row112);
                }
            }
        }
    }

    fn filter_ts(&mut self, ch1: usize, ch2: usize) {
        for sb in 0..self.nsubbands {
            // Scale factors
            for ch in ch1..=ch2 {
                if sb < 4 {
                    for i in 0..DCA_LBR_TIME_SAMPLES / 16 {
                        let mut scf = usize::from(self.high_res_scf[ch][sb][i]);
                        if scf > AMP_MAX {
                            scf = AMP_MAX;
                        }
                        let row = self.ts_mut(ch, sb);
                        for j in 0..16 {
                            row[i * 16 + j] *= FF_DCA_QUANT_AMP[scf];
                        }
                    }
                } else {
                    let g2_idx = usize::from(FF_DCA_SCF_TO_GRID_2[sb]);
                    for i in 0..DCA_LBR_TIME_SAMPLES / 2 {
                        let scf = usize::from(self.high_res_scf[ch][sb][i / 8])
                            .wrapping_sub(usize::from(self.grid_2_scf[ch][g2_idx][i]));
                        let scf = scf.min(AMP_MAX);
                        let row = self.ts_mut(ch, sb);
                        row[i * 2] *= FF_DCA_QUANT_AMP[scf];
                        row[i * 2 + 1] *= FF_DCA_QUANT_AMP[scf];
                    }
                }
            }

            // Mid-side stereo
            if ch1 != ch2 {
                let ch2_pres = self.ch_pres[ch2] & (1u32 << sb) != 0;

                for i in 0..DCA_LBR_TIME_SAMPLES / 16 {
                    let sbms = (self.sec_ch_sbms[ch1 / 2][sb] >> i) & 1;
                    let lrms = (self.sec_ch_lrms[ch1 / 2][sb] >> i) & 1;

                    let l_off = DCA_LBR_TIME_HISTORY + i * 16;
                    let l_row = (ch1 * self.nsubbands.max(1) + sb) * self.nchsamples_ts;
                    let r_row = (ch2 * self.nsubbands.max(1) + sb) * self.nchsamples_ts;
                    let (l_base, r_base) = (l_row + l_off, r_row + l_off);
                    if sb >= self.min_mono_subband {
                        if lrms != 0 && ch2_pres {
                            if sbms != 0 {
                                for j in 0..16 {
                                    let l = self.ts_buffer[l_base + j];
                                    let r = self.ts_buffer[r_base + j];
                                    self.ts_buffer[l_base + j] = r;
                                    self.ts_buffer[r_base + j] = -l;
                                }
                            } else {
                                for j in 0..16 {
                                    let l = self.ts_buffer[l_base + j];
                                    let r = self.ts_buffer[r_base + j];
                                    self.ts_buffer[l_base + j] = r;
                                    self.ts_buffer[r_base + j] = l;
                                }
                            }
                        } else if !ch2_pres {
                            if sbms != 0 && (self.part_stereo_pres & (1 << ch1)) != 0 {
                                for j in 0..16 {
                                    let l = self.ts_buffer[l_base + j];
                                    self.ts_buffer[r_base + j] = -l;
                                }
                            } else {
                                for j in 0..16 {
                                    let l = self.ts_buffer[l_base + j];
                                    self.ts_buffer[r_base + j] = l;
                                }
                            }
                        }
                    } else if sbms != 0 && ch2_pres {
                        for j in 0..16 {
                            let l = self.ts_buffer[l_base + j];
                            let r = self.ts_buffer[r_base + j];
                            self.ts_buffer[l_base + j] = (l + r) * 0.5;
                            self.ts_buffer[r_base + j] = (l - r) * 0.5;
                        }
                    }
                }
            }

            // Inverse prediction
            if sb < 3 {
                self.synth_lpc(ch1, ch2, sb);
            }
        }
    }

    /// Modulate by interpolated partial stereo coefficients.
    fn decode_part_stereo(&mut self, ch1: usize, ch2: usize) {
        for ch in ch1..=ch2 {
            for sb in self.min_mono_subband..self.nsubbands {
                let pt_st = self.part_stereo[ch][(sb - self.min_mono_subband) / 4];

                if self.ch_pres[ch2] & (1u32 << sb) != 0 {
                    continue;
                }

                for sf in 1..=4usize {
                    let prev = FF_DCA_ST_COEFF[usize::from(pt_st[sf - 1])];
                    let next = FF_DCA_ST_COEFF[usize::from(pt_st[sf])];

                    let base = sf * 32 - 32;
                    let row = self.ts_mut(ch, sb);
                    for i in 0..32 {
                        row[base + i] *= (32 - i) as f32 * prev + i as f32 * next;
                    }
                }
            }
        }
    }

    fn synth_tones(&mut self, ch: usize, values: &mut [f32], group: usize, group_sf: usize, synth_idx: i32) {
        if synth_idx < 0 {
            return;
        }

        let start = self.tonal_bounds[group][group_sf][0];
        let count = (self.tonal_bounds[group][group_sf][1] - start) & (DCA_LBR_TONES - 1);

        for i in 0..count {
            let t = self.tones[(start + i) & (DCA_LBR_TONES - 1)];

            if t.amp[ch] != 0 {
                let amp = FF_DCA_SYNTH_ENV[synth_idx as usize] * FF_DCA_QUANT_AMP[usize::from(t.amp[ch])];
                let cos_tab = &*COS_TAB;
                let c = amp * cos_tab[usize::from(t.phs[ch]) & 255];
                let s = amp * cos_tab[(usize::from(t.phs[ch]) + 64) & 255];
                let cf = &FF_DCA_CORR_CF[usize::from(t.f_delt)];
                let x_freq = usize::from(t.x_freq);

                // The C uses computed gotos with the fall-through chain
                // p4..p0..+5; the special cases for x_freq < 5 skip the
                // earlier taps. Emulate by applying the p4.. tail (which
                // indexes x_freq-4..x_freq+5, naturally clamped) plus the
                // x_freq-5 tap when x_freq >= 5.
                if x_freq >= 5 {
                    values[x_freq - 5] += cf[0] * -s;
                }
                apply_tone_tail(values, cf, x_freq, c, s);
            }

            let t = &mut self.tones[(start + i) & (DCA_LBR_TONES - 1)];
            t.phs[ch] = t.phs[ch].wrapping_add(t.ph_rot);
        }
    }

    fn base_func_synth(&mut self, ch: usize, values: &mut [f32], sf: usize) {
        // Tonal vs residual shift is 22 subframes
        for group in 0..5 {
            let group_sf = ((self.framenum << group) + ((sf + DCA_LBR_TIME_SAMPLES - 22) >> (5 - group))) & 31;
            let synth_idx = (((sf + DCA_LBR_TIME_SAMPLES - 22) & 31) << group) & 31;
            let synth_idx = synth_idx + (1 << group) - 1;

            self.synth_tones(ch, values, group, (group_sf.wrapping_sub(1)) & 31, 30 - synth_idx as i32);
            self.synth_tones(ch, values, group, group_sf, synth_idx as i32);
        }
    }

    fn transform_channel(&mut self, ch: usize, output: &mut [f32], out_off: &mut usize) {
        let nsubbands = self.nsubbands;
        let noutsubbands = 8 << self.freq_range;

        let mut values = [[0f32; 4]; DCA_LBR_SUBBANDS];

        for sf in 0..DCA_LBR_TIME_SAMPLES / 4 {
            // Hybrid filterbank. Rows include the 8-sample history head;
            // the window sits at data_start + sf*4, and lbr_bank reads
            // src[-4..3] relative to it.
            let n = self.nchsamples_ts;
            let data_start = DCA_LBR_TIME_HISTORY;
            let input: Vec<&[f32]> = (0..nsubbands)
                .map(|sb| {
                    let base = (ch * nsubbands + sb) * n;
                    &self.ts_buffer[base..base + n]
                })
                .collect();
            let mut bank_out = [[0f32; 4]; DCA_LBR_SUBBANDS];
            dsp::lbr_bank(&mut bank_out[..nsubbands], &input, &FF_DCA_BANK_COEFF, data_start + sf * 4, nsubbands);
            values[..nsubbands].copy_from_slice(&bank_out[..nsubbands]);

            self.base_func_synth(ch, &mut values[0], sf);

            // IMDCT (full): L = 4*noutsubbands coefficients in, 2L out.
            // The tx reads L floats from the values array (rows beyond
            // nsubbands were zeroed above).
            let out_len = noutsubbands * 4; // L
            let mut flat_in = vec![0f32; out_len];
            for (i, v) in values.iter().take(nsubbands).flat_map(|r| r.iter()).enumerate() {
                flat_in[i] = *v;
            }
            let mut flat_out = vec![0f32; out_len * 2];
            imdct_full(&flat_in, &mut flat_out);

            // Long window and overlap-add
            // vector_fmul_add(output, result[0], window, history[ch], noutsubbands*4)
            // vector_fmul_reverse(history[ch], result[noutsubbands], window, noutsubbands*4)
            let hist = self.history[ch];
            let mut tmp = vec![0f32; out_len];
            dsp::vector_fmul_add(&mut tmp, &flat_out[..out_len], &self.window[..out_len], &hist[..out_len], out_len);
            for (o, &t) in output[*out_off..*out_off + out_len].iter_mut().zip(tmp.iter()) {
                *o = t;
            }
            // result[noutsubbands] = flat_out[noutsubbands * 4 ..] = flat_out[L/2 .. L/2 + L]
            let rev_src: Vec<f32> = flat_out[out_len / 2..out_len / 2 + out_len].to_vec();
            let mut hist_new = vec![0f32; out_len];
            dsp::vector_fmul_reverse(&mut hist_new, &rev_src, &self.window[..out_len], out_len);
            self.history[ch][..out_len].copy_from_slice(&hist_new);
            *out_off += out_len;
        }

        // Update history for LPC and forward MDCT
        for sb in 0..nsubbands {
            let n = self.nchsamples_ts;
            let base = (ch * nsubbands + sb) * n;
            for k in 0..DCA_LBR_TIME_HISTORY {
                self.ts_buffer[base + k] = self.ts_buffer[base + DCA_LBR_TIME_HISTORY + k];
            }
        }
    }

    /// `ff_dca_lbr_filter_frame`. Returns (sample_rate, f32 planes in
    /// output order).
    pub fn lbr_filter_frame(&mut self) -> Result<(u32, Vec<Vec<f32>>), &'static str> {
        let ch_conf = (usize::try_from(self.ch_mask & 0x7).map_err(|_| "bad ch_mask")?) - 1;
        let has_lfe = self.flags & lbr_flags::FLAG_LFE_PRESENT != 0;
        let mut channel_mask = CHANNEL_LAYOUTS[ch_conf];
        if has_lfe {
            channel_mask |= spk_and(speaker::LFE1);
        }

        let nchannels = channel_mask.count_ones() as usize;
        let sample_rate = self.sample_rate;

        let reorder = if has_lfe { &CHANNEL_REORDER_LFE } else { &CHANNEL_REORDER_NOLFE };

        let frame_samples = 1024 << self.freq_range;
        let mut planes: Vec<Vec<f32>> = (0..nchannels).map(|_| vec![0f32; frame_samples]).collect();

        // Filter fullband channels
        for i in 0..(self.nchannels + 1) / 2 {
            let ch1 = i * 2;
            let ch2 = (ch1 + 1).min(self.nchannels - 1);

            self.decode_grid(ch1, ch2);
            self.random_ts(ch1, ch2);
            self.filter_ts(ch1, ch2);

            if ch1 != ch2 && (self.part_stereo_pres & (1 << ch1)) != 0 {
                self.decode_part_stereo(ch1, ch2);
            }

            if ch1 < nchannels {
                let slot = usize::try_from(i32::from(reorder[ch_conf][ch1])).map_err(|_| "bad reorder")?;
                let mut out_off = 0usize;
                let mut plane = std::mem::take(&mut planes[slot]);
                self.transform_channel(ch1, &mut plane, &mut out_off);
                planes[slot] = plane;
            }

            if ch1 != ch2 && ch2 < nchannels {
                let slot = usize::try_from(i32::from(reorder[ch_conf][ch2])).map_err(|_| "bad reorder")?;
                let mut out_off = 0usize;
                let mut plane = std::mem::take(&mut planes[slot]);
                self.transform_channel(ch2, &mut plane, &mut out_off);
                planes[slot] = plane;
            }
        }

        // Interpolate LFE channel
        if has_lfe {
            let slot = LFE_INDEX[ch_conf];
            let factor = 16 << self.freq_range;
            let lfe_in: Vec<f32> = self.lfe_data.to_vec();
            let mut hist = self.lfe_history;
            let mut out = vec![0f32; frame_samples];
            dsp::lfe_iir(&mut out, &lfe_in, &FF_DCA_LFE_IIR, &mut hist, factor);
            self.lfe_history = hist;
            if slot < planes.len() {
                planes[slot].copy_from_slice(&out);
            }
        }

        Ok((sample_rate, planes))
    }

    /// `ff_dca_lbr_flush`.
    pub fn lbr_flush(&mut self) {
        if self.sample_rate == 0 {
            return;
        }

        // Clear history
        self.part_stereo = [[[16; 5]; DCA_LBR_SUBBANDS / 4]; DCA_LBR_CHANNELS];
        self.lpc_coeff = [[[[[0.0; 8]; 2]; 3]; DCA_LBR_CHANNELS]; 2];
        self.history = [[0.0; DCA_LBR_SUBBANDS * 4]; DCA_LBR_CHANNELS];
        self.tonal_bounds = [[[0; 2]; 32]; 5];
        self.lfe_history = [[0.0; 2]; 5];
        self.framenum = 0;
        self.ntones = 0;

        let n = self.nchsamples_ts;
        for ch in 0..self.nchannels {
            for sb in 0..self.nsubbands {
                let base = (ch * self.nsubbands + sb) * n;
                for v in &mut self.ts_buffer[base..base + DCA_LBR_TIME_HISTORY] {
                    *v = 0.0;
                }
            }
        }
    }
}

/// `predict` (dca_lbr.c): subtract the 8-tap LPC prediction.
fn predict_free(samples: &mut [f32], coeff: &[f32], nsamples: usize) {
    for i in 0..nsamples {
        let mut res = 0f32;
        for j in 0..8 {
            res += coeff[j] * samples[i - j - 1];
        }
        samples[i] -= res;
    }
}

/// Tone coefficient tail: taps p4..p0..+5. The C walks slots x_freq-4 .. x_freq+5
/// (10 slots after the optional -5 slot), with signs alternating
/// (c, s) starting at cf[1] for p4: values[x_freq-4] += cf[1]*c;
/// values[x_freq-3] += cf[2]*s; values[x_freq-2] += cf[3]*-c;
/// values[x_freq-1] += cf[4]*-s; values[x_freq] += cf[5]*c;
/// values[x_freq+1] += cf[6]*s; values[x_freq+2] += cf[7]*-c;
/// values[x_freq+3] += cf[8]*-s; values[x_freq+4] += cf[9]*c;
/// values[x_freq+5] += cf[10]*s.
fn apply_tone_tail(values: &mut [f32], cf: &[f32; 11], x_freq: usize, c: f32, s: f32) {
    let taps: [(isize, usize, f32); 10] = [
        (-4, 1, c),
        (-3, 2, s),
        (-2, 3, -c),
        (-1, 4, -s),
        (0, 5, c),
        (1, 6, s),
        (2, 7, -c),
        (3, 8, -s),
        (4, 9, c),
        (5, 10, s),
    ];
    for (doff, cf_idx, val) in taps {
        let idx = x_freq as isize + doff;
        if idx >= 0 && (idx as usize) < values.len() {
            values[idx as usize] += cf[cf_idx] * val;
        }
    }
}


/// Full inverse MDCT (AV_TX_FULL_IMDCT, tx_template.c
/// `ff_tx_mdct_inv_full`). The tx len is L (`1 << (freq_range + 5)` =
/// `4 * noutsubbands`); a full IMDCT reads L coefficients and writes 2L
/// samples:
///   half IMDCT (len L) writes L samples at dst + L/4;
///   dst[i]          = -dst[L/2 - i - 1];
///   dst[2L - i - 1] =  dst[L/2 + i].
fn imdct_full(input: &[f32], output: &mut [f32]) {
    let len = output.len() / 2; // L
    let len2 = len >> 1; // L/2
    let len4 = len >> 2; // L/4
    debug_assert_eq!(input.len(), len, "input must be L coefficients");

    // Half IMDCT with frame length L: reads L inputs, writes L outputs.
    // ff_tx_mdct_inv reads src[0..len] (in1 forward, in2 backward from
    // len-1). The naive-equivalent output pairs are:
    //   dst[i]       =  sum_{j} cos((2j+1)(4L-2i-1)pi/4L) * src[j]
    //   dst[i+len2]  = -sum_{j} cos((2j+1)(3*2L+2i+1)pi/4L) * src[j]
    let phase = std::f64::consts::PI / (4.0 * len as f64);
    for i in 0..len2 {
        let mut sum_d = 0.0f64;
        let mut sum_u = 0.0f64;
        let i_d = phase * ((4 * len - 2 * i - 1) as f64);
        let i_u = phase * ((3 * len + 2 * i + 1) as f64);
        for &val in input.iter().take(len) {
            sum_d += i_d.cos() * val as f64;
            sum_u += i_u.cos() * val as f64;
        }
        output[len4 + i] = sum_d as f32;
        output[len4 + i + len2] = -sum_u as f32;
    }

    // Mirror (ff_tx_mdct_inv_full).
    for i in 0..len4 {
        output[i] = -output[len2 - i - 1];
        output[len - i - 1] = output[len2 + i];
    }
}

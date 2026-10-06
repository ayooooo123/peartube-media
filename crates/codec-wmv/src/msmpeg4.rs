//! MS-MPEG-4 v1/v2/v3, WMV1 and WMV2 decoding core.
//!
//! Ported from FFmpeg commit 2da55bf (LGPL-2.1-or-later):
//! `libavcodec/msmpeg4dec.c`, `msmpeg4.c`, `msmpeg4data.c`,
//! `msmpeg4_vc1_data.c`, the frame/slice driver of `h263dec.c`, the H.263
//! helpers of `h263.c` / `ituh263dec.c` (MV prediction, MCBPC/CBPY/MV VLCs),
//! `mpeg4videodec.c` (`ff_mpeg4_pred_ac`), `mpeg4video.c`
//! (`ff_mpeg4_clean_buffers`), `mpegvideo.c` (block indices, intra table
//! cleaning, qscale), `mpegvideo_dec.c` (macroblock reconstruction, dummy
//! reference frames) and `mpegvideo_motion.c` (half-pel motion
//! compensation). WMV2 specifics live in `wmv2.rs`, IntraX8 in `x8.rs`.
//!
//! Prediction state (DC/AC predictors, coded-block flags, the intra-MB map)
//! persists across pictures exactly like FFmpeg's `MpegEncContext` arrays,
//! with the same strides and border rows.

use std::sync::LazyLock;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};

use crate::bits::BitReader;
use crate::idct;
use crate::mpv::{self, mid_pred, Picture};
use crate::tables::*;
use crate::vlc::{RlTable, Vlc};
use crate::x8::IntraX8;

pub const CODEC_ID_MSMPEG4V1: &str = "msmpeg4v1";
pub const CODEC_ID_MSMPEG4V2: &str = "msmpeg4v2";
pub const CODEC_ID_MSMPEG4V3: &str = "msmpeg4v3";
pub const CODEC_ID_WMV1: &str = "wmv1";
pub const CODEC_ID_WMV2: &str = "wmv2";

pub(crate) const DC_MAX: i32 = 119;
const II_BITRATE: u32 = 128 * 1024;
const MBAC_BITRATE: u32 = 50 * 1024;
const DEFAULT_INTER_INDEX: usize = 3;
/// `ff_mpeg1_dc_scale_table` (`ff_mpeg12_dc_scale_table[0]`).
static MPEG1_DC_SCALE_TABLE: [u8; 32] = [8; 32];

pub(crate) const MB_TYPE_SKIP: u8 = 1;
pub(crate) const MB_TYPE_INTRA: u8 = 2;

pub(crate) const PICT_I: u8 = 1;
pub(crate) const PICT_P: u8 = 2;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum MsVersion {
    V1,
    V2,
    V3,
    Wmv1,
    Wmv2,
}

// ───────────────────────── static VLC tables ─────────────────────────

pub(crate) struct MsTables {
    pub rl: [RlTable; 6],
    pub mb_non_intra: [Vlc; 4],
    pub mb_i: Vlc,
    pub dc: [[Vlc; 2]; 2],
    pub mv: [Vlc; 2],
    pub v2_dc_lum: Vlc,
    pub v2_dc_chroma: Vlc,
    pub v2_intra_cbpc: Vlc,
    pub v2_mb_type: Vlc,
    pub inter_intra: Vlc,
    pub h263_intra_mcbpc: Vlc,
    pub h263_inter_mcbpc: Vlc,
    pub h263_cbpy: Vlc,
    pub h263_mv: Vlc,
}

fn pairs_u8(t: &[[u8; 2]]) -> Vec<(u32, u8)> {
    t.iter().map(|e| (e[0] as u32, e[1])).collect()
}
fn pairs_u16(t: &[[u16; 2]]) -> Vec<(u32, u8)> {
    t.iter().map(|e| (e[0] as u32, e[1] as u8)).collect()
}
fn pairs_u32(t: &[[u32; 2]]) -> Vec<(u32, u8)> {
    t.iter().map(|e| (e[0], e[1] as u8)).collect()
}

/// `init_h263_dc_for_msmpeg4`: the inverted MPEG-4 DC tables of v2.
fn v2_dc_table(tab: &[[u8; 2]; 13]) -> Vec<(u32, u8)> {
    let mut out = Vec::with_capacity(512);
    for level in -256i32..256 {
        let mut size = 0u32;
        let mut v = level.abs();
        while v != 0 {
            v >>= 1;
            size += 1;
        }
        let l = if level < 0 { (-level) ^ ((1 << size) - 1) } else { level } as u32;
        let mut uni_code = tab[size as usize][0] as u32;
        let mut uni_len = tab[size as usize][1] as u32;
        uni_code ^= (1 << uni_len) - 1;
        if size > 0 {
            uni_code <<= size;
            uni_code |= l;
            uni_len += size;
            if size > 8 {
                uni_code <<= 1;
                uni_code |= 1;
                uni_len += 1;
            }
        }
        out.push((uni_code, uni_len as u8));
    }
    out
}

pub(crate) static TABLES: LazyLock<MsTables> = LazyLock::new(|| {
    let mv = |vals: &[u16; 1100], lens: &[u8; 1100]| {
        let syms: Vec<i32> = vals.iter().map(|&v| v as i32).collect();
        let lens: Vec<i8> = lens.iter().map(|&l| l as i8).collect();
        Vlc::from_lengths(9, &lens, Some(&syms), 0)
    };
    MsTables {
        rl: [
            RlTable::new(132, 85, &TABLE0_VLC, &TABLE0_RUN, &TABLE0_LEVEL),
            RlTable::new(185, 119, &TABLE2_VLC, &TABLE2_RUN, &TABLE2_LEVEL),
            RlTable::new(102, 67, &MPEG4_INTRA_VLC, &MPEG4_INTRA_RUN, &MPEG4_INTRA_LEVEL),
            RlTable::new(148, 81, &TABLE1_VLC, &TABLE1_RUN, &TABLE1_LEVEL),
            RlTable::new(168, 99, &TABLE4_VLC, &TABLE4_RUN, &TABLE4_LEVEL),
            RlTable::new(102, 58, &INTER_VLC, &INTER_RUN, &INTER_LEVEL),
        ],
        mb_non_intra: [
            Vlc::new(9, &pairs_u32(&TABLE_MB_NON_INTRA2), None),
            Vlc::new(9, &pairs_u32(&TABLE_MB_NON_INTRA3), None),
            Vlc::new(9, &pairs_u32(&TABLE_MB_NON_INTRA4), None),
            Vlc::new(9, &pairs_u32(&TABLE_MB_NON_INTRA), None),
        ],
        mb_i: Vlc::new(9, &pairs_u16(&MSMP4_MB_I_TABLE), None),
        dc: [
            [
                Vlc::new(9, &pairs_u32(&MSMP4_DC_TABLES[0][0]), None),
                Vlc::new(9, &pairs_u32(&MSMP4_DC_TABLES[0][1]), None),
            ],
            [
                Vlc::new(9, &pairs_u32(&MSMP4_DC_TABLES[1][0]), None),
                Vlc::new(9, &pairs_u32(&MSMP4_DC_TABLES[1][1]), None),
            ],
        ],
        mv: [mv(&MSMP4_MV_TABLE0, &MSMP4_MV_TABLE0_LENS), mv(&MSMP4_MV_TABLE1, &MSMP4_MV_TABLE1_LENS)],
        v2_dc_lum: Vlc::new(9, &v2_dc_table(&MPEG4_DCTAB_LUM), None),
        v2_dc_chroma: Vlc::new(9, &v2_dc_table(&MPEG4_DCTAB_CHROM), None),
        v2_intra_cbpc: Vlc::new(3, &pairs_u8(&V2_INTRA_CBPC), None),
        v2_mb_type: Vlc::new(7, &pairs_u8(&V2_MB_TYPE), None),
        inter_intra: Vlc::new(3, &pairs_u8(&TABLE_INTER_INTRA), None),
        h263_intra_mcbpc: Vlc::new(
            6,
            &H263_INTRA_MCBPC_CODE.iter().zip(H263_INTRA_MCBPC_BITS.iter()).map(|(&c, &b)| (c as u32, b)).collect::<Vec<_>>(),
            None,
        ),
        h263_inter_mcbpc: Vlc::new(
            7,
            &H263_INTER_MCBPC_CODE.iter().zip(H263_INTER_MCBPC_BITS.iter()).map(|(&c, &b)| (c as u32, b)).collect::<Vec<_>>(),
            None,
        ),
        h263_cbpy: Vlc::new(6, &pairs_u8(&H263_CBPY_TAB), None),
        h263_mv: Vlc::new(9, &pairs_u8(&MVTAB), None),
    }
});

// ───────────────────────── decoder state ─────────────────────────

/// WMV2 picture-level and per-block state (`WMV2DecContext`).
pub(crate) struct Wmv2State {
    pub j_type_bit: bool,
    pub j_type: bool,
    pub abt_flag: bool,
    pub abt_type: usize,
    pub abt_type_table: [usize; 6],
    pub per_mb_abt: bool,
    pub per_block_abt: bool,
    pub mspel_bit: bool,
    pub cbp_table_index: usize,
    pub top_left_mv_flag: bool,
    pub per_mb_rl_bit: bool,
    pub hshift: usize,
    pub abt_block2: [[i16; 64]; 6],
}

impl Default for Wmv2State {
    fn default() -> Self {
        Wmv2State {
            j_type_bit: false,
            j_type: false,
            abt_flag: false,
            abt_type: 0,
            abt_type_table: [0; 6],
            per_mb_abt: false,
            per_block_abt: false,
            mspel_bit: false,
            cbp_table_index: 0,
            top_left_mv_flag: false,
            per_mb_rl_bit: false,
            hshift: 0,
            abt_block2: [[0; 64]; 6],
        }
    }
}

pub struct MsDecoder {
    codec_id: CodecId,
    pub(crate) ver: MsVersion,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) mb_width: usize,
    pub(crate) mb_height: usize,
    pub(crate) mb_stride: usize,
    pub(crate) b8_stride: usize,
    pub(crate) h_edge_pos: i32,
    pub(crate) v_edge_pos: i32,

    y_dc_scale_table: &'static [u8; 32],
    c_dc_scale_table: &'static [u8; 32],
    pub(crate) intra_scantable: [u8; 64],
    pub(crate) inter_scantable: [u8; 64],
    pub(crate) intra_h_scantable: [u8; 64],
    pub(crate) intra_v_scantable: [u8; 64],

    // Persistent prediction arrays (MpegEncContext layout).
    dc_val: Vec<i16>,
    ac_val: Vec<[i16; 16]>,
    pub(crate) coded_block: Vec<u8>,
    mbintra_table: Vec<u8>,
    // Current-picture tables.
    pub(crate) mb_type: Vec<u8>,
    pub(crate) qscale_table: Vec<i8>,
    motion_val: Vec<[i16; 2]>,

    pub(crate) cur: Picture,
    pub(crate) last: Option<Picture>,
    spare: Option<Picture>,
    edge_buf: Vec<u8>,
    edge_buf_c: Vec<u8>,

    pub(crate) pict_type: u8,
    pub(crate) qscale: i32,
    chroma_qscale: i32,
    y_dc_scale: i32,
    c_dc_scale: i32,
    pub(crate) no_rounding: bool,
    pub(crate) mspel: bool,
    pub(crate) first_slice_line: bool,
    resync_mb_x: usize,
    resync_mb_y: usize,
    pub(crate) mb_x: usize,
    pub(crate) mb_y: usize,
    pub(crate) slice_height: usize,
    /// `block_index[]` relative to the arrays' origin (`b8_stride + 1` into
    /// the DC/AC/coded-block buffers).
    block_index: [usize; 6],

    pub(crate) mb_intra: bool,
    pub(crate) mv: [i32; 2],
    mb_skipped: bool,
    pub(crate) ac_pred: bool,
    h263_aic_dir: i32,
    inter_intra_pred: bool,
    pub(crate) block: [[i16; 64]; 6],
    pub(crate) block_last_index: [i32; 6],
    last_dc: [i32; 3],

    pub(crate) rl_table_index: usize,
    pub(crate) rl_chroma_table_index: usize,
    pub(crate) dc_table_index: usize,
    pub(crate) mv_table_index: usize,
    use_skip_mb_code: bool,
    pub(crate) per_mb_rl_table: bool,
    pub(crate) bit_rate: u32,
    flipflop_rounding: bool,
    pub(crate) esc3_level_length: u32,
    pub(crate) esc3_run_length: u32,

    pub(crate) loop_filter: bool,
    pub(crate) w2: Wmv2State,
    pub(crate) x8: Option<IntraX8>,

    pending: Option<Frame>,
}

/// Result of a picture header parse.
pub(crate) enum Header {
    Ok,
    /// `FRAME_SKIPPED`: nothing is decoded or output.
    Skipped,
}

impl MsDecoder {
    pub fn new(params: &CodecParameters, ver: MsVersion) -> Result<Self> {
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) if w > 0 && h > 0 => (w as usize, h as usize),
            _ => return Err(Error::invalid("msmpeg4/wmv: container must supply width/height")),
        };
        if w > crate::MAX_DIM as usize || h > crate::MAX_DIM as usize || (w as u64) * (h as u64) > crate::MAX_PIXELS {
            return Err(Error::invalid("msmpeg4/wmv: frame dimensions too large"));
        }
        let id = match ver {
            MsVersion::V1 => CODEC_ID_MSMPEG4V1,
            MsVersion::V2 => CODEC_ID_MSMPEG4V2,
            MsVersion::V3 => CODEC_ID_MSMPEG4V3,
            MsVersion::Wmv1 => CODEC_ID_WMV1,
            MsVersion::Wmv2 => CODEC_ID_WMV2,
        };
        LazyLock::force(&TABLES);
        let mb_width = w.div_ceil(16);
        let mb_height = h.div_ceil(16);
        let mb_stride = mb_width + 1;
        let b8_stride = mb_width * 2 + 1;
        let y_size = b8_stride * (2 * mb_height + 1);
        let c_size = mb_stride * (mb_height + 1);
        let yc_size = y_size + 2 * c_size;
        let mb_array_size = mb_height * mb_stride;

        // ff_h263_decode_init / ff_mpv_idct_init / ff_msmpeg4_common_init.
        let (y_dc_scale_table, c_dc_scale_table): (&'static [u8; 32], &'static [u8; 32]) = match ver {
            MsVersion::V1 | MsVersion::V2 => (&MPEG1_DC_SCALE_TABLE, &MPEG1_DC_SCALE_TABLE),
            // workaround_bugs defaults to FF_BUG_AUTODETECT (non-zero).
            MsVersion::V3 => (&OLD_FF_Y_DC_SCALE_TABLE, &WMV1_C_DC_SCALE_TABLE),
            MsVersion::Wmv1 | MsVersion::Wmv2 => (&WMV1_Y_DC_SCALE_TABLE, &WMV1_C_DC_SCALE_TABLE),
        };
        let (intra, inter, hs, vs) = if ver >= MsVersion::Wmv1 {
            (WMV1_SCANTABLE[1], WMV1_SCANTABLE[0], WMV1_SCANTABLE[2], WMV1_SCANTABLE[3])
        } else {
            (ZIGZAG_DIRECT, ZIGZAG_DIRECT, ALTERNATE_HORIZONTAL_SCAN, ALTERNATE_VERTICAL_SCAN)
        };

        let mut d = MsDecoder {
            codec_id: CodecId::new(id),
            ver,
            width: w,
            height: h,
            mb_width,
            mb_height,
            mb_stride,
            b8_stride,
            h_edge_pos: (mb_width * 16) as i32,
            v_edge_pos: (mb_height * 16) as i32,
            y_dc_scale_table,
            c_dc_scale_table,
            intra_scantable: intra,
            inter_scantable: inter,
            intra_h_scantable: hs,
            intra_v_scantable: vs,
            dc_val: vec![1024; yc_size],
            ac_val: vec![[0; 16]; yc_size],
            coded_block: vec![0; y_size],
            mbintra_table: vec![0; mb_array_size],
            mb_type: vec![0; mb_array_size],
            qscale_table: vec![0; mb_array_size],
            motion_val: vec![[0; 2]; b8_stride * (2 * mb_height + 1) + 8],
            cur: Picture::new(mb_width * 16, mb_height * 16),
            last: None,
            spare: None,
            edge_buf: vec![0; 19 * 19],
            edge_buf_c: vec![0; 9 * 9],
            pict_type: 0,
            qscale: 1,
            chroma_qscale: 1,
            y_dc_scale: 8,
            c_dc_scale: 8,
            no_rounding: false,
            mspel: false,
            first_slice_line: true,
            resync_mb_x: 0,
            resync_mb_y: 0,
            mb_x: 0,
            mb_y: 0,
            slice_height: mb_height,
            block_index: [0; 6],
            mb_intra: false,
            mv: [0; 2],
            mb_skipped: false,
            ac_pred: false,
            h263_aic_dir: 0,
            inter_intra_pred: false,
            block: [[0; 64]; 6],
            block_last_index: [0; 6],
            last_dc: [128; 3],
            rl_table_index: 0,
            rl_chroma_table_index: 0,
            dc_table_index: 0,
            mv_table_index: 0,
            use_skip_mb_code: false,
            per_mb_rl_table: false,
            bit_rate: 0,
            flipflop_rounding: false,
            esc3_level_length: 0,
            esc3_run_length: 0,
            loop_filter: false,
            w2: Wmv2State::default(),
            x8: None,
            pending: None,
        };
        if ver == MsVersion::Wmv2 {
            d.wmv2_decode_ext_header(&params.extradata);
            d.x8 = Some(IntraX8::new(mb_width, mb_height));
        }
        Ok(d)
    }

    // ───────────────────────── block indices ─────────────────────────

    /// `ff_init_block_index` + `ff_update_block_index` for (mb_x, mb_y).
    fn set_block_index(&mut self) {
        let (x, y) = (self.mb_x, self.mb_y);
        let b8 = self.b8_stride;
        self.block_index[0] = b8 * (y * 2) + x * 2;
        self.block_index[1] = b8 * (y * 2) + 1 + x * 2;
        self.block_index[2] = b8 * (y * 2 + 1) + x * 2;
        self.block_index[3] = b8 * (y * 2 + 1) + 1 + x * 2;
        self.block_index[4] = self.mb_stride * (y + 1) + b8 * self.mb_height * 2 + x;
        self.block_index[5] = self.mb_stride * (y + self.mb_height + 2) + b8 * self.mb_height * 2 + x;
    }

    /// Index into `dc_val`/`ac_val`/`coded_block` of block `n` (origin at
    /// `b8_stride + 1`).
    #[inline]
    fn bidx(&self, n: usize) -> usize {
        self.b8_stride + 1 + self.block_index[n]
    }

    #[inline]
    fn block_wrap(&self, n: usize) -> usize {
        if n < 4 {
            self.b8_stride
        } else {
            self.mb_stride
        }
    }

    /// Index into `motion_val` of `block_index[0]` (one guard row of zeros).
    #[inline]
    fn mv_idx(&self) -> usize {
        self.b8_stride + 4 + self.block_index[0]
    }

    /// `ff_set_qscale`.
    pub(crate) fn set_qscale(&mut self, q: i32) {
        let q = q.clamp(1, 31);
        self.qscale = q;
        self.chroma_qscale = q;
        self.y_dc_scale = self.y_dc_scale_table[q as usize] as i32;
        self.c_dc_scale = self.c_dc_scale_table[q as usize] as i32;
    }

    // ───────────────────────── picture headers ─────────────────────────

    /// `msmpeg4_decode_picture_header`.
    fn msmpeg4_decode_picture_header(&mut self, br: &mut BitReader) -> Result<Header> {
        if br.bits_left() * 8 < (self.mb_width * self.mb_height) as i64 {
            return Err(Error::invalid("msmpeg4: frame too small"));
        }
        if self.ver == MsVersion::V1 {
            let start_code = br.read(32);
            if start_code != 0x0000_0100 {
                return Err(Error::invalid("msmpeg4: invalid startcode"));
            }
            br.skip(5);
        }
        let pict_type = br.read(2) as u8 + 1;
        if pict_type != PICT_I && pict_type != PICT_P {
            return Err(Error::invalid("msmpeg4: invalid picture type"));
        }
        let q = br.read(5) as i32;
        if q == 0 {
            return Err(Error::invalid("msmpeg4: invalid qscale"));
        }
        self.pict_type = pict_type;
        self.qscale = q;
        self.chroma_qscale = q;

        if pict_type == PICT_I {
            let code = br.read(5) as usize;
            if self.ver == MsVersion::V1 {
                if code == 0 || code > self.mb_height {
                    return Err(Error::invalid("msmpeg4: invalid slice height"));
                }
                self.slice_height = code;
            } else {
                if code < 0x17 {
                    return Err(Error::invalid("msmpeg4: invalid slice code"));
                }
                self.slice_height = self.mb_height / (code - 0x16);
            }
            match self.ver {
                MsVersion::V1 | MsVersion::V2 => {
                    self.rl_chroma_table_index = 2;
                    self.rl_table_index = 2;
                    self.dc_table_index = 0;
                }
                MsVersion::V3 => {
                    self.rl_chroma_table_index = br.decode012() as usize;
                    self.rl_table_index = br.decode012() as usize;
                    self.dc_table_index = br.read_bit() as usize;
                }
                MsVersion::Wmv1 => {
                    self.decode_ext_header(br, (2 + 5 + 5 + 17 + 7) / 8);
                    self.per_mb_rl_table = if self.bit_rate > MBAC_BITRATE { br.read_bit() != 0 } else { false };
                    if !self.per_mb_rl_table {
                        self.rl_chroma_table_index = br.decode012() as usize;
                        self.rl_table_index = br.decode012() as usize;
                    }
                    self.dc_table_index = br.read_bit() as usize;
                    self.inter_intra_pred = false;
                }
                MsVersion::Wmv2 => unreachable!("wmv2 has its own header"),
            }
            self.no_rounding = true;
        } else {
            match self.ver {
                MsVersion::V1 | MsVersion::V2 => {
                    self.use_skip_mb_code = if self.ver == MsVersion::V1 { true } else { br.read_bit() != 0 };
                    self.rl_table_index = 2;
                    self.rl_chroma_table_index = 2;
                    self.dc_table_index = 0;
                    self.mv_table_index = 0;
                }
                MsVersion::V3 => {
                    self.use_skip_mb_code = br.read_bit() != 0;
                    self.rl_table_index = br.decode012() as usize;
                    self.rl_chroma_table_index = self.rl_table_index;
                    self.dc_table_index = br.read_bit() as usize;
                    self.mv_table_index = br.read_bit() as usize;
                }
                MsVersion::Wmv1 => {
                    self.use_skip_mb_code = br.read_bit() != 0;
                    self.per_mb_rl_table = if self.bit_rate > MBAC_BITRATE { br.read_bit() != 0 } else { false };
                    if !self.per_mb_rl_table {
                        self.rl_table_index = br.decode012() as usize;
                        self.rl_chroma_table_index = self.rl_table_index;
                    }
                    self.dc_table_index = br.read_bit() as usize;
                    self.mv_table_index = br.read_bit() as usize;
                    self.inter_intra_pred = self.width * self.height < 320 * 240 && self.bit_rate <= II_BITRATE;
                }
                MsVersion::Wmv2 => unreachable!("wmv2 has its own header"),
            }
            if self.flipflop_rounding {
                self.no_rounding = !self.no_rounding;
            } else {
                self.no_rounding = false;
            }
        }
        self.esc3_level_length = 0;
        self.esc3_run_length = 0;
        Ok(Header::Ok)
    }

    /// `ff_msmpeg4_decode_ext_header`.
    fn decode_ext_header(&mut self, br: &mut BitReader, buf_size: usize) {
        let left = buf_size as i64 * 8 - br.position();
        let length = if self.ver >= MsVersion::V3 { 17 } else { 16 };
        if left >= length && left < length + 8 {
            br.skip(5);
            self.bit_rate = br.read(11) * 1024;
            self.flipflop_rounding = if self.ver >= MsVersion::V3 { br.read_bit() != 0 } else { false };
        } else if left < length + 8 {
            self.flipflop_rounding = false;
        }
    }

    // ───────────────────────── frame driver ─────────────────────────

    /// `ff_h263_decode_frame` for the MS-MPEG-4 family.
    fn decode_frame(&mut self, buf: &[u8], pts: Option<i64>) -> Result<Option<Frame>> {
        if buf.is_empty() {
            return Ok(None);
        }
        let mut br = BitReader::new(buf);
        let hdr = if self.ver == MsVersion::Wmv2 {
            self.wmv2_decode_picture_header(&mut br)?
        } else {
            self.msmpeg4_decode_picture_header(&mut br)?
        };
        if let Header::Skipped = hdr {
            return Ok(None);
        }

        // ff_mpv_frame_start: `cur` already holds a free buffer; a gray dummy
        // reference stands in when a P-picture has none.
        if self.pict_type != PICT_I && self.last.is_none() {
            let mut dummy = Picture::new(self.mb_width * 16, self.mb_height * 16);
            dummy.fill(self.width, self.height, 0x80, 0x80);
            self.last = Some(dummy);
        }
        self.mb_skipped = false;

        let mut j_type = false;
        if self.ver == MsVersion::Wmv2 {
            j_type = self.wmv2_decode_secondary_picture_header(&mut br)?;
        }

        if !j_type {
            self.mb_x = 0;
            self.mb_y = 0;
            let mut slice_ok = self.decode_slice(&mut br);
            while self.mb_y < self.mb_height {
                if self.slice_height == 0
                    || self.mb_x != 0
                    || !slice_ok
                    || self.mb_y % self.slice_height != 0
                    || br.bits_left() < 0
                {
                    break;
                }
                if self.ver < MsVersion::Wmv1 {
                    self.mpeg4_clean_buffers();
                }
                if !self.decode_slice(&mut br) {
                    slice_ok = false;
                }
            }
            if self.ver < MsVersion::Wmv1 && self.pict_type == PICT_I {
                self.decode_ext_header(&mut br, buf.len());
            }
        }

        // Output (low delay) and keep the picture as the next reference.
        let frame = self.cur.to_frame(self.width, self.height, pts);
        let w = self.mb_width * 16;
        let h = self.mb_height * 16;
        let new_cur = self.spare.take().unwrap_or_else(|| Picture::new(w, h));
        let decoded = std::mem::replace(&mut self.cur, new_cur);
        if let Some(prev) = self.last.replace(decoded) {
            self.spare = Some(prev);
        }
        Ok(Some(frame))
    }

    /// `ff_mpeg4_clean_buffers` (AC predictors of the slice start only).
    fn mpeg4_clean_buffers(&mut self) {
        let l_wrap = self.b8_stride;
        let origin = self.b8_stride + 1;
        // l_xy = (2*mb_y - 1)*l_wrap + 2*mb_x - 1 relative to the origin.
        let l_xy = (origin + (2 * self.mb_y) * l_wrap + 2 * self.mb_x) as isize - l_wrap as isize - 1;
        let c_wrap = self.mb_stride;
        let u_xy = origin + 2 * self.mb_height * l_wrap + self.mb_y * c_wrap + self.mb_x - 1;
        let v_xy = u_xy + c_wrap * (self.mb_height + 1);
        let len = self.ac_val.len();
        if l_xy >= 0 {
            let l = (l_xy as usize).min(len);
            self.ac_val[l..(l + l_wrap * 2 + 1).min(len)].fill([0; 16]);
        }
        let u = u_xy.min(len);
        self.ac_val[u..(u + c_wrap + 1).min(len)].fill([0; 16]);
        let v = v_xy.min(len);
        self.ac_val[v..(v + c_wrap + 1).min(len)].fill([0; 16]);
    }

    /// `decode_slice` of h263dec.c (MS-MPEG-4 paths). Returns false on a
    /// macroblock decode error.
    fn decode_slice(&mut self, br: &mut BitReader) -> bool {
        self.first_slice_line = true;
        self.resync_mb_x = self.mb_x;
        self.resync_mb_y = self.mb_y;
        self.set_qscale(self.qscale);

        while self.mb_y < self.mb_height {
            if self.resync_mb_y + self.slice_height == self.mb_y {
                return true;
            }
            if self.ver == MsVersion::V1 {
                self.last_dc = [128; 3];
            }
            while self.mb_x < self.mb_width {
                self.set_block_index();
                if self.resync_mb_x == self.mb_x && self.resync_mb_y + 1 == self.mb_y {
                    self.first_slice_line = false;
                }
                let ok = match self.ver {
                    MsVersion::V1 | MsVersion::V2 => self.msmpeg4v12_decode_mb(br),
                    MsVersion::V3 | MsVersion::Wmv1 => self.msmpeg4v34_decode_mb(br),
                    MsVersion::Wmv2 => self.wmv2_decode_mb(br),
                };
                let mb_xy = self.mb_y * self.mb_stride + self.mb_x;
                if !self.mb_intra {
                    if self.mbintra_table[mb_xy] != 0 {
                        self.mbintra_table[mb_xy] = 0;
                        self.clean_intra_table_entries();
                    }
                } else {
                    self.mbintra_table[mb_xy] = 1;
                }
                self.update_motion_val();
                if !ok {
                    return false;
                }
                self.reconstruct_mb();
                if self.loop_filter {
                    self.h263_loop_filter();
                }
                self.mb_x += 1;
            }
            self.mb_x = 0;
            self.mb_y += 1;
        }
        true
    }

    /// `ff_clean_intra_table_entries`.
    fn clean_intra_table_entries(&mut self) {
        let wrap = self.b8_stride;
        let xy = self.bidx(0);
        let uxy = self.bidx(4);
        let vxy = self.bidx(5);
        self.dc_val[xy] = 1024;
        self.dc_val[xy + 1] = 1024;
        self.dc_val[xy + wrap] = 1024;
        self.dc_val[xy + wrap + 1] = 1024;
        self.dc_val[uxy] = 1024;
        self.dc_val[vxy] = 1024;
        self.ac_val[xy + 1] = [0; 16];
        self.ac_val[xy + wrap] = [0; 16];
        self.ac_val[xy + wrap + 1] = [0; 16];
        self.ac_val[uxy] = [0; 16];
        self.ac_val[vxy] = [0; 16];
    }

    /// `ff_h263_update_motion_val` (16x16 only in this family).
    fn update_motion_val(&mut self) {
        let (mx, my) = if self.mb_intra { (0, 0) } else { (self.mv[0], self.mv[1]) };
        let xy = self.mv_idx();
        let wrap = self.b8_stride;
        let v = [mx as i16, my as i16];
        self.motion_val[xy] = v;
        self.motion_val[xy + 1] = v;
        self.motion_val[xy + wrap] = v;
        self.motion_val[xy + 1 + wrap] = v;
    }

    /// `ff_h263_pred_motion(s, 0, 0, ...)`.
    fn h263_pred_motion(&self) -> (i32, i32) {
        let wrap = self.b8_stride;
        let xy = self.mv_idx();
        let a = self.motion_val[xy - 1];
        if self.first_slice_line {
            if self.mb_x == self.resync_mb_x {
                (0, 0)
            } else if self.mb_x + 1 == self.resync_mb_x {
                // h263_pred is set for the whole family.
                let c = self.motion_val[xy + 2 - wrap];
                if self.mb_x == 0 {
                    (c[0] as i32, c[1] as i32)
                } else {
                    (mid_pred(a[0] as i32, 0, c[0] as i32), mid_pred(a[1] as i32, 0, c[1] as i32))
                }
            } else {
                (a[0] as i32, a[1] as i32)
            }
        } else {
            let b = self.motion_val[xy - wrap];
            let c = self.motion_val[xy + 2 - wrap];
            (
                mid_pred(a[0] as i32, b[0] as i32, c[0] as i32),
                mid_pred(a[1] as i32, b[1] as i32, c[1] as i32),
            )
        }
    }

    /// WMV2 motion predictor (`wmv2_pred_motion`).
    pub(crate) fn wmv2_pred_motion(&self, br: &mut BitReader) -> (i32, i32) {
        let wrap = self.b8_stride;
        let xy = self.mv_idx();
        let a = self.motion_val[xy - 1];
        let diff = if self.mb_x != 0 && !self.first_slice_line && !self.mspel && self.w2.top_left_mv_flag {
            let b = self.motion_val[xy - wrap];
            ((a[0] as i32 - b[0] as i32).abs()).max((a[1] as i32 - b[1] as i32).abs())
        } else {
            0
        };
        let ty = if diff >= 8 { br.read_bit() } else { 2 };
        if ty == 0 {
            (a[0] as i32, a[1] as i32)
        } else if ty == 1 {
            let b = self.motion_val[xy - wrap];
            (b[0] as i32, b[1] as i32)
        } else if self.first_slice_line {
            (a[0] as i32, a[1] as i32)
        } else {
            let b = self.motion_val[xy - wrap];
            let c = self.motion_val[xy + 2 - wrap];
            (
                mid_pred(a[0] as i32, b[0] as i32, c[0] as i32),
                mid_pred(a[1] as i32, b[1] as i32, c[1] as i32),
            )
        }
    }

    // ───────────────────────── macroblock layer ─────────────────────────

    fn skip_mb(&mut self) {
        self.mb_intra = false;
        self.block_last_index = [-1; 6];
        self.mv = [0, 0];
        self.mb_skipped = true;
    }

    /// `msmpeg4v2_decode_motion`.
    fn msmpeg4v2_decode_motion(&self, br: &mut BitReader, pred: i32) -> i32 {
        let code = TABLES.h263_mv.get(br);
        if code < 0 {
            return 0xffff;
        }
        if code == 0 {
            return pred;
        }
        let sign = br.read_bit();
        let mut val = code;
        if sign != 0 {
            val = -val;
        }
        val += pred;
        if val <= -64 {
            val += 64;
        } else if val >= 64 {
            val -= 64;
        }
        val
    }

    /// `msmpeg4v12_decode_mb`.
    fn msmpeg4v12_decode_mb(&mut self, br: &mut BitReader) -> bool {
        let t = &*TABLES;
        let mb_xy = self.mb_y * self.mb_stride + self.mb_x;
        let mut cbp: i32;
        if self.pict_type == PICT_P {
            if self.use_skip_mb_code && br.read_bit() != 0 {
                self.skip_mb();
                self.mb_type[mb_xy] = MB_TYPE_SKIP;
                return true;
            }
            let code = if self.ver == MsVersion::V2 { t.v2_mb_type.get(br) } else { t.h263_inter_mcbpc.get(br) };
            if !(0..=7).contains(&code) {
                return false;
            }
            self.mb_intra = code >> 2 != 0;
            cbp = code & 3;
        } else {
            self.mb_intra = true;
            cbp = if self.ver == MsVersion::V2 { t.v2_intra_cbpc.get(br) } else { t.h263_intra_mcbpc.get(br) };
            if !(0..=3).contains(&cbp) {
                return false;
            }
        }

        if !self.mb_intra {
            let cbpy = t.h263_cbpy.get(br);
            if cbpy < 0 {
                return false;
            }
            cbp |= cbpy << 2;
            if self.ver == MsVersion::V1 || (cbp & 3) != 3 {
                cbp ^= 0x3C;
            }
            let (px, py) = self.h263_pred_motion();
            let mx = self.msmpeg4v2_decode_motion(br, px);
            let my = self.msmpeg4v2_decode_motion(br, py);
            self.mv = [mx, my];
            self.mb_type[mb_xy] = 0;
        } else {
            if self.ver == MsVersion::V2 {
                self.ac_pred = br.read_bit() != 0;
                let v = t.h263_cbpy.get(br);
                if v < 0 {
                    return false;
                }
                cbp |= v << 2;
            } else {
                self.ac_pred = false;
                let v = t.h263_cbpy.get(br);
                if v < 0 {
                    return false;
                }
                cbp |= v << 2;
                if self.pict_type == PICT_P {
                    cbp ^= 0x3C;
                }
            }
            self.mb_type[mb_xy] = MB_TYPE_INTRA;
        }

        self.block = [[0; 64]; 6];
        for i in 0..6 {
            let coded = (cbp >> (5 - i)) & 1 != 0;
            if !self.decode_block(br, i, coded, None) {
                return false;
            }
        }
        true
    }

    /// `msmpeg4v34_decode_mb`.
    fn msmpeg4v34_decode_mb(&mut self, br: &mut BitReader) -> bool {
        let t = &*TABLES;
        let mb_xy = self.mb_y * self.mb_stride + self.mb_x;
        if br.bits_left() <= 0 {
            return false;
        }
        let mut cbp: i32;
        if self.pict_type == PICT_P {
            if self.use_skip_mb_code && br.read_bit() != 0 {
                self.skip_mb();
                self.mb_type[mb_xy] = MB_TYPE_SKIP;
                return true;
            }
            let code = t.mb_non_intra[DEFAULT_INTER_INDEX].get(br);
            self.mb_intra = (!code & 0x40) >> 6 != 0;
            cbp = code & 0x3f;
        } else {
            self.mb_intra = true;
            let code = t.mb_i.get(br);
            cbp = 0;
            for i in 0..6 {
                let mut val = (code >> (5 - i)) & 1;
                if i < 4 {
                    let (pred, idx) = self.coded_block_pred(i);
                    val ^= pred;
                    self.coded_block[idx] = val as u8;
                }
                cbp |= val << (5 - i);
            }
        }

        if !self.mb_intra {
            if self.per_mb_rl_table && cbp != 0 {
                self.rl_table_index = br.decode012() as usize;
                self.rl_chroma_table_index = self.rl_table_index;
            }
            let (mut mx, mut my) = self.h263_pred_motion();
            decode_ms_motion(br, self.mv_table_index, &mut mx, &mut my);
            self.mv = [mx, my];
            self.mb_type[mb_xy] = 0;
        } else {
            self.ac_pred = br.read_bit() != 0;
            self.mb_type[mb_xy] = MB_TYPE_INTRA;
            if self.inter_intra_pred {
                self.h263_aic_dir = t.inter_intra.get(br);
            }
            if self.per_mb_rl_table && cbp != 0 {
                self.rl_table_index = br.decode012() as usize;
                self.rl_chroma_table_index = self.rl_table_index;
            }
        }

        self.block = [[0; 64]; 6];
        for i in 0..6 {
            let coded = (cbp >> (5 - i)) & 1 != 0;
            if !self.decode_block(br, i, coded, None) {
                return false;
            }
        }
        true
    }

    /// `ff_msmpeg4_coded_block_pred`: returns (prediction, index to store).
    pub(crate) fn coded_block_pred(&self, n: usize) -> (i32, usize) {
        let xy = self.bidx(n);
        let wrap = self.b8_stride;
        let a = self.coded_block[xy - 1] as i32;
        let b = self.coded_block[xy - 1 - wrap] as i32;
        let c = self.coded_block[xy - wrap] as i32;
        let pred = if b == c { a } else { c };
        (pred, xy)
    }

    /// `ff_msmpeg4_pred_dc`: returns (prediction, direction, dc_val index).
    fn msmpeg4_pred_dc(&self, n: usize) -> (i32, i32, usize) {
        let scale = if n < 4 { self.y_dc_scale } else { self.c_dc_scale };
        let wrap = self.block_wrap(n);
        let xy = self.bidx(n);
        let mut a = self.dc_val[xy - 1] as i32;
        let mut b = self.dc_val[xy - 1 - wrap] as i32;
        let mut c = self.dc_val[xy - wrap] as i32;
        if self.first_slice_line && (n & 2) == 0 && self.ver < MsVersion::Wmv1 {
            b = 1024;
            c = 1024;
        }
        a = (a + (scale >> 1)) / scale;
        b = (b + (scale >> 1)) / scale;
        c = (c + (scale >> 1)) / scale;

        let pred;
        let dir;
        if self.ver > MsVersion::V3 {
            if self.inter_intra_pred {
                if n == 1 {
                    pred = a;
                    dir = 0;
                } else if n == 2 {
                    pred = c;
                    dir = 1;
                } else if n == 3 {
                    if (a - b).abs() < (b - c).abs() {
                        pred = c;
                        dir = 1;
                    } else {
                        pred = a;
                        dir = 0;
                    }
                } else {
                    let bs = 8usize;
                    let (plane, wrap, dest) = if n < 4 {
                        let ls = self.cur.linesize[0];
                        (0, ls, ((n >> 1) + 2 * self.mb_y) * bs * ls + ((n & 1) + 2 * self.mb_x) * bs)
                    } else {
                        let ls = self.cur.linesize[n - 3];
                        (n - 3, ls, self.mb_y * bs * ls + self.mb_x * bs)
                    };
                    let a2 = if self.mb_x == 0 {
                        (1024 + (scale >> 1)) / scale
                    } else {
                        get_dc(&self.cur.data[plane], dest - bs, wrap, scale * 8, bs)
                    };
                    let c2 = if self.mb_y == 0 {
                        (1024 + (scale >> 1)) / scale
                    } else {
                        get_dc(&self.cur.data[plane], dest - bs * wrap, wrap, scale * 8, bs)
                    };
                    if self.h263_aic_dir == 0 {
                        pred = a2;
                        dir = 0;
                    } else if self.h263_aic_dir == 1 {
                        if n == 0 {
                            pred = c2;
                            dir = 1;
                        } else {
                            pred = a2;
                            dir = 0;
                        }
                    } else if self.h263_aic_dir == 2 {
                        if n == 0 {
                            pred = a2;
                            dir = 0;
                        } else {
                            pred = c2;
                            dir = 1;
                        }
                    } else {
                        pred = c2;
                        dir = 1;
                    }
                }
            } else if (a - b).abs() < (b - c).abs() {
                pred = c;
                dir = 1;
            } else {
                pred = a;
                dir = 0;
            }
        } else if (a - b).abs() <= (b - c).abs() {
            pred = c;
            dir = 1;
        } else {
            pred = a;
            dir = 0;
        }
        (pred, dir, xy)
    }

    /// `msmpeg4_decode_dc`: returns (level, direction) or None on error.
    fn msmpeg4_decode_dc(&mut self, br: &mut BitReader, n: usize) -> Option<(i32, i32)> {
        let t = &*TABLES;
        let mut level;
        if self.ver <= MsVersion::V2 {
            level = if n < 4 { t.v2_dc_lum.get(br) } else { t.v2_dc_chroma.get(br) };
            if level < 0 {
                // "illegal dc vlc": FFmpeg returns -1 with direction 0 and
                // the caller keeps decoding.
                return Some((-1, 0));
            }
            level -= 256;
        } else {
            level = t.dc[self.dc_table_index][(n >= 4) as usize].get(br);
            if level == DC_MAX {
                level = br.read(8) as i32;
                if br.read_bit() != 0 {
                    level = -level;
                }
            } else if level != 0 && br.read_bit() != 0 {
                level = -level;
            }
        }
        if self.ver == MsVersion::V1 {
            let i = if n < 4 { 0 } else { n - 3 };
            level += self.last_dc[i];
            self.last_dc[i] = level;
            Some((level, -1))
        } else {
            let (pred, dir, xy) = self.msmpeg4_pred_dc(n);
            level += pred;
            let scale = if n < 4 { self.y_dc_scale } else { self.c_dc_scale };
            self.dc_val[xy] = level.wrapping_mul(scale) as i16;
            Some((level, dir))
        }
    }

    /// `ff_msmpeg4_decode_block`. `scan` overrides the inter scan table
    /// (WMV2 ABT); `block_n` selects the destination (`None` = `self.block[n]`).
    pub(crate) fn decode_block(&mut self, br: &mut BitReader, n: usize, coded: bool, scan: Option<&[u8; 64]>) -> bool {
        self.decode_block_into(br, n, coded, scan, false)
    }

    pub(crate) fn decode_block_into(
        &mut self,
        br: &mut BitReader,
        n: usize,
        coded: bool,
        scan: Option<&[u8; 64]>,
        abt2: bool,
    ) -> bool {
        let t = &*TABLES;
        let mut dc_pred_dir = -1;
        let qmul;
        let qadd;
        let rl: &RlTable;
        let run_diff: i32;
        let mut i: i32;
        let scan_table: [u8; 64];
        let q_rl;
        let mut block = if abt2 { self.w2.abt_block2[n] } else { self.block[n] };

        if self.mb_intra {
            qmul = 1;
            qadd = 0;
            let Some((mut level, dir)) = self.msmpeg4_decode_dc(br, n) else {
                return false;
            };
            dc_pred_dir = dir;
            if level < 0 && self.inter_intra_pred {
                level = 0;
            }
            if n < 4 {
                rl = &t.rl[self.rl_table_index];
                if level > 256 * self.y_dc_scale && !self.inter_intra_pred {
                    return false;
                }
            } else {
                rl = &t.rl[3 + self.rl_chroma_table_index];
                if level > 256 * self.c_dc_scale && !self.inter_intra_pred {
                    return false;
                }
            }
            block[0] = level as i16;
            run_diff = (self.ver >= MsVersion::Wmv1) as i32;
            i = 0;
            if !coded {
                self.mpeg4_pred_ac(&mut block, n, dc_pred_dir);
                self.block_last_index[n] = i;
                self.store_block(n, block, abt2);
                return true;
            }
            scan_table = if self.ac_pred {
                if dc_pred_dir == 0 {
                    self.intra_v_scantable
                } else {
                    self.intra_h_scantable
                }
            } else {
                self.intra_scantable
            };
            q_rl = 0;
        } else {
            qmul = self.qscale << 1;
            qadd = (self.qscale - 1) | 1;
            i = -1;
            rl = &t.rl[3 + self.rl_table_index];
            run_diff = if self.ver == MsVersion::V2 { 0 } else { 1 };
            if !coded {
                self.block_last_index[n] = i;
                return true;
            }
            scan_table = match scan {
                Some(s) => *s,
                None => self.inter_scantable,
            };
            q_rl = self.qscale as usize;
        }

        loop {
            let (mut level, mut run) = rl.get_rl(q_rl, br);
            if level == 0 {
                let cache = br.peek(32);
                if self.ver == MsVersion::V1 || cache & 0x8000_0000 == 0 {
                    if self.ver == MsVersion::V1 || cache & 0x4000_0000 == 0 {
                        // third escape
                        if self.ver != MsVersion::V1 {
                            br.skip(2);
                        }
                        let last;
                        if self.ver <= MsVersion::V3 {
                            last = br.read(1) as i32;
                            run = br.read(6) as i32;
                            level = br.read_signed(8);
                        } else {
                            last = br.read(1) as i32;
                            if self.esc3_level_length == 0 {
                                let mut ll;
                                if self.qscale < 8 {
                                    ll = br.read(3);
                                    if ll == 0 {
                                        ll = 8 + br.read(1);
                                    }
                                } else {
                                    ll = 2;
                                    while ll < 8 && br.peek(1) == 0 {
                                        ll += 1;
                                        br.skip(1);
                                    }
                                    if ll < 8 {
                                        br.skip(1);
                                    }
                                }
                                self.esc3_level_length = ll;
                                self.esc3_run_length = br.read(2) + 3;
                            }
                            run = br.read(self.esc3_run_length) as i32;
                            let sign = br.read(1);
                            level = br.read(self.esc3_level_length) as i32;
                            if sign != 0 {
                                level = -level;
                            }
                        }
                        if level > 0 {
                            level = level.wrapping_mul(qmul).wrapping_add(qadd);
                        } else {
                            level = level.wrapping_mul(qmul).wrapping_sub(qadd);
                        }
                        i += run + 1;
                        if last != 0 {
                            i += 192;
                        }
                    } else {
                        // second escape
                        br.skip(2);
                        let (l2, r2) = rl.get_rl(q_rl, br);
                        level = l2;
                        run = r2;
                        let lv = (level / qmul).clamp(0, 127) as usize;
                        i += run + rl.max_run[(run >> 7) as usize & 1][lv] as i32 + run_diff;
                        let sign = br.read(1) as i32;
                        level = (level ^ -sign) + sign;
                    }
                } else {
                    // first escape
                    br.skip(1);
                    let (l2, r2) = rl.get_rl(q_rl, br);
                    level = l2;
                    run = r2;
                    i += run;
                    let ml = rl.max_level[(run >> 7) as usize & 1][((run - 1) & 63).min(64) as usize] as i32;
                    level = level.wrapping_add(ml.wrapping_mul(qmul));
                    let sign = br.read(1) as i32;
                    level = (level ^ -sign) + sign;
                }
            } else {
                i += run;
                let sign = br.read(1) as i32;
                level = (level ^ -sign) + sign;
            }
            if i > 62 {
                i -= 192;
                if i & !63 != 0 {
                    // FFmpeg's default error recognition ignores the
                    // overflow while bits remain.
                    if br.bits_left() >= 0 {
                        i = 63;
                        break;
                    }
                    return false;
                }
                block[scan_table[i as usize] as usize] = level as i16;
                break;
            }
            if i < 0 {
                return false;
            }
            block[scan_table[i as usize] as usize] = level as i16;
        }
        if self.mb_intra {
            self.mpeg4_pred_ac(&mut block, n, dc_pred_dir);
        }
        self.block_last_index[n] = i;
        self.store_block(n, block, abt2);
        true
    }

    #[inline]
    fn store_block(&mut self, n: usize, block: [i16; 64], abt2: bool) {
        if abt2 {
            self.w2.abt_block2[n] = block;
        } else {
            self.block[n] = block;
        }
    }

    /// `ff_mpeg4_pred_ac`.
    fn mpeg4_pred_ac(&mut self, block: &mut [i16; 64], n: usize, dir: i32) {
        let xy = self.bidx(n);
        if self.ac_pred {
            if dir == 0 {
                let qxy = (self.mb_x as isize - 1 + (self.mb_y * self.mb_stride) as isize) as usize;
                let src = self.ac_val[xy - 1];
                if self.mb_x == 0 || self.qscale == self.qscale_table[qxy] as i32 || n == 1 || n == 3 {
                    for i in 1..8 {
                        block[i << 3] = block[i << 3].wrapping_add(src[i]);
                    }
                } else {
                    let qs = self.qscale_table[qxy] as i32;
                    for i in 1..8 {
                        block[i << 3] = block[i << 3].wrapping_add(rounded_div(src[i] as i32 * qs, self.qscale) as i16);
                    }
                }
            } else {
                let wrap = self.block_wrap(n);
                let src = self.ac_val[xy - wrap];
                if self.mb_y == 0
                    || self.qscale == self.qscale_table[self.mb_x + self.mb_y * self.mb_stride - self.mb_stride] as i32
                    || n == 2
                    || n == 3
                {
                    for i in 1..8 {
                        block[i] = block[i].wrapping_add(src[i + 8]);
                    }
                } else {
                    let qs = self.qscale_table[self.mb_x + self.mb_y * self.mb_stride - self.mb_stride] as i32;
                    for i in 1..8 {
                        block[i] = block[i].wrapping_add(rounded_div(src[i + 8] as i32 * qs, self.qscale) as i16);
                    }
                }
            }
        }
        let dst = &mut self.ac_val[xy];
        for i in 1..8 {
            dst[i] = block[i << 3];
        }
        for i in 1..8 {
            dst[8 + i] = block[i];
        }
    }

    // ───────────────────────── reconstruction ─────────────────────────

    /// `ff_mpv_reconstruct_mb` for the MS-MPEG-4 family.
    fn reconstruct_mb(&mut self) {
        let mb_xy = self.mb_y * self.mb_stride + self.mb_x;
        self.qscale_table[mb_xy] = self.qscale as i8;
        self.mb_skipped = false;
        let ls = self.cur.linesize[0];
        let uvls = self.cur.linesize[1];
        let dest_y = self.mb_y * 16 * ls + self.mb_x * 16;
        let dest_c = self.mb_y * 8 * uvls + self.mb_x * 8;

        if !self.mb_intra {
            if self.ver == MsVersion::Wmv2 && self.mspel {
                self.mspel_motion();
            } else {
                self.mpeg_motion();
            }
            if self.ver == MsVersion::Wmv2 {
                self.wmv2_add_mb(dest_y, dest_c);
            } else {
                for i in 0..6 {
                    if self.block_last_index[i] < 0 {
                        continue;
                    }
                    let (p, off, stride) = self.block_dest(i, dest_y, dest_c);
                    idct::simple_idct_add(&mut self.cur.data[p], off, stride, &mut self.block[i]);
                }
            }
        } else {
            for i in 0..6 {
                self.dct_unquantize_h263_intra(i);
                let (p, off, stride) = self.block_dest(i, dest_y, dest_c);
                if self.ver == MsVersion::Wmv2 {
                    idct::wmv2_idct_put(&mut self.cur.data[p], off, stride, &mut self.block[i]);
                } else {
                    idct::simple_idct_put(&mut self.cur.data[p], off, stride, &mut self.block[i]);
                }
            }
        }
    }

    #[inline]
    pub(crate) fn block_dest(&self, i: usize, dest_y: usize, dest_c: usize) -> (usize, usize, usize) {
        let ls = self.cur.linesize[0];
        match i {
            0 => (0, dest_y, ls),
            1 => (0, dest_y + 8, ls),
            2 => (0, dest_y + 8 * ls, ls),
            3 => (0, dest_y + 8 * ls + 8, ls),
            4 => (1, dest_c, self.cur.linesize[1]),
            _ => (2, dest_c, self.cur.linesize[2]),
        }
    }

    /// `dct_unquantize_h263_intra_c` (h263_aic = 0).
    fn dct_unquantize_h263_intra(&mut self, n: usize) {
        let qscale = if n < 4 { self.qscale } else { self.chroma_qscale };
        let qmul = qscale << 1;
        let qadd = (qscale - 1) | 1;
        let scale = if n < 4 { self.y_dc_scale } else { self.c_dc_scale };
        let b = &mut self.block[n];
        b[0] = (b[0] as i32).wrapping_mul(scale) as i16;
        for v in b[1..].iter_mut() {
            let level = *v as i32;
            if level != 0 {
                *v = if level < 0 {
                    level.wrapping_mul(qmul).wrapping_sub(qadd)
                } else {
                    level.wrapping_mul(qmul).wrapping_add(qadd)
                } as i16;
            }
        }
    }

    /// `mpeg_motion` (H.263 half-pel, 16x16, forward).
    fn mpeg_motion(&mut self) {
        let Some(refp) = self.last.as_ref() else { return };
        let (motion_x, motion_y) = (self.mv[0], self.mv[1]);
        let no_rnd = self.no_rounding;
        let dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as usize;
        let src_x = self.mb_x as i32 * 16 + (motion_x >> 1);
        let src_y = self.mb_y as i32 * 16 + (motion_y >> 1);
        let uvdxy = dxy | (motion_y & 2) as usize | ((motion_x & 2) >> 1) as usize;
        let uvsrc_x = src_x >> 1;
        let uvsrc_y = src_y >> 1;

        let ls = self.cur.linesize[0];
        let uvls = self.cur.linesize[1];
        let dest_y = self.mb_y * 16 * ls + self.mb_x * 16;
        let dest_c = self.mb_y * 8 * uvls + self.mb_x * 8;
        let (hep, vep) = (self.h_edge_pos, self.v_edge_pos);

        let src = mpv::src_block(&refp.data[0], refp.linesize[0], hep, vep, src_x, src_y, 17, 17, &mut self.edge_buf);
        mpv::put_hpel(&mut self.cur.data[0], dest_y, ls, &src, 16, 16, dxy, no_rnd);
        for p in 1..3 {
            let src = mpv::src_block(
                &refp.data[p],
                refp.linesize[p],
                hep >> 1,
                vep >> 1,
                uvsrc_x,
                uvsrc_y,
                9,
                9,
                &mut self.edge_buf_c,
            );
            mpv::put_hpel(&mut self.cur.data[p], dest_c, uvls, &src, 8, 8, uvdxy, no_rnd);
        }
    }

    /// `ff_h263_loop_filter`.
    fn h263_loop_filter(&mut self) {
        let ls = self.cur.linesize[0];
        let uvls = self.cur.linesize[1];
        let xy = self.mb_y * self.mb_stride + self.mb_x;
        let dy = self.mb_y * 16 * ls + self.mb_x * 16;
        let dc = self.mb_y * 8 * uvls + self.mb_x * 8;
        let is_skip = |t: u8| t & MB_TYPE_SKIP != 0;

        let qp_c = if !is_skip(self.mb_type[xy]) {
            let q = self.qscale as usize;
            mpv::h263_v_loop_filter(&mut self.cur.data[0], dy + 8 * ls, ls, q);
            mpv::h263_v_loop_filter(&mut self.cur.data[0], dy + 8 * ls + 8, ls, q);
            q
        } else {
            0
        };

        if self.mb_y != 0 {
            let qp_tt = if is_skip(self.mb_type[xy - self.mb_stride]) {
                0
            } else {
                self.qscale_table[xy - self.mb_stride] as usize
            };
            let qp_tc = if qp_c != 0 { qp_c } else { qp_tt };
            if qp_tc != 0 {
                mpv::h263_v_loop_filter(&mut self.cur.data[0], dy, ls, qp_tc);
                mpv::h263_v_loop_filter(&mut self.cur.data[0], dy + 8, ls, qp_tc);
                mpv::h263_v_loop_filter(&mut self.cur.data[1], dc, uvls, qp_tc);
                mpv::h263_v_loop_filter(&mut self.cur.data[2], dc, uvls, qp_tc);
            }
            if qp_tt != 0 {
                mpv::h263_h_loop_filter(&mut self.cur.data[0], dy - 8 * ls + 8, ls, qp_tt);
            }
            if self.mb_x != 0 {
                let qp_dt = if qp_tt != 0 || is_skip(self.mb_type[xy - 1 - self.mb_stride]) {
                    qp_tt
                } else {
                    self.qscale_table[xy - 1 - self.mb_stride] as usize
                };
                if qp_dt != 0 {
                    mpv::h263_h_loop_filter(&mut self.cur.data[0], dy - 8 * ls, ls, qp_dt);
                    mpv::h263_h_loop_filter(&mut self.cur.data[1], dc - 8 * uvls, uvls, qp_dt);
                    mpv::h263_h_loop_filter(&mut self.cur.data[2], dc - 8 * uvls, uvls, qp_dt);
                }
            }
        }

        if qp_c != 0 {
            mpv::h263_h_loop_filter(&mut self.cur.data[0], dy + 8, ls, qp_c);
            if self.mb_y + 1 == self.mb_height {
                mpv::h263_h_loop_filter(&mut self.cur.data[0], dy + 8 * ls + 8, ls, qp_c);
            }
        }

        if self.mb_x != 0 {
            let qp_lc = if qp_c != 0 || is_skip(self.mb_type[xy - 1]) {
                qp_c
            } else {
                self.qscale_table[xy - 1] as usize
            };
            if qp_lc != 0 {
                mpv::h263_h_loop_filter(&mut self.cur.data[0], dy, ls, qp_lc);
                if self.mb_y + 1 == self.mb_height {
                    mpv::h263_h_loop_filter(&mut self.cur.data[0], dy + 8 * ls, ls, qp_lc);
                    mpv::h263_h_loop_filter(&mut self.cur.data[1], dc, uvls, qp_lc);
                    mpv::h263_h_loop_filter(&mut self.cur.data[2], dc, uvls, qp_lc);
                }
            }
        }
    }

    pub(crate) fn edge_bufs(&mut self) -> (&mut Vec<u8>, &mut Vec<u8>) {
        (&mut self.edge_buf, &mut self.edge_buf_c)
    }
}

/// `ROUNDED_DIV`.
#[inline]
fn rounded_div(a: i32, b: i32) -> i32 {
    if a >= 0 {
        (a + (b >> 1)) / b
    } else {
        -((-a + (b >> 1)) / b)
    }
}

/// `get_dc` of msmpeg4.c.
fn get_dc(src: &[u8], off: usize, stride: usize, scale: i32, bs: usize) -> i32 {
    let mut sum = 0i32;
    for y in 0..bs {
        for x in 0..bs {
            sum += src[off + x + y * stride] as i32;
        }
    }
    (sum + (scale >> 1)) / scale
}

/// `ff_msmpeg4_decode_motion`.
pub(crate) fn decode_ms_motion(br: &mut BitReader, mv_table_index: usize, mx_ptr: &mut i32, my_ptr: &mut i32) {
    let sym = TABLES.mv[mv_table_index].get(br);
    let (mut mx, mut my);
    if sym != 0 {
        mx = sym >> 8;
        my = sym & 0xFF;
    } else {
        mx = br.read(6) as i32;
        my = br.read(6) as i32;
    }
    mx += *mx_ptr - 32;
    my += *my_ptr - 32;
    if mx <= -64 {
        mx += 64;
    } else if mx >= 64 {
        mx -= 64;
    }
    if my <= -64 {
        my += 64;
    } else if my >= 64 {
        my -= 64;
    }
    *mx_ptr = mx;
    *my_ptr = my;
}

impl Decoder for MsDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if let Some(f) = self.decode_frame(&packet.data, packet.pts)? {
            self.pending = Some(f);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.pending.take().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        // ff_mpeg_flush: drop the references; prediction arrays persist.
        self.pending = None;
        if let Some(p) = self.last.take() {
            self.spare = Some(p);
        }
        Ok(())
    }
}

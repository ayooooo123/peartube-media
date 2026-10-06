//! RealVideo 1.0 / 2.0 decoder: H.263 with RealNetworks' picture and slice
//! headers.
//!
//! Ported from FFmpeg libavcodec/rv10.c, with the parts of h263dec.c,
//! ituh263dec.c, h263.c, mpeg4video.c, mpegvideo.c, mpegvideo_dec.c and
//! mpegvideo_motion.c that RV10/RV20 use (commit 2da55bf);
//! LGPL-2.1-or-later.

mod dsp;
mod mb;
mod recon;
mod tables;

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock};

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};

use crate::bits::BitReader;
use crate::picture::{check_dimensions, Plane};
use crate::vlc::{RlTable, RlVlcElem, Vlc};
use tables::*;

pub(crate) const PICT_I: i32 = 1;
pub(crate) const PICT_P: i32 = 2;
pub(crate) const PICT_B: i32 = 3;

pub(crate) const MV_DIR_FORWARD: u32 = 1;
pub(crate) const MV_DIR_BACKWARD: u32 = 2;
pub(crate) const MV_DIRECT: u32 = 4;
pub(crate) const MV_TYPE_16X16: u32 = 0;
pub(crate) const MV_TYPE_8X8: u32 = 1;

// MB_TYPE_* flags from mpegutils.h.
pub(crate) const MB_TYPE_INTRA4X4: u32 = 1 << 0;
pub(crate) const MB_TYPE_16X16: u32 = 1 << 3;
pub(crate) const MB_TYPE_8X8: u32 = 1 << 6;
pub(crate) const MB_TYPE_DIRECT2: u32 = 1 << 8;
pub(crate) const MB_TYPE_CBP: u32 = 1 << 10;
pub(crate) const MB_TYPE_QUANT: u32 = 1 << 11;
pub(crate) const MB_TYPE_FORWARD_MV: u32 = 1 << 12;
pub(crate) const MB_TYPE_BACKWARD_MV: u32 = 1 << 13;
pub(crate) const MB_TYPE_BIDIR_MV: u32 = MB_TYPE_FORWARD_MV | MB_TYPE_BACKWARD_MV;
pub(crate) const MB_TYPE_SKIP: u32 = 1 << 17;
pub(crate) const MB_TYPE_ACPRED: u32 = 1 << 18;
pub(crate) const MB_TYPE_INTRA: u32 = MB_TYPE_INTRA4X4;

#[inline]
pub(crate) fn is_intra(t: u32) -> bool {
    t & 7 != 0
}

pub(crate) const SLICE_OK: i32 = 0;
pub(crate) const SLICE_ERROR: i32 = -1;
pub(crate) const SLICE_END: i32 = -2;

const H263_MV_VLC_BITS: u32 = 9;
const INTRA_MCBPC_VLC_BITS: u32 = 6;
const INTER_MCBPC_VLC_BITS: u32 = 7;
const CBPY_VLC_BITS: u32 = 6;
const TEX_VLC_BITS: u32 = 9;
const H263_MBTYPE_B_VLC_BITS: u32 = 6;
const CBPC_B_VLC_BITS: u32 = 3;
const DC_VLC_BITS: u32 = 9;

/// `h263_mb_type_b_map` (ituh263dec.c).
const H263_MB_TYPE_B_MAP: [u32; 15] = [
    MB_TYPE_DIRECT2 | MB_TYPE_BIDIR_MV,
    MB_TYPE_DIRECT2 | MB_TYPE_BIDIR_MV | MB_TYPE_CBP,
    MB_TYPE_DIRECT2 | MB_TYPE_BIDIR_MV | MB_TYPE_CBP | MB_TYPE_QUANT,
    MB_TYPE_FORWARD_MV | MB_TYPE_16X16,
    MB_TYPE_FORWARD_MV | MB_TYPE_CBP | MB_TYPE_16X16,
    MB_TYPE_FORWARD_MV | MB_TYPE_CBP | MB_TYPE_QUANT | MB_TYPE_16X16,
    MB_TYPE_BACKWARD_MV | MB_TYPE_16X16,
    MB_TYPE_BACKWARD_MV | MB_TYPE_CBP | MB_TYPE_16X16,
    MB_TYPE_BACKWARD_MV | MB_TYPE_CBP | MB_TYPE_QUANT | MB_TYPE_16X16,
    MB_TYPE_BIDIR_MV | MB_TYPE_16X16,
    MB_TYPE_BIDIR_MV | MB_TYPE_CBP | MB_TYPE_16X16,
    MB_TYPE_BIDIR_MV | MB_TYPE_CBP | MB_TYPE_QUANT | MB_TYPE_16X16,
    0, // stuffing
    MB_TYPE_INTRA4X4 | MB_TYPE_CBP,
    MB_TYPE_INTRA4X4 | MB_TYPE_CBP | MB_TYPE_QUANT,
];

/// The static VLC tables of ituh263dec.c and rv10.c.
pub(crate) struct H263Tables {
    pub intra_mcbpc: Vlc,
    pub inter_mcbpc: Vlc,
    pub cbpy: Vlc,
    pub mv: Vlc,
    pub mbtype_b: Vlc,
    pub cbpc_b: Vlc,
    pub rl_inter: Vec<RlVlcElem>,
    pub rl_intra_aic: Vec<RlVlcElem>,
    pub rv_dc_lum: Vlc,
    pub rv_dc_chrom: Vlc,
}

fn sparse(bits: u32, codes: &[(u32, u32)], syms: Option<&[u32]>) -> Vlc {
    let entries: Vec<(u32, u32, i16)> =
        codes.iter().enumerate().map(|(i, &(len, code))| (len, code, syms.map_or(i as i16, |s| s[i] as i16))).collect();
    Vlc::init_sparse(bits, &entries).expect("H.263 VLC tables are valid")
}

fn pairs(tab: &[[u8; 2]]) -> Vec<(u32, u32)> {
    // {code, length}
    tab.iter().map(|p| (p[1] as u32, p[0] as u32)).collect()
}

/// `rv10_build_vlc`.
fn rv10_build_vlc(len_count: &[u16; 15], sym_rl: &[[u8; 2]]) -> Vlc {
    let mut syms: Vec<i16> = Vec::new();
    for &[sym, len] in sym_rl {
        let mut cur = sym as u32;
        for _ in 0..=len {
            syms.push((cur & 0xFF) as i16);
            cur = cur.wrapping_sub(1);
        }
    }
    let mut lens: Vec<i8> = Vec::new();
    for (i, &count) in len_count.iter().enumerate() {
        for _ in 0..count {
            lens.push(i as i8 + 2);
        }
    }
    debug_assert_eq!(lens.len(), syms.len());
    Vlc::init_from_lengths(DC_VLC_BITS, &lens, &syms, 0).expect("RV10 DC VLC tables are valid")
}

static TABLES: LazyLock<H263Tables> = LazyLock::new(|| {
    let intra: Vec<(u32, u32)> = INTRA_MCBPC_BITS.iter().zip(INTRA_MCBPC_CODE.iter()).map(|(&b, &c)| (b as u32, c as u32)).collect();
    let inter: Vec<(u32, u32)> = INTER_MCBPC_BITS.iter().zip(INTER_MCBPC_CODE.iter()).map(|(&b, &c)| (b as u32, c as u32)).collect();
    let rl_inter = RlTable { n: 102, last: 58, table_vlc: &INTER_VLC, table_run: &INTER_RUN, table_level: &INTER_LEVEL };
    let rl_aic = RlTable { n: 102, last: 58, table_vlc: &INTRA_VLC_AIC, table_run: &INTRA_RUN_AIC, table_level: &INTRA_LEVEL_AIC };

    let mut rv_dc_lum = rv10_build_vlc(&RV_LUM_LEN_COUNT, &RV_SYM_RUN_LEN);
    // All codes beginning with 0x7F have the same length and value.
    for i in 0..1usize << (DC_VLC_BITS - 7) {
        let e = &mut rv_dc_lum.table[(0x7F << (DC_VLC_BITS - 7)) + i];
        e.sym = 255;
        e.len = 18;
    }
    let mut rv_dc_chrom = rv10_build_vlc(&RV_CHROM_LEN_COUNT, &RV_SYM_RUN_LEN[..RV_SYM_RUN_LEN.len() - 2]);
    for i in 0..1usize << (DC_VLC_BITS - 9) {
        let e = &mut rv_dc_chrom.table[(0x1FE << (DC_VLC_BITS - 9)) + i];
        e.sym = 255;
        e.len = 18;
    }
    H263Tables {
        intra_mcbpc: sparse(INTRA_MCBPC_VLC_BITS, &intra, None),
        inter_mcbpc: sparse(INTER_MCBPC_VLC_BITS, &inter, None),
        cbpy: sparse(CBPY_VLC_BITS, &pairs(&CBPY_TAB), None),
        mv: sparse(H263_MV_VLC_BITS, &pairs(&MVTAB), None),
        mbtype_b: sparse(H263_MBTYPE_B_VLC_BITS, &pairs(&MBTYPE_B_TAB), Some(&H263_MB_TYPE_B_MAP)),
        cbpc_b: sparse(CBPC_B_VLC_BITS, &pairs(&CBPC_B_TAB), None),
        rl_inter: rl_inter.build_rl_vlc(TEX_VLC_BITS),
        rl_intra_aic: rl_aic.build_rl_vlc(TEX_VLC_BITS),
        rv_dc_lum,
        rv_dc_chrom,
    }
});

pub(crate) fn tables() -> &'static H263Tables {
    &TABLES
}

/// A decoded picture with the per-macroblock data later pictures read.
pub(crate) struct MpvPicture {
    pub planes: [Plane; 3],
    pub mb_type: Vec<u32>,
    pub qscale: Vec<i8>,
    /// `motion_val[dir]`, offset by [`MV_BASE`] like FFmpeg's padded arrays.
    pub mv: [Vec<[i16; 2]>; 2],
    pub pict_type: i32,
    pub pts: Option<i64>,
}

/// FFmpeg allocates `motion_val` four entries before index 0.
pub(crate) const MV_BASE: usize = 4;

/// Picture geometry derived from the coded size (`ff_mpv_init_context_frame`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub width: usize,
    pub height: usize,
    pub mb_width: usize,
    pub mb_height: usize,
    pub mb_stride: usize,
    pub b8_stride: usize,
    pub mb_num: usize,
}

impl Geometry {
    fn new(width: usize, height: usize) -> Self {
        let mb_width = width.div_ceil(16);
        let mb_height = height.div_ceil(16);
        Geometry {
            width,
            height,
            mb_width,
            mb_height,
            mb_stride: mb_width + 1,
            b8_stride: mb_width * 2 + 1,
            mb_num: mb_width * mb_height,
        }
    }

    /// `y_size + 2 * c_size`: entries of the DC/AC prediction arrays.
    fn yc_size(&self) -> usize {
        self.b8_stride * (2 * self.mb_height + 1) + 2 * self.mb_stride * (self.mb_height + 1)
    }
}

impl MpvPicture {
    fn new(g: &Geometry, fill: u8, pict_type: i32) -> Self {
        let w = g.mb_width * 16;
        let h = g.mb_height * 16;
        let mv_len = MV_BASE + g.b8_stride * g.mb_height * 2 + 1;
        MpvPicture {
            planes: [Plane::new(w, h, fill), Plane::new(w / 2, h / 2, fill), Plane::new(w / 2, h / 2, fill)],
            mb_type: vec![0; g.mb_stride * (g.mb_height + 1)],
            qscale: vec![0; g.mb_stride * (g.mb_height + 1)],
            mv: [vec![[0; 2]; mv_len], vec![[0; 2]; mv_len]],
            pict_type,
            pts: None,
        }
    }
}

pub struct Rv1020Decoder {
    codec_id: CodecId,
    pub(crate) rv20: bool,
    extradata: Vec<u8>,
    sub_id: u32,
    orig_width: usize,
    orig_height: usize,

    // H263DecContext
    pub(crate) h263_long_vectors: bool,
    pub(crate) modified_quant: bool,
    pub(crate) loop_filter: bool,
    pub(crate) rv10_version: i32,
    pub(crate) rv10_first_dc_coded: [bool; 3],
    pub(crate) last_dc: [i32; 3],
    pub(crate) mb_num_left: i32,

    // MpegEncContext
    pub(crate) g: Geometry,
    pub(crate) low_delay: bool,
    pub(crate) obmc: bool,
    pub(crate) pict_type: i32,
    pub(crate) qscale: i32,
    pub(crate) chroma_qscale: i32,
    pub(crate) y_dc_scale: i32,
    pub(crate) c_dc_scale: i32,
    pub(crate) chroma_qscale_table: &'static [u8; 32],
    pub(crate) dc_scale_table: &'static [u8; 32],
    pub(crate) h263_aic: bool,
    pub(crate) h263_aic_dir: bool,
    pub(crate) ac_pred: bool,
    pub(crate) mb_intra: bool,
    pub(crate) mb_skipped: bool,
    pub(crate) no_rounding: bool,
    pub(crate) mb_x: usize,
    pub(crate) mb_y: usize,
    pub(crate) resync_mb_x: usize,
    pub(crate) resync_mb_y: usize,
    pub(crate) first_slice_line: bool,
    /// `block_index[0..6]` (relative to the arrays' index 0).
    pub(crate) block_index: [isize; 6],
    pub(crate) block_last_index: [i32; 6],
    pub(crate) mv_dir: u32,
    pub(crate) mv_type: u32,
    /// `mv[dir][block][xy]`.
    pub(crate) mv: [[[i32; 2]; 4]; 2],
    /// `dc_val`, indexed by `DC_BASE + block_index`.
    pub(crate) dc_val: Vec<i16>,
    pub(crate) ac_val: Vec<[i16; 16]>,
    time: i64,
    last_non_b_time: i64,
    pub(crate) pp_time: u16,
    pub(crate) pb_time: u16,
    pub(crate) direct_scale_mv: [[i16; 64]; 2],

    pub(crate) cur: Option<MpvPicture>,
    pub(crate) last: Option<Arc<MpvPicture>>,
    pub(crate) next: Option<Arc<MpvPicture>>,
    pub(crate) block: [[i16; 64]; 6],

    ready: VecDeque<Frame>,
}

/// Header outcome for a slice that FFmpeg drops without error output.
enum HeaderError {
    Invalid,
    /// "messed up order" B-frame skip (`ERROR_SKIP_FRAME`).
    Skip,
}

impl Rv1020Decoder {
    pub fn new(params: &CodecParameters, rv20: bool) -> Result<Self> {
        let extradata = crate::real_extradata(&params.extradata);
        if extradata.len() < 8 {
            return Err(Error::invalid("rv10: extradata is too small"));
        }
        let width = params.width.unwrap_or(0) as usize;
        let height = params.height.unwrap_or(0) as usize;
        check_dimensions(width, height)?;
        let sub_id = u32::from_be_bytes([extradata[4], extradata[5], extradata[6], extradata[7]]);
        let major = sub_id >> 28;
        let minor = (sub_id >> 20) & 0xFF;
        let micro = (sub_id >> 12) & 0xFF;
        let mut low_delay = true;
        let mut rv10_version = 0;
        let mut obmc = false;
        match major {
            1 => {
                rv10_version = if micro != 0 { 3 } else { 1 };
                obmc = micro == 2;
            }
            2 => {
                if minor >= 2 {
                    low_delay = false;
                }
            }
            _ => return Err(Error::unsupported(format!("rv10: unknown header {sub_id:X}"))),
        }
        let g = Geometry::new(width, height);
        let mut d = Rv1020Decoder {
            codec_id: CodecId::new(if rv20 { "rv20" } else { "rv10" }),
            rv20,
            h263_long_vectors: extradata[3] & 1 != 0,
            extradata,
            sub_id,
            orig_width: width,
            orig_height: height,
            modified_quant: rv20,
            loop_filter: false,
            rv10_version,
            rv10_first_dc_coded: [false; 3],
            last_dc: [0; 3],
            mb_num_left: 0,
            g,
            low_delay,
            obmc,
            pict_type: 0,
            qscale: 0,
            chroma_qscale: 0,
            y_dc_scale: 0,
            c_dc_scale: 0,
            chroma_qscale_table: if rv20 { &H263_CHROMA_QSCALE_TABLE } else { &DEFAULT_CHROMA_QSCALE_TABLE },
            dc_scale_table: &MPEG1_DC_SCALE_TABLE,
            h263_aic: false,
            h263_aic_dir: false,
            ac_pred: false,
            mb_intra: false,
            mb_skipped: false,
            no_rounding: false,
            mb_x: 0,
            mb_y: 0,
            resync_mb_x: 0,
            resync_mb_y: 0,
            first_slice_line: false,
            block_index: [0; 6],
            block_last_index: [0; 6],
            mv_dir: 0,
            mv_type: 0,
            mv: [[[0; 2]; 4]; 2],
            dc_val: Vec::new(),
            ac_val: Vec::new(),
            time: 0,
            last_non_b_time: 0,
            pp_time: 0,
            pb_time: 0,
            direct_scale_mv: [[0; 64]; 2],
            cur: None,
            last: None,
            next: None,
            block: [[0; 64]; 6],
            ready: VecDeque::new(),
        };
        d.common_init(width, height);
        LazyLock::force(&TABLES);
        Ok(d)
    }

    /// `ff_mpv_common_init` for the coded size: per-context arrays, no
    /// pictures.
    fn common_init(&mut self, width: usize, height: usize) {
        self.g = Geometry::new(width, height);
        let n = self.g.yc_size();
        self.dc_val = vec![1024; n];
        self.ac_val = vec![[0; 16]; n];
        self.cur = None;
        self.last = None;
        self.next = None;
    }

    /// Offset of `dc_val`/`ac_val` index 0 in the arrays.
    #[inline]
    pub(crate) fn dc_base(&self) -> usize {
        self.g.b8_stride + 1
    }

    /// `ff_set_qscale`.
    pub(crate) fn set_qscale(&mut self, qscale: i32) {
        let qscale = qscale.clamp(1, 31);
        self.qscale = qscale;
        self.chroma_qscale = self.chroma_qscale_table[qscale as usize] as i32;
        self.y_dc_scale = self.dc_scale_table[qscale as usize] as i32;
        self.c_dc_scale = self.dc_scale_table[self.chroma_qscale as usize] as i32;
    }

    /// `ff_init_block_index` (the destination pointers are derived from
    /// `mb_x`/`mb_y` where needed).
    pub(crate) fn init_block_index(&mut self) {
        let g = self.g;
        let (mb_x, mb_y) = (self.mb_x as isize, self.mb_y as isize);
        let b8 = g.b8_stride as isize;
        let mbs = g.mb_stride as isize;
        let mbh = g.mb_height as isize;
        self.block_index[0] = b8 * (mb_y * 2) - 2 + mb_x * 2;
        self.block_index[1] = b8 * (mb_y * 2) - 1 + mb_x * 2;
        self.block_index[2] = b8 * (mb_y * 2 + 1) - 2 + mb_x * 2;
        self.block_index[3] = b8 * (mb_y * 2 + 1) - 1 + mb_x * 2;
        self.block_index[4] = mbs * (mb_y + 1) + b8 * mbh * 2 + mb_x - 1;
        self.block_index[5] = mbs * (mb_y + mbh + 2) + b8 * mbh * 2 + mb_x - 1;
    }

    /// `ff_update_block_index`.
    pub(crate) fn update_block_index(&mut self) {
        for i in 0..4 {
            self.block_index[i] += 2;
        }
        self.block_index[4] += 1;
        self.block_index[5] += 1;
    }

    /// `rv10_decode_picture_header`: the macroblock count, or an error.
    fn rv10_decode_picture_header(&mut self, gb: &mut BitReader) -> std::result::Result<i32, HeaderError> {
        let _marker = gb.get_bits1();
        self.pict_type = if gb.get_bits1() != 0 { PICT_P } else { PICT_I };
        let pb_frame = gb.get_bits1();
        if pb_frame != 0 {
            return Err(HeaderError::Invalid);
        }
        self.qscale = gb.get_bits(5) as i32;
        if self.qscale == 0 {
            return Err(HeaderError::Invalid);
        }
        if self.pict_type == PICT_I && self.rv10_version == 3 {
            // Specific MPEG-like DC coding not used.
            self.last_dc[0] = gb.get_bits(8) as i32;
            self.last_dc[1] = gb.get_bits(8) as i32;
            self.last_dc[2] = gb.get_bits(8) as i32;
        }
        // With several packets per frame the macroblock position is coded.
        let mb_xy = self.mb_x + self.mb_y * self.g.mb_width;
        let mb_count;
        if gb.show_bits(12) == 0 || (mb_xy != 0 && mb_xy < self.g.mb_num) {
            self.mb_x = gb.get_bits(6) as usize;
            self.mb_y = gb.get_bits(6) as usize;
            mb_count = gb.get_bits(12) as i32;
        } else {
            self.mb_x = 0;
            self.mb_y = 0;
            mb_count = self.g.mb_num as i32;
        }
        gb.skip_bits(3);
        Ok(mb_count)
    }

    /// `ff_h263_decode_mba`.
    fn decode_mba(&mut self, gb: &mut BitReader) -> usize {
        let mut i = 0;
        while i < 6 {
            if self.g.mb_num as i64 - 1 <= MBA_MAX[i] as i64 {
                break;
            }
            i += 1;
        }
        let mb_pos = gb.get_bits(MBA_LENGTH[i] as u32) as usize;
        self.mb_x = mb_pos % self.g.mb_width;
        self.mb_y = mb_pos / self.g.mb_width;
        mb_pos
    }

    /// `ff_mpeg4_init_direct_mv`.
    fn init_direct_mv(&mut self) {
        let pp = self.pp_time as i32;
        let pb = self.pb_time as i32;
        if pp == 0 {
            return;
        }
        for i in 0..64 {
            self.direct_scale_mv[0][i] = ((i as i32 - 32) * pb / pp) as i16;
            self.direct_scale_mv[1][i] = ((i as i32 - 32) * (pb - pp) / pp) as i16;
        }
    }

    /// `rv20_decode_picture_header`.
    fn rv20_decode_picture_header(&mut self, gb: &mut BitReader, whole_size: usize) -> std::result::Result<i32, HeaderError> {
        const PICT_TYPES: [i32; 4] = [PICT_I, PICT_I, PICT_P, PICT_B];
        self.pict_type = PICT_TYPES[gb.get_bits(2) as usize];
        if self.low_delay && self.pict_type == PICT_B {
            return Err(HeaderError::Invalid);
        }
        if self.last.is_none() && self.pict_type == PICT_B {
            // "early B-frame"
            return Err(HeaderError::Skip);
        }
        if gb.get_bits1() != 0 {
            return Err(HeaderError::Invalid);
        }
        self.qscale = gb.get_bits(5) as i32;
        if self.qscale == 0 {
            return Err(HeaderError::Invalid);
        }
        let minor = (self.sub_id >> 20) & 0xFF;
        if minor >= 2 {
            self.loop_filter = gb.get_bits1() != 0;
        }
        let mut seq: i32 = if minor <= 1 { (gb.get_bits(8) << 7) as i32 } else { (gb.get_bits(13) << 2) as i32 };

        let rpr_max = (self.extradata[1] & 7) as u32;
        if rpr_max != 0 {
            let rpr_bits = 32 - rpr_max.leading_zeros();
            let f = gb.get_bits(rpr_bits) as usize;
            let (new_w, new_h) = if f != 0 {
                if self.extradata.len() < 8 + 2 * f {
                    return Err(HeaderError::Invalid);
                }
                (4 * self.extradata[6 + 2 * f] as usize, 4 * self.extradata[7 + 2 * f] as usize)
            } else {
                (self.orig_width, self.orig_height)
            };
            if new_w != self.g.width || new_h != self.g.height {
                if check_dimensions(new_w, new_h).is_err() {
                    return Err(HeaderError::Invalid);
                }
                if whole_size < new_w.div_ceil(16) * new_h.div_ceil(16) / 8 {
                    return Err(HeaderError::Invalid);
                }
                self.common_init(new_w, new_h);
            }
        }
        if check_dimensions(self.g.width, self.g.height).is_err() {
            return Err(HeaderError::Invalid);
        }

        let mb_pos = self.decode_mba(gb) as i32;

        seq |= (self.time & !0x7FFF) as i32;
        if (seq as i64) - self.time > 0x4000 {
            seq = seq.wrapping_sub(0x8000);
        }
        if (seq as i64) - self.time < -0x4000 {
            seq = seq.wrapping_add(0x8000);
        }
        if seq as i64 != self.time {
            if self.pict_type != PICT_B {
                self.time = seq as i64;
                self.pp_time = (self.time - self.last_non_b_time) as u16;
                self.last_non_b_time = self.time;
            } else {
                self.time = seq as i64;
                self.pb_time = (self.pp_time as i64 - (self.last_non_b_time - self.time)) as u16;
            }
        }
        if self.pict_type == PICT_B {
            let pp = self.pp_time as i32;
            let pb = self.pb_time as i32;
            if pp <= pb || pp <= pp - pb || pp <= 0 {
                // "messed up order, possible from seeking? skipping current B-frame"
                return Err(HeaderError::Skip);
            }
            self.init_direct_mv();
        }

        self.no_rounding = gb.get_bits1() != 0;
        if minor <= 1 && self.pict_type == PICT_B {
            // The binary decoder reads 3+2 bits here but they seem unused.
            gb.skip_bits(5);
        }
        self.h263_aic = self.pict_type == PICT_I;
        self.dc_scale_table = if self.h263_aic { &AIC_DC_SCALE_TABLE } else { &MPEG1_DC_SCALE_TABLE };
        self.loop_filter = true;
        Ok(self.g.mb_num as i32 - mb_pos)
    }

    /// `ff_mpv_frame_start` (+ dummy reference frames).
    fn frame_start(&mut self, pts: Option<i64>) {
        let mut pic = MpvPicture::new(&self.g, 0, self.pict_type);
        pic.pts = pts;
        if self.pict_type != PICT_B {
            self.last = self.next.take();
        }
        self.cur = Some(pic);
        if self.last.is_none() && self.pict_type != PICT_I {
            self.last = Some(Arc::new(MpvPicture::new(&self.g, 0x80, PICT_I)));
        }
        if self.next.is_none() && self.pict_type == PICT_B {
            self.next = Some(Arc::new(MpvPicture::new(&self.g, 0, PICT_I)));
        }
    }

    /// `rv10_decode_packet`: decodes one slice; returns the active bit size.
    fn decode_slice(&mut self, data: &[u8], size: usize, size2: usize, whole_size: usize, pts: Option<i64>) -> Result<usize> {
        let mut gb = BitReader::new(data, size.max(size2));
        let mut active_bits_size = size * 8;
        let header = if self.rv20 { self.rv20_decode_picture_header(&mut gb, whole_size) } else { self.rv10_decode_picture_header(&mut gb) };
        let mb_count = match header {
            Ok(n) if n >= 0 => n,
            Err(HeaderError::Skip) => return Err(Error::NeedMore),
            _ => return Err(Error::invalid("rv10: header error")),
        };
        let g = self.g;
        if self.mb_x >= g.mb_width || self.mb_y >= g.mb_height {
            return Err(Error::invalid("rv10: slice position error"));
        }
        let mb_pos = self.mb_y * g.mb_width + self.mb_x;
        let left = (g.mb_num - mb_pos) as i32;
        if mb_count > left {
            return Err(Error::invalid("rv10: macroblock count error"));
        }
        if whole_size < g.mb_width * g.mb_height / 8 {
            return Err(Error::invalid("rv10: packet too small"));
        }

        if (self.mb_x == 0 && self.mb_y == 0) || self.cur.is_none() {
            if self.cur.is_some() {
                self.abandon_current();
                self.mb_x = 0;
                self.mb_y = 0;
                self.resync_mb_x = 0;
                self.resync_mb_y = 0;
            }
            self.frame_start(pts);
        } else if self.cur.as_ref().is_some_and(|c| c.pict_type != self.pict_type) {
            return Err(Error::invalid("rv10: slice type mismatch"));
        }

        if !self.rv20 {
            if self.mb_y == 0 {
                self.first_slice_line = true;
            }
        } else {
            self.first_slice_line = true;
            self.resync_mb_x = self.mb_x;
        }
        self.resync_mb_y = self.mb_y;
        self.set_qscale(self.qscale);
        self.rv10_first_dc_coded = [false; 3];
        self.init_block_index();

        self.mb_num_left = mb_count;
        while self.mb_num_left > 0 {
            self.update_block_index();
            self.mv_dir = MV_DIR_FORWARD;
            self.mv_type = MV_TYPE_16X16;
            let mut ret = self.decode_mb(&mut gb);

            // Repeat the slice end check of ff_h263_decode_mb with the
            // active bitstream size.
            if ret != SLICE_ERROR && active_bits_size as u32 >= gb.bits_count() {
                let mut v = gb.show_bits(16);
                if gb.bits_count() + 16 > active_bits_size as u32 {
                    v >>= gb.bits_count() + 16 - active_bits_size as u32;
                }
                if v == 0 {
                    ret = SLICE_END;
                }
            }
            if ret != SLICE_ERROR && (active_bits_size as u32) < gb.bits_count() && 8 * size2 as u32 >= gb.bits_count() {
                active_bits_size = size2 * 8;
                ret = SLICE_OK;
            }
            if ret == SLICE_ERROR || (active_bits_size as u32) < gb.bits_count() {
                return Err(Error::invalid("rv10: macroblock decode error"));
            }
            if self.pict_type != PICT_B {
                self.update_motion_val();
            }
            self.reconstruct_mb();
            if self.loop_filter {
                self.h263_loop_filter();
            }
            self.mb_x += 1;
            if self.mb_x == g.mb_width {
                self.mb_x = 0;
                self.mb_y += 1;
                self.init_block_index();
            }
            if self.mb_x == self.resync_mb_x {
                self.first_slice_line = false;
            }
            if ret == SLICE_END {
                break;
            }
            self.mb_num_left -= 1;
        }
        Ok(active_bits_size)
    }

    /// Drops the picture being decoded when a new one starts before it was
    /// finished; a reference picture stays the next reference, as in FFmpeg.
    fn abandon_current(&mut self) {
        if let Some(cur) = self.cur.take() {
            if cur.pict_type != PICT_B {
                self.next = Some(Arc::new(cur));
            }
        }
    }

    /// `rv10_decode_frame` for a non-empty packet.
    fn decode_frame(&mut self, pkt: &[u8], pts: Option<i64>) -> Result<()> {
        let slice_count = pkt[0] as usize + 1;
        let buf = &pkt[1..];
        if buf.len() <= 8 * slice_count {
            return Err(Error::invalid("rv10: invalid slice count"));
        }
        let hdr = &buf[..8 * slice_count];
        let data = &buf[8 * slice_count..];
        let buf_size = data.len() as i64;
        let offset_of = |n: usize| -> i64 {
            let o = 4 + 8 * n;
            u32::from_le_bytes([hdr[o], hdr[o + 1], hdr[o + 2], hdr[o + 3]]) as i64
        };
        let mut i = 0;
        while i < slice_count {
            let offset = offset_of(i);
            if offset >= buf_size {
                return Err(Error::invalid("rv10: slice offset out of range"));
            }
            let size = if i + 1 == slice_count { buf_size - offset } else { (offset_of(i + 1) as i32 as i64) - offset };
            let size2 = if i + 2 >= slice_count { buf_size - offset } else { (offset_of(i + 2) as i32 as i64) - offset };
            if size <= 0 || size2 <= 0 || offset + size.max(size2) > buf_size {
                return Err(Error::invalid("rv10: invalid slice size"));
            }
            let (offset, size, size2) = (offset as usize, size as usize, size2 as usize);
            match self.decode_slice(&data[offset..], size, size2, data.len(), pts) {
                Ok(active) => {
                    if active > 8 * size {
                        i += 1;
                    }
                }
                // A B-frame FFmpeg skips on purpose produces no output.
                Err(Error::NeedMore) => return Ok(()),
                Err(e) => return Err(e),
            }
            i += 1;
        }

        if self.cur.is_some() && self.mb_y >= self.g.mb_height {
            let cur = self.cur.take().unwrap();
            if cur.pict_type == PICT_B || self.low_delay {
                let f = self.output_frame(&cur);
                if self.last.is_some() || self.low_delay {
                    self.ready.push_back(f);
                }
            } else if let Some(last) = &self.last {
                let f = self.output_frame(last);
                self.ready.push_back(f);
            }
            if cur.pict_type != PICT_B {
                self.next = Some(Arc::new(cur));
            }
        }
        Ok(())
    }

    fn output_frame(&self, pic: &MpvPicture) -> Frame {
        crate::picture::yuv420_frame(&pic.planes, self.g.width, self.g.height, pic.pts)
    }
}

impl Decoder for Rv1020Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, pkt: &Packet) -> Result<()> {
        if pkt.data.is_empty() {
            return Ok(());
        }
        self.decode_frame(&pkt.data, pkt.pts)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.ready.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        // rv10_decode_frame outputs nothing for the empty drain packet: the
        // last reference picture of a stream with B-frames is never shown.
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // ff_mpeg_flush
        self.cur = None;
        self.last = None;
        self.next = None;
        self.ready.clear();
        self.mb_x = 0;
        self.mb_y = 0;
        Ok(())
    }
}

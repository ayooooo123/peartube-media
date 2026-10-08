//! RealVideo 3.0 / 4.0 decoder core.
//!
//! Ported from FFmpeg libavcodec/rv34.c (with the picture bookkeeping of
//! mpegvideo_dec.c that RV30/RV40 rely on) at commit 2da55bf;
//! LGPL-2.1-or-later. The RV30- and RV40-specific parts live in
//! [`rv30`] and [`rv40`].

mod data;
mod dsp;
mod pred;
mod rv30;
mod rv40;
mod rv40vlc2;
mod vlc_tables;

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock};

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result};

use crate::bits::{mid_pred, BitReader, INVALID_VLC};
use crate::picture::{check_dimensions, Plane};
use crate::vlc::Vlc;
use data::*;
use dsp::{McDst, McSrc};
use pred::*;
use vlc_tables::*;

// MB_TYPE_* flags from mpegutils.h.
pub(crate) const MB_TYPE_INTRA4X4: u32 = 1 << 0;
pub(crate) const MB_TYPE_INTRA16X16: u32 = 1 << 1;
pub(crate) const MB_TYPE_16X16: u32 = 1 << 3;
pub(crate) const MB_TYPE_16X8: u32 = 1 << 4;
pub(crate) const MB_TYPE_8X16: u32 = 1 << 5;
pub(crate) const MB_TYPE_8X8: u32 = 1 << 6;
pub(crate) const MB_TYPE_DIRECT2: u32 = 1 << 8;
pub(crate) const MB_TYPE_FORWARD_MV: u32 = 1 << 12;
pub(crate) const MB_TYPE_BACKWARD_MV: u32 = 1 << 13;
pub(crate) const MB_TYPE_BIDIR_MV: u32 = MB_TYPE_FORWARD_MV | MB_TYPE_BACKWARD_MV;
pub(crate) const MB_TYPE_SKIP: u32 = 1 << 17;
pub(crate) const MB_TYPE_INTRA: u32 = MB_TYPE_INTRA4X4;
pub(crate) const MB_TYPE_SEPARATE_DC: u32 = 0x0100_0000;

#[inline]
pub(crate) fn is_intra(t: u32) -> bool {
    t & 7 != 0
}

// RV34 macroblock types (enum RV40BlockTypes).
pub(crate) const RV34_MB_TYPE_INTRA: usize = 0;
pub(crate) const RV34_MB_TYPE_INTRA16X16: usize = 1;
pub(crate) const RV34_MB_P_16X16: usize = 2;
pub(crate) const RV34_MB_P_8X8: usize = 3;
pub(crate) const RV34_MB_B_FORWARD: usize = 4;
pub(crate) const RV34_MB_B_BACKWARD: usize = 5;
pub(crate) const RV34_MB_SKIP: usize = 6;
pub(crate) const RV34_MB_B_DIRECT: usize = 7;
pub(crate) const RV34_MB_P_16X8: usize = 8;
pub(crate) const RV34_MB_P_8X16: usize = 9;
pub(crate) const RV34_MB_B_BIDIR: usize = 10;
pub(crate) const RV34_MB_P_MIX16X16: usize = 11;
pub(crate) const RV34_MB_TYPES: usize = 12;

const RV34_MB_TYPE_TO_LAVC: [u32; 12] = [
    MB_TYPE_INTRA,
    MB_TYPE_INTRA16X16 | MB_TYPE_SEPARATE_DC,
    MB_TYPE_16X16 | MB_TYPE_FORWARD_MV,
    MB_TYPE_8X8 | MB_TYPE_FORWARD_MV,
    MB_TYPE_16X16 | MB_TYPE_FORWARD_MV,
    MB_TYPE_16X16 | MB_TYPE_BACKWARD_MV,
    MB_TYPE_SKIP,
    MB_TYPE_DIRECT2 | MB_TYPE_16X16,
    MB_TYPE_16X8 | MB_TYPE_FORWARD_MV,
    MB_TYPE_8X16 | MB_TYPE_FORWARD_MV,
    MB_TYPE_16X16 | MB_TYPE_BIDIR_MV,
    MB_TYPE_16X16 | MB_TYPE_FORWARD_MV | MB_TYPE_SEPARATE_DC,
];

pub(crate) const PICT_I: i32 = 1;
pub(crate) const PICT_P: i32 = 2;
pub(crate) const PICT_B: i32 = 3;

/// Macroblock partition width in 8x8 blocks.
const PART_SIZES_W: [i32; RV34_MB_TYPES] = [2, 2, 2, 1, 2, 2, 2, 2, 2, 1, 2, 2];
/// Macroblock partition height in 8x8 blocks.
const PART_SIZES_H: [i32; RV34_MB_TYPES] = [2, 2, 2, 1, 2, 2, 2, 2, 1, 2, 2, 2];
/// Availability index for subblocks.
const AVAIL_INDEXES: [usize; 4] = [6, 7, 10, 11];
/// Number of motion vectors in each macroblock type.
const NUM_MVS: [usize; RV34_MB_TYPES] = [0, 0, 1, 4, 1, 1, 0, 0, 2, 2, 2, 1];
const CHROMA_COEFFS: [i32; 3] = [0, 3, 5];
/// RV30/40 intra 4x4 types -> H.264 types.
const ITTRANS: [usize; 9] =
    [DC_PRED, VERT_PRED, HOR_PRED, DIAG_DOWN_RIGHT_PRED, DIAG_DOWN_LEFT_PRED, VERT_RIGHT_PRED, VERT_LEFT_PRED, HOR_UP_PRED, HOR_DOWN_PRED];
/// RV30/40 intra 16x16 types -> H.264 types.
const ITTRANS16: [usize; 4] = [DC_PRED8X8, VERT_PRED8X8, HOR_PRED8X8, PLANE_PRED8X8];

/// One VLC set (`RV34VLC`).
pub(crate) struct Rv34Vlc {
    cbppattern: Vec<Vlc>,
    /// `cbp[table][ones]`.
    cbp: Vec<[Vlc; 4]>,
    first_pattern: Vec<Vlc>,
    second_pattern: [Vlc; 2],
    third_pattern: [Vlc; 2],
    coefficient: Vlc,
}

pub(crate) struct Rv34Tables {
    intra: Vec<Rv34Vlc>,
    inter: Vec<Rv34Vlc>,
}

/// `rv34_gen_vlc_ext`.
fn gen_vlc(bits: &[u8], syms: Option<&[u8]>, mod_three_bits_offset: i32) -> Vlc {
    let mut counts = [0i32; 17];
    for &b in bits {
        counts[b as usize] += 1;
    }
    let mut codes = [0i32; 17];
    counts[0] = 0;
    let mut maxbits = 0;
    for i in 0..16 {
        codes[i + 1] = (codes[i] + counts[i]) << 1;
        if counts[i] != 0 {
            maxbits = i as u32;
        }
    }
    let mut entries = Vec::with_capacity(bits.len());
    for (i, &b) in bits.iter().enumerate() {
        let cw = codes[b as usize] as u16;
        codes[b as usize] += 1;
        let sym: i16 = if mod_three_bits_offset > 0 {
            let off = mod_three_bits_offset as usize;
            let mask = (1usize << off) - 1;
            ((MODULO_THREE_TABLE[i >> off] as usize) << off | (i & mask)) as i16
        } else if mod_three_bits_offset == 0 {
            MODULO_THREE_TABLE[i] as i16
        } else if let Some(s) = syms {
            s[i] as i16
        } else {
            i as i16
        };
        entries.push((b as u32, cw as u32, sym));
    }
    Vlc::init_sparse(maxbits.min(9), &entries).expect("RV34 VLC tables are valid")
}

static TABLES: LazyLock<Rv34Tables> = LazyLock::new(build_rv34_tables);

fn rv34_tables() -> &'static Rv34Tables {
    &TABLES
}

/// `rv34_init_tables`.
fn build_rv34_tables() -> Rv34Tables {
    {
        let mut intra = Vec::with_capacity(NUM_INTRA_TABLES);
        for i in 0..NUM_INTRA_TABLES {
            let cbppattern = (0..2).map(|j| gen_vlc(&RV34_TABLE_INTRA_CBPPAT[i][j], None, 4)).collect();
            let cbp = (0..2)
                .map(|j| std::array::from_fn(|k| gen_vlc(&RV34_TABLE_INTRA_CBP[i][j + k * 2], Some(&RV34_CBP_CODE), -1)))
                .collect();
            let first_pattern = (0..4).map(|j| gen_vlc(&RV34_TABLE_INTRA_FIRSTPAT[i][j], None, 3)).collect();
            intra.push(Rv34Vlc {
                cbppattern,
                cbp,
                first_pattern,
                second_pattern: std::array::from_fn(|j| gen_vlc(&RV34_TABLE_INTRA_SECONDPAT[i][j], None, 0)),
                third_pattern: std::array::from_fn(|j| gen_vlc(&RV34_TABLE_INTRA_THIRDPAT[i][j], None, 0)),
                coefficient: gen_vlc(&RV34_INTRA_COEFF[i], None, -1),
            });
        }
        let mut inter = Vec::with_capacity(NUM_INTER_TABLES);
        for i in 0..NUM_INTER_TABLES {
            inter.push(Rv34Vlc {
                cbppattern: vec![gen_vlc(&RV34_INTER_CBPPAT[i], None, 4)],
                cbp: vec![std::array::from_fn(|j| gen_vlc(&RV34_INTER_CBP[i][j], Some(&RV34_CBP_CODE), -1))],
                first_pattern: (0..2).map(|j| gen_vlc(&RV34_TABLE_INTER_FIRSTPAT[i][j], None, 3)).collect(),
                second_pattern: std::array::from_fn(|j| gen_vlc(&RV34_TABLE_INTER_SECONDPAT[i][j], None, 0)),
                third_pattern: std::array::from_fn(|j| gen_vlc(&RV34_TABLE_INTER_THIRDPAT[i][j], None, 0)),
                coefficient: gen_vlc(&RV34_INTER_COEFF[i], None, -1),
            });
        }
        Rv34Tables { intra, inter }
    }
}

/// Which VLC set the current macroblock uses (`r->cur_vlcs`).
#[derive(Clone, Copy)]
struct VlcSel {
    inter: bool,
    idx: usize,
}

/// `choose_vlc_set`.
fn choose_vlc_set(quant: i32, modifier: i32, ty: i32) -> VlcSel {
    let mut quant = quant;
    if modifier == 2 && quant < 19 {
        quant += 10;
    } else if modifier != 0 && quant < 26 {
        quant += 5;
    }
    let quant = quant.clamp(0, 31) as usize;
    if ty != 0 {
        VlcSel { inter: true, idx: RV34_QUANT_TO_VLC_SET[1][quant] as usize }
    } else {
        VlcSel { inter: false, idx: RV34_QUANT_TO_VLC_SET[0][quant] as usize }
    }
}

fn vlc_set(sel: VlcSel) -> &'static Rv34Vlc {
    let t = rv34_tables();
    if sel.inter { &t.inter[sel.idx] } else { &t.intra[sel.idx] }
}

/// `rv34_decode_cbp`.
fn decode_cbp(gb: &mut BitReader, vlc: &Rv34Vlc, table: usize) -> i32 {
    const CBP_MASKS: [i32; 3] = [0x100000, 0x010000, 0x110000];
    const SHIFTS: [i32; 4] = [0, 2, 8, 10];
    let Some(pat_vlc) = vlc.cbppattern.get(table) else { return -1 };
    let Some(cbp_vlcs) = vlc.cbp.get(table) else { return -1 };
    let mut code = gb.get_vlc2(&pat_vlc.table, 9, 2);
    let pattern = (code & 0xF) as usize;
    code >>= 4;
    let ones = RV34_COUNT_ONES[pattern] as usize;
    let mut cbp: i32 = 0;
    let mut mask = 8;
    let mut k = 0;
    while mask != 0 {
        if pattern & mask != 0 {
            let v = &cbp_vlcs[ones];
            cbp |= gb.get_vlc2(&v.table, v.bits, 1) << SHIFTS[k];
        }
        mask >>= 1;
        k += 1;
    }
    for i in 0..4 {
        let t = (code >> (6 - 2 * i)) & 3;
        if t == 1 {
            cbp |= CBP_MASKS[gb.get_bits1() as usize] << i;
        }
        if t == 2 {
            cbp |= CBP_MASKS[2] << i;
        }
    }
    cbp
}

/// `decode_coeff`.
#[inline]
fn decode_coeff(dst: &mut i16, coef: i32, esc: i32, gb: &mut BitReader, vlc: &Vlc, q: i32) {
    if coef == 0 {
        return;
    }
    let mut coef = coef;
    if coef == esc {
        coef = gb.get_vlc2(&vlc.table, 9, 2);
        if coef > 23 {
            coef -= 23;
            coef = 22 + ((1 << coef) | gb.get_bits(coef as u32) as i32);
        }
        coef += esc;
    }
    if gb.get_bits1() != 0 {
        coef = -coef;
    }
    *dst = (coef.wrapping_mul(q).wrapping_add(8) >> 4) as i16;
}

/// `decode_subblock`: a 2x2 group at `off` in the 4x4 block.
#[inline]
fn decode_subblock(dst: &mut [i16; 16], off: usize, flags: i32, is_block2: bool, gb: &mut BitReader, vlc: &Vlc, q: i32) {
    decode_coeff(&mut dst[off], flags >> 6, 3, gb, vlc, q);
    if is_block2 {
        decode_coeff(&mut dst[off + 4], (flags >> 4) & 3, 2, gb, vlc, q);
        decode_coeff(&mut dst[off + 1], (flags >> 2) & 3, 2, gb, vlc, q);
    } else {
        decode_coeff(&mut dst[off + 1], (flags >> 4) & 3, 2, gb, vlc, q);
        decode_coeff(&mut dst[off + 4], (flags >> 2) & 3, 2, gb, vlc, q);
    }
    decode_coeff(&mut dst[off + 5], flags & 3, 2, gb, vlc, q);
}

/// `rv34_decode_block`: returns whether AC coefficients were coded.
#[allow(clippy::too_many_arguments)]
fn decode_block(dst: &mut [i16; 16], gb: &mut BitReader, rvlc: &Rv34Vlc, fc: usize, sc: usize, q_dc: i32, q_ac1: i32, q_ac2: i32) -> bool {
    let Some(first) = rvlc.first_pattern.get(fc) else { return false };
    let mut flags = gb.get_vlc2(&first.table, 9, 2);
    let pattern = flags & 7;
    flags >>= 3;
    let coef = &rvlc.coefficient;
    if flags & 0x3F != 0 {
        // decode_subblock3
        decode_coeff(&mut dst[0], flags >> 6, 3, gb, coef, q_dc);
        decode_coeff(&mut dst[1], (flags >> 4) & 3, 2, gb, coef, q_ac1);
        decode_coeff(&mut dst[4], (flags >> 2) & 3, 2, gb, coef, q_ac1);
        decode_coeff(&mut dst[5], flags & 3, 2, gb, coef, q_ac2);
    } else {
        // decode_subblock1
        decode_coeff(&mut dst[0], flags >> 6, 3, gb, coef, q_dc);
        if pattern == 0 {
            return false;
        }
    }
    if pattern & 4 != 0 {
        let f = gb.get_vlc2(&rvlc.second_pattern[sc].table, 9, 2);
        decode_subblock(dst, 2, f, false, gb, coef, q_ac2);
    }
    if pattern & 2 != 0 {
        // Coefficients 1 and 2 are swapped for this block.
        let f = gb.get_vlc2(&rvlc.second_pattern[sc].table, 9, 2);
        decode_subblock(dst, 8, f, true, gb, coef, q_ac2);
    }
    if pattern & 1 != 0 {
        let f = gb.get_vlc2(&rvlc.third_pattern[sc].table, 9, 2);
        decode_subblock(dst, 10, f, false, gb, coef, q_ac2);
    }
    true
}

/// `ff_rv34_get_start_offset`.
pub(crate) fn get_start_offset(gb: &mut BitReader, mb_size: i32) -> i32 {
    let mut i = 0;
    while i < 5 {
        if RV34_MB_MAX_SIZES[i] as i32 >= mb_size - 1 {
            break;
        }
        i += 1;
    }
    gb.get_bits(RV34_MB_BITS_SIZES[i] as u32) as i32
}

/// Essential slice information (`SliceInfo`).
#[derive(Clone, Copy, Default, Debug)]
pub(crate) struct SliceInfo {
    pub ty: i32,
    pub quant: i32,
    pub vlc_set: i32,
    pub start: i32,
    pub end: i32,
    pub width: i32,
    pub height: i32,
    pub pts: i32,
}

/// A decoded picture with the per-macroblock data later pictures read.
pub(crate) struct Picture {
    pub planes: [Plane; 3],
    pub mb_type: Vec<u32>,
    /// `motion_val[dir]`, indexed by `MV_BASE + b8 index`.
    pub mv: [Vec<[i16; 2]>; 2],
    pub qscale: Vec<i8>,
    pub pts: Option<i64>,
}

#[derive(Clone, Copy)]
pub(crate) struct Geometry {
    pub width: usize,
    pub height: usize,
    pub mb_width: usize,
    pub mb_height: usize,
    pub mb_stride: usize,
    pub b8_stride: usize,
}

impl Geometry {
    fn new(width: usize, height: usize) -> Self {
        let mb_width = width.div_ceil(16);
        let mb_height = height.div_ceil(16);
        Geometry { width, height, mb_width, mb_height, mb_stride: mb_width + 1, b8_stride: mb_width * 2 + 1 }
    }

    /// Offset of b8 index 0 in the motion vector arrays (room for the
    /// row-above / left-of-picture reads FFmpeg's padded arrays allow).
    fn mv_base(&self) -> usize {
        self.b8_stride + 1
    }
}

impl Picture {
    fn new(g: &Geometry, fill: u8) -> Self {
        let w = g.mb_width * 16;
        let h = g.mb_height * 16;
        let mv_len = g.mv_base() + g.b8_stride * (g.mb_height * 2 + 1);
        Picture {
            planes: [Plane::new(w, h, fill), Plane::new(w / 2, h / 2, fill), Plane::new(w / 2, h / 2, fill)],
            mb_type: vec![0; g.mb_stride * (g.mb_height + 1)],
            mv: [vec![[0; 2]; mv_len], vec![[0; 2]; mv_len]],
            qscale: vec![0; g.mb_stride * (g.mb_height + 1)],
            pts: None,
        }
    }
}

pub struct Rv34Decoder {
    codec_id: CodecId,
    pub(crate) rv30: bool,
    extradata: Vec<u8>,
    pub(crate) orig_width: i32,
    pub(crate) orig_height: i32,
    pub(crate) max_rpr: i32,

    pub(crate) g: Geometry,
    context_reinit: bool,

    cur: Option<Picture>,
    last: Option<Arc<Picture>>,
    next: Option<Arc<Picture>>,
    pub(crate) pict_type: i32,

    pub(crate) si: SliceInfo,
    pub(crate) mb_x: usize,
    pub(crate) mb_y: usize,
    resync_mb_x: usize,
    resync_mb_y: usize,
    pub(crate) first_slice_line: bool,
    qscale: i32,
    mb_num_left: i32,
    pub(crate) mb_skip_run: i32,

    /// `intra_types_hist`; the current rows start at `intra_types_stride * 4`.
    pub(crate) intra_types_hist: Vec<i8>,
    pub(crate) intra_types_stride: usize,
    /// Internal (RV34) macroblock types.
    pub(crate) mb_type: Vec<usize>,
    pub(crate) cbp_luma: Vec<u16>,
    pub(crate) cbp_chroma: Vec<u8>,
    pub(crate) deblock_coefs: Vec<u16>,

    block_type: usize,
    luma_vlc: usize,
    chroma_vlc: usize,
    is16: bool,
    dmv: [[i32; 2]; 4],
    pub(crate) avail_cache: [u32; 12],
    cur_vlcs: VlcSel,

    cur_pts: i32,
    last_pts: i32,
    next_pts: i32,
    scaled_weight: bool,
    weight1: i32,
    weight2: i32,
    mv_weight1: i32,
    mv_weight2: i32,

    /// RV40 weighted bi-prediction temporaries (`tmp_b_block_y/uv`).
    tmp_y: [[u8; 256]; 2],
    tmp_uv: [[[u8; 64]; 2]; 2],

    /// Output frames not yet returned, each with the size it was cropped
    /// to (`set_dimensions` drops the references when the size changes).
    ready: VecDeque<(Frame, (u32, u32))>,
    /// Size of the frame `receive_frame` last returned.
    last_output: Option<(u32, u32)>,
}

/// Result of decoding a slice (`rv34_decode_slice`'s return value).
enum SliceEnd {
    /// The slice ended before the last macroblock row.
    More,
    /// The slice reached the end of the picture, or failed (FFmpeg treats
    /// both as "picture finished").
    Last,
}

impl Rv34Decoder {
    pub fn new(params: &CodecParameters, rv30: bool) -> Result<Self> {
        let extradata = crate::real_extradata(&params.extradata);
        let width = params.width.unwrap_or(0) as usize;
        let height = params.height.unwrap_or(0) as usize;
        let mut max_rpr = 0;
        if rv30 {
            if extradata.len() < 2 {
                return Err(Error::invalid("rv30: extradata is too small"));
            }
            max_rpr = (extradata[1] & 7) as i32;
        }
        let codec_id = CodecId::new(if rv30 { "rv30" } else { "rv40" });
        let mut d = Rv34Decoder {
            codec_id,
            rv30,
            extradata,
            orig_width: width as i32,
            orig_height: height as i32,
            max_rpr,
            g: Geometry::new(0, 0),
            context_reinit: true,
            cur: None,
            last: None,
            next: None,
            pict_type: 0,
            si: SliceInfo::default(),
            mb_x: 0,
            mb_y: 0,
            resync_mb_x: 0,
            resync_mb_y: 0,
            first_slice_line: false,
            qscale: 0,
            mb_num_left: 0,
            mb_skip_run: 0,
            intra_types_hist: Vec::new(),
            intra_types_stride: 0,
            mb_type: Vec::new(),
            cbp_luma: Vec::new(),
            cbp_chroma: Vec::new(),
            deblock_coefs: Vec::new(),
            block_type: 0,
            luma_vlc: 0,
            chroma_vlc: 0,
            is16: false,
            dmv: [[0; 2]; 4],
            avail_cache: [0; 12],
            cur_vlcs: VlcSel { inter: false, idx: 0 },
            cur_pts: 0,
            last_pts: 0,
            next_pts: 0,
            scaled_weight: false,
            weight1: 0,
            weight2: 0,
            mv_weight1: 0,
            mv_weight2: 0,
            tmp_y: [[0; 256]; 2],
            tmp_uv: [[[0; 64]; 2]; 2],
            ready: VecDeque::new(),
            last_output: None,
        };
        if width > 0 && height > 0 && check_dimensions(width, height).is_ok() {
            d.set_dimensions(width, height);
        }
        // Build the shared tables up front so the first packet does not pay.
        rv34_tables();
        if !rv30 {
            rv40::rv40_tables();
        }
        Ok(d)
    }

    /// `ff_mpv_common_frame_size_change` + `rv34_decoder_realloc`.
    fn set_dimensions(&mut self, width: usize, height: usize) {
        self.g = Geometry::new(width, height);
        self.last = None;
        self.next = None;
        self.cur = None;
        let g = self.g;
        self.intra_types_stride = g.mb_width * 4 + 4;
        let n = g.mb_stride * (g.mb_height + 1);
        self.cbp_chroma = vec![0; n];
        self.cbp_luma = vec![0; n];
        self.deblock_coefs = vec![0; n];
        self.intra_types_hist = vec![-1; self.intra_types_stride * 4 * 2];
        self.mb_type = vec![0; n];
        self.context_reinit = false;
    }

    #[inline]
    pub(crate) fn intra_types_base(&self) -> usize {
        self.intra_types_stride * 4
    }

    fn parse_slice_header(&self, gb: &mut BitReader) -> Option<SliceInfo> {
        if self.rv30 {
            rv30::parse_slice_header(self, gb)
        } else {
            rv40::parse_slice_header(self, gb)
        }
    }

    /// `rv34_decode_intra_mb_header`.
    fn decode_intra_mb_header(&mut self, gb: &mut BitReader, it: usize) -> i32 {
        let mb_pos = self.mb_x + self.mb_y * self.g.mb_stride;
        self.is16 = gb.get_bits1() != 0;
        if self.is16 {
            self.cur.as_mut().unwrap().mb_type[mb_pos] = MB_TYPE_INTRA16X16;
            self.block_type = RV34_MB_TYPE_INTRA16X16;
            let t = gb.get_bits(2) as i8;
            self.fill_intra_types(it, t);
            self.luma_vlc = 2;
        } else {
            if !self.rv30 {
                // "Need DQUANT" when the bit is clear; FFmpeg only logs it.
                let _ = gb.get_bits1();
            }
            self.cur.as_mut().unwrap().mb_type[mb_pos] = MB_TYPE_INTRA;
            self.block_type = RV34_MB_TYPE_INTRA;
            if self.decode_intra_types(gb, it).is_err() {
                return -1;
            }
            self.luma_vlc = 1;
        }
        self.chroma_vlc = 0;
        self.cur_vlcs = choose_vlc_set(self.si.quant, self.si.vlc_set, 0);
        decode_cbp(gb, vlc_set(self.cur_vlcs), self.is16 as usize)
    }

    fn fill_intra_types(&mut self, it: usize, t: i8) {
        let stride = self.intra_types_stride;
        for j in 0..4 {
            self.intra_types_hist[it + j * stride..it + j * stride + 4].fill(t);
        }
    }

    fn decode_intra_types(&mut self, gb: &mut BitReader, it: usize) -> std::result::Result<(), ()> {
        if self.rv30 {
            rv30::decode_intra_types(self, gb, it)
        } else {
            rv40::decode_intra_types(self, gb, it)
        }
    }

    /// `rv34_decode_inter_mb_header`.
    fn decode_inter_mb_header(&mut self, gb: &mut BitReader, it: usize) -> i32 {
        let mb_pos = self.mb_x + self.mb_y * self.g.mb_stride;
        let bt = if self.rv30 { rv30::decode_mb_info(self, gb) } else { rv40::decode_mb_info(self, gb) };
        if bt < 0 {
            return -1;
        }
        self.block_type = bt as usize;
        let lavc = RV34_MB_TYPE_TO_LAVC[self.block_type];
        self.cur.as_mut().unwrap().mb_type[mb_pos] = lavc;
        self.mb_type[mb_pos] = self.block_type;
        if self.block_type == RV34_MB_SKIP {
            if self.pict_type == PICT_P {
                self.mb_type[mb_pos] = RV34_MB_P_16X16;
            }
            if self.pict_type == PICT_B {
                self.mb_type[mb_pos] = RV34_MB_B_DIRECT;
            }
        }
        self.is16 = lavc & MB_TYPE_INTRA16X16 != 0;
        if self.decode_mv(gb, self.block_type).is_err() {
            return -1;
        }
        if self.block_type == RV34_MB_SKIP {
            self.fill_intra_types(it, 0);
            return 0;
        }
        self.chroma_vlc = 1;
        self.luma_vlc = 0;

        if is_intra(lavc) {
            if self.is16 {
                let t = gb.get_bits(2) as i8;
                self.fill_intra_types(it, t);
                self.luma_vlc = 2;
            } else {
                if self.decode_intra_types(gb, it).is_err() {
                    return -1;
                }
                self.luma_vlc = 1;
            }
            self.chroma_vlc = 0;
            self.cur_vlcs = choose_vlc_set(self.si.quant, self.si.vlc_set, 0);
        } else {
            self.fill_intra_types(it, 0);
            self.cur_vlcs = choose_vlc_set(self.si.quant, self.si.vlc_set, 1);
            if self.mb_type[mb_pos] == RV34_MB_P_MIX16X16 {
                self.is16 = true;
                self.chroma_vlc = 1;
                self.luma_vlc = 2;
                self.cur_vlcs = choose_vlc_set(self.si.quant, self.si.vlc_set, 0);
            }
        }
        decode_cbp(gb, vlc_set(self.cur_vlcs), self.is16 as usize)
    }

    // ---- motion vectors -------------------------------------------------

    #[inline]
    fn mv_index(&self, b8: isize) -> usize {
        (self.g.mv_base() as isize + b8) as usize
    }

    #[inline]
    fn cur_mv(&self, dir: usize, b8: isize) -> [i32; 2] {
        let v = self.cur.as_ref().unwrap().mv[dir][self.mv_index(b8)];
        [v[0] as i32, v[1] as i32]
    }

    #[inline]
    fn set_cur_mv(&mut self, dir: usize, b8: isize, v: [i32; 2]) {
        let idx = self.mv_index(b8);
        self.cur.as_mut().unwrap().mv[dir][idx] = [v[0] as i16, v[1] as i16];
    }

    /// `ZERO8x2`: clears the 2x2 vectors of the macroblock at b8 index `pos`.
    fn zero8x2(&mut self, dir: usize, pos: isize) {
        let s = self.g.b8_stride as isize;
        for p in [pos, pos + 1, pos + s, pos + s + 1] {
            self.set_cur_mv(dir, p, [0, 0]);
        }
    }

    /// `rv34_pred_mv`.
    fn pred_mv(&mut self, block_type: usize, subblock_no: usize, dmv_no: usize) {
        let b8s = self.g.b8_stride as isize;
        let mut mv_pos = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * b8s;
        let avail = AVAIL_INDEXES[subblock_no] as isize;
        let ac = |i: isize| self.avail_cache[(avail + i) as usize];
        let mut c_off = PART_SIZES_W[block_type] as isize;
        mv_pos += (subblock_no & 1) as isize + (subblock_no >> 1) as isize * b8s;
        if subblock_no == 3 {
            c_off = -1;
        }
        let mut a = [0i32; 2];
        if ac(-1) != 0 {
            a = self.cur_mv(0, mv_pos - 1);
        }
        let b = if ac(-4) != 0 { self.cur_mv(0, mv_pos - b8s) } else { a };
        let c = if ac(c_off - 4) == 0 {
            if ac(-4) != 0 && (ac(-1) != 0 || self.rv30) { self.cur_mv(0, mv_pos - b8s - 1) } else { a }
        } else {
            self.cur_mv(0, mv_pos - b8s + c_off)
        };
        let mx = mid_pred(a[0], b[0], c[0]).wrapping_add(self.dmv[dmv_no][0]);
        let my = mid_pred(a[1], b[1], c[1]).wrapping_add(self.dmv[dmv_no][1]);
        for j in 0..PART_SIZES_H[block_type] as isize {
            for i in 0..PART_SIZES_W[block_type] as isize {
                self.set_cur_mv(0, mv_pos + i + j * b8s, [mx, my]);
            }
        }
    }

    /// `calc_add_mv`.
    fn calc_add_mv(&self, dir: usize, val: i32) -> i32 {
        let mul = if dir != 0 { self.mv_weight2.wrapping_neg() } else { self.mv_weight1 };
        ((val as u32).wrapping_mul(mul as u32).wrapping_add(0x2000) as i32) >> 14
    }

    /// `rv34_pred_mv_b`.
    fn pred_mv_b(&mut self, block_type: usize, dir: usize) {
        let g = self.g;
        let b8s = g.b8_stride as isize;
        let mb_pos = self.mb_x + self.mb_y * g.mb_stride;
        let mv_pos = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * b8s;
        let mask = if dir != 0 { MB_TYPE_BACKWARD_MV } else { MB_TYPE_FORWARD_MV };
        let ty = self.cur.as_ref().unwrap().mb_type[mb_pos];
        let (mut a, mut b, mut c) = ([0i32; 2], [0i32; 2], [0i32; 2]);
        let (mut has_a, mut has_b, mut has_c) = (0, 0, 0);
        if (self.avail_cache[5] & ty) & mask != 0 {
            a = self.cur_mv(dir, mv_pos - 1);
            has_a = 1;
        }
        if (self.avail_cache[2] & ty) & mask != 0 {
            b = self.cur_mv(dir, mv_pos - b8s);
            has_b = 1;
        }
        if self.avail_cache[2] != 0 && (self.avail_cache[4] & ty) & mask != 0 {
            c = self.cur_mv(dir, mv_pos - b8s + 2);
            has_c = 1;
        } else if self.mb_x + 1 == g.mb_width && (self.avail_cache[1] & ty) & mask != 0 {
            c = self.cur_mv(dir, mv_pos - b8s - 1);
            has_c = 1;
        }
        // rv34_pred_b_vector
        let (mut mx, mut my);
        if has_a + has_b + has_c != 3 {
            mx = a[0] + b[0] + c[0];
            my = a[1] + b[1] + c[1];
            if has_a + has_b + has_c == 2 {
                mx /= 2;
                my /= 2;
            }
        } else {
            mx = mid_pred(a[0], b[0], c[0]);
            my = mid_pred(a[1], b[1], c[1]);
        }
        mx = mx.wrapping_add(self.dmv[dir][0]);
        my = my.wrapping_add(self.dmv[dir][1]);
        for j in 0..2 {
            for i in 0..2 {
                self.set_cur_mv(dir, mv_pos + i + j * b8s, [mx, my]);
            }
        }
        if block_type == RV34_MB_B_BACKWARD || block_type == RV34_MB_B_FORWARD {
            self.zero8x2(1 - dir, mv_pos);
        }
    }

    /// `rv34_pred_mv_rv3`.
    fn pred_mv_rv3(&mut self) {
        let b8s = self.g.b8_stride as isize;
        let mv_pos = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * b8s;
        let ac = |i: isize| self.avail_cache[(6 + i) as usize];
        let mut a = [0i32; 2];
        if ac(-1) != 0 {
            a = self.cur_mv(0, mv_pos - 1);
        }
        let b = if ac(-4) != 0 { self.cur_mv(0, mv_pos - b8s) } else { a };
        let c = if ac(-4 + 2) == 0 {
            if ac(-4) != 0 && ac(-1) != 0 { self.cur_mv(0, mv_pos - b8s - 1) } else { a }
        } else {
            self.cur_mv(0, mv_pos - b8s + 2)
        };
        let mx = mid_pred(a[0], b[0], c[0]).wrapping_add(self.dmv[0][0]);
        let my = mid_pred(a[1], b[1], c[1]).wrapping_add(self.dmv[0][1]);
        for j in 0..2 {
            for i in 0..2 {
                for k in 0..2 {
                    self.set_cur_mv(k, mv_pos + i + j * b8s, [mx, my]);
                }
            }
        }
    }

    /// `rv34_mc`: motion compensation of one partition from `dir`'s
    /// reference into the picture (or the weighting temporaries).
    #[allow(clippy::too_many_arguments)]
    fn mc(&mut self, block_type: usize, xoff: usize, yoff: usize, mv_off: isize, width: usize, height: usize, dir: usize, weighted: bool, avg: bool) {
        let g = self.g;
        let mv_pos = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * g.b8_stride as isize + mv_off;
        let mv = self.cur_mv(dir, mv_pos);
        let (mx, my, lx, ly, umx, umy, uvmx, uvmy);
        if self.rv30 {
            mx = (mv[0] + (3 << 24)) / 3 - (1 << 24);
            my = (mv[1] + (3 << 24)) / 3 - (1 << 24);
            lx = (mv[0] + (3 << 24)) % 3;
            ly = (mv[1] + (3 << 24)) % 3;
            let chroma_mx = mv[0] / 2;
            let chroma_my = mv[1] / 2;
            umx = (chroma_mx + (3 << 24)) / 3 - (1 << 24);
            umy = (chroma_my + (3 << 24)) / 3 - (1 << 24);
            uvmx = CHROMA_COEFFS[((chroma_mx + (3 << 24)) % 3) as usize];
            uvmy = CHROMA_COEFFS[((chroma_my + (3 << 24)) % 3) as usize];
        } else {
            mx = mv[0] >> 2;
            my = mv[1] >> 2;
            lx = mv[0] & 3;
            ly = mv[1] & 3;
            let cx = mv[0] / 2;
            let cy = mv[1] / 2;
            umx = cx >> 2;
            umy = cy >> 2;
            let (ux, uy) = ((cx & 3) << 1, (cy & 3) << 1);
            // RV40 uses the same chroma filter for H2V2 and H3V3.
            if ux == 6 && uy == 6 {
                uvmx = 4;
                uvmy = 4;
            } else {
                uvmx = ux;
                uvmy = uy;
            }
        }
        let src_x = (self.mb_x * 16 + xoff) as i32 + mx;
        let src_y = (self.mb_y * 16 + yoff) as i32 + my;
        let uvsrc_x = (self.mb_x * 8 + (xoff >> 1)) as i32 + umx;
        let uvsrc_y = (self.mb_y * 8 + (yoff >> 1)) as i32 + umy;

        let Some(refp) = (if dir != 0 { self.next.as_ref() } else { self.last.as_ref() }) else { return };
        let refp = Arc::clone(refp);

        // Luma: fetch (8w+6)x(8h+6) samples around the block with the
        // reference's edge clamping (`emulated_edge_mc`).
        let bw = width * 8;
        let bh = height * 8;
        let fw = bw + 6;
        let fh = bh + 6;
        let mut fetched = [0u8; 22 * 22];
        refp.planes[0].fetch(src_x - 2, src_y - 2, fw, fh, &mut fetched);
        let (lx, ly) = (lx as usize, ly as usize);

        let is16x16 = block_type != RV34_MB_P_8X8 && block_type != RV34_MB_P_16X8 && block_type != RV34_MB_P_8X16;
        let cur = self.cur.as_mut().unwrap();
        let (ydst, ypos, ystride): (&mut [u8], usize, usize) = if weighted {
            (&mut self.tmp_y[dir][..], xoff + yoff * 16, 16)
        } else {
            let p = &mut cur.planes[0];
            let pos = p.idx(self.mb_x * 16 + xoff, self.mb_y * 16 + yoff);
            let stride = p.stride;
            (&mut p.data[..], pos, stride)
        };
        let mut run = |dst_off: usize, src_off: usize, size: usize| {
            let mut d = McDst { data: &mut *ydst, stride: ystride, pos: ypos + dst_off };
            let s = McSrc { data: &fetched, stride: fw, origin: 2 * fw + 2 + src_off };
            if self.rv30 {
                dsp::rv30_tpel(&mut d, &s, size, lx, ly, avg);
            } else {
                dsp::rv40_qpel(&mut d, &s, size, lx, ly, avg);
            }
        };
        if block_type == RV34_MB_P_16X8 {
            run(0, 0, 8);
            run(8, 8, 8);
        } else if block_type == RV34_MB_P_8X16 {
            run(0, 0, 8);
            run(8 * ystride, 8 * fw, 8);
        } else {
            run(0, 0, if is16x16 { 16 } else { 8 });
        }

        // Chroma: (4w+1)x(4h+1) samples, clamped to the chroma planes.
        let cw = width * 4;
        let ch = height * 4;
        let cfw = cw + 1;
        let cfh = ch + 1;
        for c in 0..2 {
            let mut cf = [0u8; 9 * 9];
            refp.planes[1 + c].fetch(uvsrc_x, uvsrc_y, cfw, cfh, &mut cf);
            let s = McSrc { data: &cf, stride: cfw, origin: 0 };
            if weighted {
                let mut d = McDst { data: &mut self.tmp_uv[dir][c][..], stride: 8, pos: (xoff >> 1) + (yoff >> 1) * 8 };
                dsp::chroma_mc(&mut d, &s, cw, ch, uvmx, uvmy, !self.rv30, avg);
            } else {
                let p = &mut cur.planes[1 + c];
                let pos = p.idx(self.mb_x * 8 + (xoff >> 1), self.mb_y * 8 + (yoff >> 1));
                let stride = p.stride;
                let mut d = McDst { data: &mut p.data[..], stride, pos };
                dsp::chroma_mc(&mut d, &s, cw, ch, uvmx, uvmy, !self.rv30, avg);
            }
        }
    }

    fn mc_1mv(&mut self, block_type: usize, xoff: usize, yoff: usize, mv_off: isize, width: usize, height: usize, dir: usize) {
        self.mc(block_type, xoff, yoff, mv_off, width, height, dir, false, false);
    }

    /// `rv4_weight`.
    fn rv4_weight(&mut self) {
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let (w1, w2, scaled) = (self.weight1, self.weight2, self.scaled_weight);
        let cur = self.cur.as_mut().unwrap();
        let p = &mut cur.planes[0];
        let pos = p.idx(mb_x * 16, mb_y * 16);
        dsp::rv40_weight(&mut p.data, pos, p.stride, &self.tmp_y[0], &self.tmp_y[1], 16, 16, w1, w2, scaled);
        for c in 0..2 {
            let p = &mut cur.planes[1 + c];
            let pos = p.idx(mb_x * 8, mb_y * 8);
            dsp::rv40_weight(&mut p.data, pos, p.stride, &self.tmp_uv[0][c], &self.tmp_uv[1][c], 8, 8, w1, w2, scaled);
        }
    }

    /// `rv34_mc_2mv`.
    fn mc_2mv(&mut self, block_type: usize) {
        let weighted = !self.rv30 && block_type != RV34_MB_B_BIDIR && self.weight1 != 8192;
        self.mc(block_type, 0, 0, 0, 2, 2, 0, weighted, false);
        if !weighted {
            self.mc(block_type, 0, 0, 0, 2, 2, 1, false, true);
        } else {
            self.mc(block_type, 0, 0, 0, 2, 2, 1, true, false);
            self.rv4_weight();
        }
    }

    /// `rv34_mc_2mv_skip`.
    fn mc_2mv_skip(&mut self) {
        let weighted = !self.rv30 && self.weight1 != 8192;
        let b8s = self.g.b8_stride as isize;
        for j in 0..2usize {
            for i in 0..2usize {
                let off = i as isize + j as isize * b8s;
                self.mc(RV34_MB_P_8X8, i * 8, j * 8, off, 1, 1, 0, weighted, false);
                self.mc(RV34_MB_P_8X8, i * 8, j * 8, off, 1, 1, 1, weighted, !weighted);
            }
        }
        if weighted {
            self.rv4_weight();
        }
    }

    /// `rv34_decode_mv`.
    fn decode_mv(&mut self, gb: &mut BitReader, block_type: usize) -> std::result::Result<(), ()> {
        let g = self.g;
        let b8s = g.b8_stride as isize;
        let mv_pos = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * b8s;
        self.dmv = [[0; 2]; 4];
        for i in 0..NUM_MVS[block_type] {
            self.dmv[i][0] = gb.get_interleaved_se_golomb();
            self.dmv[i][1] = gb.get_interleaved_se_golomb();
            if self.dmv[i][0] == INVALID_VLC || self.dmv[i][1] == INVALID_VLC {
                self.dmv[i] = [0, 0];
                return Err(());
            }
        }
        match block_type {
            RV34_MB_TYPE_INTRA | RV34_MB_TYPE_INTRA16X16 => {
                self.zero8x2(0, mv_pos);
                return Ok(());
            }
            RV34_MB_SKIP if self.pict_type == PICT_P => {
                self.zero8x2(0, mv_pos);
                self.mc_1mv(block_type, 0, 0, 0, 2, 2, 0);
            }
            RV34_MB_SKIP | RV34_MB_B_DIRECT => {
                // Direct mode uses the motion of the next reference picture.
                let mb_pos = self.mb_x + self.mb_y * g.mb_stride;
                let next = self.next.as_ref().map(Arc::clone);
                let next_bt = next.as_ref().map_or(0, |n| n.mb_type[mb_pos]);
                if is_intra(next_bt) || next_bt & MB_TYPE_SKIP != 0 || next.is_none() {
                    self.zero8x2(0, mv_pos);
                    self.zero8x2(1, mv_pos);
                } else {
                    let next = next.as_ref().unwrap();
                    for j in 0..2 {
                        for i in 0..2 {
                            let p = mv_pos + i + j * b8s;
                            let nv = next.mv[0][self.mv_index(p)];
                            for l in 0..2 {
                                let v = [self.calc_add_mv(l, nv[0] as i32), self.calc_add_mv(l, nv[1] as i32)];
                                self.set_cur_mv(l, p, v);
                            }
                        }
                    }
                }
                if next_bt & (MB_TYPE_16X8 | MB_TYPE_8X16 | MB_TYPE_8X8) == 0 {
                    self.mc_2mv(block_type);
                } else {
                    self.mc_2mv_skip();
                }
                self.zero8x2(0, mv_pos);
            }
            RV34_MB_P_16X16 | RV34_MB_P_MIX16X16 => {
                self.pred_mv(block_type, 0, 0);
                self.mc_1mv(block_type, 0, 0, 0, 2, 2, 0);
            }
            RV34_MB_B_FORWARD | RV34_MB_B_BACKWARD => {
                self.dmv[1] = self.dmv[0];
                let dir = (block_type == RV34_MB_B_BACKWARD) as usize;
                if self.rv30 {
                    self.pred_mv_rv3();
                } else {
                    self.pred_mv_b(block_type, dir);
                }
                self.mc_1mv(block_type, 0, 0, 0, 2, 2, dir);
            }
            RV34_MB_P_16X8 | RV34_MB_P_8X16 => {
                self.pred_mv(block_type, 0, 0);
                self.pred_mv(block_type, 1 + (block_type == RV34_MB_P_16X8) as usize, 1);
                if block_type == RV34_MB_P_16X8 {
                    self.mc_1mv(block_type, 0, 0, 0, 2, 1, 0);
                    self.mc_1mv(block_type, 0, 8, b8s, 2, 1, 0);
                }
                if block_type == RV34_MB_P_8X16 {
                    self.mc_1mv(block_type, 0, 0, 0, 1, 2, 0);
                    self.mc_1mv(block_type, 8, 0, 1, 1, 2, 0);
                }
            }
            RV34_MB_B_BIDIR => {
                self.pred_mv_b(block_type, 0);
                self.pred_mv_b(block_type, 1);
                self.mc_2mv(block_type);
            }
            RV34_MB_P_8X8 => {
                for i in 0..4usize {
                    self.pred_mv(block_type, i, i);
                    let off = (i & 1) as isize + (i >> 1) as isize * b8s;
                    self.mc_1mv(block_type, (i & 1) << 3, (i & 2) << 2, off, 1, 1, 0);
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- reconstruction ---------------------------------------------------

    /// `rv34_pred_4x4_block`.
    #[allow(clippy::too_many_arguments)]
    fn pred_4x4_block(plane: &mut Plane, pos: usize, itype: usize, up: bool, left: bool, down: bool, right: bool) {
        let stride = plane.stride;
        let mut itype = itype;
        if !up && !left {
            itype = DC_128_PRED;
        } else if !up {
            if itype == VERT_PRED {
                itype = HOR_PRED;
            }
            if itype == DC_PRED {
                itype = LEFT_DC_PRED;
            }
        } else if !left {
            if itype == HOR_PRED {
                itype = VERT_PRED;
            }
            if itype == DC_PRED {
                itype = TOP_DC_PRED;
            }
            if itype == DIAG_DOWN_LEFT_PRED {
                itype = DIAG_DOWN_LEFT_PRED_RV40_NODOWN;
            }
        }
        if !down {
            if itype == DIAG_DOWN_LEFT_PRED {
                itype = DIAG_DOWN_LEFT_PRED_RV40_NODOWN;
            }
            if itype == HOR_UP_PRED {
                itype = HOR_UP_PRED_RV40_NODOWN;
            }
            if itype == VERT_LEFT_PRED {
                itype = VERT_LEFT_PRED_RV40_NODOWN;
            }
        }
        let tr = pos - stride + 4;
        let topright = if !right && up {
            [plane.data[pos - stride + 3]; 4]
        } else {
            [plane.data[tr], plane.data[tr + 1], plane.data[tr + 2], plane.data[tr + 3]]
        };
        pred4x4(itype, &mut plane.data, pos, stride, topright);
    }

    /// `rv34_process_block`.
    #[allow(clippy::too_many_arguments)]
    fn process_block(gb: &mut BitReader, vlcs: &Rv34Vlc, plane: &mut Plane, pos: usize, fc: usize, sc: usize, q_dc: i32, q_ac: i32) {
        let mut block = [0i16; 16];
        let has_ac = decode_block(&mut block, gb, vlcs, fc, sc, q_dc, q_ac, q_ac);
        let stride = plane.stride;
        if has_ac {
            dsp::idct_add(&mut plane.data, pos, stride, &mut block);
        } else {
            dsp::idct_dc_add(&mut plane.data, pos, stride, block[0] as i32);
        }
    }

    fn luma_dc_quant(&self, intra: bool, q: usize) -> usize {
        if self.rv30 {
            RV30_LUMA_DC_QUANT[q] as usize
        } else {
            RV40_LUMA_DC_QUANT[if intra { 0 } else { 1 }][q] as usize
        }
    }

    /// The 16 luma 4x4 blocks of a 16x16 macroblock with separately coded
    /// DCs (shared by `rv34_output_i16x16` and the MIX16x16 inter path).
    fn decode_luma16(&mut self, gb: &mut BitReader, cbp: &mut i32, q_dc: i32, q_ac: i32, predict: Option<usize>) {
        let vlcs = vlc_set(self.cur_vlcs);
        let mut block16 = [0i16; 16];
        if decode_block(&mut block16, gb, vlcs, 3, 0, q_dc, q_dc, q_ac) {
            dsp::inv_transform_noround(&mut block16);
        } else {
            dsp::inv_transform_dc_noround(&mut block16);
        }
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let luma_vlc = self.luma_vlc;
        let plane = &mut self.cur.as_mut().unwrap().planes[0];
        let stride = plane.stride;
        let mut dst = plane.idx(mb_x * 16, mb_y * 16);
        if let Some(itype) = predict {
            pred16x16(itype, &mut plane.data, dst, stride);
        }
        for j in 0..4 {
            for i in 0..4 {
                let dc = block16[i + j * 4];
                let mut ptr = [0i16; 16];
                let has_ac = if *cbp & 1 != 0 { decode_block(&mut ptr, gb, vlcs, luma_vlc, 0, q_ac, q_ac, q_ac) } else { false };
                if has_ac {
                    ptr[0] = dc;
                    dsp::idct_add(&mut plane.data, dst + 4 * i, stride, &mut ptr);
                } else {
                    dsp::idct_dc_add(&mut plane.data, dst + 4 * i, stride, dc as i32);
                }
                *cbp >>= 1;
            }
            dst += 4 * stride;
        }
    }

    /// `rv34_output_i16x16`.
    fn output_i16x16(&mut self, gb: &mut BitReader, it: usize, mut cbp: i32) {
        let q = self.qscale as usize;
        let q_dc = RV34_QSCALE_TAB[self.luma_dc_quant(true, q)] as i32;
        let q_ac = RV34_QSCALE_TAB[q] as i32;
        let t = self.intra_types_hist[it];
        let Some(&base) = ITTRANS16.get(t as usize) else { return };
        let up = self.avail_cache[2] != 0;
        let left = self.avail_cache[5] != 0;
        let itype = adjust_pred16(base, up, left);
        self.decode_luma16(gb, &mut cbp, q_dc, q_ac, Some(itype));

        let mut itype = base;
        if itype == PLANE_PRED8X8 {
            itype = DC_PRED8X8;
        }
        let itype = adjust_pred16(itype, up, left);
        let q_dc = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[1][q] as usize] as i32;
        let q_ac = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[0][q] as usize] as i32;
        let vlcs = vlc_set(self.cur_vlcs);
        let chroma_vlc = self.chroma_vlc;
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        for j in 1..3 {
            let plane = &mut self.cur.as_mut().unwrap().planes[j];
            let dst = plane.idx(mb_x * 8, mb_y * 8);
            let stride = plane.stride;
            pred8x8(itype, &mut plane.data, dst, stride);
            for i in 0..4 {
                let coded = cbp & 1 != 0;
                cbp >>= 1;
                if !coded {
                    continue;
                }
                let pdst = dst + (i & 1) * 4 + (i & 2) * 2 * stride;
                Self::process_block(gb, vlcs, plane, pdst, chroma_vlc, 1, q_dc, q_ac);
            }
        }
    }

    /// `rv34_output_intra`.
    fn output_intra(&mut self, gb: &mut BitReader, it: usize, mut cbp: i32) -> std::result::Result<(), ()> {
        let mut avail = [0u8; 6 * 8];
        if self.avail_cache[1] != 0 {
            avail[0] = 1;
        }
        if self.avail_cache[2] != 0 {
            avail[1] = 1;
            avail[2] = 1;
        }
        if self.avail_cache[3] != 0 {
            avail[3] = 1;
            avail[4] = 1;
        }
        if self.avail_cache[4] != 0 {
            avail[5] = 1;
        }
        if self.avail_cache[5] != 0 {
            avail[8] = 1;
            avail[16] = 1;
        }
        if self.avail_cache[9] != 0 {
            avail[24] = 1;
            avail[32] = 1;
        }
        let stride_it = self.intra_types_stride;
        let mut types = [[0usize; 4]; 4];
        for (j, row) in types.iter_mut().enumerate() {
            for (i, t) in row.iter_mut().enumerate() {
                let v = self.intra_types_hist[it + j * stride_it + i];
                *t = *ITTRANS.get(v as usize).ok_or(())?;
            }
        }
        let q = self.qscale as usize;
        let q_ac = RV34_QSCALE_TAB[q] as i32;
        let vlcs = vlc_set(self.cur_vlcs);
        let (luma_vlc, chroma_vlc) = (self.luma_vlc, self.chroma_vlc);
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        {
            let plane = &mut self.cur.as_mut().unwrap().planes[0];
            let stride = plane.stride;
            let base = plane.idx(mb_x * 16, mb_y * 16);
            for j in 0..4 {
                let mut idx = 9 + j * 8;
                for i in 0..4 {
                    let dst = base + j * 4 * stride + i * 4;
                    Self::pred_4x4_block(
                        plane,
                        dst,
                        types[j][i],
                        avail[idx - 8] != 0,
                        avail[idx - 1] != 0,
                        avail[idx + 7] != 0,
                        avail[idx - 7] != 0,
                    );
                    avail[idx] = 1;
                    let coded = cbp & 1 != 0;
                    cbp >>= 1;
                    idx += 1;
                    if !coded {
                        continue;
                    }
                    Self::process_block(gb, vlcs, plane, dst, luma_vlc, 0, q_ac, q_ac);
                }
            }
        }

        let q_dc = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[1][q] as usize] as i32;
        let q_ac = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[0][q] as usize] as i32;
        for k in 0..2 {
            // fill_rectangle(r->avail_cache + 6, 2, 2, 4, 0, 4)
            self.avail_cache[6] = 0;
            self.avail_cache[7] = 0;
            self.avail_cache[10] = 0;
            self.avail_cache[11] = 0;
            let plane = &mut self.cur.as_mut().unwrap().planes[1 + k];
            let stride = plane.stride;
            let base = plane.idx(mb_x * 8, mb_y * 8);
            for j in 0..2 {
                let acache = 6 + j * 4;
                for i in 0..2 {
                    let a = acache + i;
                    let itype = types[j * 2][i * 2];
                    let dst = base + j * 4 * stride + 4 * i;
                    Self::pred_4x4_block(
                        plane,
                        dst,
                        itype,
                        self.avail_cache[a - 4] != 0,
                        self.avail_cache[a - 1] != 0,
                        i == 0 && j == 0,
                        self.avail_cache[a - 3] != 0,
                    );
                    self.avail_cache[a] = 1;
                    let coded = cbp & 1 != 0;
                    cbp >>= 1;
                    if !coded {
                        continue;
                    }
                    Self::process_block(gb, vlcs, plane, dst, chroma_vlc, 1, q_dc, q_ac);
                }
            }
        }
        Ok(())
    }

    /// `rv34_set_deblock_coef`.
    fn set_deblock_coef(&mut self) -> u16 {
        let g = self.g;
        let b8s = g.b8_stride as isize;
        let mut midx = (self.mb_x * 2) as isize + (self.mb_y * 2) as isize * b8s;
        let mut hmvmask: i32 = 0;
        let mut vmvmask: i32 = 0;
        let diff_gt_3 = |a: [i32; 2], b: [i32; 2]| -> bool {
            let d = a[0] - b[0];
            if !(-3..=3).contains(&d) {
                return true;
            }
            let d = a[1] - b[1];
            !(-3..=3).contains(&d)
        };
        for j in [0, 8] {
            for i in 0..2isize {
                let cur = self.cur_mv(0, midx + i);
                if diff_gt_3(cur, self.cur_mv(0, midx + i - 1)) {
                    vmvmask |= 0x11 << (j + i * 2);
                }
                if (j != 0 || self.mb_y != 0) && diff_gt_3(cur, self.cur_mv(0, midx + i - b8s)) {
                    hmvmask |= 0x03 << (j + i * 2);
                }
            }
            midx += b8s;
        }
        if self.first_slice_line {
            hmvmask &= !0x000F;
        }
        if self.mb_x == 0 {
            vmvmask &= !0x1111;
        }
        if self.rv30 {
            // RV30 marks both subblocks on the edge for filtering.
            vmvmask |= (vmvmask & 0x4444) >> 1;
            hmvmask |= (hmvmask & 0x0F00) >> 4;
            if self.mb_x != 0 {
                let p = self.mb_x - 1 + self.mb_y * g.mb_stride;
                self.deblock_coefs[p] |= ((vmvmask & 0x1111) << 3) as u16;
            }
            if !self.first_slice_line {
                let p = self.mb_x + (self.mb_y - 1) * g.mb_stride;
                self.deblock_coefs[p] |= ((hmvmask & 0xF) << 12) as u16;
            }
        }
        (hmvmask | vmvmask) as u16
    }

    /// Neighbour availability (`avail_cache`) for the current macroblock.
    fn set_avail_cache(&mut self) {
        let g = self.g;
        let mb_pos = self.mb_x + self.mb_y * g.mb_stride;
        self.avail_cache = [0; 12];
        self.avail_cache[6] = 1;
        self.avail_cache[7] = 1;
        self.avail_cache[10] = 1;
        self.avail_cache[11] = 1;
        let dist = (self.mb_x as isize - self.resync_mb_x as isize) + (self.mb_y as isize - self.resync_mb_y as isize) * g.mb_width as isize;
        let mbt = &self.cur.as_ref().unwrap().mb_type;
        let mw = g.mb_width as isize;
        if self.mb_x != 0 && dist != 0 {
            self.avail_cache[5] = mbt[mb_pos - 1];
            self.avail_cache[9] = mbt[mb_pos - 1];
        }
        if dist >= mw {
            self.avail_cache[2] = mbt[mb_pos - g.mb_stride];
            self.avail_cache[3] = mbt[mb_pos - g.mb_stride];
        }
        if self.mb_x + 1 < g.mb_width && dist >= mw - 1 {
            self.avail_cache[4] = mbt[mb_pos - g.mb_stride + 1];
        }
        if self.mb_x != 0 && dist > mw {
            self.avail_cache[1] = mbt[mb_pos - g.mb_stride - 1];
        }
    }

    /// `rv34_decode_inter_macroblock`.
    fn decode_inter_macroblock(&mut self, gb: &mut BitReader, it: usize) -> std::result::Result<(), ()> {
        let g = self.g;
        let mb_pos = self.mb_x + self.mb_y * g.mb_stride;
        self.set_avail_cache();
        self.qscale = self.si.quant;
        let mut cbp = self.decode_inter_mb_header(gb, it);
        self.cbp_luma[mb_pos] = cbp as u16;
        self.cbp_chroma[mb_pos] = (cbp >> 16) as u8;
        self.deblock_coefs[mb_pos] = self.set_deblock_coef() | self.cbp_luma[mb_pos];
        self.cur.as_mut().unwrap().qscale[mb_pos] = self.qscale as i8;

        if cbp == -1 {
            return Err(());
        }

        let lavc = self.cur.as_ref().unwrap().mb_type[mb_pos];
        if is_intra(lavc) {
            if self.is16 {
                self.output_i16x16(gb, it, cbp);
            } else {
                self.output_intra(gb, it, cbp)?;
            }
            return Ok(());
        }

        let q = self.qscale as usize;
        if self.is16 {
            // Only for RV34_MB_P_MIX16x16.
            let q_dc = RV34_QSCALE_TAB[self.luma_dc_quant(false, q)] as i32;
            let q_ac = RV34_QSCALE_TAB[q] as i32;
            self.decode_luma16(gb, &mut cbp, q_dc, q_ac, None);
            self.cur_vlcs = choose_vlc_set(self.si.quant, self.si.vlc_set, 1);
        } else {
            let q_ac = RV34_QSCALE_TAB[q] as i32;
            let vlcs = vlc_set(self.cur_vlcs);
            let luma_vlc = self.luma_vlc;
            let (mb_x, mb_y) = (self.mb_x, self.mb_y);
            let plane = &mut self.cur.as_mut().unwrap().planes[0];
            let stride = plane.stride;
            let base = plane.idx(mb_x * 16, mb_y * 16);
            for j in 0..4 {
                for i in 0..4 {
                    let coded = cbp & 1 != 0;
                    cbp >>= 1;
                    if !coded {
                        continue;
                    }
                    Self::process_block(gb, vlcs, plane, base + j * 4 * stride + 4 * i, luma_vlc, 0, q_ac, q_ac);
                }
            }
        }

        let q_dc = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[1][q] as usize] as i32;
        let q_ac = RV34_QSCALE_TAB[RV34_CHROMA_QUANT[0][q] as usize] as i32;
        let vlcs = vlc_set(self.cur_vlcs);
        let chroma_vlc = self.chroma_vlc;
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        for j in 1..3 {
            let plane = &mut self.cur.as_mut().unwrap().planes[j];
            let stride = plane.stride;
            let dst = plane.idx(mb_x * 8, mb_y * 8);
            for i in 0..4 {
                let coded = cbp & 1 != 0;
                cbp >>= 1;
                if !coded {
                    continue;
                }
                let pdst = dst + (i & 1) * 4 + (i & 2) * 2 * stride;
                Self::process_block(gb, vlcs, plane, pdst, chroma_vlc, 1, q_dc, q_ac);
            }
        }
        Ok(())
    }

    /// `rv34_decode_intra_macroblock`.
    fn decode_intra_macroblock(&mut self, gb: &mut BitReader, it: usize) -> std::result::Result<(), ()> {
        let mb_pos = self.mb_x + self.mb_y * self.g.mb_stride;
        self.set_avail_cache();
        self.qscale = self.si.quant;
        let cbp = self.decode_intra_mb_header(gb, it);
        self.cbp_luma[mb_pos] = cbp as u16;
        self.cbp_chroma[mb_pos] = (cbp >> 16) as u8;
        self.deblock_coefs[mb_pos] = 0xFFFF;
        self.cur.as_mut().unwrap().qscale[mb_pos] = self.qscale as i8;
        if cbp == -1 {
            return Err(());
        }
        if self.is16 {
            self.output_i16x16(gb, it, cbp);
            return Ok(());
        }
        self.output_intra(gb, it, cbp)
    }

    /// `check_slice_end`.
    fn check_slice_end(&self, gb: &BitReader) -> bool {
        if self.mb_y >= self.g.mb_height {
            return true;
        }
        if self.mb_num_left == 0 {
            return true;
        }
        if self.mb_skip_run > 1 {
            return false;
        }
        let bits = gb.bits_left();
        bits <= 0 || (bits < 8 && gb.show_bits(bits as u32) == 0)
    }

    fn loop_filter(&mut self, row: usize) {
        let cur = self.cur.as_mut().unwrap();
        if self.rv30 {
            rv30::loop_filter(self.g, cur, &mut self.deblock_coefs, &mut self.cbp_chroma, row);
        } else {
            rv40::loop_filter(self.g, cur, &mut self.deblock_coefs, &mut self.cbp_luma, &mut self.cbp_chroma, row);
        }
    }

    /// `rv34_decode_slice`. `data` runs to the end of the packet; the slice
    /// owns its first `size` bytes.
    fn decode_slice(&mut self, end: i32, data: &[u8], size: usize) -> SliceEnd {
        let mut gb = BitReader::new(data, size);
        let Some(si) = self.parse_slice_header(&mut gb) else { return SliceEnd::Last };
        self.si = si;
        let slice_type = if si.ty != 0 { si.ty } else { PICT_I };
        if slice_type != self.pict_type {
            return SliceEnd::Last;
        }
        if self.g.width as i32 != si.width || self.g.height as i32 != si.height {
            return SliceEnd::Last;
        }
        let g = self.g;
        self.si.end = end;
        self.qscale = si.quant;
        self.mb_num_left = self.si.end - self.si.start;
        self.mb_skip_run = 0;

        let mb_pos = (self.mb_x + self.mb_y * g.mb_width) as i32;
        if si.start != mb_pos {
            self.mb_x = si.start as usize % g.mb_width;
            self.mb_y = si.start as usize / g.mb_width;
        }
        self.intra_types_hist.fill(-1);
        self.first_slice_line = true;
        self.resync_mb_x = self.mb_x;
        self.resync_mb_y = self.mb_y;

        let base = self.intra_types_base();
        while !self.check_slice_end(&gb) {
            let it = base + self.mb_x * 4 + 4;
            let res = if self.si.ty != 0 { self.decode_inter_macroblock(&mut gb, it) } else { self.decode_intra_macroblock(&mut gb, it) };
            if res.is_err() {
                return SliceEnd::Last;
            }
            self.mb_x += 1;
            if self.mb_x == g.mb_width {
                self.mb_x = 0;
                self.mb_y += 1;
                let n = self.intra_types_stride * 4;
                self.intra_types_hist.copy_within(n..2 * n, 0);
                self.intra_types_hist[n..2 * n].fill(-1);
                if self.mb_y >= 2 {
                    self.loop_filter(self.mb_y - 2);
                }
            }
            if self.mb_x == self.resync_mb_x {
                self.first_slice_line = false;
            }
            self.mb_num_left -= 1;
        }
        if self.mb_y == g.mb_height { SliceEnd::Last } else { SliceEnd::More }
    }

    /// Converts a finished picture into an output frame.
    fn output_frame(&self, pic: &Picture) -> Frame {
        crate::picture::yuv420_frame(&pic.planes, self.g.width, self.g.height, pic.pts)
    }

    /// `finish_frame`.
    fn finish_frame(&mut self) {
        self.mb_num_left = 0;
        let Some(cur) = self.cur.take() else { return };
        let size = self.output_size();
        if self.pict_type == PICT_B {
            let f = self.output_frame(&cur);
            self.ready.push_back((f, size));
        } else {
            if let Some(last) = &self.last {
                let f = self.output_frame(last);
                self.ready.push_back((f, size));
            }
            self.next = Some(Arc::new(cur));
        }
    }

    /// The size `output_frame` crops to (`check_dimensions` bounds it).
    fn output_size(&self) -> (u32, u32) {
        (self.g.width as u32, self.g.height as u32)
    }

    /// `ff_mpv_frame_start` for the new picture.
    fn frame_start(&mut self, pts: Option<i64>) {
        if self.pict_type != PICT_B {
            self.last = self.next.take();
        }
        let mut pic = Picture::new(&self.g, 0);
        pic.pts = pts;
        self.cur = Some(pic);
        // ff_mpv_alloc_dummy_frames
        if self.last.is_none() && self.pict_type != PICT_I {
            let mut dummy = Picture::new(&self.g, 0x80);
            dummy.pts = None;
            self.last = Some(Arc::new(dummy));
        }
        if self.next.is_none() && self.pict_type == PICT_B {
            self.next = Some(Arc::new(Picture::new(&self.g, 0)));
        }
    }

    /// Ends a picture that never received its last slice: FFmpeg keeps it
    /// as the reference it already is (`next_pic`) and drops a B picture.
    fn abandon_current(&mut self) {
        if let Some(cur) = self.cur.take() {
            if self.pict_type != PICT_B {
                self.next = Some(Arc::new(cur));
            }
        }
    }

    fn get_slice_offset(hdr: &[u8], n: usize, slice_count: usize, buf_size: usize) -> i64 {
        if n < slice_count {
            let flag = u32::from_le_bytes([hdr[n * 8], hdr[n * 8 + 1], hdr[n * 8 + 2], hdr[n * 8 + 3]]);
            let o = &hdr[n * 8 + 4..n * 8 + 8];
            let v = if flag == 1 { u32::from_le_bytes([o[0], o[1], o[2], o[3]]) } else { u32::from_be_bytes([o[0], o[1], o[2], o[3]]) };
            v as i32 as i64
        } else {
            buf_size as i64
        }
    }

    /// `ff_rv34_decode_frame` for a non-empty packet.
    fn decode_frame(&mut self, pkt: &[u8], pts: Option<i64>) -> Result<()> {
        let slice_count = pkt[0] as usize + 1;
        let hdr_end = 1 + 8 * slice_count;
        if pkt.len() < hdr_end {
            return Err(Error::invalid("rv34: truncated slice table"));
        }
        let hdr = &pkt[1..hdr_end];
        let buf = &pkt[hdr_end..];
        let buf_size = buf.len();

        let offset = Self::get_slice_offset(hdr, 0, slice_count, buf_size);
        if offset < 0 || offset > buf_size as i64 {
            return Err(Error::invalid("rv34: slice offset is invalid"));
        }
        let offset = offset as usize;
        let mut gb = BitReader::new(&buf[offset..], buf_size - offset);
        let si = match self.parse_slice_header(&mut gb) {
            Some(si) if si.start == 0 => si,
            _ => return Err(Error::invalid("rv34: first slice header is incorrect")),
        };
        let faulty_b = self.last.is_none() && si.ty == PICT_B;

        // First slice: start a new picture.
        if self.mb_num_left > 0 && self.cur.is_some() {
            self.abandon_current();
        }
        if self.g.width as i32 != si.width || self.g.height as i32 != si.height || self.context_reinit {
            let (w, h) = (si.width.max(0) as usize, si.height.max(0) as usize);
            check_dimensions(w, h)?;
            self.set_dimensions(w, h);
        }
        if faulty_b {
            // FFmpeg drops a B-frame with no reference (e.g. right after a
            // seek) without output; it is not stream corruption.
            return Ok(());
        }
        // A picture left unfinished by a previous packet ends here.
        self.abandon_current();
        self.pict_type = if si.ty != 0 { si.ty } else { PICT_I };
        self.frame_start(pts);
        self.cur_pts = si.pts;
        if self.pict_type != PICT_B {
            self.last_pts = self.next_pts;
            self.next_pts = self.cur_pts;
        } else {
            let pts_diff = |a: i32, b: i32| (a - b + 8192) & 0x1FFF;
            let refdist = pts_diff(self.next_pts, self.last_pts);
            let dist0 = pts_diff(self.cur_pts, self.last_pts);
            let dist1 = pts_diff(self.next_pts, self.cur_pts);
            if refdist == 0 {
                self.mv_weight1 = 8192;
                self.mv_weight2 = 8192;
                self.weight1 = 8192;
                self.weight2 = 8192;
                self.scaled_weight = false;
            } else {
                self.mv_weight1 = (dist0 << 14) / refdist;
                self.mv_weight2 = (dist1 << 14) / refdist;
                if (self.mv_weight1 | self.mv_weight2) & 511 != 0 {
                    self.weight1 = self.mv_weight1;
                    self.weight2 = self.mv_weight2;
                    self.scaled_weight = false;
                } else {
                    self.weight1 = self.mv_weight1 >> 9;
                    self.weight2 = self.mv_weight2 >> 9;
                    self.scaled_weight = true;
                }
            }
        }
        self.mb_x = 0;
        self.mb_y = 0;

        let g = self.g;
        let mut last = false;
        for i in 0..slice_count {
            let offset = Self::get_slice_offset(hdr, i, slice_count, buf_size);
            let offset1 = Self::get_slice_offset(hdr, i + 1, slice_count, buf_size);
            if offset < 0 || offset > offset1 || offset1 > buf_size as i64 {
                break;
            }
            let mut size = (offset1 - offset) as usize;
            self.si.end = (g.mb_width * g.mb_height) as i32;
            self.mb_num_left = (self.mb_x + self.mb_y * g.mb_width) as i32 - self.si.start;
            if i + 1 < slice_count {
                let offset2 = Self::get_slice_offset(hdr, i + 2, slice_count, buf_size);
                if offset2 < offset1 || offset2 > buf_size as i64 {
                    break;
                }
                let o1 = offset1 as usize;
                let mut gb = BitReader::new(&buf[o1..], buf_size - o1);
                match self.parse_slice_header(&mut gb) {
                    None => size = (offset2 - offset) as usize,
                    Some(next_si) => self.si.end = next_si.start,
                }
            }
            let offset = offset as usize;
            let end = self.si.end;
            if let SliceEnd::Last = self.decode_slice(end, &buf[offset..], size) {
                last = true;
                break;
            }
        }
        if self.cur.is_some() && last {
            self.loop_filter(g.mb_height - 1);
            self.finish_frame();
        }
        Ok(())
    }
}

/// `adjust_pred16`.
fn adjust_pred16(itype: usize, up: bool, left: bool) -> usize {
    let mut itype = itype;
    if !up && !left {
        itype = DC_128_PRED8X8;
    } else if !up {
        if itype == PLANE_PRED8X8 {
            itype = HOR_PRED8X8;
        }
        if itype == VERT_PRED8X8 {
            itype = HOR_PRED8X8;
        }
        if itype == DC_PRED8X8 {
            itype = LEFT_DC_PRED8X8;
        }
    } else if !left {
        if itype == PLANE_PRED8X8 {
            itype = VERT_PRED8X8;
        }
        if itype == HOR_PRED8X8 {
            itype = VERT_PRED8X8;
        }
        if itype == DC_PRED8X8 {
            itype = TOP_DC_PRED8X8;
        }
    }
    itype
}

impl Decoder for Rv34Decoder {
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
        let (frame, size) = self.ready.pop_front().ok_or(Error::NeedMore)?;
        self.last_output = Some(size);
        Ok(frame)
    }

    /// The frame last returned; before the first, the next queued one.
    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        self.last_output
            .or_else(|| self.ready.front().map(|(_, size)| *size))
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.output_video_dimensions().map(|_| PixelFormat::Yuv420P)
    }

    fn flush(&mut self) -> Result<()> {
        // "special case for last picture": output the delayed reference.
        if let Some(next) = self.next.take() {
            let f = self.output_frame(&next);
            self.ready.push_back((f, self.output_size()));
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // ff_mpeg_flush: drop every picture and pending output.
        self.cur = None;
        self.last = None;
        self.next = None;
        self.ready.clear();
        self.last_output = None;
        self.mb_num_left = 0;
        self.mb_x = 0;
        self.mb_y = 0;
        Ok(())
    }
}

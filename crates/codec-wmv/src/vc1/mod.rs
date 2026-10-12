//! VC-1 (SMPTE 421M) and WMV3 (Windows Media Video 9) decoder.
//!
//! Ported from FFmpeg commit 2da55bf (LGPL-2.1-or-later): `vc1dec.c`
//! (decoder driver, slices, fields, output order), `vc1.c` (sequence /
//! entry-point / picture headers, bitplanes, intensity-compensation LUTs),
//! `vc1_block.c` (macroblock layer, see `block.rs`), `vc1_pred.c`
//! (`pred.rs`), `vc1_mc.c` (`mc.rs`), `vc1_loopfilter.c` (`lf.rs`),
//! `vc1dsp.c` (`dsp.rs`), `vc1data.c` / `vc1_vlc_data.h` / `vc1acdata.h`
//! (`vc1_tables.rs`) and the picture management of `mpegvideo_dec.c`.

mod block;
mod dsp;
mod lf;
mod mc;
mod pred;

use std::sync::LazyLock;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result};

use crate::bits::BitReader;
use crate::mpv::Picture;
use crate::demuxers::{CODEC_ID_VC1, CODEC_ID_WMV3};
use crate::tables::{MSMP4_DC_TABLES, MSMP4_MB_I_TABLE, WMV1_SCANTABLE, WMV2_SCANTABLE_A, WMV2_SCANTABLE_B};
use crate::vc1_tables::*;
use crate::vlc::Vlc;
use crate::x8::IntraX8;

pub(crate) const PROFILE_ADVANCED: i32 = 3;

const VC1_CODE_ENDOFSEQ: u32 = 0x10A;
const VC1_CODE_SLICE: u32 = 0x10B;
const VC1_CODE_FIELD: u32 = 0x10C;
const VC1_CODE_FRAME: u32 = 0x10D;
const VC1_CODE_ENTRYPOINT: u32 = 0x10E;
const VC1_CODE_SEQHDR: u32 = 0x10F;

pub(crate) const PICT_I: u8 = 1;
pub(crate) const PICT_P: u8 = 2;
pub(crate) const PICT_B: u8 = 3;
pub(crate) const PICT_BI: u8 = 7;

// Frame coding modes.
pub(crate) const PROGRESSIVE: u8 = 0;
pub(crate) const ILACE_FRAME: u8 = 1;
pub(crate) const ILACE_FIELD: u8 = 2;

// Quantizer modes.
const QUANT_FRAME_IMPLICIT: i32 = 0;
const QUANT_FRAME_EXPLICIT: i32 = 1;
const QUANT_NON_UNIFORM: i32 = 2;

// DQ profiles.
pub(crate) const DQPROFILE_FOUR_EDGES: u8 = 0;
pub(crate) const DQPROFILE_DOUBLE_EDGES: u8 = 1;
pub(crate) const DQPROFILE_SINGLE_EDGE: u8 = 2;
pub(crate) const DQPROFILE_ALL_MBS: u8 = 3;

// MV modes.
pub(crate) const MV_PMODE_1MV_HPEL_BILIN: u8 = 0;
pub(crate) const MV_PMODE_1MV: u8 = 1;
pub(crate) const MV_PMODE_1MV_HPEL: u8 = 2;
pub(crate) const MV_PMODE_MIXED_MV: u8 = 3;
pub(crate) const MV_PMODE_INTENSITY_COMP: u8 = 4;

// Interlaced frame P MB modes.
pub(crate) const MV_PMODE_INTFR_1MV: u8 = 0;
pub(crate) const MV_PMODE_INTFR_2MV_FIELD: u8 = 1;
pub(crate) const MV_PMODE_INTFR_4MV_FIELD: u8 = 3;
pub(crate) const MV_PMODE_INTFR_4MV: u8 = 4;
pub(crate) const MV_PMODE_INTFR_INTRA: u8 = 5;

// B MV types.
pub(crate) const BMV_TYPE_BACKWARD: i32 = 0;
pub(crate) const BMV_TYPE_FORWARD: i32 = 1;
pub(crate) const BMV_TYPE_INTERPOLATED: i32 = 2;
pub(crate) const BMV_TYPE_DIRECT: i32 = 3;

// Transform types.
pub(crate) const TT_8X8: i32 = 0;
pub(crate) const TT_8X4_BOTTOM: i32 = 1;
pub(crate) const TT_8X4_TOP: i32 = 2;
pub(crate) const TT_8X4: i32 = 3;
pub(crate) const TT_4X8_RIGHT: i32 = 4;
pub(crate) const TT_4X8_LEFT: i32 = 5;
pub(crate) const TT_4X8: i32 = 6;
pub(crate) const TT_4X4: i32 = 7;

// Coding sets.
pub(crate) const CS_HIGH_MOT_INTRA: usize = 0;
pub(crate) const CS_HIGH_MOT_INTER: usize = 1;
pub(crate) const CS_LOW_MOT_INTRA: usize = 2;
pub(crate) const CS_LOW_MOT_INTER: usize = 3;
pub(crate) const CS_MID_RATE_INTRA: usize = 4;
pub(crate) const CS_MID_RATE_INTER: usize = 5;
pub(crate) const CS_HIGH_RATE_INTRA: usize = 6;
pub(crate) const CS_HIGH_RATE_INTER: usize = 7;

// Overlap conditions.
pub(crate) const CONDOVER_NONE: u8 = 0;
pub(crate) const CONDOVER_ALL: u8 = 1;
pub(crate) const CONDOVER_SELECT: u8 = 2;

// Imodes.
const IMODE_RAW: i32 = 0;
const IMODE_NORM2: i32 = 1;
const IMODE_DIFF2: i32 = 2;
const IMODE_NORM6: i32 = 3;
const IMODE_DIFF6: i32 = 4;
const IMODE_ROWSKIP: i32 = 5;
const IMODE_COLSKIP: i32 = 6;

// Picture mb_type flags (subset of mpegutils.h semantics).
pub(crate) const MB_TYPE_INTRA: u8 = 1;
pub(crate) const MB_TYPE_SKIP: u8 = 2;
pub(crate) const MB_TYPE_16X16: u8 = 4;

pub(crate) const B_FRACTION_DEN: i32 = 256;

/// `ff_vc1_bfraction_lut` (B_FRACTION_DEN == 256).
const VC1_BFRACTION_LUT: [i16; 23] =
    [128, 85, 170, 64, 192, 51, 102, 153, 204, 43, 215, 37, 74, 111, 148, 185, 222, 32, 96, 160, 224, -1, 0];

// ───────────────────────── VLC tables ─────────────────────────

pub(crate) struct Vc1Vlcs {
    pub imode: Vlc,
    pub norm2: Vlc,
    pub norm6: Vlc,
    pub ttmb: Vec<Vlc>,
    pub ttblk: Vec<Vlc>,
    pub subblkpat: Vec<Vlc>,
    pub fourmv_block_pattern: Vec<Vlc>,
    pub cbpcy_p: Vec<Vlc>,
    pub mv_diff: Vec<Vlc>,
    pub intfr_4mv_mbmode: Vec<Vlc>,
    pub intfr_non4mv_mbmode: Vec<Vlc>,
    pub onemv_ref: Vec<Vlc>,
    pub twomv_block_pattern: Vec<Vlc>,
    pub ac_coeff: Vec<Vlc>,
    pub tworef_mvdata: Vec<Vlc>,
    pub icbpcy: Vec<Vlc>,
    pub if_mmv_mbmode: Vec<Vlc>,
    pub if_1mv_mbmode: Vec<Vlc>,
    pub msmp4_dc: [[Vlc; 2]; 2],
    pub msmp4_mb_i: Vlc,
}

fn vlc_cb<C: Copy + Into<u32>>(bits: u32, codes: &[C], lens: &[u8]) -> Vlc {
    let pairs: Vec<(u32, u8)> = codes.iter().zip(lens.iter()).map(|(&c, &l)| (c.into(), l)).collect();
    Vlc::new(bits, &pairs, None)
}

pub(crate) static VLCS: LazyLock<Vc1Vlcs> = LazyLock::new(|| Vc1Vlcs {
    imode: vlc_cb(4, &VC1_IMODE_CODES, &VC1_IMODE_BITS),
    norm2: vlc_cb(3, &VC1_NORM2_CODES, &VC1_NORM2_BITS),
    norm6: vlc_cb(9, &VC1_NORM6_CODES, &VC1_NORM6_BITS),
    ttmb: (0..3).map(|i| vlc_cb(9, &VC1_TTMB_CODES[i], &VC1_TTMB_BITS[i])).collect(),
    ttblk: (0..3).map(|i| vlc_cb(5, &VC1_TTBLK_CODES[i], &VC1_TTBLK_BITS[i])).collect(),
    subblkpat: (0..3).map(|i| vlc_cb(6, &VC1_SUBBLKPAT_CODES[i], &VC1_SUBBLKPAT_BITS[i])).collect(),
    fourmv_block_pattern: (0..4)
        .map(|i| vlc_cb(6, &VC1_4MV_BLOCK_PATTERN_CODES[i], &VC1_4MV_BLOCK_PATTERN_BITS[i]))
        .collect(),
    cbpcy_p: (0..4).map(|i| vlc_cb(9, &VC1_CBPCY_P_CODES[i], &VC1_CBPCY_P_BITS[i])).collect(),
    mv_diff: (0..4).map(|i| vlc_cb(9, &VC1_MV_DIFF_CODES[i], &VC1_MV_DIFF_BITS[i])).collect(),
    intfr_4mv_mbmode: (0..4)
        .map(|i| vlc_cb(9, &VC1_INTFR_4MV_MBMODE_CODES[i], &VC1_INTFR_4MV_MBMODE_BITS[i]))
        .collect(),
    intfr_non4mv_mbmode: (0..4)
        .map(|i| vlc_cb(6, &VC1_INTFR_NON4MV_MBMODE_CODES[i], &VC1_INTFR_NON4MV_MBMODE_BITS[i]))
        .collect(),
    onemv_ref: (0..4).map(|i| vlc_cb(9, &VC1_1REF_MVDATA_CODES[i], &VC1_1REF_MVDATA_BITS[i])).collect(),
    twomv_block_pattern: (0..4)
        .map(|i| vlc_cb(3, &VC1_2MV_BLOCK_PATTERN_CODES[i], &VC1_2MV_BLOCK_PATTERN_BITS[i]))
        .collect(),
    ac_coeff: (0..8)
        .map(|i| {
            let n = VC1_AC_SIZES[i];
            let pairs: Vec<(u32, u8)> = VC1_AC_TABLES[i][..n].iter().map(|e| (e[0], e[1] as u8)).collect();
            Vlc::new(9, &pairs, None)
        })
        .collect(),
    tworef_mvdata: (0..8).map(|i| vlc_cb(9, &VC1_2REF_MVDATA_CODES[i], &VC1_2REF_MVDATA_BITS[i])).collect(),
    icbpcy: (0..8).map(|i| vlc_cb(9, &VC1_ICBPCY_P_CODES[i], &VC1_ICBPCY_P_BITS[i])).collect(),
    if_mmv_mbmode: (0..8).map(|i| vlc_cb(5, &VC1_IF_MMV_MBMODE_CODES[i], &VC1_IF_MMV_MBMODE_BITS[i])).collect(),
    if_1mv_mbmode: (0..8).map(|i| vlc_cb(5, &VC1_IF_1MV_MBMODE_CODES[i], &VC1_IF_1MV_MBMODE_BITS[i])).collect(),
    msmp4_dc: [
        [
            Vlc::new(9, &MSMP4_DC_TABLES[0][0].iter().map(|e| (e[0], e[1] as u8)).collect::<Vec<_>>(), None),
            Vlc::new(9, &MSMP4_DC_TABLES[0][1].iter().map(|e| (e[0], e[1] as u8)).collect::<Vec<_>>(), None),
        ],
        [
            Vlc::new(9, &MSMP4_DC_TABLES[1][0].iter().map(|e| (e[0], e[1] as u8)).collect::<Vec<_>>(), None),
            Vlc::new(9, &MSMP4_DC_TABLES[1][1].iter().map(|e| (e[0], e[1] as u8)).collect::<Vec<_>>(), None),
        ],
    ],
    msmp4_mb_i: Vlc::new(9, &MSMP4_MB_I_TABLE.iter().map(|e| (e[0] as u32, e[1] as u8)).collect::<Vec<_>>(), None),
});

/// Which VLC table a picture-level selector points at.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum VlcSel {
    #[default]
    None,
    CbpcyP(usize),
    Icbpcy(usize),
    Intfr4mv(usize),
    IntfrNon4mv(usize),
    IfMmv(usize),
    If1mv(usize),
    OneRef(usize),
    TwoRef(usize),
    TwoMvBp(usize),
    FourMvBp(usize),
}

impl VlcSel {
    pub fn vlc(self) -> Option<&'static Vlc> {
        let t = &*VLCS;
        Some(match self {
            VlcSel::None => return None,
            VlcSel::CbpcyP(i) => &t.cbpcy_p[i],
            VlcSel::Icbpcy(i) => &t.icbpcy[i],
            VlcSel::Intfr4mv(i) => &t.intfr_4mv_mbmode[i],
            VlcSel::IntfrNon4mv(i) => &t.intfr_non4mv_mbmode[i],
            VlcSel::IfMmv(i) => &t.if_mmv_mbmode[i],
            VlcSel::If1mv(i) => &t.if_1mv_mbmode[i],
            VlcSel::OneRef(i) => &t.onemv_ref[i],
            VlcSel::TwoRef(i) => &t.tworef_mvdata[i],
            VlcSel::TwoMvBp(i) => &t.twomv_block_pattern[i],
            VlcSel::FourMvBp(i) => &t.fourmv_block_pattern[i],
        })
    }
}

// ───────────────────────── pictures ─────────────────────────

/// A decoded picture with the per-picture side data VC-1 B-pictures use.
pub(crate) struct VPic {
    pub pic: Picture,
    /// `motion_val[dir]`, origin at `MvOrigin`.
    pub motion_val: [Vec<[i16; 2]>; 2],
    pub mb_type: Vec<u8>,
    pub field_picture: bool,
    pub interlaced: bool,
}

/// Transform selection (the WMV3 `res_fasttx == 0` path uses the simple
/// IDCT family instead of the VC-1 transforms).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransformKind {
    Vc1,
    Simple,
}

pub struct Vc1Decoder {
    codec_id: CodecId,
    pub(crate) is_vc1: bool,
    /// Container-supplied size (WMV3) or sequence/entry-point size.
    pub(crate) coded_width: i32,
    pub(crate) coded_height: i32,
    seq_initialized: bool,
    ep_initialized: bool,
    pub(crate) transform: TransformKind,

    // ── sequence header ──
    pub(crate) profile: i32,
    pub(crate) res_sprite: bool,
    res_y411: bool,
    pub(crate) res_x8: bool,
    pub(crate) multires: bool,
    pub(crate) res_fasttx: bool,
    pub(crate) rangered: bool,
    pub(crate) res_rtm_flag: bool,
    level: i32,
    chromaformat: i32,
    postprocflag: bool,
    broadcast: bool,
    pub(crate) interlace: bool,
    tfcntrflag: bool,
    panscanflag: bool,
    refdist_flag: bool,
    pub(crate) extended_dmv: bool,
    hrd_param_flag: bool,
    hrd_num_leaky_buckets: i32,
    psf: bool,
    pub(crate) loop_filter: bool,
    max_coded_width: i32,
    max_coded_height: i32,
    pub(crate) fastuvmc: bool,
    pub(crate) extended_mv: bool,
    pub(crate) dquant: i32,
    pub(crate) vstransform: bool,
    pub(crate) overlap: bool,
    max_b_frames: i32,
    quantizer_mode: i32,
    finterpflag: bool,
    resync_marker: bool,
    broken_link: bool,
    closed_entry: bool,

    // ── picture header ──
    pub(crate) mv_mode: u8,
    pub(crate) mv_mode2: u8,
    pub(crate) k_x: i32,
    pub(crate) k_y: i32,
    pub(crate) range_x: i32,
    pub(crate) range_y: i32,
    pub(crate) pq: i32,
    pub(crate) altpq: i32,
    pub(crate) zz_8x8: [[u8; 64]; 4],
    pub(crate) left_blk_sh: u32,
    pub(crate) top_blk_sh: u32,
    pub(crate) zz_8x4: [u8; 64],
    pub(crate) zz_4x8: [u8; 64],
    pub(crate) zzi_8x8: [u8; 64],
    pub(crate) dquantfrm: bool,
    pub(crate) dqprofile: u8,
    pub(crate) dqsbedge: u8,
    pub(crate) dqbilevel: bool,
    pub(crate) dc_table_index: usize,
    pub(crate) c_ac_table_index: u32,
    pub(crate) y_ac_table_index: u32,
    pub(crate) esc3_level_length: u32,
    pub(crate) esc3_run_length: u32,
    pub(crate) ttfrm: i32,
    pub(crate) ttmbf: bool,
    pub(crate) codingset: usize,
    pub(crate) codingset2: usize,
    pub(crate) pqindex: i32,
    pub(crate) a_avail: bool,
    pub(crate) c_avail: bool,
    pub(crate) lumscale: i32,
    pub(crate) lumshift: i32,
    pub(crate) bfraction: i32,
    pub(crate) halfpq: i32,
    respic: i32,
    pub(crate) mvrange: i32,
    pub(crate) pquantizer: bool,
    pub(crate) cbpcy_vlc: VlcSel,
    pub(crate) tt_index: usize,
    pub(crate) mv_table_index: usize,
    pub(crate) mv_type_is_raw: bool,
    pub(crate) dmb_is_raw: bool,
    pub(crate) fmb_is_raw: bool,
    pub(crate) skip_is_raw: bool,
    pub(crate) last_luty: [[u8; 256]; 2],
    pub(crate) last_lutuv: [[u8; 256]; 2],
    pub(crate) aux_luty: [[u8; 256]; 2],
    pub(crate) aux_lutuv: [[u8; 256]; 2],
    pub(crate) next_luty: [[u8; 256]; 2],
    pub(crate) next_lutuv: [[u8; 256]; 2],
    /// `curr_luty` / `curr_lutuv` / `curr_use_ic` alias: true = aux, false = next.
    pub(crate) curr_is_aux: bool,
    pub(crate) last_use_ic: bool,
    pub(crate) next_use_ic: bool,
    pub(crate) aux_use_ic: bool,
    pub(crate) last_interlaced: bool,
    pub(crate) next_interlaced: bool,
    pub(crate) rnd: i32,
    pub(crate) rangeredfrm: bool,
    interpfrm: bool,
    pub(crate) fcm: u8,
    rptfrm: i32,
    pub(crate) tff: bool,
    rff: bool,
    uvsamp: bool,
    postproc: i32,
    pub(crate) acpred_is_raw: bool,
    pub(crate) overflg_is_raw: bool,
    pub(crate) condover: u8,
    pub(crate) dmvrange: i32,
    pub(crate) fourmvswitch: bool,
    pub(crate) intcomp: bool,
    pub(crate) lumscale2: i32,
    pub(crate) lumshift2: i32,
    pub(crate) mbmode_vlc: VlcSel,
    pub(crate) imv_vlc: VlcSel,
    pub(crate) twomvbp_vlc: VlcSel,
    pub(crate) fourmvbp_vlc: VlcSel,
    pub(crate) twomvbp: i32,
    pub(crate) fourmvbp: i32,
    pub(crate) fieldtx_is_raw: bool,
    pub(crate) field_mode: bool,
    pub(crate) fptype: i32,
    pub(crate) second_field: bool,
    pub(crate) refdist: i32,
    pub(crate) numref: i32,
    pub(crate) reffield: i32,
    pub(crate) intcompfield: i32,
    pub(crate) cur_field_type: i32,
    pub(crate) ref_field_type: [i32; 2],
    pub(crate) blocks_off: isize,
    pub(crate) mb_off: isize,
    pub(crate) bmvtype: i32,
    pub(crate) frfd: i32,
    pub(crate) brfd: i32,
    first_pic_header_flag: bool,
    pic_header_flag: bool,
    pub(crate) p_frame_skipped: bool,
    pub(crate) bi_type: bool,
    pub(crate) x8_type: bool,

    // ── per-MB planes and state (VC1Context) ──
    pub(crate) mv_type_mb_plane: Vec<u8>,
    pub(crate) direct_mb_plane: Vec<u8>,
    pub(crate) forward_mb_plane: Vec<u8>,
    pub(crate) fieldtx_plane: Vec<u8>,
    pub(crate) acpred_plane: Vec<u8>,
    pub(crate) over_flags_plane: Vec<u8>,
    pub(crate) mbskip_table: Vec<u8>,
    /// Ring of `n_allocated_blks` macroblocks x 6 blocks x 64 coefficients.
    pub(crate) blk: Vec<i16>,
    pub(crate) n_allocated_blks: usize,
    pub(crate) cur_blk_idx: isize,
    pub(crate) left_blk_idx: isize,
    pub(crate) topleft_blk_idx: isize,
    pub(crate) top_blk_idx: isize,
    /// `cbp_base`, 3 rows of `mb_stride`; `cbp` = base + 2 * mb_stride.
    pub(crate) cbp_base: Vec<u32>,
    pub(crate) ttblk_base: Vec<i32>,
    pub(crate) is_intra_base: Vec<u8>,
    pub(crate) luma_mv_base: Vec<[i16; 2]>,
    /// Block-level intra flags (`v->mb_type`), origin `b8_stride + 1`.
    pub(crate) vmb_type: Vec<u8>,
    pub(crate) blk_mv_type: Vec<u8>,
    /// `mv_f[2]` / `mv_f_next[2]`, origin `b8_stride + 1`; the second
    /// direction starts at `mv_f_size`.
    pub(crate) mv_f: Vec<u8>,
    pub(crate) mv_f_next: Vec<u8>,
    pub(crate) mv_f_size: usize,
    /// `v->blocks` (B-picture scratch blocks).
    pub(crate) blocks: [[i16; 64]; 6],

    // ── MpegEncContext subset ──
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) mb_width: usize,
    pub(crate) mb_height: usize,
    pub(crate) alloc_mb_height: usize,
    pub(crate) mb_stride: usize,
    pub(crate) b8_stride: usize,
    pub(crate) h_edge_pos: i32,
    pub(crate) v_edge_pos: i32,
    pub(crate) linesize: usize,
    pub(crate) uvlinesize: usize,
    pub(crate) dc_val: Vec<i16>,
    pub(crate) ac_val: Vec<[i16; 16]>,
    pub(crate) coded_block: Vec<u8>,
    pub(crate) qscale_table: Vec<i8>,
    pub(crate) cur: Option<VPic>,
    pub(crate) last: Option<VPic>,
    pub(crate) next: Option<VPic>,
    spare: Vec<VPic>,
    pub(crate) mb_x: usize,
    pub(crate) mb_y: usize,
    pub(crate) first_slice_line: bool,
    pub(crate) start_mb_y: usize,
    pub(crate) end_mb_y: usize,
    pub(crate) end_mb_x: usize,
    pub(crate) block_index: [isize; 6],
    pub(crate) dest: [isize; 3],
    pub(crate) mb_intra: bool,
    pub(crate) ac_pred: bool,
    pub(crate) mv: [[[i32; 2]; 4]; 2],
    pub(crate) mspel: bool,
    pub(crate) quarter_sample: bool,
    pub(crate) y_dc_scale: i32,
    pub(crate) pict_type: u8,
    low_delay: bool,
    pub(crate) x8: Option<IntraX8>,

    /// Output frames not yet returned, each with the size it was cropped
    /// to (the size in force when it was output; `init_context` drops the
    /// references when the coded size changes).
    pending: std::collections::VecDeque<(Frame, (u32, u32))>,
    /// Size of the frame `receive_frame` last returned.
    last_output: Option<(u32, u32)>,
}

/// Index helpers for the arrays allocated "so they can be used with
/// `block_index[]`" (origin `b8_stride + 1`).
impl Vc1Decoder {
    #[inline]
    pub(crate) fn bo(&self) -> isize {
        (self.b8_stride + 1) as isize
    }
    /// Origin of the per-picture `motion_val` arrays.
    #[inline]
    pub(crate) fn mvo(&self) -> isize {
        (2 * self.b8_stride + 4) as isize
    }
    /// `v->cbp[i]` etc. live at `base + 2 * mb_stride + i`.
    #[inline]
    pub(crate) fn row3(&self, i: isize) -> usize {
        (2 * self.mb_stride as isize + i) as usize
    }
}

impl Vc1Decoder {
    pub fn new_wmv3(params: &CodecParameters) -> Result<Self> {
        let mut d = Self::blank(CODEC_ID_WMV3, false);
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as i32, h as i32),
            _ => return Err(Error::invalid("wmv3: container must supply width/height")),
        };
        d.coded_width = w;
        d.coded_height = h;
        if params.extradata.is_empty() {
            return Err(Error::invalid("wmv3: missing sequence header extradata"));
        }
        let ext = params.extradata.clone();
        let mut gb = BitReader::new(&ext);
        d.decode_sequence_header(&mut gb)?;
        d.seq_initialized = true;
        d.ep_initialized = true;
        d.finish_init()?;
        Ok(d)
    }

    pub fn new_vc1(params: &CodecParameters) -> Result<Self> {
        let mut d = Self::blank(CODEC_ID_VC1, true);
        if let (Some(w), Some(h)) = (params.width, params.height) {
            d.coded_width = w as i32;
            d.coded_height = h as i32;
        }
        if !params.extradata.is_empty() {
            d.decode_extradata(&params.extradata.clone())?;
        }
        if d.seq_initialized && d.ep_initialized {
            d.finish_init()?;
        }
        Ok(d)
    }

    fn blank(id: &'static str, is_vc1: bool) -> Self {
        LazyLock::force(&VLCS);
        Vc1Decoder {
            codec_id: CodecId::new(id),
            is_vc1,
            coded_width: 0,
            coded_height: 0,
            seq_initialized: false,
            ep_initialized: false,
            transform: TransformKind::Vc1,
            profile: 0,
            res_sprite: false,
            res_y411: false,
            res_x8: false,
            multires: false,
            res_fasttx: false,
            rangered: false,
            res_rtm_flag: false,
            level: 0,
            chromaformat: 1,
            postprocflag: false,
            broadcast: false,
            interlace: false,
            tfcntrflag: false,
            panscanflag: false,
            refdist_flag: false,
            extended_dmv: false,
            hrd_param_flag: false,
            hrd_num_leaky_buckets: 0,
            psf: false,
            loop_filter: false,
            max_coded_width: 0,
            max_coded_height: 0,
            fastuvmc: false,
            extended_mv: false,
            dquant: 0,
            vstransform: false,
            overlap: false,
            max_b_frames: 0,
            quantizer_mode: 0,
            finterpflag: false,
            resync_marker: false,
            broken_link: false,
            closed_entry: false,
            mv_mode: 0,
            mv_mode2: 0,
            k_x: 9,
            k_y: 8,
            range_x: 256,
            range_y: 128,
            pq: -1,
            altpq: 0,
            zz_8x8: [[0; 64]; 4],
            left_blk_sh: 0,
            top_blk_sh: 3,
            zz_8x4: [0; 64],
            zz_4x8: [0; 64],
            zzi_8x8: [0; 64],
            dquantfrm: false,
            dqprofile: 0,
            dqsbedge: 0,
            dqbilevel: false,
            dc_table_index: 0,
            c_ac_table_index: 0,
            y_ac_table_index: 0,
            esc3_level_length: 0,
            esc3_run_length: 0,
            ttfrm: 0,
            ttmbf: false,
            codingset: 0,
            codingset2: 0,
            pqindex: 0,
            a_avail: false,
            c_avail: false,
            lumscale: 0,
            lumshift: 0,
            bfraction: 0,
            halfpq: 0,
            respic: 0,
            mvrange: 0,
            pquantizer: false,
            cbpcy_vlc: VlcSel::None,
            tt_index: 0,
            mv_table_index: 0,
            mv_type_is_raw: false,
            dmb_is_raw: false,
            fmb_is_raw: false,
            skip_is_raw: false,
            last_luty: [[0; 256]; 2],
            last_lutuv: [[0; 256]; 2],
            aux_luty: [[0; 256]; 2],
            aux_lutuv: [[0; 256]; 2],
            next_luty: [[0; 256]; 2],
            next_lutuv: [[0; 256]; 2],
            curr_is_aux: false,
            last_use_ic: false,
            next_use_ic: false,
            aux_use_ic: false,
            last_interlaced: false,
            next_interlaced: false,
            rnd: 0,
            rangeredfrm: false,
            interpfrm: false,
            fcm: PROGRESSIVE,
            rptfrm: 0,
            tff: false,
            rff: false,
            uvsamp: false,
            postproc: 0,
            acpred_is_raw: false,
            overflg_is_raw: false,
            condover: 0,
            dmvrange: 0,
            fourmvswitch: false,
            intcomp: false,
            lumscale2: 0,
            lumshift2: 0,
            mbmode_vlc: VlcSel::None,
            imv_vlc: VlcSel::None,
            twomvbp_vlc: VlcSel::None,
            fourmvbp_vlc: VlcSel::None,
            twomvbp: 0,
            fourmvbp: 0,
            fieldtx_is_raw: false,
            field_mode: false,
            fptype: 0,
            second_field: false,
            refdist: 0,
            numref: 0,
            reffield: 0,
            intcompfield: 0,
            cur_field_type: 0,
            ref_field_type: [0; 2],
            blocks_off: 0,
            mb_off: 0,
            bmvtype: 0,
            frfd: 0,
            brfd: 0,
            first_pic_header_flag: false,
            pic_header_flag: false,
            p_frame_skipped: false,
            bi_type: false,
            x8_type: false,
            mv_type_mb_plane: Vec::new(),
            direct_mb_plane: Vec::new(),
            forward_mb_plane: Vec::new(),
            fieldtx_plane: Vec::new(),
            acpred_plane: Vec::new(),
            over_flags_plane: Vec::new(),
            mbskip_table: Vec::new(),
            blk: Vec::new(),
            n_allocated_blks: 0,
            cur_blk_idx: 0,
            left_blk_idx: 0,
            topleft_blk_idx: 0,
            top_blk_idx: 0,
            cbp_base: Vec::new(),
            ttblk_base: Vec::new(),
            is_intra_base: Vec::new(),
            luma_mv_base: Vec::new(),
            vmb_type: Vec::new(),
            blk_mv_type: Vec::new(),
            mv_f: Vec::new(),
            mv_f_next: Vec::new(),
            mv_f_size: 0,
            blocks: [[0; 64]; 6],
            width: 0,
            height: 0,
            mb_width: 0,
            mb_height: 0,
            alloc_mb_height: 0,
            mb_stride: 0,
            b8_stride: 0,
            h_edge_pos: 0,
            v_edge_pos: 0,
            linesize: 0,
            uvlinesize: 0,
            dc_val: Vec::new(),
            ac_val: Vec::new(),
            coded_block: Vec::new(),
            qscale_table: Vec::new(),
            cur: None,
            last: None,
            next: None,
            spare: Vec::new(),
            mb_x: 0,
            mb_y: 0,
            first_slice_line: true,
            start_mb_y: 0,
            end_mb_y: 0,
            end_mb_x: 0,
            block_index: [0; 6],
            dest: [0; 3],
            mb_intra: false,
            ac_pred: false,
            mv: [[[0; 2]; 4]; 2],
            mspel: false,
            quarter_sample: false,
            y_dc_scale: 0,
            pict_type: 0,
            low_delay: true,
            x8: None,
            pending: std::collections::VecDeque::new(),
            last_output: None,
        }
    }

    /// The rest of `vc1_decode_init` once the headers are known.
    fn finish_init(&mut self) -> Result<()> {
        if self.profile == PROFILE_ADVANCED || self.res_fasttx {
            // ff_vc1_init_transposed_scantables
            let tr = |x: u8| (x >> 3) | ((x & 7) << 3);
            for i in 0..64 {
                for t in 0..4 {
                    self.zz_8x8[t][i] = tr(WMV1_SCANTABLE[t][i]);
                }
                self.zzi_8x8[i] = tr(VC1_ADV_INTERLACED_8X8_ZZ[i]);
            }
            self.left_blk_sh = 0;
            self.top_blk_sh = 3;
            self.transform = TransformKind::Vc1;
        } else {
            self.zz_8x8 = WMV1_SCANTABLE;
            self.left_blk_sh = 3;
            self.top_blk_sh = 0;
            self.transform = TransformKind::Simple;
        }
        self.low_delay = self.max_b_frames == 0 || self.res_sprite;
        self.init_context()
    }

    /// `ff_mpv_common_init` + `vc1_decode_init_alloc_tables` for the current
    /// coded size.
    fn init_context(&mut self) -> Result<()> {
        let (w, h) = (self.coded_width, self.coded_height);
        if w <= 0 || h <= 0 {
            return Err(Error::invalid("vc1: invalid dimensions"));
        }
        if w > crate::MAX_DIM as i32 || h > crate::MAX_DIM as i32 || (w as u64) * (h as u64) > crate::MAX_PIXELS {
            return Err(Error::invalid("vc1: frame dimensions too large"));
        }
        self.width = w;
        self.height = h;
        self.mb_width = (w as usize).div_ceil(16);
        self.mb_height = (h as usize).div_ceil(16);
        let mb_height = self.mb_height.div_ceil(2) * 2;
        self.alloc_mb_height = mb_height;
        self.mb_stride = self.mb_width + 1;
        self.b8_stride = self.mb_width * 2 + 1;
        self.h_edge_pos = (self.mb_width * 16) as i32;
        self.v_edge_pos = (self.mb_height * 16) as i32;
        if self.profile == PROFILE_ADVANCED {
            self.h_edge_pos = w;
            self.v_edge_pos = h;
        }
        let mb_array = self.mb_stride * mb_height;
        let y_size = self.b8_stride * (2 * mb_height + 1);
        let c_size = self.mb_stride * (mb_height + 1);
        let yc_size = y_size + 2 * c_size;
        self.dc_val = vec![1024; yc_size];
        self.ac_val = vec![[0; 16]; yc_size];
        self.coded_block = vec![0; y_size + 2 * self.b8_stride];
        self.qscale_table = vec![0; (mb_height + 2) * self.mb_stride + 1];
        self.mbskip_table = vec![0; mb_array + 2];
        self.mv_type_mb_plane = vec![0; mb_array];
        self.direct_mb_plane = vec![0; mb_array];
        self.forward_mb_plane = vec![0; mb_array];
        self.fieldtx_plane = vec![0; mb_array];
        self.acpred_plane = vec![0; mb_array];
        self.over_flags_plane = vec![0; mb_array];
        self.n_allocated_blks = self.mb_width + 2;
        self.blk = vec![0; self.n_allocated_blks * 6 * 64];
        self.cbp_base = vec![0; 3 * self.mb_stride];
        self.ttblk_base = vec![0; 3 * self.mb_stride];
        self.is_intra_base = vec![0; 3 * self.mb_stride];
        self.luma_mv_base = vec![[0; 2]; 3 * self.mb_stride];
        let mbt = self.b8_stride * (mb_height * 2 + 1) + self.mb_stride * (mb_height + 1) * 2;
        self.vmb_type = vec![0; mbt];
        self.blk_mv_type = vec![0; self.b8_stride * (mb_height * 2 + 1)];
        self.mv_f_size = mbt;
        self.mv_f = vec![0; 2 * mbt];
        self.mv_f_next = vec![0; 2 * mbt];
        self.x8 = Some(IntraX8::new(self.mb_width, self.mb_height));
        self.cur = None;
        self.last = None;
        self.next = None;
        self.spare.clear();
        Ok(())
    }

    fn alloc_vpic(&mut self) -> VPic {
        if let Some(p) = self.spare.pop() {
            return p;
        }
        let mvlen = self.mvo() as usize + self.b8_stride * (2 * self.alloc_mb_height + 2) + 8;
        VPic {
            pic: Picture::new(self.mb_width * 16, self.alloc_mb_height * 16),
            motion_val: [vec![[0; 2]; mvlen], vec![[0; 2]; mvlen]],
            mb_type: vec![0; self.mb_stride * self.alloc_mb_height + 2],
            field_picture: false,
            interlaced: false,
        }
    }

    // ───────────────────────── headers (vc1.c) ─────────────────────────

    /// `ff_vc1_decode_sequence_header`.
    fn decode_sequence_header(&mut self, gb: &mut BitReader) -> Result<()> {
        self.profile = gb.read(2) as i32;
        if self.profile == PROFILE_ADVANCED {
            self.zz_8x4 = pad64(&VC1_ADV_PROGRESSIVE_8X4_ZZ);
            self.zz_4x8 = pad64(&VC1_ADV_PROGRESSIVE_4X8_ZZ);
            return self.decode_sequence_header_adv(gb);
        }
        self.chromaformat = 1;
        self.zz_8x4 = WMV2_SCANTABLE_A;
        self.zz_4x8 = WMV2_SCANTABLE_B;
        self.res_y411 = gb.read_bit() != 0;
        self.res_sprite = gb.read_bit() != 0;
        if self.res_y411 {
            return Err(Error::invalid("vc1: old interlaced mode is not supported"));
        }
        let _frmrtq = gb.read(3);
        let _bitrtq = gb.read(5);
        self.loop_filter = gb.read_bit() != 0;
        self.res_x8 = gb.read_bit() != 0;
        self.multires = gb.read_bit() != 0;
        self.res_fasttx = gb.read_bit() != 0;
        self.fastuvmc = gb.read_bit() != 0;
        if self.profile == 0 && !self.fastuvmc {
            return Err(Error::invalid("vc1: FASTUVMC unavailable in Simple Profile"));
        }
        self.extended_mv = gb.read_bit() != 0;
        if self.profile == 0 && self.extended_mv {
            return Err(Error::invalid("vc1: extended MVs unavailable in Simple Profile"));
        }
        self.dquant = gb.read(2) as i32;
        self.vstransform = gb.read_bit() != 0;
        if gb.read_bit() != 0 {
            return Err(Error::invalid("vc1: reserved RES_TRANSTAB set"));
        }
        self.overlap = gb.read_bit() != 0;
        self.resync_marker = gb.read_bit() != 0;
        self.rangered = gb.read_bit() != 0;
        self.max_b_frames = gb.read(3) as i32;
        self.quantizer_mode = gb.read(2) as i32;
        self.finterpflag = gb.read_bit() != 0;
        if self.res_sprite {
            let w = gb.read(11) as i32;
            let h = gb.read(11) as i32;
            if w <= 0 || h <= 0 {
                return Err(Error::invalid("vc1: invalid sprite dimensions"));
            }
            self.coded_width = w;
            self.coded_height = h;
            gb.skip(5);
            self.res_x8 = gb.read_bit() != 0;
            if gb.read_bit() != 0 {
                return Err(Error::invalid("vc1: unsupported sprite feature"));
            }
            gb.skip(3);
            self.res_rtm_flag = false;
        } else {
            self.res_rtm_flag = gb.read_bit() != 0;
        }
        if !self.res_fasttx {
            gb.skip(16);
        }
        Ok(())
    }

    /// `decode_sequence_header_adv`.
    fn decode_sequence_header_adv(&mut self, gb: &mut BitReader) -> Result<()> {
        self.res_rtm_flag = true;
        self.level = gb.read(3) as i32;
        self.chromaformat = gb.read(2) as i32;
        if self.chromaformat != 1 {
            return Err(Error::invalid("vc1: only 4:2:0 chroma format supported"));
        }
        let _frmrtq = gb.read(3);
        let _bitrtq = gb.read(5);
        self.postprocflag = gb.read_bit() != 0;
        self.max_coded_width = ((gb.read(12) + 1) << 1) as i32;
        self.max_coded_height = ((gb.read(12) + 1) << 1) as i32;
        self.broadcast = gb.read_bit() != 0;
        self.interlace = gb.read_bit() != 0;
        self.tfcntrflag = gb.read_bit() != 0;
        self.finterpflag = gb.read_bit() != 0;
        gb.skip(1);
        self.psf = gb.read_bit() != 0;
        if self.psf {
            return Err(Error::invalid("vc1: progressive segmented frames are not supported"));
        }
        self.max_b_frames = 7;
        if gb.read_bit() != 0 {
            // display extension: decoding is not affected
            gb.skip(14);
            gb.skip(14);
            let mut ar = 0;
            if gb.read_bit() != 0 {
                ar = gb.read(4);
            }
            if ar == 15 {
                gb.skip(8);
                gb.skip(8);
            }
            if gb.read_bit() != 0 {
                if gb.read_bit() != 0 {
                    gb.skip(16);
                } else {
                    gb.skip(8);
                    gb.skip(4);
                }
            }
            if gb.read_bit() != 0 {
                gb.skip(24);
            }
        }
        self.hrd_param_flag = gb.read_bit() != 0;
        if self.hrd_param_flag {
            self.hrd_num_leaky_buckets = gb.read(5) as i32;
            gb.skip(4);
            gb.skip(4);
            for _ in 0..self.hrd_num_leaky_buckets {
                gb.skip(16);
                gb.skip(16);
            }
        }
        Ok(())
    }

    /// `ff_vc1_decode_entry_point`.
    fn decode_entry_point(&mut self, gb: &mut BitReader) -> Result<()> {
        self.broken_link = gb.read_bit() != 0;
        self.closed_entry = gb.read_bit() != 0;
        self.panscanflag = gb.read_bit() != 0;
        self.refdist_flag = gb.read_bit() != 0;
        self.loop_filter = gb.read_bit() != 0;
        self.fastuvmc = gb.read_bit() != 0;
        self.extended_mv = gb.read_bit() != 0;
        self.dquant = gb.read(2) as i32;
        self.vstransform = gb.read_bit() != 0;
        self.overlap = gb.read_bit() != 0;
        self.quantizer_mode = gb.read(2) as i32;
        if self.hrd_param_flag {
            for _ in 0..self.hrd_num_leaky_buckets {
                gb.skip(8);
            }
        }
        let (w, h) = if gb.read_bit() != 0 {
            (((gb.read(12) + 1) << 1) as i32, ((gb.read(12) + 1) << 1) as i32)
        } else {
            (self.max_coded_width, self.max_coded_height)
        };
        if w <= 0 || h <= 0 || w > crate::MAX_DIM as i32 || h > crate::MAX_DIM as i32 {
            return Err(Error::invalid("vc1: invalid entry-point dimensions"));
        }
        self.coded_width = w;
        self.coded_height = h;
        if self.extended_mv {
            self.extended_dmv = gb.read_bit() != 0;
        }
        if gb.read_bit() != 0 {
            gb.skip(3); // range_mapy (not supported by FFmpeg either)
        }
        if gb.read_bit() != 0 {
            gb.skip(3);
        }
        Ok(())
    }

    /// `ff_vc1_decode_extradata`.
    fn decode_extradata(&mut self, ext: &[u8]) -> Result<()> {
        let end = ext.len();
        let mut start = dsp::find_next_marker(ext, 0);
        let mut next = start;
        while next < end {
            next = dsp::find_next_marker(ext, start + 4);
            let size = next as isize - start as isize - 4;
            if size > 0 {
                let buf = dsp::unescape_buffer(&ext[start + 4..next]);
                let mut gb = BitReader::new(&buf);
                let code = u32::from_be_bytes([ext[start], ext[start + 1], ext[start + 2], ext[start + 3]]);
                match code {
                    VC1_CODE_SEQHDR => {
                        self.decode_sequence_header(&mut gb)?;
                        self.seq_initialized = true;
                    }
                    VC1_CODE_ENTRYPOINT => {
                        self.decode_entry_point(&mut gb)?;
                        self.ep_initialized = true;
                    }
                    _ => {}
                }
            }
            start = next;
        }
        Ok(())
    }

    /// `bitplane_decoding`; returns `(imode << 1) + invert` like FFmpeg.
    pub(crate) fn bitplane_decoding(&self, plane: &mut [u8], raw_flag: &mut bool, gb: &mut BitReader) -> Result<i32> {
        let width = self.mb_width;
        let height = self.mb_height >> self.field_mode as usize;
        let stride = self.mb_stride;
        let invert = gb.read_bit() as u8;
        let imode = VLCS.imode.get(gb);
        *raw_flag = false;
        match imode {
            IMODE_RAW => {
                *raw_flag = true;
                return Ok(invert as i32);
            }
            IMODE_DIFF2 | IMODE_NORM2 => {
                let mut p = 0usize;
                let total = height * width;
                let (mut y, mut offset);
                if total & 1 != 0 {
                    plane[p] = gb.read_bit() as u8;
                    p += 1;
                    y = 1;
                    offset = 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                } else {
                    y = 0;
                    offset = 0;
                }
                while y < total {
                    let code = VLCS.norm2.get(gb);
                    plane[p] = (code & 1) as u8;
                    p += 1;
                    offset += 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                    plane[p] = ((code >> 1) & 1) as u8;
                    p += 1;
                    offset += 1;
                    if offset == width {
                        offset = 0;
                        p += stride - width;
                    }
                    y += 2;
                }
            }
            IMODE_DIFF6 | IMODE_NORM6 => {
                if height % 3 == 0 && width % 3 != 0 {
                    // 2x3
                    let mut p = 0usize;
                    let mut y = 0;
                    while y < height {
                        let mut x = width & 1;
                        while x < width {
                            let code = VLCS.norm6.get(gb);
                            if code < 0 {
                                return Err(Error::invalid("vc1: invalid NORM-6 VLC"));
                            }
                            plane[p + x] = (code & 1) as u8;
                            plane[p + x + 1] = ((code >> 1) & 1) as u8;
                            plane[p + x + stride] = ((code >> 2) & 1) as u8;
                            plane[p + x + 1 + stride] = ((code >> 3) & 1) as u8;
                            plane[p + x + stride * 2] = ((code >> 4) & 1) as u8;
                            plane[p + x + 1 + stride * 2] = ((code >> 5) & 1) as u8;
                            x += 2;
                        }
                        p += stride * 3;
                        y += 3;
                    }
                    if width & 1 != 0 {
                        decode_colskip(plane, 0, 1, height, stride, gb);
                    }
                } else {
                    // 3x2
                    let mut p = (height & 1) * stride;
                    let mut y = height & 1;
                    while y < height {
                        let mut x = width % 3;
                        while x < width {
                            let code = VLCS.norm6.get(gb);
                            if code < 0 {
                                return Err(Error::invalid("vc1: invalid NORM-6 VLC"));
                            }
                            plane[p + x] = (code & 1) as u8;
                            plane[p + x + 1] = ((code >> 1) & 1) as u8;
                            plane[p + x + 2] = ((code >> 2) & 1) as u8;
                            plane[p + x + stride] = ((code >> 3) & 1) as u8;
                            plane[p + x + 1 + stride] = ((code >> 4) & 1) as u8;
                            plane[p + x + 2 + stride] = ((code >> 5) & 1) as u8;
                            x += 3;
                        }
                        p += stride * 2;
                        y += 2;
                    }
                    let x = width % 3;
                    if x != 0 {
                        decode_colskip(plane, 0, x, height, stride, gb);
                    }
                    if height & 1 != 0 {
                        decode_rowskip(plane, x, width - x, 1, stride, gb);
                    }
                }
            }
            IMODE_ROWSKIP => decode_rowskip(plane, 0, width, height, stride, gb),
            IMODE_COLSKIP => decode_colskip(plane, 0, width, height, stride, gb),
            _ => {}
        }
        if imode == IMODE_DIFF2 || imode == IMODE_DIFF6 {
            plane[0] ^= invert;
            for x in 1..width {
                plane[x] ^= plane[x - 1];
            }
            let mut p = 0usize;
            for _y in 1..height {
                p += stride;
                plane[p] ^= plane[p - stride];
                for x in 1..width {
                    if plane[p + x - 1] != plane[p + x - stride] {
                        plane[p + x] ^= invert;
                    } else {
                        plane[p + x] ^= plane[p + x - 1];
                    }
                }
            }
        } else if invert != 0 {
            for v in &mut plane[..stride * height] {
                *v = (*v == 0) as u8;
            }
        }
        Ok((imode << 1) + invert as i32)
    }

    /// `vop_dquant_decoding`.
    fn vop_dquant_decoding(&mut self, gb: &mut BitReader) {
        if self.dquant != 2 {
            self.dquantfrm = gb.read_bit() != 0;
            if !self.dquantfrm {
                return;
            }
            self.dqprofile = gb.read(2) as u8;
            match self.dqprofile {
                DQPROFILE_SINGLE_EDGE | DQPROFILE_DOUBLE_EDGES => self.dqsbedge = gb.read(2) as u8,
                DQPROFILE_ALL_MBS => {
                    self.dqbilevel = gb.read_bit() != 0;
                    if !self.dqbilevel {
                        self.halfpq = 0;
                        return;
                    }
                }
                _ => {}
            }
        }
        let pqdiff = gb.read(3) as i32;
        if pqdiff == 7 {
            self.altpq = gb.read(5) as i32;
        } else {
            self.altpq = (self.pq + pqdiff + 1) & 0xFF;
        }
    }

    /// `rotate_luts`.
    fn rotate_luts(&mut self) {
        if self.pict_type == PICT_BI || self.pict_type == PICT_B {
            self.curr_is_aux = true;
        } else {
            std::mem::swap(&mut self.last_use_ic, &mut self.next_use_ic);
            std::mem::swap(&mut self.last_luty, &mut self.next_luty);
            std::mem::swap(&mut self.last_lutuv, &mut self.next_lutuv);
            self.curr_is_aux = false;
        }
        let (ly, luv) = self.curr_luts_mut();
        init_lut(32, 0, &mut ly[0], &mut luv[0], false);
        init_lut(32, 0, &mut ly[1], &mut luv[1], false);
        *self.curr_use_ic_mut() = false;
    }

    pub(crate) fn curr_luts(&self) -> (&[[u8; 256]; 2], &[[u8; 256]; 2]) {
        if self.curr_is_aux {
            (&self.aux_luty, &self.aux_lutuv)
        } else {
            (&self.next_luty, &self.next_lutuv)
        }
    }

    fn curr_luts_mut(&mut self) -> (&mut [[u8; 256]; 2], &mut [[u8; 256]; 2]) {
        if self.curr_is_aux {
            (&mut self.aux_luty, &mut self.aux_lutuv)
        } else {
            (&mut self.next_luty, &mut self.next_lutuv)
        }
    }

    pub(crate) fn curr_use_ic(&self) -> bool {
        if self.curr_is_aux {
            self.aux_use_ic
        } else {
            self.next_use_ic
        }
    }

    fn curr_use_ic_mut(&mut self) -> &mut bool {
        if self.curr_is_aux {
            &mut self.aux_use_ic
        } else {
            &mut self.next_use_ic
        }
    }

    /// `read_bfraction`.
    fn read_bfraction(&mut self, gb: &mut BitReader) -> Result<()> {
        let mut idx = gb.read(3) as usize;
        if idx == 7 {
            idx = 7 + gb.read(4) as usize;
        }
        if idx == 21 {
            return Err(Error::invalid("vc1: invalid bfraction"));
        }
        self.bfraction = VC1_BFRACTION_LUT[idx] as i32;
        Ok(())
    }

    fn set_mv_range(&mut self) {
        self.k_x = self.mvrange + 9 + (self.mvrange >> 1);
        self.k_y = self.mvrange + 8;
        self.range_x = 1 << (self.k_x - 1);
        self.range_y = 1 << (self.k_y - 1);
    }

    fn set_qs_mspel(&mut self, mode: u8) {
        self.quarter_sample = mode != MV_PMODE_1MV_HPEL && mode != MV_PMODE_1MV_HPEL_BILIN;
        self.mspel = mode != MV_PMODE_1MV_HPEL_BILIN;
    }

    fn read_ttfrm(&mut self, gb: &mut BitReader) {
        if self.vstransform {
            self.ttmbf = gb.read_bit() != 0;
            if self.ttmbf {
                self.ttfrm = VC1_TTFRM_TO_TT[gb.read(2) as usize] as i32;
            } else {
                self.ttfrm = 0;
            }
        } else {
            self.ttmbf = true;
            self.ttfrm = TT_8X8;
        }
    }

    fn read_pquant(&mut self, gb: &mut BitReader) -> Result<()> {
        let pqindex = gb.read(5) as i32;
        if pqindex == 0 {
            return Err(Error::invalid("vc1: zero pqindex"));
        }
        self.pq = if self.quantizer_mode == QUANT_FRAME_IMPLICIT {
            VC1_PQUANT_TABLE[0][pqindex as usize] as i32
        } else {
            VC1_PQUANT_TABLE[1][pqindex as usize] as i32
        };
        self.pqindex = pqindex;
        self.halfpq = if pqindex < 9 { gb.read_bit() as i32 } else { 0 };
        self.pquantizer = match self.quantizer_mode {
            QUANT_FRAME_IMPLICIT => pqindex < 9,
            QUANT_NON_UNIFORM => false,
            QUANT_FRAME_EXPLICIT => gb.read_bit() != 0,
            _ => true,
        };
        self.dquantfrm = false;
        Ok(())
    }

    /// `ff_vc1_parse_frame_header` (Simple/Main profiles).
    fn parse_frame_header(&mut self, gb: &mut BitReader) -> Result<()> {
        self.field_mode = false;
        self.fcm = PROGRESSIVE;
        if self.finterpflag {
            self.interpfrm = gb.read_bit() != 0;
        }
        gb.skip(2); // framecnt
        self.rangeredfrm = false;
        if self.rangered {
            self.rangeredfrm = gb.read_bit() != 0;
        }
        if gb.read_bit() != 0 {
            self.pict_type = PICT_P;
        } else if self.max_b_frames != 0 && gb.read_bit() == 0 {
            self.pict_type = PICT_B;
        } else {
            self.pict_type = PICT_I;
        }
        self.bi_type = false;
        if self.pict_type == PICT_B {
            self.read_bfraction(gb)?;
            if self.bfraction == 0 {
                self.pict_type = PICT_BI;
            }
        }
        if self.pict_type == PICT_I || self.pict_type == PICT_BI {
            gb.skip(7);
        }
        if self.pict_type == PICT_I || self.pict_type == PICT_BI {
            self.rnd = 1;
        }
        if self.pict_type == PICT_P {
            self.rnd ^= 1;
        }
        if gb.bits_left() < 5 {
            return Err(Error::invalid("vc1: truncated picture header"));
        }
        self.read_pquant(gb)?;
        if self.extended_mv {
            self.mvrange = gb.get_unary(0, 3) as i32;
        }
        self.set_mv_range();
        if self.multires && self.pict_type != PICT_B {
            self.respic = gb.read(2) as i32;
        }
        self.x8_type = if self.res_x8 && (self.pict_type == PICT_I || self.pict_type == PICT_BI) {
            gb.read_bit() != 0
        } else {
            false
        };
        if self.first_pic_header_flag {
            self.rotate_luts();
        }
        match self.pict_type {
            PICT_P => {
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                let lowquant = if self.pq > 12 { 0 } else { 1 };
                self.mv_mode = VC1_MV_PMODE_TABLE[lowquant][gb.get_unary(1, 4) as usize];
                if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                    self.mv_mode2 = VC1_MV_PMODE_TABLE2[lowquant][gb.get_unary(1, 3) as usize];
                    self.lumscale = gb.read(6) as i32;
                    self.lumshift = gb.read(6) as i32;
                    self.last_use_ic = true;
                    let (s, h) = (self.lumscale, self.lumshift);
                    init_lut(s, h, &mut self.last_luty[0], &mut self.last_lutuv[0], true);
                    init_lut(s, h, &mut self.last_luty[1], &mut self.last_lutuv[1], true);
                }
                if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                    self.set_qs_mspel(self.mv_mode2);
                } else {
                    self.set_qs_mspel(self.mv_mode);
                }
                if (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)
                    || self.mv_mode == MV_PMODE_MIXED_MV
                {
                    let mut plane = std::mem::take(&mut self.mv_type_mb_plane);
                    let mut raw = false;
                    let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                    self.mv_type_mb_plane = plane;
                    self.mv_type_is_raw = raw;
                    r?;
                } else {
                    self.mv_type_is_raw = false;
                    let n = self.mb_stride * self.mb_height;
                    self.mv_type_mb_plane[..n].fill(0);
                }
                self.decode_skip_plane(gb)?;
                if gb.bits_left() < 4 {
                    return Err(Error::invalid("vc1: truncated picture header"));
                }
                self.mv_table_index = gb.read(2) as usize;
                self.cbpcy_vlc = VlcSel::CbpcyP(gb.read(2) as usize);
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.read_ttfrm(gb);
            }
            PICT_B => {
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                self.mv_mode = if gb.read_bit() != 0 { MV_PMODE_1MV } else { MV_PMODE_1MV_HPEL_BILIN };
                self.quarter_sample = self.mv_mode == MV_PMODE_1MV;
                self.mspel = self.quarter_sample;
                self.decode_direct_plane(gb)?;
                self.decode_skip_plane(gb)?;
                self.mv_table_index = gb.read(2) as usize;
                self.cbpcy_vlc = VlcSel::CbpcyP(gb.read(2) as usize);
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.read_ttfrm(gb);
            }
            _ => {}
        }
        if !self.x8_type {
            self.c_ac_table_index = gb.decode012();
            if self.pict_type == PICT_I || self.pict_type == PICT_BI {
                self.y_ac_table_index = gb.decode012();
            }
            self.dc_table_index = gb.read_bit() as usize;
        }
        if self.pict_type == PICT_BI {
            self.pict_type = PICT_B;
            self.bi_type = true;
        }
        Ok(())
    }

    fn decode_skip_plane(&mut self, gb: &mut BitReader) -> Result<()> {
        let mut plane = std::mem::take(&mut self.mbskip_table);
        let mut raw = false;
        let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
        self.mbskip_table = plane;
        self.skip_is_raw = raw;
        r.map(|_| ())
    }

    fn decode_direct_plane(&mut self, gb: &mut BitReader) -> Result<()> {
        let mut plane = std::mem::take(&mut self.direct_mb_plane);
        let mut raw = false;
        let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
        self.direct_mb_plane = plane;
        self.dmb_is_raw = raw;
        r.map(|_| ())
    }

    /// `ff_vc1_parse_frame_header_adv`.
    fn parse_frame_header_adv(&mut self, gb: &mut BitReader) -> Result<()> {
        self.numref = 0;
        self.p_frame_skipped = false;
        let mut common_only = false;
        if self.second_field {
            if self.fcm != ILACE_FIELD || !self.field_mode {
                return Err(Error::invalid("vc1: second field without field mode"));
            }
            self.pict_type = if self.fptype & 4 != 0 {
                if self.fptype & 1 != 0 { PICT_BI } else { PICT_B }
            } else if self.fptype & 1 != 0 {
                PICT_P
            } else {
                PICT_I
            };
            if !self.pic_header_flag {
                common_only = true;
            }
        }
        if !common_only {
            let mut field_mode = false;
            let fcm = if self.interlace {
                let f = gb.decode012() as u8;
                if f == ILACE_FIELD {
                    field_mode = true;
                }
                f
            } else {
                PROGRESSIVE
            };
            if !self.first_pic_header_flag && self.field_mode != field_mode {
                return Err(Error::invalid("vc1: field mode changed within a frame"));
            }
            self.field_mode = field_mode;
            self.fcm = fcm;
            if self.field_mode {
                self.mb_height = ((self.height as usize + 15) >> 4).div_ceil(2) * 2;
                self.fptype = gb.read(3) as i32;
                self.pict_type = if self.fptype & 4 != 0 {
                    if self.fptype & 2 != 0 { PICT_BI } else { PICT_B }
                } else if self.fptype & 2 != 0 {
                    PICT_P
                } else {
                    PICT_I
                };
            } else {
                self.mb_height = (self.height as usize + 15) >> 4;
                match gb.get_unary(0, 4) {
                    0 => self.pict_type = PICT_P,
                    1 => self.pict_type = PICT_B,
                    2 => self.pict_type = PICT_I,
                    3 => self.pict_type = PICT_BI,
                    _ => {
                        self.pict_type = PICT_P;
                        self.p_frame_skipped = true;
                    }
                }
            }
            if self.tfcntrflag {
                gb.skip(8);
            }
            if self.broadcast {
                if !self.interlace || self.psf {
                    self.rptfrm = gb.read(2) as i32;
                } else {
                    self.tff = gb.read_bit() != 0;
                    self.rff = gb.read_bit() != 0;
                }
            } else {
                self.tff = true;
            }
            if self.p_frame_skipped {
                return Ok(());
            }
            self.rnd = gb.read_bit() as i32;
            if self.interlace {
                self.uvsamp = gb.read_bit() != 0;
            }
            if self.field_mode {
                if !self.refdist_flag {
                    self.refdist = 0;
                } else if self.pict_type != PICT_B && self.pict_type != PICT_BI {
                    self.refdist = gb.read(2) as i32;
                    if self.refdist == 3 {
                        self.refdist += gb.get_unary(0, 14) as i32;
                    }
                    if self.refdist > 16 {
                        return Err(Error::invalid("vc1: invalid refdist"));
                    }
                }
                if self.pict_type == PICT_B || self.pict_type == PICT_BI {
                    self.read_bfraction(gb)?;
                    self.frfd = (self.bfraction * self.refdist) >> 8;
                    self.brfd = self.refdist - self.frfd - 1;
                    if self.brfd < 0 {
                        self.brfd = 0;
                    }
                }
            } else if self.fcm == PROGRESSIVE {
                if self.finterpflag {
                    self.interpfrm = gb.read_bit() != 0;
                }
                if self.pict_type == PICT_B {
                    self.read_bfraction(gb)?;
                    if self.bfraction == 0 {
                        self.pict_type = PICT_BI;
                    }
                }
            }
        }

        // parse_common_info:
        if self.field_mode {
            self.cur_field_type = (!(self.tff ^ self.second_field)) as i32;
        }
        self.read_pquant(gb)?;
        if self.postprocflag {
            self.postproc = gb.read(2) as i32;
        }
        if self.first_pic_header_flag {
            self.rotate_luts();
        }

        match self.pict_type {
            PICT_I | PICT_BI => {
                if self.fcm == ILACE_FRAME {
                    let mut plane = std::mem::take(&mut self.fieldtx_plane);
                    let mut raw = false;
                    let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                    self.fieldtx_plane = plane;
                    self.fieldtx_is_raw = raw;
                    r?;
                } else {
                    self.fieldtx_is_raw = false;
                }
                let mut plane = std::mem::take(&mut self.acpred_plane);
                let mut raw = false;
                let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                self.acpred_plane = plane;
                self.acpred_is_raw = raw;
                r?;
                self.condover = CONDOVER_NONE;
                if self.overlap && self.pq <= 8 {
                    self.condover = gb.decode012() as u8;
                    if self.condover == CONDOVER_SELECT {
                        let mut plane = std::mem::take(&mut self.over_flags_plane);
                        let mut raw = false;
                        let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                        self.over_flags_plane = plane;
                        self.overflg_is_raw = raw;
                        r?;
                    }
                }
            }
            PICT_P => {
                if self.field_mode {
                    self.numref = gb.read_bit() as i32;
                    if self.numref == 0 {
                        self.reffield = gb.read_bit() as i32;
                        self.ref_field_type[0] = self.reffield ^ (self.cur_field_type == 0) as i32;
                    }
                }
                self.mvrange = if self.extended_mv { gb.get_unary(0, 3) as i32 } else { 0 };
                if self.interlace {
                    self.dmvrange = if self.extended_dmv { gb.get_unary(0, 3) as i32 } else { 0 };
                    if self.fcm == ILACE_FRAME {
                        self.fourmvswitch = gb.read_bit() != 0;
                        self.intcomp = gb.read_bit() != 0;
                        if self.intcomp {
                            self.lumscale = gb.read(6) as i32;
                            self.lumshift = gb.read(6) as i32;
                            let (s, h) = (self.lumscale, self.lumshift);
                            init_lut(s, h, &mut self.last_luty[0], &mut self.last_lutuv[0], true);
                            init_lut(s, h, &mut self.last_luty[1], &mut self.last_lutuv[1], true);
                            self.last_use_ic = true;
                        }
                        self.decode_skip_plane(gb)?;
                        let mbmodetab = gb.read(2) as usize;
                        self.mbmode_vlc = if self.fourmvswitch {
                            VlcSel::Intfr4mv(mbmodetab)
                        } else {
                            VlcSel::IntfrNon4mv(mbmodetab)
                        };
                        self.imv_vlc = VlcSel::OneRef(gb.read(2) as usize);
                        self.cbpcy_vlc = VlcSel::Icbpcy(gb.read(3) as usize);
                        self.twomvbp_vlc = VlcSel::TwoMvBp(gb.read(2) as usize);
                        if self.fourmvswitch {
                            self.fourmvbp_vlc = VlcSel::FourMvBp(gb.read(2) as usize);
                        }
                    }
                }
                self.set_mv_range();
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                if self.fcm != ILACE_FRAME {
                    let mvmode = gb.get_unary(1, 4) as usize;
                    let lowquant = if self.pq > 12 { 0 } else { 1 };
                    self.mv_mode = VC1_MV_PMODE_TABLE[lowquant][mvmode];
                    if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                        let mvmode2 = gb.get_unary(1, 3) as usize;
                        self.mv_mode2 = VC1_MV_PMODE_TABLE2[lowquant][mvmode2];
                        self.intcompfield = if self.field_mode { (gb.decode210() ^ 3) as i32 } else { 3 };
                        self.lumscale2 = 32;
                        self.lumscale = 32;
                        self.lumshift2 = 0;
                        self.lumshift = 0;
                        if self.intcompfield & 1 != 0 {
                            self.lumscale = gb.read(6) as i32;
                            self.lumshift = gb.read(6) as i32;
                        }
                        if (self.intcompfield & 2) != 0 && self.field_mode {
                            self.lumscale2 = gb.read(6) as i32;
                            self.lumshift2 = gb.read(6) as i32;
                        } else if !self.field_mode {
                            self.lumscale2 = self.lumscale;
                            self.lumshift2 = self.lumshift;
                        }
                        if self.field_mode && self.second_field {
                            let cft = self.cur_field_type as usize;
                            let (s1, h1, s2, h2) = if self.cur_field_type != 0 {
                                (self.lumscale, self.lumshift, self.lumscale2, self.lumshift2)
                            } else {
                                (self.lumscale2, self.lumshift2, self.lumscale, self.lumshift)
                            };
                            {
                                let (ly, luv) = self.curr_luts_mut();
                                let (a, b) = (&mut ly[cft ^ 1], &mut luv[cft ^ 1]);
                                init_lut(s1, h1, a, b, false);
                            }
                            init_lut(s2, h2, &mut self.last_luty[cft], &mut self.last_lutuv[cft], true);
                            self.next_use_ic = true;
                            *self.curr_use_ic_mut() = true;
                        } else {
                            let (s1, h1, s2, h2) = (self.lumscale, self.lumshift, self.lumscale2, self.lumshift2);
                            init_lut(s1, h1, &mut self.last_luty[0], &mut self.last_lutuv[0], true);
                            init_lut(s2, h2, &mut self.last_luty[1], &mut self.last_lutuv[1], true);
                        }
                        self.last_use_ic = true;
                    }
                    if self.mv_mode == MV_PMODE_INTENSITY_COMP {
                        self.set_qs_mspel(self.mv_mode2);
                    } else {
                        self.set_qs_mspel(self.mv_mode);
                    }
                }
                if self.fcm == PROGRESSIVE {
                    if (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)
                        || self.mv_mode == MV_PMODE_MIXED_MV
                    {
                        let mut plane = std::mem::take(&mut self.mv_type_mb_plane);
                        let mut raw = false;
                        let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                        self.mv_type_mb_plane = plane;
                        self.mv_type_is_raw = raw;
                        r?;
                    } else {
                        self.mv_type_is_raw = false;
                        let n = self.mb_stride * self.mb_height;
                        self.mv_type_mb_plane[..n].fill(0);
                    }
                    self.decode_skip_plane(gb)?;
                    self.mv_table_index = gb.read(2) as usize;
                    self.cbpcy_vlc = VlcSel::CbpcyP(gb.read(2) as usize);
                } else if self.fcm == ILACE_FRAME {
                    self.quarter_sample = true;
                    self.mspel = true;
                } else {
                    let mbmodetab = gb.read(3) as usize;
                    let imvtab = gb.read(2 + self.numref as u32) as usize;
                    self.imv_vlc = if self.numref == 0 { VlcSel::OneRef(imvtab) } else { VlcSel::TwoRef(imvtab) };
                    self.cbpcy_vlc = VlcSel::Icbpcy(gb.read(3) as usize);
                    if (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV)
                        || self.mv_mode == MV_PMODE_MIXED_MV
                    {
                        self.fourmvbp_vlc = VlcSel::FourMvBp(gb.read(2) as usize);
                        self.mbmode_vlc = VlcSel::IfMmv(mbmodetab);
                    } else {
                        self.mbmode_vlc = VlcSel::If1mv(mbmodetab);
                    }
                }
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.read_ttfrm(gb);
            }
            PICT_B => {
                if self.fcm == ILACE_FRAME {
                    self.read_bfraction(gb)?;
                    if self.bfraction == 0 {
                        return Err(Error::invalid("vc1: zero bfraction in interlaced frame B"));
                    }
                }
                self.mvrange = if self.extended_mv { gb.get_unary(0, 3) as i32 } else { 0 };
                self.set_mv_range();
                self.tt_index = (self.pq > 4) as usize + (self.pq > 12) as usize;
                if self.field_mode {
                    if self.extended_dmv {
                        self.dmvrange = gb.get_unary(0, 3) as i32;
                    }
                    let mvmode = gb.get_unary(1, 3) as usize;
                    let lowquant = if self.pq > 12 { 0 } else { 1 };
                    self.mv_mode = VC1_MV_PMODE_TABLE2[lowquant][mvmode];
                    self.quarter_sample = self.mv_mode == MV_PMODE_1MV || self.mv_mode == MV_PMODE_MIXED_MV;
                    self.mspel = self.mv_mode != MV_PMODE_1MV_HPEL_BILIN;
                    let mut plane = std::mem::take(&mut self.forward_mb_plane);
                    let mut raw = false;
                    let r = self.bitplane_decoding(&mut plane, &mut raw, gb);
                    self.forward_mb_plane = plane;
                    self.fmb_is_raw = raw;
                    r?;
                    let mbmodetab = gb.read(3) as usize;
                    self.mbmode_vlc =
                        if self.mv_mode == MV_PMODE_MIXED_MV { VlcSel::IfMmv(mbmodetab) } else { VlcSel::If1mv(mbmodetab) };
                    self.imv_vlc = VlcSel::TwoRef(gb.read(3) as usize);
                    self.cbpcy_vlc = VlcSel::Icbpcy(gb.read(3) as usize);
                    if self.mv_mode == MV_PMODE_MIXED_MV {
                        self.fourmvbp_vlc = VlcSel::FourMvBp(gb.read(2) as usize);
                    }
                    self.numref = 1;
                } else if self.fcm == ILACE_FRAME {
                    if self.extended_dmv {
                        self.dmvrange = gb.get_unary(0, 3) as i32;
                    }
                    let _intcomp = gb.read_bit();
                    self.intcomp = false;
                    self.mv_mode = MV_PMODE_1MV;
                    self.fourmvswitch = false;
                    self.quarter_sample = true;
                    self.mspel = true;
                    self.decode_direct_plane(gb)?;
                    self.decode_skip_plane(gb)?;
                    self.mbmode_vlc = VlcSel::IntfrNon4mv(gb.read(2) as usize);
                    self.imv_vlc = VlcSel::OneRef(gb.read(2) as usize);
                    self.cbpcy_vlc = VlcSel::Icbpcy(gb.read(3) as usize);
                    self.twomvbp_vlc = VlcSel::TwoMvBp(gb.read(2) as usize);
                    self.fourmvbp_vlc = VlcSel::FourMvBp(gb.read(2) as usize);
                } else {
                    self.mv_mode = if gb.read_bit() != 0 { MV_PMODE_1MV } else { MV_PMODE_1MV_HPEL_BILIN };
                    self.quarter_sample = self.mv_mode == MV_PMODE_1MV;
                    self.mspel = self.quarter_sample;
                    self.decode_direct_plane(gb)?;
                    self.decode_skip_plane(gb)?;
                    self.mv_table_index = gb.read(2) as usize;
                    self.cbpcy_vlc = VlcSel::CbpcyP(gb.read(2) as usize);
                }
                if self.dquant != 0 {
                    self.vop_dquant_decoding(gb);
                }
                self.read_ttfrm(gb);
            }
            _ => {}
        }

        self.c_ac_table_index = gb.decode012();
        if self.pict_type == PICT_I || self.pict_type == PICT_BI {
            self.y_ac_table_index = gb.decode012();
        } else if self.fcm != PROGRESSIVE && !self.quarter_sample {
            self.range_x <<= 1;
            self.range_y <<= 1;
        }
        self.dc_table_index = gb.read_bit() as usize;
        if (self.pict_type == PICT_I || self.pict_type == PICT_BI) && self.dquant != 0 {
            self.vop_dquant_decoding(gb);
        }
        self.bi_type = self.pict_type == PICT_BI;
        if self.bi_type {
            self.pict_type = PICT_B;
        }
        Ok(())
    }

    // ───────────────────────── frame decoding (vc1dec.c) ─────────────────────────

    /// `ff_mpv_frame_start` for VC-1: new current picture, reference
    /// rotation and dummy references.
    fn frame_start(&mut self) {
        let mut cur = self.alloc_vpic();
        // The motion_val pool is AV_REFSTRUCT_POOL_FLAG_ZERO_EVERY_TIME.
        for mv in cur.motion_val.iter_mut() {
            mv.fill([0, 0]);
        }
        cur.field_picture = self.field_mode;
        cur.interlaced = self.fcm != PROGRESSIVE;
        if self.pict_type != PICT_B {
            if let Some(old) = self.last.take() {
                self.spare.push(old);
            }
            self.last = self.next.take();
        }
        // The current picture becomes `next` at the end of the frame (it is
        // decoded in place while `next` still refers to the previous one).
        if self.last.is_none() && self.pict_type != PICT_I {
            let mut dummy = self.alloc_vpic();
            dummy.pic.fill(self.width as usize, self.height as usize, 0x80, 0x80);
            dummy.field_picture = false;
            dummy.interlaced = false;
            for mv in dummy.motion_val.iter_mut() {
                mv.fill([0, 0]);
            }
            dummy.mb_type.fill(0);
            self.last = Some(dummy);
        }
        self.cur = Some(cur);
    }

    fn decode_frame(&mut self, data: &[u8], pts: Option<i64>) -> Result<()> {
        self.second_field = false;
        let mut buf_size = data.len();
        if buf_size >= 4 && u32::from_be_bytes([data[buf_size - 4], data[buf_size - 3], data[buf_size - 2], data[buf_size - 1]]) == VC1_CODE_ENDOFSEQ {
            buf_size -= 4;
        }
        let buf = &data[..buf_size];
        if buf.is_empty() {
            return Ok(());
        }

        struct Slice {
            buf: Vec<u8>,
            mby_start: usize,
            /// Bits already consumed (the 9-bit SLICE_ADDR of slice units).
            skip: u32,
        }
        let mut slices: Vec<Slice> = Vec::new();
        let mut n_slices1: isize = -1;
        let main_buf: Vec<u8>;
        if self.is_vc1 {
            let byte = |i: usize| buf.get(i).copied().unwrap_or(0);
            let first = u32::from_be_bytes([byte(0), byte(1), byte(2), byte(3)]);
            if buf.len() >= 4 && (first & !0xFF) == 0x100 {
                let mut frame_buf = Vec::new();
                let end = buf.len();
                let mut start = 0usize;
                let mut next = 0usize;
                while next < end {
                    next = dsp::find_next_marker(buf, start + 4);
                    let size = next as isize - start as isize - 4;
                    if size > 0 {
                        let code = u32::from_be_bytes([buf[start], buf[start + 1], buf[start + 2], buf[start + 3]]);
                        let payload = &buf[start + 4..next];
                        match code {
                            VC1_CODE_FRAME => frame_buf = dsp::unescape_buffer(payload),
                            VC1_CODE_FIELD => {
                                let ub = dsp::unescape_buffer(payload);
                                let mby_start = ((self.coded_height + 31) >> 5) as usize;
                                n_slices1 = slices.len() as isize - 1;
                                slices.push(Slice { buf: ub, mby_start, skip: 0 });
                            }
                            VC1_CODE_ENTRYPOINT => {
                                let ub = dsp::unescape_buffer(payload);
                                let mut gb = BitReader::new(&ub);
                                if self.seq_initialized {
                                    let (ow, oh) = (self.coded_width, self.coded_height);
                                    self.decode_entry_point(&mut gb)?;
                                    self.ep_initialized = true;
                                    if self.width != 0 && (ow != self.coded_width || oh != self.coded_height) {
                                        self.width = 0; // force re-init
                                    }
                                }
                                frame_buf = ub;
                            }
                            VC1_CODE_SEQHDR => {
                                // FFmpeg reads the sequence header from
                                // extradata; raw streams carry it in-band.
                                if !self.seq_initialized {
                                    let ub = dsp::unescape_buffer(payload);
                                    let mut gb = BitReader::new(&ub);
                                    self.decode_sequence_header(&mut gb)?;
                                    self.seq_initialized = true;
                                }
                            }
                            VC1_CODE_SLICE => {
                                let ub = dsp::unescape_buffer(payload);
                                let mut gb = BitReader::new(&ub);
                                let mby_start = gb.read(9) as usize;
                                slices.push(Slice { buf: ub, mby_start, skip: 9 });
                            }
                            _ => {}
                        }
                    }
                    start = next;
                }
                main_buf = frame_buf;
            } else if self.interlace && (buf[0] & 0xC0) == 0xC0 {
                let divider = dsp::find_next_marker(buf, 0);
                if divider == buf.len()
                    || u32::from_be_bytes([buf[divider], buf[divider + 1], buf[divider + 2], buf[divider + 3]]) != VC1_CODE_FIELD
                {
                    return Err(Error::invalid("vc1: error in WVC1 interlaced frame"));
                }
                let ub = dsp::unescape_buffer(&buf[divider + 4..]);
                n_slices1 = slices.len() as isize - 1;
                slices.push(Slice { buf: ub, mby_start: (self.mb_height + 1) >> 1, skip: 0 });
                main_buf = dsp::unescape_buffer(&buf[..divider]);
            } else {
                main_buf = dsp::unescape_buffer(buf);
            }
        } else {
            main_buf = buf.to_vec();
        }

        if !self.seq_initialized || !self.ep_initialized {
            return Err(Error::invalid("vc1: missing sequence header or entry point"));
        }
        if self.width != self.coded_width || self.height != self.coded_height || self.dc_val.is_empty() {
            self.finish_init()?;
        }

        // The slice bit readers: slice i reads from its own buffer, after
        // the 9-bit address for SLICE units.
        let mut gb = BitReader::new(&main_buf);

        if self.res_sprite {
            // new_sprite / two_sprites (raw sprite decoding, as FFmpeg's
            // wmv3/vc1 decoders do without the image compositor).
            let _new_sprite = gb.read_bit();
            let _two = gb.read_bit();
        }

        self.pic_header_flag = false;
        self.first_pic_header_flag = true;
        if self.profile < PROFILE_ADVANCED {
            self.parse_frame_header(&mut gb)?;
        } else {
            self.parse_frame_header_adv(&mut gb)?;
        }
        self.first_pic_header_flag = false;

        if (self.mb_height >> self.field_mode as usize) == 0 {
            return Err(Error::invalid("vc1: image too short"));
        }
        // Skip B-frames without reference frames.
        if self.last.is_none() && self.pict_type == PICT_B {
            return Ok(());
        }

        self.frame_start();
        self.last_interlaced = self.last.as_ref().is_some_and(|p| p.interlaced);
        self.next_interlaced = self.next.as_ref().is_some_and(|p| p.interlaced);

        let frame_linesize = self.mb_width * 16;
        let frame_uvlinesize = self.mb_width * 8;
        self.end_mb_x = self.mb_width;
        self.linesize = frame_linesize << self.field_mode as usize;
        self.uvlinesize = frame_uvlinesize << self.field_mode as usize;
        let mb_height = self.mb_height >> self.field_mode as usize;
        let n_slices = slices.len();
        let slice_gbs: Vec<BitReader> = slices
            .iter()
            .map(|sl| {
                let mut g = BitReader::new(&sl.buf);
                g.skip(sl.skip);
                g
            })
            .collect();
        // `v->gb` only moves on to the next slice after a slice was decoded,
        // exactly like vc1_decode_frame.
        let mut cur_gb = gb;
        let mut header_ok = true;
        for i in 0..=n_slices {
            if i > 0 && slices[i - 1].mby_start >= mb_height {
                if !self.field_mode {
                    continue;
                }
                self.second_field = true;
                self.blocks_off = (self.b8_stride * (self.mb_height & !1)) as isize;
                self.mb_off = ((self.mb_stride * self.mb_height) >> 1) as isize;
            } else {
                self.second_field = false;
                self.blocks_off = 0;
                self.mb_off = 0;
            }
            if i > 0 {
                self.pic_header_flag = false;
                if self.field_mode && i as isize == n_slices1 + 2 {
                    if self.parse_frame_header_adv(&mut cur_gb).is_err() {
                        header_ok = false;
                        continue;
                    }
                    header_ok = true;
                } else if cur_gb.read_bit() != 0 {
                    self.pic_header_flag = true;
                    if self.parse_frame_header_adv(&mut cur_gb).is_err() {
                        header_ok = false;
                        continue;
                    }
                    header_ok = true;
                }
            }
            if !header_ok {
                continue;
            }
            self.start_mb_y = if i == 0 { 0 } else { slices[i - 1].mby_start % mb_height };
            if !self.field_mode || self.second_field {
                self.end_mb_y =
                    if i == n_slices { mb_height } else { mb_height.min(slices[i].mby_start % mb_height) };
            } else {
                if i >= n_slices {
                    continue;
                }
                self.end_mb_y = if i as isize == n_slices1 + 1 {
                    mb_height
                } else {
                    mb_height.min(slices[i].mby_start % mb_height)
                };
            }
            if self.end_mb_y <= self.start_mb_y {
                continue;
            }
            if ((self.pict_type == PICT_P && !self.p_frame_skipped) || (self.pict_type == PICT_B && !self.bi_type))
                && self.cbpcy_vlc == VlcSel::None
            {
                continue;
            }
            self.decode_blocks(&mut cur_gb);
            if i != n_slices {
                cur_gb = slice_gbs[i].clone();
            }
        }
        if self.field_mode {
            self.second_field = false;
            if self.pict_type != PICT_B {
                std::mem::swap(&mut self.mv_f, &mut self.mv_f_next);
            }
        }
        self.linesize = frame_linesize;
        self.uvlinesize = frame_uvlinesize;

        // Output and reference update.
        let cur = self.cur.take().expect("current picture");
        let size = (self.width as usize, self.height as usize);
        let reported = (self.width as u32, self.height as u32);
        if self.pict_type == PICT_B {
            self.pending.push_back((cur.pic.to_frame(size.0, size.1, pts), reported));
            self.spare.push(cur);
        } else {
            if self.low_delay {
                self.pending.push_back((cur.pic.to_frame(size.0, size.1, pts), reported));
            } else if let Some(last) = self.last.as_ref() {
                self.pending.push_back((last.pic.to_frame(size.0, size.1, pts), reported));
            }
            self.next = Some(cur);
        }
        Ok(())
    }
}

fn pad64(t: &[u8]) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..t.len()].copy_from_slice(t);
    out
}

/// `decode_rowskip`.
fn decode_rowskip(plane: &mut [u8], off: usize, width: usize, height: usize, stride: usize, gb: &mut BitReader) {
    let mut p = off;
    for _ in 0..height {
        if gb.read_bit() == 0 {
            plane[p..p + width].fill(0);
        } else {
            for x in 0..width {
                plane[p + x] = gb.read_bit() as u8;
            }
        }
        p += stride;
    }
}

/// `decode_colskip`.
fn decode_colskip(plane: &mut [u8], off: usize, width: usize, height: usize, stride: usize, gb: &mut BitReader) {
    for x in 0..width {
        let p = off + x;
        if gb.read_bit() == 0 {
            for y in 0..height {
                plane[p + y * stride] = 0;
            }
        } else {
            for y in 0..height {
                plane[p + y * stride] = gb.read_bit() as u8;
            }
        }
    }
}

/// `INIT_LUT`.
pub(crate) fn init_lut(lumscale: i32, lumshift: i32, luty: &mut [u8; 256], lutuv: &mut [u8; 256], chain: bool) {
    let (scale, shift);
    if lumscale == 0 {
        scale = -64;
        let mut s = (255 - lumshift * 2) * 64;
        if lumshift > 31 {
            s += 128 << 6;
        }
        shift = s;
    } else {
        scale = lumscale + 32;
        shift = if lumshift > 31 { (lumshift - 64) * 64 } else { lumshift << 6 };
    }
    for i in 0..256 {
        let iy = if chain { luty[i] as i32 } else { i as i32 };
        let iu = if chain { lutuv[i] as i32 } else { i as i32 };
        luty[i] = ((scale * iy + shift + 32) >> 6).clamp(0, 255) as u8;
        lutuv[i] = ((scale * (iu - 128) + 128 * 64 + 32) >> 6).clamp(0, 255) as u8;
    }
}

impl Decoder for Vc1Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.decode_frame(&packet.data, packet.pts)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let (frame, size) = self.pending.pop_front().ok_or(Error::NeedMore)?;
        self.last_output = Some(size);
        Ok(frame)
    }

    /// The frame last returned; before the first, the next queued one.
    /// The coded size (`init_context`); display size only sets the aspect.
    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        self.last_output
            .or_else(|| self.pending.front().map(|(_, size)| *size))
            .filter(|&(w, h)| w > 0 && h > 0)
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        self.output_video_dimensions().map(|_| PixelFormat::Yuv420P)
    }

    fn flush(&mut self) -> Result<()> {
        // End of stream: the last reference is still waiting (B-frame delay).
        if !self.low_delay {
            if let Some(next) = self.next.take() {
                let size = (self.width as u32, self.height as u32);
                self.pending
                    .push_back((next.pic.to_frame(self.width as usize, self.height as usize, None), size));
                self.spare.push(next);
            }
        }
        if let Some(l) = self.last.take() {
            self.spare.push(l);
        }
        Ok(())
    }
}

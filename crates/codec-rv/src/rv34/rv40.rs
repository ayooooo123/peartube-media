//! RV40-specific parsing and the RV40 deblocking filter.
//!
//! Ported from FFmpeg libavcodec/rv40.c (commit 2da55bf);
//! LGPL-2.1-or-later.

use std::sync::LazyLock;

use super::data::{
    MODE2_PATTERNS_NUM, RV40_AIC_TABLE_INDEX, RV40_ALPHA_TAB, RV40_BETA_TAB, RV40_FILTER_CLIP_TBL, RV40_STANDARD_HEIGHTS,
    RV40_STANDARD_WIDTHS,
};
use super::dsp::{rv40_loop_filter_strength, rv40_strong_loop_filter, rv40_weak_loop_filter};
use super::rv40vlc2::*;
use super::{get_start_offset, is_intra, Geometry, Picture, Rv34Decoder, SliceInfo, MB_TYPE_SEPARATE_DC, PICT_P, RV34_MB_SKIP, RV34_MB_TYPES};
use crate::bits::BitReader;
use crate::picture::check_dimensions;
use crate::vlc::Vlc;

pub(super) struct Rv40Tables {
    aic_top: Vlc,
    aic_mode1: Vec<Option<Vlc>>,
    aic_mode2: Vec<Vlc>,
    ptype: Vec<Vlc>,
    btype: Vec<Vlc>,
}

/// `rv40_init_table`: `{symbol, length}` pairs.
fn init_table(nb_bits: usize, tab: &[[u8; 2]]) -> Vlc {
    let lens: Vec<i8> = tab.iter().map(|p| p[1] as i8).collect();
    let syms: Vec<i16> = tab.iter().map(|p| p[0] as i16).collect();
    Vlc::init_from_lengths(nb_bits as u32, &lens, &syms, 0).expect("RV40 VLC tables are valid")
}

static TABLES: LazyLock<Rv40Tables> = LazyLock::new(|| {
    let aic_top = init_table(AIC_TOP_BITS, &RV40_AIC_TOP_VLC_TAB);
    // Every tenth mode-1 table is empty.
    let aic_mode1 = (0..AIC_MODE1_NUM).map(|i| (i % 10 != 9).then(|| init_table(AIC_MODE1_BITS, &AIC_MODE1_VLC_TABS[i]))).collect();
    let aic_mode2 = (0..AIC_MODE2_NUM)
        .map(|i| {
            // Two 4-bit types per symbol, stored low byte first.
            let syms: Vec<i16> = AIC_MODE2_VLC_SYMS[i].iter().map(|&s| ((s >> 4) as i16) | (((s & 0xF) as i16) << 8)).collect();
            let lens: Vec<i8> = AIC_MODE2_VLC_BITS[i].iter().map(|&l| l as i8).collect();
            Vlc::init_from_lengths(AIC_MODE2_BITS as u32, &lens, &syms, 0).expect("RV40 VLC tables are valid")
        })
        .collect();
    let ptype = (0..NUM_PTYPE_VLCS).map(|i| init_table(PTYPE_VLC_BITS, &PTYPE_VLC_TABS[i])).collect();
    let btype = (0..NUM_BTYPE_VLCS).map(|i| init_table(BTYPE_VLC_BITS, &BTYPE_VLC_TABS[i])).collect();
    Rv40Tables { aic_top, aic_mode1, aic_mode2, ptype, btype }
});

pub(super) fn rv40_tables() -> &'static Rv40Tables {
    &TABLES
}

/// `get_dimension`.
fn get_dimension(gb: &mut BitReader, dim: &[i32]) -> i32 {
    let t = gb.get_bits(3) as usize;
    let mut val = dim[t];
    if val < 0 {
        val = dim[(gb.get_bits1() as i32 - val) as usize];
    }
    if val == 0 {
        loop {
            if gb.bits_left() < 8 {
                return -1;
            }
            let t = gb.get_bits(8) as i32;
            val += t << 2;
            if t != 0xFF {
                break;
            }
        }
    }
    val
}

/// `rv40_parse_slice_header`.
pub(super) fn parse_slice_header(r: &Rv34Decoder, gb: &mut BitReader) -> Option<SliceInfo> {
    let mut si = SliceInfo::default();
    let (mut w, mut h) = (r.g.width as i32, r.g.height as i32);
    if gb.get_bits1() != 0 {
        return None;
    }
    si.ty = gb.get_bits(2) as i32;
    if si.ty == 1 {
        si.ty = 0;
    }
    si.quant = gb.get_bits(5) as i32;
    if gb.get_bits(2) != 0 {
        return None;
    }
    si.vlc_set = gb.get_bits(2) as i32;
    gb.skip_bits(1);
    si.pts = gb.get_bits(13) as i32;
    if si.ty == 0 || gb.get_bits1() == 0 {
        w = get_dimension(gb, &RV40_STANDARD_WIDTHS);
        h = get_dimension(gb, &RV40_STANDARD_HEIGHTS);
    }
    // av_image_check_size, tightened to the decoder's limits.
    if w <= 0 || h <= 0 || check_dimensions(w as usize, h as usize).is_err() {
        return None;
    }
    si.width = w;
    si.height = h;
    let mb_size = ((w + 15) >> 4) * ((h + 15) >> 4);
    si.start = get_start_offset(gb, mb_size);
    Some(si)
}

/// `rv40_decode_intra_types`: `it` indexes the macroblock's first 4x4
/// type in `intra_types_hist`.
pub(super) fn decode_intra_types(r: &mut Rv34Decoder, gb: &mut BitReader, it: usize) -> Result<(), ()> {
    let t = rv40_tables();
    let stride = r.intra_types_stride;
    let types = &mut r.intra_types_hist;
    for i in 0..4 {
        let dst = it + i * stride;
        if i == 0 && r.first_slice_line {
            let pattern = gb.get_vlc2(&t.aic_top.table, AIC_TOP_BITS as u32, 1);
            types[dst] = ((pattern >> 2) & 2) as i8;
            types[dst + 1] = ((pattern >> 1) & 2) as i8;
            types[dst + 2] = (pattern & 2) as i8;
            types[dst + 3] = ((pattern << 1) & 2) as i8;
            continue;
        }
        let mut ptr = dst;
        let mut j = 0;
        while j < 4 {
            // The first VLC (a pair of types) is chosen by the top-right,
            // top and left types; the single-type VLC by top + 10 * left.
            let a = types[ptr - stride + 1] as i32;
            let b = types[ptr - stride] as i32;
            let c = types[ptr - 1] as i32;
            let pattern = a + b * (1 << 4) + c * (1 << 8);
            let k = RV40_AIC_TABLE_INDEX.iter().position(|&p| p as i32 == pattern).unwrap_or(MODE2_PATTERNS_NUM);
            if j < 3 && k < MODE2_PATTERNS_NUM {
                let v = gb.get_vlc2(&t.aic_mode2[k].table, AIC_MODE2_BITS as u32, 2) as u16;
                types[ptr] = (v & 0xFF) as u8 as i8;
                types[ptr + 1] = (v >> 8) as u8 as i8;
                ptr += 2;
                j += 1;
            } else {
                let v = if b != -1 && c != -1 {
                    let idx = (b + c * 10) as usize;
                    match t.aic_mode1.get(idx).and_then(Option::as_ref) {
                        Some(vlc) => gb.get_vlc2(&vlc.table, AIC_MODE1_BITS as u32, 1),
                        None => return Err(()),
                    }
                } else {
                    // "Tricky decoding".
                    match c {
                        -1 => {
                            if b < 2 {
                                (gb.get_bits1() ^ 1) as i32
                            } else {
                                0
                            }
                        }
                        0 | 2 => ((gb.get_bits1() ^ 1) << 1) as i32,
                        _ => 0,
                    }
                };
                types[ptr] = v as i8;
                ptr += 1;
            }
            j += 1;
        }
    }
    // Reject types FFmpeg would index its tables out of bounds with.
    for i in 0..4 {
        for k in 0..4 {
            let v = types[it + i * stride + k];
            if !(0..=8).contains(&v) {
                return Err(());
            }
        }
    }
    Ok(())
}

/// `rv40_decode_mb_info`: the macroblock type, or -1.
pub(super) fn decode_mb_info(r: &mut Rv34Decoder, gb: &mut BitReader) -> i32 {
    let t = rv40_tables();
    let g = r.g;
    let mb_pos = r.mb_x + r.mb_y * g.mb_stride;
    if r.mb_skip_run == 0 {
        r.mb_skip_run = gb.get_interleaved_ue_golomb().wrapping_add(1) as i32;
        if r.mb_skip_run as u32 > (g.mb_width * g.mb_height) as u32 {
            return -1;
        }
    }
    r.mb_skip_run = r.mb_skip_run.wrapping_sub(1);
    if r.mb_skip_run != 0 {
        return RV34_MB_SKIP as i32;
    }

    let mut prev_type = 0usize;
    if r.avail_cache[2] != 0 {
        let mut blocks = [0i32; RV34_MB_TYPES];
        let mut count = 0;
        if r.avail_cache[5] != 0 {
            blocks[r.mb_type[mb_pos - 1]] += 1;
        }
        blocks[r.mb_type[mb_pos - g.mb_stride]] += 1;
        if r.avail_cache[4] != 0 {
            blocks[r.mb_type[mb_pos - g.mb_stride + 1]] += 1;
        }
        if r.avail_cache[1] != 0 {
            blocks[r.mb_type[mb_pos - g.mb_stride - 1]] += 1;
        }
        for (i, &n) in blocks.iter().enumerate() {
            if n > count {
                count = n;
                prev_type = i;
                if count > 1 {
                    break;
                }
            }
        }
    } else if r.avail_cache[5] != 0 {
        prev_type = r.mb_type[mb_pos - 1];
    }

    if r.pict_type == PICT_P {
        let vlc = &t.ptype[BLOCK_NUM_TO_PTYPE_VLC_NUM[prev_type] as usize];
        let q = gb.get_vlc2(&vlc.table, PTYPE_VLC_BITS as u32, 1);
        if q < PBTYPE_ESCAPE as i32 {
            return q;
        }
        // "Dquant for P-frame": FFmpeg reads the next type and drops it.
        let _ = gb.get_vlc2(&vlc.table, PTYPE_VLC_BITS as u32, 1);
    } else {
        let vlc = &t.btype[BLOCK_NUM_TO_BTYPE_VLC_NUM[prev_type] as usize];
        let q = gb.get_vlc2(&vlc.table, BTYPE_VLC_BITS as u32, 1);
        if q < PBTYPE_ESCAPE as i32 {
            return q;
        }
        let _ = gb.get_vlc2(&vlc.table, BTYPE_VLC_BITS as u32, 1);
    }
    0
}

const POS_CUR: usize = 0;
const POS_TOP: usize = 1;
const POS_LEFT: usize = 2;
const POS_BOTTOM: usize = 3;

const MASK_CUR: u32 = 0x0001;
const MASK_RIGHT: u32 = 0x0008;
const MASK_BOTTOM: u32 = 0x0010;
const MASK_TOP: u32 = 0x1000;
const MASK_Y_TOP_ROW: u32 = 0x000F;
const MASK_Y_LAST_ROW: u32 = 0xF000;
const MASK_Y_LEFT_COL: u32 = 0x1111;
const MASK_Y_RIGHT_COL: u32 = 0x8888;
const MASK_C_TOP_ROW: u32 = 0x0003;
const MASK_C_LAST_ROW: u32 = 0x000C;
const MASK_C_LEFT_COL: u32 = 0x0005;
const MASK_C_RIGHT_COL: u32 = 0x000A;

const NEIGHBOUR_OFFS_X: [isize; 4] = [0, 0, -1, 0];
const NEIGHBOUR_OFFS_Y: [isize; 4] = [0, -1, 0, 1];

/// `rv40_adaptive_loop_filter`; `dir` 0 filters a horizontal edge.
#[allow(clippy::too_many_arguments)]
fn adaptive_loop_filter(
    buf: &mut [u8],
    src: usize,
    stride: usize,
    dmode: usize,
    lim_q1: i32,
    lim_p1: i32,
    alpha: i32,
    beta: i32,
    beta2: i32,
    chroma: bool,
    edge: bool,
    dir: usize,
) {
    let (step, along) = if dir == 0 { (stride, 1) } else { (1, stride) };
    let (strong, filter_p1, filter_q1) = rv40_loop_filter_strength(buf, src, step, along, beta, beta2, edge);
    let lims = filter_p1 as i32 + filter_q1 as i32 + ((lim_q1 + lim_p1) >> 1) + 1;
    if strong {
        rv40_strong_loop_filter(buf, src, step, along, alpha, lims, dmode, chroma);
    } else if filter_p1 && filter_q1 {
        rv40_weak_loop_filter(buf, src, step, along, true, true, alpha, beta, lims, lim_q1, lim_p1);
    } else if filter_p1 || filter_q1 {
        rv40_weak_loop_filter(buf, src, step, along, filter_p1, filter_q1, alpha, beta, lims >> 1, lim_q1 >> 1, lim_p1 >> 1);
    }
}

/// `rv40_loop_filter` for macroblock row `row`.
pub(super) fn loop_filter(g: Geometry, pic: &mut Picture, deblock_coefs: &mut [u16], cbp_luma: &mut [u16], cbp_chroma: &mut [u8], row: usize) {
    let mut mb_pos = row * g.mb_stride;
    for _ in 0..g.mb_width {
        let mbtype = pic.mb_type[mb_pos];
        if is_intra(mbtype) || mbtype & MB_TYPE_SEPARATE_DC != 0 {
            cbp_luma[mb_pos] = 0xFFFF;
            deblock_coefs[mb_pos] = 0xFFFF;
        }
        if is_intra(mbtype) {
            cbp_chroma[mb_pos] = 0xFF;
        }
        mb_pos += 1;
    }

    let mut mb_pos = row * g.mb_stride;
    for mb_x in 0..g.mb_width {
        let q = (pic.qscale[mb_pos] as u8 & 31) as usize;
        let alpha = RV40_ALPHA_TAB[q] as i32;
        let beta = RV40_BETA_TAB[q] as i32;
        let beta_c = beta * 3;
        let mut beta_y = beta * 3;
        if g.width * g.height <= 176 * 144 {
            beta_y += beta;
        }

        let avail = [true, row != 0, mb_x != 0, row < g.mb_height - 1];
        let mut mvmasks = [0u32; 4];
        let mut mbtype = [0u32; 4];
        let mut cbp = [0u32; 4];
        let mut uvcbp = [[0u32; 2]; 4];
        let mut mb_strong = [false; 4];
        let mut clip = [0i32; 4];
        for i in 0..4 {
            if avail[i] {
                let pos = (mb_pos as isize + NEIGHBOUR_OFFS_X[i] + NEIGHBOUR_OFFS_Y[i] * g.mb_stride as isize) as usize;
                mvmasks[i] = deblock_coefs[pos] as u32;
                mbtype[i] = pic.mb_type[pos];
                cbp[i] = cbp_luma[pos] as u32;
                uvcbp[i][0] = (cbp_chroma[pos] & 0xF) as u32;
                uvcbp[i][1] = (cbp_chroma[pos] >> 4) as u32;
            } else {
                mvmasks[i] = 0;
                mbtype[i] = mbtype[0];
                cbp[i] = 0;
                uvcbp[i] = [0, 0];
            }
            mb_strong[i] = is_intra(mbtype[i]) || mbtype[i] & MB_TYPE_SEPARATE_DC != 0;
            clip[i] = RV40_FILTER_CLIP_TBL[mb_strong[i] as usize + 1][q] as i32;
        }
        let y_to_deblock = mvmasks[POS_CUR] | (mvmasks[POS_BOTTOM] << 16);
        // Horizontal edges of the current block can be filtered when either
        // neighbouring subblock is coded or lies on an 8x8 edge with motion
        // vectors differing by more than 3/4 pel.
        let mut y_h_deblock = y_to_deblock | ((cbp[POS_CUR] << 4) & !MASK_Y_TOP_ROW) | ((cbp[POS_TOP] & MASK_Y_LAST_ROW) >> 12);
        // Likewise for vertical edges.
        let mut y_v_deblock = y_to_deblock | ((cbp[POS_CUR] << 1) & !MASK_Y_LEFT_COL) | ((cbp[POS_LEFT] & MASK_Y_RIGHT_COL) >> 3);
        if mb_x == 0 {
            y_v_deblock &= !MASK_Y_LEFT_COL;
        }
        if row == 0 {
            y_h_deblock &= !MASK_Y_TOP_ROW;
        }
        if row == g.mb_height - 1 || (mb_strong[POS_CUR] | mb_strong[POS_BOTTOM]) {
            y_h_deblock &= !(MASK_Y_TOP_ROW << 16);
        }
        // Chroma patterns: no motion vector part.
        let mut c_v_deblock = [0u32; 2];
        let mut c_h_deblock = [0u32; 2];
        let mut c_to_deblock = [0u32; 2];
        for i in 0..2 {
            c_to_deblock[i] = (uvcbp[POS_BOTTOM][i] << 4) | uvcbp[POS_CUR][i];
            c_v_deblock[i] = c_to_deblock[i] | ((uvcbp[POS_CUR][i] << 1) & !MASK_C_LEFT_COL) | ((uvcbp[POS_LEFT][i] & MASK_C_RIGHT_COL) >> 1);
            c_h_deblock[i] = c_to_deblock[i] | ((uvcbp[POS_TOP][i] & MASK_C_LAST_ROW) >> 2) | (uvcbp[POS_CUR][i] << 2);
            if mb_x == 0 {
                c_v_deblock[i] &= !MASK_C_LEFT_COL;
            }
            if row == 0 {
                c_h_deblock[i] &= !MASK_C_TOP_ROW;
            }
            if row == g.mb_height - 1 || (mb_strong[POS_CUR] | mb_strong[POS_BOTTOM]) {
                c_h_deblock[i] &= !(MASK_C_TOP_ROW << 4);
            }
        }

        let strong_cl = mb_strong[POS_CUR] | mb_strong[POS_LEFT];
        let strong_ct = mb_strong[POS_CUR] | mb_strong[POS_TOP];
        {
            let yp = &mut pic.planes[0];
            let ls = yp.stride;
            for j in (0..16usize).step_by(4) {
                let mut y = yp.idx(mb_x * 16, row * 16 + j);
                for i in 0..4usize {
                    let ij = (i + j) as u32;
                    let clip_cur = if y_to_deblock & (MASK_CUR << ij) != 0 { clip[POS_CUR] } else { 0 };
                    let dither = if j != 0 { ij as usize } else { i * 4 };

                    // If the bottom block is coded, filter its top edge
                    // (the bottom edge of this block).
                    if y_h_deblock & (MASK_BOTTOM << ij) != 0 {
                        let lim_q1 = if y_to_deblock & (MASK_BOTTOM << ij) != 0 { clip[POS_CUR] } else { 0 };
                        adaptive_loop_filter(&mut yp.data, y + 4 * ls, ls, dither, lim_q1, clip_cur, alpha, beta, beta_y, false, false, 0);
                    }
                    // Left edge in ordinary mode (low strength).
                    if y_v_deblock & (MASK_CUR << ij) != 0 && (i != 0 || !strong_cl) {
                        let clip_left = if i == 0 {
                            if mvmasks[POS_LEFT] & (MASK_RIGHT << j) != 0 { clip[POS_LEFT] } else { 0 }
                        } else if y_to_deblock & (MASK_CUR << (ij - 1)) != 0 {
                            clip[POS_CUR]
                        } else {
                            0
                        };
                        adaptive_loop_filter(&mut yp.data, y, ls, dither, clip_cur, clip_left, alpha, beta, beta_y, false, false, 1);
                    }
                    // Top edge of the macroblock at high strength.
                    if j == 0 && y_h_deblock & (MASK_CUR << i) != 0 && strong_ct {
                        let lim_p1 = if mvmasks[POS_TOP] & (MASK_TOP << i) != 0 { clip[POS_TOP] } else { 0 };
                        adaptive_loop_filter(&mut yp.data, y, ls, dither, clip_cur, lim_p1, alpha, beta, beta_y, false, true, 0);
                    }
                    // Left edge in edge mode (high strength).
                    if y_v_deblock & (MASK_CUR << ij) != 0 && i == 0 && strong_cl {
                        let clip_left = if mvmasks[POS_LEFT] & (MASK_RIGHT << j) != 0 { clip[POS_LEFT] } else { 0 };
                        adaptive_loop_filter(&mut yp.data, y, ls, dither, clip_cur, clip_left, alpha, beta, beta_y, false, true, 1);
                    }
                    y += 4;
                }
            }
        }
        for k in 0..2 {
            let cp = &mut pic.planes[k + 1];
            let uvs = cp.stride;
            for j in 0..2usize {
                let mut c = cp.idx(mb_x * 8, row * 8 + j * 4);
                for i in 0..2usize {
                    let ij = (i + j * 2) as u32;
                    let clip_cur = if c_to_deblock[k] & (MASK_CUR << ij) != 0 { clip[POS_CUR] } else { 0 };
                    if c_h_deblock[k] & (MASK_CUR << (ij + 2)) != 0 {
                        let clip_bot = if c_to_deblock[k] & (MASK_CUR << (ij + 2)) != 0 { clip[POS_CUR] } else { 0 };
                        adaptive_loop_filter(&mut cp.data, c + 4 * uvs, uvs, i * 8, clip_bot, clip_cur, alpha, beta, beta_c, true, false, 0);
                    }
                    if c_v_deblock[k] & (MASK_CUR << ij) != 0 && (i != 0 || !strong_cl) {
                        let clip_left = if i == 0 {
                            if uvcbp[POS_LEFT][k] & (MASK_CUR << (2 * j + 1)) != 0 { clip[POS_LEFT] } else { 0 }
                        } else if c_to_deblock[k] & (MASK_CUR << (ij - 1)) != 0 {
                            clip[POS_CUR]
                        } else {
                            0
                        };
                        adaptive_loop_filter(&mut cp.data, c, uvs, j * 8, clip_cur, clip_left, alpha, beta, beta_c, true, false, 1);
                    }
                    if j == 0 && c_h_deblock[k] & (MASK_CUR << ij) != 0 && strong_ct {
                        let clip_top = if uvcbp[POS_TOP][k] & (MASK_CUR << (ij + 2)) != 0 { clip[POS_TOP] } else { 0 };
                        adaptive_loop_filter(&mut cp.data, c, uvs, i * 8, clip_cur, clip_top, alpha, beta, beta_c, true, true, 0);
                    }
                    if c_v_deblock[k] & (MASK_CUR << ij) != 0 && i == 0 && strong_cl {
                        let clip_left = if uvcbp[POS_LEFT][k] & (MASK_CUR << (2 * j + 1)) != 0 { clip[POS_LEFT] } else { 0 };
                        adaptive_loop_filter(&mut cp.data, c, uvs, j * 8, clip_cur, clip_left, alpha, beta, beta_c, true, true, 1);
                    }
                    c += 4;
                }
            }
        }
        mb_pos += 1;
    }
}

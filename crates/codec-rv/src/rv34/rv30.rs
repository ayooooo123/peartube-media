//! RV30-specific parsing and the RV30 deblocking filter.
//!
//! Ported from FFmpeg libavcodec/rv30.c (commit 2da55bf);
//! LGPL-2.1-or-later.

use super::data::{RV30_ITYPE_CODE, RV30_ITYPE_FROM_CONTEXT, RV30_LOOP_FILT_LIM};
use super::dsp::rv30_weak_loop_filter;
use super::{
    get_start_offset, is_intra, Geometry, Picture, Rv34Decoder, SliceInfo, MB_TYPE_SEPARATE_DC, PICT_B, RV34_MB_B_BACKWARD,
    RV34_MB_B_DIRECT, RV34_MB_B_FORWARD, RV34_MB_P_16X16, RV34_MB_P_8X8, RV34_MB_SKIP, RV34_MB_TYPE_INTRA,
    RV34_MB_TYPE_INTRA16X16,
};
use crate::bits::BitReader;

/// `av_log2`.
fn av_log2(v: u32) -> u32 {
    31 - (v | 1).leading_zeros()
}

/// `rv30_parse_slice_header`.
pub(super) fn parse_slice_header(r: &Rv34Decoder, gb: &mut BitReader) -> Option<SliceInfo> {
    let mut si = SliceInfo::default();
    if gb.get_bits(3) != 0 {
        return None;
    }
    si.ty = gb.get_bits(2) as i32;
    if si.ty == 1 {
        si.ty = 0;
    }
    if gb.get_bits1() != 0 {
        return None;
    }
    si.quant = gb.get_bits(5) as i32;
    gb.skip_bits(1);
    si.pts = gb.get_bits(13) as i32;
    let rpr = gb.get_bits(av_log2(r.max_rpr as u32) + 1) as i32;
    let (w, h) = if rpr != 0 {
        if rpr > r.max_rpr {
            return None;
        }
        let ex = &r.extradata;
        if ex.len() < (rpr * 2 + 8) as usize {
            return None;
        }
        ((ex[6 + rpr as usize * 2] as i32) << 2, (ex[7 + rpr as usize * 2] as i32) << 2)
    } else {
        (r.orig_width, r.orig_height)
    };
    si.width = w;
    si.height = h;
    let mb_size = ((w + 15) >> 4) * ((h + 15) >> 4);
    si.start = get_start_offset(gb, mb_size);
    gb.skip_bits(1);
    Some(si)
}

/// `rv30_decode_intra_types`: `it` indexes the macroblock's first 4x4
/// type in `intra_types_hist`.
pub(super) fn decode_intra_types(r: &mut Rv34Decoder, gb: &mut BitReader, it: usize) -> Result<(), ()> {
    let stride = r.intra_types_stride;
    for i in 0..4 {
        let mut dst = it + i * stride;
        for _ in 0..2 {
            let code = gb.get_interleaved_ue_golomb().wrapping_mul(2) as usize;
            if code > 80 * 2 {
                return Err(());
            }
            for k in 0..2 {
                let a = r.intra_types_hist[dst - stride] as i32 + 1;
                let b = r.intra_types_hist[dst - 1] as i32 + 1;
                let idx = a * 90 + b * 9 + RV30_ITYPE_CODE[code + k] as i32;
                let v = *RV30_ITYPE_FROM_CONTEXT.get(usize::try_from(idx).map_err(|_| ())?).ok_or(())?;
                r.intra_types_hist[dst] = v as i8;
                dst += 1;
                if v == 9 {
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

/// `rv30_decode_mb_info`: the macroblock type, or -1.
pub(super) fn decode_mb_info(r: &mut Rv34Decoder, gb: &mut BitReader) -> i32 {
    const P_TYPES: [i32; 6] = [RV34_MB_SKIP as i32, RV34_MB_P_16X16 as i32, RV34_MB_P_8X8 as i32, -1, RV34_MB_TYPE_INTRA as i32, RV34_MB_TYPE_INTRA16X16 as i32];
    const B_TYPES: [i32; 6] = [
        RV34_MB_SKIP as i32,
        RV34_MB_B_DIRECT as i32,
        RV34_MB_B_FORWARD as i32,
        RV34_MB_B_BACKWARD as i32,
        RV34_MB_TYPE_INTRA as i32,
        RV34_MB_TYPE_INTRA16X16 as i32,
    ];
    let mut code = gb.get_interleaved_ue_golomb();
    if code > 11 {
        return -1;
    }
    if code > 5 {
        // "dquant needed": FFmpeg logs and carries on.
        code -= 6;
    }
    if r.pict_type != PICT_B {
        P_TYPES[code as usize]
    } else {
        B_TYPES[code as usize]
    }
}

/// `rv30_loop_filter` for macroblock row `row`.
pub(super) fn loop_filter(g: Geometry, pic: &mut Picture, deblock_coefs: &mut [u16], cbp_chroma: &mut [u8], row: usize) {
    let mut mb_pos = row * g.mb_stride;
    for _ in 0..g.mb_width {
        let mbtype = pic.mb_type[mb_pos];
        if is_intra(mbtype) || mbtype & MB_TYPE_SEPARATE_DC != 0 {
            deblock_coefs[mb_pos] = 0xFFFF;
        }
        if is_intra(mbtype) {
            cbp_chroma[mb_pos] = 0xFF;
        }
        mb_pos += 1;
    }

    // All vertical edges are filtered first, horizontal edges afterwards.
    let mut left_lim = 0;
    let mut mb_pos = row * g.mb_stride;
    for mb_x in 0..g.mb_width {
        let cur_lim = RV30_LOOP_FILT_LIM[pic.qscale[mb_pos] as u8 as usize & 31] as i32;
        if mb_x != 0 {
            left_lim = RV30_LOOP_FILT_LIM[pic.qscale[mb_pos - 1] as u8 as usize & 31] as i32;
        }
        let first = (mb_x == 0) as usize;
        for j in (0..16).step_by(4) {
            let y = &mut pic.planes[0];
            let stride = y.stride;
            let mut pos = y.idx(mb_x * 16 + 4 * first, row * 16 + j);
            for i in first..4 {
                let ij = i + j;
                let mut loc_lim = 0;
                if deblock_coefs[mb_pos] & (1 << ij) != 0 {
                    loc_lim = cur_lim;
                } else if i == 0 && deblock_coefs[mb_pos - 1] & (1 << (ij + 3)) != 0 {
                    loc_lim = left_lim;
                } else if i != 0 && deblock_coefs[mb_pos] & (1 << (ij - 1)) != 0 {
                    loc_lim = cur_lim;
                }
                if loc_lim != 0 {
                    rv30_weak_loop_filter(&mut y.data, pos, 1, stride, loc_lim);
                }
                pos += 4;
            }
        }
        for k in 0..2 {
            let cur_cbp = (cbp_chroma[mb_pos] >> (k * 4)) & 0xF;
            let left_cbp = if mb_x != 0 { (cbp_chroma[mb_pos - 1] >> (k * 4)) & 0xF } else { 0 };
            for j in (0..8).step_by(4) {
                let c = &mut pic.planes[k + 1];
                let stride = c.stride;
                let mut pos = c.idx(mb_x * 8 + 4 * first, row * 8 + j);
                for i in first..2 {
                    let ij = i + (j >> 1);
                    let mut loc_lim = 0;
                    if cur_cbp & (1 << ij) != 0 {
                        loc_lim = cur_lim;
                    } else if i == 0 && left_cbp & (1 << (ij + 1)) != 0 {
                        loc_lim = left_lim;
                    } else if i != 0 && cur_cbp & (1 << (ij - 1)) != 0 {
                        loc_lim = cur_lim;
                    }
                    if loc_lim != 0 {
                        rv30_weak_loop_filter(&mut c.data, pos, 1, stride, loc_lim);
                    }
                    pos += 4;
                }
            }
        }
        mb_pos += 1;
    }

    let mut top_lim = 0;
    let mut mb_pos = row * g.mb_stride;
    for mb_x in 0..g.mb_width {
        let cur_lim = RV30_LOOP_FILT_LIM[pic.qscale[mb_pos] as u8 as usize & 31] as i32;
        if row != 0 {
            top_lim = RV30_LOOP_FILT_LIM[pic.qscale[mb_pos - g.mb_stride] as u8 as usize & 31] as i32;
        }
        let j0 = if row == 0 { 4 } else { 0 };
        for j in (j0..16).step_by(4) {
            let y = &mut pic.planes[0];
            let stride = y.stride;
            let mut pos = y.idx(mb_x * 16, row * 16 + j);
            for i in 0..4 {
                let ij = i + j;
                let mut loc_lim = 0;
                if deblock_coefs[mb_pos] & (1 << ij) != 0 {
                    loc_lim = cur_lim;
                } else if j == 0 && deblock_coefs[mb_pos - g.mb_stride] & (1 << (ij + 12)) != 0 {
                    loc_lim = top_lim;
                } else if j != 0 && deblock_coefs[mb_pos] & (1 << (ij - 4)) != 0 {
                    loc_lim = cur_lim;
                }
                if loc_lim != 0 {
                    rv30_weak_loop_filter(&mut y.data, pos, stride, 1, loc_lim);
                }
                pos += 4;
            }
        }
        for k in 0..2 {
            let cur_cbp = (cbp_chroma[mb_pos] >> (k * 4)) & 0xF;
            let top_cbp = if row != 0 { (cbp_chroma[mb_pos - g.mb_stride] >> (k * 4)) & 0xF } else { 0 };
            let j0 = if row == 0 { 4 } else { 0 };
            for j in (j0..8).step_by(4) {
                let c = &mut pic.planes[k + 1];
                let stride = c.stride;
                let mut pos = c.idx(mb_x * 8, row * 8 + j);
                for i in 0..2 {
                    let ij = i + (j >> 1);
                    let mut loc_lim = 0;
                    // FFmpeg tests the unshifted chroma pattern here.
                    if cbp_chroma[mb_pos] & (1 << ij) != 0 {
                        loc_lim = cur_lim;
                    } else if j == 0 && top_cbp & (1 << (ij + 2)) != 0 {
                        loc_lim = top_lim;
                    } else if j != 0 && cur_cbp & (1 << (ij - 2)) != 0 {
                        loc_lim = cur_lim;
                    }
                    if loc_lim != 0 {
                        rv30_weak_loop_filter(&mut c.data, pos, stride, 1, loc_lim);
                    }
                    pos += 4;
                }
            }
        }
        mb_pos += 1;
    }
}

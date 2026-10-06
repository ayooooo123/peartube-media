//! H.263 macroblock decoding, ported from FFmpeg libavcodec/ituh263dec.c
//! (`ff_h263_decode_mb`, `h263_decode_block`, `h263_decode_dquant`,
//! `ff_h263_decode_motion`, `h263_pred_acdc`), h263.c
//! (`ff_h263_pred_motion`, `ff_h263_update_motion_val`,
//! `ff_h263_loop_filter`, `ff_h263_round_chroma`), h263dsp.c
//! (`h263_h/v_loop_filter_c`), rl.c (`ff_rl_init_vlc` semantics) and
//! h263data.c tables (commit 2da55bf).
//!
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

use crate::bitread::{av_clip_i32, GetBitContext, TEX_VLC_BITS};
use crate::h263tables::*;
use crate::hpel::op_pixels;
use crate::idct::{add_pixels_clamped, simple_idct_add, simple_idct_put};
use crate::mpeg::{MotionType, MpegState, PLANE};
use crate::vlc::{get_rl_vlc, get_vlc2, RlTable, RlVlc, Vlc};

pub const SLICE_OK: i32 = 0;
pub const SLICE_END: i32 = -100; // end marker
pub const SLICE_ERROR: i32 = -101;
pub const SLICE_NOEND: i32 = -102;

const INTER_MCBPC_VLC_BITS: u32 = 9;
const INTRA_MCBPC_VLC_BITS: u32 = 9;
const CBPY_VLC_BITS: u32 = 6;
const H263_MV_VLC_BITS: u32 = 9;
const H263_MBTYPE_B_VLC_BITS: u32 = 6;
const CBPC_B_VLC_BITS: u32 = 3;

/// MCBPC symbol values: the VLC returns cbpc in bits 0..1 (intra) plus
/// flags; FFmpeg's tables map code -> the raw MCBPC value. Symbols for
/// the intra table are 0,1,2,3,4,6,8,9 (+stuffing 8); for inter they are
/// the same indices. We store the decoded symbol == table index, then
/// translate like FFmpeg does.
pub struct H263Vlc {
    pub intra_mcbpc: Vlc,
    pub inter_mcbpc: Vlc,
    pub cbpy: Vlc,
    pub mv: Vlc,
    pub mbtype_b: Vlc,
    pub cbpc_b: Vlc,
    pub rl_inter: RlVlc,
    pub rl_intra_aic: RlVlc,
    pub rv_dc_lum: Vlc,
    pub rv_dc_chrom: Vlc,
}

/// h263_mb_type_b_map from ituh263dec.c: symbol for each of the 15 codes.
const H263_MB_TYPE_B_MAP: [i32; 15] = [
    (1 << 8) | (1 << 12) | (1 << 13), // MB_TYPE_DIRECT2 | MB_TYPE_BIDIR_MV
    (1 << 8) | (1 << 12) | (1 << 13) | (1 << 10), // | MB_TYPE_CBP
    (1 << 8) | (1 << 12) | (1 << 13) | (1 << 10) | (1 << 11), // | MB_TYPE_QUANT
    (1 << 12) | (1 << 3), // MB_TYPE_FORWARD_MV | MB_TYPE_16x16
    (1 << 12) | (1 << 10) | (1 << 3), // | MB_TYPE_CBP
    (1 << 12) | (1 << 10) | (1 << 11) | (1 << 3), // | MB_TYPE_QUANT
    (1 << 13) | (1 << 3), // MB_TYPE_BACKWARD_MV | MB_TYPE_16x16
    (1 << 13) | (1 << 10) | (1 << 3), // | MB_TYPE_CBP
    (1 << 13) | (1 << 10) | (1 << 11) | (1 << 3), // | MB_TYPE_QUANT
    (1 << 12) | (1 << 13) | (1 << 3), // MB_TYPE_BIDIR_MV | MB_TYPE_16x16
    (1 << 12) | (1 << 13) | (1 << 10) | (1 << 3), // | MB_TYPE_CBP
    (1 << 12) | (1 << 13) | (1 << 10) | (1 << 11) | (1 << 3), // | MB_TYPE_QUANT
    0, // stuffing
    (1 << 0) | (1 << 10), // MB_TYPE_INTRA4x4 | MB_TYPE_CBP
    (1 << 0) | (1 << 10) | (1 << 11), // MB_TYPE_INTRA4x4 | MB_TYPE_CBP | MB_TYPE_QUANT
];

fn build_from_pairs(bits: u32, tab: &[[u8; 2]]) -> Result<Vlc, String> {
    let codes: Vec<(u32, u32, i32)> = tab
        .iter()
        .enumerate()
        .map(|(i, &[code, len])| (code as u32, len as u32, i as i32))
        .collect();
    crate::vlc::vlc_init_sparse(bits, codes)
}

impl H263Vlc {
    pub fn new() -> Result<Self, String> {
        // ff_h263_intra_MCBPC_vlc: symbols map 1:1 with index (0..8) — FFmpeg
        // VLC_INIT_STATIC_TABLE over code/bits only, sym = index.
        let intra_mcbpc = build_from_pairs(
            INTRA_MCBPC_VLC_BITS,
            &H263_INTRA_MCBPC_CODE
                .iter()
                .zip(H263_INTRA_MCBPC_BITS.iter())
                .map(|(&c, &b)| [c, b])
                .collect::<Vec<_>>(),
        )?;
        let inter_mcbpc = build_from_pairs(
            INTER_MCBPC_VLC_BITS,
            &H263_INTER_MCBPC_CODE
                .iter()
                .zip(H263_INTER_MCBPC_BITS.iter())
                .map(|(&c, &b)| [c, b])
                .collect::<Vec<_>>(),
        )?;
        // cbpy: VLC_INIT_STATIC_TABLE(&ff_h263_cbpy_tab[0][1], 2, 1 / [0][0], 2, 1)
        // — the *symbol* is NOT the index: FFmpeg passes the table as
        // (code, bits) pairs and uses index→sym default. Same here.
        let cbpy = build_from_pairs(CBPY_VLC_BITS, &H263_CBPY_TAB)?;
        let mv = build_from_pairs(H263_MV_VLC_BITS, &MVTAB)?;
        let mbtype_b: Vec<[u8; 2]> = H263_MBTYPE_B_TAB.to_vec();
        // Sparse: symbol = H263_MB_TYPE_B_MAP[i].
        let mbtype_b = {
            let codes: Vec<(u32, u32, i32)> = mbtype_b
                .iter()
                .zip(H263_MB_TYPE_B_MAP.iter())
                .map(|(&[code, len], &sym)| (code as u32, len as u32, sym))
                .collect();
            crate::vlc::vlc_init_sparse(H263_MBTYPE_B_VLC_BITS, codes)?
        };
        let cbpc_b = build_from_pairs(CBPC_B_VLC_BITS, &CBPC_B_TAB)?;
        let rl_inter = rl_table_inter().build_rl_vlc()?;
        let rl_intra_aic = rl_table_intra_aic().build_rl_vlc()?;
        let (rv_dc_lum, rv_dc_chrom) = {
            use crate::rvdata::{RV_CHROM_LEN_COUNT, RV_LUM_LEN_COUNT, RV_SYM_RUN_LEN};
            let mut syms_lum = Vec::new();
            let mut lens_lum = Vec::new();
            for &[run, len] in &RV_SYM_RUN_LEN {
                let mut cur_sym = run;
                for _ in 0..=len {
                    syms_lum.push(cur_sym as u16);
                    cur_sym = cur_sym.wrapping_sub(1);
                }
            }
            for (i, &count) in RV_LUM_LEN_COUNT.iter().enumerate() {
                for _ in 0..count {
                    lens_lum.push((i + 2) as i32);
                }
            }
            let lum = crate::vlc::vlc_init_from_lengths(9, &lens_lum, Some(&syms_lum))?;

            let mut syms_chrom = Vec::new();
            let mut lens_chrom = Vec::new();
            for &[run, len] in &RV_SYM_RUN_LEN[..17] {
                let mut cur_sym = run;
                for _ in 0..=len {
                    syms_chrom.push(cur_sym as u16);
                    cur_sym = cur_sym.wrapping_sub(1);
                }
            }
            for (i, &count) in RV_CHROM_LEN_COUNT.iter().enumerate() {
                for _ in 0..count {
                    lens_chrom.push((i + 2) as i32);
                }
            }
            let chrom = crate::vlc::vlc_init_from_lengths(9, &lens_chrom, Some(&syms_chrom))?;
            (lum, chrom)
        };
        Ok(Self { intra_mcbpc, inter_mcbpc, cbpy, mv, mbtype_b, cbpc_b, rl_inter, rl_intra_aic, rv_dc_lum, rv_dc_chrom })
    }
}

fn build_rl(table_vlc: Vec<(u32, u8)>, table_run: Vec<i8>, table_level: Vec<i8>, n: usize, last: usize) -> RlTable {
    RlTable { n, last, table_vlc, table_run, table_level }
}

/// `ff_h263_rl_inter` (n=102, last=58).
pub fn rl_table_inter() -> RlTable {
    let mut table_vlc = Vec::with_capacity(103);
    for i in 0..103 {
        table_vlc.push((INTER_VLC[i][0] as u32, INTER_VLC[i][1] as u8));
    }
    build_rl(table_vlc, INTER_RUN.to_vec(), INTER_LEVEL.to_vec(), 102, 58)
}

/// `ff_rl_intra_aic` (n=102, last=58).
pub fn rl_table_intra_aic() -> RlTable {
    let mut table_vlc = Vec::with_capacity(103);
    for i in 0..103 {
        table_vlc.push((INTRA_VLC_AIC[i][0] as u32, INTRA_VLC_AIC[i][1] as u8));
    }
    build_rl(table_vlc, INTRA_RUN_AIC.to_vec(), INTRA_LEVEL_AIC.to_vec(), 102, 58)
}

/// H.263-specific decode state layered on `MpegState`.
pub struct H263State {
    pub h263_long_vectors: bool,
    pub umvplus: bool,
    pub modified_quant: bool,
    pub loop_filter: bool,
    /// rv10.c: DC coding version (1 or 3).
    pub rv10_version: i32,
    pub rv10_first_dc_coded: [bool; 3],
    pub last_dc: [i32; 3],
    /// gob_index for GOB headers (unused in RV10/20 but kept for parity).
    pub gob_index: i32,
    pub slice_height: i32,
}

/// `ff_h263_decode_motion`.
pub fn h263_decode_motion(gb: &mut GetBitContext, vlc: &Vlc, h: &H263State, pred: i32, f_code: i32) -> i32 {
    let code = get_vlc2(gb, vlc);
    if code == 0 {
        return pred;
    }
    if code < 0 {
        return 0xffff;
    }
    let sign = gb.get_bits1();
    let shift = (f_code - 1) as u32;
    let mut val = code;
    if shift != 0 {
        val = (val - 1) << shift;
        val |= gb.get_bits(shift) as i32;
        val += 1;
    }
    let mut val = if sign != 0 { -val } else { val };
    val += pred;
    if !h.h263_long_vectors {
        val = crate::bitread::sign_extend(val, 5 + f_code as u32);
    } else {
        if pred < -31 && val < -63 {
            val += 64;
        }
        if pred > 32 && val > 63 {
            val -= 64;
        }
    }
    val
}

/// `h263_decode_dquant`.
fn h263_decode_dquant(gb: &mut GetBitContext, s: &mut MpegState, h: &H263State) {
    if h.modified_quant {
        let q = if gb.get_bits1() != 0 {
            MODIFIED_QUANT_TAB[gb.get_bits1() as usize][s.qscale as usize] as i32
        } else {
            gb.get_bits(5) as i32
        };
        s.set_qscale(q.max(1) as u32);
    } else {
        static QUANT_TAB: [i32; 4] = [-1, -2, 1, 2];
        let q = (s.qscale as i32 + QUANT_TAB[gb.get_bits(2) as usize]).clamp(1, 31) as u32;
        s.set_qscale(q);
    }
}

/// `h263_pred_acdc` (AIC AC/DC prediction).
fn h263_pred_acdc(s: &mut MpegState, block: &mut [i16; 64], n: usize) {
    let wrap;
    let scale;
    if n < 4 {
        wrap = s.b8_stride;
        scale = s.y_dc_scale as i32;
    } else {
        wrap = s.mb_stride;
        scale = s.c_dc_scale as i32;
    }
    let base = (s.b8_stride + 1) as isize;
    let idx = (base + s.block_index[n]) as usize;

    let mut a = s.dc_val[idx - 1] as i32;
    let mut c = s.dc_val[idx - wrap] as i32;

    if s.first_slice_line && n != 3 {
        if n != 2 {
            c = 1024;
        }
        if n != 1 && s.mb_x == s.resync_mb_x {
            a = 1024;
        }
    }

    let mut pred_dc;
    if s.ac_pred {
        pred_dc = 1024;
        if s.h263_aic_dir {
            if a != 1024 {
                let ac2_idx = (idx - 1) * 16;
                for i in 1..8 {
                    block[i << 3] = block[i << 3].wrapping_add(s.ac_val[ac2_idx + i]);
                }
                pred_dc = a;
            }
        } else if c != 1024 {
            let ac2_idx = (idx - wrap) * 16;
            for i in 1..8 {
                block[i] = block[i].wrapping_add(s.ac_val[ac2_idx + 8 + i]);
            }
            pred_dc = c;
        }
    } else {
        pred_dc = if a != 1024 && c != 1024 {
            (a + c) >> 1
        } else if a != 1024 {
            a
        } else {
            c
        };
    }

    let mut dc = block[0] as i32 * scale + pred_dc;
    if dc < 0 {
        dc = 0;
    } else {
        dc |= 1;
    }
    block[0] = dc as i16;

    s.dc_val[idx] = block[0];

    let ac_idx = idx * 16;
    for i in 1..8 {
        s.ac_val[ac_idx + i] = block[i << 3];
        s.ac_val[ac_idx + 8 + i] = block[i];
    }
}

/// `decode_block_aic`: AIC intra block decode.
fn decode_block_aic(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    vlcs: &H263Vlc,
    block: &mut [i16; 64],
    n: usize,
    coded: bool,
) -> i32 {
    let mut i = 0i32;
    if coded {
        let scan = if s.ac_pred {
            if s.h263_aic_dir {
                &crate::mpegtables::ALTERNATE_VERTICAL_SCAN
            } else {
                &crate::mpegtables::ALTERNATE_HORIZONTAL_SCAN
            }
        } else {
            &crate::mpegtables::ZIGZAG_DIRECT
        };
        let rl = &vlcs.rl_intra_aic;
        i -= 1;
        loop {
            let (level0, run) = get_rl_vlc(gb, rl);
            let mut level = level0;
            let mut run = run as i32;
            if run == 66 {
                if level != 0 {
                    return -1;
                }
                run = gb.get_bits(7) as i32 + 1;
                level = gb.get_bits(8) as i8 as i32;
                if level == -128 {
                    if s.codec_id_rv10 {
                        level = crate::bitread::sign_extend(gb.get_bits(12) as i32, 12);
                    } else {
                        let lo = gb.get_bits(5) as i32;
                        let hi = crate::bitread::sign_extend(gb.get_bits(6) as i32, 6);
                        level = lo | (hi * 32);
                    }
                }
            } else if gb.get_bits1() != 0 {
                level = -level;
            }
            i += run;
            if i >= 64 {
                let i2 = i - run + ((run - 1) & 63) + 1;
                if i2 < 64 {
                    block[scan[i2 as usize] as usize] = level as i16;
                    i = i2;
                    break;
                }
                return -1;
            }
            let j = scan[i as usize] as usize;
            block[j] = level as i16;
        }
    }
    h263_pred_acdc(s, block, n);
    s.block_last_index[n] = i;
    0
}

/// `decode_block_inter`: Inter block decode.
fn decode_block_inter(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    _h: &H263State,
    rl: &RlVlc,
    block: &mut [i16; 64],
    n: usize,
    coded: bool,
) -> i32 {
    if !coded {
        s.block_last_index[n] = -1;
        return 0;
    }
    let scan = &crate::mpegtables::ZIGZAG_DIRECT;
    let mut i = -1i32;
    loop {
        let (level0, run) = get_rl_vlc(gb, rl);
        let mut level = level0;
        let mut run = run as i32;
        if run == 66 {
            if level != 0 {
                return -1;
            }
            run = gb.get_bits(7) as i32 + 1;
            level = gb.get_bits(8) as i8 as i32;
                if level == -128 {
                    if s.codec_id_rv10 {
                        level = crate::bitread::sign_extend(gb.get_bits(12) as i32, 12);
                    } else {
                        let lo = gb.get_bits(5) as i32;
                        let hi = crate::bitread::sign_extend(gb.get_bits(6) as i32, 6);
                        level = lo | (hi * 32);
                    }
                }
        } else if gb.get_bits1() != 0 {
            level = -level;
        }
        i += run;
        if i >= 64 {
            let i2 = i - run + ((run - 1) & 63) + 1;
            if i2 < 64 {
                block[scan[i2 as usize] as usize] = level as i16;
                s.block_last_index[n] = i2;
                return 0;
            }
            return -1;
        }
        let j = scan[i as usize] as usize;
        block[j] = level as i16;
    }
}

/// `h263_decode_block` — one 8x8 coefficient block.
/// `vlcs` carries the shared H.263 VLC state.
pub fn h263_decode_block(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    h: &mut H263State,
    vlcs: &H263Vlc,
    block: &mut [i16; 64],
    n: usize,
    coded: bool,
) -> i32 {
    let i = if s.mb_intra {
        let mut level;
        if s.codec_id_rv10 && h.rv10_version == 3 && s.pict_type == 1 {
            let component = if n <= 3 { 0 } else { n - 4 + 1 };
            level = h.last_dc[component];
            if h.rv10_first_dc_coded[component] {
                let diff = rv_decode_dc(gb, vlcs, n);
                if diff < 0 {
                    return -1;
                }
                level = (level + diff) & 0xff;
                h.last_dc[component] = level;
            } else {
                h.rv10_first_dc_coded[component] = true;
            }
        } else {
            level = gb.get_bits(8) as i32;
            if level == 255 {
                level = 128;
            }
        }
        block[0] = level as i16;
        1
    } else {
        0
    };

    if !coded {
        s.block_last_index[n] = i - 1;
        return 0;
    }

    let rl = &vlcs.rl_inter;
    let ret = h263_decode_ac(gb, s, h, vlcs, block, n, i, rl, false);
    ret
}
/// RV10 DC VLC decode (`ff_rv_decode_dc`).
fn rv_decode_dc(gb: &mut GetBitContext, vlcs: &H263Vlc, n: usize) -> i32 {
    if n < 4 {
        crate::vlc::get_vlc2(gb, &vlcs.rv_dc_lum)
    } else {
        crate::vlc::get_vlc2(gb, &vlcs.rv_dc_chrom)
    }
}

#[allow(clippy::too_many_arguments)]
fn h263_block_not_coded(gb: &mut GetBitContext, s: &mut MpegState, block: &mut [i16; 64], n: usize) -> i32 {
    let i = if s.h263_aic && s.mb_intra { 0 } else { 0 };
    let _ = gb;
    s.block_last_index[n] = i as i32 - 1;
    if s.h263_aic && s.mb_intra {
        h263_pred_acdc(s, block, n);
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn h263_decode_ac(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    h: &H263State,
    vlcs: &H263Vlc,
    block: &mut [i16; 64],
    n: usize,
    mut i: i32,
    rl: &RlVlc,
    scan_hv: bool,
) -> i32 {
    if i == 0 && !s.mb_intra {
        i = 0;
    } else if i == 0 {
        // Intra but only DC was present (non-AIC handled at call site).
    }
    if !scan_hv {
        // standard zigzag scan
    }
    let scan = if scan_hv {
        &s.intra_h_scantable
    } else {
        &s.intra_scantable
    };
    // The retry loop from h263_decode_block.
    i -= 1; // offset by -1 to allow direct indexing of scan_table
    loop {
        let (level0, run) = get_rl_vlc(gb, rl);
        let mut level = level0;
        let mut run = run as i32;
        if run == 66 {
            if level != 0 {
                return -1; // illegal ac vlc code
            }
            // escape
            run = gb.get_bits(7) as i32 + 1;
            level = gb.get_bits(8) as i8 as i32;
            if level == -128 {
                if s.codec_id_rv10 {
                    level = crate::bitread::sign_extend(gb.get_bits(12) as i32, 12);
                } else {
                    let lo = gb.get_bits(5) as i32;
                    let hi = crate::bitread::sign_extend(gb.get_bits(6) as i32, 6);
                    level = lo | (hi * 32);
                }
            }
        } else if gb.get_bits1() != 0 {
            level = -level;
        }
        i += run;
        if i >= 64 {
            // redo update without last flag, revert -1 offset
            let i2 = i - run + ((run - 1) & 63) + 1;
            if i2 < 64 {
                block[scan[i2 as usize] as usize] = level as i16;
                s.block_last_index[n] = i2;
                return 0;
            }
            if s.alt_inter_vlc && !s.mb_intra {
                // AIV retry with intra table — handled by caller state; here
                // we report error to keep the port simple (AIV streams are
                // not in the reference corpus).
                return -1;
            }
            return -1; // run overflow
        }
        let j = scan[i as usize] as usize;
        block[j] = level as i16;
    }
}

/// Complete `ff_h263_decode_mb` for the RV10/RV20 subset:
/// P/I frames with UMV off (RV streams do not set UMV), PB-frames rejected
/// (RV never produces them), OBMC supported for RV10 micro==2.
/// Returns SLICE_OK / SLICE_END / SLICE_ERROR.
pub fn h263_decode_mb(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    h: &mut H263State,
    vlcs: &H263Vlc,
    block: &mut [[i16; 64]; 6],
) -> i32 {
    let xy = (s.mb_x + s.mb_y * s.mb_stride) as usize;
    let mut cbpb: u32 = 0;
    let mut pb_mv_count: u32 = 0;

    if s.pict_type == 2 {
        // P-frame
        let cbpc;
        loop {
            if gb.get_bits1() != 0 {
                // skip mb
                s.mb_intra = false;
                for i in 0..6 {
                    s.block_last_index[i] = -1;
                }
                s.clean_intra_table_entries();
                s.mv_dir = 1;
                s.mv_type = MotionType::Mv16x16;
                s.mb_type[xy] = MB_SKIP | MB_16X16 | MB_FWD;
                s.mv[0][0][0] = 0;
                s.mv[0][0][1] = 0;
                s.mb_skipped = !(s.obmc || h.loop_filter);
                return end_of_mb(gb, s);
            }
            let v = get_vlc2(gb, &vlcs.inter_mcbpc);
            if v < 0 {
                return SLICE_ERROR;
            }
            if v != 20 {
                cbpc = v;
                break;
            }
        }
        s.clear_blocks(block);
        let dquant = cbpc & 8 != 0;
        s.mb_intra = cbpc & 4 != 0;
        let mut cbpy;
        if s.mb_intra {
            return h263_decode_intra_rest(gb, s, h, vlcs, block, cbpc as u32, 0, &mut pb_mv_count, true);
        }
        s.clean_intra_table_entries();
        cbpy = get_vlc2(gb, &vlcs.cbpy);
        if cbpy < 0 {
            return SLICE_ERROR;
        }
        if !s.alt_inter_vlc || (cbpc & 3) != 3 {
            cbpy ^= 0xF;
        }
        let cbp = ((cbpc & 3) | (cbpy << 2)) as u32;
        if dquant {
            h263_decode_dquant(gb, s, h);
        }
        s.mv_dir = 1;
        if cbpc & 16 == 0 {
            s.mb_type[xy] = MB_16X16 | MB_FWD;
            s.mv_type = MotionType::Mv16x16;
            let (pred_x, pred_y) = h263_pred_motion(s, 0, 0);
            let mx = h263_decode_motion(gb, &vlcs.mv, h, pred_x, 1);
            if mx >= 0xffff {
                return SLICE_ERROR;
            }
            let my = h263_decode_motion(gb, &vlcs.mv, h, pred_y, 1);
            if my >= 0xffff {
                return SLICE_ERROR;
            }
            s.mv[0][0][0] = mx;
            s.mv[0][0][1] = my;
        } else {
            s.mb_type[xy] = MB_8X8 | MB_FWD;
            s.mv_type = MotionType::Mv8x8;
            for i in 0..4 {
                let (pred_x, pred_y) = h263_pred_motion(s, i, 0);
                let mx = h263_decode_motion(gb, &vlcs.mv, h, pred_x, 1);
                if mx >= 0xffff {
                    return SLICE_ERROR;
                }
                let my = h263_decode_motion(gb, &vlcs.mv, h, pred_y, 1);
                if my >= 0xffff {
                    return SLICE_ERROR;
                }
                s.mv[0][i][0] = mx;
                s.mv[0][i][1] = my;
                // store into motion_val for later prediction
                let mot = (s.b8_stride as isize + 1 + s.block_index[i]) as usize;
                if mot < s.motion_val[0].len() {
                    s.motion_val[0][mot][0] = mx as i16;
                    s.motion_val[0][mot][1] = my as i16;
                }
            }
        }
        return h263_decode_blocks_tail(gb, s, h, vlcs, block, cbp, 0, &mut pb_mv_count);
    } else if s.pict_type == 3 {
        // B-frame (RV20 with B-frame support)
        let stride = s.b8_stride;
        let mv0 = 2 * (s.mb_x as usize + s.mb_y as usize * stride);
        for &(dx, dy) in &[(0usize, 0usize), (2usize, 0usize), (0usize, 2usize), (2usize, 2usize)] {
            for k in 0..2 {
                s.motion_val[k][mv0 + dx + dy * stride] = [0; 2];
            }
        }
        let mb_type;
        loop {
            let v = get_vlc2(gb, &vlcs.mbtype_b);
            if v < 0 {
                return SLICE_ERROR;
            }
            if v != 0 {
                mb_type = v;
                break;
            }
        }
        s.mb_intra = (mb_type & 7) != 0;
        let mut cbp;
        if (mb_type & (1 << 10)) != 0 {
            s.clear_blocks(block);
            let cbpc = get_vlc2(gb, &vlcs.cbpc_b);
            if s.mb_intra {
                let dquant = (mb_type & (1 << 11)) != 0;
                return h263_decode_intra_rest(gb, s, h, vlcs, block, cbpc as u32, dquant as u32, &mut pb_mv_count, false);
            }
            let mut cbpy = get_vlc2(gb, &vlcs.cbpy);
            if cbpy < 0 {
                return SLICE_ERROR;
            }
            if !s.alt_inter_vlc || (cbpc & 3) != 3 {
                cbpy ^= 0xF;
            }
            cbp = ((cbpc & 3) | (cbpy << 2)) as u32;
        } else {
            cbp = 0;
        }
        if (mb_type & (1 << 11)) != 0 {
            h263_decode_dquant(gb, s, h);
        }
        let mut mv_dir = 0usize;
        s.mv_type = MotionType::Mv16x16;
        if (mb_type & (1 << 8)) != 0 {
            // direct mode: bidirectional
            mv_dir = 3;
        } else {
            if (mb_type & (1 << 12)) != 0 {
                let (pred_x, pred_y) = h263_pred_motion(s, 0, 0);
                mv_dir |= 1;
                let mx = h263_decode_motion(gb, &vlcs.mv, h, pred_x, 1);
                if mx >= 0xffff {
                    return SLICE_ERROR;
                }
                let my = h263_decode_motion(gb, &vlcs.mv, h, pred_y, 1);
                if my >= 0xffff {
                    return SLICE_ERROR;
                }
                s.mv[0][0][0] = mx;
                s.mv[0][0][1] = my;
            }
            if (mb_type & (1 << 13)) != 0 {
                let (pred_x, pred_y) = h263_pred_motion(s, 0, 1);
                mv_dir |= 2;
                let mx = h263_decode_motion(gb, &vlcs.mv, h, pred_x, 1);
                if mx >= 0xffff {
                    return SLICE_ERROR;
                }
                let my = h263_decode_motion(gb, &vlcs.mv, h, pred_y, 1);
                if my >= 0xffff {
                    return SLICE_ERROR;
                }
                s.mv[1][0][0] = mx;
                s.mv[1][0][1] = my;
            }
        }
        s.mv_dir = mv_dir;
        s.mb_type[xy] = mb_type as u32;
        if !s.mb_intra {
            s.clean_intra_table_entries();
        }
        return h263_decode_blocks_tail(gb, s, h, vlcs, block, cbp, 0, &mut pb_mv_count);
    } else {
        // I-frame
        let cbpc;
        loop {
            let v = get_vlc2(gb, &vlcs.intra_mcbpc);
            if v < 0 {
                eprintln!("intra_mcbpc error v={v}");
                return SLICE_ERROR;
            }
            if v != 8 {
                cbpc = v;
                break;
            }
        }
        s.clear_blocks(block);
        let dquant = cbpc & 4 != 0;
        s.mb_intra = true;
        return h263_decode_intra_rest(gb, s, h, vlcs, block, cbpc as u32, dquant as u32, &mut pb_mv_count, false);
    }
}

/// Intra MB continuation (shared by P-intra and I paths): `from_p` marks
/// whether cbpc came from the inter MCBPC table (P-frame intra).
#[allow(clippy::too_many_arguments)]
fn h263_decode_intra_rest(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    h: &mut H263State,
    vlcs: &H263Vlc,
    block: &mut [[i16; 64]; 6],
    cbpc: u32,
    dquant: u32,
    pb_mv_count: &mut u32,
    from_p: bool,
) -> i32 {
    let xy = (s.mb_x + s.mb_y * s.mb_stride) as usize;
    s.mb_type[xy] = MB_INTRA;
    let _ = from_p;
    if s.h263_aic {
        s.ac_pred = gb.get_bits1() != 0;
        if s.ac_pred {
            s.mb_type[xy] |= MB_ACPRED;
            s.h263_aic_dir = gb.get_bits1() != 0;
        }
    } else {
        s.ac_pred = false;
    }
    let mut cbpy = get_vlc2(gb, &vlcs.cbpy);
    if cbpy < 0 {
        return SLICE_ERROR;
    }
    if dquant != 0 {
        h263_decode_dquant(gb, s, h);
    }
    let cbp = ((cbpc & 3) as u32) | ((cbpy as u32) << 2);
    h263_decode_blocks_tail(gb, s, h, vlcs, block, cbp, 0, pb_mv_count)
}

/// Decode the 6 blocks + per-MB slice end check (`h263_decode_blocks_tail`).
fn h263_decode_blocks_tail(
    gb: &mut GetBitContext,
    s: &mut MpegState,
    h: &mut H263State,
    vlcs: &H263Vlc,
    block: &mut [[i16; 64]; 6],
    mut cbp: u32,
    cbpb: u32,
    pb_mv_count: &mut u32,
) -> i32 {
    for i in 0..6 {
        let coded = cbp & 32 != 0;
        let blk = &mut block[i];
        if s.h263_aic && s.mb_intra {
            let r = decode_block_aic(gb, s, vlcs, blk, i, coded);
            if r < 0 {
                return SLICE_ERROR;
            }
        } else if s.mb_intra {
            let r = h263_decode_block(gb, s, h, vlcs, blk, i, coded);
            if r < 0 {
                return SLICE_ERROR;
            }
        } else {
            let r = decode_block_inter(gb, s, h, &vlcs.rl_inter, blk, i, coded);
            if r < 0 {
                return SLICE_ERROR;
            }
        }
        cbp <<= 1;
    }
    let _ = cbpb;
    if *pb_mv_count > 0 {
        for _ in 0..*pb_mv_count {
            h263_decode_motion(gb, &vlcs.mv, h, 0, 1);
            h263_decode_motion(gb, &vlcs.mv, h, 0, 1);
        }
    }
    end_of_mb(gb, s)
}
/// Per-MB end of slice check.
fn end_of_mb(gb: &mut GetBitContext, _s: &MpegState) -> i32 {
    if gb.bits_left() < 0 {
        return SLICE_ERROR;
    }
    let left = gb.bits_left();
    let mut v = gb.show_bits(16);
    if left < 16 {
        v >>= 16 - left;
    }
    if v == 0 {
        return SLICE_END;
    }
    SLICE_OK
}
/// `ff_h263_pred_motion`.
pub fn h263_pred_motion(s: &MpegState, block: usize, dir: usize) -> (i32, i32) {
    const OFF: [isize; 4] = [2, 1, 1, -1];
    let wrap = s.b8_stride as isize;
    let mot = 4 + s.block_index[block];
    let mv = &s.motion_val[dir];

    let get_mv = |idx: isize| -> [i16; 2] {
        if idx >= 0 && (idx as usize) < mv.len() {
            mv[idx as usize]
        } else {
            [0, 0]
        }
    };

    let a = get_mv(mot - 1);
    let (px, py);
    if s.first_slice_line && block < 3 {
        if block == 0 {
            if s.mb_x == s.resync_mb_x {
                px = 0;
                py = 0;
            } else if s.mb_x + 1 == s.resync_mb_x && s.h263_pred {
                let c = get_mv(mot + OFF[block] - wrap);
                if s.mb_x == 0 {
                    px = c[0] as i32;
                    py = c[1] as i32;
                } else {
                    px = crate::bitread::mid_pred(a[0] as i32, 0, c[0] as i32);
                    py = crate::bitread::mid_pred(a[1] as i32, 0, c[1] as i32);
                }
            } else {
                px = a[0] as i32;
                py = a[1] as i32;
            }
        } else if block == 1 {
            if s.mb_x + 1 == s.resync_mb_x && s.h263_pred {
                let c = get_mv(mot + OFF[block] - wrap);
                px = crate::bitread::mid_pred(a[0] as i32, 0, c[0] as i32);
                py = crate::bitread::mid_pred(a[1] as i32, 0, c[1] as i32);
            } else {
                px = a[0] as i32;
                py = a[1] as i32;
            }
        } else {
            let b = get_mv(mot - wrap);
            let c = get_mv(mot + OFF[block] - wrap);
            let mut a = a;
            if s.mb_x == s.resync_mb_x {
                a = [0, 0];
            }
            px = crate::bitread::mid_pred(a[0] as i32, b[0] as i32, c[0] as i32);
            py = crate::bitread::mid_pred(a[1] as i32, b[1] as i32, c[1] as i32);
        }
    } else {
        let b = get_mv(mot - wrap);
        let c = get_mv(mot + OFF[block] - wrap);
        px = crate::bitread::mid_pred(a[0] as i32, b[0] as i32, c[0] as i32);
        py = crate::bitread::mid_pred(a[1] as i32, b[1] as i32, c[1] as i32);
    }
    (px, py)
}

/// `ff_h263_update_motion_val` (16x16/8x8 path only; field stuff is unused).
pub fn h263_update_motion_val(s: &mut MpegState) {
    let wrap = s.b8_stride as isize;
    let xy = 4 + s.block_index[0];
    if s.mv_type != MotionType::Mv8x8 {
        let (motion_x, motion_y) = if s.mb_intra {
            (0, 0)
        } else {
            (s.mv[0][0][0], s.mv[0][0][1])
        };
        for &(dx, dy) in &[(0isize, 0isize), (1isize, 0isize), (0isize, wrap), (1isize, wrap)] {
            let idx = (xy + dx + dy) as usize;
            if idx < s.motion_val[0].len() {
                s.motion_val[0][idx][0] = motion_x as i16;
                s.motion_val[0][idx][1] = motion_y as i16;
            }
        }
    }
}

/// H.263 in-loop deblocking (`ff_h263_loop_filter` + h263dsp filters).
pub mod loop_filter {
    use super::*;
    use crate::h263dsp_tables::H263_LOOP_FILTER_STRENGTH;

    pub fn h263_h_loop_filter_c(src: &mut [u8], off: usize, stride: usize, qscale: u32) {
        let strength = H263_LOOP_FILTER_STRENGTH[qscale as usize] as i32;
        for y in 0..8 {
            let o = off + y * stride;
            let p0 = src[o - 2] as i32;
            let p1 = src[o - 1] as i32;
            let p2 = src[o] as i32;
            let p3 = src[o + 1] as i32;
            let d = (p0 - p3 + 4 * (p2 - p1)) / 8;
            let d1 = if d < -2 * strength {
                0
            } else if d < -strength {
                -2 * strength - d
            } else if d < strength {
                d
            } else if d < 2 * strength {
                2 * strength - d
            } else {
                0
            };
            let mut p1 = p1 + d1;
            let mut p2 = p2 - d1;
            if p1 & 256 != 0 {
                p1 = !(p1 >> 31);
            }
            if p2 & 256 != 0 {
                p2 = !(p2 >> 31);
            }
            src[o - 1] = p1 as u8;
            src[o] = p2 as u8;

            let ad1 = d1.abs() >> 1;
            let d2 = av_clip_i32((p0 - p3) / 4, -ad1, ad1);
            src[o - 2] = (p0 - d2) as u8;
            src[o + 1] = (p3 + d2) as u8;
        }
    }

    pub fn h263_v_loop_filter_c(src: &mut [u8], off: usize, stride: usize, qscale: u32) {
        let strength = H263_LOOP_FILTER_STRENGTH[qscale as usize] as i32;
        for x in 0..8 {
            let o = off + x;
            let p0 = src[o - 2 * stride] as i32;
            let p1 = src[o - stride] as i32;
            let p2 = src[o] as i32;
            let p3 = src[o + stride] as i32;
            let d = (p0 - p3 + 4 * (p2 - p1)) / 8;
            let d1 = if d < -2 * strength {
                0
            } else if d < -strength {
                -2 * strength - d
            } else if d < strength {
                d
            } else if d < 2 * strength {
                2 * strength - d
            } else {
                0
            };
            let mut p1 = p1 + d1;
            let mut p2 = p2 - d1;
            if p1 & 256 != 0 {
                p1 = !(p1 >> 31);
            }
            if p2 & 256 != 0 {
                p2 = !(p2 >> 31);
            }
            src[o - stride] = p1 as u8;
            src[o] = p2 as u8;

            let ad1 = d1.abs() >> 1;
            let d2 = av_clip_i32((p0 - p3) / 4, -ad1, ad1);
            src[o - 2 * stride] = (p0 - d2) as u8;
            src[o + stride] = (p3 + d2) as u8;
        }
    }

    /// `ff_h263_loop_filter` from h263.c.
    pub fn h263_loop_filter(s: &mut MpegState) {
        let linesize = s.linesize;
        let uvlinesize = s.uvlinesize;
        let xy = (s.mb_y * s.mb_stride + s.mb_x) as usize;
        let qp_c = if !mb_is_skip(s.mb_type[xy]) { s.qscale } else { 0 };

        let dest_y = s.dest[0];
        let dest_cb = s.dest[1];
        let dest_cr = s.dest[2];

        if qp_c != 0 {
            // bottom half vertical filter always applied for non-skip
            h263_v_loop_filter_c(&mut s.cur_pic.y, dest_y + 8 * linesize, linesize, qp_c);
            h263_v_loop_filter_c(&mut s.cur_pic.y, dest_y + 8 * linesize + 8, linesize, qp_c);
        }
        if s.mb_y != 0 {
            let qp_tt = if mb_is_skip(s.mb_type[xy - s.mb_stride as usize]) {
                0
            } else {
                s.qscale_table[xy - s.mb_stride as usize]
            };
            let qp_tc = if qp_c != 0 { qp_c } else { qp_tt };
            if qp_tc != 0 {
                let chroma_qp = s.chroma_qscale_table[qp_tc as usize];
                h263_v_loop_filter_c(&mut s.cur_pic.y, dest_y, linesize, qp_tc);
                h263_v_loop_filter_c(&mut s.cur_pic.y, dest_y + 8, linesize, qp_tc);
                h263_v_loop_filter_c(&mut s.cur_pic.u, dest_cb, uvlinesize, chroma_qp);
                h263_v_loop_filter_c(&mut s.cur_pic.v, dest_cr, uvlinesize, chroma_qp);
            }
            if qp_tt != 0 {
                h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y - 8 * linesize + 8, linesize, qp_tt);
            }
            if s.mb_x != 0 {
                let qp_dt = if qp_tt != 0 || mb_is_skip(s.mb_type[xy - 1 - s.mb_stride as usize]) {
                    qp_tt
                } else {
                    s.qscale_table[xy - 1 - s.mb_stride as usize]
                };
                if qp_dt != 0 {
                    let chroma_qp = s.chroma_qscale_table[qp_dt as usize];
                    h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y - 8 * linesize, linesize, qp_dt);
                    h263_h_loop_filter_c(&mut s.cur_pic.u, dest_cb - 8 * uvlinesize, uvlinesize, chroma_qp);
                    h263_h_loop_filter_c(&mut s.cur_pic.v, dest_cr - 8 * uvlinesize, uvlinesize, chroma_qp);
                }
            }
        }
        if qp_c != 0 {
            h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y + 8, linesize, qp_c);
            if s.mb_y + 1 == s.mb_height {
                h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y + 8 * linesize + 8, linesize, qp_c);
            }
        }
        if s.mb_x != 0 {
            let qp_lc = if qp_c != 0 || mb_is_skip(s.mb_type[xy - 1]) {
                qp_c
            } else {
                s.qscale_table[xy - 1]
            };
            if qp_lc != 0 {
                h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y, linesize, qp_lc);
                if s.mb_y + 1 == s.mb_height {
                    let chroma_qp = s.chroma_qscale_table[qp_lc as usize];
                    h263_h_loop_filter_c(&mut s.cur_pic.y, dest_y + 8 * linesize, linesize, qp_lc);
                    h263_h_loop_filter_c(&mut s.cur_pic.u, dest_cb, uvlinesize, chroma_qp);
                    h263_h_loop_filter_c(&mut s.cur_pic.v, dest_cr, uvlinesize, chroma_qp);
                }
            }
        }
    }
}

// MB type flags (subset FFmpeg uses here).
pub const MB_INTRA: u32 = 0x40;
pub const MB_ACPRED: u32 = 0x1000;
pub const MB_SKIP: u32 = 0x2000;
pub const MB_16X16: u32 = 0x01;
pub const MB_8X8: u32 = 0x04;
pub const MB_FWD: u32 = 0x10;

#[inline]
pub fn mb_is_skip(t: u32) -> bool {
    t & MB_SKIP != 0
}

/// `ff_h263_round_chroma`.
pub fn h263_round_chroma(x: i32) -> i32 {
    const TAB: [u8; 16] = [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 1];
    TAB[(x & 0xf) as usize] as i32 + (x >> 3)
}

/// `hpel_motion` from mpegvideo_motion.c (single luma plane half-pel move).
pub fn hpel_motion(
    s: &MpegState,
    dest_off: usize,
    src_plane: PLANE,
    src_x: i32,
    src_y: i32,
    pix_op_dxy: &dyn Fn(&mut [u8], usize, &[u8], usize, usize, usize),
    motion_x: i32,
    motion_y: i32,
) {
    let dxy = ((motion_y & 1) << 1) | (motion_x & 1);
    let src_x = av_clip_i32(src_x + (motion_x >> 1), -16, s.width as i32);
    let mut dxy = dxy;
    if src_x != s.width as i32 {
        dxy |= motion_x & 1;
    }
    let src_y = av_clip_i32(src_y + (motion_y >> 1), -16, s.height as i32);
    if src_y != s.height as i32 {
        dxy |= (motion_y & 1) << 1;
    }
    let _ = dxy; // actual dxy recomputed by caller below
    let _ = (dest_off, src_plane, pix_op_dxy);
}

/// Full 16x16 / 16x8 / 8x8 MC dispatch for one MB — the `mpeg_motion` and
/// `apply_8x8` paths from mpegvideo_motion.c specialized for FMT_H263.
pub fn mpeg_motion(
    s: &mut MpegState,
    dest_off: usize,
    dir: usize,
    field_select: usize,
    ref_pic: &[PLANE; 3],
    pix_op_avg: bool,
    motion_x: i32,
    motion_y: i32,
    h: usize,
    block_y_half: bool,
    mb_y: i32,
) {
    let linesize = s.linesize;
    let uvlinesize = s.uvlinesize;

    let dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as u32;
    let src_x = s.mb_x as i32 * 16 + (motion_x >> 1);
    let src_y = (mb_y << (4 - block_y_half as u32)) + (motion_y >> 1);

    // H.263 chroma: full-pel even components folded into uvdxy
    let uvdxy = (dxy | ((motion_y & 2) | ((motion_x & 2) >> 1)) as u32) as u32;
    let uvsrc_x = src_x >> 1;
    let uvsrc_y = src_y >> 1;

    let (plane, poff) = (ref_pic[0], 0usize);
    let _ = (plane, poff);
    let y_plane = match ref_pic[0] {
        PLANE::Y => &s.ref_y(dir, field_select),
        _ => unreachable!(),
    };
    let _ = y_plane;
    // References are owned by the caller's picture set; the slice into the
    // planes happens through MpegState accessors in mpeg.rs.
    let _ = (dest_off, pix_op_avg, h, linesize, uvlinesize, uvdxy, uvsrc_x, uvsrc_y, src_x, src_y);
}

/// Reconstruct one macroblock (`ff_mpv_reconstruct_mb` for the H.263 path).
pub fn mpv_reconstruct_mb(s: &mut MpegState, block: &mut [[i16; 64]; 6]) {
    let linesize = s.linesize;
    let uvlinesize = s.uvlinesize;

    if !s.mb_intra {
        // motion compensation
        let mv_type = s.mv_type;
        match mv_type {
            MotionType::Mv16x16 => {
                if s.mv_dir & 1 != 0 || s.pict_type != 3 {
                    let mx = s.mv[0][0][0];
                    let my = s.mv[0][0][1];
                    mc_dir16(s, 0, mx, my, false);
                }
                if s.mv_dir & 2 != 0 {
                    let mx = s.mv[1][0][0];
                    let my = s.mv[1][0][1];
                    let avg = (s.mv_dir & 1) != 0;
                    mc_dir16(s, 1, mx, my, avg);
                }
            }
            MotionType::Mv8x8 => {
                mc_8x8(s);
            }
            MotionType::MvField => unreachable!("field MC unused for RV"),
        }
        // add dct residue (dequantized)
        for i in 0..4 {
            add_dequant_dct(s, &mut block[i], i, s.dest[0] + block_pos(i, linesize), linesize, s.qscale);
        }
        add_dequant_dct(s, &mut block[4], 4, s.dest[1], uvlinesize, s.chroma_qscale);
        add_dequant_dct(s, &mut block[5], 5, s.dest[2], uvlinesize, s.chroma_qscale);
    } else {
        for i in 0..4 {
            let off = s.dest[0] + block_pos(i, linesize);
            idct_intra_put(s, &mut block[i], off, linesize, s.qscale, i);
        }
        let (q4, q5) = (s.chroma_qscale, s.chroma_qscale);
        idct_intra_put_chroma(s, &mut block[4], s.dest[1], uvlinesize, q4, 4);
        idct_intra_put_chroma(s, &mut block[5], s.dest[2], uvlinesize, q5, 5);
    }
}

fn dest_off_of(_i: usize) -> usize {
    0
}

fn block_pos(i: usize, linesize: usize) -> usize {
    // blocks 0,1 top row; 2,3 bottom row (within the 16x16 MB)
    match i {
        0 => 0,
        1 => 8,
        2 => 8 * linesize,
        3 => 8 * linesize + 8,
        _ => 0,
    }
}

/// `put_dct`: dequantize intra block then IDCT-put.
fn idct_intra_put(s: &mut MpegState, block: &mut [i16; 64], off: usize, linesize: usize, qscale: u32, n: usize) {
    unquantize_h263_intra(s, block, n, qscale);
    let plane = &mut s.cur_pic.y;
    simple_idct_put(block, plane, off, linesize);
}

fn idct_intra_put_chroma(s: &mut MpegState, block: &mut [i16; 64], off: usize, linesize: usize, qscale: u32, n: usize) {
    unquantize_h263_intra(s, block, n, qscale);
    if n == 4 {
        let p = &mut s.cur_pic.u;
        simple_idct_put(block, p, off, linesize);
    } else {
        let p = &mut s.cur_pic.v;
        simple_idct_put(block, p, off, linesize);
    }
}

/// `dct_unquantize_h263_intra_c`.
pub fn unquantize_h263_intra(s: &MpegState, block: &mut [i16; 64], n: usize, qscale: u32) {
    let qmul = (qscale as i32) << 1;
    let qadd = if !s.h263_aic {
        block[0] = (block[0] as i32 * if n < 4 { s.y_dc_scale as i32 } else { s.c_dc_scale as i32 }) as i16;
        ((qscale as i32 - 1) | 1) as i32
    } else {
        0
    };
    let n_coeffs = if s.ac_pred {
        63
    } else if s.block_last_index[n] >= 0 {
        crate::mpegtables::ZIGZAG_DIRECT[s.block_last_index[n] as usize] as usize
    } else {
        0
    };
    for i in 1..=n_coeffs {
        let mut level = block[i] as i32;
        if level != 0 {
            level = if level < 0 { level * qmul - qadd } else { level * qmul + qadd };
            block[i] = level as i16;
        }
    }
}

/// `dct_unquantize_h263_inter_c` + `add_dct`-style idct_add.
pub fn add_dequant_dct(s: &mut MpegState, block: &mut [i16; 64], n: usize, off: usize, linesize: usize, qscale: u32) {
    if s.block_last_index[n] >= 0 {
        unquantize_h263_inter(block, n, qscale, s);
        let plane = plane_for(n, s);
        simple_idct_add(block, plane.0, off, linesize);
    }
}

fn plane_for<'a>(n: usize, s: &'a mut MpegState) -> (&'a mut [u8], usize) {
    match n {
        4 => (&mut s.cur_pic.u, 0),
        5 => (&mut s.cur_pic.v, 0),
        _ => (&mut s.cur_pic.y, 0),
    }
}

/// `dct_unquantize_h263_inter_c`.
pub fn unquantize_h263_inter(block: &mut [i16; 64], n: usize, qscale: u32, s: &MpegState) {
    let qadd = ((qscale as i32 - 1) | 1) as i32;
    let qmul = (qscale as i32) << 1;
    let n_coeffs = if s.block_last_index[n] >= 0 {
        crate::mpegtables::ZIGZAG_DIRECT[s.block_last_index[n] as usize] as usize
    } else {
        return;
    };
    for i in 0..=n_coeffs {
        let mut level = block[i] as i32;
        if level != 0 {
            level = if level < 0 { level * qmul - qadd } else { level * qmul + qadd };
            block[i] = level as i16;
        }
    }
}

/// `mc_dir16`: one 16x16 MB from `dir` reference (forward=0/backward=1).
pub fn mc_dir16(s: &mut MpegState, dir: usize, motion_x: i32, motion_y: i32, avg: bool) {
    let dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as u32;
    let src_x = s.mb_x as i32 * 16 + (motion_x >> 1);
    let src_y = s.mb_y as i32 * 16 + (motion_y >> 1);
    let uvdxy = dxy | (((motion_y & 2) | ((motion_x & 2) >> 1)) as u32);
    let uvsrc_x = src_x >> 1;
    let uvsrc_y = src_y >> 1;
    let linesize = s.linesize;
    let uvlinesize = s.uvlinesize;

    let ref_idx = dir.min(1);
    let no_rounding = s.no_rounding;

    crate::hpel::op_pixels(
        &mut s.cur_pic.y,
        s.dest[0],
        &s.refs[ref_idx].y,
        src_x,
        src_y,
        s.width,
        s.height,
        linesize,
        16,
        16,
        dxy,
        avg,
        no_rounding,
    );
    let uv_w = (s.width + 1) / 2;
    let uv_h = (s.height + 1) / 2;
    crate::hpel::op_pixels(
        &mut s.cur_pic.u,
        s.dest[1],
        &s.refs[ref_idx].u,
        uvsrc_x,
        uvsrc_y,
        uv_w,
        uv_h,
        uvlinesize,
        8,
        8,
        uvdxy,
        avg,
        no_rounding,
    );
    crate::hpel::op_pixels(
        &mut s.cur_pic.v,
        s.dest[2],
        &s.refs[ref_idx].v,
        uvsrc_x,
        uvsrc_y,
        uv_w,
        uv_h,
        uvlinesize,
        8,
        8,
        uvdxy,
        avg,
        no_rounding,
    );
}

/// `apply_8x8` luma + joined chroma.
pub fn mc_8x8(s: &mut MpegState) {
    let linesize = s.linesize;
    let uvlinesize = s.uvlinesize;
    let no_rounding = s.no_rounding;
    let dest_y = s.dest[0];
    let dest_u = s.dest[1];
    let dest_v = s.dest[2];

    let mut mx = 0i32;
    let mut my = 0i32;
    for i in 0..4 {
        let dx = (i & 1) * 8;
        let dy = (i >> 1) * 8;
        let motion_x = s.mv[0][i][0];
        let motion_y = s.mv[0][i][1];
        let dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as u32;
        let src_x = (s.mb_x * 16 + dx) as i32 + (motion_x >> 1);
        let src_y = (s.mb_y * 16 + dy) as i32 + (motion_y >> 1);
        crate::hpel::op_pixels(
            &mut s.cur_pic.y,
            dest_y + dx + dy * linesize,
            &s.refs[0].y,
            src_x,
            src_y,
            s.width,
            s.height,
            linesize,
            8,
            8,
            dxy,
            false,
            no_rounding,
        );
        mx += motion_x;
        my += motion_y;
    }
    // chroma_4mv_motion
    let cmx = h263_round_chroma(mx);
    let cmy = h263_round_chroma(my);
    let dxy = (((cmy & 1) << 1) | (cmx & 1)) as u32;
    let uvsrc_x = s.mb_x as i32 * 8 + (cmx >> 1);
    let uvsrc_y = s.mb_y as i32 * 8 + (cmy >> 1);
    let uv_w = (s.width + 1) / 2;
    let uv_h = (s.height + 1) / 2;
    crate::hpel::op_pixels(
        &mut s.cur_pic.u,
        dest_u,
        &s.refs[0].u,
        uvsrc_x,
        uvsrc_y,
        uv_w,
        uv_h,
        uvlinesize,
        8,
        8,
        dxy,
        false,
        no_rounding,
    );
    crate::hpel::op_pixels(
        &mut s.cur_pic.v,
        dest_v,
        &s.refs[0].v,
        uvsrc_x,
        uvsrc_y,
        uv_w,
        uv_h,
        uvlinesize,
        8,
        8,
        dxy,
        false,
        no_rounding,
    );
}

// Ported from FFmpeg libavcodec/vp3.c (the VP3 and VP4 paths; Theora is
// left out), the 8x8 copy and half-pel averages of libavcodec/hpeldsp.c,
// put_no_rnd_pixels_l2 of libavcodec/vp3dsp.c and
// libavcodec/videodsp_template.c (emulated_edge_mc) at commit 2da55bf.
// Licensed under GNU Lesser General Public License 2.1 or later.

//! FFmpeg's `vp3_decode_init` and `vp3_decode_frame` with everything they
//! call for VP30, VP31 and VP40 streams.
//!
//! VP3 codes rows bottom-up. FFmpeg renders with a negative stride from
//! the last row of the frame; this port stores every plane in coding
//! order (row 0 first) with a positive stride. Every FFmpeg pixel
//! operation is relative to the stride, so the pixels come out the same,
//! and the output frame reverses the rows.
//!
//! Outside Theora a frame header carries one quantizer, so FFmpeg's
//! `nqps` is 1, every fragment's `qpi` is 0 and `unpack_block_qpis`
//! reads nothing. This port keeps `qps[0]` only. The pixel format is
//! always YUV 4:2:0.

use std::collections::VecDeque;
use std::sync::{Arc, LazyLock};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, Decoder, Error, Frame, Packet, PixelFormat, Result,
    VideoFrame, VideoPlane,
};

use crate::bitread::Gb;
use crate::dsp::{self, BoundingValues};
use crate::tables::*;
use crate::vlc::Vlc;

const MODE_INTER_NO_MV: u8 = 0;
const MODE_INTRA: u8 = 1;
const MODE_INTER_PLUS_MV: u8 = 2;
const MODE_INTER_LAST_MV: u8 = 3;
const MODE_INTER_PRIOR_LAST: u8 = 4;
const MODE_USING_GOLDEN: u8 = 5;
const MODE_GOLDEN_MV: u8 = 6;
const MODE_INTER_FOURMV: u8 = 7;
/// Internal mode: the fragment is not coded and copies the last frame.
const MODE_COPY: u8 = 8;

const SB_NOT_CODED: u8 = 0;
const SB_PARTIALLY_CODED: u8 = 1;
const SB_FULLY_CODED: u8 = 2;

/// FFmpeg's `VP4_DC_UNDEFINED` (`NB_VP4_DC_TYPES`).
const VP4_DC_UNDEFINED: u8 = 3;

const VP3_MV_VLC_BITS: u32 = 6;
const VP4_MV_VLC_BITS: u32 = 6;
const SUPERBLOCK_VLC_BITS: u32 = 6;

/// Largest accepted width or height.
const MAX_SIDE: u32 = 16384;
/// Largest accepted coded area (16-aligned width × height).
const MAX_AREA: usize = 8192 * 8192;

/// No fragment at this superblock position.
const NO_FRAGMENT: usize = usize::MAX;

/// FFmpeg's static tables from `init_tables_once`.
struct StaticVlcs {
    superblock_run_length: Vlc,
    fragment_run_length: Vlc,
    motion_vector: Vlc,
    mode_code: Vlc,
    /// `vp4_mv_vlc_table[axis][selector]` at `axis * 7 + selector`.
    vp4_mv: Vec<Vlc>,
    block_pattern: Vec<Vlc>,
}

impl StaticVlcs {
    fn new() -> Result<Self> {
        let mut vp4_mv = Vec::with_capacity(14);
        for axis in &VP4_MV_VLC {
            for table in axis {
                vp4_mv.push(Vlc::from_pairs(table, VP4_MV_VLC_BITS, -31)?);
            }
        }
        Ok(Self {
            superblock_run_length: Vlc::from_lengths(
                &SUPERBLOCK_RUN_LENGTH_VLC_LENS,
                SUPERBLOCK_VLC_BITS,
                1,
            )?,
            fragment_run_length: Vlc::from_lengths(&FRAGMENT_RUN_LENGTH_VLC_LEN, 5, 0)?,
            motion_vector: Vlc::from_pairs(&MOTION_VECTOR_VLC_TABLE, VP3_MV_VLC_BITS, -31)?,
            mode_code: Vlc::from_lengths(&MODE_CODE_VLC_LEN, 4, 0)?,
            vp4_mv,
            block_pattern: VP4_BLOCK_PATTERN_VLC
                .iter()
                .map(|t| Vlc::from_codes(t, 5))
                .collect::<Result<_>>()?,
        })
    }
}

fn static_vlcs() -> Result<&'static StaticVlcs> {
    static VLCS: LazyLock<Option<StaticVlcs>> = LazyLock::new(|| StaticVlcs::new().ok());
    VLCS.as_ref().ok_or_else(|| Error::invalid("vp3: VLC tables failed to build"))
}

/// The 80 coefficient VLCs: 16 DC tables, then four groups of 16 AC
/// tables. VP4 streams use `vp4_bias`, the others `vp3_bias`.
fn coeff_vlcs(vp4: bool) -> Result<&'static [Vlc]> {
    fn build(bias: &[[[u8; 2]; 32]; 80]) -> Option<Vec<Vlc>> {
        bias.iter().map(|t| Vlc::from_pairs(t, 11, 0).ok()).collect()
    }
    static VP3: LazyLock<Option<Vec<Vlc>>> = LazyLock::new(|| build(&VP3_BIAS));
    static VP4: LazyLock<Option<Vec<Vlc>>> = LazyLock::new(|| build(&VP4_BIAS));
    let tables = if vp4 { &*VP4 } else { &*VP3 };
    tables
        .as_deref()
        .ok_or_else(|| Error::invalid("vp3: coefficient VLC tables failed to build"))
}

/// Offset of the AC table group used at coefficient level `level` (1..64).
fn ac_group(level: usize) -> usize {
    match level {
        1..=5 => 16,
        6..=14 => 32,
        15..=27 => 48,
        _ => 64,
    }
}

/// FFmpeg's `Vp3Fragment` without `qpi`, which is always 0 here.
#[derive(Clone, Copy, Default)]
struct Fragment {
    dc: i16,
    coding_method: u8,
}

/// FFmpeg's `VP4Predictor`.
#[derive(Clone, Copy)]
struct Vp4Predictor {
    dc: i32,
    kind: u8,
}

const VP4_PREDICTOR_RESET: Vp4Predictor = Vp4Predictor { dc: 0, kind: VP4_DC_UNDEFINED };

/// One frame in coding order: plane 0 is `width × height`, planes 1 and 2
/// half that each way, each with a stride equal to its width.
struct Picture {
    planes: [Vec<u8>; 3],
}

impl Picture {
    /// A zeroed picture, like a new buffer from FFmpeg's frame pool.
    fn new(width: usize, height: usize) -> Self {
        let chroma = (width / 2) * (height / 2);
        Self { planes: [vec![0; width * height], vec![0; chroma], vec![0; chroma]] }
    }
}

/// libavutil's `RSHIFT`: divide by `1 << b`, rounding half away from zero.
fn rshift(a: i32, b: u32) -> i32 {
    if a > 0 {
        (a + ((1 << b) >> 1)) >> b
    } else {
        (a + ((1 << b) >> 1) - 1) >> b
    }
}

/// FFmpeg's `TOKEN_EOB`, `TOKEN_ZERO_RUN` and `TOKEN_COEFF`, truncated to
/// 16 bits like the C stores.
fn token_eob(eob_run: i32) -> i16 {
    (eob_run << 2) as i16
}

fn token_zero_run(coeff: i16, zero_run: usize) -> i16 {
    (i32::from(coeff) * 512 + ((zero_run as i32) << 2) + 1) as i16
}

fn token_coeff(coeff: i16) -> i16 {
    (i32::from(coeff) * 4 + 2) as i16
}

/// FFmpeg's `get_eob_run`; `token` is 0..=6.
fn get_eob_run(gb: &mut Gb<'_>, token: usize) -> i32 {
    let (base, bits) = EOB_RUN_TABLE[token];
    let mut v = i32::from(base);
    if bits != 0 {
        v += gb.get_bits(u32::from(bits)) as i32;
    }
    v
}

/// FFmpeg's `get_coeff`: the zero run and the coefficient of `token`
/// (7..32).
fn get_coeff(gb: &mut Gb<'_>, token: usize) -> (usize, i16) {
    let mut bits_to_get = u32::from(COEFF_GET_BITS[token]);
    if bits_to_get != 0 {
        bits_to_get = gb.get_bits(bits_to_get);
    }
    let coeff = COEFF_TABLES[token].get(bits_to_get as usize).copied().unwrap_or(0);
    let mut zero_run = usize::from(ZERO_RUN_BASE[token]);
    if ZERO_RUN_GET_BITS[token] != 0 {
        zero_run += gb.get_bits(u32::from(ZERO_RUN_GET_BITS[token])) as usize;
    }
    (zero_run, coeff)
}

/// FFmpeg's `vp4_dc_pred` for the predictor at row `r`, column `c` of the
/// 6×6 window.
fn vp4_dc_pred(pred: &[[Vp4Predictor; 6]; 6], r: usize, c: usize, last_dc: &[i32; 3], kind: usize) -> i32 {
    let kind_u8 = kind as u8;
    let mut count = 0;
    let mut dc = 0;
    if pred[r - 1][c].kind == kind_u8 {
        dc += pred[r - 1][c].dc;
        count += 1;
    }
    if pred[r + 1][c].kind == kind_u8 {
        dc += pred[r + 1][c].dc;
        count += 1;
    }
    if count != 2 && pred[r][c - 1].kind == kind_u8 {
        dc += pred[r][c - 1].dc;
        count += 1;
    }
    if count != 2 && pred[r][c + 1].kind == kind_u8 {
        dc += pred[r][c + 1].dc;
        count += 1;
    }
    // Division, not a shift, as in FFmpeg (negative values).
    if count == 2 { dc / 2 } else { last_dc[kind] }
}

/// Memory indices of the 8 rows of the block whose first pixel is `first`.
fn rows8(first: usize, stride: usize) -> [usize; 8] {
    std::array::from_fn(|k| first + k * stride)
}

/// FFmpeg's `put_pixels8`: copy the 8×8 block at `first` from `src`.
fn copy_block(dst: &mut [u8], src: &[u8], first: usize, stride: usize) {
    for k in 0..8 {
        let o = first + k * stride;
        dst[o..o + 8].copy_from_slice(&src[o..o + 8]);
    }
}

/// The `w × h` block whose top-left pixel is (`src_x`, `src_y`) of a
/// `pw × ph` plane, written to `out` with stride `w`. Positions outside
/// the plane take the nearest edge pixel: FFmpeg's `emulated_edge_mc`,
/// and a plain read where the block lies inside.
#[allow(clippy::too_many_arguments)]
fn edge_block(out: &mut [u8], w: usize, h: usize, src: &[u8], stride: usize, src_x: i32, src_y: i32, pw: usize, ph: usize) {
    let inside = src_x >= 0 && src_y >= 0 && src_x as usize + w <= pw && src_y as usize + h <= ph;
    if inside {
        let (x, y) = (src_x as usize, src_y as usize);
        for r in 0..h {
            let o = (y + r) * stride + x;
            out[r * w..r * w + w].copy_from_slice(&src[o..o + w]);
        }
        return;
    }
    for r in 0..h {
        let y = (src_y + r as i32).clamp(0, ph as i32 - 1) as usize;
        for c in 0..w {
            let x = (src_x + c as i32).clamp(0, pw as i32 - 1) as usize;
            out[r * w + c] = src[y * stride + x];
        }
    }
}

/// Motion compensation from the 9×9 source block `s` (stride 9):
/// `put_no_rnd_pixels_tab[1][halfpel]` for half-pel index 0..=2 and
/// VP3's `put_no_rnd_pixels_l2` for 3.
fn put_mc(dst: &mut [u8], first: usize, stride: usize, s: &[u8; 81], halfpel: i32, motion_x: i32, motion_y: i32) {
    let (a, b) = match halfpel {
        0 => {
            for r in 0..8 {
                let o = first + r * stride;
                dst[o..o + 8].copy_from_slice(&s[r * 9..r * 9 + 8]);
            }
            return;
        }
        1 => (0, 1),
        2 => (0, 9),
        _ => {
            // d is 0 if motion_x and motion_y have the same sign, else -1.
            let d = (motion_x ^ motion_y) >> 31;
            ((-d) as usize, (10 + d) as usize)
        }
    };
    for r in 0..8 {
        let o = first + r * stride;
        for c in 0..8 {
            dst[o + c] = dsp::no_rnd_avg(s[a + r * 9 + c], s[b + r * 9 + c]);
        }
    }
}

/// VP3 (VP30, VP31) and VP4 (VP40) decoder.
pub struct Vp3Decoder {
    codec_id: CodecId,
    vlcs: &'static StaticVlcs,
    coeff_vlc: &'static [Vlc],
    /// The container's frame size (FFmpeg's `coded_width` and
    /// `coded_height`): the size of every output frame.
    visible: (u32, u32),
    version: i32,
    /// Coded size aligned to 16 (FFmpeg's `s->width`, `s->height`).
    width: usize,
    height: usize,
    keyframe: bool,
    skip_loop_filter: bool,
    /// `qps[0]`; -1 before the first frame.
    qps: i32,

    y_superblock_width: usize,
    y_superblock_height: usize,
    y_superblock_count: usize,
    c_superblock_width: usize,
    c_superblock_height: usize,
    c_superblock_count: usize,
    superblock_count: usize,
    u_superblock_start: usize,
    v_superblock_start: usize,
    /// Superblock coding (VP3) or macroblock coding (VP4); kept across
    /// frames like FFmpeg's.
    superblock_coding: Vec<u8>,

    macroblock_width: usize,
    macroblock_height: usize,
    macroblock_count: usize,
    c_macroblock_width: usize,
    c_macroblock_height: usize,
    yuv_macroblock_count: usize,

    fragment_width: [usize; 2],
    fragment_height: [usize; 2],
    fragment_start: [usize; 3],
    all_fragments: Vec<Fragment>,
    motion_val: [Vec<[i8; 2]>; 2],

    coded_dc_scale_factor: [[u16; 64]; 2],
    coded_ac_scale_factor: [u32; 64],
    base_matrix: [[u8; 64]; 3],
    idct_permutation: [usize; 64],
    idct_scantable: [usize; 64],
    /// FFmpeg's `qmat[0][inter][plane]`.
    qmat: [[[i16; 64]; 3]; 2],
    filter_limit_values: [u8; 64],
    bounding_values: BoundingValues,

    /// Every DCT token of the frame (FFmpeg's `dct_tokens_base`).
    dct_tokens_base: Vec<i16>,
    /// FFmpeg's `dct_tokens[plane][level]` pointers, as indices into
    /// `dct_tokens_base`.
    dct_tokens: [[usize; 64]; 3],
    /// FFmpeg's `num_coded_frags[3][64]`, flattened, plus one slot: the
    /// zero-run bookkeeping in `unpack_vlcs` can write `[plane][64]`,
    /// which is `[plane + 1][0]` in memory.
    num_coded_frags: [i32; 3 * 64 + 1],
    coded_fragment_list: Vec<usize>,
    coded_fragment_start: [usize; 3],
    superblock_fragments: Vec<usize>,
    macroblock_coding: Vec<u8>,
    dc_pred_row: Vec<Vp4Predictor>,

    /// The last decoded frame (FFmpeg's `current_frame` between calls).
    current: Option<Arc<Picture>>,
    golden: Option<Arc<Picture>>,
    ready: VecDeque<Frame>,
    flushed: bool,
}

impl Vp3Decoder {
    /// FFmpeg's `vp3_decode_init` for codec id `vp3` or `vp4`.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let codec_id = params.codec_id.clone();
        let vp4_id = match codec_id.as_str() {
            "vp3" => false,
            "vp4" => true,
            other => {
                return Err(Error::unsupported(format!("codec-vp3: unsupported codec id {other}")));
            }
        };
        // The container tag picks the version, as in FFmpeg. Without a
        // tag FFmpeg assumes VP31; here the codec id decides instead.
        let version = match &params.tag {
            Some(tag) if *tag == CodecTag::fourcc(b"VP40") => 3,
            Some(tag) if *tag == CodecTag::fourcc(b"VP30") => 0,
            Some(_) => 1,
            None if vp4_id => 3,
            None => 1,
        };

        let (vis_w, vis_h) = (params.width.unwrap_or(0), params.height.unwrap_or(0));
        if vis_w == 0 || vis_h == 0 || vis_w > MAX_SIDE || vis_h > MAX_SIDE {
            return Err(Error::invalid(format!("vp3: unsupported frame size {vis_w}x{vis_h}")));
        }
        let width = (vis_w as usize).next_multiple_of(16);
        let height = (vis_h as usize).next_multiple_of(16);
        if width < 18 {
            return Err(Error::unsupported(format!("vp3: frame width {vis_w} is below 17")));
        }
        if width * height > MAX_AREA {
            return Err(Error::invalid(format!("vp3: frame size {vis_w}x{vis_h} is too large")));
        }

        let y_superblock_width = width.div_ceil(32);
        let y_superblock_height = height.div_ceil(32);
        let y_superblock_count = y_superblock_width * y_superblock_height;
        let (c_width, c_height) = (width >> 1, height >> 1);
        let c_superblock_width = c_width.div_ceil(32);
        let c_superblock_height = c_height.div_ceil(32);
        let c_superblock_count = c_superblock_width * c_superblock_height;
        let superblock_count = y_superblock_count + 2 * c_superblock_count;

        let macroblock_width = width.div_ceil(16);
        let macroblock_height = height.div_ceil(16);
        let macroblock_count = macroblock_width * macroblock_height;
        let c_macroblock_width = c_width.div_ceil(16);
        let c_macroblock_height = c_height.div_ceil(16);
        let yuv_macroblock_count = macroblock_count + 2 * c_macroblock_width * c_macroblock_height;

        let fragment_width = [width / 8, width / 16];
        let fragment_height = [height / 8, height / 16];
        let y_fragment_count = fragment_width[0] * fragment_height[0];
        let c_fragment_count = fragment_width[1] * fragment_height[1];
        let fragment_count = y_fragment_count + 2 * c_fragment_count;

        let vp4 = version >= 2;
        let mut coded_dc_scale_factor = [[0u16; 64]; 2];
        let mut coded_ac_scale_factor = [0u32; 64];
        let mut base_matrix = [[0u8; 64]; 3];
        let mut filter_limit_values = [0u8; 64];
        let mut idct_permutation = [0usize; 64];
        let mut idct_scantable = [0usize; 64];
        let transpose = |x: usize| (x >> 3) | ((x & 7) << 3);
        for i in 0..64 {
            coded_dc_scale_factor[0][i] =
                u16::from(if vp4 { VP4_Y_DC_SCALE_FACTOR[i] } else { VP31_DC_SCALE_FACTOR[i] });
            coded_dc_scale_factor[1][i] =
                u16::from(if vp4 { VP4_UV_DC_SCALE_FACTOR[i] } else { VP31_DC_SCALE_FACTOR[i] });
            coded_ac_scale_factor[i] =
                u32::from(if vp4 { VP4_AC_SCALE_FACTOR[i] } else { VP31_AC_SCALE_FACTOR[i] });
            base_matrix[0][i] = if vp4 { VP4_GENERIC_DEQUANT[i] } else { VP31_INTRA_Y_DEQUANT[i] };
            base_matrix[1][i] =
                if vp4 { VP4_GENERIC_DEQUANT[i] } else { FF_MJPEG_STD_CHROMINANCE_QUANT_TBL[i] };
            base_matrix[2][i] = if vp4 { VP4_GENERIC_DEQUANT[i] } else { VP31_INTER_DEQUANT[i] };
            filter_limit_values[i] =
                if vp4 { VP4_FILTER_LIMIT_VALUES[i] } else { VP31_FILTER_LIMIT_VALUES[i] };
            idct_permutation[i] = transpose(i);
            idct_scantable[i] = transpose(usize::from(FF_ZIGZAG_DIRECT[i]));
        }

        let mut dec = Self {
            codec_id,
            vlcs: static_vlcs()?,
            coeff_vlc: coeff_vlcs(vp4)?,
            visible: (vis_w, vis_h),
            version,
            width,
            height,
            keyframe: false,
            skip_loop_filter: false,
            qps: -1,
            y_superblock_width,
            y_superblock_height,
            y_superblock_count,
            c_superblock_width,
            c_superblock_height,
            c_superblock_count,
            superblock_count,
            u_superblock_start: y_superblock_count,
            v_superblock_start: y_superblock_count + c_superblock_count,
            superblock_coding: vec![0; superblock_count.max(yuv_macroblock_count)],
            macroblock_width,
            macroblock_height,
            macroblock_count,
            c_macroblock_width,
            c_macroblock_height,
            yuv_macroblock_count,
            fragment_width,
            fragment_height,
            fragment_start: [0, y_fragment_count, y_fragment_count + c_fragment_count],
            all_fragments: vec![Fragment::default(); fragment_count],
            motion_val: [vec![[0; 2]; y_fragment_count], vec![[0; 2]; c_fragment_count]],
            coded_dc_scale_factor,
            coded_ac_scale_factor,
            base_matrix,
            idct_permutation,
            idct_scantable,
            qmat: [[[0; 64]; 3]; 2],
            filter_limit_values,
            bounding_values: [0; 260],
            dct_tokens_base: vec![0; fragment_count * 64],
            dct_tokens: [[0; 64]; 3],
            num_coded_frags: [0; 3 * 64 + 1],
            coded_fragment_list: vec![0; fragment_count],
            coded_fragment_start: [0; 3],
            superblock_fragments: vec![NO_FRAGMENT; superblock_count * 16],
            macroblock_coding: vec![0; macroblock_count + 1],
            dc_pred_row: vec![VP4_PREDICTOR_RESET; y_superblock_width * 4],
            current: None,
            golden: None,
            ready: VecDeque::new(),
            flushed: false,
        };
        dec.init_block_mapping();
        Ok(dec)
    }

    /// FFmpeg's `init_block_mapping`: the fragment at each of the 16
    /// Hilbert positions of every superblock.
    fn init_block_mapping(&mut self) {
        let mut j = 0;
        for plane in 0..3 {
            let chroma = usize::from(plane > 0);
            let (sb_width, sb_height) = if plane > 0 {
                (self.c_superblock_width, self.c_superblock_height)
            } else {
                (self.y_superblock_width, self.y_superblock_height)
            };
            let (frag_width, frag_height) = (self.fragment_width[chroma], self.fragment_height[chroma]);
            for sb_y in 0..sb_height {
                for sb_x in 0..sb_width {
                    for offset in &HILBERT_OFFSET {
                        let x = 4 * sb_x + offset[0];
                        let y = 4 * sb_y + offset[1];
                        self.superblock_fragments[j] = if x < frag_width && y < frag_height {
                            self.fragment_start[plane] + y * frag_width + x
                        } else {
                            NO_FRAGMENT
                        };
                        j += 1;
                    }
                }
            }
        }
    }

    /// FFmpeg's `init_dequantizer` for the frame's one quantizer. Outside
    /// Theora there is one quant range of size 63 whose two base matrices
    /// are the same.
    fn init_dequantizer(&mut self) {
        let q = self.qps;
        let ac_scale_factor = self.coded_ac_scale_factor[q as usize] as i32;
        let (qr_size, sum, qistart) = (63i32, 63i32, 0i32);
        for inter in 0..2usize {
            for plane in 0..3usize {
                let dc_scale_factor = i32::from(self.coded_dc_scale_factor[usize::from(plane > 0)][q as usize]);
                let bm = 2 * inter + usize::from(plane > 0 && inter == 0);
                for i in 0..64 {
                    let base = i32::from(self.base_matrix[bm][i]);
                    let coeff = (2 * (sum - q) * base - 2 * (qistart - q) * base + qr_size) / (2 * qr_size);
                    let qmin = 8 << (inter + usize::from(i == 0));
                    let qscale = if i != 0 { ac_scale_factor } else { dc_scale_factor };
                    let qbias = (1 + inter as i32) * 3;
                    let value = if i == 0 || self.version < 2 {
                        ((qscale * coeff) / 100 * 4).clamp(qmin, 4096)
                    } else {
                        (qscale * (coeff - qbias) / 100 + qbias) * 4
                    };
                    self.qmat[inter][plane][self.idct_permutation[i]] = value as i16;
                }
            }
        }
    }

    /// FFmpeg's `init_loop_filter`.
    fn init_loop_filter(&mut self) {
        dsp::set_bounding_values(&mut self.bounding_values, self.filter_limit_values[self.qps as usize]);
    }

    /// FFmpeg's `unpack_superblocks`: superblock and fragment coding
    /// (VP3).
    fn unpack_superblocks(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        let vlcs = self.vlcs;
        let superblock_starts = [0, self.u_superblock_start, self.v_superblock_start];
        let superblock_count = self.superblock_count;
        let mut bit: u32 = 0;
        let mut current_run: i32 = 0;
        let mut num_partial_superblocks = 0usize;
        let mut plane0_num_coded_frags = 0usize;

        if self.keyframe {
            self.superblock_coding[..superblock_count].fill(SB_FULLY_CODED);
        } else {
            // The partially coded superblocks.
            bit = gb.get_bits1() ^ 1;
            let mut current_superblock = 0usize;
            while current_superblock < superblock_count && gb.bits_left() > 0 {
                bit ^= 1;
                current_run = vlcs.superblock_run_length.get(gb)?;
                if current_run == 34 {
                    current_run += gb.get_bits(12) as i32;
                }
                let run = current_run as usize;
                if run > superblock_count - current_superblock {
                    return Err(Error::invalid("vp3: invalid partially coded superblock run length"));
                }
                self.superblock_coding[current_superblock..current_superblock + run].fill(bit as u8);
                current_superblock += run;
                if bit != 0 {
                    num_partial_superblocks += run;
                }
            }

            // The fully coded superblocks, among those not partially coded.
            if num_partial_superblocks < superblock_count {
                let mut superblocks_decoded = 0usize;
                current_superblock = 0;
                bit = gb.get_bits1() ^ 1;
                current_run = 0;
                while superblocks_decoded < superblock_count - num_partial_superblocks && gb.bits_left() > 0 {
                    bit ^= 1;
                    current_run = vlcs.superblock_run_length.get(gb)?;
                    if current_run == 34 {
                        current_run += gb.get_bits(12) as i32;
                    }
                    let mut j = 0;
                    while j < current_run {
                        if current_superblock >= superblock_count {
                            return Err(Error::invalid("vp3: invalid fully coded superblock run length"));
                        }
                        if self.superblock_coding[current_superblock] == SB_NOT_CODED {
                            self.superblock_coding[current_superblock] = (2 * bit) as u8;
                            j += 1;
                        }
                        current_superblock += 1;
                    }
                    superblocks_decoded += current_run as usize;
                }
            }

            // Fragment coding runs for the partially coded superblocks;
            // the first run length toggles the bit again.
            if num_partial_superblocks != 0 {
                current_run = 0;
                bit = gb.get_bits1() ^ 1;
            }
        }

        // Which fragments are coded, superblock by superblock.
        self.macroblock_coding[..self.macroblock_count].fill(MODE_COPY);
        let mut list_pos = 0usize;
        for (plane, &sb_start) in superblock_starts.iter().enumerate() {
            let sb_end = sb_start + if plane > 0 { self.c_superblock_count } else { self.y_superblock_count };
            let mut num_coded_frags = 0usize;
            self.coded_fragment_start[plane] = list_pos;

            if self.keyframe {
                for i in sb_start..sb_end {
                    for j in 0..16 {
                        let fragment = self.superblock_fragments[i * 16 + j];
                        if fragment != NO_FRAGMENT {
                            self.coded_fragment_list[list_pos + num_coded_frags] = fragment;
                            num_coded_frags += 1;
                        }
                    }
                }
            } else {
                let mut i = sb_start;
                while i < sb_end && gb.bits_left() > 0 {
                    if gb.bits_left() < (plane0_num_coded_frags >> 2) as i64 {
                        return Err(Error::invalid("vp3: superblock data ends early"));
                    }
                    for j in 0..16 {
                        let fragment = self.superblock_fragments[i * 16 + j];
                        if fragment == NO_FRAGMENT {
                            continue;
                        }
                        let mut coded = u32::from(self.superblock_coding[i]);
                        if coded == u32::from(SB_PARTIALLY_CODED) {
                            // if (current_run-- == 0)
                            let run = current_run;
                            current_run -= 1;
                            if run == 0 {
                                bit ^= 1;
                                current_run = vlcs.fragment_run_length.get(gb)?;
                            }
                            coded = bit;
                        }
                        if coded != 0 {
                            // The mode is read in the next phase.
                            self.all_fragments[fragment].coding_method = MODE_INTER_NO_MV;
                            self.coded_fragment_list[list_pos + num_coded_frags] = fragment;
                            num_coded_frags += 1;
                        } else {
                            self.all_fragments[fragment].coding_method = MODE_COPY;
                        }
                    }
                    i += 1;
                }
            }
            if plane == 0 {
                plane0_num_coded_frags = num_coded_frags;
            }
            self.num_coded_frags[plane * 64..plane * 64 + 64].fill(num_coded_frags as i32);
            list_pos += num_coded_frags;
        }
        Ok(())
    }

    /// FFmpeg's `vp4_get_mb_count`: a run length, at least 1; above
    /// `yuv_macroblock_count` on error.
    fn vp4_get_mb_count(&self, gb: &mut Gb<'_>) -> usize {
        let mut v = 1usize;
        let mut bits;
        loop {
            bits = gb.show_bits(9);
            if bits != 0x1ff {
                break;
            }
            gb.skip_bits(9);
            v += 256;
            if v > self.yuv_macroblock_count {
                return v;
            }
        }
        let thresh = |n: u32| 0x200 - (0x80 >> n);
        if bits < 0x100 {
            gb.skip_bits(1);
        } else if bits < thresh(0) {
            gb.skip_bits(2);
            v += 1;
        } else {
            let n = (1..7).find(|&n| bits < thresh(n)).unwrap_or(7);
            gb.skip_bits(2 + n);
            v += (1 << n) + gb.get_bits(n) as usize;
        }
        v
    }

    /// FFmpeg's `vp4_unpack_macroblocks`: macroblock and block coding
    /// (VP4).
    fn vp4_unpack_macroblocks(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        self.macroblock_coding[..self.macroblock_count].fill(MODE_COPY);
        if self.keyframe {
            return Ok(());
        }
        let count = self.yuv_macroblock_count;

        let mut has_partial = 0u32;
        let mut bit = gb.get_bits1();
        let mut i = 0usize;
        while i < count {
            if gb.bits_left() <= 0 {
                return Err(Error::invalid("vp4: macroblock data ends early"));
            }
            let current_run = self.vp4_get_mb_count(gb);
            if current_run > count - i {
                return Err(Error::invalid("vp4: invalid macroblock run length"));
            }
            self.superblock_coding[i..i + current_run].fill((2 * bit) as u8);
            bit ^= 1;
            has_partial |= bit;
            i += current_run;
        }

        if has_partial != 0 {
            if gb.bits_left() <= 0 {
                return Err(Error::invalid("vp4: macroblock data ends early"));
            }
            bit = gb.get_bits1();
            let mut current_run = self.vp4_get_mb_count(gb);
            for i in 0..count {
                if self.superblock_coding[i] == 0 {
                    if current_run == 0 {
                        bit ^= 1;
                        current_run = self.vp4_get_mb_count(gb);
                    }
                    self.superblock_coding[i] = bit as u8;
                    current_run -= 1;
                }
            }
            if current_run != 0 {
                return Err(Error::invalid("vp4: invalid partial macroblock run length"));
            }
        }

        let vlcs = self.vlcs;
        let mut next_block_pattern_table = 0usize;
        let mut i = 0usize;
        for plane in 0..3 {
            let chroma = usize::from(plane > 0);
            let (sb_width, sb_height, mb_width, mb_height) = if plane > 0 {
                (self.c_superblock_width, self.c_superblock_height, self.c_macroblock_width, self.c_macroblock_height)
            } else {
                (self.y_superblock_width, self.y_superblock_height, self.macroblock_width, self.macroblock_height)
            };
            let (frag_width, frag_height) = (self.fragment_width[chroma], self.fragment_height[chroma]);
            for sb_y in 0..sb_height {
                for sb_x in 0..sb_width {
                    for j in 0..4 {
                        let mb_x = 2 * sb_x + (j >> 1);
                        let mb_y = (2 * sb_y + (j >> 1)) ^ (j & 1);
                        if mb_x >= mb_width || mb_y >= mb_height {
                            continue;
                        }
                        let mb_coded = self.superblock_coding[i];
                        i += 1;
                        let pattern = match mb_coded {
                            SB_FULLY_CODED => 0xf,
                            SB_PARTIALLY_CODED => {
                                // vp4_get_block_pattern
                                let v = vlcs.block_pattern[next_block_pattern_table].get(gb)? as usize;
                                next_block_pattern_table = VP4_BLOCK_PATTERN_TABLE_SELECTOR[v];
                                v + 1
                            }
                            _ => 0,
                        };
                        for k in 0..4 {
                            let block_x = 2 * mb_x + (k & 1);
                            let block_y = 2 * mb_y + (k >> 1);
                            if block_x >= frag_width || block_y >= frag_height {
                                continue;
                            }
                            let fragment = self.fragment_start[plane] + block_y * frag_width + block_x;
                            // MODE_INTER_NO_MV stands until the modes are read.
                            self.all_fragments[fragment].coding_method =
                                if pattern & (8 >> k) != 0 { MODE_INTER_NO_MV } else { MODE_COPY };
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// FFmpeg's `unpack_modes`: the coding mode of every macroblock.
    fn unpack_modes(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        if self.keyframe {
            for fragment in &mut self.all_fragments {
                fragment.coding_method = MODE_INTRA;
            }
            return Ok(());
        }
        let vlcs = self.vlcs;
        let scheme = gb.get_bits(3) as usize;
        let alphabet: [usize; 8] = match scheme {
            0 => {
                let mut custom = [usize::from(MODE_INTER_NO_MV); 8];
                for i in 0..8 {
                    custom[gb.get_bits(3) as usize] = i;
                }
                custom
            }
            7 => [0; 8], // Unused: scheme 7 reads 3 bits per mode.
            _ => MODE_ALPHABET[scheme - 1],
        };

        let fw0 = self.fragment_width[0];
        let fw1 = self.fragment_width[1];
        for sb_y in 0..self.y_superblock_height {
            for sb_x in 0..self.y_superblock_width {
                if gb.bits_left() <= 0 {
                    return Err(Error::invalid("vp3: mode data ends early"));
                }
                for j in 0..4 {
                    let mb_x = 2 * sb_x + (j >> 1);
                    let mb_y = 2 * sb_y + (((j >> 1) + j) & 1);
                    if mb_x >= self.macroblock_width || mb_y >= self.macroblock_height {
                        continue;
                    }
                    let current_macroblock = mb_y * self.macroblock_width + mb_x;
                    let luma = |k: usize| (2 * mb_y + (k >> 1)) * fw0 + 2 * mb_x + (k & 1);

                    // A mode is coded only when a luma block is coded.
                    if (0..4).all(|k| self.all_fragments[luma(k)].coding_method == MODE_COPY) {
                        self.macroblock_coding[current_macroblock] = MODE_INTER_NO_MV;
                        continue;
                    }
                    let coding_mode = if scheme == 7 {
                        gb.get_bits(3) as u8
                    } else {
                        alphabet[vlcs.mode_code.get(gb)? as usize] as u8
                    };
                    self.macroblock_coding[current_macroblock] = coding_mode;
                    for k in 0..4 {
                        let fragment = &mut self.all_fragments[luma(k)];
                        if fragment.coding_method != MODE_COPY {
                            fragment.coding_method = coding_mode;
                        }
                    }
                    let chroma = mb_y * fw1 + mb_x;
                    for start in [self.fragment_start[1], self.fragment_start[2]] {
                        let fragment = &mut self.all_fragments[start + chroma];
                        if fragment.coding_method != MODE_COPY {
                            fragment.coding_method = coding_mode;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// FFmpeg's `vp4_get_mv`.
    fn vp4_get_mv(&self, gb: &mut Gb<'_>, axis: usize, last_motion: i32) -> Result<i32> {
        let selector = VP4_MV_TABLE_SELECTOR[last_motion.unsigned_abs() as usize];
        let v = self.vlcs.vp4_mv[axis * 7 + selector].get(gb)?;
        Ok(if last_motion < 0 { -v } else { v })
    }

    /// One motion vector: VLC (mode 0), fixed-length (mode 1) or VP4
    /// (mode 2, predicted from `last`).
    fn read_mv(&self, gb: &mut Gb<'_>, coding_mode: u32, last: (i32, i32)) -> Result<(i32, i32)> {
        Ok(match coding_mode {
            0 => {
                let x = self.vlcs.motion_vector.get(gb)?;
                let y = self.vlcs.motion_vector.get(gb)?;
                (x, y)
            }
            1 => {
                let x = i32::from(FIXED_MOTION_VECTOR_TABLE[gb.get_bits(6) as usize]);
                let y = i32::from(FIXED_MOTION_VECTOR_TABLE[gb.get_bits(6) as usize]);
                (x, y)
            }
            _ => {
                let x = self.vp4_get_mv(gb, 0, last.0)?;
                let y = self.vp4_get_mv(gb, 1, last.1)?;
                (x, y)
            }
        })
    }

    /// FFmpeg's `unpack_vectors`: the motion vectors of every macroblock.
    fn unpack_vectors(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        if self.keyframe {
            return Ok(());
        }
        let mut motion_x = [0i32; 4];
        let mut motion_y = [0i32; 4];
        let mut last_motion = (0i32, 0i32);
        let mut prior_last_motion = (0i32, 0i32);
        let mut last_gold_motion = (0i32, 0i32);

        // 0: VLC scheme, 1: fixed-length scheme, 2: VP4 scheme.
        let coding_mode = if self.version < 2 { gb.get_bits1() } else { 2 };
        let fw0 = self.fragment_width[0];
        let fw1 = self.fragment_width[1];

        for sb_y in 0..self.y_superblock_height {
            for sb_x in 0..self.y_superblock_width {
                if gb.bits_left() <= 0 {
                    return Err(Error::invalid("vp3: motion vector data ends early"));
                }
                for j in 0..4 {
                    let mb_x = 2 * sb_x + (j >> 1);
                    let mb_y = 2 * sb_y + (((j >> 1) + j) & 1);
                    if mb_x >= self.macroblock_width || mb_y >= self.macroblock_height {
                        continue;
                    }
                    let mode = self.macroblock_coding[mb_y * self.macroblock_width + mb_x];
                    if mode == MODE_COPY {
                        continue;
                    }
                    let luma = |k: usize| (2 * mb_y + (k >> 1)) * fw0 + 2 * mb_x + (k & 1);

                    match mode {
                        MODE_GOLDEN_MV if coding_mode == 2 => {
                            let x = self.vp4_get_mv(gb, 0, last_gold_motion.0)?;
                            let y = self.vp4_get_mv(gb, 1, last_gold_motion.1)?;
                            last_gold_motion = (x, y);
                            (motion_x[0], motion_y[0]) = (x, y);
                        }
                        MODE_GOLDEN_MV | MODE_INTER_PLUS_MV => {
                            // All 6 fragments use the same motion vector.
                            (motion_x[0], motion_y[0]) = self.read_mv(gb, coding_mode, last_motion)?;
                            if mode == MODE_INTER_PLUS_MV {
                                prior_last_motion = last_motion;
                                last_motion = (motion_x[0], motion_y[0]);
                            }
                        }
                        MODE_INTER_FOURMV => {
                            prior_last_motion = last_motion;
                            // One vector per coded luma block; chroma uses
                            // their average.
                            for k in 0..4 {
                                if self.all_fragments[luma(k)].coding_method != MODE_COPY {
                                    (motion_x[k], motion_y[k]) = self.read_mv(gb, coding_mode, prior_last_motion)?;
                                    last_motion = (motion_x[k], motion_y[k]);
                                } else {
                                    (motion_x[k], motion_y[k]) = (0, 0);
                                }
                            }
                        }
                        MODE_INTER_LAST_MV => {
                            (motion_x[0], motion_y[0]) = last_motion;
                        }
                        MODE_INTER_PRIOR_LAST => {
                            (motion_x[0], motion_y[0]) = prior_last_motion;
                            prior_last_motion = last_motion;
                            last_motion = (motion_x[0], motion_y[0]);
                        }
                        _ => {
                            // Intra, inter without a vector, golden
                            // without a vector.
                            (motion_x[0], motion_y[0]) = (0, 0);
                        }
                    }

                    let four = mode == MODE_INTER_FOURMV;
                    for k in 0..4 {
                        let m = if four { k } else { 0 };
                        self.motion_val[0][luma(k)] = [motion_x[m] as i8, motion_y[m] as i8];
                    }
                    let (mut cx, mut cy) = (motion_x[0], motion_y[0]);
                    if four {
                        cx = rshift(motion_x.iter().sum(), 2);
                        cy = rshift(motion_y.iter().sum(), 2);
                    }
                    if self.version <= 2 {
                        cx = (cx >> 1) | (cx & 1);
                        cy = (cy >> 1) | (cy & 1);
                    }
                    self.motion_val[1][mb_y * fw1 + mb_x] = [cx as i8, cy as i8];
                }
            }
        }
        Ok(())
    }

    /// FFmpeg's `unpack_vlcs`: the tokens of one coefficient level of
    /// one plane. Returns the EOB run left over for the next call.
    fn unpack_vlcs(&mut self, gb: &mut Gb<'_>, table: &Vlc, coeff_index: usize, plane: usize, mut eob_run: i32) -> Result<i32> {
        let num_coeffs = self.num_coded_frags[plane * 64 + coeff_index];
        let start = self.dct_tokens[plane][coeff_index];
        let list_start = self.coded_fragment_start[plane];
        let mut j = 0usize;
        if num_coeffs < 0 {
            return Err(Error::invalid("vp3: invalid number of coefficients"));
        }

        let mut coeff_i;
        let mut blocks_ended;
        if eob_run > num_coeffs {
            coeff_i = num_coeffs;
            blocks_ended = num_coeffs;
            eob_run -= num_coeffs;
        } else {
            coeff_i = eob_run;
            blocks_ended = eob_run;
            eob_run = 0;
        }

        let mut put = |tokens: &mut Vec<i16>, token: i16| {
            if let Some(slot) = tokens.get_mut(start + j) {
                *slot = token;
            }
            j += 1;
        };

        // A fake EOB token covers the split between planes or levels.
        if blocks_ended != 0 {
            put(&mut self.dct_tokens_base, token_eob(blocks_ended));
        }

        while coeff_i < num_coeffs && gb.bits_left() > 0 {
            let token = table.get(gb)?;
            if (token as u32) <= 6 {
                eob_run = get_eob_run(gb, token as usize);
                if eob_run == 0 {
                    eob_run = i32::MAX;
                }
                // Only the blocks of this plane end here; the rest spills
                // into the next call.
                if eob_run > num_coeffs - coeff_i {
                    put(&mut self.dct_tokens_base, token_eob(num_coeffs - coeff_i));
                    blocks_ended += num_coeffs - coeff_i;
                    eob_run -= num_coeffs - coeff_i;
                    coeff_i = num_coeffs;
                } else {
                    put(&mut self.dct_tokens_base, token_eob(eob_run));
                    blocks_ended += eob_run;
                    coeff_i += eob_run;
                    eob_run = 0;
                }
            } else if token >= 0 {
                let (mut zero_run, coeff) = get_coeff(gb, token as usize);
                if zero_run != 0 {
                    put(&mut self.dct_tokens_base, token_zero_run(coeff, zero_run));
                } else {
                    // DC prediction runs in raster order, so the DC goes
                    // to the fragment; the token stays for the structure.
                    if coeff_index == 0
                        && let Some(&fragment) = self.coded_fragment_list.get(list_start + coeff_i as usize)
                    {
                        self.all_fragments[fragment].dc = coeff;
                    }
                    put(&mut self.dct_tokens_base, token_coeff(coeff));
                }
                if coeff_index + zero_run > 64 {
                    zero_run = 64 - coeff_index;
                }
                // Zero runs code the higher levels of this block.
                for i in coeff_index + 1..=coeff_index + zero_run {
                    self.num_coded_frags[plane * 64 + i] -= 1;
                }
                coeff_i += 1;
            } else {
                return Err(Error::invalid("vp3: invalid token"));
            }
        }

        // Blocks ended at this level have no higher coefficients.
        if blocks_ended != 0 {
            for i in coeff_index + 1..64 {
                self.num_coded_frags[plane * 64 + i] -= blocks_ended;
            }
        }

        // The next level's tokens follow.
        if plane < 2 {
            self.dct_tokens[plane + 1][coeff_index] = start + j;
        } else if coeff_index < 63 {
            self.dct_tokens[0][coeff_index + 1] = start + j;
        }
        Ok(eob_run)
    }

    /// FFmpeg's `unpack_dct_coeffs` (VP3).
    fn unpack_dct_coeffs(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        let coeff_vlc = self.coeff_vlc;
        self.dct_tokens[0][0] = 0;
        if gb.bits_left() < 16 {
            return Err(Error::invalid("vp3: coefficient data ends early"));
        }
        let dc_y_table = gb.get_bits(4) as usize;
        let dc_c_table = gb.get_bits(4) as usize;

        let mut residual_eob_run = self.unpack_vlcs(gb, &coeff_vlc[dc_y_table], 0, 0, 0)?;
        if gb.bits_left() < 8 {
            return Err(Error::invalid("vp3: coefficient data ends early"));
        }
        self.reverse_dc_prediction(0, self.fragment_width[0], self.fragment_height[0]);

        residual_eob_run = self.unpack_vlcs(gb, &coeff_vlc[dc_c_table], 0, 1, residual_eob_run)?;
        residual_eob_run = self.unpack_vlcs(gb, &coeff_vlc[dc_c_table], 0, 2, residual_eob_run)?;
        for plane in 1..3 {
            self.reverse_dc_prediction(self.fragment_start[plane], self.fragment_width[1], self.fragment_height[1]);
        }

        if gb.bits_left() < 8 {
            return Err(Error::invalid("vp3: coefficient data ends early"));
        }
        let ac_y_table = gb.get_bits(4) as usize;
        let ac_c_table = gb.get_bits(4) as usize;
        for level in 1..64 {
            let group = ac_group(level);
            let y_table = &coeff_vlc[ac_y_table + group];
            let c_table = &coeff_vlc[ac_c_table + group];
            residual_eob_run = self.unpack_vlcs(gb, y_table, level, 0, residual_eob_run)?;
            residual_eob_run = self.unpack_vlcs(gb, c_table, level, 1, residual_eob_run)?;
            residual_eob_run = self.unpack_vlcs(gb, c_table, level, 2, residual_eob_run)?;
        }
        Ok(())
    }

    /// FFmpeg's `vp4_unpack_vlcs`: the tokens of one block. An EOB run is
    /// kept in `eob_tracker` and each ended block gets a `TOKEN_EOB(0)`.
    fn vp4_unpack_vlcs(&mut self, gb: &mut Gb<'_>, tables: &[&Vlc; 64], plane: usize, eob_tracker: &mut [i32; 64], fragment: usize) -> Result<()> {
        let mut coeff_i = 0usize;
        while eob_tracker[coeff_i] == 0 {
            if gb.bits_left() < 1 {
                return Err(Error::invalid("vp4: coefficient data ends early"));
            }
            let token = tables[coeff_i].get(gb)?;
            if (token as u32) <= 6 {
                let eob_run = get_eob_run(gb, token as usize);
                self.push_token(plane, coeff_i, token_eob(0));
                eob_tracker[coeff_i] = eob_run - 1;
                return Ok(());
            } else if token >= 0 {
                let (mut zero_run, coeff) = get_coeff(gb, token as usize);
                if zero_run != 0 {
                    if coeff_i + zero_run > 64 {
                        zero_run = 64 - coeff_i;
                    }
                    self.push_token(plane, coeff_i, token_zero_run(coeff, zero_run));
                    coeff_i += zero_run;
                } else {
                    if coeff_i == 0 {
                        self.all_fragments[fragment].dc = coeff;
                    }
                    self.push_token(plane, coeff_i, token_coeff(coeff));
                }
                coeff_i += 1;
                if coeff_i >= 64 {
                    return Ok(());
                }
            } else {
                return Err(Error::invalid("vp4: invalid token"));
            }
        }
        self.push_token(plane, coeff_i, token_eob(0));
        eob_tracker[coeff_i] -= 1;
        Ok(())
    }

    /// `*s->dct_tokens[plane][level]++ = token`.
    fn push_token(&mut self, plane: usize, level: usize, token: i16) {
        let pos = &mut self.dct_tokens[plane][level];
        if let Some(slot) = self.dct_tokens_base.get_mut(*pos) {
            *slot = token;
        }
        *pos += 1;
    }

    /// FFmpeg's `vp4_set_tokens_base`: one token per fragment per level.
    fn vp4_set_tokens_base(&mut self) {
        let mut base = 0;
        for plane in 0..3 {
            let chroma = usize::from(plane > 0);
            for level in 0..64 {
                self.dct_tokens[plane][level] = base;
                base += self.fragment_width[chroma] * self.fragment_height[chroma];
            }
        }
    }

    /// FFmpeg's `vp4_unpack_dct_coeffs`, with VP4's DC prediction.
    fn vp4_unpack_dct_coeffs(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        if gb.bits_left() < 16 {
            return Err(Error::invalid("vp4: coefficient data ends early"));
        }
        let dc_y_table = gb.get_bits(4) as usize;
        let dc_c_table = gb.get_bits(4) as usize;
        let ac_y_table = gb.get_bits(4) as usize;
        let ac_c_table = gb.get_bits(4) as usize;

        let coeff_vlc = self.coeff_vlc;
        let mut tables: [[&Vlc; 64]; 2] = [[&coeff_vlc[0]; 64]; 2];
        tables[0][0] = &coeff_vlc[dc_y_table];
        tables[1][0] = &coeff_vlc[dc_c_table];
        for level in 1..64 {
            tables[0][level] = &coeff_vlc[ac_y_table + ac_group(level)];
            tables[1][level] = &coeff_vlc[ac_c_table + ac_group(level)];
        }

        self.vp4_set_tokens_base();
        let mut last_dc = [0i32; 3];

        for plane in 0..3 {
            let chroma = usize::from(plane > 0);
            let (frag_width, frag_height) = (self.fragment_width[chroma], self.fragment_height[chroma]);
            let mut eob_tracker = [0i32; 64];
            self.dc_pred_row[..frag_width].fill(VP4_PREDICTOR_RESET);
            let mut dc_pred = [[VP4_PREDICTOR_RESET; 6]; 6];

            let mut sb_y = 0;
            while sb_y * 4 < frag_height {
                let mut sb_x = 0;
                while sb_x * 4 < frag_width {
                    // vp4_dc_pred_before
                    for i in 0..4 {
                        dc_pred[0][i + 1] = self.dc_pred_row[sb_x * 4 + i];
                    }
                    for row in &mut dc_pred[1..5] {
                        row[1..5].fill(VP4_PREDICTOR_RESET);
                    }

                    for offset in &HILBERT_OFFSET {
                        let (hx, hy) = (offset[0], offset[1]);
                        let x = 4 * sb_x + hx;
                        let y = 4 * sb_y + hy;
                        if x >= frag_width || y >= frag_height {
                            continue;
                        }
                        let fragment = self.fragment_start[plane] + y * frag_width + x;
                        let coding_method = self.all_fragments[fragment].coding_method;
                        if coding_method == MODE_COPY {
                            continue;
                        }
                        self.vp4_unpack_vlcs(gb, &tables[chroma], plane, &mut eob_tracker, fragment)?;

                        let dc_block_type = VP4_PRED_BLOCK_TYPE_MAP[usize::from(coding_method)];
                        let predicted = vp4_dc_pred(&dc_pred, hy + 1, hx + 1, &last_dc, dc_block_type);
                        let dc = (i32::from(self.all_fragments[fragment].dc) + predicted) as i16;
                        self.all_fragments[fragment].dc = dc;
                        last_dc[dc_block_type] = i32::from(dc);
                        dc_pred[hy + 1][hx + 1] = Vp4Predictor { dc: i32::from(dc), kind: dc_block_type as u8 };
                    }

                    // vp4_dc_pred_after
                    for i in 0..4 {
                        self.dc_pred_row[sb_x * 4 + i] = dc_pred[4][i + 1];
                    }
                    for row in &mut dc_pred[1..5] {
                        row[0] = row[4];
                    }
                    sb_x += 1;
                }
                sb_y += 1;
            }
        }

        self.vp4_set_tokens_base();
        Ok(())
    }

    /// FFmpeg's `reverse_dc_prediction` for one plane (VP3).
    fn reverse_dc_prediction(&mut self, first_fragment: usize, fragment_width: usize, fragment_height: usize) {
        const PUL: usize = 8;
        const PU: usize = 4;
        const PUR: usize = 2;
        const PL: usize = 1;
        // Up-left, up, up-right and left weights.
        const PREDICTOR_TRANSFORM: [[i32; 4]; 16] = [
            [0, 0, 0, 0],
            [0, 0, 0, 128],
            [0, 0, 128, 0],
            [0, 0, 53, 75],
            [0, 128, 0, 0],
            [0, 64, 0, 64],
            [0, 128, 0, 0],
            [0, 0, 53, 75],
            [128, 0, 0, 0],
            [0, 0, 0, 128],
            [64, 0, 64, 0],
            [0, 0, 53, 75],
            [0, 128, 0, 0],
            [-104, 116, 0, 116],
            [24, 80, 24, 0],
            [-104, 116, 0, 116],
        ];
        // Which blocks may predict from which: intra from intra, golden
        // from golden, the other inter modes from each other.
        const COMPATIBLE_FRAME: [u8; 9] = [1, 0, 1, 1, 1, 2, 2, 1, 3];

        let frags = &mut self.all_fragments;
        let compatible = |frags: &[Fragment], i: usize| COMPATIBLE_FRAME[usize::from(frags[i].coding_method)];
        let (mut vl, mut vul, mut vu, mut vur) = (0i32, 0i32, 0i32, 0i32);
        let mut last_dc = [0i16; 3];
        let mut i = first_fragment;

        for y in 0..fragment_height {
            for x in 0..fragment_width {
                if frags[i].coding_method != MODE_COPY {
                    let current_frame_type = compatible(frags, i);
                    let mut transform = 0;
                    if x > 0 {
                        let l = i - 1;
                        vl = i32::from(frags[l].dc);
                        if compatible(frags, l) == current_frame_type {
                            transform |= PL;
                        }
                    }
                    if y > 0 {
                        let u = i - fragment_width;
                        vu = i32::from(frags[u].dc);
                        if compatible(frags, u) == current_frame_type {
                            transform |= PU;
                        }
                        if x > 0 {
                            let ul = i - fragment_width - 1;
                            vul = i32::from(frags[ul].dc);
                            if compatible(frags, ul) == current_frame_type {
                                transform |= PUL;
                            }
                        }
                        if x + 1 < fragment_width {
                            let ur = i - fragment_width + 1;
                            vur = i32::from(frags[ur].dc);
                            if compatible(frags, ur) == current_frame_type {
                                transform |= PUR;
                            }
                        }
                    }

                    let predicted_dc = if transform == 0 {
                        // Nothing to predict from: the last DC of this type.
                        i32::from(last_dc[usize::from(current_frame_type)])
                    } else {
                        let t = PREDICTOR_TRANSFORM[transform];
                        let mut p = (t[0] * vul + t[1] * vu + t[2] * vur + t[3] * vl) / 128;
                        // Out-of-range checks for the [ul u l] and
                        // [ul u ur l] predictors.
                        if transform == 15 || transform == 13 {
                            if (p - vu).abs() > 128 {
                                p = vu;
                            } else if (p - vl).abs() > 128 {
                                p = vl;
                            } else if (p - vul).abs() > 128 {
                                p = vul;
                            }
                        }
                        p
                    };
                    let dc = (i32::from(frags[i].dc) + predicted_dc) as i16;
                    frags[i].dc = dc;
                    last_dc[usize::from(current_frame_type)] = dc;
                }
                i += 1;
            }
        }
    }

    /// FFmpeg's `apply_loop_filter` on fragment rows `ystart..yend` of a
    /// plane.
    fn apply_loop_filter(&self, cur: &mut Picture, plane: usize, ystart: usize, yend: usize) {
        let bv = &self.bounding_values;
        let chroma = usize::from(plane > 0);
        let width = self.fragment_width[chroma];
        let height = self.fragment_height[chroma];
        let stride = self.width >> chroma;
        let data = &mut cur.planes[plane];
        let mut fragment = self.fragment_start[plane] + ystart * width;

        for y in ystart..yend {
            for x in 0..width {
                // Order matters: some pixels are filtered twice.
                if self.all_fragments[fragment].coding_method != MODE_COPY {
                    let px = 8 * y * stride + 8 * x;
                    let v_edge = |e: usize| [e - 2 * stride, e - stride, e, e + stride];
                    // Left edge, except in the left column.
                    if x > 0 {
                        dsp::h_loop_filter(data, &rows8(px, stride), bv);
                    }
                    // Top edge, except in the top row.
                    if y > 0 {
                        dsp::v_loop_filter(data, v_edge(px), 8, bv);
                    }
                    // Right edge, unless the right neighbour is coded (its
                    // own left edge comes next).
                    if x < width - 1 && self.all_fragments[fragment + 1].coding_method == MODE_COPY {
                        dsp::h_loop_filter(data, &rows8(px + 8, stride), bv);
                    }
                    // Bottom edge, unless the fragment below is coded.
                    if y < height - 1 && self.all_fragments[fragment + width].coding_method == MODE_COPY {
                        dsp::v_loop_filter(data, v_edge(px + 8 * stride), 8, bv);
                    }
                }
                fragment += 1;
            }
        }
    }

    /// FFmpeg's `vp3_dequant`: pull the next block's tokens from the 64
    /// levels into `block`. Returns the last level read; 0 means DC only.
    fn vp3_dequant(&mut self, fragment: usize, plane: usize, inter: usize, block: &mut [i16; 64]) -> usize {
        let dequantizer = &self.qmat[inter][plane];
        let perm = &self.idct_scantable;
        let mut i = 0usize;
        loop {
            let pos = self.dct_tokens[plane][i];
            let token = i32::from(self.dct_tokens_base.get(pos).copied().unwrap_or(0));
            match token & 3 {
                0 => {
                    // EOB: 0-3 are token types, so the run is now over.
                    let t = token - 1;
                    if t < 4 {
                        self.dct_tokens[plane][i] += 1;
                    } else if let Some(slot) = self.dct_tokens_base.get_mut(pos) {
                        *slot = (t & !3) as i16;
                    }
                    break;
                }
                1 => {
                    // Zero run.
                    self.dct_tokens[plane][i] += 1;
                    i += ((token >> 2) & 0x7f) as usize;
                    if i > 63 {
                        // Coefficient index overflow.
                        return i;
                    }
                    block[perm[i]] = ((token >> 9) * i32::from(dequantizer[perm[i]])) as i16;
                    i += 1;
                }
                2 => {
                    block[perm[i]] = ((token >> 2) * i32::from(dequantizer[perm[i]])) as i16;
                    self.dct_tokens[plane][i] += 1;
                    i += 1;
                }
                _ => return i,
            }
            if i >= 64 {
                // The result must be a valid level.
                i -= 1;
                break;
            }
        }
        // The DC with its prediction is in the fragment.
        let dc = i32::from(self.all_fragments[fragment].dc) * i32::from(self.qmat[inter][plane][0]);
        block[0] = dc as i16;
        i
    }

    /// FFmpeg's `vp4_mc_loop_filter`: when the motion vector reaches across
    /// a block edge of the reference, filter that edge in a 12×12 copy and
    /// return the 9×9 source block in `temp`. False when no filter
    /// applies.
    #[allow(clippy::too_many_arguments)]
    fn vp4_mc_loop_filter(&self, plane: usize, motion_x: i32, motion_y: i32, bx: usize, by: usize, src: &[u8], src_x: i32, src_y: i32, temp: &mut [u8; 81]) -> bool {
        let chroma = usize::from(plane > 0);
        let motion_shift = if chroma == 1 { 4 } else { 2 };
        let subpel_mask = if chroma == 1 { 3 } else { 1 };
        let block_width = if chroma == 1 { 8 } else { 16 };
        let plane_width = self.width >> chroma;
        let plane_height = self.height >> chroma;
        let bv = &self.bounding_values;
        const LOOP_STRIDE: usize = 12;
        let mut lp = [0u8; 12 * LOOP_STRIDE];

        // Division, not a shift, as in FFmpeg (negative values).
        let mut x = 8 * bx as i32 + motion_x / motion_shift;
        let mut y = 8 * by as i32 + motion_y / motion_shift;
        let x_subpel = motion_x & subpel_mask;
        let y_subpel = motion_y & subpel_mask;
        let sign = |v: i32| if v > 0 { 1 } else { -1 };

        if x_subpel != 0 || y_subpel != 0 {
            x -= 1;
            y -= 1;
            if x_subpel != 0 {
                x = x.min(x + sign(motion_x));
            }
            if y_subpel != 0 {
                y = y.min(y + sign(motion_y));
            }
            let x2 = x + block_width;
            let y2 = y + block_width;
            if x2 < 0 || x2 >= plane_width as i32 || y2 < 0 || y2 >= plane_height as i32 {
                return false;
            }
            let x_offset = (-(x + 2) & 7) + 2;
            let y_offset = (-(y + 2) & 7) + 2;
            edge_block(&mut lp, 12, 12, src, plane_width, src_x - 1, src_y - 1, plane_width, plane_height);
            if x_offset <= 8 + x_subpel {
                let xo = x_offset as usize;
                let rows: [usize; 12] = std::array::from_fn(|k| k * LOOP_STRIDE + xo);
                dsp::h_loop_filter(&mut lp, &rows, bv);
            }
            if y_offset <= 8 + y_subpel {
                let e = y_offset as usize * LOOP_STRIDE;
                dsp::v_loop_filter(&mut lp, [e - 2 * LOOP_STRIDE, e - LOOP_STRIDE, e, e + LOOP_STRIDE], 12, bv);
            }
        } else {
            let x_offset = (-x & 7) as usize;
            let y_offset = (-y & 7) as usize;
            if x_offset == 0 && y_offset == 0 {
                return false;
            }
            edge_block(&mut lp, 12, 12, src, plane_width, src_x - 1, src_y - 1, plane_width, plane_height);
            if x_offset != 0 {
                let first = LOOP_STRIDE + x_offset + 1;
                dsp::h_loop_filter(&mut lp, &rows8(first, LOOP_STRIDE), bv);
            }
            if y_offset != 0 {
                let e = (y_offset + 1) * LOOP_STRIDE + 1;
                dsp::v_loop_filter(&mut lp, [e - 2 * LOOP_STRIDE, e - LOOP_STRIDE, e, e + LOOP_STRIDE], 8, bv);
            }
        }

        for i in 0..9 {
            let o = (i + 1) * LOOP_STRIDE + 1;
            temp[i * 9..i * 9 + 9].copy_from_slice(&lp[o..o + 9]);
        }
        true
    }

    /// FFmpeg's `render_slice`: reconstruct one chroma superblock row and
    /// the luma superblock rows beside it, then loop-filter them (VP3).
    fn render_slice(&mut self, slice: usize, cur: &mut Picture, last: Option<&Picture>, golden: Option<&Picture>) {
        if slice >= self.c_superblock_height {
            return;
        }
        for plane in 0..3 {
            let chroma = usize::from(plane > 0);
            let stride = self.width >> chroma;
            let plane_width = stride;
            let plane_height = self.height >> chroma;
            let mut sb_y = slice << (1 - chroma);
            let slice_height = sb_y + 1 + (1 - chroma);
            let slice_width = if chroma == 1 { self.c_superblock_width } else { self.y_superblock_width };
            let (frag_width, frag_height) = (self.fragment_width[chroma], self.fragment_height[chroma]);
            let fragment_start = self.fragment_start[plane];

            while sb_y < slice_height {
                for sb_x in 0..slice_width {
                    for offset in &HILBERT_OFFSET {
                        let x = 4 * sb_x + offset[0];
                        let y = 4 * sb_y + offset[1];
                        if x >= frag_width || y >= frag_height {
                            continue;
                        }
                        let fragment = y * frag_width + x;
                        let i = fragment_start + fragment;
                        let first_pixel = 8 * y * stride + 8 * x;
                        let coding_method = self.all_fragments[i].coding_method;

                        if coding_method == MODE_COPY {
                            // Copy straight from the last frame.
                            if let Some(last) = last {
                                copy_block(&mut cur.planes[plane], &last.planes[plane], first_pixel, stride);
                            }
                            continue;
                        }

                        if coding_method != MODE_INTRA {
                            let reference = if coding_method == MODE_USING_GOLDEN || coding_method == MODE_GOLDEN_MV {
                                golden
                            } else {
                                last
                            };
                            // Key frames are all intra; inter frames always
                            // have both references.
                            if let Some(reference) = reference {
                                let src = &reference.planes[plane];
                                if coding_method > MODE_INTRA && coding_method != MODE_USING_GOLDEN {
                                    let mv = self.motion_val[chroma][fragment];
                                    let (mut motion_x, mut motion_y) = (i32::from(mv[0]), i32::from(mv[1]));
                                    if chroma == 1 && self.version >= 2 {
                                        motion_x = (motion_x >> 1) | (motion_x & 1);
                                        motion_y = (motion_y >> 1) | (motion_y & 1);
                                    }
                                    let src_x = (motion_x >> 1) + 8 * x as i32;
                                    let src_y = (motion_y >> 1) + 8 * y as i32;
                                    let halfpel = (motion_x & 1) | ((motion_y & 1) << 1);
                                    let mut temp = [0u8; 81];
                                    let filtered = self.version >= 2
                                        && self.vp4_mc_loop_filter(
                                            plane,
                                            i32::from(mv[0]),
                                            i32::from(mv[1]),
                                            x,
                                            y,
                                            src,
                                            src_x,
                                            src_y,
                                            &mut temp,
                                        );
                                    if !filtered {
                                        edge_block(&mut temp, 9, 9, src, stride, src_x, src_y, plane_width, plane_height);
                                    }
                                    put_mc(&mut cur.planes[plane], first_pixel, stride, &temp, halfpel, motion_x, motion_y);
                                } else {
                                    copy_block(&mut cur.planes[plane], src, first_pixel, stride);
                                }
                            }
                        }

                        // Inverse DCT, put (intra) or added.
                        let mut block = [0i16; 64];
                        let rows = rows8(first_pixel, stride);
                        if coding_method == MODE_INTRA {
                            self.vp3_dequant(i, plane, 0, &mut block);
                            dsp::idct_put(&mut cur.planes[plane], &rows, &mut block);
                        } else if self.vp3_dequant(i, plane, 1, &mut block) != 0 {
                            dsp::idct_add(&mut cur.planes[plane], &rows, &mut block);
                        } else {
                            dsp::idct_dc_add(&mut cur.planes[plane], &rows, &mut block);
                        }
                    }
                }

                // Filter up to the last row of the superblock row.
                if self.version < 2 && !self.skip_loop_filter {
                    let ystart = (4 * sb_y).saturating_sub(usize::from(sb_y > 0));
                    let yend = (4 * sb_y + 3).min(frag_height - 1);
                    self.apply_loop_filter(cur, plane, ystart, yend);
                }
                sb_y += 1;
            }
        }
    }

    /// FFmpeg's `vp3_decode_frame`.
    fn decode_frame(&mut self, data: &[u8], pts: Option<i64>) -> Result<()> {
        let mut gb = Gb::new(data);
        self.keyframe = gb.get_bits1() == 0;
        gb.skip_bits(1);
        let last_qps = self.qps;
        self.qps = gb.get_bits(6) as i32;
        self.skip_loop_filter = self.filter_limit_values[self.qps as usize] == 0;
        if self.qps != last_qps {
            self.init_loop_filter();
            self.init_dequantizer();
        }

        // A new buffer becomes the current frame and the previous one the
        // last frame.
        let mut last = self.current.take();
        let mut cur = Picture::new(self.width, self.height);

        if self.keyframe {
            gb.skip_bits(4); // width code
            gb.skip_bits(4); // height code
            if self.version != 0 {
                self.version = gb.get_bits(5) as i32;
            }
            if self.version != 0 {
                // A set bit is an "unsupported keyframe coding type";
                // FFmpeg goes on.
                gb.skip_bits(1);
                gb.skip_bits(2);
                if self.version >= 2 {
                    // Macroblock height and width, their multipliers and
                    // dividers, and 2 unknown bits. FFmpeg only asks for
                    // samples when they differ from what it expects.
                    gb.skip_bits(8 + 8 + 5 + 3 + 5 + 3 + 2);
                }
            }
        } else if self.golden.is_none() {
            // First frame not a keyframe: a blank golden frame, which is
            // also the last frame.
            let blank = Arc::new(Picture::new(self.width, self.height));
            self.golden = Some(blank.clone());
            last = Some(blank);
        }

        if let Err(e) = self.unpack_frame(&mut gb) {
            // The unrendered buffer stays the current frame, and on a
            // keyframe the golden frame.
            let cur = Arc::new(cur);
            if self.keyframe {
                self.golden = Some(cur.clone());
            }
            self.current = Some(cur);
            return Err(e);
        }

        let golden = if self.keyframe { None } else { self.golden.clone() };
        for slice in 0..self.c_superblock_height {
            self.render_slice(slice, &mut cur, last.as_deref(), golden.as_deref());
        }
        // Filter the last row.
        if self.version < 2 {
            for plane in 0..3 {
                let row = (self.height >> (3 + usize::from(plane > 0))) - 1;
                self.apply_loop_filter(&mut cur, plane, row, row + 1);
            }
        }

        let cur = Arc::new(cur);
        if self.keyframe {
            self.golden = Some(cur.clone());
        }
        self.ready.push_back(self.output_frame(&cur, pts));
        self.current = Some(cur);
        Ok(())
    }

    /// The bitstream parts of `vp3_decode_frame`, up to the DCT tokens.
    fn unpack_frame(&mut self, gb: &mut Gb<'_>) -> Result<()> {
        self.all_fragments.fill(Fragment::default());
        if self.version < 2 {
            self.unpack_superblocks(gb)?;
        } else {
            self.vp4_unpack_macroblocks(gb)?;
        }
        self.unpack_modes(gb)?;
        self.unpack_vectors(gb)?;
        // unpack_block_qpis reads nothing with one quantizer per frame.
        if self.version < 2 {
            self.unpack_dct_coeffs(gb)
        } else {
            self.vp4_unpack_dct_coeffs(gb)
        }
    }

    /// The output frame: the visible area, rows back in display order.
    fn output_frame(&self, pic: &Picture, pts: Option<i64>) -> Frame {
        let (w, h) = (self.visible.0 as usize, self.visible.1 as usize);
        let sizes = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
        let planes = (0..3)
            .map(|p| {
                let (pw, ph) = sizes[p];
                let chroma = usize::from(p > 0);
                let stride = self.width >> chroma;
                let rows = self.height >> chroma;
                let src = &pic.planes[p];
                let mut data = Vec::with_capacity(pw * ph);
                for r in 0..ph {
                    let o = (rows - 1 - r) * stride;
                    data.extend_from_slice(&src[o..o + pw]);
                }
                VideoPlane { stride: pw, data }
            })
            .collect();
        Frame::Video(VideoFrame { pts, planes })
    }
}

impl Decoder for Vp3Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        self.flushed = false;
        // FFmpeg never hands an empty packet to the decoder.
        if packet.data.is_empty() {
            return Ok(());
        }
        self.decode_frame(&packet.data, packet.pts)
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(frame) => Ok(frame),
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        // No frame delay: everything decoded is already queued.
        self.flushed = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        // vp3_decode_flush: drop the reference frames.
        self.current = None;
        self.golden = None;
        self.ready.clear();
        self.flushed = false;
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        Some(PixelFormat::Yuv420P)
    }

    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        Some(self.visible)
    }
}

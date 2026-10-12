// Ported from FFmpeg (commit 2da55bf): libavcodec/svq3.c (svq3_decode_block,
// svq3_fetch_diagonal_mv, svq3_pred_motion, svq3_mc_dir_part, svq3_mc_dir,
// hl_decode_mb and its luma helpers, svq3_decode_mb,
// svq3_decode_slice_header, svq3_decode_extradata, svq3_decode_init,
// alloc_dummy_frame, svq3_decode_frame) with libavutil/crc.c's
// AV_CRC_16_CCITT for the watermark key.
// License: LGPL-2.1-or-later

//! The SVQ3 decoding process: the SEQH header, the watermark key, slices,
//! macroblocks, motion compensation and the three-picture reference pool
//! with FFmpeg's output order (one reference behind, B pictures at once).

use compcol::Decoder as _;
use oxideav_core::{Error, Result};

use crate::dsp;
use crate::getbits::{get_interleaved_se_golomb, get_interleaved_ue_golomb, skip_1stop_8data_bits, GetBits};
use crate::tables::{
    CHROMA_DC_SCAN, CHROMA_QP, GOLOMB_TO_INTER_CBP, GOLOMB_TO_INTRA4X4_CBP, I_MB_TYPE_INFO, LUMA_DC_ZIGZAG_SCAN,
    PART_NOT_AVAILABLE, SCAN8, SVQ3_DCT_TABLES, SVQ3_PRED_0, SVQ3_PRED_1, SVQ3_SCAN, ZIGZAG_SCAN,
};

const NUM_PICS: usize = 3;
const FULLPEL_MODE: u32 = 1;
const HALFPEL_MODE: u32 = 2;
const THIRDPEL_MODE: u32 = 3;
const PREDICT_MODE: u32 = 4;

// mpegutils.h macroblock type flags, the ones SVQ3 sets.
const MB_TYPE_INTRA4X4: u32 = 1 << 0;
const MB_TYPE_INTRA16X16: u32 = 1 << 1;
const MB_TYPE_16X16: u32 = 1 << 3;
const MB_TYPE_SKIP: u32 = 1 << 11;
/// FFmpeg's `-1` in a picture's `mb_type` (intra, or no motion to borrow).
const NO_MOTION: u32 = u32::MAX;

fn is_intra(t: u32) -> bool {
    t & 7 != 0
}
fn is_inter(t: u32) -> bool {
    t & (MB_TYPE_16X16 | (1 << 4) | (1 << 5) | (1 << 6)) != 0
}

/// Luma border around each picture (intra prediction reads one sample
/// past the macroblock edges); chroma has half.
const PAD: usize = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PictType {
    I,
    P,
    B,
}

struct Picture {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
    mb_type: Vec<u32>,
    motion_val: [Vec<[i16; 2]>; 2],
    /// FFmpeg's `f->data[0]` being set.
    valid: bool,
    pts: Option<i64>,
}

/// The stream's SEQH settings.
struct SeqHeader {
    width: usize,
    height: usize,
    halfpel: bool,
    thirdpel: bool,
    low_delay: bool,
    watermark_key: u32,
    has_watermark: bool,
}

/// svq3_decode_extradata on the SEQH at `at` of `extradata`.
fn parse_seqh(extradata: &[u8], at: usize) -> Result<SeqHeader> {
    let bad = || Error::invalid("svq3: bad SEQH header");
    let size = u32::from_be_bytes(extradata[at + 4..at + 8].try_into().map_err(|_| bad())?) as usize;
    if size > extradata.len() - at - 8 {
        return Err(bad());
    }
    let payload = &extradata[at + 8..at + 8 + size];
    let mut gb = GetBits::new(payload).ok_or_else(bad)?;
    let (width, height) = match gb.get_bits(3).ok_or_else(bad)? {
        0 => (160, 120),
        1 => (128, 96),
        2 => (176, 144),
        3 => (352, 288),
        4 => (704, 576),
        5 => (240, 180),
        6 => (320, 240),
        _ => (gb.get_bits(12).ok_or_else(bad)? as usize, gb.get_bits(12).ok_or_else(bad)? as usize),
    };
    if width == 0 || height == 0 {
        return Err(Error::invalid("svq3: zero picture size"));
    }
    let halfpel = gb.get_bit().ok_or_else(bad)? == 1;
    let thirdpel = gb.get_bit().ok_or_else(bad)? == 1;
    gb.skip(4).ok_or_else(bad)?; // unknown fields
    let low_delay = gb.get_bit().ok_or_else(bad)? == 1;
    gb.skip(1).ok_or_else(bad)?; // unknown field
    skip_1stop_8data_bits(&mut gb).ok_or_else(bad)?;
    let has_watermark = gb.get_bit().ok_or_else(bad)? == 1;
    let mut watermark_key = 0;
    if has_watermark {
        let ue = |gb: &mut GetBits| get_interleaved_ue_golomb(gb).ok_or_else(bad);
        let w = ue(&mut gb)?;
        let h = ue(&mut gb)?;
        ue(&mut gb)?;
        gb.get_bits(8).ok_or_else(bad)?;
        gb.get_bits(2).ok_or_else(bad)?;
        ue(&mut gb)?;
        let offset = gb.position().div_ceil(8);
        if h == 0 || gb.bits_left() == 0 || u64::from(w) * 4 > u64::from(u32::MAX / h) {
            return Err(bad());
        }
        let logo = inflate_capped(&payload[offset.min(size)..], w as usize * h as usize * 4)
            .ok_or_else(|| Error::invalid("svq3: could not uncompress watermark logo"))?;
        let crc = crc16_ccitt(&logo);
        watermark_key = u32::from(crc) << 16 | u32::from(crc);
    }
    Ok(SeqHeader { width, height, halfpel, thirdpel, low_delay, watermark_key, has_watermark })
}

/// zlib's `uncompress` into at most `cap` bytes: the whole stream, or None.
fn inflate_capped(input: &[u8], cap: usize) -> Option<Vec<u8>> {
    // The contract's allocation bound on top of FFmpeg's.
    let cap = cap.min(256 << 20);
    let mut dec = compcol::zlib::Decoder::new();
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 << 10];
    let mut consumed = 0;
    loop {
        let (p, status) = dec.decode(&input[consumed..], &mut chunk).ok()?;
        consumed += p.consumed;
        out.extend_from_slice(&chunk[..p.written]);
        if out.len() > cap {
            return None;
        }
        match status {
            compcol::Status::StreamEnd => return Some(out),
            compcol::Status::InputEmpty if p.written == 0 => return None,
            _ => {}
        }
    }
}

/// av_crc with AV_CRC_16_CCITT, byte-swapped back as svq3.c does: the
/// MSB-first CRC-16 of polynomial 0x1021 from 0.
fn crc16_ccitt(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

/// What one packet decoded to: the pool picture to output, if any.
pub type Output = Option<usize>;

/// SVQ3Context.
pub struct Svq3Decoder {
    pub width: usize,
    pub height: usize,
    halfpel_flag: bool,
    thirdpel_flag: bool,
    has_watermark: bool,
    watermark_key: u32,
    low_delay: bool,
    mb_width: usize,
    mb_height: usize,
    mb_stride: usize,
    mb_num: usize,
    b_stride: usize,
    h_edge_pos: i32,
    v_edge_pos: i32,
    ys: usize,
    cs: usize,
    y_origin: usize,
    c_origin: usize,

    frames: [Picture; NUM_PICS],
    cur_pic: usize,
    last_pic: usize,
    next_pic: usize,

    slice_num: i32,
    qscale: i32,
    adaptive_quant: bool,
    cbp: u32,
    frame_num: i32,
    frame_num_offset: i32,
    prev_frame_num_offset: i32,
    prev_frame_num: i32,
    pict_type: PictType,
    slice_type: PictType,

    mb_x: usize,
    mb_y: usize,
    mb_xy: usize,
    chroma_pred_mode: i32,
    intra16x16_pred_mode: i32,
    intra4x4_pred_mode_cache: [i8; 40],
    intra4x4_pred_mode: Vec<i8>,
    mb2br_xy: Vec<usize>,
    top_samples_available: u32,
    left_samples_available: u32,
    emu: Vec<u8>,

    mv_cache: [[[i16; 2]; 40]; 2],
    ref_cache: [[i8; 40]; 2],
    mb: Vec<i16>,
    mb_luma_dc: [i16; 16],
    non_zero_count_cache: [u8; 120],
    block_offset: [usize; 96],
    chroma_dc_qmul: i32,
}

/// One picture of the pool and one other, mutable and shared.
fn pair(frames: &mut [Picture; NUM_PICS], cur: usize, other: usize) -> (&mut Picture, &Picture) {
    debug_assert_ne!(cur, other);
    if cur < other {
        let (a, b) = frames.split_at_mut(other);
        (&mut a[cur], &b[0])
    } else {
        let (a, b) = frames.split_at_mut(cur);
        (&mut b[0], &a[other])
    }
}

impl Svq3Decoder {
    /// svq3_decode_init: the SEQH in `extradata` if there is one, else the
    /// container's `width`×`height` and FFmpeg's defaults.
    pub fn new(extradata: &[u8], width: Option<u32>, height: Option<u32>) -> Result<Self> {
        let seqh = extradata.windows(4).enumerate().find(|(m, w)| m + 8 < extradata.len() && *w == b"SEQH").map(|(m, _)| m);
        let seq = match seqh {
            Some(at) => parse_seqh(extradata, at)?,
            None => SeqHeader {
                width: width.unwrap_or(0) as usize,
                height: height.unwrap_or(0) as usize,
                halfpel: true,
                thirdpel: true,
                low_delay: false,
                watermark_key: 0,
                has_watermark: false,
            },
        };
        if seq.width == 0 || seq.height == 0 || seq.width > 4096 || seq.height > 4096 {
            return Err(Error::invalid("svq3: no usable picture size"));
        }
        let mb_width = seq.width.div_ceil(16);
        let mb_height = seq.height.div_ceil(16);
        let mb_stride = mb_width + 1;
        let b_stride = 4 * mb_width;
        let ys = mb_width * 16 + 2 * PAD;
        let cs = mb_width * 8 + PAD;
        let picture = || Picture {
            y: vec![0; ys * (mb_height * 16 + 2 * PAD)],
            u: vec![0; cs * (mb_height * 8 + PAD)],
            v: vec![0; cs * (mb_height * 8 + PAD)],
            mb_type: vec![0; mb_stride * mb_height],
            motion_val: [vec![[0; 2]; b_stride * mb_height * 4], vec![[0; 2]; b_stride * mb_height * 4]],
            valid: false,
            pts: None,
        };
        let mut mb2br_xy = vec![0; mb_stride * (mb_height + 1)];
        for y in 0..mb_height {
            for x in 0..mb_width {
                let mb_xy = x + y * mb_stride;
                mb2br_xy[mb_xy] = 8 * (mb_xy % (2 * mb_stride));
            }
        }
        let mut block_offset = [0usize; 96];
        for i in 0..16 {
            let d = usize::from(SCAN8[i] - SCAN8[0]);
            block_offset[i] = 4 * (d & 7) + 4 * ys * (d >> 3);
            block_offset[48 + i] = 4 * (d & 7) + 8 * ys * (d >> 3);
            block_offset[16 + i] = 4 * (d & 7) + 4 * cs * (d >> 3);
            block_offset[32 + i] = block_offset[16 + i];
            block_offset[64 + i] = 4 * (d & 7) + 8 * cs * (d >> 3);
            block_offset[80 + i] = block_offset[64 + i];
        }
        Ok(Self {
            width: seq.width,
            height: seq.height,
            halfpel_flag: seq.halfpel,
            thirdpel_flag: seq.thirdpel,
            has_watermark: seq.has_watermark,
            watermark_key: seq.watermark_key,
            low_delay: seq.low_delay,
            mb_width,
            mb_height,
            mb_stride,
            mb_num: mb_width * mb_height,
            b_stride,
            h_edge_pos: (mb_width * 16) as i32,
            v_edge_pos: (mb_height * 16) as i32,
            ys,
            cs,
            y_origin: PAD * ys + PAD,
            c_origin: PAD / 2 * cs + PAD / 2,
            frames: [picture(), picture(), picture()],
            cur_pic: 0,
            last_pic: 1,
            next_pic: 2,
            slice_num: 0,
            qscale: 0,
            adaptive_quant: false,
            cbp: 0,
            frame_num: 0,
            frame_num_offset: 0,
            prev_frame_num_offset: 0,
            prev_frame_num: 0,
            pict_type: PictType::I,
            slice_type: PictType::I,
            mb_x: 0,
            mb_y: 0,
            mb_xy: 0,
            chroma_pred_mode: 0,
            intra16x16_pred_mode: 0,
            intra4x4_pred_mode_cache: [0; 40],
            intra4x4_pred_mode: vec![0; mb_stride * 2 * 8],
            mb2br_xy,
            top_samples_available: 0,
            left_samples_available: 0,
            emu: Vec::with_capacity(17 * 17),
            mv_cache: [[[0; 2]; 40]; 2],
            ref_cache: [[0; 40]; 2],
            mb: vec![0; 16 * 48 * 2],
            mb_luma_dc: [0; 16],
            non_zero_count_cache: [0; 120],
            block_offset,
            chroma_dc_qmul: dsp::chroma_dc_qmul(),
        })
    }

    /// The pool picture `idx`: luma, chroma strides, pts.
    pub fn picture(&self, idx: usize) -> (&[u8], &[u8], &[u8], Option<i64>) {
        let p = &self.frames[idx];
        (&p.y, &p.u, &p.v, p.pts)
    }

    pub fn strides(&self) -> (usize, usize, usize, usize) {
        (self.ys, self.cs, self.y_origin, self.c_origin)
    }

    /// The end of the stream: FFmpeg's last reference, not yet output.
    pub fn flush(&mut self) -> Output {
        let next = self.next_pic;
        if self.frames[next].valid && !self.low_delay {
            self.frames[next].valid = false;
            return Some(next);
        }
        None
    }

    /// A fresh start (after a seek): no reference pictures.
    pub fn reset(&mut self) {
        for f in &mut self.frames {
            f.valid = false;
        }
    }

    /// svq3_decode_block into `mb[block..]`.
    fn decode_block(mb: &mut [i16], gb: &mut GetBits, block: usize, mut index: usize, ty: usize) -> Option<()> {
        let scan: &[u8] = match ty {
            0 => &LUMA_DC_ZIGZAG_SCAN,
            1 => &ZIGZAG_SCAN,
            2 => &SVQ3_SCAN,
            _ => &CHROMA_DC_SCAN,
        };
        let intra = 3 * ty >> 2;
        let mut limit = 16 >> intra;
        while index < 16 {
            loop {
                let mut vlc = get_interleaved_ue_golomb(gb)?;
                if vlc == 0 {
                    break;
                }
                let sign: i32 = if vlc & 1 != 0 { 0 } else { -1 };
                vlc = (vlc + 1) >> 1;
                let (run, level): (usize, i32) = if ty == 3 {
                    if vlc < 3 {
                        (0, vlc as i32)
                    } else if vlc < 4 {
                        (1, 1)
                    } else {
                        let run = (vlc & 3) as usize;
                        (run, ((vlc + 9) >> 2) as i32 - run as i32)
                    }
                } else if vlc < 16 {
                    let (r, l) = SVQ3_DCT_TABLES[intra][vlc as usize];
                    (usize::from(r), i32::from(l))
                } else if intra != 0 {
                    let run = (vlc & 7) as usize;
                    let add = match run {
                        0 => 8,
                        1 => 2,
                        2..=4 => 0,
                        _ => -1,
                    };
                    (run, (vlc >> 3) as i32 + add)
                } else {
                    let run = (vlc & 15) as usize;
                    let add = match run {
                        0 => 4,
                        1 | 2 => 2,
                        3..=9 => 1,
                        _ => 0,
                    };
                    (run, (vlc >> 4) as i32 + add)
                };
                index += run;
                if index >= limit {
                    return None;
                }
                mb[block + usize::from(scan[index])] = ((level ^ sign) - sign) as i16;
                index += 1;
            }
            if ty != 2 {
                break;
            }
            index = limit;
            limit += 8;
        }
        Some(())
    }

    /// svq3_fetch_diagonal_mv.
    fn fetch_diagonal_mv(&self, i: usize, list: usize, part_width: usize) -> ([i16; 2], i8) {
        let topright_ref = self.ref_cache[list][i - 8 + part_width];
        if topright_ref != PART_NOT_AVAILABLE {
            (self.mv_cache[list][i - 8 + part_width], topright_ref)
        } else {
            (self.mv_cache[list][i - 8 - 1], self.ref_cache[list][i - 8 - 1])
        }
    }

    /// svq3_pred_motion.
    fn pred_motion(&self, n: usize, part_width: usize, list: usize, r: i8) -> (i32, i32) {
        let index8 = usize::from(SCAN8[n]);
        let top_ref = self.ref_cache[list][index8 - 8];
        let left_ref = self.ref_cache[list][index8 - 1];
        let a = self.mv_cache[list][index8 - 1];
        let b = self.mv_cache[list][index8 - 8];
        let (c, diagonal_ref) = self.fetch_diagonal_mv(index8, list, part_width);
        let mid = |k: usize| mid_pred(i32::from(a[k]), i32::from(b[k]), i32::from(c[k]));
        let matches = i32::from(diagonal_ref == r) + i32::from(top_ref == r) + i32::from(left_ref == r);
        if matches > 1 {
            (mid(0), mid(1))
        } else if matches == 1 {
            let v = if left_ref == r {
                a
            } else if top_ref == r {
                b
            } else {
                c
            };
            (i32::from(v[0]), i32::from(v[1]))
        } else if top_ref == PART_NOT_AVAILABLE && diagonal_ref == PART_NOT_AVAILABLE && left_ref != PART_NOT_AVAILABLE {
            (i32::from(a[0]), i32::from(a[1]))
        } else {
            (mid(0), mid(1))
        }
    }

    /// svq3_mc_dir_part.
    #[allow(clippy::too_many_arguments)]
    fn mc_dir_part(&mut self, x: usize, y: usize, width: usize, height: usize, mx: i32, my: i32, dxy: usize, thirdpel: bool, dir: usize, avg: bool) {
        let refi = if dir == 0 { self.last_pic } else { self.next_pic };
        let (ys, cs) = (self.ys, self.cs);
        let (w, h) = (width as i32, height as i32);
        let mut mx = mx + x as i32;
        let mut my = my + y as i32;
        let emu = mx < 0 || mx >= self.h_edge_pos - w - 1 || my < 0 || my >= self.v_edge_pos - h - 1;
        if emu {
            mx = av_clip(mx, -16, self.h_edge_pos - w + 15);
            my = av_clip(my, -16, self.v_edge_pos - h + 15);
        }
        let (cur, src) = pair(&mut self.frames, self.cur_pic, refi);
        let dst = self.y_origin + x + y * ys;
        if emu {
            dsp::emulated_edge(&mut self.emu, &src.y, self.y_origin, ys, width + 1, height + 1, mx, my, self.h_edge_pos, self.v_edge_pos);
            dsp::mc(&mut cur.y, dst, ys, &self.emu, 0, width + 1, width, height, dxy, thirdpel, avg);
        } else {
            let at = self.y_origin + mx as usize + my as usize * ys;
            dsp::mc(&mut cur.y, dst, ys, &src.y, at, ys, width, height, dxy, thirdpel, avg);
        }
        // chroma
        let mx = (mx + i32::from(mx < x as i32)) >> 1;
        let my = (my + i32::from(my < y as i32)) >> 1;
        let (cw, ch) = (width >> 1, height >> 1);
        let dst = self.c_origin + (x >> 1) + (y >> 1) * cs;
        for (d, s) in [(&mut cur.u, &src.u), (&mut cur.v, &src.v)] {
            if emu {
                dsp::emulated_edge(&mut self.emu, s, self.c_origin, cs, cw + 1, ch + 1, mx, my, self.h_edge_pos >> 1, self.v_edge_pos >> 1);
                dsp::mc(d, dst, cs, &self.emu, 0, cw + 1, cw, ch, dxy, thirdpel, avg);
            } else {
                let at = self.c_origin + mx as usize + my as usize * cs;
                dsp::mc(d, dst, cs, s, at, cs, cw, ch, dxy, thirdpel, avg);
            }
        }
    }

    /// svq3_mc_dir.
    fn mc_dir(&mut self, gb: &mut GetBits, size: u32, mode: u32, dir: usize, avg: bool) -> Option<()> {
        let part_width: usize = if size & 5 == 4 { 4 } else { 16 >> (size & 1) };
        let part_height: usize = 16 >> ((size + 1) / 3);
        let extra_width: i32 = if mode == PREDICT_MODE { -16 * 6 } else { 0 };
        let h_edge_pos = 6 * (self.h_edge_pos - part_width as i32) - extra_width;
        let v_edge_pos = 6 * (self.v_edge_pos - part_height as i32) - extra_width;
        for i in (0..16).step_by(part_height) {
            for j in (0..16).step_by(part_width) {
                let b_xy = (4 * self.mb_x + (j >> 2)) + (4 * self.mb_y + (i >> 2)) * self.b_stride;
                let x = 16 * self.mb_x + j;
                let y = 16 * self.mb_y + i;
                let k = (j >> 2 & 1) + (i >> 1 & 2) + (j >> 1 & 4) + (i & 8);
                let (mut mx, mut my) = if mode != PREDICT_MODE {
                    self.pred_motion(k, part_width >> 2, dir, 1)
                } else {
                    let mv = self.frames[self.next_pic].motion_val[0][b_xy];
                    let (mx, my) = (i32::from(mv[0]) * 2, i32::from(mv[1]) * 2);
                    // FFmpeg rejects B pictures whose offsets would be 0.
                    let (num, den) = if dir == 0 {
                        (self.frame_num_offset, self.prev_frame_num_offset)
                    } else {
                        (self.frame_num_offset - self.prev_frame_num_offset, self.prev_frame_num_offset)
                    };
                    ((mx.wrapping_mul(num) / den + 1) >> 1, (my.wrapping_mul(num) / den + 1) >> 1)
                };
                mx = av_clip(mx, extra_width - 6 * x as i32, h_edge_pos - 6 * x as i32);
                my = av_clip(my, extra_width - 6 * y as i32, v_edge_pos - 6 * y as i32);
                let (dx, dy) = if mode == PREDICT_MODE {
                    (0, 0)
                } else {
                    let dy = get_interleaved_se_golomb(gb)?;
                    let dx = get_interleaved_se_golomb(gb)?;
                    if dx != i32::from(dx as i16) || dy != i32::from(dy as i16) {
                        return None;
                    }
                    (dx, dy)
                };
                if mode == THIRDPEL_MODE {
                    mx = ((mx + 1) >> 1) + dx;
                    my = ((my + 1) >> 1) + dy;
                    let fx = ((mx as u32).wrapping_add(0x30000) / 3).wrapping_sub(0x10000) as i32;
                    let fy = ((my as u32).wrapping_add(0x30000) / 3).wrapping_sub(0x10000) as i32;
                    let dxy = ((mx - 3 * fx) + 4 * (my - 3 * fy)) as usize;
                    self.mc_dir_part(x, y, part_width, part_height, fx, fy, dxy, true, dir, avg);
                    mx += mx;
                    my += my;
                } else if mode == HALFPEL_MODE || mode == PREDICT_MODE {
                    mx = ((mx as u32).wrapping_add(1 + 0x30000) / 3).wrapping_add(dx as u32).wrapping_sub(0x10000) as i32;
                    my = ((my as u32).wrapping_add(1 + 0x30000) / 3).wrapping_add(dy as u32).wrapping_sub(0x10000) as i32;
                    let dxy = ((mx & 1) + 2 * (my & 1)) as usize;
                    self.mc_dir_part(x, y, part_width, part_height, mx >> 1, my >> 1, dxy, false, dir, avg);
                    mx *= 3;
                    my *= 3;
                } else {
                    mx = ((mx as u32).wrapping_add(3 + 0x60000) / 6).wrapping_add(dx as u32).wrapping_sub(0x10000) as i32;
                    my = ((my as u32).wrapping_add(3 + 0x60000) / 6).wrapping_add(dy as u32).wrapping_sub(0x10000) as i32;
                    self.mc_dir_part(x, y, part_width, part_height, mx, my, 0, false, dir, avg);
                    mx *= 6;
                    my *= 6;
                }
                let mv = [mx as i16, my as i16];
                if mode != PREDICT_MODE {
                    let s8 = usize::from(SCAN8[k]);
                    if part_height == 8 && i < 8 {
                        self.mv_cache[dir][s8 + 8] = mv;
                        if part_width == 8 && j < 8 {
                            self.mv_cache[dir][s8 + 1 + 8] = mv;
                        }
                    }
                    if part_width == 8 && j < 8 {
                        self.mv_cache[dir][s8 + 1] = mv;
                    }
                    if part_width == 4 || part_height == 4 {
                        self.mv_cache[dir][s8] = mv;
                    }
                }
                let mvs = &mut self.frames[self.cur_pic].motion_val[dir];
                for r in 0..part_height >> 2 {
                    let at = b_xy + r * self.b_stride;
                    mvs[at..at + (part_width >> 2)].fill(mv);
                }
            }
        }
        Some(())
    }

    /// The four motion vectors of each row of this macroblock in list `m`
    /// zeroed.
    fn zero_motion(&mut self, m: usize) {
        let b_xy = 4 * self.mb_x + 4 * self.mb_y * self.b_stride;
        for i in 0..4 {
            let at = b_xy + i * self.b_stride;
            self.frames[self.cur_pic].motion_val[m][at..at + 4].fill([0, 0]);
        }
    }

    /// svq3_decode_mb.
    fn decode_mb(&mut self, gb: &mut GetBits, mb_type: u32) -> Option<()> {
        let mb_xy = self.mb_xy;
        let b_xy = 4 * self.mb_x + 4 * self.mb_y * self.b_stride;
        let mut cbp: u32 = 0;
        let scan0 = usize::from(SCAN8[0]);
        self.top_samples_available = if self.mb_y == 0 { 0x33FF } else { 0xFFFF };
        self.left_samples_available = if self.mb_x == 0 { 0x5F5F } else { 0xFFFF };

        let mb_type = if mb_type == 0 {
            // SKIP
            if self.pict_type == PictType::P || self.frames[self.next_pic].mb_type[mb_xy] == NO_MOTION {
                self.mc_dir_part(16 * self.mb_x, 16 * self.mb_y, 16, 16, 0, 0, 0, false, 0, false);
                if self.pict_type == PictType::B {
                    self.mc_dir_part(16 * self.mb_x, 16 * self.mb_y, 16, 16, 0, 0, 0, false, 1, true);
                }
                MB_TYPE_SKIP
            } else {
                let size = self.frames[self.next_pic].mb_type[mb_xy].min(6);
                self.mc_dir(gb, size, PREDICT_MODE, 0, false)?;
                self.mc_dir(gb, size, PREDICT_MODE, 1, true)?;
                MB_TYPE_16X16
            }
        } else if mb_type < 8 {
            // INTER
            let mode = if self.thirdpel_flag && self.halfpel_flag == (gb.get_bit()? == 0) {
                THIRDPEL_MODE
            } else if self.halfpel_flag && self.thirdpel_flag == (gb.get_bit()? == 0) {
                HALFPEL_MODE
            } else {
                FULLPEL_MODE
            };
            let lists = if self.pict_type == PictType::B { 2 } else { 1 };
            for m in 0..lists {
                let mv = &self.frames[self.cur_pic].motion_val[m];
                if self.mb_x > 0 && self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - 1] + 6] != -1 {
                    for i in 0..4 {
                        self.mv_cache[m][scan0 - 1 + i * 8] = mv[b_xy - 1 + i * self.b_stride];
                    }
                } else {
                    for i in 0..4 {
                        self.mv_cache[m][scan0 - 1 + i * 8] = [0, 0];
                    }
                }
                if self.mb_y > 0 {
                    let above = b_xy - self.b_stride;
                    self.mv_cache[m][scan0 - 8..scan0 - 4].copy_from_slice(&mv[above..above + 4]);
                    let r = if self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - self.mb_stride]] == -1 { PART_NOT_AVAILABLE } else { 1 };
                    self.ref_cache[m][scan0 - 8..scan0 - 4].fill(r);
                    if self.mb_x < self.mb_width - 1 {
                        self.mv_cache[m][scan0 + 4 - 8] = mv[above + 4];
                        self.ref_cache[m][scan0 + 4 - 8] = if self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - self.mb_stride + 1] + 6] == -1
                            || self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - self.mb_stride]] == -1
                        {
                            PART_NOT_AVAILABLE
                        } else {
                            1
                        };
                    } else {
                        self.ref_cache[m][scan0 + 4 - 8] = PART_NOT_AVAILABLE;
                    }
                    if self.mb_x > 0 {
                        self.mv_cache[m][scan0 - 1 - 8] = mv[above - 1];
                        self.ref_cache[m][scan0 - 1 - 8] =
                            if self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - self.mb_stride - 1] + 3] == -1 { PART_NOT_AVAILABLE } else { 1 };
                    } else {
                        self.ref_cache[m][scan0 - 1 - 8] = PART_NOT_AVAILABLE;
                    }
                } else {
                    self.ref_cache[m][scan0 - 9..scan0 - 1].fill(PART_NOT_AVAILABLE);
                }
            }
            if self.pict_type == PictType::P {
                self.mc_dir(gb, mb_type - 1, mode, 0, false)?;
            } else {
                if mb_type != 2 {
                    self.mc_dir(gb, 0, mode, 0, false)?;
                } else {
                    self.zero_motion(0);
                }
                if mb_type != 1 {
                    self.mc_dir(gb, 0, mode, 1, mb_type == 3)?;
                } else {
                    self.zero_motion(1);
                }
            }
            MB_TYPE_16X16
        } else if mb_type == 8 || mb_type == 33 {
            // INTRA4x4
            let i4x4 = self.mb2br_xy[mb_xy];
            self.intra4x4_pred_mode_cache = [-1; 40];
            if mb_type == 8 {
                if self.mb_x > 0 {
                    for i in 0..4 {
                        self.intra4x4_pred_mode_cache[scan0 - 1 + i * 8] = self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - 1] + 6 - i];
                    }
                    if self.intra4x4_pred_mode_cache[scan0 - 1] == -1 {
                        self.left_samples_available = 0x5F5F;
                    }
                }
                if self.mb_y > 0 {
                    let above = self.mb2br_xy[mb_xy - self.mb_stride];
                    for k in 0..4 {
                        self.intra4x4_pred_mode_cache[4 + k] = self.intra4x4_pred_mode[above + k];
                    }
                    if self.intra4x4_pred_mode_cache[4] == -1 {
                        self.top_samples_available = 0x33FF;
                    }
                }
                for i in (0..16).step_by(2) {
                    let vlc = get_interleaved_ue_golomb(gb)?;
                    if vlc >= 25 {
                        return None;
                    }
                    let left = usize::from(SCAN8[i]) - 1;
                    let top = usize::from(SCAN8[i]) - 8;
                    let c = &mut self.intra4x4_pred_mode_cache;
                    let pick = |t: i8, l: i8, e: u8| SVQ3_PRED_1.get((t + 1) as usize)?.get((l + 1) as usize).map(|r| r[usize::from(e)]);
                    c[left + 1] = pick(c[top], c[left], SVQ3_PRED_0[vlc as usize][0])?;
                    c[left + 2] = pick(c[top + 1], c[left + 1], SVQ3_PRED_0[vlc as usize][1])?;
                    if c[left + 1] == -1 || c[left + 2] == -1 {
                        return None;
                    }
                }
            } else {
                for i in 0..4 {
                    self.intra4x4_pred_mode_cache[scan0 + 8 * i..scan0 + 8 * i + 4].fill(dsp::DC_PRED);
                }
            }
            let c = self.intra4x4_pred_mode_cache;
            self.intra4x4_pred_mode[i4x4..i4x4 + 4].copy_from_slice(&c[36..40]);
            self.intra4x4_pred_mode[i4x4 + 4] = c[7 + 8 * 3];
            self.intra4x4_pred_mode[i4x4 + 5] = c[7 + 8 * 2];
            self.intra4x4_pred_mode[i4x4 + 6] = c[7 + 8];
            if mb_type == 8 {
                // svq3.c does not check the result.
                dsp::check_intra4x4_pred_mode(&mut self.intra4x4_pred_mode_cache, self.top_samples_available, self.left_samples_available);
                self.top_samples_available = if self.mb_y == 0 { 0x33FF } else { 0xFFFF };
                self.left_samples_available = if self.mb_x == 0 { 0x5F5F } else { 0xFFFF };
            } else {
                for i in 0..4 {
                    self.intra4x4_pred_mode_cache[scan0 + 8 * i..scan0 + 8 * i + 4].fill(dsp::DC_128_PRED);
                }
                self.top_samples_available = 0x33FF;
                self.left_samples_available = 0x5F5F;
            }
            MB_TYPE_INTRA4X4
        } else {
            // INTRA16x16
            let (pred_mode, mb_cbp) = I_MB_TYPE_INFO[(mb_type - 8) as usize];
            let dir = i32::from(pred_mode);
            let dir = (dir >> 1) ^ (3 * (dir & 1)) ^ 1;
            self.intra16x16_pred_mode = dsp::check_intra_pred_mode(self.top_samples_available, self.left_samples_available, dir, false)?;
            cbp = mb_cbp as u32;
            MB_TYPE_INTRA16X16
        };

        if !is_inter(mb_type) && self.pict_type != PictType::I {
            self.zero_motion(0);
            if self.pict_type == PictType::B {
                self.zero_motion(1);
            }
        }
        if mb_type & MB_TYPE_INTRA4X4 == 0 {
            let at = self.mb2br_xy[mb_xy];
            self.intra4x4_pred_mode[at..at + 8].fill(dsp::DC_PRED);
        }
        let skip = mb_type & MB_TYPE_SKIP != 0;
        if !skip || self.pict_type == PictType::B {
            self.non_zero_count_cache[8..8 + 14 * 8].fill(0);
        }
        if mb_type & MB_TYPE_INTRA16X16 == 0 && (!skip || self.pict_type == PictType::B) {
            let vlc = get_interleaved_ue_golomb(gb)?;
            if vlc >= 48 {
                return None;
            }
            cbp = u32::from(if is_intra(mb_type) { GOLOMB_TO_INTRA4X4_CBP[vlc as usize] } else { GOLOMB_TO_INTER_CBP[vlc as usize] });
        }
        if mb_type & MB_TYPE_INTRA16X16 != 0 || (self.pict_type != PictType::I && self.adaptive_quant && cbp != 0) {
            self.qscale = self.qscale.wrapping_add(get_interleaved_se_golomb(gb)?);
            if !(0..=31).contains(&self.qscale) {
                return None;
            }
        }
        if mb_type & MB_TYPE_INTRA16X16 != 0 {
            self.mb_luma_dc = [0; 16];
            Self::decode_block(&mut self.mb_luma_dc, gb, 0, 0, 1)?;
        }
        if cbp != 0 {
            let index = usize::from(mb_type & MB_TYPE_INTRA16X16 != 0);
            let ty = if self.qscale < 24 && mb_type & MB_TYPE_INTRA4X4 != 0 { 2 } else { 1 };
            for i in 0..4 {
                if cbp & (1 << i) != 0 {
                    for j in 0..4 {
                        let k = if index != 0 { (j & 1) + 2 * (i & 1) + 2 * (j & 2) + 4 * (i & 2) } else { 4 * i + j };
                        self.non_zero_count_cache[usize::from(SCAN8[k])] = 1;
                        Self::decode_block(&mut self.mb, gb, 16 * k, index, ty)?;
                    }
                }
            }
            if cbp & 0x30 != 0 {
                for i in 1..3 {
                    Self::decode_block(&mut self.mb, gb, 16 * 16 * i, 0, 3)?;
                }
                if cbp & 0x20 != 0 {
                    for i in 1..3 {
                        for j in 0..4 {
                            let k = 16 * i + j;
                            self.non_zero_count_cache[usize::from(SCAN8[k])] = 1;
                            Self::decode_block(&mut self.mb, gb, 16 * k, 1, 1)?;
                        }
                    }
                }
            }
        }
        self.cbp = cbp;
        self.frames[self.cur_pic].mb_type[mb_xy] = mb_type;
        if is_intra(mb_type) {
            self.chroma_pred_mode = dsp::check_intra_pred_mode(self.top_samples_available, self.left_samples_available, dsp::DC_PRED8X8, true)?;
        }
        Some(())
    }

    /// hl_decode_mb.
    fn hl_decode_mb(&mut self) {
        let mb_type = self.frames[self.cur_pic].mb_type[self.mb_xy];
        let (ys, cs) = (self.ys, self.cs);
        let dest_y = self.y_origin + (self.mb_x + self.mb_y * ys) * 16;
        let dest_c = self.c_origin + self.mb_x * 8 + self.mb_y * cs * 8;
        let qscale = self.qscale as usize;
        let pic = &mut self.frames[self.cur_pic];
        let mb = &mut self.mb;
        let nnz = &self.non_zero_count_cache;
        let bo = &self.block_offset;
        if is_intra(mb_type) {
            dsp::pred8x8(self.chroma_pred_mode, &mut pic.u, dest_c, cs);
            dsp::pred8x8(self.chroma_pred_mode, &mut pic.v, dest_c, cs);
            if mb_type & MB_TYPE_INTRA4X4 != 0 {
                for i in 0..16 {
                    let ptr = dest_y + bo[i];
                    dsp::pred4x4(self.intra4x4_pred_mode_cache[usize::from(SCAN8[i])], &mut pic.y, ptr, ys);
                    if nnz[usize::from(SCAN8[i])] != 0 {
                        dsp::add_idct(&mut pic.y, ptr, ys, &mut mb[16 * i..16 * i + 16], qscale, 0);
                    }
                }
            } else {
                dsp::pred16x16(self.intra16x16_pred_mode, &mut pic.y, dest_y, ys);
                dsp::luma_dc_dequant_idct(mb, &self.mb_luma_dc, qscale);
            }
        }
        if mb_type & MB_TYPE_INTRA4X4 == 0 {
            let dc = i32::from(is_intra(mb_type));
            for i in 0..16 {
                if nnz[usize::from(SCAN8[i])] != 0 || mb[i * 16] != 0 {
                    dsp::add_idct(&mut pic.y, dest_y + bo[i], ys, &mut mb[16 * i..16 * i + 16], qscale, dc);
                }
            }
        }
        if self.cbp & 0x30 != 0 {
            dsp::chroma_dc_dequant_idct(&mut mb[256..], self.chroma_dc_qmul);
            dsp::chroma_dc_dequant_idct(&mut mb[512..], self.chroma_dc_qmul);
            let qp = usize::from(CHROMA_QP[qscale + 12]) - 12;
            for (j, plane) in [(1usize, &mut pic.u), (2, &mut pic.v)] {
                for i in j * 16..j * 16 + 4 {
                    if nnz[usize::from(SCAN8[i])] != 0 || mb[i * 16] != 0 {
                        dsp::add_idct(plane, dest_c + bo[i], cs, &mut mb[16 * i..16 * i + 16], qp, 2);
                    }
                }
            }
        }
    }

    /// svq3_decode_slice_header: the slice at `gb`'s position copied into
    /// `slice` (watermark removed); its reader positioned after the header.
    fn decode_slice_header<'s>(&mut self, gb: &mut GetBits, slice: &'s mut Vec<u8>) -> Option<GetBits<'s>> {
        let mb_xy = self.mb_xy;
        let header = gb.get_bits(8)?;
        if (header & 0x9F != 1 && header & 0x9F != 2) || header & 0x60 == 0 {
            return None;
        }
        let length = (header >> 5 & 3) as usize;
        let slice_length = gb.show_bits(8 * length)? as usize;
        let slice_bits = slice_length * 8;
        let slice_bytes = slice_length + length - 1;
        gb.skip(8)?;
        if slice_bytes * 8 > gb.bits_left() {
            return None;
        }
        let start = gb.byte_index();
        slice.clear();
        slice.extend_from_slice(&gb.data()[start..start + slice_bytes]);
        if length > 1 {
            slice.copy_within(slice_length..slice_length + length - 1, 0);
        }
        if self.watermark_key != 0 {
            // FFmpeg's slice buffer is zero-padded past the slice.
            if slice.len() < 5 {
                slice.resize(5, 0);
            }
            let h = u32::from_le_bytes([slice[1], slice[2], slice[3], slice[4]]) ^ self.watermark_key;
            slice[1..5].copy_from_slice(&h.to_le_bytes());
        }
        gb.skip(slice_bytes * 8)?;
        let mut gs = GetBits::with_size(slice, slice_bits)?;

        let slice_id = get_interleaved_ue_golomb(&mut gs)?;
        self.slice_type = match slice_id {
            0 => PictType::P,
            1 => PictType::B,
            2 => PictType::I,
            _ => return None,
        };
        if header & 0x9F == 2 {
            let i = if self.mb_num < 64 { 6 } else { 1 + (self.mb_num - 1).ilog2() as usize };
            gs.skip(i)?;
        } else if gs.get_bit()? == 1 {
            // media key encryption, unsupported in FFmpeg too
            return None;
        }
        self.slice_num = gs.get_bits(8)? as i32;
        self.qscale = gs.get_bits(5)? as i32;
        self.adaptive_quant = gs.get_bit()? == 1;
        gs.skip(1)?;
        if self.has_watermark {
            gs.skip(1)?;
        }
        gs.skip(3)?;
        skip_1stop_8data_bits(&mut gs)?;

        // reset intra predictors and invalidate motion vector references
        if self.mb_x > 0 {
            let at = self.mb2br_xy[mb_xy - 1] + 3;
            self.intra4x4_pred_mode[at..at + 4].fill(-1);
            let at = self.mb2br_xy[mb_xy - self.mb_x];
            self.intra4x4_pred_mode[at..at + 8 * self.mb_x].fill(-1);
        }
        if self.mb_y > 0 {
            let at = self.mb2br_xy[mb_xy - self.mb_stride];
            self.intra4x4_pred_mode[at..at + 8 * (self.mb_width - self.mb_x)].fill(-1);
            if self.mb_x > 0 {
                self.intra4x4_pred_mode[self.mb2br_xy[mb_xy - self.mb_stride - 1] + 3] = -1;
            }
        }
        Some(gs)
    }

    /// svq3_decode_frame on a non-empty packet: the pool picture to output.
    pub fn decode(&mut self, data: &[u8], pts: Option<i64>) -> Result<Output> {
        let bad = || Error::invalid("svq3: invalid picture");
        self.mb_x = 0;
        self.mb_y = 0;
        self.mb_xy = 0;
        let mut gb = GetBits::new(data).ok_or_else(bad)?;
        let mut slice = Vec::new();
        let mut gs = self.decode_slice_header(&mut gb, &mut slice).ok_or_else(bad)?;
        if data.len() < self.mb_width * self.mb_height / 8 {
            return Err(bad());
        }
        self.pict_type = self.slice_type;
        if self.pict_type != PictType::B {
            std::mem::swap(&mut self.next_pic, &mut self.last_pic);
        }
        let cur = self.cur_pic;
        self.frames[cur].valid = true;
        self.frames[cur].pts = pts;
        if self.pict_type != PictType::I {
            let dummy = |p: &mut Picture| {
                p.y.fill(0);
                p.u.fill(0x80);
                p.v.fill(0x80);
                p.valid = true;
            };
            if !self.frames[self.last_pic].valid {
                dummy(&mut self.frames[self.last_pic]);
            }
            if self.pict_type == PictType::B && !self.frames[self.next_pic].valid {
                dummy(&mut self.frames[self.next_pic]);
            }
        }
        if self.pict_type == PictType::B {
            self.frame_num_offset = self.slice_num - self.prev_frame_num;
            if self.frame_num_offset < 0 {
                self.frame_num_offset += 256;
            }
            if self.frame_num_offset == 0 || self.frame_num_offset >= self.prev_frame_num_offset {
                return Err(Error::invalid("svq3: error in B-frame picture id"));
            }
        } else {
            self.prev_frame_num = self.frame_num;
            self.frame_num = self.slice_num;
            self.prev_frame_num_offset = self.frame_num - self.prev_frame_num;
            if self.prev_frame_num_offset < 0 {
                self.prev_frame_num_offset += 256;
            }
        }
        let scan0 = usize::from(SCAN8[0]);
        for m in 0..2 {
            for i in 0..4 {
                self.ref_cache[m][scan0 + 8 * i - 1..scan0 + 8 * i + 4].fill(1);
                if i < 3 {
                    self.ref_cache[m][scan0 + 8 * i + 4] = PART_NOT_AVAILABLE;
                }
            }
        }
        for mb_y in 0..self.mb_height {
            for mb_x in 0..self.mb_width {
                self.mb_y = mb_y;
                self.mb_x = mb_x;
                self.mb_xy = mb_x + mb_y * self.mb_stride;
                if gs.bits_left() <= 7 && (gs.position() & 7 == 0 || gs.show_bits(gs.bits_left() & 7) == Some(0)) {
                    drop(gs);
                    gs = self.decode_slice_header(&mut gb, &mut slice).ok_or_else(bad)?;
                }
                let mut mb_type = get_interleaved_ue_golomb(&mut gs).ok_or_else(bad)?;
                if self.pict_type == PictType::I {
                    mb_type += 8;
                } else if self.pict_type == PictType::B && mb_type >= 4 {
                    mb_type += 4;
                }
                if mb_type > 33 || self.decode_mb(&mut gs, mb_type).is_none() {
                    return Err(Error::invalid(format!("svq3: error while decoding MB {mb_x} {mb_y}")));
                }
                if mb_type != 0 || self.cbp != 0 {
                    self.hl_decode_mb();
                }
                if self.pict_type != PictType::B && !self.low_delay {
                    self.frames[cur].mb_type[self.mb_xy] = if self.pict_type == PictType::P && mb_type < 8 { mb_type.wrapping_sub(1) } else { NO_MOTION };
                }
            }
        }
        let out = if self.pict_type == PictType::B || self.low_delay {
            Some(cur)
        } else if self.frames[self.last_pic].valid {
            Some(self.last_pic)
        } else {
            None
        };
        let got = self.frames[self.last_pic].valid || self.low_delay;
        if self.pict_type != PictType::B {
            std::mem::swap(&mut self.cur_pic, &mut self.next_pic);
        } else {
            // av_frame_unref: the B picture's buffer is scratch again (its
            // samples stay readable for the caller's copy).
            self.frames[cur].valid = false;
        }
        Ok(out.filter(|_| got))
    }
}

/// av_clip.
fn av_clip(a: i32, lo: i32, hi: i32) -> i32 {
    if a < lo {
        lo
    } else if a > hi {
        hi
    } else {
        a
    }
}

fn mid_pred(a: i32, b: i32, c: i32) -> i32 {
    a.max(b).min(a.min(b).max(c))
}

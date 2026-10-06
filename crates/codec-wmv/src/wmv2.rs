//! WMV2 (Windows Media Video 8) specifics on top of the MS-MPEG-4 core.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/wmv2dec.c` (picture
//! headers, MB skip map, macroblock layer, adaptive block transform, the
//! quarter-pel "mspel" motion compensation) and `wmv2.h`. LGPL-2.1-or-later.

use oxideav_core::{Error, Result};

use crate::bits::BitReader;
use crate::idct;
use crate::mpv::{self, SrcBlock};
use crate::msmpeg4::{decode_ms_motion, Header, MsDecoder, MB_TYPE_SKIP, PICT_I, PICT_P, TABLES};
use crate::tables::{WMV2_SCANTABLE_A, WMV2_SCANTABLE_B};

const SKIP_TYPE_NONE: u32 = 0;
const SKIP_TYPE_MPEG: u32 = 1;
const SKIP_TYPE_ROW: u32 = 2;
const SKIP_TYPE_COL: u32 = 3;

/// `wmv2_get_cbp_table_index`.
fn cbp_table_index(qscale: i32, cbp_index: usize) -> usize {
    const MAP: [[usize; 3]; 3] = [[0, 2, 1], [1, 0, 2], [2, 1, 0]];
    MAP[(qscale > 10) as usize + (qscale > 20) as usize][cbp_index]
}

impl MsDecoder {
    /// `decode_ext_header` (32-bit extradata).
    pub(crate) fn wmv2_decode_ext_header(&mut self, extradata: &[u8]) {
        if extradata.len() < 4 {
            return;
        }
        let mut gb = BitReader::new(&extradata[..4]);
        let _fps = gb.read(5);
        self.bit_rate = gb.read(11) * 1024;
        self.w2.mspel_bit = gb.read_bit() != 0;
        self.loop_filter = gb.read_bit() != 0;
        self.w2.abt_flag = gb.read_bit() != 0;
        self.w2.j_type_bit = gb.read_bit() != 0;
        self.w2.top_left_mv_flag = gb.read_bit() != 0;
        self.w2.per_mb_rl_bit = gb.read_bit() != 0;
        let code = gb.read(3) as usize;
        if code == 0 {
            return;
        }
        self.slice_height = self.mb_height / code;
    }

    /// `wmv2_decode_picture_header`.
    pub(crate) fn wmv2_decode_picture_header(&mut self, br: &mut BitReader) -> Result<Header> {
        let pict_type = br.read_bit() as u8 + 1;
        if pict_type == PICT_I {
            let _code = br.read(7);
        }
        let q = br.read(5) as i32;
        if q <= 0 {
            return Err(Error::invalid("wmv2: invalid qscale"));
        }
        self.pict_type = pict_type;
        self.qscale = q;

        if pict_type != PICT_I && br.peek(1) != 0 {
            let mut gb = br.clone();
            let skip_type = gb.read(2);
            let mut run = if skip_type == SKIP_TYPE_COL { self.mb_width } else { self.mb_height } as i64;
            while run > 0 {
                let block = run.min(25) as u32;
                if gb.read(block) as u64 + 1 != 1u64 << block {
                    break;
                }
                run -= block as i64;
            }
            if run == 0 {
                return Ok(Header::Skipped);
            }
        }
        Ok(Header::Ok)
    }

    /// `parse_mb_skip`.
    fn parse_mb_skip(&mut self, br: &mut BitReader) -> Result<()> {
        let (w, h, st) = (self.mb_width, self.mb_height, self.mb_stride);
        let skip_type = br.read(2);
        match skip_type {
            SKIP_TYPE_NONE => {
                for y in 0..h {
                    for x in 0..w {
                        self.mb_type[y * st + x] = 0;
                    }
                }
            }
            SKIP_TYPE_MPEG => {
                if br.bits_left() < (h * w) as i64 {
                    return Err(Error::invalid("wmv2: skip map truncated"));
                }
                for y in 0..h {
                    for x in 0..w {
                        self.mb_type[y * st + x] = if br.read_bit() != 0 { MB_TYPE_SKIP } else { 0 };
                    }
                }
            }
            SKIP_TYPE_ROW => {
                for y in 0..h {
                    if br.bits_left() < 1 {
                        return Err(Error::invalid("wmv2: skip map truncated"));
                    }
                    if br.read_bit() != 0 {
                        for x in 0..w {
                            self.mb_type[y * st + x] = MB_TYPE_SKIP;
                        }
                    } else {
                        if br.bits_left() < w as i64 {
                            return Err(Error::invalid("wmv2: skip map truncated"));
                        }
                        for x in 0..w {
                            self.mb_type[y * st + x] = if br.read_bit() != 0 { MB_TYPE_SKIP } else { 0 };
                        }
                    }
                }
            }
            _ => {
                for x in 0..w {
                    if br.bits_left() < 1 {
                        return Err(Error::invalid("wmv2: skip map truncated"));
                    }
                    if br.read_bit() != 0 {
                        for y in 0..h {
                            self.mb_type[y * st + x] = MB_TYPE_SKIP;
                        }
                    } else {
                        if br.bits_left() < h as i64 {
                            return Err(Error::invalid("wmv2: skip map truncated"));
                        }
                        for y in 0..h {
                            self.mb_type[y * st + x] = if br.read_bit() != 0 { MB_TYPE_SKIP } else { 0 };
                        }
                    }
                }
            }
        }
        let mut coded = 0i64;
        for y in 0..h {
            for x in 0..w {
                coded += (self.mb_type[y * st + x] & MB_TYPE_SKIP == 0) as i64;
            }
        }
        if coded > br.bits_left() {
            return Err(Error::invalid("wmv2: skip map exceeds the packet"));
        }
        Ok(())
    }

    /// `ff_wmv2_decode_secondary_picture_header`; returns true for an
    /// IntraX8 (J-type) picture, which it decodes completely.
    pub(crate) fn wmv2_decode_secondary_picture_header(&mut self, br: &mut BitReader) -> Result<bool> {
        if self.pict_type == PICT_I {
            for v in self.mb_type.iter_mut() {
                *v = 0;
            }
            self.w2.j_type = if self.w2.j_type_bit { br.read_bit() != 0 } else { false };
            if !self.w2.j_type {
                self.per_mb_rl_table = if self.w2.per_mb_rl_bit { br.read_bit() != 0 } else { false };
                if !self.per_mb_rl_table {
                    self.rl_chroma_table_index = br.decode012() as usize;
                    self.rl_table_index = br.decode012() as usize;
                }
                self.dc_table_index = br.read_bit() as usize;
                if br.bits_left() * 8 < (self.mb_width * self.mb_height) as i64 {
                    return Err(Error::invalid("wmv2: frame too small"));
                }
            }
            self.no_rounding = true;
        } else {
            self.w2.j_type = false;
            self.parse_mb_skip(br)?;
            let cbp_index = br.decode012() as usize;
            self.w2.cbp_table_index = cbp_table_index(self.qscale, cbp_index);
            self.mspel = if self.w2.mspel_bit { br.read_bit() != 0 } else { false };
            if self.w2.abt_flag {
                self.w2.per_mb_abt = br.read_bit() == 0;
                if !self.w2.per_mb_abt {
                    self.w2.abt_type = br.decode012() as usize;
                }
            }
            self.per_mb_rl_table = if self.w2.per_mb_rl_bit { br.read_bit() != 0 } else { false };
            if !self.per_mb_rl_table {
                self.rl_table_index = br.decode012() as usize;
                self.rl_chroma_table_index = self.rl_table_index;
            }
            if br.bits_left() < 2 {
                return Err(Error::invalid("wmv2: header truncated"));
            }
            self.dc_table_index = br.read_bit() as usize;
            self.mv_table_index = br.read_bit() as usize;
            self.no_rounding = !self.no_rounding;
        }
        self.esc3_level_length = 0;
        self.esc3_run_length = 0;

        if self.w2.j_type {
            let q = self.qscale;
            let loop_filter = self.loop_filter;
            if let Some(x8) = self.x8.as_mut() {
                x8.decode_picture(&mut self.cur, br, 2 * q, (q - 1) | 1, loop_filter, &mut self.qscale_table);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// `wmv2_decode_inter_block`.
    fn wmv2_decode_inter_block(&mut self, br: &mut BitReader, n: usize, cbp: bool) -> bool {
        const SUB_CBP_TABLE: [u32; 3] = [2, 3, 1];
        if !cbp {
            self.block_last_index[n] = -1;
            return true;
        }
        if self.w2.per_block_abt {
            self.w2.abt_type = br.decode012() as usize;
        }
        self.w2.abt_type_table[n] = self.w2.abt_type;
        if self.w2.abt_type != 0 {
            let scantable = if self.w2.abt_type == 1 { &WMV2_SCANTABLE_A } else { &WMV2_SCANTABLE_B };
            let sub_cbp = SUB_CBP_TABLE[br.decode012() as usize];
            if sub_cbp & 1 != 0 && !self.decode_block_into(br, n, true, Some(scantable), false) {
                return false;
            }
            if sub_cbp & 2 != 0 && !self.decode_block_into(br, n, true, Some(scantable), true) {
                return false;
            }
            self.block_last_index[n] = 63;
            true
        } else {
            let scan = self.inter_scantable;
            self.decode_block_into(br, n, true, Some(&scan), false)
        }
    }

    /// `wmv2_decode_mb`.
    pub(crate) fn wmv2_decode_mb(&mut self, br: &mut BitReader) -> bool {
        let t = &*TABLES;
        let mb_xy = self.mb_y * self.mb_stride + self.mb_x;
        let cbp: i32;
        if self.pict_type == PICT_P {
            if self.mb_type[mb_xy] & MB_TYPE_SKIP != 0 {
                self.mb_intra = false;
                self.block_last_index = [-1; 6];
                self.mv = [0, 0];
                self.w2.hshift = 0;
                return true;
            }
            if br.bits_left() <= 0 {
                return false;
            }
            let code = t.mb_non_intra[self.w2.cbp_table_index].get(br);
            self.mb_intra = (!code & 0x40) >> 6 != 0;
            cbp = code & 0x3f;
        } else {
            self.mb_intra = true;
            if br.bits_left() <= 0 {
                return false;
            }
            let code = t.mb_i.get(br);
            let mut c = 0;
            for i in 0..6 {
                let mut val = (code >> (5 - i)) & 1;
                if i < 4 {
                    let (pred, idx) = self.coded_block_pred(i);
                    val ^= pred;
                    self.coded_block[idx] = val as u8;
                }
                c |= val << (5 - i);
            }
            cbp = c;
        }

        if !self.mb_intra {
            let (mut mx, mut my) = self.wmv2_pred_motion(br);
            if cbp != 0 {
                self.block = [[0; 64]; 6];
                if self.per_mb_rl_table {
                    self.rl_table_index = br.decode012() as usize;
                    self.rl_chroma_table_index = self.rl_table_index;
                }
                if self.w2.abt_flag && self.w2.per_mb_abt {
                    self.w2.per_block_abt = br.read_bit() != 0;
                    if !self.w2.per_block_abt {
                        self.w2.abt_type = br.decode012() as usize;
                    }
                } else {
                    self.w2.per_block_abt = false;
                }
            }
            // wmv2_decode_motion
            decode_ms_motion(br, self.mv_table_index, &mut mx, &mut my);
            self.w2.hshift = if ((mx | my) & 1) != 0 && self.mspel { br.read_bit() as usize } else { 0 };
            self.mv = [mx, my];
            for i in 0..6 {
                if !self.wmv2_decode_inter_block(br, i, (cbp >> (5 - i)) & 1 != 0) {
                    return false;
                }
            }
        } else {
            self.ac_pred = br.read_bit() != 0;
            if self.per_mb_rl_table && cbp != 0 {
                self.rl_table_index = br.decode012() as usize;
                self.rl_chroma_table_index = self.rl_table_index;
            }
            self.block = [[0; 64]; 6];
            for i in 0..6 {
                if !self.decode_block(br, i, (cbp >> (5 - i)) & 1 != 0, None) {
                    return false;
                }
            }
        }
        true
    }

    /// `ff_mspel_motion`.
    pub(crate) fn mspel_motion(&mut self) {
        let Some(refp) = self.last.as_ref() else { return };
        let (motion_x, motion_y) = (self.mv[0], self.mv[1]);
        let mut dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as usize;
        dxy = 2 * dxy + self.w2.hshift;
        let mut src_x = self.mb_x as i32 * 16 + (motion_x >> 1);
        let mut src_y = self.mb_y as i32 * 16 + (motion_y >> 1);
        let (w, h) = (self.width as i32, self.height as i32);
        src_x = src_x.clamp(-16, w);
        src_y = src_y.clamp(-16, h);
        if src_x <= -16 || src_x >= w {
            dxy &= !3;
        }
        if src_y <= -16 || src_y >= h {
            dxy &= !4;
        }
        let ls = self.cur.linesize[0];
        let uvls = self.cur.linesize[1];
        let dest_y = self.mb_y * 16 * ls + self.mb_x * 16;
        let dest_c = self.mb_y * 8 * uvls + self.mb_x * 8;
        let (hep, vep) = (self.h_edge_pos, self.v_edge_pos);
        let no_rnd = self.no_rounding;

        // Luma: a 19x19 area starting one pixel up-left of the block.
        let mut ebuf = [0u8; 19 * 19];
        let src = mpv::src_block(&refp.data[0], refp.linesize[0], hep, vep, src_x - 1, src_y - 1, 19, 19, &mut ebuf);
        let base = src.off + src.stride + 1;
        for (bx, by) in [(0usize, 0usize), (8, 0), (0, 8), (8, 8)] {
            put_mspel8(
                &mut self.cur.data[0],
                dest_y + by * ls + bx,
                ls,
                src.data,
                base + by * src.stride + bx,
                src.stride,
                dxy,
            );
        }

        let mut cdxy = 0usize;
        if motion_x & 3 != 0 {
            cdxy |= 1;
        }
        if motion_y & 3 != 0 {
            cdxy |= 2;
        }
        let mx = motion_x >> 2;
        let my = motion_y >> 2;
        let mut csx = self.mb_x as i32 * 8 + mx;
        let mut csy = self.mb_y as i32 * 8 + my;
        csx = csx.clamp(-8, w >> 1);
        if csx == (w >> 1) {
            cdxy &= !1;
        }
        csy = csy.clamp(-8, h >> 1);
        if csy == (h >> 1) {
            cdxy &= !2;
        }
        for p in 1..3 {
            let mut cbuf = [0u8; 9 * 9];
            let src: SrcBlock =
                mpv::src_block(&refp.data[p], refp.linesize[p], hep >> 1, vep >> 1, csx, csy, 9, 9, &mut cbuf);
            mpv::put_hpel(&mut self.cur.data[p], dest_c, uvls, &src, 8, 8, cdxy, no_rnd);
        }
    }

    /// `ff_wmv2_add_mb`.
    pub(crate) fn wmv2_add_mb(&mut self, dest_y: usize, dest_c: usize) {
        for n in 0..6 {
            if self.block_last_index[n] < 0 {
                continue;
            }
            let (p, off, stride) = self.block_dest(n, dest_y, dest_c);
            match self.w2.abt_type_table[n] {
                0 => idct::wmv2_idct_add(&mut self.cur.data[p], off, stride, &mut self.block[n]),
                1 => {
                    idct::simple_idct84_add(&mut self.cur.data[p], off, stride, &mut self.block[n]);
                    idct::simple_idct84_add(&mut self.cur.data[p], off + 4 * stride, stride, &mut self.w2.abt_block2[n]);
                    self.w2.abt_block2[n] = [0; 64];
                }
                _ => {
                    idct::simple_idct48_add(&mut self.cur.data[p], off, stride, &mut self.block[n]);
                    idct::simple_idct48_add(&mut self.cur.data[p], off + 4, stride, &mut self.w2.abt_block2[n]);
                    self.w2.abt_block2[n] = [0; 64];
                }
            }
        }
    }
}

/// `wmv2_mspel8_h_lowpass`: `h` rows of 8 from `src` (needs src[-1..9]).
fn mspel_h(dst: &mut [u8], doff: usize, dstride: usize, src: &[u8], soff: usize, sstride: usize, h: usize) {
    for i in 0..h {
        let s = soff + i * sstride;
        let d = doff + i * dstride;
        for x in 0..8 {
            let v = 9 * (src[s + x] as i32 + src[s + x + 1] as i32) - (src[s + x - 1] as i32 + src[s + x + 2] as i32);
            dst[d + x] = ((v + 8) >> 4).clamp(0, 255) as u8;
        }
    }
}

/// `wmv2_mspel8_v_lowpass`: 8 rows, `w` columns (needs rows -1..9).
fn mspel_v(dst: &mut [u8], doff: usize, dstride: usize, src: &[u8], soff: usize, sstride: usize, w: usize) {
    for x in 0..w {
        let g = |r: isize| src[(soff as isize + x as isize + r * sstride as isize) as usize] as i32;
        for y in 0..8isize {
            let v = 9 * (g(y) + g(y + 1)) - (g(y - 1) + g(y + 2));
            dst[doff + x + y as usize * dstride] = ((v + 8) >> 4).clamp(0, 255) as u8;
        }
    }
}

/// `ff_put_pixels8_l2_8`: average of two 8-wide sources (rounding up).
fn put_l2(dst: &mut [u8], doff: usize, dstride: usize, a: &[u8], aoff: usize, astride: usize, b: &[u8], boff: usize, bstride: usize) {
    for y in 0..8 {
        for x in 0..8 {
            let va = a[aoff + y * astride + x] as u32;
            let vb = b[boff + y * bstride + x] as u32;
            dst[doff + y * dstride + x] = ((va + vb + 1) >> 1) as u8;
        }
    }
}

/// `put_mspel_pixels_tab[dxy]` for one 8x8 block at `soff` of `src`.
fn put_mspel8(dst: &mut [u8], doff: usize, dstride: usize, src: &[u8], soff: usize, stride: usize, dxy: usize) {
    match dxy {
        0 => {
            for y in 0..8 {
                dst[doff + y * dstride..doff + y * dstride + 8].copy_from_slice(&src[soff + y * stride..soff + y * stride + 8]);
            }
        }
        1 => {
            let mut half = [0u8; 64];
            mspel_h(&mut half, 0, 8, src, soff, stride, 8);
            put_l2(dst, doff, dstride, src, soff, stride, &half, 0, 8);
        }
        2 => mspel_h(dst, doff, dstride, src, soff, stride, 8),
        3 => {
            let mut half = [0u8; 64];
            mspel_h(&mut half, 0, 8, src, soff, stride, 8);
            put_l2(dst, doff, dstride, src, soff + 1, stride, &half, 0, 8);
        }
        4 => mspel_v(dst, doff, dstride, src, soff, stride, 8),
        5 | 7 => {
            let mut half_h = [0u8; 88];
            let mut half_v = [0u8; 64];
            let mut half_hv = [0u8; 64];
            mspel_h(&mut half_h, 0, 8, src, soff - stride, stride, 11);
            let voff = if dxy == 5 { soff } else { soff + 1 };
            mspel_v(&mut half_v, 0, 8, src, voff, stride, 8);
            mspel_v(&mut half_hv, 0, 8, &half_h, 8, 8, 8);
            put_l2(dst, doff, dstride, &half_v, 0, 8, &half_hv, 0, 8);
        }
        _ => {
            let mut half_h = [0u8; 88];
            mspel_h(&mut half_h, 0, 8, src, soff - stride, stride, 11);
            mspel_v(dst, doff, dstride, &half_h, 8, 8, 8);
        }
    }
}

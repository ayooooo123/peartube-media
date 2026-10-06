//! VC-1 motion compensation.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vc1_mc.c`
//! (LGPL-2.1-or-later). Reference blocks are always fetched through an
//! edge-clamping copy, which is equivalent to FFmpeg's direct reads (taken
//! only when the block is fully inside) plus `emulated_edge_mc` (per-pixel
//! clamping). The row mapping reproduces the frame / field / interlaced
//! reference variants of the emulation calls.

use super::dsp::{self, Src};
use super::*;
use crate::mpv::mid_pred;

const POPCOUNT4: [i32; 16] = [0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4];
const S_RNDTBLFIELD: [i32; 16] = [0, 0, 1, 2, 4, 4, 5, 6, 2, 2, 3, 8, 6, 6, 7, 12];

/// Which picture a prediction reads from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RefSel {
    Last,
    Next,
    /// The first field of the picture being decoded.
    Cur,
}

/// Geometry of a reference fetch: logical row `r` reads frame row
/// `y0 + step * r`, clamped either in frame coordinates or within its field
/// (`interlaced`), exactly like the `emulated_edge_mc` call variants.
#[derive(Clone, Copy)]
struct Fetch {
    x0: i32,
    y0: i32,
    step: i32,
    w: usize,
    h: usize,
    interlaced: bool,
    hmax: i32,
    vmax: i32,
}

fn fetch(plane: &[u8], stride: usize, f: &Fetch, out: &mut [u8]) {
    let hmax = f.hmax.max(1);
    let vmax = f.vmax.max(1);
    for r in 0..f.h {
        let y = f.y0 + f.step * r as i32;
        let row = if f.interlaced {
            let fh = (vmax >> 1).max(1);
            2 * (y >> 1).clamp(0, fh - 1) + (y & 1)
        } else {
            y.clamp(0, vmax - 1)
        } as usize;
        let base = row * stride;
        for c in 0..f.w {
            let x = (f.x0 + c as i32).clamp(0, hmax - 1) as usize;
            out[r * f.w + c] = plane.get(base + x).copied().unwrap_or(0);
        }
    }
}

/// `vc1_scale_luma` / `vc1_scale_chroma`.
fn scale_block(buf: &mut [u8]) {
    for v in buf.iter_mut() {
        *v = (((*v as i32 - 128) >> 1) + 128) as u8;
    }
}

/// `vc1_lut_scale_*`: logical row `r` uses `luts[parity(r)]`.
fn lut_block(buf: &mut [u8], w: usize, h: usize, luts: &[[u8; 256]; 2], parity: impl Fn(usize) -> usize) {
    for r in 0..h {
        let lut = &luts[parity(r) & 1];
        for v in &mut buf[r * w..r * w + w] {
            *v = lut[*v as usize];
        }
    }
}

/// `median4`.
fn median4(a: i32, b: i32, c: i32, d: i32) -> i32 {
    if a < b {
        if c < d {
            (b.min(d) + a.max(c)) / 2
        } else {
            (b.min(c) + a.max(d)) / 2
        }
    } else if c < d {
        (a.min(d) + b.max(c)) / 2
    } else {
        (a.min(c) + b.max(d)) / 2
    }
}

#[inline]
fn fits(len: usize, off: isize, stride: usize, w: usize, h: usize) -> bool {
    off >= 0 && off as usize + (h - 1) * stride + w <= len
}

impl Vc1Decoder {
    fn ref_pic(&self, sel: RefSel) -> Option<&VPic> {
        match sel {
            RefSel::Last => self.last.as_ref(),
            RefSel::Next => self.next.as_ref(),
            RefSel::Cur => self.cur.as_ref(),
        }
    }

    /// Selects (picture, LUT owner, use_ic, interlaced) like the `!dir`
    /// / `dir` branches of the MC functions.
    fn select_ref(&self, dir: usize, ref_type: i32) -> (RefSel, usize, bool, bool) {
        if dir == 0 {
            if self.field_mode && self.cur_field_type != ref_type && self.second_field {
                (RefSel::Cur, 2, self.curr_use_ic(), true)
            } else {
                (RefSel::Last, 0, self.last_use_ic, self.last_interlaced)
            }
        } else {
            (RefSel::Next, 1, self.next_use_ic, self.next_interlaced)
        }
    }

    fn luts(&self, which: usize, chroma: bool) -> &[[u8; 256]; 2] {
        match (which, chroma) {
            (0, false) => &self.last_luty,
            (0, true) => &self.last_lutuv,
            (1, false) => &self.next_luty,
            (1, true) => &self.next_lutuv,
            (_, false) => self.curr_luts().0,
            (_, true) => self.curr_luts().1,
        }
    }

    /// Row geometry of a luma or chroma fetch for frame or field pictures.
    #[allow(clippy::too_many_arguments)]
    fn fetch_geom(&self, x0: i32, y: i32, ref_type: i32, w: usize, interlace: bool, chroma: bool, fieldmv: bool) -> Fetch {
        let (hmax, vmax) = if chroma {
            (self.h_edge_pos >> 1, self.v_edge_pos >> 1)
        } else {
            (self.h_edge_pos, self.v_edge_pos)
        };
        let (y0, step) = if self.field_mode {
            (2 * y + ref_type, 2)
        } else if fieldmv {
            (y, 2)
        } else {
            (y, 1)
        };
        Fetch { x0, y0, step, w, h: w, interlaced: interlace, hmax, vmax }
    }

    fn clip_src_1mv(&self, src_x: &mut i32, src_y: &mut i32, uvsrc_x: &mut i32, uvsrc_y: &mut i32) {
        if self.profile != PROFILE_ADVANCED {
            *src_x = (*src_x).clamp(-16, (self.mb_width * 16) as i32);
            *src_y = (*src_y).clamp(-16, (self.mb_height * 16) as i32);
            *uvsrc_x = (*uvsrc_x).clamp(-8, (self.mb_width * 8) as i32);
            *uvsrc_y = (*uvsrc_y).clamp(-8, (self.mb_height * 8) as i32);
        } else {
            let cw = self.coded_width;
            let ch = self.coded_height;
            *src_x = (*src_x).clamp(-17, cw);
            *uvsrc_x = (*uvsrc_x).clamp(-8, cw >> 1);
            if self.fcm == ILACE_FRAME {
                let sy = *src_y;
                *src_y = sy.clamp(-18 + (sy & 1), ch + (sy & 1));
                let uy = *uvsrc_y;
                *uvsrc_y = uy.clamp(-8 + (uy & 1), (ch >> 1) + (uy & 1));
            } else {
                *src_y = (*src_y).clamp(-18, ch + 1);
                *uvsrc_y = (*uvsrc_y).clamp(-8, ch >> 1);
            }
        }
    }

    /// Fetches the 16x16 (+mspel border) luma and both 9x9 chroma source
    /// blocks of a 1-MV prediction, with range reduction and intensity
    /// compensation applied. Returns None when the reference is missing.
    #[allow(clippy::too_many_arguments)]
    fn fetch_1mv(
        &self,
        sel: RefSel,
        lut_owner: usize,
        use_ic: bool,
        interlace: bool,
        ref_type: i32,
        src_x: i32,
        src_y: i32,
        uvsrc_x: i32,
        uvsrc_y: i32,
        ybuf: &mut [u8],
        ubuf: &mut [u8],
        vbuf: &mut [u8],
    ) -> Option<usize> {
        let pic = self.ref_pic(sel)?;
        let ms = self.mspel as i32;
        let k = (17 + 2 * ms) as usize;
        let fy = self.fetch_geom(src_x - ms, src_y - ms, ref_type, k, interlace, false, false);
        fetch(&pic.pic.data[0], pic.pic.linesize[0], &fy, ybuf);
        let fc = self.fetch_geom(uvsrc_x, uvsrc_y, ref_type, 9, interlace, true, false);
        fetch(&pic.pic.data[1], pic.pic.linesize[1], &fc, ubuf);
        fetch(&pic.pic.data[2], pic.pic.linesize[2], &fc, vbuf);
        if self.rangeredfrm {
            scale_block(&mut ybuf[..k * k]);
            scale_block(&mut ubuf[..81]);
            scale_block(&mut vbuf[..81]);
        }
        if use_ic {
            let fm = self.field_mode;
            let lp = |base: i32| move |r: usize| if fm { ref_type as usize } else { ((base + r as i32) & 1) as usize };
            lut_block(ybuf, k, k, self.luts(lut_owner, false), lp(src_y - ms));
            lut_block(ubuf, 9, 9, self.luts(lut_owner, true), lp(uvsrc_y));
            lut_block(vbuf, 9, 9, self.luts(lut_owner, true), lp(uvsrc_y));
        }
        Some(k)
    }

    /// Writes the luma and chroma predictions of a 1-MV block.
    #[allow(clippy::too_many_arguments)]
    fn put_1mv(&mut self, ybuf: &[u8], ubuf: &[u8], vbuf: &[u8], k: usize, mx: i32, my: i32, uvmx: i32, uvmy: i32, avg: bool) {
        let ms = self.mspel as usize;
        let rnd = self.rnd;
        let (ls, uvls) = (self.linesize, self.uvlinesize);
        let dest = self.dest;
        let mspel = self.mspel;
        let Some(cur) = self.cur.as_mut() else { return };
        let ysrc = Src { data: ybuf, off: (ms * (k + 1)) as isize, stride: k as isize };
        let d0 = &mut cur.pic.data[0];
        if fits(d0.len(), dest[0], ls, 16, 16) {
            if mspel {
                let dxy = (((my & 3) << 2) | (mx & 3)) as usize;
                dsp::vc1_mspel_mc(d0, dest[0] as usize, ls, &ysrc, 16, dxy, rnd, avg);
            } else {
                let dxy = ((my & 2) | ((mx & 2) >> 1)) as usize;
                dsp::hpel(d0, dest[0] as usize, ls, &ysrc, 16, dxy, rnd != 0, avg);
            }
        }
        let (cx, cy) = ((uvmx & 3) << 1, (uvmy & 3) << 1);
        for (p, buf) in [(1usize, ubuf), (2usize, vbuf)] {
            let src = Src { data: buf, off: 0, stride: 9 };
            let d = &mut cur.pic.data[p];
            if fits(d.len(), dest[p], uvls, 8, 8) {
                dsp::chroma_mc(d, dest[p] as usize, uvls, &src, 8, 8, cx, cy, rnd != 0, avg);
            }
        }
    }

    /// `ff_vc1_mc_1mv`.
    pub(crate) fn mc_1mv(&mut self, dir: usize) {
        if (!self.field_mode || (self.ref_field_type[dir] == 1 && self.cur_field_type == 1)) && self.last.is_none() {
            return;
        }
        let mx = self.mv[dir][0][0];
        let mut my = self.mv[dir][0][1];
        if self.pict_type == PICT_P {
            for i in 0..4 {
                self.set_mv_at(1, self.block_index[i] + self.blocks_off, [mx as i16, my as i16]);
            }
        }
        let mut uvmx = (mx + ((mx & 3) == 3) as i32) >> 1;
        let mut uvmy = (my + ((my & 3) == 3) as i32) >> 1;
        let r = self.row3(self.mb_x as isize);
        self.luma_mv_base[r] = [uvmx as i16, uvmy as i16];
        let ref_type = self.ref_field_type[dir];
        if self.field_mode && self.cur_field_type != ref_type {
            my = my - 2 + 4 * self.cur_field_type;
            uvmy = uvmy - 2 + 4 * self.cur_field_type;
        }
        if self.fastuvmc && self.fcm != ILACE_FRAME {
            uvmx += if uvmx < 0 { uvmx & 1 } else { -(uvmx & 1) };
            uvmy += if uvmy < 0 { uvmy & 1 } else { -(uvmy & 1) };
        }
        let (sel, lut_owner, use_ic, interlace) = self.select_ref(dir, ref_type);
        let mut src_x = self.mb_x as i32 * 16 + (mx >> 2);
        let mut src_y = self.mb_y as i32 * 16 + (my >> 2);
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        self.clip_src_1mv(&mut src_x, &mut src_y, &mut uvsrc_x, &mut uvsrc_y);

        let mut ybuf = [0u8; 19 * 19];
        let mut ubuf = [0u8; 81];
        let mut vbuf = [0u8; 81];
        let Some(k) = self.fetch_1mv(
            sel, lut_owner, use_ic, interlace, ref_type, src_x, src_y, uvsrc_x, uvsrc_y, &mut ybuf, &mut ubuf, &mut vbuf,
        ) else {
            return;
        };
        self.put_1mv(&ybuf, &ubuf, &vbuf, k, mx, my, uvmx, uvmy, false);
        if self.field_mode {
            let f = (self.cur_field_type != ref_type) as u8;
            let i4 = self.mvf_idx(dir, self.block_index[4] + self.mb_off);
            let i5 = self.mvf_idx(dir, self.block_index[5] + self.mb_off);
            self.mv_f[i4] = f;
            self.mv_f[i5] = f;
        }
    }

    /// `get_luma_mv`: returns (tx, ty, opp_count).
    fn get_luma_mv(&self, dir: usize) -> (i16, i16, i32) {
        const INDEX2: [u8; 16] = [0, 0, 0, 0x23, 0, 0x13, 0x03, 0, 0, 0x12, 0x02, 0, 0x01, 0, 0, 0];
        let mut idx = 0usize;
        for k in 0..4 {
            idx |= ((self.mv_f[self.mvf_idx(dir, self.block_index[k] + self.blocks_off)] & 1) as usize) << k;
        }
        let opp = POPCOUNT4[idx];
        let m = &self.mv[dir];
        let (tx, ty) = match opp {
            0 | 4 => (median4(m[0][0], m[1][0], m[2][0], m[3][0]), median4(m[0][1], m[1][1], m[2][1], m[3][1])),
            1 => {
                let (a, b, c) = ((idx < 2) as usize, 1 + (idx < 4) as usize, 2 + (idx < 8) as usize);
                (mid_pred(m[a][0], m[b][0], m[c][0]), mid_pred(m[a][1], m[b][1], m[c][1]))
            }
            3 => {
                let (a, b, c) = ((idx > 0xd) as usize, 1 + (idx > 0xb) as usize, 2 + (idx > 0x7) as usize);
                (mid_pred(m[a][0], m[b][0], m[c][0]), mid_pred(m[a][1], m[b][1], m[c][1]))
            }
            _ => {
                let (a, b) = ((INDEX2[idx] >> 4) as usize, (INDEX2[idx] & 0xf) as usize);
                ((m[a][0] + m[b][0]) / 2, (m[a][1] + m[b][1]) / 2)
            }
        };
        (tx as i16, ty as i16, opp)
    }

    /// `get_chroma_mv`: returns (tx, ty, valid_count).
    fn get_chroma_mv(&self, dir: usize) -> (i16, i16, i32) {
        const INDEX2: [u8; 16] = [0, 0, 0, 0x01, 0, 0x02, 0x12, 0, 0, 0x03, 0x13, 0, 0x23, 0, 0, 0];
        let mut idx = 0usize;
        for k in 0..4 {
            idx |= ((self.vmbt(self.block_index[k]) == 0) as usize) << k;
        }
        let valid = POPCOUNT4[idx];
        let m = &self.mv[dir];
        let (tx, ty) = match valid {
            4 => (median4(m[0][0], m[1][0], m[2][0], m[3][0]), median4(m[0][1], m[1][1], m[2][1], m[3][1])),
            3 => {
                let (a, b, c) = ((idx > 0xd) as usize, 1 + (idx > 0xb) as usize, 2 + (idx > 0x7) as usize);
                (mid_pred(m[a][0], m[b][0], m[c][0]), mid_pred(m[a][1], m[b][1], m[c][1]))
            }
            2 => {
                let (a, b) = ((INDEX2[idx] >> 4) as usize, (INDEX2[idx] & 0xf) as usize);
                ((m[a][0] + m[b][0]) / 2, (m[a][1] + m[b][1]) / 2)
            }
            _ => return (0, 0, 0),
        };
        (tx as i16, ty as i16, valid)
    }

    /// `ff_vc1_mc_4mv_luma`.
    pub(crate) fn mc_4mv_luma(&mut self, n: usize, dir: usize, avg: bool) {
        let fieldmv = self.fcm == ILACE_FRAME && self.blk_mv_type_at(self.block_index[n]) != 0;
        if (!self.field_mode || (self.ref_field_type[dir] == 1 && self.cur_field_type == 1)) && self.last.is_none() {
            return;
        }
        let mut mx = self.mv[dir][n][0];
        let mut my = self.mv[dir][n][1];
        let ref_type = self.ref_field_type[dir];
        let (sel, lut_owner, use_ic, interlace) = self.select_ref(dir, ref_type);
        if self.ref_pic(sel).is_none() {
            return;
        }
        if self.field_mode && self.cur_field_type != ref_type {
            my = my - 2 + 4 * self.cur_field_type;
        }
        if self.pict_type == PICT_P && n == 3 && self.field_mode {
            let (tx, ty, opp) = self.get_luma_mv(0);
            self.set_mv_at(1, self.block_index[0] + self.blocks_off, [tx, ty]);
            let f = (opp > 2) as u8;
            for k in 0..4 {
                let i = self.mvf_idx(1, self.block_index[k] + self.blocks_off);
                self.mv_f[i] = f;
            }
        }
        if self.fcm == ILACE_FRAME {
            let width = self.coded_width;
            let height = self.coded_height >> 1;
            if self.pict_type == PICT_P {
                self.set_mv_at(1, self.block_index[n] + self.blocks_off, [mx as i16, my as i16]);
            }
            let qx = self.mb_x as i32 * 16 + (mx >> 2);
            let qy = self.mb_y as i32 * 8 + (my >> 3);
            if qx < -17 {
                mx -= 4 * (qx + 17);
            } else if qx > width {
                mx -= 4 * (qx - width);
            }
            if qy < -18 {
                my -= 8 * (qy + 18);
            } else if qy > height + 1 {
                my -= 8 * (qy - height - 1);
            }
        }
        let ls = self.linesize as isize;
        let off = if fieldmv {
            (if n > 1 { ls } else { 0 }) + (n & 1) as isize * 8
        } else {
            ls * 4 * (n & 2) as isize + (n & 1) as isize * 8
        };
        let mut src_x = self.mb_x as i32 * 16 + (n & 1) as i32 * 8 + (mx >> 2);
        let mut src_y = if !fieldmv {
            self.mb_y as i32 * 16 + (n & 2) as i32 * 4 + (my >> 2)
        } else {
            self.mb_y as i32 * 16 + (n > 1) as i32 + (my >> 2)
        };
        if self.profile != PROFILE_ADVANCED {
            src_x = src_x.clamp(-16, (self.mb_width * 16) as i32);
            src_y = src_y.clamp(-16, (self.mb_height * 16) as i32);
        } else {
            src_x = src_x.clamp(-17, self.coded_width);
            if self.fcm == ILACE_FRAME {
                src_y = src_y.clamp(-18 + (src_y & 1), self.coded_height + (src_y & 1));
            } else {
                src_y = src_y.clamp(-18, self.coded_height + 1);
            }
        }

        let ms = self.mspel as i32;
        let k = (9 + 2 * ms) as usize;
        let mut ybuf = [0u8; 11 * 11];
        let y0 = if self.field_mode { src_y - ms } else { src_y - (ms << fieldmv as i32) };
        let geom = self.fetch_geom(src_x - ms, y0, ref_type, k, interlace, false, fieldmv);
        {
            let Some(pic) = self.ref_pic(sel) else { return };
            fetch(&pic.pic.data[0], pic.pic.linesize[0], &geom, &mut ybuf);
        }
        if self.rangeredfrm {
            scale_block(&mut ybuf[..k * k]);
        }
        if use_ic {
            let fm = self.field_mode;
            let step = geom.step;
            let base = src_y - (ms << fieldmv as i32);
            let rt = ref_type as usize;
            let luts = *self.luts(lut_owner, false);
            lut_block(&mut ybuf, k, k, &luts, |r| if fm { rt } else { ((base + step * r as i32) & 1) as usize });
        }
        let rnd = self.rnd;
        let mspel = self.mspel;
        let dst_off = self.dest[0] + off;
        let Some(cur) = self.cur.as_mut() else { return };
        let d0 = &mut cur.pic.data[0];
        let src = Src { data: &ybuf, off: ms as isize * (k as isize + 1), stride: k as isize };
        if mspel {
            let stride = (ls as usize) << fieldmv as usize;
            if fits(d0.len(), dst_off, stride, 8, 8) {
                let dxy = (((my & 3) << 2) | (mx & 3)) as usize;
                dsp::vc1_mspel_mc(d0, dst_off as usize, stride, &src, 8, dxy, rnd, avg);
            }
        } else if fits(d0.len(), dst_off, ls as usize, 8, 8) {
            let dxy = ((my & 2) | ((mx & 2) >> 1)) as usize;
            dsp::hpel(d0, dst_off as usize, ls as usize, &src, 8, dxy, rnd != 0, false);
        }
    }

    /// `ff_vc1_mc_4mv_chroma`.
    pub(crate) fn mc_4mv_chroma(&mut self, dir: usize) {
        if !self.field_mode && self.last.is_none() {
            return;
        }
        let (tx, ty, chroma_ref_type);
        if !self.field_mode || self.numref == 0 {
            let (x, y, valid) = self.get_chroma_mv(dir);
            if valid == 0 {
                self.set_mv_at(1, self.block_index[0] + self.blocks_off, [0, 0]);
                let r = self.row3(self.mb_x as isize);
                self.luma_mv_base[r] = [0, 0];
                return;
            }
            tx = x;
            ty = y;
            chroma_ref_type = self.ref_field_type[dir];
        } else {
            let (x, y, opp) = self.get_luma_mv(dir);
            tx = x;
            ty = y;
            chroma_ref_type = self.cur_field_type ^ (opp > 2) as i32;
        }
        if self.field_mode && chroma_ref_type == 1 && self.cur_field_type == 1 && self.last.is_none() {
            return;
        }
        self.set_mv_at(1, self.block_index[0] + self.blocks_off, [tx, ty]);
        let (tx, ty) = (tx as i32, ty as i32);
        let mut uvmx = (tx + ((tx & 3) == 3) as i32) >> 1;
        let mut uvmy = (ty + ((ty & 3) == 3) as i32) >> 1;
        let r = self.row3(self.mb_x as isize);
        self.luma_mv_base[r] = [uvmx as i16, uvmy as i16];
        if self.fastuvmc {
            uvmx += if uvmx < 0 { uvmx & 1 } else { -(uvmx & 1) };
            uvmy += if uvmy < 0 { uvmy & 1 } else { -(uvmy & 1) };
        }
        if self.cur_field_type != chroma_ref_type {
            uvmy += 2 - 4 * chroma_ref_type;
        }
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        if self.profile != PROFILE_ADVANCED {
            uvsrc_x = uvsrc_x.clamp(-8, (self.mb_width * 8) as i32);
            uvsrc_y = uvsrc_y.clamp(-8, (self.mb_height * 8) as i32);
        } else {
            uvsrc_x = uvsrc_x.clamp(-8, self.coded_width >> 1);
            uvsrc_y = uvsrc_y.clamp(-8, self.coded_height >> 1);
        }
        let (sel, lut_owner, use_ic, interlace) = self.select_ref(dir, chroma_ref_type);
        let mut ubuf = [0u8; 81];
        let mut vbuf = [0u8; 81];
        let geom = self.fetch_geom(uvsrc_x, uvsrc_y, chroma_ref_type, 9, interlace, true, false);
        {
            let Some(pic) = self.ref_pic(sel) else { return };
            fetch(&pic.pic.data[1], pic.pic.linesize[1], &geom, &mut ubuf);
            fetch(&pic.pic.data[2], pic.pic.linesize[2], &geom, &mut vbuf);
        }
        if self.rangeredfrm {
            scale_block(&mut ubuf);
            scale_block(&mut vbuf);
        }
        if use_ic {
            let fm = self.field_mode;
            let rt = chroma_ref_type as usize;
            let luts = *self.luts(lut_owner, true);
            let par = |r: usize| if fm { rt } else { ((uvsrc_y + r as i32) & 1) as usize };
            lut_block(&mut ubuf, 9, 9, &luts, par);
            lut_block(&mut vbuf, 9, 9, &luts, par);
        }
        let (cx, cy) = ((uvmx & 3) << 1, (uvmy & 3) << 1);
        let rnd = self.rnd;
        let uvls = self.uvlinesize;
        let dest = self.dest;
        if let Some(cur) = self.cur.as_mut() {
            for (p, buf) in [(1usize, &ubuf), (2usize, &vbuf)] {
                let src = Src { data: buf, off: 0, stride: 9 };
                let d = &mut cur.pic.data[p];
                if fits(d.len(), dest[p], uvls, 8, 8) {
                    dsp::chroma_mc(d, dest[p] as usize, uvls, &src, 8, 8, cx, cy, rnd != 0, false);
                }
            }
        }
        if self.field_mode {
            let f = (self.cur_field_type != chroma_ref_type) as u8;
            let i4 = self.mvf_idx(dir, self.block_index[4] + self.mb_off);
            let i5 = self.mvf_idx(dir, self.block_index[5] + self.mb_off);
            self.mv_f[i4] = f;
            self.mv_f[i5] = f;
        }
    }

    /// `ff_vc1_mc_4mv_chroma4` (interlaced frame pictures).
    pub(crate) fn mc_4mv_chroma4(&mut self, dir: usize, dir2: usize, avg: bool) {
        let fieldmv = self.blk_mv_type_at(self.block_index[0]) != 0;
        let v_dist = if fieldmv { 1 } else { 4 };
        let mut uvmx_field = [0i32; 4];
        let mut uvmy_field = [0i32; 4];
        for i in 0..4 {
            let d = if i < 2 { dir } else { dir2 };
            let tx = self.mv[d][i][0];
            uvmx_field[i] = (tx + ((tx & 3) == 3) as i32) >> 1;
            let ty = self.mv[d][i][1];
            uvmy_field[i] = if fieldmv {
                (ty >> 4) * 8 + S_RNDTBLFIELD[(ty & 0xF) as usize]
            } else {
                (ty + ((ty & 3) == 3) as i32) >> 1
            };
        }
        let uvls = self.uvlinesize;
        for i in 0..4 {
            let off = (i & 1) as isize * 4 + if i & 2 != 0 { v_dist * uvls as isize } else { 0 };
            let mut uvsrc_x = self.mb_x as i32 * 8 + (i & 1) as i32 * 4 + (uvmx_field[i] >> 2);
            let mut uvsrc_y = self.mb_y as i32 * 8 + if i & 2 != 0 { v_dist as i32 } else { 0 } + (uvmy_field[i] >> 2);
            uvsrc_x = uvsrc_x.clamp(-8, self.coded_width >> 1);
            if self.fcm == ILACE_FRAME {
                uvsrc_y = uvsrc_y.clamp(-8 + (uvsrc_y & 1), (self.coded_height >> 1) + (uvsrc_y & 1));
            } else {
                uvsrc_y = uvsrc_y.clamp(-8, self.coded_height >> 1);
            }
            let use_next = (if i < 2 { dir } else { dir2 }) != 0;
            let (sel, lut_owner, use_ic, interlace) = if use_next {
                (RefSel::Next, 1, self.next_use_ic, self.next_interlaced)
            } else {
                (RefSel::Last, 0, self.last_use_ic, self.last_interlaced)
            };
            let cx = (uvmx_field[i] & 3) << 1;
            let cy = (uvmy_field[i] & 3) << 1;
            let step = if fieldmv { 2 } else { 1 };
            let geom = Fetch {
                x0: uvsrc_x,
                y0: uvsrc_y,
                step,
                w: 5,
                h: 5,
                interlaced: interlace,
                hmax: self.h_edge_pos >> 1,
                vmax: self.v_edge_pos >> 1,
            };
            let mut ubuf = [0u8; 25];
            let mut vbuf = [0u8; 25];
            {
                let Some(pic) = self.ref_pic(sel) else { return };
                fetch(&pic.pic.data[1], pic.pic.linesize[1], &geom, &mut ubuf);
                fetch(&pic.pic.data[2], pic.pic.linesize[2], &geom, &mut vbuf);
            }
            if use_ic {
                let luts = *self.luts(lut_owner, true);
                let par = |r: usize| ((uvsrc_y + step * r as i32) & 1) as usize;
                lut_block(&mut ubuf, 5, 5, &luts, par);
                lut_block(&mut vbuf, 5, 5, &luts, par);
            }
            let rnd = self.rnd;
            let dest = self.dest;
            let stride = uvls << fieldmv as usize;
            if let Some(cur) = self.cur.as_mut() {
                for (p, buf) in [(1usize, &ubuf), (2usize, &vbuf)] {
                    let src = Src { data: buf, off: 0, stride: 5 };
                    let d = &mut cur.pic.data[p];
                    let o = dest[p] + off;
                    if fits(d.len(), o, stride, 4, 4) {
                        dsp::chroma_mc(d, o as usize, stride, &src, 4, 4, cx, cy, rnd != 0, avg);
                    }
                }
            }
        }
    }

    /// `ff_vc1_interp_mc`: averages the backward prediction into the
    /// forward one already in place.
    pub(crate) fn interp_mc(&mut self) {
        if !self.field_mode && self.next.is_none() {
            return;
        }
        let use_ic = self.next_use_ic;
        let interlace = self.next_interlaced;
        let mx = self.mv[1][0][0];
        let mut my = self.mv[1][0][1];
        let mut uvmx = (mx + ((mx & 3) == 3) as i32) >> 1;
        let mut uvmy = (my + ((my & 3) == 3) as i32) >> 1;
        let ref_type = self.ref_field_type[1];
        if self.field_mode && self.cur_field_type != ref_type {
            my = my - 2 + 4 * self.cur_field_type;
            uvmy = uvmy - 2 + 4 * self.cur_field_type;
        }
        if self.fastuvmc {
            uvmx += if uvmx < 0 { -(uvmx & 1) } else { uvmx & 1 };
            uvmy += if uvmy < 0 { -(uvmy & 1) } else { uvmy & 1 };
        }
        let mut src_x = self.mb_x as i32 * 16 + (mx >> 2);
        let mut src_y = self.mb_y as i32 * 16 + (my >> 2);
        let mut uvsrc_x = self.mb_x as i32 * 8 + (uvmx >> 2);
        let mut uvsrc_y = self.mb_y as i32 * 8 + (uvmy >> 2);
        self.clip_src_1mv(&mut src_x, &mut src_y, &mut uvsrc_x, &mut uvsrc_y);
        let mut ybuf = [0u8; 19 * 19];
        let mut ubuf = [0u8; 81];
        let mut vbuf = [0u8; 81];
        let Some(k) = self.fetch_1mv(
            RefSel::Next,
            1,
            use_ic,
            interlace,
            ref_type,
            src_x,
            src_y,
            uvsrc_x,
            uvsrc_y,
            &mut ybuf,
            &mut ubuf,
            &mut vbuf,
        ) else {
            return;
        };
        self.put_1mv(&ybuf, &ubuf, &vbuf, k, mx, my, uvmx, uvmy, true);
    }
}

//! Macroblock reconstruction for the H.263 path: half-pel motion
//! compensation (16x16, 4MV with H.263 chroma rounding, OBMC), dequantised
//! IDCT residuals and intra blocks.
//!
//! Ported from FFmpeg libavcodec/mpegvideo_dec.c (`ff_mpv_reconstruct_mb`
//! for `NOT_MPEG12_H261`) and mpegvideo_motion.c (`mpeg_motion_internal`,
//! `hpel_motion`, `chroma_4mv_motion`, `apply_8x8`, `apply_obmc`,
//! `put_obmc`) at commit 2da55bf; LGPL-2.1-or-later.
//!
//! References are read through [`Plane::fetch`], whose edge clamping is
//! FFmpeg's `emulated_edge_mc` (and a plain read for blocks inside).

use super::dsp::{hpel, simple_idct_add, simple_idct_put, unquantize_h263_inter, unquantize_h263_intra, PixOp};
use super::tables::ZIGZAG_DIRECT;
use super::*;

/// `raster_end` of the zigzag scan with the identity IDCT permutation.
static RASTER_END: LazyLock<[u8; 64]> = LazyLock::new(|| {
    let mut out = [0u8; 64];
    let mut end = 0;
    for (i, &z) in ZIGZAG_DIRECT.iter().enumerate() {
        end = end.max(z);
        out[i] = end;
    }
    out
});

/// `ff_h263_round_chroma`.
fn h263_round_chroma(x: i32) -> i32 {
    const ROUNDTAB: [i32; 16] = [0, 0, 0, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 1];
    ROUNDTAB[(x & 0xf) as usize] + (x >> 3)
}

/// Motion-compensates a `w`x`h` block whose integer source position is
/// (`x`, `y`) in `refp` into `dst` at `dpos`.
#[allow(clippy::too_many_arguments)]
fn mc_block(dst: &mut Plane, dpos: usize, refp: &Plane, x: i32, y: i32, w: usize, h: usize, dxy: usize, op: PixOp) {
    let fw = w + 1;
    let mut src = [0u8; 17 * 17];
    refp.fetch(x, y, fw, h + 1, &mut src);
    let stride = dst.stride;
    hpel(&mut dst.data, dpos, stride, &src, 0, fw, w, h, dxy, op);
}

impl Rv1020Decoder {
    /// The source position and half-pel phase `hpel_motion` uses for an
    /// 8x8 block at (`src_x`, `src_y`) moved by a half-pel vector.
    fn hpel_source(&self, src_x: i32, src_y: i32, motion_x: i32, motion_y: i32) -> (i32, i32, usize) {
        let g = self.g;
        let mut dxy = 0usize;
        let mut src_x = src_x + (motion_x >> 1);
        let mut src_y = src_y + (motion_y >> 1);
        // Do not forget the half pels.
        src_x = src_x.clamp(-16, g.width as i32);
        if src_x != g.width as i32 {
            dxy |= (motion_x & 1) as usize;
        }
        src_y = src_y.clamp(-16, g.height as i32);
        if src_y != g.height as i32 {
            dxy |= ((motion_y & 1) << 1) as usize;
        }
        (src_x, src_y, dxy)
    }

    /// `hpel_motion`: one 8x8 luma block.
    #[allow(clippy::too_many_arguments)]
    fn hpel_motion(&self, dst: &mut Plane, dpos: usize, refp: &Plane, src_x: i32, src_y: i32, op: PixOp, motion_x: i32, motion_y: i32) {
        let (src_x, src_y, dxy) = self.hpel_source(src_x, src_y, motion_x, motion_y);
        mc_block(dst, dpos, refp, src_x, src_y, 8, 8, dxy, op);
    }

    /// `chroma_4mv_motion`: chroma of a 4MV macroblock from the summed
    /// luma vectors.
    fn chroma_4mv_motion(&self, cur: &mut MpvPicture, refp: &MpvPicture, op: PixOp, mx: i32, my: i32) {
        let g = self.g;
        let mut mx = h263_round_chroma(mx);
        let mut my = h263_round_chroma(my);
        let mut dxy = (((my & 1) << 1) | (mx & 1)) as usize;
        mx >>= 1;
        my >>= 1;
        let cw = (g.width >> 1) as i32;
        let ch = (g.height >> 1) as i32;
        let mut src_x = (self.mb_x * 8) as i32 + mx;
        let mut src_y = (self.mb_y * 8) as i32 + my;
        src_x = src_x.clamp(-8, cw);
        if src_x == cw {
            dxy &= !1;
        }
        src_y = src_y.clamp(-8, ch);
        if src_y == ch {
            dxy &= !2;
        }
        for c in 1..3 {
            let dpos = cur.planes[c].idx(self.mb_x * 8, self.mb_y * 8);
            mc_block(&mut cur.planes[c], dpos, &refp.planes[c], src_x, src_y, 8, 8, dxy, op);
        }
    }

    /// `mpeg_motion` for a 16x16 H.263 vector.
    fn mpeg_motion(&self, cur: &mut MpvPicture, refp: &MpvPicture, op: PixOp, motion_x: i32, motion_y: i32) {
        let dxy = (((motion_y & 1) << 1) | (motion_x & 1)) as usize;
        let src_x = (self.mb_x * 16) as i32 + (motion_x >> 1);
        let src_y = (self.mb_y * 16) as i32 + (motion_y >> 1);
        let uvdxy = dxy | (motion_y & 2) as usize | ((motion_x & 2) >> 1) as usize;
        let uvsrc_x = src_x >> 1;
        let uvsrc_y = src_y >> 1;
        let dpos = cur.planes[0].idx(self.mb_x * 16, self.mb_y * 16);
        mc_block(&mut cur.planes[0], dpos, &refp.planes[0], src_x, src_y, 16, 16, dxy, op);
        for c in 1..3 {
            let dpos = cur.planes[c].idx(self.mb_x * 8, self.mb_y * 8);
            mc_block(&mut cur.planes[c], dpos, &refp.planes[c], uvsrc_x, uvsrc_y, 8, 8, uvdxy, op);
        }
    }

    /// `apply_8x8` (half-pel).
    fn apply_8x8(&self, cur: &mut MpvPicture, refp: &MpvPicture, dir: usize, op: PixOp) {
        let mut mx = 0;
        let mut my = 0;
        for i in 0..4 {
            let x = self.mb_x * 16 + (i & 1) * 8;
            let y = self.mb_y * 16 + (i >> 1) * 8;
            let dpos = cur.planes[0].idx(x, y);
            let v = self.mv[dir][i];
            self.hpel_motion(&mut cur.planes[0], dpos, &refp.planes[0], x as i32, y as i32, op, v[0], v[1]);
            mx += v[0];
            my += v[1];
        }
        self.chroma_4mv_motion(cur, refp, op, mx, my);
    }

    /// `apply_obmc`: overlapped block motion compensation of a P macroblock.
    fn apply_obmc(&self, cur: &mut MpvPicture, refp: &MpvPicture, op: PixOp) {
        let g = self.g;
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let xy = mb_x + mb_y * g.mb_stride;
        let mot_stride = g.b8_stride as isize;
        let mot_xy = (mb_x * 2) as isize + (mb_y * 2) as isize * mot_stride;
        let mv = |b8: isize| -> [i32; 2] {
            let v = cur.mv[0][(MV_BASE as isize + b8) as usize];
            [v[0] as i32, v[1] as i32]
        };
        let mut cache = [[[0i32; 2]; 4]; 4];
        cache[1][1] = mv(mot_xy);
        cache[1][2] = mv(mot_xy + 1);
        cache[2][1] = mv(mot_xy + mot_stride);
        cache[2][2] = mv(mot_xy + mot_stride + 1);
        cache[3][1] = mv(mot_xy + mot_stride);
        cache[3][2] = mv(mot_xy + mot_stride + 1);
        if mb_y == 0 || is_intra(cur.mb_type[xy - g.mb_stride]) {
            cache[0][1] = cache[1][1];
            cache[0][2] = cache[1][2];
        } else {
            cache[0][1] = mv(mot_xy - mot_stride);
            cache[0][2] = mv(mot_xy - mot_stride + 1);
        }
        if mb_x == 0 || is_intra(cur.mb_type[xy - 1]) {
            cache[1][0] = cache[1][1];
            cache[2][0] = cache[2][1];
        } else {
            cache[1][0] = mv(mot_xy - 1);
            cache[2][0] = mv(mot_xy - 1 + mot_stride);
        }
        if mb_x + 1 >= g.mb_width || is_intra(cur.mb_type[xy + 1]) {
            cache[1][3] = cache[1][2];
            cache[2][3] = cache[2][2];
        } else {
            cache[1][3] = mv(mot_xy + 2);
            cache[2][3] = mv(mot_xy + 2 + mot_stride);
        }

        let mut mx = 0;
        let mut my = 0;
        for i in 0..4 {
            let x = (i & 1) + 1;
            let y = (i >> 1) + 1;
            // mid, top, left, right, bottom
            let mvs = [cache[y][x], cache[y - 1][x], cache[y][x - 1], cache[y][x + 1], cache[y + 1][x]];
            let bx = mb_x * 16 + (i & 1) * 8;
            let by = mb_y * 16 + (i >> 1) * 8;
            // obmc_motion: predict the block with each vector, sharing equal ones.
            let mut preds = [[0u8; 64]; 5];
            for k in 0..5 {
                if k != 0 && mvs[k] == mvs[0] {
                    preds[k] = preds[0];
                } else {
                    let (sx, sy, dxy) = self.hpel_source(bx as i32, by as i32, mvs[k][0], mvs[k][1]);
                    let mut src = [0u8; 9 * 9];
                    refp.planes[0].fetch(sx, sy, 9, 9, &mut src);
                    hpel(&mut preds[k], 0, 8, &src, 0, 9, 8, 8, dxy, op);
                }
            }
            let dpos = cur.planes[0].idx(bx, by);
            put_obmc(&mut cur.planes[0], dpos, &preds);
            mx += mvs[0][0];
            my += mvs[0][1];
        }
        self.chroma_4mv_motion(cur, refp, op, mx, my);
    }

    /// `ff_mpv_motion` for one direction.
    fn mpv_motion(&self, cur: &mut MpvPicture, dir: usize, op: PixOp) {
        let refp = if dir == 0 { self.last.as_ref() } else { self.next.as_ref() };
        let Some(refp) = refp.map(Arc::clone) else { return };
        if self.obmc && self.pict_type != PICT_B {
            self.apply_obmc(cur, &refp, op);
            return;
        }
        match self.mv_type {
            MV_TYPE_16X16 => self.mpeg_motion(cur, &refp, op, self.mv[dir][0][0], self.mv[dir][0][1]),
            _ => self.apply_8x8(cur, &refp, dir, op),
        }
    }

    /// `ff_mpv_reconstruct_mb` (H.263 path, no lowres).
    pub(crate) fn reconstruct_mb(&mut self) {
        let g = self.g;
        let mb_xy = self.mb_y * g.mb_stride + self.mb_x;
        let mut cur = self.cur.take().expect("picture in progress");
        cur.qscale[mb_xy] = self.qscale as i8;
        // mbskip_table bookkeeping only.
        self.mb_skipped = false;

        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        if !self.mb_intra {
            let mut op = if !self.no_rounding || self.pict_type == PICT_B { PixOp::Put } else { PixOp::PutNoRnd };
            if self.mv_dir & MV_DIR_FORWARD != 0 {
                self.mpv_motion(&mut cur, 0, op);
                op = PixOp::Avg;
            }
            if self.mv_dir & MV_DIR_BACKWARD != 0 {
                self.mpv_motion(&mut cur, 1, op);
            }
            // Add the dequantised residue.
            for i in 0..6 {
                if self.block_last_index[i] < 0 {
                    continue;
                }
                let n_coeffs = RASTER_END[self.block_last_index[i] as usize] as usize;
                let q = if i < 4 { self.qscale } else { self.chroma_qscale };
                unquantize_h263_inter(&mut self.block[i], q, n_coeffs);
                let (plane, pos) = block_dest(&cur, i, mb_x, mb_y);
                let p = &mut cur.planes[plane];
                let stride = p.stride;
                simple_idct_add(&mut p.data, pos, stride, &mut self.block[i]);
            }
        } else {
            for i in 0..6 {
                let q = if i < 4 { self.qscale } else { self.chroma_qscale };
                let dc_scale = if self.h263_aic { None } else { Some(if i < 4 { self.y_dc_scale } else { self.c_dc_scale }) };
                let n_coeffs = if self.ac_pred { 63 } else { RASTER_END[self.block_last_index[i].clamp(0, 63) as usize] as usize };
                unquantize_h263_intra(&mut self.block[i], q, dc_scale, n_coeffs);
                let (plane, pos) = block_dest(&cur, i, mb_x, mb_y);
                let p = &mut cur.planes[plane];
                let stride = p.stride;
                simple_idct_put(&mut p.data, pos, stride, &mut self.block[i]);
            }
        }
        self.cur = Some(cur);
    }
}

/// Plane and position of block `i` (0-3 luma, 4 Cb, 5 Cr) of a macroblock.
fn block_dest(cur: &MpvPicture, i: usize, mb_x: usize, mb_y: usize) -> (usize, usize) {
    match i {
        0..=3 => (0, cur.planes[0].idx(mb_x * 16 + (i & 1) * 8, mb_y * 16 + (i >> 1) * 8)),
        4 => (1, cur.planes[1].idx(mb_x * 8, mb_y * 8)),
        _ => (2, cur.planes[2].idx(mb_x * 8, mb_y * 8)),
    }
}

/// `put_obmc`: blends the five predictions (mid, top, left, right, bottom)
/// of an 8x8 block.
fn put_obmc(dst: &mut Plane, dpos: usize, src: &[[u8; 64]; 5]) {
    // Weights (top, left, mid, right, bottom) per sample, from put_obmc().
    const W: [[[u8; 5]; 8]; 8] = obmc_weights();
    let stride = dst.stride;
    for y in 0..8 {
        for x in 0..8 {
            let [t, l, m, r, b] = W[y][x];
            let i = y * 8 + x;
            let v = t as u32 * src[1][i] as u32
                + l as u32 * src[2][i] as u32
                + m as u32 * src[0][i] as u32
                + r as u32 * src[3][i] as u32
                + b as u32 * src[4][i] as u32
                + 4;
            dst.data[dpos + y * stride + x] = (v >> 3) as u8;
        }
    }
}

/// The per-sample weights of `put_obmc` (`OBMC_FILTER`/`OBMC_FILTER4`).
const fn obmc_weights() -> [[[u8; 5]; 8]; 8] {
    let mut w = [[[0u8; 5]; 8]; 8];
    // Each entry: (row, col, rows, cols, [t, l, m, r, b]) as put_obmc lays out.
    let spec: [(usize, usize, usize, usize, [u8; 5]); 28] = [
        (0, 0, 1, 1, [2, 2, 4, 0, 0]),
        (0, 1, 1, 1, [2, 1, 5, 0, 0]),
        (0, 2, 2, 2, [2, 1, 5, 0, 0]),
        (0, 4, 2, 2, [2, 0, 5, 1, 0]),
        (0, 6, 1, 1, [2, 0, 5, 1, 0]),
        (0, 7, 1, 1, [2, 0, 4, 2, 0]),
        (1, 0, 1, 1, [1, 2, 5, 0, 0]),
        (1, 1, 1, 1, [1, 2, 5, 0, 0]),
        (1, 6, 1, 1, [1, 0, 5, 2, 0]),
        (1, 7, 1, 1, [1, 0, 5, 2, 0]),
        (2, 0, 2, 2, [1, 2, 5, 0, 0]),
        (2, 2, 2, 2, [1, 1, 6, 0, 0]),
        (2, 4, 2, 2, [1, 0, 6, 1, 0]),
        (2, 6, 2, 2, [1, 0, 5, 2, 0]),
        (4, 0, 2, 2, [0, 2, 5, 0, 1]),
        (4, 2, 2, 2, [0, 1, 6, 0, 1]),
        (4, 4, 2, 2, [0, 0, 6, 1, 1]),
        (4, 6, 2, 2, [0, 0, 5, 2, 1]),
        (6, 0, 1, 1, [0, 2, 5, 0, 1]),
        (6, 1, 1, 1, [0, 2, 5, 0, 1]),
        (6, 2, 2, 2, [0, 1, 5, 0, 2]),
        (6, 4, 2, 2, [0, 0, 5, 1, 2]),
        (6, 6, 1, 1, [0, 0, 5, 2, 1]),
        (6, 7, 1, 1, [0, 0, 5, 2, 1]),
        (7, 0, 1, 1, [0, 2, 4, 0, 2]),
        (7, 1, 1, 1, [0, 1, 5, 0, 2]),
        (7, 6, 1, 1, [0, 0, 5, 1, 2]),
        (7, 7, 1, 1, [0, 0, 4, 2, 2]),
    ];
    let mut k = 0;
    while k < spec.len() {
        let (r0, c0, rows, cols, wt) = spec[k];
        let mut r = 0;
        while r < rows {
            let mut c = 0;
            while c < cols {
                w[r0 + r][c0 + c] = wt;
                c += 1;
            }
            r += 1;
        }
        k += 1;
    }
    w
}

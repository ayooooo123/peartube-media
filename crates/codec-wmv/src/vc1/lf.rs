//! VC-1 overlap smoothing and in-loop deblocking.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vc1_loopfilter.c`
//! (LGPL-2.1-or-later). The kernels (`vc1_[hv]_s_overlap`,
//! `vc1_[hv]_loop_filter{4,8,16}`) are in `dsp.rs`.
//!
//! Every pass trails the decoding loop: the overlap filter by one MB
//! column (H) and row (V), the loop filters by up to two rows and columns.
//! A group of calls is addressed by how many MB rows (`dy`) and columns
//! (`dx`) it lies behind the current macroblock.

use super::*;

const LEFT_EDGE: u32 = 1 << 0;
const RIGHT_EDGE: u32 = 1 << 1;
const TOP_EDGE: u32 = 1 << 2;
const BOTTOM_EDGE: u32 = 1 << 3;

impl Vc1Decoder {
    // ───────────────────────── overlap smoothing ─────────────────────────

    fn over_flag(&self, mb_pos: isize) -> bool {
        mb_pos >= 0 && self.over_flags_plane.get(mb_pos as usize).is_some_and(|&f| f != 0)
    }

    fn fieldtx_at(&self, mb_pos: isize) -> bool {
        mb_pos >= 0 && self.fieldtx_plane.get(mb_pos as usize).is_some_and(|&f| f != 0)
    }

    /// `vc1_h_overlap_filter` on blocks of the ring entries `left`/`right`.
    fn h_overlap_filter(&mut self, left: isize, right: isize, left_fieldtx: bool, right_fieldtx: bool, block_num: usize) {
        let (lf, rf) = (left_fieldtx as usize, right_fieldtx as usize);
        let (ls, rs) = if lf != rf { (16 - 8 * lf, 16 - 8 * rf) } else { (8, 8) };
        let any = lf | rf != 0;
        let (l, r, ls, rs, flags) = match block_num {
            0 => (self.blk_off(left, 2), self.blk_off(right, 0), ls, rs, if any { 0 } else { 1 }),
            1 => (self.blk_off(right, 0), self.blk_off(right, 2), 8, 8, if rf != 0 { 0 } else { 1 }),
            2 => (
                if lf == 0 && rf != 0 { self.blk_off(left, 2) + 8 } else { self.blk_off(left, 3) },
                if lf != 0 && rf == 0 { self.blk_off(right, 0) + 8 } else { self.blk_off(right, 1) },
                ls,
                rs,
                if any { 2 } else { 1 },
            ),
            3 => (self.blk_off(right, 1), self.blk_off(right, 3), 8, 8, if rf != 0 { 2 } else { 1 }),
            _ => (self.blk_off(left, block_num), self.blk_off(right, block_num), 8, 8, 1),
        };
        dsp::h_s_overlap(&mut self.blk, l, r, ls, rs, flags);
    }

    /// `vc1_v_overlap_filter`.
    fn v_overlap_filter(&mut self, top: isize, bottom: isize, block_num: usize) {
        let (t, b) = match block_num {
            0 => (self.blk_off(top, 1), self.blk_off(bottom, 0)),
            1 => (self.blk_off(top, 3), self.blk_off(bottom, 2)),
            2 => (self.blk_off(bottom, 0), self.blk_off(bottom, 1)),
            3 => (self.blk_off(bottom, 2), self.blk_off(bottom, 3)),
            _ => (self.blk_off(top, block_num), self.blk_off(bottom, block_num)),
        };
        dsp::v_s_overlap(&mut self.blk, t, b);
    }

    /// `ff_vc1_i_overlap_filter`.
    pub(crate) fn i_overlap_filter(&mut self) {
        let mb_pos = self.mb_pos() as isize;
        let mbs = self.mb_stride as isize;
        let (topleft, top, left, cur) = (self.topleft_blk_idx, self.top_blk_idx, self.left_blk_idx, self.cur_blk_idx);
        let adv = self.profile == PROFILE_ADVANCED;
        let all = self.condover == CONDOVER_ALL;
        let ilace_frame = self.fcm == ILACE_FRAME;
        for i in 0..6 {
            if self.mb_x == 0 && (i & 5) != 1 {
                continue;
            }
            if self.pq >= 9
                || (adv && (all || (self.over_flag(mb_pos) && ((i & 5) == 1 || self.over_flag(mb_pos - 1)))))
            {
                let lf = ilace_frame && self.mb_x != 0 && self.fieldtx_at(mb_pos - 1);
                let rf = ilace_frame && self.fieldtx_at(mb_pos);
                self.h_overlap_filter(if self.mb_x != 0 { left } else { cur }, cur, lf, rf, i);
            }
        }
        if !ilace_frame {
            for i in 0..6 {
                if self.first_slice_line && (i & 2) == 0 {
                    continue;
                }
                if self.mb_x != 0
                    && (self.pq >= 9
                        || (adv
                            && (all
                                || (self.over_flag(mb_pos - 1)
                                    && ((i & 2) != 0 || self.over_flag(mb_pos - 1 - mbs))))))
                {
                    self.v_overlap_filter(if self.first_slice_line { left } else { topleft }, left, i);
                }
                if self.mb_x + 1 == self.mb_width
                    && (self.pq >= 9
                        || (adv
                            && (all || (self.over_flag(mb_pos) && ((i & 2) != 0 || self.over_flag(mb_pos - mbs))))))
                {
                    self.v_overlap_filter(if self.first_slice_line { cur } else { top }, cur, i);
                }
            }
        }
    }

    /// `ff_vc1_p_overlap_filter`.
    pub(crate) fn p_overlap_filter(&mut self) {
        let mb_pos = self.mb_pos() as isize;
        let (topleft, top, left, cur) = (self.topleft_blk_idx, self.top_blk_idx, self.left_blk_idx, self.cur_blk_idx);
        let ilace_frame = self.fcm == ILACE_FRAME;
        for i in 0..6 {
            if self.mb_x == 0 && (i & 5) != 1 {
                continue;
            }
            let bi = self.block_index[i];
            if self.vmbt(bi) != 0 && self.vmbt(bi - 1) != 0 {
                let lf = ilace_frame && self.mb_x != 0 && self.fieldtx_at(mb_pos - 1);
                let rf = ilace_frame && self.fieldtx_at(mb_pos);
                self.h_overlap_filter(if self.mb_x != 0 { left } else { cur }, cur, lf, rf, i);
            }
        }
        if !ilace_frame {
            for i in 0..6 {
                if self.first_slice_line && (i & 2) == 0 {
                    continue;
                }
                let bi = self.block_index[i];
                let wrap = self.block_wrap(i);
                let c = (i > 3) as isize;
                if self.mb_x != 0 && self.vmbt(bi - 2 + c) != 0 && self.vmbt(bi - wrap - 2 + c) != 0 {
                    self.v_overlap_filter(if self.first_slice_line { left } else { topleft }, left, i);
                }
                if self.mb_x + 1 == self.mb_width && self.vmbt(bi) != 0 && self.vmbt(bi - wrap) != 0 {
                    self.v_overlap_filter(if self.first_slice_line { cur } else { top }, cur, i);
                }
            }
        }
    }

    // ───────────────────────── pixel filters ─────────────────────────

    /// `vc1_v_loop_filter{4,8,16}`: the horizontal edge above `off`, `len`
    /// pixels wide, when all taps lie inside the plane.
    fn vlf(&mut self, plane: usize, off: isize, stride: isize, len: usize) {
        let pq = self.pq;
        let Some(cur) = self.cur.as_mut() else { return };
        let data = &mut cur.pic.data[plane];
        if off - 4 * stride >= 0 && off + 3 * stride + len as isize <= data.len() as isize {
            dsp::v_loop_filter(data, off, stride, len, pq);
        }
    }

    /// `vc1_h_loop_filter{4,8,16}`: the vertical edge left of `off`, `len`
    /// pixels tall.
    fn hlf(&mut self, plane: usize, off: isize, stride: isize, len: usize) {
        let pq = self.pq;
        let Some(cur) = self.cur.as_mut() else { return };
        let data = &mut cur.pic.data[plane];
        if off - 4 >= 0 && off + (len as isize - 1) * stride + 4 <= data.len() as isize {
            dsp::h_loop_filter(data, off, stride, len, pq);
        }
    }

    /// Plane, top-left pixel of block `n` and the plane's line size, for the
    /// macroblock `dy` rows and `dx` columns behind the current one.
    fn lf_block(&self, n: usize, dy: isize, dx: isize) -> (usize, isize, isize) {
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        if n > 3 {
            (n - 3, self.dest[n - 3] - 8 * dy * uvls - 8 * dx, uvls)
        } else {
            let mb = self.dest[0] - 16 * dy * ls - 16 * dx;
            (0, mb + (n as isize & 2) * 4 * ls + (n as isize & 1) * 8, ls)
        }
    }

    /// `v->cbp` / `is_intra` / `luma_mv` / `ttblk` index of the macroblock
    /// `dy` rows and `dx` columns behind the current one.
    fn lf_mb(&self, dy: isize, dx: isize) -> usize {
        self.row3(self.mb_x as isize - dy * self.mb_stride as isize - dx)
    }

    fn mv_f_at(&self, bi: isize) -> u8 {
        let o = self.mvf_idx(0, bi);
        self.mv_f.get(o).copied().unwrap_or(0)
    }

    // ── I pictures ──

    /// `vc1_i_h_loop_filter`.
    fn i_h_lf(&mut self, dy: isize, dx: isize, flags: u32, n: usize) {
        if n & 2 != 0 {
            return;
        }
        if flags & LEFT_EDGE == 0 || (n & 5) == 1 {
            let (plane, dst, ls) = self.lf_block(n, dy, dx);
            if self.fcm == ILACE_FRAME {
                if n > 3 {
                    self.hlf(plane, dst, 2 * ls, 4);
                    self.hlf(plane, dst + ls, 2 * ls, 4);
                } else {
                    self.hlf(plane, dst, 2 * ls, 8);
                    self.hlf(plane, dst + ls, 2 * ls, 8);
                }
            } else if n > 3 {
                self.hlf(plane, dst, ls, 8);
            } else {
                self.hlf(plane, dst, ls, 16);
            }
        }
    }

    /// `vc1_i_v_loop_filter`.
    fn i_v_lf(&mut self, dy: isize, dx: isize, flags: u32, fieldtx: bool, n: usize) {
        if (n & 5) == 1 {
            return;
        }
        if flags & TOP_EDGE == 0 || n & 2 != 0 {
            let (plane, dst, ls) = self.lf_block(n, dy, dx);
            if self.fcm == ILACE_FRAME {
                if n > 3 {
                    self.vlf(plane, dst, 2 * ls, 8);
                    self.vlf(plane, dst + ls, 2 * ls, 8);
                } else if n < 2 || !fieldtx {
                    self.vlf(plane, dst, 2 * ls, 16);
                    self.vlf(plane, dst + ls, 2 * ls, 16);
                }
            } else if n > 3 {
                self.vlf(plane, dst, ls, 8);
            } else {
                self.vlf(plane, dst, ls, 16);
            }
        }
    }

    fn i_v_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        let fieldtx = self.fieldtx_at(self.mb_pos() as isize - dy * self.mb_stride as isize - dx);
        for i in 0..6 {
            self.i_v_lf(dy, dx, flags, fieldtx, i);
        }
    }

    fn i_h_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        for i in 0..6 {
            self.i_h_lf(dy, dx, flags, i);
        }
    }

    /// `ff_vc1_i_loop_filter`.
    pub(crate) fn i_loop_filter(&mut self) {
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let last_col = mb_x + 1 == self.end_mb_x;
        let last_row = mb_y + 1 == self.end_mb_y;
        if !self.first_slice_line {
            let flags = if mb_y == self.start_mb_y + 1 { TOP_EDGE } else { 0 };
            if mb_x != 0 {
                self.i_v_lf_mb(1, 1, flags);
            }
            if last_col {
                self.i_v_lf_mb(1, 0, flags);
            }
        }
        if last_row {
            let flags = if self.first_slice_line { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
            if mb_x != 0 {
                self.i_v_lf_mb(0, 1, flags);
            }
            if last_col {
                self.i_v_lf_mb(0, 0, flags);
            }
        }
        if mb_y >= self.start_mb_y + 2 {
            if mb_x != 0 {
                self.i_h_lf_mb(2, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                self.i_h_lf_mb(2, 0, if mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE });
            }
        }
        if last_row {
            if mb_y >= self.start_mb_y + 1 {
                if mb_x != 0 {
                    self.i_h_lf_mb(1, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                }
                if last_col {
                    self.i_h_lf_mb(1, 0, if mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE });
                }
            }
            if mb_x != 0 {
                self.i_h_lf_mb(0, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                self.i_h_lf_mb(0, 0, if mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE });
            }
        }
    }

    // ── P pictures ──

    /// `vc1_p_h_loop_filter` for block `n` of the macroblock `dy`/`dx`
    /// behind the current one.
    fn p_h_lf(&mut self, dy: isize, dx: isize, flags: u32, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let left_cbp = self.cbp_base[c] >> (n * 4);
        if flags & RIGHT_EDGE == 0 || n & 5 == 0 {
            let left_is_intra = self.is_intra_base[c] & (1 << n);
            let (right_is_intra, right_cbp) = if n > 3 {
                (self.is_intra_base[c + 1] & (1 << n), self.cbp_base[c + 1] >> (n * 4))
            } else if n & 1 != 0 {
                (self.is_intra_base[c + 1] & (1 << (n - 1)), self.cbp_base[c + 1] >> ((n - 1) * 4))
            } else {
                (self.is_intra_base[c] & (1 << (n + 1)), self.cbp_base[c] >> ((n + 1) * 4))
            };
            let differ = if n > 3 {
                let mvf = self.block_index[n] - dy * self.mb_stride as isize - dx + self.mb_off;
                self.luma_mv_base[c] != self.luma_mv_base[c + 1]
                    || (self.fcm == ILACE_FIELD && self.mv_f_at(mvf) != self.mv_f_at(mvf + 1))
            } else {
                let bi = self.block_index[n] - 2 * dy * self.b8_stride as isize - 2 * dx + self.blocks_off;
                self.mv_at(0, bi) != self.mv_at(0, bi + 1)
                    || (self.fcm == ILACE_FIELD && self.mv_f_at(bi) != self.mv_f_at(bi + 1))
            };
            if left_is_intra != 0 || right_is_intra != 0 || differ {
                self.hlf(plane, dst + 8, ls, 8);
            } else {
                let idx = (left_cbp | (right_cbp >> 1)) & 5;
                if idx & 1 != 0 {
                    self.hlf(plane, dst + 4 * ls + 8, ls, 4);
                }
                if idx & 4 != 0 {
                    self.hlf(plane, dst + 8, ls, 4);
                }
            }
        }
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_4X8 {
            if left_cbp & 3 != 0 {
                self.hlf(plane, dst + 4 * ls + 4, ls, 4);
            }
            if left_cbp & 12 != 0 {
                self.hlf(plane, dst + 4, ls, 4);
            }
        }
    }

    /// `vc1_p_v_loop_filter`.
    fn p_v_lf(&mut self, dy: isize, dx: isize, flags: u32, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let mbs = self.mb_stride;
        let top_cbp = self.cbp_base[c] >> (n * 4);
        if flags & BOTTOM_EDGE == 0 || n < 2 {
            let top_is_intra = self.is_intra_base[c] & (1 << n);
            let (bottom_is_intra, bottom_cbp) = if n > 3 {
                (self.is_intra_base[c + mbs] & (1 << n), self.cbp_base[c + mbs] >> (n * 4))
            } else if n < 2 {
                (self.is_intra_base[c] & (1 << (n + 2)), self.cbp_base[c] >> ((n + 2) * 4))
            } else {
                (self.is_intra_base[c + mbs] & (1 << (n - 2)), self.cbp_base[c + mbs] >> ((n - 2) * 4))
            };
            let differ = if n > 3 {
                let mvf = self.block_index[n] - dy * mbs as isize - dx + self.mb_off;
                self.luma_mv_base[c] != self.luma_mv_base[c + mbs]
                    || (self.fcm == ILACE_FIELD && self.mv_f_at(mvf) != self.mv_f_at(mvf + mbs as isize))
            } else {
                let b8 = self.b8_stride as isize;
                let bi = self.block_index[n] - 2 * dy * b8 - 2 * dx + self.blocks_off;
                self.mv_at(0, bi) != self.mv_at(0, bi + b8)
                    || (self.fcm == ILACE_FIELD && self.mv_f_at(bi) != self.mv_f_at(bi + b8))
            };
            if top_is_intra != 0 || bottom_is_intra != 0 || differ {
                self.vlf(plane, dst + 8 * ls, ls, 8);
            } else {
                let idx = (top_cbp | (bottom_cbp >> 2)) & 3;
                if idx & 1 != 0 {
                    self.vlf(plane, dst + 8 * ls + 4, ls, 4);
                }
                if idx & 2 != 0 {
                    self.vlf(plane, dst + 8 * ls, ls, 4);
                }
            }
        }
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_8X4 {
            if top_cbp & 5 != 0 {
                self.vlf(plane, dst + 4 * ls + 4, ls, 4);
            }
            if top_cbp & 10 != 0 {
                self.vlf(plane, dst + 4 * ls, ls, 4);
            }
        }
    }

    fn p_v_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        for i in 0..6 {
            self.p_v_lf(dy, dx, flags, i);
        }
    }

    fn p_h_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        for i in 0..6 {
            self.p_h_lf(dy, dx, flags, i);
        }
    }

    /// `ff_vc1_p_loop_filter`.
    pub(crate) fn p_loop_filter(&mut self) {
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let start = self.start_mb_y;
        let last_col = mb_x + 1 == self.mb_width;
        let last_row = mb_y + 1 == self.end_mb_y;
        if mb_y >= start + 2 {
            let flags = if mb_y == start + 2 { TOP_EDGE } else { 0 };
            if mb_x != 0 {
                self.p_v_lf_mb(2, 1, flags);
            }
            if last_col {
                self.p_v_lf_mb(2, 0, flags);
            }
        }
        if last_row {
            if mb_x != 0 {
                if mb_y >= start + 1 {
                    self.p_v_lf_mb(1, 1, if mb_y == start + 1 { TOP_EDGE } else { 0 });
                }
                self.p_v_lf_mb(0, 1, if mb_y == start { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE });
            }
            if last_col {
                if mb_y >= start + 1 {
                    self.p_v_lf_mb(1, 0, if mb_y == start + 1 { TOP_EDGE } else { 0 });
                }
                self.p_v_lf_mb(0, 0, if mb_y == start { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE });
            }
        }
        if mb_y >= start + 2 {
            if mb_x >= 2 {
                self.p_h_lf_mb(2, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                if mb_x >= 1 {
                    self.p_h_lf_mb(2, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                }
                self.p_h_lf_mb(2, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
            }
        }
        if last_row {
            if mb_y >= start + 1 {
                if mb_x >= 2 {
                    self.p_h_lf_mb(1, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
                }
                if last_col {
                    if mb_x >= 1 {
                        self.p_h_lf_mb(1, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                    }
                    self.p_h_lf_mb(1, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
                }
            }
            if mb_x >= 2 {
                self.p_h_lf_mb(0, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                if mb_x >= 1 {
                    self.p_h_lf_mb(0, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                }
                self.p_h_lf_mb(0, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
            }
        }
    }

    // ── interlaced-frame P/B pictures ──

    /// `vc1_p_h_intfr_loop_filter`.
    fn p_h_intfr_lf(&mut self, dy: isize, dx: isize, flags: u32, fieldtx: bool, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        let t4 = tt == TT_4X4 || tt == TT_4X8;
        if n < 4 {
            if fieldtx {
                if n < 2 {
                    if t4 {
                        self.hlf(plane, dst + 4, 2 * ls, 8);
                    }
                    if flags & RIGHT_EDGE == 0 || n == 0 {
                        self.hlf(plane, dst + 8, 2 * ls, 8);
                    }
                } else {
                    if t4 {
                        self.hlf(plane, dst - 7 * ls + 4, 2 * ls, 8);
                    }
                    if flags & RIGHT_EDGE == 0 || n == 2 {
                        self.hlf(plane, dst - 7 * ls + 8, 2 * ls, 8);
                    }
                }
            } else {
                if t4 {
                    self.hlf(plane, dst + 4, 2 * ls, 4);
                    self.hlf(plane, dst + ls + 4, 2 * ls, 4);
                }
                if flags & RIGHT_EDGE == 0 || n & 5 == 0 {
                    self.hlf(plane, dst + 8, 2 * ls, 4);
                    self.hlf(plane, dst + ls + 8, 2 * ls, 4);
                }
            }
        } else {
            if t4 {
                self.hlf(plane, dst + 4, 2 * ls, 4);
                self.hlf(plane, dst + ls + 4, 2 * ls, 4);
            }
            if flags & RIGHT_EDGE == 0 {
                self.hlf(plane, dst + 8, 2 * ls, 4);
                self.hlf(plane, dst + ls + 8, 2 * ls, 4);
            }
        }
    }

    /// `vc1_p_v_intfr_loop_filter`.
    fn p_v_intfr_lf(&mut self, dy: isize, dx: isize, flags: u32, fieldtx: bool, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        let t4 = tt == TT_4X4 || tt == TT_8X4;
        if n < 4 {
            if fieldtx {
                if n < 2 {
                    if t4 {
                        self.vlf(plane, dst + 8 * ls, 2 * ls, 8);
                    }
                    if flags & BOTTOM_EDGE == 0 {
                        self.vlf(plane, dst + 16 * ls, 2 * ls, 8);
                    }
                } else {
                    if t4 {
                        self.vlf(plane, dst + ls, 2 * ls, 8);
                    }
                    if flags & BOTTOM_EDGE == 0 {
                        self.vlf(plane, dst + 9 * ls, 2 * ls, 8);
                    }
                }
            } else if n < 2 {
                if flags & TOP_EDGE == 0 && t4 {
                    self.vlf(plane, dst + 4 * ls, 2 * ls, 8);
                    self.vlf(plane, dst + 5 * ls, 2 * ls, 8);
                }
                self.vlf(plane, dst + 8 * ls, 2 * ls, 8);
                self.vlf(plane, dst + 9 * ls, 2 * ls, 8);
            } else if flags & BOTTOM_EDGE == 0 {
                if t4 {
                    self.vlf(plane, dst + 4 * ls, 2 * ls, 8);
                    self.vlf(plane, dst + 5 * ls, 2 * ls, 8);
                }
                self.vlf(plane, dst + 8 * ls, 2 * ls, 8);
                self.vlf(plane, dst + 9 * ls, 2 * ls, 8);
            }
        } else if flags & BOTTOM_EDGE == 0 {
            if flags & TOP_EDGE == 0 && t4 {
                self.vlf(plane, dst + 4 * ls, 2 * ls, 8);
                self.vlf(plane, dst + 5 * ls, 2 * ls, 8);
            }
            self.vlf(plane, dst + 8 * ls, 2 * ls, 8);
            self.vlf(plane, dst + 9 * ls, 2 * ls, 8);
        }
    }

    fn p_v_intfr_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        let fieldtx = self.fieldtx_at(self.mb_pos() as isize - dy * self.mb_stride as isize - dx);
        for i in 0..6 {
            self.p_v_intfr_lf(dy, dx, flags, fieldtx, i);
        }
    }

    fn p_h_intfr_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        let fieldtx = self.fieldtx_at(self.mb_pos() as isize - dy * self.mb_stride as isize - dx);
        for i in 0..6 {
            self.p_h_intfr_lf(dy, dx, flags, fieldtx, i);
        }
    }

    /// `ff_vc1_p_intfr_loop_filter`.
    pub(crate) fn p_intfr_loop_filter(&mut self) {
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let start = self.start_mb_y;
        let last_col = mb_x + 1 == self.mb_width;
        let last_row = mb_y + 1 == self.end_mb_y;
        if mb_x != 0 && mb_y >= start + 1 {
            self.p_v_intfr_lf_mb(1, 1, if mb_y == start + 1 { TOP_EDGE } else { 0 });
        }
        if last_col && mb_y >= start + 1 {
            self.p_v_intfr_lf_mb(1, 0, if mb_y == start + 1 { TOP_EDGE } else { 0 });
        }
        if last_row {
            let flags = if mb_y == start { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE };
            if mb_x != 0 {
                self.p_v_intfr_lf_mb(0, 1, flags);
            }
            if last_col {
                self.p_v_intfr_lf_mb(0, 0, flags);
            }
        }
        if mb_y >= start + 2 {
            if mb_x >= 2 {
                self.p_h_intfr_lf_mb(2, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                if mb_x >= 1 {
                    self.p_h_intfr_lf_mb(2, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                }
                self.p_h_intfr_lf_mb(2, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
            }
        }
        if last_row {
            if mb_y >= start + 1 {
                if mb_x >= 2 {
                    self.p_h_intfr_lf_mb(1, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
                }
                if last_col {
                    if mb_x >= 1 {
                        self.p_h_intfr_lf_mb(1, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                    }
                    self.p_h_intfr_lf_mb(1, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
                }
            }
            if mb_x >= 2 {
                self.p_h_intfr_lf_mb(0, 2, if mb_x == 2 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                if mb_x >= 1 {
                    self.p_h_intfr_lf_mb(0, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
                }
                self.p_h_intfr_lf_mb(0, 0, if mb_x != 0 { RIGHT_EDGE } else { LEFT_EDGE | RIGHT_EDGE });
            }
        }
    }

    // ── interlaced-field B pictures ──

    /// `vc1_b_h_intfi_loop_filter`.
    fn b_h_intfi_lf(&mut self, dy: isize, dx: isize, flags: u32, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let block_cbp = self.cbp_base[c] >> (n * 4);
        if flags & RIGHT_EDGE == 0 || n & 5 == 0 {
            self.hlf(plane, dst + 8, ls, 8);
        }
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_4X8 {
            let idx = (block_cbp | (block_cbp >> 1)) & 5;
            if idx & 1 != 0 {
                self.hlf(plane, dst + 4 * ls + 4, ls, 4);
            }
            if idx & 4 != 0 {
                self.hlf(plane, dst + 4, ls, 4);
            }
        }
    }

    /// `vc1_b_v_intfi_loop_filter`.
    fn b_v_intfi_lf(&mut self, dy: isize, dx: isize, flags: u32, n: usize) {
        let (plane, dst, ls) = self.lf_block(n, dy, dx);
        let c = self.lf_mb(dy, dx);
        let block_cbp = self.cbp_base[c] >> (n * 4);
        if flags & BOTTOM_EDGE == 0 || n < 2 {
            self.vlf(plane, dst + 8 * ls, ls, 8);
        }
        let tt = (self.ttblk_base[c] >> (n * 4)) & 0xf;
        if tt == TT_4X4 || tt == TT_8X4 {
            let idx = (block_cbp | (block_cbp >> 2)) & 3;
            if idx & 1 != 0 {
                self.vlf(plane, dst + 4 * ls + 4, ls, 4);
            }
            if idx & 2 != 0 {
                self.vlf(plane, dst + 4 * ls, ls, 4);
            }
        }
    }

    fn b_v_intfi_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        for i in 0..6 {
            self.b_v_intfi_lf(dy, dx, flags, i);
        }
    }

    fn b_h_intfi_lf_mb(&mut self, dy: isize, dx: isize, flags: u32) {
        for i in 0..6 {
            self.b_h_intfi_lf(dy, dx, flags, i);
        }
    }

    /// `ff_vc1_b_intfi_loop_filter`.
    pub(crate) fn b_intfi_loop_filter(&mut self) {
        let mb_x = self.mb_x;
        let last_col = mb_x + 1 == self.mb_width;
        let last_row = self.mb_y + 1 == self.end_mb_y;
        if !self.first_slice_line {
            self.b_v_intfi_lf_mb(1, 0, if self.mb_y == self.start_mb_y + 1 { TOP_EDGE } else { 0 });
        }
        if last_row {
            self.b_v_intfi_lf_mb(0, 0, if self.first_slice_line { TOP_EDGE | BOTTOM_EDGE } else { BOTTOM_EDGE });
        }
        if !self.first_slice_line {
            if mb_x != 0 {
                self.b_h_intfi_lf_mb(1, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                self.b_h_intfi_lf_mb(1, 0, if mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE });
            }
        }
        if last_row {
            if mb_x != 0 {
                self.b_h_intfi_lf_mb(0, 1, if mb_x == 1 { LEFT_EDGE } else { 0 });
            }
            if last_col {
                self.b_h_intfi_lf_mb(0, 0, if mb_x == 0 { LEFT_EDGE | RIGHT_EDGE } else { RIGHT_EDGE });
            }
        }
    }
}

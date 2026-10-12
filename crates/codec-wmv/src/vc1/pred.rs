//! VC-1 motion vector prediction.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vc1_pred.c` and
//! `libavcodec/vc1_pred.h` (LGPL-2.1-or-later).

use super::*;
use crate::bits::BitReader;
use crate::mpv::mid_pred;

/// `scale_mv` (B_FRACTION_DEN == 256).
#[inline]
pub(crate) fn scale_mv(value: i32, bfrac: i32, inv: i32, qs: bool) -> i32 {
    let mut n = bfrac;
    if inv != 0 {
        n -= 256;
    }
    if !qs {
        return 2 * (value.wrapping_mul(n).wrapping_add(255) >> 9);
    }
    value.wrapping_mul(n).wrapping_add(128) >> 8
}

impl Vc1Decoder {
    #[inline]
    pub(crate) fn mvf_idx(&self, dir: usize, bi: isize) -> usize {
        dir * self.mv_f_size + (self.bo() + bi) as usize
    }

    fn refdist_for(&self, dir: usize) -> usize {
        let r = if self.pict_type != PICT_B {
            self.refdist
        } else if dir != 0 {
            self.brfd
        } else {
            self.frfd
        };
        r.clamp(0, 3) as usize
    }

    fn scaleforsame_x(&self, n: i32, dir: usize) -> i32 {
        let t = dir ^ self.second_field as usize;
        let rd = self.refdist_for(dir);
        let s = &VC1_FIELD_MVPRED_SCALES[t];
        let (scalesame1, scalesame2, scalezone1_x, zone1offset_x) = (s[1][rd], s[2][rd], s[3][rd], s[5][rd]);
        let v = if n.abs() > 255 {
            n
        } else if n.abs() < scalezone1_x {
            (n * scalesame1) >> 8
        } else if n < 0 {
            ((n * scalesame2) >> 8) - zone1offset_x
        } else {
            ((n * scalesame2) >> 8) + zone1offset_x
        };
        v.clamp(-self.range_x, self.range_x - 1)
    }

    fn clip_y(&self, v: i32, dir: usize) -> i32 {
        if self.cur_field_type != 0 && self.ref_field_type[dir] == 0 {
            v.clamp(-self.range_y / 2 + 1, self.range_y / 2)
        } else {
            v.clamp(-self.range_y / 2, self.range_y / 2 - 1)
        }
    }

    fn scaleforsame_y(&self, n: i32, dir: usize) -> i32 {
        let t = dir ^ self.second_field as usize;
        let rd = self.refdist_for(dir);
        let s = &VC1_FIELD_MVPRED_SCALES[t];
        let (scalesame1, scalesame2, scalezone1_y, zone1offset_y) = (s[1][rd], s[2][rd], s[4][rd], s[6][rd]);
        let v = if n.abs() > 63 {
            n
        } else if n.abs() < scalezone1_y {
            (n * scalesame1) >> 8
        } else if n < 0 {
            ((n * scalesame2) >> 8) - zone1offset_y
        } else {
            ((n * scalesame2) >> 8) + zone1offset_y
        };
        self.clip_y(v, dir)
    }

    fn scaleforopp_x(&self, n: i32) -> i32 {
        let brfd = self.brfd.clamp(0, 3) as usize;
        let s = &VC1_B_FIELD_MVPRED_SCALES;
        let (scalezone1_x, zone1offset_x, scaleopp1, scaleopp2) = (s[3][brfd], s[5][brfd], s[1][brfd], s[2][brfd]);
        let v = if n.abs() > 255 {
            n
        } else if n.abs() < scalezone1_x {
            (n * scaleopp1) >> 8
        } else if n < 0 {
            ((n * scaleopp2) >> 8) - zone1offset_x
        } else {
            ((n * scaleopp2) >> 8) + zone1offset_x
        };
        v.clamp(-self.range_x, self.range_x - 1)
    }

    fn scaleforopp_y(&self, n: i32, dir: usize) -> i32 {
        let brfd = self.brfd.clamp(0, 3) as usize;
        let s = &VC1_B_FIELD_MVPRED_SCALES;
        let (scalezone1_y, zone1offset_y, scaleopp1, scaleopp2) = (s[4][brfd], s[6][brfd], s[1][brfd], s[2][brfd]);
        let v = if n.abs() > 63 {
            n
        } else if n.abs() < scalezone1_y {
            (n * scaleopp1) >> 8
        } else if n < 0 {
            ((n * scaleopp2) >> 8) - zone1offset_y
        } else {
            ((n * scaleopp2) >> 8) + zone1offset_y
        };
        self.clip_y(v, dir)
    }

    fn scaleforsame(&self, n: i32, dim: bool, dir: usize) -> i32 {
        let hpel = 1 - self.quarter_sample as i32;
        let n = n >> hpel;
        if self.pict_type != PICT_B || self.second_field || dir == 0 {
            return if dim { self.scaleforsame_y(n, dir) } else { self.scaleforsame_x(n, dir) } * (1 << hpel);
        }
        let brfd = self.brfd.clamp(0, 3) as usize;
        let scalesame = VC1_B_FIELD_MVPRED_SCALES[0][brfd];
        ((n * scalesame) >> 8) * (1 << hpel)
    }

    fn scaleforopp(&self, n: i32, dim: bool, dir: usize) -> i32 {
        let hpel = 1 - self.quarter_sample as i32;
        let n = n >> hpel;
        if self.pict_type == PICT_B && !self.second_field && dir == 1 {
            return if dim { self.scaleforopp_y(n, dir) } else { self.scaleforopp_x(n) } * (1 << hpel);
        }
        let rd = self.refdist_for(dir);
        let scaleopp = VC1_FIELD_MVPRED_SCALES[dir ^ self.second_field as usize][0][rd];
        ((n * scaleopp) >> 8) * (1 << hpel)
    }

    /// `ff_vc1_pred_mv`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pred_mv(
        &mut self,
        gb: &mut BitReader,
        n: usize,
        mut dmv_x: i32,
        mut dmv_y: i32,
        mv1: bool,
        r_x: i32,
        mut r_y: i32,
        pred_flag: i32,
        dir: usize,
    ) {
        let mixedmv_pic = self.mv_mode == MV_PMODE_MIXED_MV
            || (self.mv_mode == MV_PMODE_INTENSITY_COMP && self.mv_mode2 == MV_PMODE_MIXED_MV);
        if !self.quarter_sample {
            dmv_x *= 2;
            dmv_y *= 2;
        }
        let wrap = self.b8_stride as isize;
        let xy = self.block_index[n];
        let bo = self.blocks_off;

        if self.mb_intra {
            self.mv[0][n] = [0, 0];
            self.set_mv_at(0, xy + bo, [0, 0]);
            self.set_mv_at(1, xy + bo, [0, 0]);
            if mv1 {
                for d in [1, wrap, wrap + 1] {
                    self.set_mv_at(0, xy + d + bo, [0, 0]);
                }
                let r = self.row3(self.mb_x as isize);
                self.luma_mv_base[r] = [0, 0];
                for d in [1, wrap, wrap + 1] {
                    self.set_mv_at(1, xy + d + bo, [0, 0]);
                }
            }
            return;
        }

        let mut a_valid = !self.first_slice_line || n == 2 || n == 3;
        let mut b_valid = a_valid;
        let mut c_valid = self.mb_x != 0 || n == 1 || n == 3;
        let off: isize;
        let mw = self.mb_width;
        if mv1 {
            off = if self.field_mode && mixedmv_pic {
                if self.mb_x == mw - 1 {
                    -2
                } else {
                    2
                }
            } else if self.mb_x == mw - 1 {
                -1
            } else {
                2
            };
            b_valid = b_valid && mw > 1;
        } else {
            off = match n {
                0 => {
                    if self.res_rtm_flag {
                        if self.mb_x != 0 {
                            -1
                        } else {
                            1
                        }
                    } else if self.mb_x != 0 {
                        -1
                    } else {
                        2 * mw as isize - wrap - 1
                    }
                }
                1 => {
                    if self.mb_x == mw - 1 {
                        -1
                    } else {
                        1
                    }
                }
                2 => 1,
                _ => -1,
            };
            if self.field_mode && mw == 1 {
                b_valid = b_valid && c_valid;
            }
        }

        if self.field_mode {
            a_valid = a_valid && self.vmbt(xy - wrap) == 0;
            b_valid = b_valid && self.vmbt(xy - wrap + off) == 0;
            c_valid = c_valid && self.vmbt(xy - 1) == 0;
        }

        let mut num_oppfield = 0;
        let mut num_samefield = 0;
        let mut fa = [0i16; 2];
        let mut fb = [0i16; 2];
        let mut fc = [0i16; 2];
        let (mut a_f, mut b_f, mut c_f) = (0, 0, 0);
        if a_valid {
            fa = self.mv_at(dir, xy - wrap + bo);
            a_f = self.mv_f[self.mvf_idx(dir, xy - wrap + bo)] as i32;
            num_oppfield += a_f;
            num_samefield += 1 - a_f;
        }
        if b_valid {
            fb = self.mv_at(dir, xy - wrap + off + bo);
            b_f = self.mv_f[self.mvf_idx(dir, xy - wrap + off + bo)] as i32;
            num_oppfield += b_f;
            num_samefield += 1 - b_f;
        }
        if c_valid {
            fc = self.mv_at(dir, xy - 1 + bo);
            c_f = self.mv_f[self.mvf_idx(dir, xy - 1 + bo)] as i32;
            num_oppfield += c_f;
            num_samefield += 1 - c_f;
        }

        let opposite = if self.field_mode {
            if self.numref == 0 {
                1 - self.reffield
            } else if num_samefield <= num_oppfield {
                1 - pred_flag
            } else {
                pred_flag
            }
        } else {
            0
        };
        let cur_idx = self.mvf_idx(dir, xy + bo);
        if opposite != 0 {
            self.mv_f[cur_idx] = 1;
            self.ref_field_type[dir] = (self.cur_field_type == 0) as i32;
            if a_valid && a_f == 0 {
                fa = [self.scaleforopp(fa[0] as i32, false, dir) as i16, self.scaleforopp(fa[1] as i32, true, dir) as i16];
            }
            if b_valid && b_f == 0 {
                fb = [self.scaleforopp(fb[0] as i32, false, dir) as i16, self.scaleforopp(fb[1] as i32, true, dir) as i16];
            }
            if c_valid && c_f == 0 {
                fc = [self.scaleforopp(fc[0] as i32, false, dir) as i16, self.scaleforopp(fc[1] as i32, true, dir) as i16];
            }
        } else {
            self.mv_f[cur_idx] = 0;
            self.ref_field_type[dir] = self.cur_field_type;
            if a_valid && a_f != 0 {
                fa = [self.scaleforsame(fa[0] as i32, false, dir) as i16, self.scaleforsame(fa[1] as i32, true, dir) as i16];
            }
            if b_valid && b_f != 0 {
                fb = [self.scaleforsame(fb[0] as i32, false, dir) as i16, self.scaleforsame(fb[1] as i32, true, dir) as i16];
            }
            if c_valid && c_f != 0 {
                fc = [self.scaleforsame(fc[0] as i32, false, dir) as i16, self.scaleforsame(fc[1] as i32, true, dir) as i16];
            }
        }

        let (mut px, mut py) = if a_valid {
            (fa[0] as i32, fa[1] as i32)
        } else if c_valid {
            (fc[0] as i32, fc[1] as i32)
        } else if b_valid {
            (fb[0] as i32, fb[1] as i32)
        } else {
            (0, 0)
        };
        if num_samefield + num_oppfield > 1 {
            px = mid_pred(fa[0] as i32, fb[0] as i32, fc[0] as i32);
            py = mid_pred(fa[1] as i32, fb[1] as i32, fc[1] as i32);
        }

        if !self.field_mode {
            let mv = if mv1 { -60 } else { -28 };
            let qx = ((self.mb_x as i32) << 6) + if n == 1 || n == 3 { 32 } else { 0 };
            let qy = ((self.mb_y as i32) << 6) + if n == 2 || n == 3 { 32 } else { 0 };
            let x = ((self.mb_width as i32) << 6) - 4;
            let y = ((self.mb_height as i32) << 6) - 4;
            if qx + px < mv {
                px = mv - qx;
            }
            if qy + py < mv {
                py = mv - qy;
            }
            if qx + px > x {
                px = x - qx;
            }
            if qy + py > y {
                py = y - qy;
            }
        }

        if (!self.field_mode || self.pict_type != PICT_B) && a_valid && c_valid {
            let sum = if self.vmbt(xy - wrap) != 0 {
                px.abs() + py.abs()
            } else {
                (px - fa[0] as i32).abs() + (py - fa[1] as i32).abs()
            };
            if sum > 32 {
                if gb.read_bit() != 0 {
                    px = fa[0] as i32;
                    py = fa[1] as i32;
                } else {
                    px = fc[0] as i32;
                    py = fc[1] as i32;
                }
            } else {
                let sum = if self.vmbt(xy - 1) != 0 {
                    px.abs() + py.abs()
                } else {
                    (px - fc[0] as i32).abs() + (py - fc[1] as i32).abs()
                };
                if sum > 32 {
                    if gb.read_bit() != 0 {
                        px = fa[0] as i32;
                        py = fa[1] as i32;
                    } else {
                        px = fc[0] as i32;
                        py = fc[1] as i32;
                    }
                }
            }
        }

        if self.field_mode && self.numref != 0 {
            r_y >>= 1;
        }
        let y_bias = (self.field_mode && self.cur_field_type != 0 && self.ref_field_type[dir] == 0) as i32;
        let mx = ((px + dmv_x + r_x) & ((r_x << 1) - 1)) - r_x;
        let my = ((py + dmv_y + r_y - y_bias) & ((r_y << 1) - 1)) - r_y + y_bias;
        let v = [mx as i16, my as i16];
        self.mv[dir][n] = [v[0] as i32, v[1] as i32];
        self.set_mv_at(dir, xy + bo, v);
        if mv1 {
            for d in [1, wrap, wrap + 1] {
                self.set_mv_at(dir, xy + d + bo, v);
            }
            let f = self.mv_f[cur_idx];
            let i1 = self.mvf_idx(dir, xy + 1 + bo);
            let i2 = self.mvf_idx(dir, xy + wrap + bo);
            let i3 = self.mvf_idx(dir, xy + wrap + 1 + bo);
            self.mv_f[i1] = f;
            self.mv_f[i2] = f;
            self.mv_f[i3] = f;
        }
    }

    #[inline]
    fn is_intra_row(&self, i: isize) -> u8 {
        self.is_intra_base[self.row3(i)]
    }

    /// `ff_vc1_pred_mv_intfr`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn pred_mv_intfr(&mut self, n: usize, dmv_x: i32, dmv_y: i32, mvn: i32, r_x: i32, r_y: i32, dir: usize) {
        let wrap = self.b8_stride as isize;
        let xy = self.block_index[n];
        let mb_x = self.mb_x as isize;
        let ms = self.mb_stride as isize;

        if self.mb_intra {
            self.mv[0][n] = [0, 0];
            self.set_mv_at(0, xy, [0, 0]);
            self.set_mv_at(1, xy, [0, 0]);
            if mvn == 1 {
                for d in [1, wrap, wrap + 1] {
                    self.set_mv_at(0, xy + d, [0, 0]);
                }
                let r = self.row3(mb_x);
                self.luma_mv_base[r] = [0, 0];
                for d in [1, wrap, wrap + 1] {
                    self.set_mv_at(1, xy + d, [0, 0]);
                }
            }
            return;
        }

        let mvi = |s: &Self, bi: isize| -> [i32; 2] {
            let v = s.mv_at(dir, bi);
            [v[0] as i32, v[1] as i32]
        };
        let off: isize = if n == 0 || n == 1 { 1 } else { -1 };
        let mut a = [0i32; 2];
        let mut b = [0i32; 2];
        let mut c = [0i32; 2];
        let (mut a_valid, mut b_valid, mut c_valid) = (false, false, false);
        let cur_field = self.blk_mv_type_at(xy) != 0;
        if self.mb_x != 0 || n == 1 || n == 3 {
            if cur_field || self.blk_mv_type_at(xy - 1) == 0 {
                a = mvi(self, xy - 1);
            } else {
                let p = mvi(self, xy - 1);
                let q = mvi(self, xy - 1 + off * wrap);
                a = [(p[0] + q[0] + 1) >> 1, (p[1] + q[1] + 1) >> 1];
            }
            a_valid = true;
            if n & 1 == 0 && self.is_intra_row(mb_x - 1) != 0 {
                a_valid = false;
                a = [0, 0];
            }
        }
        if n == 0 || n == 1 || cur_field {
            if !self.first_slice_line {
                if self.is_intra_row(mb_x - ms) == 0 {
                    b_valid = true;
                    let mut n_adj = n | 2;
                    let pos_b = self.block_index[n_adj] - 2 * wrap;
                    let pos_b_field = self.blk_mv_type_at(pos_b) != 0;
                    if pos_b_field && cur_field {
                        n_adj = (n & 2) | (n & 1);
                    }
                    b = mvi(self, self.block_index[n_adj] - 2 * wrap);
                    if pos_b_field && !cur_field {
                        let q = mvi(self, self.block_index[n_adj ^ 2] - 2 * wrap);
                        b = [(b[0] + q[0] + 1) >> 1, (b[1] + q[1] + 1) >> 1];
                    }
                }
                if self.mb_width > 1 {
                    if self.is_intra_row(mb_x - ms + 1) == 0 {
                        c_valid = true;
                        let mut n_adj = 2;
                        let pos_c = self.block_index[2] - 2 * wrap + 2;
                        let pos_c_field = self.blk_mv_type_at(pos_c) != 0;
                        if pos_c_field && cur_field {
                            n_adj = n & 2;
                        }
                        c = mvi(self, self.block_index[n_adj] - 2 * wrap + 2);
                        if pos_c_field && !cur_field {
                            let q = mvi(self, self.block_index[n_adj ^ 2] - 2 * wrap + 2);
                            c = [(1 + c[0] + q[0]) >> 1, (1 + c[1] + q[1]) >> 1];
                        }
                        if self.mb_x == self.mb_width - 1 {
                            if self.is_intra_row(mb_x - ms - 1) == 0 {
                                c_valid = true;
                                let mut n_adj = 3;
                                let pos_c = self.block_index[3] - 2 * wrap - 2;
                                let pos_c_field = self.blk_mv_type_at(pos_c) != 0;
                                if pos_c_field && cur_field {
                                    n_adj = n | 1;
                                }
                                c = mvi(self, self.block_index[n_adj] - 2 * wrap - 2);
                                if pos_c_field && !cur_field {
                                    let q = mvi(self, self.block_index[1] - 2 * wrap - 2);
                                    c = [(1 + c[0] + q[0]) >> 1, (1 + c[1] + q[1]) >> 1];
                                }
                            } else {
                                c_valid = false;
                            }
                        }
                    }
                }
            }
        } else {
            b_valid = true;
            b = mvi(self, self.block_index[1]);
            c_valid = true;
            c = mvi(self, self.block_index[0]);
        }

        let total_valid = a_valid as i32 + b_valid as i32 + c_valid as i32;
        if self.mb_x == 0 && !(n == 1 || n == 3) {
            a = [0, 0];
        }
        if (self.first_slice_line && cur_field) || (self.first_slice_line && n & 2 == 0) {
            b = [0, 0];
            c = [0, 0];
        }
        let (mut px, mut py) = (0, 0);
        if !cur_field {
            if self.mb_width == 1 {
                px = b[0];
                py = b[1];
            } else if total_valid >= 2 {
                px = mid_pred(a[0], b[0], c[0]);
                py = mid_pred(a[1], b[1], c[1]);
            } else if total_valid != 0 {
                if a_valid {
                    px = a[0];
                    py = a[1];
                } else if b_valid {
                    px = b[0];
                    py = b[1];
                } else {
                    px = c[0];
                    py = c[1];
                }
            }
        } else {
            let field_a = (a_valid && a[1] & 4 != 0) as i32;
            let field_b = (b_valid && b[1] & 4 != 0) as i32;
            let field_c = (c_valid && c[1] & 4 != 0) as i32;
            let num_oppfield = field_a + field_b + field_c;
            let num_samefield = total_valid - num_oppfield;
            if total_valid == 3 {
                if num_samefield == 3 || num_oppfield == 3 {
                    px = mid_pred(a[0], b[0], c[0]);
                    py = mid_pred(a[1], b[1], c[1]);
                } else if num_samefield >= num_oppfield {
                    px = if field_a == 0 { a[0] } else { b[0] };
                    py = if field_a == 0 { a[1] } else { b[1] };
                } else {
                    px = if field_a != 0 { a[0] } else { b[0] };
                    py = if field_a != 0 { a[1] } else { b[1] };
                }
            } else if total_valid == 2 {
                if num_samefield >= num_oppfield {
                    if field_a == 0 && a_valid {
                        px = a[0];
                        py = a[1];
                    } else if field_b == 0 && b_valid {
                        px = b[0];
                        py = b[1];
                    } else {
                        px = c[0];
                        py = c[1];
                    }
                } else if field_a != 0 && a_valid {
                    px = a[0];
                    py = a[1];
                } else {
                    px = b[0];
                    py = b[1];
                }
            } else if total_valid == 1 {
                px = if a_valid {
                    a[0]
                } else if b_valid {
                    b[0]
                } else {
                    c[0]
                };
                py = if a_valid {
                    a[1]
                } else if b_valid {
                    b[1]
                } else {
                    c[1]
                };
            }
        }

        let mx = ((px + dmv_x + r_x) & ((r_x << 1) - 1)) - r_x;
        let my = ((py + dmv_y + r_y) & ((r_y << 1) - 1)) - r_y;
        let v = [mx as i16, my as i16];
        self.mv[dir][n] = [v[0] as i32, v[1] as i32];
        self.set_mv_at(dir, xy, v);
        if mvn == 1 {
            for d in [1, wrap, wrap + 1] {
                self.set_mv_at(dir, xy + d, v);
            }
        } else if mvn == 2 {
            self.set_mv_at(dir, xy + 1, v);
            self.mv[dir][n + 1] = self.mv[dir][n];
        }
    }

    /// `ff_vc1_pred_b_mv`.
    pub(crate) fn pred_b_mv(&mut self, dmv_x: &mut [i32; 2], dmv_y: &mut [i32; 2], direct: bool, mvtype: i32) {
        let r_x = self.range_x;
        let r_y = self.range_y;
        if !self.quarter_sample {
            for k in 0..2 {
                dmv_x[k] *= 2;
                dmv_y[k] *= 2;
            }
        }
        let wrap = self.b8_stride as isize;
        let xy = self.block_index[0];
        if self.mb_intra {
            self.set_mv_at(0, xy, [0, 0]);
            self.set_mv_at(1, xy, [0, 0]);
            return;
        }
        let nmv = {
            let o = (self.mvo() + xy) as usize;
            self.next.as_ref().map(|n| n.motion_val[1][o]).unwrap_or([0, 0])
        };
        let qs = self.quarter_sample;
        let bf = self.bfraction;
        self.mv[0][0] = [scale_mv(nmv[0] as i32, bf, 0, qs), scale_mv(nmv[1] as i32, bf, 0, qs)];
        self.mv[1][0] = [scale_mv(nmv[0] as i32, bf, 1, qs), scale_mv(nmv[1] as i32, bf, 1, qs)];

        let mbx = self.mb_x as i32;
        let mby = self.mb_y as i32;
        let mw = self.mb_width as i32;
        let mh = self.mb_height as i32;
        for d in 0..2 {
            self.mv[d][0][0] = self.mv[d][0][0].clamp(-60 - (mbx << 6), (mw << 6) - 4 - (mbx << 6));
            self.mv[d][0][1] = self.mv[d][0][1].clamp(-60 - (mby << 6), (mh << 6) - 4 - (mby << 6));
        }
        if direct {
            let a = self.mv[0][0];
            let b = self.mv[1][0];
            self.set_mv_at(0, xy, [a[0] as i16, a[1] as i16]);
            self.set_mv_at(1, xy, [b[0] as i16, b[1] as i16]);
            return;
        }

        for (d, wanted) in [(0usize, BMV_TYPE_FORWARD), (1usize, BMV_TYPE_BACKWARD)] {
            if mvtype != wanted && mvtype != BMV_TYPE_INTERPOLATED {
                continue;
            }
            let off: isize = if self.mb_x == self.mb_width - 1 { -2 } else { 2 };
            if self.mb_x == 0 {
                self.set_mv_at(d, xy - 2, [0, 0]);
            }
            let a = self.mv_at(d, xy - wrap * 2);
            let b = self.mv_at(d, xy - wrap * 2 + off);
            let c = self.mv_at(d, xy - 2);
            let (mut px, mut py);
            if !self.first_slice_line {
                if self.mb_width == 1 {
                    px = a[0] as i32;
                    py = a[1] as i32;
                } else {
                    px = mid_pred(a[0] as i32, b[0] as i32, c[0] as i32);
                    py = mid_pred(a[1] as i32, b[1] as i32, c[1] as i32);
                }
            } else if self.mb_x != 0 {
                px = c[0] as i32;
                py = c[1] as i32;
            } else {
                px = 0;
                py = 0;
            }
            let sh = if self.profile < PROFILE_ADVANCED { 5 } else { 6 };
            let mv = 4 - (1 << sh);
            let qx = mbx << sh;
            let qy = mby << sh;
            let x = (mw << sh) - 4;
            let y = (mh << sh) - 4;
            if qx + px < mv {
                px = mv - qx;
            }
            if qy + py < mv {
                py = mv - qy;
            }
            if qx + px > x {
                px = x - qx;
            }
            if qy + py > y {
                py = y - qy;
            }
            self.mv[d][0][0] = ((px + dmv_x[d] + r_x) & ((r_x << 1) - 1)) - r_x;
            self.mv[d][0][1] = ((py + dmv_y[d] + r_y) & ((r_y << 1) - 1)) - r_y;
        }
        let a = self.mv[0][0];
        let b = self.mv[1][0];
        self.set_mv_at(0, xy, [a[0] as i16, a[1] as i16]);
        self.set_mv_at(1, xy, [b[0] as i16, b[1] as i16]);
    }

    /// `ff_vc1_pred_b_mv_intfi`.
    pub(crate) fn pred_b_mv_intfi(
        &mut self,
        gb: &mut BitReader,
        n: usize,
        dmv_x: &[i32; 2],
        dmv_y: &[i32; 2],
        mv1: bool,
        pred_flag: &[i32; 2],
    ) {
        let dir = (self.bmvtype == BMV_TYPE_BACKWARD) as usize;
        let mb_pos = (self.mb_x + self.mb_y * self.mb_stride) as isize;
        let (rx, ry) = (self.range_x, self.range_y);
        if self.bmvtype == BMV_TYPE_DIRECT {
            let bo = self.blocks_off;
            let f;
            let next_intra = self
                .next
                .as_ref()
                .and_then(|p| p.mb_type.get((mb_pos + self.mb_off) as usize).copied())
                .unwrap_or(0)
                == MB_TYPE_INTRA;
            if !next_intra {
                let o = (self.mvo() + self.block_index[0] + bo) as usize;
                let nmv = self.next.as_ref().map(|p| p.motion_val[1][o]).unwrap_or([0, 0]);
                let qs = self.quarter_sample;
                let bf = self.bfraction;
                self.mv[0][0] = [scale_mv(nmv[0] as i32, bf, 0, qs), scale_mv(nmv[1] as i32, bf, 0, qs)];
                self.mv[1][0] = [scale_mv(nmv[0] as i32, bf, 1, qs), scale_mv(nmv[1] as i32, bf, 1, qs)];
                let mut total_opp = 0;
                for k in 0..4 {
                    total_opp += self.mv_f_next[self.mvf_idx(0, self.block_index[k] + bo)] as i32;
                }
                f = (total_opp > 2) as i32;
            } else {
                self.mv[0][0] = [0, 0];
                self.mv[1][0] = [0, 0];
                f = 0;
            }
            self.ref_field_type[0] = self.cur_field_type ^ f;
            self.ref_field_type[1] = self.cur_field_type ^ f;
            let a = self.mv[0][0];
            let b = self.mv[1][0];
            for k in 0..4 {
                let bi = self.block_index[k] + bo;
                self.set_mv_at(0, bi, [a[0] as i16, a[1] as i16]);
                self.set_mv_at(1, bi, [b[0] as i16, b[1] as i16]);
                let i0 = self.mvf_idx(0, bi);
                let i1 = self.mvf_idx(1, bi);
                self.mv_f[i0] = f as u8;
                self.mv_f[i1] = f as u8;
            }
            return;
        }
        if self.bmvtype == BMV_TYPE_INTERPOLATED {
            self.pred_mv(gb, 0, dmv_x[0], dmv_y[0], true, rx, ry, pred_flag[0], 0);
            self.pred_mv(gb, 0, dmv_x[1], dmv_y[1], true, rx, ry, pred_flag[1], 1);
            return;
        }
        if dir != 0 {
            self.pred_mv(gb, n, dmv_x[1], dmv_y[1], mv1, rx, ry, pred_flag[1], 1);
            if n == 3 || mv1 {
                self.pred_mv(gb, 0, dmv_x[0], dmv_y[0], true, rx, ry, 0, 0);
            }
        } else {
            self.pred_mv(gb, n, dmv_x[0], dmv_y[0], mv1, rx, ry, pred_flag[0], 0);
            if n == 3 || mv1 {
                self.pred_mv(gb, 0, dmv_x[1], dmv_y[1], true, rx, ry, 0, 1);
            }
        }
    }
}

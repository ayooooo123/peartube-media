//! VC-1 / WMV3 macroblock and block layer.
//!
//! Ported from FFmpeg commit 2da55bf `libavcodec/vc1_block.c`
//! (LGPL-2.1-or-later): DC/AC prediction, coefficient decoding, the
//! I/P/B macroblock decoders for progressive, interlaced-frame and
//! interlaced-field pictures, the delayed put-pixels loop and the per-slice
//! block loops.

use super::*;
use crate::bits::BitReader;
use crate::idct;

const OFFSET_TABLE: [[i32; 9]; 2] = [[0, 1, 2, 4, 8, 16, 32, 64, 128], [0, 1, 3, 7, 15, 31, 63, 127, 255]];
pub(crate) const BLOCK_MAP: [usize; 6] = [0, 2, 1, 3, 4, 5];
const SIZE_TABLE: [i32; 6] = [0, 2, 3, 4, 5, 8];
const DCPRED: [i32; 32] = [
    65535, 1024, 512, 341, 256, 205, 171, 146, 128, 114, 102, 93, 85, 79, 73, 68, 64, 60, 57, 54, 51, 49, 47, 45, 43,
    41, 39, 38, 37, 35, 34, 33,
];

/// Where a decoded block lives.
#[derive(Clone, Copy)]
enum BlkDst {
    /// `v->block[cur_blk_idx][block_map[i]]`
    Ring(usize),
    /// `v->blocks[i]`
    Scratch(usize),
}

impl Vc1Decoder {
    // ───────────────────────── small helpers ─────────────────────────

    #[inline]
    pub(crate) fn blk_off(&self, idx: isize, j: usize) -> usize {
        let n = self.n_allocated_blks as isize;
        ((idx.rem_euclid(n)) as usize * 6 + j) * 64
    }

    fn load_blk(&self, d: BlkDst) -> [i16; 64] {
        match d {
            BlkDst::Ring(j) => {
                let o = self.blk_off(self.cur_blk_idx, j);
                let mut b = [0i16; 64];
                b.copy_from_slice(&self.blk[o..o + 64]);
                b
            }
            BlkDst::Scratch(i) => self.blocks[i],
        }
    }

    fn store_blk(&mut self, d: BlkDst, b: &[i16; 64]) {
        match d {
            BlkDst::Ring(j) => {
                let o = self.blk_off(self.cur_blk_idx, j);
                self.blk[o..o + 64].copy_from_slice(b);
            }
            BlkDst::Scratch(i) => self.blocks[i] = *b,
        }
    }

    fn clear_cur_blocks(&mut self) {
        let o = self.blk_off(self.cur_blk_idx, 0);
        self.blk[o..o + 6 * 64].fill(0);
    }

    #[inline]
    pub(crate) fn mb_pos(&self) -> usize {
        self.mb_x + self.mb_y * self.mb_stride
    }

    /// Index into `qscale_table` (origin `2 * mb_stride + 1`).
    #[inline]
    pub(crate) fn qidx(&self, mb_pos: isize) -> usize {
        (mb_pos + 2 * self.mb_stride as isize + 1) as usize
    }

    #[inline]
    pub(crate) fn block_wrap(&self, n: usize) -> isize {
        if n < 4 {
            self.b8_stride as isize
        } else {
            self.mb_stride as isize
        }
    }

    /// `v->mb_type[block_index[i] + d]`.
    #[inline]
    pub(crate) fn vmbt(&self, bi: isize) -> u8 {
        self.vmb_type[(self.bo() + bi) as usize]
    }
    #[inline]
    pub(crate) fn set_vmbt(&mut self, bi: isize, v: u8) {
        let o = (self.bo() + bi) as usize;
        self.vmb_type[o] = v;
    }

    /// `s->dc_val[block_index[n]] = v`.
    #[inline]
    fn set_dc_val(&mut self, n: usize, v: i16) {
        let o = (self.bo() + self.block_index[n]) as usize;
        self.dc_val[o] = v;
    }

    #[inline]
    pub(crate) fn blk_mv_type_at(&self, bi: isize) -> u8 {
        self.blk_mv_type[(self.bo() + bi) as usize]
    }
    #[inline]
    pub(crate) fn set_blk_mv_type4(&mut self, v: u8) {
        for i in 0..4 {
            let o = (self.bo() + self.block_index[i]) as usize;
            self.blk_mv_type[o] = v;
        }
    }

    /// `cur_pic.motion_val[dir][bi]` (block index, origin `mvo`).
    #[inline]
    pub(crate) fn mv_at(&self, dir: usize, bi: isize) -> [i16; 2] {
        let o = (self.mvo() + bi) as usize;
        self.cur.as_ref().map(|c| c.motion_val[dir][o]).unwrap_or([0, 0])
    }
    #[inline]
    pub(crate) fn set_mv_at(&mut self, dir: usize, bi: isize, v: [i16; 2]) {
        let o = (self.mvo() + bi) as usize;
        if let Some(c) = self.cur.as_mut() {
            c.motion_val[dir][o] = v;
        }
    }

    #[inline]
    pub(crate) fn set_cur_mb_type(&mut self, mb_pos: isize, v: u8) {
        if let Some(c) = self.cur.as_mut() {
            if let Some(e) = c.mb_type.get_mut(mb_pos as usize) {
                *e = v;
            }
        }
    }

    #[inline]
    fn set_qscale(&mut self, mb_pos: usize, q: i32) {
        let o = self.qidx(mb_pos as isize);
        self.qscale_table[o] = q as i8;
    }

    // ───────────────────────── block index / dest ─────────────────────────

    /// `init_block_index` (VC-1 variant with the field offset).
    pub(crate) fn init_block_index(&mut self) {
        let b8 = self.b8_stride as isize;
        let mbs = self.mb_stride as isize;
        let (x, y) = (self.mb_x as isize, self.mb_y as isize);
        let mh = self.mb_height as isize;
        self.block_index[0] = b8 * (y * 2) - 2 + x * 2;
        self.block_index[1] = b8 * (y * 2) - 1 + x * 2;
        self.block_index[2] = b8 * (y * 2 + 1) - 2 + x * 2;
        self.block_index[3] = b8 * (y * 2 + 1) - 1 + x * 2;
        self.block_index[4] = mbs * (y + 1) + b8 * mh * 2 + x - 1;
        self.block_index[5] = mbs * (y + mh + 2) + b8 * mh * 2 + x - 1;
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        self.dest[0] = (x - 1) * 16 + y * ls * 16;
        self.dest[1] = (x - 1) * 8 + y * uvls * 8;
        self.dest[2] = (x - 1) * 8 + y * uvls * 8;
        if self.field_mode && !(self.second_field ^ self.tff) {
            self.dest[0] += (self.mb_width * 16) as isize;
            self.dest[1] += (self.mb_width * 8) as isize;
            self.dest[2] += (self.mb_width * 8) as isize;
        }
    }

    /// `ff_update_block_index` (8-bit, 4:2:0).
    pub(crate) fn update_block_index(&mut self) {
        for i in 0..4 {
            self.block_index[i] += 2;
        }
        self.block_index[4] += 1;
        self.block_index[5] += 1;
        self.dest[0] += 16;
        self.dest[1] += 8;
        self.dest[2] += 8;
    }

    // ───────────────────────── put pixels ─────────────────────────

    fn put_block(&mut self, idx: isize, j: usize, plane: usize, off: isize, stride: usize, signed: bool) {
        let bo = self.blk_off(idx, j);
        let mut b = [0i16; 64];
        b.copy_from_slice(&self.blk[bo..bo + 64]);
        if let Some(cur) = self.cur.as_mut() {
            let data = &mut cur.pic.data[plane];
            if off < 0 || off as usize + 7 * stride + 8 > data.len() {
                return;
            }
            if signed {
                dsp::put_signed_pixels_clamped(&b, data, off as usize, stride);
            } else {
                dsp::put_pixels_clamped(&b, data, off as usize, stride);
            }
        }
    }

    /// `vc1_put_blocks_clamped`.
    pub(crate) fn put_blocks_clamped(&mut self, put_signed: bool) {
        let ls = self.linesize as isize;
        let uvls = self.uvlinesize as isize;
        let mut fieldtx = 0usize;
        if !self.first_slice_line && self.fcm != ILACE_FRAME {
            if self.mb_x != 0 {
                for i in 0..6 {
                    let cond = if i > 3 {
                        self.vmbt(self.block_index[i] - self.block_wrap(i) - 1)
                    } else {
                        self.vmbt(self.block_index[i] - 2 * self.block_wrap(i) - 2)
                    };
                    if cond != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, self.dest[i - 3] - 8 * uvls - 8, self.uvlinesize)
                        } else {
                            (
                                0,
                                self.dest[0] + ((i as isize & 2) - 4) * 4 * ls + ((i as isize & 1) - 2) * 8,
                                self.linesize,
                            )
                        };
                        self.put_block(self.topleft_blk_idx, BLOCK_MAP[i], plane, off, stride, put_signed);
                    }
                }
            }
            if self.mb_x == self.end_mb_x - 1 {
                for i in 0..6 {
                    let cond = if i > 3 {
                        self.vmbt(self.block_index[i] - self.block_wrap(i))
                    } else {
                        self.vmbt(self.block_index[i] - 2 * self.block_wrap(i))
                    };
                    if cond != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, self.dest[i - 3] - 8 * uvls, self.uvlinesize)
                        } else {
                            (0, self.dest[0] + ((i as isize & 2) - 4) * 4 * ls + (i as isize & 1) * 8, self.linesize)
                        };
                        self.put_block(self.top_blk_idx, BLOCK_MAP[i], plane, off, stride, put_signed);
                    }
                }
            }
        }
        if self.mb_y == self.end_mb_y - 1 || self.fcm == ILACE_FRAME {
            if self.mb_x != 0 {
                if self.fcm == ILACE_FRAME {
                    fieldtx = self.fieldtx_plane[self.mb_y * self.mb_stride + self.mb_x - 1] as usize;
                }
                for i in 0..6 {
                    let cond = if i > 3 {
                        self.vmbt(self.block_index[i] - 1)
                    } else {
                        self.vmbt(self.block_index[i] - 2)
                    };
                    if cond != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, self.dest[i - 3] - 8, self.uvlinesize)
                        } else {
                            let off = if fieldtx != 0 {
                                self.dest[0] + ((i as isize & 2) >> 1) * ls + ((i as isize & 1) - 2) * 8
                            } else {
                                self.dest[0] + (i as isize & 2) * 4 * ls + ((i as isize & 1) - 2) * 8
                            };
                            (0, off, self.linesize << fieldtx)
                        };
                        self.put_block(self.left_blk_idx, BLOCK_MAP[i], plane, off, stride, put_signed);
                    }
                }
            }
            if self.mb_x == self.end_mb_x - 1 {
                if self.fcm == ILACE_FRAME {
                    fieldtx = self.fieldtx_plane[self.mb_y * self.mb_stride + self.mb_x] as usize;
                }
                for i in 0..6 {
                    if self.vmbt(self.block_index[i]) != 0 {
                        let (plane, off, stride) = if i > 3 {
                            (i - 3, self.dest[i - 3], self.uvlinesize)
                        } else {
                            let off = if fieldtx != 0 {
                                self.dest[0] + ((i as isize & 2) >> 1) * ls + (i as isize & 1) * 8
                            } else {
                                self.dest[0] + (i as isize & 2) * 4 * ls + (i as isize & 1) * 8
                            };
                            (0, off, self.linesize << fieldtx)
                        };
                        self.put_block(self.cur_blk_idx, BLOCK_MAP[i], plane, off, stride, put_signed);
                    }
                }
            }
        }
    }

    fn inc_blk_idx(&mut self) {
        let n = self.n_allocated_blks as isize;
        for idx in [
            &mut self.topleft_blk_idx,
            &mut self.top_blk_idx,
            &mut self.left_blk_idx,
            &mut self.cur_blk_idx,
        ] {
            *idx += 1;
            if *idx >= n {
                *idx = 0;
            }
        }
    }

    // ───────────────────────── MQUANT / MVDATA ─────────────────────────

    /// `GET_MQUANT()`.
    fn get_mquant(&mut self, gb: &mut BitReader, mquant: &mut i32) {
        if !self.dquantfrm {
            return;
        }
        let mut edges = 0;
        if self.dqprofile == DQPROFILE_ALL_MBS {
            if self.dqbilevel {
                *mquant = if gb.read_bit() != 0 { -self.altpq } else { self.pq };
            } else {
                let mqdiff = gb.read(3) as i32;
                *mquant = if mqdiff != 7 { -self.pq - mqdiff } else { -(gb.read(5) as i32) };
            }
        }
        if self.dqprofile == DQPROFILE_SINGLE_EDGE {
            edges = 1 << self.dqsbedge;
        } else if self.dqprofile == DQPROFILE_DOUBLE_EDGES {
            edges = (3 << self.dqsbedge) % 15;
        } else if self.dqprofile == DQPROFILE_FOUR_EDGES {
            edges = 15;
        }
        if (edges & 1) != 0 && self.mb_x == 0 {
            *mquant = -self.altpq;
        }
        if (edges & 2) != 0 && self.mb_y == 0 {
            *mquant = -self.altpq;
        }
        if (edges & 4) != 0 && self.mb_x == self.mb_width - 1 {
            *mquant = -self.altpq;
        }
        if (edges & 8) != 0 && self.mb_y == (self.mb_height >> self.field_mode as usize) - 1 {
            *mquant = -self.altpq;
        }
        if *mquant == 0 || *mquant > 31 || *mquant < -31 {
            *mquant = 1;
        }
    }

    /// `GET_MVDATA`: returns (dmv_x, dmv_y, mb_has_coeffs) and sets `mb_intra`.
    fn get_mvdata(&mut self, gb: &mut BitReader) -> (i32, i32, bool) {
        let mut index = 1 + VLCS.mv_diff[self.mv_table_index].get(gb);
        let mb_has_coeffs = if index > 36 {
            index -= 37;
            true
        } else {
            false
        };
        self.mb_intra = false;
        let (mut dx, mut dy) = (0, 0);
        if index == 0 {
        } else if index == 35 {
            dx = gb.read((self.k_x - 1 + self.quarter_sample as i32) as u32) as i32;
            dy = gb.read((self.k_y - 1 + self.quarter_sample as i32) as u32) as i32;
        } else if index == 36 {
            self.mb_intra = true;
        } else {
            let qs = self.quarter_sample;
            let comp = |gb: &mut BitReader, index1: usize| -> i32 {
                let mut v = OFFSET_TABLE[1][index1];
                let val = SIZE_TABLE[index1] - (!qs && index1 == 5) as i32;
                if val > 0 {
                    let val = gb.read(val as u32) as i32;
                    let sign = -(val & 1);
                    v = (sign ^ ((val >> 1) + v)) - sign;
                }
                v
            };
            let index = index.clamp(0, 35) as usize;
            dx = comp(gb, index % 6);
            dy = comp(gb, index / 6);
        }
        (dx, dy, mb_has_coeffs)
    }

    /// `get_mvdata_interlaced`: returns (dmv_x, dmv_y, pred_flag).
    fn get_mvdata_interlaced(&mut self, gb: &mut BitReader, want_pred_flag: bool) -> (i32, i32, i32) {
        let esc = if self.numref != 0 { 125 } else { 71 };
        let extend_x = (self.dmvrange & 1) as usize;
        let extend_y = ((self.dmvrange >> 1) & 1) as usize;
        let index = self.imv_vlc.vlc().map(|v| v.get(gb)).unwrap_or(-1);
        let mut pred_flag = 0;
        let (dmv_x, mut dmv_y);
        if index == esc {
            dmv_x = gb.read(self.k_x as u32) as i32;
            dmv_y = gb.read(self.k_y as u32) as i32;
            if self.numref != 0 {
                if want_pred_flag {
                    pred_flag = dmv_y & 1;
                }
                dmv_y = (dmv_y + (dmv_y & 1)) >> 1;
            }
        } else {
            // av_assert0(index < esc) in FFmpeg; a negative (invalid) code
            // behaves like index -1 here.
            let index1 = ((index + 1) % 9) as i32;
            if index1 != 0 {
                let val = gb.read((index1 as usize + extend_x) as u32) as i32;
                let sign = -(val & 1);
                dmv_x = (sign ^ ((val >> 1) + OFFSET_TABLE[extend_x][index1 as usize])) - sign;
            } else {
                dmv_x = 0;
            }
            let index1 = (index + 1) / 9;
            if index1 > self.numref {
                let i1 = (index1 >> self.numref) as usize;
                let val = gb.read((i1 + extend_y) as u32) as i32;
                let sign = -(val & 1);
                let off = OFFSET_TABLE[extend_y].get(i1).copied().unwrap_or(0);
                dmv_y = (sign ^ ((val >> 1) + off)) - sign;
            } else {
                dmv_y = 0;
            }
            if self.numref != 0 && want_pred_flag {
                pred_flag = index1 & 1;
            }
        }
        (dmv_x, dmv_y, pred_flag)
    }

    /// `vc1_b_mc`.
    fn b_mc(&mut self, direct: bool, mode: i32) {
        if direct || mode == BMV_TYPE_INTERPOLATED {
            self.mc_1mv(0);
            self.interp_mc();
            return;
        }
        self.mc_1mv((mode == BMV_TYPE_BACKWARD) as usize);
    }

    // ───────────────────────── DC prediction ─────────────────────────

    /// `vc1_i_pred_dc`: returns (pred, dir, dc_val index).
    fn i_pred_dc(&self, overlap: bool, pq: i32, n: usize) -> (i32, i32, usize) {
        let scale = self.y_dc_scale;
        let wrap = self.block_wrap(n);
        let xy = self.bo() + self.block_index[n];
        let mut c = self.dc_val[(xy - 1) as usize] as i32;
        let mut b = self.dc_val[(xy - 1 - wrap) as usize] as i32;
        let mut a = self.dc_val[(xy - wrap) as usize] as i32;
        let dp = DCPRED[(scale as usize) & 31];
        if pq < 9 || !overlap {
            if self.first_slice_line && n != 2 && n != 3 {
                b = dp;
                a = dp;
            }
            if self.mb_x == 0 && n != 1 && n != 3 {
                b = dp;
                c = dp;
            }
        } else {
            if self.first_slice_line && n != 2 && n != 3 {
                b = 0;
                a = 0;
            }
            if self.mb_x == 0 && n != 1 && n != 3 {
                b = 0;
                c = 0;
            }
        }
        if (a - b).abs() <= (b - c).abs() {
            (c, 1, xy as usize)
        } else {
            (a, 0, xy as usize)
        }
    }

    /// `ff_vc1_pred_dc`: returns (pred, dir, dc_val index).
    fn pred_dc(&self, n: usize, a_avail: bool, c_avail: bool) -> (i32, i32, usize) {
        let mb_pos = self.mb_pos() as isize;
        let q1 = (self.qscale_table[self.qidx(mb_pos)] as i32).abs();
        let dqscale_index = WMV3_DC_SCALE_TABLE[(q1 as usize) & 31] as i32 - 1;
        let xy = self.bo() + self.block_index[n];
        if dqscale_index < 0 {
            return (0, 0, xy as usize);
        }
        let dqs = VC1_DQSCALE[dqscale_index as usize];
        let wrap = self.block_wrap(n);
        let mut c = self.dc_val[(xy - 1) as usize] as i32;
        let mut b = self.dc_val[(xy - 1 - wrap) as usize] as i32;
        let mut a = self.dc_val[(xy - wrap) as usize] as i32;
        let rescale = |v: i32, q2: i32| -> i32 {
            ((v as u32)
                .wrapping_mul(WMV3_DC_SCALE_TABLE[(q2 as usize) & 31] as u32)
                .wrapping_mul(dqs as u32)
                .wrapping_add(0x20000) as i32)
                >> 18
        };
        if c_avail && n != 1 && n != 3 {
            let q2 = (self.qscale_table[self.qidx(mb_pos - 1)] as i32).abs();
            if q2 != 0 && q2 != q1 {
                c = rescale(c, q2);
            }
        }
        if a_avail && n != 2 && n != 3 {
            let q2 = (self.qscale_table[self.qidx(mb_pos - self.mb_stride as isize)] as i32).abs();
            if q2 != 0 && q2 != q1 {
                a = rescale(a, q2);
            }
        }
        if a_avail && c_avail && n != 3 {
            let mut off = mb_pos;
            if n != 1 {
                off -= 1;
            }
            if n != 2 {
                off -= self.mb_stride as isize;
            }
            let q2 = (self.qscale_table[self.qidx(off)] as i32).abs();
            if q2 != 0 && q2 != q1 {
                b = rescale(b, q2);
            }
        }
        if c_avail && (!a_avail || (a - b).abs() <= (b - c).abs()) {
            (c, 1, xy as usize)
        } else if a_avail {
            (a, 0, xy as usize)
        } else {
            (0, 1, xy as usize)
        }
    }

    /// `vc1_coded_block_pred`.
    fn coded_block_pred(&mut self, n: usize, diff: i32) -> i32 {
        let xy = self.bo() + self.block_index[n];
        let wrap = self.b8_stride as isize;
        let a = self.coded_block[(xy - 1) as usize] as i32;
        let b = self.coded_block[(xy - 1 - wrap) as usize] as i32;
        let c = self.coded_block[(xy - wrap) as usize] as i32;
        let pred = if b == c { a } else { c };
        self.coded_block[xy as usize] = (pred ^ diff) as u8;
        pred ^ diff
    }

    // ───────────────────────── coefficients ─────────────────────────

    /// `vc1_decode_ac_coeff`: returns (last, skip, value) or None on error.
    fn decode_ac_coeff(&mut self, gb: &mut BitReader, codingset: usize) -> Option<(bool, i32, i32)> {
        let vlc = &VLCS.ac_coeff[codingset];
        let size = VC1_AC_SIZES[codingset] as i32;
        let mut index = vlc.get(gb);
        if index < 0 {
            return None;
        }
        let (run, level, lst, sign);
        if index != size - 1 {
            let e = VC1_INDEX_DECODE_TABLE[codingset][index as usize];
            run = e[0] as i32;
            level = e[1] as i32;
            lst = index >= VC1_LAST_DECODE_TABLE[codingset] || gb.bits_left() < 0;
            sign = gb.read_bit() as i32;
        } else {
            let escape = gb.decode210();
            if escape != 2 {
                index = vlc.get(gb);
                if index < 0 || index >= size - 1 {
                    return None;
                }
                let e = VC1_INDEX_DECODE_TABLE[codingset][index as usize];
                let mut r = e[0] as i32;
                let mut l = e[1] as i32;
                lst = index >= VC1_LAST_DECODE_TABLE[codingset];
                if escape == 0 {
                    l += if lst {
                        VC1_LAST_DELTA_LEVEL_TABLE[codingset].get(r as usize).copied().unwrap_or(0)
                    } else {
                        VC1_DELTA_LEVEL_TABLE[codingset].get(r as usize).copied().unwrap_or(0)
                    } as i32;
                } else {
                    r += if lst {
                        VC1_LAST_DELTA_RUN_TABLE[codingset].get(l as usize).copied().unwrap_or(0)
                    } else {
                        VC1_DELTA_RUN_TABLE[codingset].get(l as usize).copied().unwrap_or(0)
                    } as i32
                        + 1;
                }
                run = r;
                level = l;
                sign = gb.read_bit() as i32;
            } else {
                lst = gb.read_bit() != 0;
                if self.esc3_level_length == 0 {
                    if self.pq < 8 || self.dquantfrm {
                        self.esc3_level_length = gb.read(3);
                        if self.esc3_level_length == 0 {
                            self.esc3_level_length = gb.read(2) + 8;
                        }
                    } else {
                        self.esc3_level_length = gb.get_unary(1, 6) + 2;
                    }
                    self.esc3_run_length = 3 + gb.read(2);
                }
                run = gb.read(self.esc3_run_length) as i32;
                sign = gb.read_bit() as i32;
                level = gb.read(self.esc3_level_length) as i32;
            }
        }
        Some((lst, run, (level ^ -sign) + sign))
    }

    /// Applies `block[k] *= scale; block[k] += sign * quant` to coefficient k.
    #[inline]
    fn scale_coef(&self, v: i16, scale: i32, quant: i32) -> i16 {
        let mut b = (v as i32).wrapping_mul(scale) as i16;
        if !self.pquantizer {
            b = (b as i32 + if b < 0 { -quant } else { quant }) as i16;
        }
        b
    }

    /// `vc1_decode_i_block` (Simple/Main I-pictures).
    fn decode_i_block(&mut self, gb: &mut BitReader, block: &mut [i16; 64], n: usize, coded: bool, codingset: usize) -> bool {
        let dcdiff = self.read_dcdiff(gb, n, self.pq);
        let (pred, dc_pred_dir, dc_idx) = self.i_pred_dc(self.overlap, self.pq, n);
        let dcdiff = dcdiff.wrapping_add(pred);
        self.dc_val[dc_idx] = dcdiff as i16;
        block[0] = dcdiff.wrapping_mul(self.y_dc_scale) as i16;

        let ac2 = dc_idx;
        let ac = if dc_pred_dir != 0 { ac2 - 1 } else { (ac2 as isize - self.block_wrap(n)) as usize };
        let scale = self.pq * 2 + self.halfpq;

        if coded {
            let zz = if self.ac_pred {
                if dc_pred_dir == 0 {
                    self.zz_8x8[2]
                } else {
                    self.zz_8x8[3]
                }
            } else {
                self.zz_8x8[1]
            };
            let mut i = 1;
            loop {
                let Some((last, skip, value)) = self.decode_ac_coeff(gb, codingset) else { return false };
                i += skip;
                if i > 63 {
                    break;
                }
                block[zz[i as usize] as usize] = value as i16;
                if last {
                    break;
                }
                i += 1;
            }
            if self.ac_pred {
                let (sh, half) = if dc_pred_dir != 0 { (self.left_blk_sh, 0) } else { (self.top_blk_sh, 8) };
                let src = self.ac_val[ac];
                for k in 1..8 {
                    let e = &mut block[k << sh];
                    *e = e.wrapping_add(src[k + half]);
                }
            }
            for k in 1..8 {
                self.ac_val[ac2][k] = block[k << self.left_blk_sh];
                self.ac_val[ac2][k + 8] = block[k << self.top_blk_sh];
            }
            for k in 1..64 {
                if block[k] != 0 {
                    block[k] = self.scale_coef(block[k], scale, self.pq);
                }
            }
        } else {
            self.ac_val[ac2] = [0; 16];
            if self.ac_pred {
                let (sh, half) = if dc_pred_dir != 0 { (self.left_blk_sh, 0) } else { (self.top_blk_sh, 8) };
                let src = self.ac_val[ac];
                for k in 0..8 {
                    self.ac_val[ac2][half + k] = src[half + k];
                }
                for k in 1..8 {
                    let v = (self.ac_val[ac2][half + k] as i32).wrapping_mul(scale) as i16;
                    block[k << sh] = v;
                    if !self.pquantizer && v != 0 {
                        block[k << sh] = (v as i32 + if v < 0 { -self.pq } else { self.pq }) as i16;
                    }
                }
            }
        }
        true
    }

    /// DC differential (`get_vlc2(ff_msmp4_dc_vlc...)` + escape).
    fn read_dcdiff(&mut self, gb: &mut BitReader, n: usize, quant: i32) -> i32 {
        let mut dcdiff = VLCS.msmp4_dc[self.dc_table_index][(n >= 4) as usize].get(gb);
        if dcdiff != 0 {
            let m = if quant == 1 || quant == 2 { 3 - quant } else { 0 };
            if dcdiff == 119 {
                dcdiff = gb.read((8 + m) as u32) as i32;
            } else if m != 0 {
                dcdiff = (dcdiff << m) + gb.read(m as u32) as i32 - ((1 << m) - 1);
            }
            if gb.read_bit() != 0 {
                dcdiff = -dcdiff;
            }
        }
        dcdiff
    }

    /// AC prediction rescale `(int)(v * q2 * dqscale[q1 - 1] + 0x20000) >> 18`.
    #[inline]
    fn ac_rescale(v: i16, q2: i32, q1: i32) -> i32 {
        let ds = VC1_DQSCALE[((q1 - 1).clamp(0, 62)) as usize] as u32;
        ((v as i32 as u32).wrapping_mul(q2 as u32).wrapping_mul(ds).wrapping_add(0x20000) as i32) >> 18
    }

    /// `vc1_decode_i_block_adv`.
    #[allow(clippy::too_many_arguments)]
    fn decode_i_block_adv(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        codingset: usize,
        mquant: i32,
    ) -> bool {
        let a_avail = self.a_avail;
        let c_avail = self.c_avail;
        let mut use_pred = self.ac_pred;
        let mb_pos = self.mb_pos() as isize;
        let quant = mquant.abs();

        let dcdiff = self.read_dcdiff(gb, n, quant);
        let (pred, dc_pred_dir, dc_idx) = self.pred_dc(n, a_avail, c_avail);
        let dcdiff = dcdiff.wrapping_add(pred);
        self.dc_val[dc_idx] = dcdiff as i16;
        block[0] = dcdiff.wrapping_mul(self.y_dc_scale) as i16;

        if !a_avail && !c_avail {
            use_pred = false;
        }
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        let ac2 = dc_idx;
        let ac = if dc_pred_dir != 0 { ac2 - 1 } else { (ac2 as isize - self.block_wrap(n)) as usize };

        let mut q1 = self.qscale_table[self.qidx(mb_pos)] as i32;
        let mut q2 = 0;
        if n == 3 {
            q2 = q1;
        } else if dc_pred_dir != 0 {
            if n == 1 {
                q2 = q1;
            } else if c_avail && mb_pos != 0 {
                q2 = self.qscale_table[self.qidx(mb_pos - 1)] as i32;
            }
        } else if n == 2 {
            q2 = q1;
        } else if a_avail && mb_pos >= self.mb_stride as isize {
            q2 = self.qscale_table[self.qidx(mb_pos - self.mb_stride as isize)] as i32;
        }

        if coded {
            let zz = if self.ac_pred {
                if !use_pred && self.fcm == ILACE_FRAME {
                    self.zzi_8x8
                } else if dc_pred_dir == 0 {
                    self.zz_8x8[2]
                } else {
                    self.zz_8x8[3]
                }
            } else if self.fcm != ILACE_FRAME {
                self.zz_8x8[1]
            } else {
                self.zzi_8x8
            };
            let mut i = 1;
            loop {
                let Some((last, skip, value)) = self.decode_ac_coeff(gb, codingset) else { return false };
                i += skip;
                if i > 63 {
                    break;
                }
                block[zz[i as usize] as usize] = value as i16;
                if last {
                    break;
                }
                i += 1;
            }
            if use_pred {
                let (sh, half) = if dc_pred_dir != 0 { (self.left_blk_sh, 0) } else { (self.top_blk_sh, 8) };
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return false;
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                let src = self.ac_val[ac];
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let e = &mut block[k << sh];
                        *e = (*e as i32).wrapping_add(Self::ac_rescale(src[k + half], q2, q1)) as i16;
                    }
                } else {
                    for k in 1..8 {
                        let e = &mut block[k << sh];
                        *e = e.wrapping_add(src[k + half]);
                    }
                }
            }
            for k in 1..8 {
                self.ac_val[ac2][k] = block[k << self.left_blk_sh];
                self.ac_val[ac2][k + 8] = block[k << self.top_blk_sh];
            }
            for k in 1..64 {
                if block[k] != 0 {
                    block[k] = self.scale_coef(block[k], scale, quant);
                }
            }
        } else {
            self.ac_val[ac2] = [0; 16];
            if use_pred {
                let (sh, half) = if dc_pred_dir != 0 { (self.left_blk_sh, 0) } else { (self.top_blk_sh, 8) };
                let src = self.ac_val[ac];
                for k in 0..8 {
                    self.ac_val[ac2][half + k] = src[half + k];
                }
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return false;
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let v = self.ac_val[ac2][half + k];
                        // (int)(ac_val2[k] * q2 * (unsigned)dqscale + 0x20000) >> 18
                        self.ac_val[ac2][half + k] = Self::ac_rescale(v, q2, q1) as i16;
                    }
                }
                for k in 1..8 {
                    let v = (self.ac_val[ac2][half + k] as i32).wrapping_mul(scale) as i16;
                    block[k << sh] = v;
                    if !self.pquantizer && v != 0 {
                        block[k << sh] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                }
            }
        }
        true
    }

    /// `vc1_decode_intra_block` (intra blocks in P/B pictures and
    /// interlaced intra pictures).
    #[allow(clippy::too_many_arguments)]
    fn decode_intra_block(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        mquant: i32,
        codingset: usize,
    ) -> bool {
        let mb_pos = self.mb_pos() as isize;
        let a_avail = self.a_avail;
        let c_avail = self.c_avail;
        let mut use_pred = self.ac_pred;
        *block = [0; 64];
        let quant = mquant.abs().clamp(0, 31);
        self.y_dc_scale = WMV3_DC_SCALE_TABLE[quant as usize] as i32;

        let dcdiff = self.read_dcdiff(gb, n, quant);
        let (pred, mut dc_pred_dir, dc_idx) = self.pred_dc(n, a_avail, c_avail);
        let dcdiff = dcdiff.wrapping_add(pred);
        self.dc_val[dc_idx] = dcdiff as i16;
        block[0] = dcdiff.wrapping_mul(self.y_dc_scale) as i16;

        if !a_avail {
            dc_pred_dir = 1;
        }
        if !c_avail {
            dc_pred_dir = 0;
        }
        if !a_avail && !c_avail {
            use_pred = false;
        }
        let ac2 = dc_idx;
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        let ac = if dc_pred_dir != 0 { ac2 - 1 } else { (ac2 as isize - self.block_wrap(n)) as usize };

        let mut q1 = self.qscale_table[self.qidx(mb_pos)] as i32;
        let mut q2 = 0;
        if dc_pred_dir != 0 && c_avail && mb_pos != 0 {
            q2 = self.qscale_table[self.qidx(mb_pos - 1)] as i32;
        }
        if dc_pred_dir == 0 && a_avail && mb_pos >= self.mb_stride as isize {
            q2 = self.qscale_table[self.qidx(mb_pos - self.mb_stride as isize)] as i32;
        }
        if dc_pred_dir != 0 && n == 1 {
            q2 = q1;
        }
        if dc_pred_dir == 0 && n == 2 {
            q2 = q1;
        }
        if n == 3 {
            q2 = q1;
        }

        if coded {
            let mut i = 1;
            loop {
                let Some((last, skip, value)) = self.decode_ac_coeff(gb, codingset) else { return false };
                i += skip;
                if i > 63 {
                    break;
                }
                let iu = i as usize;
                let pos = if self.fcm == PROGRESSIVE {
                    self.zz_8x8[0][iu]
                } else if use_pred && self.fcm == ILACE_FRAME {
                    if dc_pred_dir == 0 {
                        self.zz_8x8[2][iu]
                    } else {
                        self.zz_8x8[3][iu]
                    }
                } else {
                    self.zzi_8x8[iu]
                };
                block[pos as usize] = value as i16;
                if last {
                    break;
                }
                i += 1;
            }
            if use_pred {
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return false;
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                let src = self.ac_val[ac];
                let (sh, half) = if dc_pred_dir != 0 { (self.left_blk_sh, 0) } else { (self.top_blk_sh, 8) };
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let e = &mut block[k << sh];
                        *e = (*e as i32).wrapping_add(Self::ac_rescale(src[k + half], q2, q1)) as i16;
                    }
                } else {
                    for k in 1..8 {
                        let e = &mut block[k << sh];
                        *e = e.wrapping_add(src[k + half]);
                    }
                }
            }
            for k in 1..8 {
                self.ac_val[ac2][k] = block[k << self.left_blk_sh];
                self.ac_val[ac2][k + 8] = block[k << self.top_blk_sh];
            }
            for k in 1..64 {
                if block[k] != 0 {
                    block[k] = self.scale_coef(block[k], scale, quant);
                }
            }
        } else {
            self.ac_val[ac2] = [0; 16];
            let half = if dc_pred_dir != 0 { 0 } else { 8 };
            if use_pred {
                let src = self.ac_val[ac];
                for k in 0..8 {
                    self.ac_val[ac2][half + k] = src[half + k];
                }
                q1 = q1.abs() * 2 + if q1 < 0 { 0 } else { self.halfpq } - 1;
                if q1 < 1 {
                    return false;
                }
                if q2 != 0 {
                    q2 = q2.abs() * 2 + if q2 < 0 { 0 } else { self.halfpq } - 1;
                }
                if q2 != 0 && q1 != q2 {
                    for k in 1..8 {
                        let v = self.ac_val[ac2][half + k];
                        self.ac_val[ac2][half + k] = Self::ac_rescale(v, q2, q1) as i16;
                    }
                }
            }
            if use_pred {
                let sh = if dc_pred_dir != 0 { self.left_blk_sh } else { self.top_blk_sh };
                for k in 1..8 {
                    let v = (self.ac_val[ac2][half + k] as i32).wrapping_mul(scale) as i16;
                    block[k << sh] = v;
                    if !self.pquantizer && v != 0 {
                        block[k << sh] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                }
            }
        }
        true
    }

    // ───────────────────────── transforms ─────────────────────────

    pub(crate) fn inv_trans_8x8(&self, block: &mut [i16; 64]) {
        match self.transform {
            TransformKind::Vc1 => dsp::inv_trans_8x8(block),
            TransformKind::Simple => idct::simple_idct_inplace(block),
        }
    }

    /// Adds the transformed sub-block `kind` of `block` (at coefficient
    /// offset `boff`) to `plane` at `off`.
    #[allow(clippy::too_many_arguments)]
    fn inv_trans_add(&mut self, kind: i32, dc_only: bool, plane: usize, off: isize, stride: usize, block: &mut [i16; 64], boff: usize) {
        let simple = self.transform == TransformKind::Simple;
        let Some(cur) = self.cur.as_mut() else { return };
        let data = &mut cur.pic.data[plane];
        let (w, h) = match kind {
            TT_8X8 => (8, 8),
            TT_8X4 => (8, 4),
            TT_4X8 => (4, 8),
            _ => (4, 4),
        };
        if off < 0 || off as usize + (h - 1) * stride + w > data.len() {
            return;
        }
        let off = off as usize;
        let b = &mut block[boff..];
        match (kind, simple, dc_only) {
            (TT_8X8, false, true) => dsp::inv_trans_8x8_dc(data, off, stride, b),
            (TT_8X8, true, true) => {
                let mut full = [0i16; 64];
                full.copy_from_slice(&b[..64]);
                idct::simple_idct_add(data, off, stride, &mut full);
                b[..64].copy_from_slice(&full);
            }
            (TT_8X4, false, true) => dsp::inv_trans_8x4_dc(data, off, stride, b),
            (TT_8X4, false, false) => dsp::inv_trans_8x4(data, off, stride, b),
            (TT_8X4, true, _) => idct::simple_idct84_add(data, off, stride, b),
            (TT_4X8, false, true) => dsp::inv_trans_4x8_dc(data, off, stride, b),
            (TT_4X8, false, false) => dsp::inv_trans_4x8(data, off, stride, b),
            (TT_4X8, true, _) => idct::simple_idct48_add(data, off, stride, b),
            (_, false, true) => dsp::inv_trans_4x4_dc(data, off, stride, b),
            (_, false, false) => dsp::inv_trans_4x4(data, off, stride, b),
            (_, true, _) => idct::simple_idct44_add(data, off, stride, b),
        }
    }

    /// `vc1_decode_p_block`: returns the coded sub-block pattern or None.
    #[allow(clippy::too_many_arguments)]
    fn decode_p_block(
        &mut self,
        gb: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        mquant: i32,
        ttmb: i32,
        first_block: bool,
        plane: usize,
        dst: isize,
        linesize: usize,
        ttmb_out: Option<&mut i32>,
    ) -> Option<i32> {
        let mut subblkpat: i32 = 0;
        let mut ttblk = ttmb & 7;
        let pat;
        let quant = mquant.abs();
        *block = [0; 64];
        if ttmb == -1 {
            let v = VLCS.ttblk[self.tt_index].get(gb);
            ttblk = VC1_TTBLK_TO_TT[self.tt_index][(v.clamp(0, 7)) as usize] as i32;
        }
        if ttblk == TT_4X4 {
            subblkpat = !(VLCS.subblkpat[self.tt_index].get(gb) + 1);
        }
        if (ttblk != TT_8X8 && ttblk != TT_4X4)
            && ((self.ttmbf || (ttmb != -1 && (ttmb & 8) != 0 && !first_block)) || (!self.res_rtm_flag && !first_block))
        {
            subblkpat = gb.decode012() as i32;
            if subblkpat != 0 {
                subblkpat ^= 3;
            }
            if ttblk == TT_8X4_TOP || ttblk == TT_8X4_BOTTOM {
                ttblk = TT_8X4;
            }
            if ttblk == TT_4X8_RIGHT || ttblk == TT_4X8_LEFT {
                ttblk = TT_4X8;
            }
        }
        let scale = quant * 2 + if mquant < 0 { 0 } else { self.halfpq };
        if ttblk == TT_8X4_TOP || ttblk == TT_8X4_BOTTOM {
            subblkpat = 2 - (ttblk == TT_8X4_TOP) as i32;
            ttblk = TT_8X4;
        }
        if ttblk == TT_4X8_RIGHT || ttblk == TT_4X8_LEFT {
            subblkpat = 2 - (ttblk == TT_4X8_LEFT) as i32;
            ttblk = TT_4X8;
        }
        let progressive = self.fcm == PROGRESSIVE;
        match ttblk {
            TT_8X8 => {
                pat = 0xF;
                let mut i = 0;
                loop {
                    let (last, skip, value) = self.decode_ac_coeff(gb, self.codingset2)?;
                    i += skip;
                    if i > 63 {
                        break;
                    }
                    let idx = if progressive { self.zz_8x8[0][i as usize] } else { self.zzi_8x8[i as usize] } as usize;
                    i += 1;
                    block[idx] = (value.wrapping_mul(scale)) as i16;
                    if !self.pquantizer {
                        let v = block[idx];
                        block[idx] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                    }
                    if last {
                        break;
                    }
                }
                if i == 1 {
                    self.inv_trans_add(TT_8X8, true, plane, dst, linesize, block, 0);
                } else {
                    self.inv_trans_8x8(block);
                    if let Some(cur) = self.cur.as_mut() {
                        let data = &mut cur.pic.data[plane];
                        if dst >= 0 && dst as usize + 7 * linesize + 8 <= data.len() {
                            dsp::add_pixels_clamped(block, data, dst as usize, linesize);
                        }
                    }
                }
            }
            TT_4X4 => {
                pat = !subblkpat & 0xF;
                for j in 0..4usize {
                    let mut last = subblkpat & (1 << (3 - j)) != 0;
                    let mut i = 0;
                    let off = (j & 1) * 4 + (j & 2) * 16;
                    while !last {
                        let (l, skip, value) = self.decode_ac_coeff(gb, self.codingset2)?;
                        last = l;
                        i += skip;
                        if i > 15 {
                            break;
                        }
                        let idx = if progressive {
                            VC1_SIMPLE_PROGRESSIVE_4X4_ZZ[i as usize]
                        } else {
                            VC1_ADV_INTERLACED_4X4_ZZ[i as usize]
                        } as usize;
                        i += 1;
                        let p = idx + off;
                        block[p] = value.wrapping_mul(scale) as i16;
                        if !self.pquantizer {
                            let v = block[p];
                            block[p] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                        }
                    }
                    if subblkpat & (1 << (3 - j)) == 0 {
                        let d = dst + ((j & 1) * 4) as isize + ((j & 2) * 2 * linesize) as isize;
                        self.inv_trans_add(TT_4X4, i == 1, plane, d, linesize, block, off);
                    }
                }
            }
            TT_8X4 => {
                pat = !((subblkpat & 2) * 6 + (subblkpat & 1) * 3) & 0xF;
                for j in 0..2usize {
                    let mut last = subblkpat & (1 << (1 - j)) != 0;
                    let mut i = 0;
                    let off = j * 32;
                    while !last {
                        let (l, skip, value) = self.decode_ac_coeff(gb, self.codingset2)?;
                        last = l;
                        i += skip;
                        if i > 31 {
                            break;
                        }
                        let idx = if progressive {
                            self.zz_8x4[i as usize] as usize + off
                        } else {
                            VC1_ADV_INTERLACED_8X4_ZZ[i as usize] as usize + off
                        };
                        i += 1;
                        block[idx] = value.wrapping_mul(scale) as i16;
                        if !self.pquantizer {
                            let v = block[idx];
                            block[idx] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                        }
                    }
                    if subblkpat & (1 << (1 - j)) == 0 {
                        let d = dst + (j * 4 * linesize) as isize;
                        self.inv_trans_add(TT_8X4, i == 1, plane, d, linesize, block, off);
                    }
                }
            }
            _ => {
                // TT_4X8
                pat = !(subblkpat * 5) & 0xF;
                for j in 0..2usize {
                    let mut last = subblkpat & (1 << (1 - j)) != 0;
                    let mut i = 0;
                    let off = j * 4;
                    while !last {
                        let (l, skip, value) = self.decode_ac_coeff(gb, self.codingset2)?;
                        last = l;
                        i += skip;
                        if i > 31 {
                            break;
                        }
                        let idx = if progressive {
                            self.zz_4x8[i as usize] as usize + off
                        } else {
                            VC1_ADV_INTERLACED_4X8_ZZ[i as usize] as usize + off
                        };
                        i += 1;
                        block[idx] = value.wrapping_mul(scale) as i16;
                        if !self.pquantizer {
                            let v = block[idx];
                            block[idx] = (v as i32 + if v < 0 { -quant } else { quant }) as i16;
                        }
                    }
                    if subblkpat & (1 << (1 - j)) == 0 {
                        let d = dst + (j * 4) as isize;
                        self.inv_trans_add(TT_4X8, i == 1, plane, d, linesize, block, off);
                    }
                }
            }
        }
        if let Some(out) = ttmb_out {
            *out |= ttblk << (n * 4);
        }
        Some(pat)
    }

    /// Destination (plane, offset, stride) of block `i` of the current MB
    /// for the progressive layouts.
    fn blk_dst(&self, i: usize) -> (usize, isize, usize) {
        if i < 4 {
            let off = (i & 1) as isize * 8 + (i & 2) as isize * 4 * self.linesize as isize;
            (0, self.dest[0] + off, self.linesize)
        } else {
            (i - 3, self.dest[i - 3], self.uvlinesize)
        }
    }

    // ───────────────────────── macroblocks ─────────────────────────

    /// `vc1_decode_p_mb`.
    fn decode_p_mb(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let mut ttmb = self.ttfrm;
        let mut mquant = self.pq;
        let mut first_block = true;
        let mut block_cbp: u32 = 0;
        let mut block_tt: i32 = 0;
        let mut block_intra: u8 = 0;

        let fourmv = if self.mv_type_is_raw { gb.read_bit() != 0 } else { self.mv_type_mb_plane[mb_pos] != 0 };
        let skipped = if self.skip_is_raw { gb.read_bit() != 0 } else { self.mbskip_table[mb_pos] != 0 };

        if !fourmv {
            if !skipped {
                let (dmv_x, dmv_y, mb_has_coeffs) = self.get_mvdata(gb);
                if self.mb_intra {
                    self.set_mv_at(1, self.block_index[0], [0, 0]);
                }
                self.set_cur_mb_type(mb_pos as isize, if self.mb_intra { MB_TYPE_INTRA } else { MB_TYPE_16X16 });
                self.pred_mv(gb, 0, dmv_x, dmv_y, true, self.range_x, self.range_y, 0, 0);
                let cbp;
                if self.mb_intra && !mb_has_coeffs {
                    self.get_mquant(gb, &mut mquant);
                    self.ac_pred = gb.read_bit() != 0;
                    cbp = 0;
                } else if mb_has_coeffs {
                    if self.mb_intra {
                        self.ac_pred = gb.read_bit() != 0;
                    }
                    cbp = self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                    self.get_mquant(gb, &mut mquant);
                } else {
                    mquant = self.pq;
                    cbp = 0;
                }
                self.set_qscale(mb_pos, mquant);
                if !self.ttmbf && !self.mb_intra && mb_has_coeffs {
                    ttmb = VLCS.ttmb[self.tt_index].get(gb);
                }
                if !self.mb_intra {
                    self.mc_1mv(0);
                }
                for i in 0..6 {
                    self.set_dc_val(i, 0);
                    let val = (cbp >> (5 - i)) & 1 != 0;
                    self.set_vmbt(self.block_index[i], self.mb_intra as u8);
                    if self.mb_intra {
                        self.a_avail = false;
                        self.c_avail = false;
                        if i == 2 || i == 3 || !self.first_slice_line {
                            self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
                        }
                        if i == 1 || i == 3 || self.mb_x != 0 {
                            self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
                        }
                        let d = BlkDst::Ring(BLOCK_MAP[i]);
                        let mut b = self.load_blk(d);
                        let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                        if !self.decode_intra_block(gb, &mut b, i, val, mquant, cs) {
                            return false;
                        }
                        self.inv_trans_8x8(&mut b);
                        if self.rangeredfrm {
                            for v in b.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        self.store_blk(d, &b);
                        block_cbp |= 0xF << (i << 2);
                        block_intra |= 1 << i;
                    } else if val {
                        let d = BlkDst::Ring(BLOCK_MAP[i]);
                        let mut b = self.load_blk(d);
                        let (plane, dst, ls) = self.blk_dst(i);
                        let Some(pat) =
                            self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                        else {
                            return false;
                        };
                        self.store_blk(d, &b);
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            } else {
                self.mb_intra = false;
                for i in 0..6 {
                    self.set_vmbt(self.block_index[i], 0);
                    self.set_dc_val(i, 0);
                }
                self.set_cur_mb_type(mb_pos as isize, MB_TYPE_SKIP);
                self.set_qscale(mb_pos, 0);
                self.pred_mv(gb, 0, 0, 0, true, self.range_x, self.range_y, 0, 0);
                self.mc_1mv(0);
            }
        } else if !skipped {
            let mut intra_count = 0;
            let mut coded_inter = false;
            let mut is_intra = [false; 6];
            let mut is_coded = [false; 6];
            let cbp = self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            for i in 0..6 {
                let val = (cbp >> (5 - i)) & 1 != 0;
                self.set_dc_val(i, 0);
                self.mb_intra = false;
                if i < 4 {
                    let (mut dmv_x, mut dmv_y) = (0, 0);
                    self.mb_intra = false;
                    let mut mb_has_coeffs = false;
                    if val {
                        let r = self.get_mvdata(gb);
                        dmv_x = r.0;
                        dmv_y = r.1;
                        mb_has_coeffs = r.2;
                    }
                    self.pred_mv(gb, i, dmv_x, dmv_y, false, self.range_x, self.range_y, 0, 0);
                    if !self.mb_intra {
                        self.mc_4mv_luma(i, 0, false);
                    }
                    intra_count += self.mb_intra as i32;
                    is_intra[i] = self.mb_intra;
                    is_coded[i] = mb_has_coeffs;
                }
                if i & 4 != 0 {
                    is_intra[i] = intra_count >= 3;
                    is_coded[i] = val;
                }
                if i == 4 {
                    self.mc_4mv_chroma(0);
                }
                self.set_vmbt(self.block_index[i], is_intra[i] as u8);
                if !coded_inter {
                    coded_inter = !is_intra[i] & is_coded[i];
                }
            }
            if intra_count != 0 || coded_inter {
                self.get_mquant(gb, &mut mquant);
                self.set_qscale(mb_pos, mquant);
                let mut intrapred = false;
                for i in 0..6 {
                    if is_intra[i]
                        && (((!self.first_slice_line || i == 2 || i == 3)
                            && self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0)
                            || ((self.mb_x != 0 || i == 1 || i == 3) && self.vmbt(self.block_index[i] - 1) != 0))
                    {
                        intrapred = true;
                        break;
                    }
                }
                self.ac_pred = if intrapred { gb.read_bit() != 0 } else { false };
                if !self.ttmbf && coded_inter {
                    ttmb = VLCS.ttmb[self.tt_index].get(gb);
                }
                for i in 0..6 {
                    self.mb_intra = is_intra[i];
                    if is_intra[i] {
                        self.a_avail = false;
                        self.c_avail = false;
                        if i == 2 || i == 3 || !self.first_slice_line {
                            self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
                        }
                        if i == 1 || i == 3 || self.mb_x != 0 {
                            self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
                        }
                        let d = BlkDst::Ring(BLOCK_MAP[i]);
                        let mut b = self.load_blk(d);
                        let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                        if !self.decode_intra_block(gb, &mut b, i, is_coded[i], mquant, cs) {
                            return false;
                        }
                        self.inv_trans_8x8(&mut b);
                        if self.rangeredfrm {
                            for v in b.iter_mut() {
                                *v = v.wrapping_mul(2);
                            }
                        }
                        self.store_blk(d, &b);
                        block_cbp |= 0xF << (i << 2);
                        block_intra |= 1 << i;
                    } else if is_coded[i] {
                        let d = BlkDst::Ring(BLOCK_MAP[i]);
                        let mut b = self.load_blk(d);
                        let (plane, dst, ls) = self.blk_dst(i);
                        let Some(pat) =
                            self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                        else {
                            return false;
                        };
                        self.store_blk(d, &b);
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            }
        } else {
            self.mb_intra = false;
            self.set_qscale(mb_pos, 0);
            for i in 0..6 {
                self.set_vmbt(self.block_index[i], 0);
                self.set_dc_val(i, 0);
            }
            for i in 0..4 {
                self.pred_mv(gb, i, 0, 0, false, self.range_x, self.range_y, 0, 0);
                self.mc_4mv_luma(i, 0, false);
            }
            self.mc_4mv_chroma(0);
            self.set_qscale(mb_pos, 0);
        }
        // end:
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        let r = self.row3(self.mb_x as isize);
        self.cbp_base[r] = block_cbp;
        self.ttblk_base[r] = block_tt;
        self.is_intra_base[r] = block_intra;
        true
    }

    /// Decodes the six intra blocks of an interlaced intra MB (P/B
    /// interlaced pictures), storing to the ring or the scratch blocks.
    fn decode_intra_mb_blocks(&mut self, gb: &mut BitReader, cbp: i32, mquant: i32, ring: bool) -> Option<u32> {
        let mut block_cbp = 0u32;
        for i in 0..6 {
            self.a_avail = false;
            self.c_avail = false;
            self.set_vmbt(self.block_index[i], 1);
            self.set_dc_val(i, 0);
            let val = (cbp >> (5 - i)) & 1 != 0;
            if i == 2 || i == 3 || !self.first_slice_line {
                self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
            }
            if i == 1 || i == 3 || self.mb_x != 0 {
                self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
            }
            let d = if ring { BlkDst::Ring(BLOCK_MAP[i]) } else { BlkDst::Scratch(i) };
            let mut b = self.load_blk(d);
            let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
            if !self.decode_intra_block(gb, &mut b, i, val, mquant, cs) {
                return None;
            }
            self.inv_trans_8x8(&mut b);
            self.store_blk(d, &b);
            block_cbp |= 0xF << (i << 2);
        }
        Some(block_cbp)
    }

    /// `vc1_decode_p_mb_intfr`.
    fn decode_p_mb_intfr(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let mut cbp = 0;
        let mut ttmb = self.ttfrm;
        let mut mquant = self.pq;
        let mut first_block = true;
        let mut block_cbp = 0u32;
        let mut block_tt = 0i32;
        let mut fourmv = false;
        let mut twomv = false;
        let mut idx_mbmode = 0usize;

        let skipped = if self.skip_is_raw { gb.read_bit() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        if !skipped {
            idx_mbmode = self.mbmode_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0).clamp(0, 14) as usize;
            let mode = VC1_MBMODE_INTFRP[self.fourmvswitch as usize][idx_mbmode];
            match mode[0] {
                MV_PMODE_INTFR_4MV => {
                    fourmv = true;
                    self.set_blk_mv_type4(0);
                }
                MV_PMODE_INTFR_4MV_FIELD => {
                    fourmv = true;
                    self.set_blk_mv_type4(1);
                }
                MV_PMODE_INTFR_2MV_FIELD => {
                    twomv = true;
                    self.set_blk_mv_type4(1);
                }
                MV_PMODE_INTFR_1MV => self.set_blk_mv_type4(0),
                _ => {}
            }
            if mode[0] == MV_PMODE_INTFR_INTRA {
                for i in 0..4 {
                    self.set_mv_at(1, self.block_index[i], [0, 0]);
                }
                let r = self.row3(self.mb_x as isize);
                self.is_intra_base[r] = 0x3f;
                self.mb_intra = true;
                self.set_cur_mb_type(mb_pos as isize, MB_TYPE_INTRA);
                let fieldtx = gb.read_bit() as u8;
                self.fieldtx_plane[mb_pos] = fieldtx;
                let mb_has_coeffs = gb.read_bit() != 0;
                if mb_has_coeffs {
                    cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                }
                self.ac_pred = gb.read_bit() != 0;
                self.acpred_plane[mb_pos] = self.ac_pred as u8;
                self.get_mquant(gb, &mut mquant);
                self.set_qscale(mb_pos, mquant);
                self.y_dc_scale = WMV3_DC_SCALE_TABLE[(mquant.unsigned_abs() as usize) & 31] as i32;
                let Some(bc) = self.decode_intra_mb_blocks(gb, cbp, mquant, true) else { return false };
                block_cbp = bc;
            } else {
                let mb_has_coeffs = mode[3] != 0;
                if mb_has_coeffs {
                    cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                }
                if mode[0] == MV_PMODE_INTFR_2MV_FIELD {
                    self.twomvbp = self.twomvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                } else if mode[0] == MV_PMODE_INTFR_4MV || mode[0] == MV_PMODE_INTFR_4MV_FIELD {
                    self.fourmvbp = self.fourmvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                }
                self.mb_intra = false;
                let r = self.row3(self.mb_x as isize);
                self.is_intra_base[r] = 0;
                for i in 0..6 {
                    self.set_vmbt(self.block_index[i], 0);
                }
                let fieldtx = mode[1];
                self.fieldtx_plane[mb_pos] = fieldtx;
                if fourmv {
                    let mvbp = self.fourmvbp;
                    for i in 0..4 {
                        let (mut dx, mut dy) = (0, 0);
                        if mvbp & (8 >> i) != 0 {
                            let r = self.get_mvdata_interlaced(gb, false);
                            dx = r.0;
                            dy = r.1;
                        }
                        self.pred_mv_intfr(i, dx, dy, 0, self.range_x, self.range_y, 0);
                        self.mc_4mv_luma(i, 0, false);
                    }
                    self.mc_4mv_chroma4(0, 0, false);
                } else if twomv {
                    let mvbp = self.twomvbp;
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 2, self.range_x, self.range_y, 0);
                    self.mc_4mv_luma(0, 0, false);
                    self.mc_4mv_luma(1, 0, false);
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(2, dx, dy, 2, self.range_x, self.range_y, 0);
                    self.mc_4mv_luma(2, 0, false);
                    self.mc_4mv_luma(3, 0, false);
                    self.mc_4mv_chroma4(0, 0, false);
                } else {
                    let mvbp = mode[2];
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 1, self.range_x, self.range_y, 0);
                    self.mc_1mv(0);
                }
                if cbp != 0 {
                    self.get_mquant(gb, &mut mquant);
                }
                self.set_qscale(mb_pos, mquant);
                if !self.ttmbf && cbp != 0 {
                    ttmb = VLCS.ttmb[self.tt_index].get(gb);
                }
                for i in 0..6 {
                    self.set_dc_val(i, 0);
                    let val = (cbp >> (5 - i)) & 1 != 0;
                    let ft = fieldtx as usize;
                    let (plane, dst, ls) = if i < 4 {
                        let off = if ft == 0 {
                            (i & 1) as isize * 8 + (i & 2) as isize * 4 * self.linesize as isize
                        } else {
                            (i & 1) as isize * 8 + (i > 1) as isize * self.linesize as isize
                        };
                        (0, self.dest[0] + off, self.linesize << ft)
                    } else {
                        (i - 3, self.dest[i - 3], self.uvlinesize)
                    };
                    if val {
                        let d = BlkDst::Ring(BLOCK_MAP[i]);
                        let mut b = self.load_blk(d);
                        let Some(pat) =
                            self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                        else {
                            return false;
                        };
                        self.store_blk(d, &b);
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            }
        } else {
            self.mb_intra = false;
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0;
            for i in 0..6 {
                self.set_vmbt(self.block_index[i], 0);
                self.set_dc_val(i, 0);
            }
            self.set_cur_mb_type(mb_pos as isize, MB_TYPE_SKIP);
            self.set_qscale(mb_pos, 0);
            self.set_blk_mv_type4(0);
            self.pred_mv_intfr(0, 0, 0, 1, self.range_x, self.range_y, 0);
            self.mc_1mv(0);
            self.fieldtx_plane[mb_pos] = 0;
        }
        let _ = idx_mbmode;
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        let r = self.row3(self.mb_x as isize);
        self.cbp_base[r] = block_cbp;
        self.ttblk_base[r] = block_tt;
        true
    }

    /// `vc1_decode_p_mb_intfi`.
    fn decode_p_mb_intfi(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let mut cbp = 0;
        let mut ttmb = self.ttfrm;
        let mut mquant = self.pq;
        let mut first_block = true;
        let mut block_cbp = 0u32;
        let mut block_tt = 0i32;

        let idx_mbmode = self.mbmode_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
        if idx_mbmode <= 1 {
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0x3f;
            self.mb_intra = true;
            self.set_mv_at(1, self.block_index[0] + self.blocks_off, [0, 0]);
            self.set_cur_mb_type(mb_pos as isize + self.mb_off, MB_TYPE_INTRA);
            self.get_mquant(gb, &mut mquant);
            self.set_qscale(mb_pos, mquant);
            self.y_dc_scale = WMV3_DC_SCALE_TABLE[(mquant.unsigned_abs() as usize) & 31] as i32;
            self.ac_pred = gb.read_bit() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            let mb_has_coeffs = idx_mbmode & 1 != 0;
            if mb_has_coeffs {
                cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            }
            let Some(bc) = self.decode_intra_mb_blocks(gb, cbp, mquant, true) else { return false };
            block_cbp = bc;
        } else {
            self.mb_intra = false;
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0;
            self.set_cur_mb_type(mb_pos as isize + self.mb_off, MB_TYPE_16X16);
            for i in 0..6 {
                self.set_vmbt(self.block_index[i], 0);
            }
            let mb_has_coeffs;
            if idx_mbmode <= 5 {
                let (mut dx, mut dy, mut pf) = (0, 0, 0);
                if idx_mbmode & 1 != 0 {
                    let r = self.get_mvdata_interlaced(gb, true);
                    dx = r.0;
                    dy = r.1;
                    pf = r.2;
                }
                self.pred_mv(gb, 0, dx, dy, true, self.range_x, self.range_y, pf, 0);
                self.mc_1mv(0);
                mb_has_coeffs = idx_mbmode & 2 == 0;
            } else {
                self.fourmvbp = self.fourmvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                for i in 0..4 {
                    let (mut dx, mut dy, mut pf) = (0, 0, 0);
                    if self.fourmvbp & (8 >> i) != 0 {
                        let r = self.get_mvdata_interlaced(gb, true);
                        dx = r.0;
                        dy = r.1;
                        pf = r.2;
                    }
                    self.pred_mv(gb, i, dx, dy, false, self.range_x, self.range_y, pf, 0);
                    self.mc_4mv_luma(i, 0, false);
                }
                self.mc_4mv_chroma(0);
                mb_has_coeffs = idx_mbmode & 1 != 0;
            }
            if mb_has_coeffs {
                cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            }
            if cbp != 0 {
                self.get_mquant(gb, &mut mquant);
            }
            self.set_qscale(mb_pos, mquant);
            if !self.ttmbf && cbp != 0 {
                ttmb = VLCS.ttmb[self.tt_index].get(gb);
            }
            for i in 0..6 {
                self.set_dc_val(i, 0);
                let val = (cbp >> (5 - i)) & 1 != 0;
                if val {
                    let (plane, dst, ls) = self.blk_dst(i);
                    let d = BlkDst::Ring(BLOCK_MAP[i]);
                    let mut b = self.load_blk(d);
                    let Some(pat) =
                        self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                    else {
                        return false;
                    };
                    self.store_blk(d, &b);
                    block_cbp |= (pat as u32) << (i << 2);
                    if !self.ttmbf && ttmb < 8 {
                        ttmb = -1;
                    }
                    first_block = false;
                }
            }
        }
        if self.overlap && self.pq >= 9 {
            self.p_overlap_filter();
        }
        self.put_blocks_clamped(true);
        let r = self.row3(self.mb_x as isize);
        self.cbp_base[r] = block_cbp;
        self.ttblk_base[r] = block_tt;
        true
    }

    /// Writes scratch block `i` with `put_signed_pixels_clamped`.
    fn put_scratch_signed(&mut self, i: usize, plane: usize, off: isize, stride: usize) {
        let b = self.blocks[i];
        if let Some(cur) = self.cur.as_mut() {
            let data = &mut cur.pic.data[plane];
            if off >= 0 && off as usize + 7 * stride + 8 <= data.len() {
                dsp::put_signed_pixels_clamped(&b, data, off as usize, stride);
            }
        }
    }

    /// `vc1_decode_b_mb`.
    fn decode_b_mb(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let cbp;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut mb_has_coeffs = false;
        let mut first_block = true;
        let mut dmv_x = [0i32; 2];
        let mut dmv_y = [0i32; 2];
        let mut bmvtype = BMV_TYPE_BACKWARD;
        self.mb_intra = false;

        let direct = if self.dmb_is_raw { gb.read_bit() != 0 } else { self.direct_mb_plane[mb_pos] != 0 };
        let skipped = if self.skip_is_raw { gb.read_bit() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        for i in 0..6 {
            self.set_vmbt(self.block_index[i], 0);
            self.set_dc_val(i, 0);
        }
        self.set_qscale(mb_pos, 0);

        if !direct {
            if !skipped {
                let r = self.get_mvdata(gb);
                dmv_x[0] = r.0;
                dmv_y[0] = r.1;
                mb_has_coeffs = r.2;
                dmv_x[1] = dmv_x[0];
                dmv_y[1] = dmv_y[0];
            }
            if skipped || !self.mb_intra {
                bmvtype = match gb.decode012() {
                    0 => {
                        if self.bfraction >= B_FRACTION_DEN / 2 {
                            BMV_TYPE_BACKWARD
                        } else {
                            BMV_TYPE_FORWARD
                        }
                    }
                    1 => {
                        if self.bfraction >= B_FRACTION_DEN / 2 {
                            BMV_TYPE_FORWARD
                        } else {
                            BMV_TYPE_BACKWARD
                        }
                    }
                    _ => {
                        dmv_x[0] = 0;
                        dmv_y[0] = 0;
                        BMV_TYPE_INTERPOLATED
                    }
                };
            }
        }
        for i in 0..6 {
            self.set_vmbt(self.block_index[i], self.mb_intra as u8);
        }
        if skipped {
            if direct {
                bmvtype = BMV_TYPE_INTERPOLATED;
            }
            self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
            self.b_mc(direct, bmvtype);
            return true;
        }
        if direct {
            cbp = self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            self.get_mquant(gb, &mut mquant);
            self.mb_intra = false;
            self.set_qscale(mb_pos, mquant);
            if !self.ttmbf {
                ttmb = VLCS.ttmb[self.tt_index].get(gb);
            }
            dmv_x = [0; 2];
            dmv_y = [0; 2];
            self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
            self.b_mc(direct, bmvtype);
        } else {
            if !mb_has_coeffs && !self.mb_intra {
                self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
                self.b_mc(direct, bmvtype);
                return true;
            }
            if self.mb_intra && !mb_has_coeffs {
                self.get_mquant(gb, &mut mquant);
                self.set_qscale(mb_pos, mquant);
                self.ac_pred = gb.read_bit() != 0;
                cbp = 0;
                self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
            } else {
                if bmvtype == BMV_TYPE_INTERPOLATED {
                    let r = self.get_mvdata(gb);
                    dmv_x[0] = r.0;
                    dmv_y[0] = r.1;
                    mb_has_coeffs = r.2;
                    if !mb_has_coeffs {
                        self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
                        self.b_mc(direct, bmvtype);
                        return true;
                    }
                }
                self.pred_b_mv(&mut dmv_x, &mut dmv_y, direct, bmvtype);
                if !self.mb_intra {
                    self.b_mc(direct, bmvtype);
                }
                if self.mb_intra {
                    self.ac_pred = gb.read_bit() != 0;
                }
                cbp = self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                self.get_mquant(gb, &mut mquant);
                self.set_qscale(mb_pos, mquant);
                if !self.ttmbf && !self.mb_intra && mb_has_coeffs {
                    ttmb = VLCS.ttmb[self.tt_index].get(gb);
                }
            }
        }
        for i in 0..6 {
            self.set_dc_val(i, 0);
            let val = (cbp >> (5 - i)) & 1 != 0;
            let (plane, dst, ls) = self.blk_dst(i);
            self.set_vmbt(self.block_index[i], self.mb_intra as u8);
            if self.mb_intra {
                self.a_avail = false;
                self.c_avail = false;
                if i == 2 || i == 3 || !self.first_slice_line {
                    self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
                }
                if i == 1 || i == 3 || self.mb_x != 0 {
                    self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
                }
                let mut b = self.blocks[i];
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                if !self.decode_intra_block(gb, &mut b, i, val, mquant, cs) {
                    return false;
                }
                self.inv_trans_8x8(&mut b);
                if self.rangeredfrm {
                    for v in b.iter_mut() {
                        *v = v.wrapping_mul(2);
                    }
                }
                self.blocks[i] = b;
                self.put_scratch_signed(i, plane, dst, ls);
            } else if val {
                let mut b = self.blocks[i];
                if self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, None).is_none() {
                    return false;
                }
                self.blocks[i] = b;
                if !self.ttmbf && ttmb < 8 {
                    ttmb = -1;
                }
                first_block = false;
            }
        }
        true
    }

    /// `vc1_decode_b_mb_intfi`.
    fn decode_b_mb_intfi(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let mut cbp = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut first_block = true;
        let mut dmv_x = [0i32; 2];
        let mut dmv_y = [0i32; 2];
        let mut pred_flag = [0i32; 2];
        let mut bmvtype = BMV_TYPE_BACKWARD;
        let mut block_cbp = 0u32;
        let mut block_tt = 0i32;
        self.mb_intra = false;

        let idx_mbmode = self.mbmode_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
        if idx_mbmode <= 1 {
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0x3f;
            self.mb_intra = true;
            self.set_mv_at(1, self.block_index[0], [0, 0]);
            self.set_cur_mb_type(mb_pos as isize + self.mb_off, MB_TYPE_INTRA);
            self.get_mquant(gb, &mut mquant);
            self.set_qscale(mb_pos, mquant);
            self.y_dc_scale = WMV3_DC_SCALE_TABLE[(mquant.unsigned_abs() as usize) & 31] as i32;
            self.ac_pred = gb.read_bit() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            let mb_has_coeffs = idx_mbmode & 1 != 0;
            if mb_has_coeffs {
                cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            }
            for i in 0..6 {
                self.a_avail = false;
                self.c_avail = false;
                self.set_vmbt(self.block_index[i], 1);
                self.set_dc_val(i, 0);
                let val = (cbp >> (5 - i)) & 1 != 0;
                if i == 2 || i == 3 || !self.first_slice_line {
                    self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
                }
                if i == 1 || i == 3 || self.mb_x != 0 {
                    self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
                }
                let mut b = self.blocks[i];
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                if !self.decode_intra_block(gb, &mut b, i, val, mquant, cs) {
                    return false;
                }
                self.inv_trans_8x8(&mut b);
                if self.rangeredfrm {
                    for v in b.iter_mut() {
                        *v = ((*v as i32) << 1) as i16;
                    }
                }
                self.blocks[i] = b;
                let (plane, dst, ls) = self.blk_dst(i);
                self.put_scratch_signed(i, plane, dst, ls);
            }
        } else {
            self.mb_intra = false;
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0;
            self.set_cur_mb_type(mb_pos as isize + self.mb_off, MB_TYPE_16X16);
            for i in 0..6 {
                self.set_vmbt(self.block_index[i], 0);
            }
            let fwd = if self.fmb_is_raw {
                let f = gb.read_bit() as u8;
                self.forward_mb_plane[mb_pos] = f;
                f != 0
            } else {
                self.forward_mb_plane[mb_pos] != 0
            };
            let mb_has_coeffs;
            if idx_mbmode <= 5 {
                let mut interpmvp = false;
                if fwd {
                    bmvtype = BMV_TYPE_FORWARD;
                } else {
                    bmvtype = match gb.decode012() {
                        0 => BMV_TYPE_BACKWARD,
                        1 => BMV_TYPE_DIRECT,
                        _ => {
                            interpmvp = gb.read_bit() != 0;
                            BMV_TYPE_INTERPOLATED
                        }
                    };
                }
                self.bmvtype = bmvtype;
                if bmvtype != BMV_TYPE_DIRECT && idx_mbmode & 1 != 0 {
                    let k = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let r = self.get_mvdata_interlaced(gb, true);
                    dmv_x[k] = r.0;
                    dmv_y[k] = r.1;
                    pred_flag[k] = r.2;
                }
                if interpmvp {
                    let r = self.get_mvdata_interlaced(gb, true);
                    dmv_x[1] = r.0;
                    dmv_y[1] = r.1;
                    pred_flag[1] = r.2;
                }
                if bmvtype == BMV_TYPE_DIRECT {
                    dmv_x = [0; 2];
                    dmv_y = [0; 2];
                    pred_flag[0] = 0;
                    if !self.next.as_ref().is_some_and(|n| n.field_picture) {
                        return false;
                    }
                }
                self.pred_b_mv_intfi(gb, 0, &dmv_x, &dmv_y, true, &pred_flag);
                self.b_mc(bmvtype == BMV_TYPE_DIRECT, bmvtype);
                mb_has_coeffs = idx_mbmode & 2 == 0;
            } else {
                if fwd {
                    bmvtype = BMV_TYPE_FORWARD;
                }
                self.bmvtype = bmvtype;
                self.fourmvbp = self.fourmvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                for i in 0..4 {
                    dmv_x = [0; 2];
                    dmv_y = [0; 2];
                    pred_flag = [0; 2];
                    if self.fourmvbp & (8 >> i) != 0 {
                        let k = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                        let r = self.get_mvdata_interlaced(gb, true);
                        dmv_x[k] = r.0;
                        dmv_y[k] = r.1;
                        pred_flag[k] = r.2;
                    }
                    self.pred_b_mv_intfi(gb, i, &dmv_x, &dmv_y, false, &pred_flag);
                    self.mc_4mv_luma(i, (bmvtype == BMV_TYPE_BACKWARD) as usize, false);
                }
                self.mc_4mv_chroma((bmvtype == BMV_TYPE_BACKWARD) as usize);
                mb_has_coeffs = idx_mbmode & 1 != 0;
            }
            if mb_has_coeffs {
                cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            }
            if cbp != 0 {
                self.get_mquant(gb, &mut mquant);
            }
            self.set_qscale(mb_pos, mquant);
            if !self.ttmbf && cbp != 0 {
                ttmb = VLCS.ttmb[self.tt_index].get(gb);
            }
            for i in 0..6 {
                self.set_dc_val(i, 0);
                let val = (cbp >> (5 - i)) & 1 != 0;
                if val {
                    let (plane, dst, ls) = self.blk_dst(i);
                    let mut b = self.blocks[i];
                    let Some(pat) =
                        self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                    else {
                        return false;
                    };
                    self.blocks[i] = b;
                    block_cbp |= (pat as u32) << (i << 2);
                    if !self.ttmbf && ttmb < 8 {
                        ttmb = -1;
                    }
                    first_block = false;
                }
            }
        }
        let r = self.row3(self.mb_x as isize);
        self.cbp_base[r] = block_cbp;
        self.ttblk_base[r] = block_tt;
        true
    }

    /// Copies MVs between blocks `i` and `i+2` (the mvsw handling of
    /// interlaced frame B MBs).
    fn mvsw_copy(&mut self, dir: usize, dir2: usize) {
        for i in 0..2 {
            let a = self.mv_at(dir, self.block_index[i]);
            self.set_mv_at(dir, self.block_index[i + 2], a);
            self.mv[dir][i] = [a[0] as i32, a[1] as i32];
            self.mv[dir][i + 2] = [a[0] as i32, a[1] as i32];
            let b = self.mv_at(dir2, self.block_index[i + 2]);
            self.set_mv_at(dir2, self.block_index[i], b);
            self.mv[dir2][i] = [b[0] as i32, b[1] as i32];
            self.mv[dir2][i + 2] = [b[0] as i32, b[1] as i32];
        }
    }

    /// The `!dir` copy loop: `mv[!dir][i+2] = mv[!dir][i] = motion_val[!dir][block_index[i]]`
    /// with `motion_val[!dir][block_index[i+2]]` updated too.
    fn copy_top_to_bottom(&mut self, d: usize) {
        for i in 0..2 {
            let a = self.mv_at(d, self.block_index[i]);
            self.set_mv_at(d, self.block_index[i + 2], a);
            self.mv[d][i] = [a[0] as i32, a[1] as i32];
            self.mv[d][i + 2] = [a[0] as i32, a[1] as i32];
        }
    }

    /// `vc1_decode_b_mb_intfr`.
    fn decode_b_mb_intfr(&mut self, gb: &mut BitReader) -> bool {
        let mb_pos = self.mb_pos();
        let mut cbp = 0;
        let mut mquant = self.pq;
        let mut ttmb = self.ttfrm;
        let mut mvsw = false;
        let mut first_block = true;
        let mut twomv = false;
        let mut block_cbp = 0u32;
        let mut block_tt = 0i32;
        let mut idx_mbmode = 0usize;
        let mut bmvtype = BMV_TYPE_BACKWARD;
        self.mb_intra = false;

        let skipped = if self.skip_is_raw { gb.read_bit() != 0 } else { self.mbskip_table[mb_pos] != 0 };
        if !skipped {
            idx_mbmode = self.mbmode_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0).clamp(0, 8) as usize;
            if VC1_MBMODE_INTFRP[0][idx_mbmode][0] == MV_PMODE_INTFR_2MV_FIELD {
                twomv = true;
                self.set_blk_mv_type4(1);
            } else {
                self.set_blk_mv_type4(0);
            }
        }
        let mode = VC1_MBMODE_INTFRP[0][idx_mbmode];
        if mode[0] == MV_PMODE_INTFR_INTRA {
            for i in 0..4 {
                self.mv[0][i] = [0, 0];
                self.mv[1][i] = [0, 0];
                self.set_mv_at(0, self.block_index[i], [0, 0]);
                self.set_mv_at(1, self.block_index[i], [0, 0]);
            }
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0x3f;
            self.mb_intra = true;
            self.set_cur_mb_type(mb_pos as isize, MB_TYPE_INTRA);
            let fieldtx = gb.read_bit() as u8;
            self.fieldtx_plane[mb_pos] = fieldtx;
            let mb_has_coeffs = gb.read_bit() != 0;
            if mb_has_coeffs {
                cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
            }
            self.ac_pred = gb.read_bit() != 0;
            self.acpred_plane[mb_pos] = self.ac_pred as u8;
            self.get_mquant(gb, &mut mquant);
            self.set_qscale(mb_pos, mquant);
            self.y_dc_scale = WMV3_DC_SCALE_TABLE[(mquant.unsigned_abs() as usize) & 31] as i32;
            for i in 0..6 {
                self.a_avail = false;
                self.c_avail = false;
                self.set_vmbt(self.block_index[i], 1);
                self.set_dc_val(i, 0);
                let val = (cbp >> (5 - i)) & 1 != 0;
                if i == 2 || i == 3 || !self.first_slice_line {
                    self.a_avail = self.vmbt(self.block_index[i] - self.block_wrap(i)) != 0;
                }
                if i == 1 || i == 3 || self.mb_x != 0 {
                    self.c_avail = self.vmbt(self.block_index[i] - 1) != 0;
                }
                let mut b = self.blocks[i];
                let cs = if i & 4 != 0 { self.codingset2 } else { self.codingset };
                if !self.decode_intra_block(gb, &mut b, i, val, mquant, cs) {
                    return false;
                }
                self.inv_trans_8x8(&mut b);
                self.blocks[i] = b;
                let ft = fieldtx as usize;
                let (plane, off, stride) = if i < 4 {
                    let off = if ft != 0 {
                        ((i & 1) * 8) as isize + ((i & 2) >> 1) as isize * self.linesize as isize
                    } else {
                        ((i & 1) * 8) as isize + (4 * (i & 2)) as isize * self.linesize as isize
                    };
                    (0, self.dest[0] + off, self.linesize << ft)
                } else {
                    (i - 3, self.dest[i - 3], self.uvlinesize)
                };
                self.put_scratch_signed(i, plane, off, stride);
            }
        } else {
            self.mb_intra = false;
            let r = self.row3(self.mb_x as isize);
            self.is_intra_base[r] = 0;
            let direct = if self.dmb_is_raw { gb.read_bit() != 0 } else { self.direct_mb_plane[mb_pos] != 0 };
            if direct {
                let qs = self.quarter_sample;
                let bf = self.bfraction;
                let nb = |s: &Self, i: usize| -> [i16; 2] {
                    let o = (s.mvo() + s.block_index[i]) as usize;
                    s.next.as_ref().map(|n| n.motion_val[1][o]).unwrap_or([0, 0])
                };
                let n0 = nb(self, 0);
                for (dir, inv) in [(0usize, 0), (1usize, 1)] {
                    let v = [
                        pred::scale_mv(n0[0] as i32, bf, inv, qs),
                        pred::scale_mv(n0[1] as i32, bf, inv, qs),
                    ];
                    self.mv[dir][0] = v;
                    self.set_mv_at(dir, self.block_index[0], [v[0] as i16, v[1] as i16]);
                }
                if twomv {
                    let n2 = nb(self, 2);
                    for (dir, inv) in [(0usize, 0), (1usize, 1)] {
                        let v = [
                            pred::scale_mv(n2[0] as i32, bf, inv, qs),
                            pred::scale_mv(n2[1] as i32, bf, inv, qs),
                        ];
                        self.mv[dir][2] = v;
                        self.set_mv_at(dir, self.block_index[2], [v[0] as i16, v[1] as i16]);
                    }
                    for i in [1usize, 3] {
                        for dir in 0..2 {
                            let v = self.mv[dir][i - 1];
                            self.mv[dir][i] = v;
                            self.set_mv_at(dir, self.block_index[i], [v[0] as i16, v[1] as i16]);
                        }
                    }
                } else {
                    for i in 1..4 {
                        for dir in 0..2 {
                            let v = self.mv[dir][0];
                            self.mv[dir][i] = v;
                            self.set_mv_at(dir, self.block_index[i], [v[0] as i16, v[1] as i16]);
                        }
                    }
                }
            }
            if !direct {
                if skipped || !self.mb_intra {
                    bmvtype = match gb.decode012() {
                        0 => {
                            if self.bfraction >= B_FRACTION_DEN / 2 {
                                BMV_TYPE_BACKWARD
                            } else {
                                BMV_TYPE_FORWARD
                            }
                        }
                        1 => {
                            if self.bfraction >= B_FRACTION_DEN / 2 {
                                BMV_TYPE_FORWARD
                            } else {
                                BMV_TYPE_BACKWARD
                            }
                        }
                        _ => BMV_TYPE_INTERPOLATED,
                    };
                }
                if twomv && bmvtype != BMV_TYPE_INTERPOLATED {
                    mvsw = gb.read_bit() != 0;
                }
            }
            if !skipped {
                let mb_has_coeffs = mode[3] != 0;
                if mb_has_coeffs {
                    cbp = 1 + self.cbpcy_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                }
                if !direct {
                    if bmvtype == BMV_TYPE_INTERPOLATED && twomv {
                        self.fourmvbp = self.fourmvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                    } else if bmvtype == BMV_TYPE_INTERPOLATED || twomv {
                        self.twomvbp = self.twomvbp_vlc.vlc().map(|v| v.get(gb)).unwrap_or(0);
                    }
                }
                for i in 0..6 {
                    self.set_vmbt(self.block_index[i], 0);
                }
                let fieldtx = mode[1];
                self.fieldtx_plane[mb_pos] = fieldtx;
                if direct {
                    if twomv {
                        for i in 0..4 {
                            self.mc_4mv_luma(i, 0, false);
                            self.mc_4mv_luma(i, 1, true);
                        }
                        self.mc_4mv_chroma4(0, 0, false);
                        self.mc_4mv_chroma4(1, 1, true);
                    } else {
                        self.mc_1mv(0);
                        self.interp_mc();
                    }
                } else if twomv && bmvtype == BMV_TYPE_INTERPOLATED {
                    let mvbp = self.fourmvbp;
                    for i in 0..4 {
                        let dir = (i == 1 || i == 3) as usize;
                        let (mut dx, mut dy) = (0, 0);
                        if (mvbp >> (3 - i)) & 1 != 0 {
                            let r = self.get_mvdata_interlaced(gb, false);
                            dx = r.0;
                            dy = r.1;
                        }
                        let j = if i > 1 { 2 } else { 0 };
                        self.pred_mv_intfr(j, dx, dy, 2, self.range_x, self.range_y, dir);
                        self.mc_4mv_luma(j, dir, dir != 0);
                        self.mc_4mv_luma(j + 1, dir, dir != 0);
                    }
                    self.mc_4mv_chroma4(0, 0, false);
                    self.mc_4mv_chroma4(1, 1, true);
                } else if bmvtype == BMV_TYPE_INTERPOLATED {
                    let mvbp = self.twomvbp;
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 1, self.range_x, self.range_y, 0);
                    self.mc_1mv(0);
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 1, self.range_x, self.range_y, 1);
                    self.interp_mc();
                } else if twomv {
                    let dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let dir2 = if mvsw { 1 - dir } else { dir };
                    let mvbp = self.twomvbp;
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 2 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 2, self.range_x, self.range_y, dir);
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp & 1 != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(2, dx, dy, 2, self.range_x, self.range_y, dir2);
                    if mvsw {
                        self.mvsw_copy(dir, dir2);
                    } else {
                        self.pred_mv_intfr(0, 0, 0, 2, self.range_x, self.range_y, 1 - dir);
                        self.pred_mv_intfr(2, 0, 0, 2, self.range_x, self.range_y, 1 - dir);
                    }
                    self.mc_4mv_luma(0, dir, false);
                    self.mc_4mv_luma(1, dir, false);
                    self.mc_4mv_luma(2, dir2, false);
                    self.mc_4mv_luma(3, dir2, false);
                    self.mc_4mv_chroma4(dir, dir2, false);
                } else {
                    let dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                    let mvbp = mode[2];
                    let (mut dx, mut dy) = (0, 0);
                    if mvbp != 0 {
                        let r = self.get_mvdata_interlaced(gb, false);
                        dx = r.0;
                        dy = r.1;
                    }
                    self.pred_mv_intfr(0, dx, dy, 1, self.range_x, self.range_y, dir);
                    self.set_blk_mv_type4(1);
                    self.pred_mv_intfr(0, 0, 0, 2, self.range_x, self.range_y, 1 - dir);
                    self.copy_top_to_bottom(1 - dir);
                    self.mc_1mv(dir);
                }
                if cbp != 0 {
                    self.get_mquant(gb, &mut mquant);
                }
                self.set_qscale(mb_pos, mquant);
                if !self.ttmbf && cbp != 0 {
                    ttmb = VLCS.ttmb[self.tt_index].get(gb);
                }
                for i in 0..6 {
                    self.set_dc_val(i, 0);
                    let val = (cbp >> (5 - i)) & 1 != 0;
                    let ft = fieldtx as usize;
                    let (plane, dst, ls) = if i < 4 {
                        let off = if ft == 0 {
                            (i & 1) as isize * 8 + (i & 2) as isize * 4 * self.linesize as isize
                        } else {
                            (i & 1) as isize * 8 + (i > 1) as isize * self.linesize as isize
                        };
                        (0, self.dest[0] + off, self.linesize << ft)
                    } else {
                        (i - 3, self.dest[i - 3], self.uvlinesize)
                    };
                    if val {
                        let mut b = self.blocks[i];
                        let Some(pat) =
                            self.decode_p_block(gb, &mut b, i, mquant, ttmb, first_block, plane, dst, ls, Some(&mut block_tt))
                        else {
                            return false;
                        };
                        self.blocks[i] = b;
                        block_cbp |= (pat as u32) << (i << 2);
                        if !self.ttmbf && ttmb < 8 {
                            ttmb = -1;
                        }
                        first_block = false;
                    }
                }
            } else {
                let mut dir = 0usize;
                for i in 0..6 {
                    self.set_vmbt(self.block_index[i], 0);
                    self.set_dc_val(i, 0);
                }
                self.set_cur_mb_type(mb_pos as isize, MB_TYPE_SKIP);
                self.set_qscale(mb_pos, 0);
                self.set_blk_mv_type4(0);
                if !direct {
                    if bmvtype == BMV_TYPE_INTERPOLATED {
                        self.pred_mv_intfr(0, 0, 0, 1, self.range_x, self.range_y, 0);
                        self.pred_mv_intfr(0, 0, 0, 1, self.range_x, self.range_y, 1);
                    } else {
                        dir = (bmvtype == BMV_TYPE_BACKWARD) as usize;
                        self.pred_mv_intfr(0, 0, 0, 1, self.range_x, self.range_y, dir);
                        if mvsw {
                            let dir2 = 1 - dir;
                            self.mvsw_copy(dir, dir2);
                        } else {
                            self.set_blk_mv_type4(1);
                            self.pred_mv_intfr(0, 0, 0, 2, self.range_x, self.range_y, 1 - dir);
                            self.copy_top_to_bottom(1 - dir);
                        }
                    }
                }
                self.mc_1mv(dir);
                if direct || bmvtype == BMV_TYPE_INTERPOLATED {
                    self.interp_mc();
                }
                self.fieldtx_plane[mb_pos] = 0;
            }
        }
        let r = self.row3(self.mb_x as isize);
        self.cbp_base[r] = block_cbp;
        self.ttblk_base[r] = block_tt;
        true
    }

    // ───────────────────────── picture loops ─────────────────────────

    fn set_codingsets(&mut self, intra_from_y: bool) {
        let ysel = if intra_from_y { self.y_ac_table_index } else { self.c_ac_table_index };
        self.codingset = match ysel {
            0 => {
                if self.pqindex <= 8 {
                    CS_HIGH_RATE_INTRA
                } else {
                    CS_LOW_MOT_INTRA
                }
            }
            1 => CS_HIGH_MOT_INTRA,
            _ => CS_MID_RATE_INTRA,
        };
        self.codingset2 = match self.c_ac_table_index {
            0 => {
                if self.pqindex <= 8 {
                    CS_HIGH_RATE_INTER
                } else {
                    CS_LOW_MOT_INTER
                }
            }
            1 => CS_HIGH_MOT_INTER,
            _ => CS_MID_RATE_INTER,
        };
    }

    /// `vc1_decode_i_blocks` (Simple/Main).
    fn decode_i_blocks(&mut self, gb: &mut BitReader) {
        self.set_codingsets(true);
        self.y_dc_scale = WMV3_DC_SCALE_TABLE[(self.pq as usize) & 31] as i32;
        self.mb_x = 0;
        self.mb_y = 0;
        self.mb_intra = true;
        self.first_slice_line = true;
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            self.init_block_index();
            while self.mb_x < self.end_mb_x {
                self.update_block_index();
                self.clear_cur_blocks();
                let mb_pos = self.mb_x + self.mb_y * self.mb_width;
                self.set_cur_mb_type(mb_pos as isize, MB_TYPE_INTRA);
                self.set_qscale(mb_pos, self.pq);
                for i in 0..4 {
                    self.set_mv_at(1, self.block_index[i], [0, 0]);
                }
                let mut cbp = VLCS.msmp4_mb_i.get(gb);
                self.ac_pred = gb.read_bit() != 0;
                for k in 0..6 {
                    self.set_vmbt(self.block_index[k], 1);
                    let mut val = (cbp >> (5 - k)) & 1;
                    if k < 4 {
                        val = self.coded_block_pred(k, val);
                    }
                    cbp |= val << (5 - k);
                    let d = BlkDst::Ring(BLOCK_MAP[k]);
                    let mut b = self.load_blk(d);
                    let cs = if k < 4 { self.codingset } else { self.codingset2 };
                    // FFmpeg ignores the return value here.
                    let _ = self.decode_i_block(gb, &mut b, k, val != 0, cs);
                    self.inv_trans_8x8(&mut b);
                    self.store_blk(d, &b);
                }
                if self.overlap && self.pq >= 9 {
                    self.i_overlap_filter();
                    if self.rangeredfrm {
                        self.scale_cur_blocks(|v| v.wrapping_mul(2));
                    }
                    self.put_blocks_clamped(true);
                } else {
                    if self.rangeredfrm {
                        self.scale_cur_blocks(|v| v.wrapping_sub(64).wrapping_mul(2));
                    }
                    self.put_blocks_clamped(false);
                }
                if self.loop_filter {
                    self.i_loop_filter();
                }
                if gb.bits_left() < 0 {
                    return;
                }
                let n = (self.end_mb_x + 2) as isize;
                self.topleft_blk_idx = (self.topleft_blk_idx + 1) % n;
                self.top_blk_idx = (self.top_blk_idx + 1) % n;
                self.left_blk_idx = (self.left_blk_idx + 1) % n;
                self.cur_blk_idx = (self.cur_blk_idx + 1) % n;
                self.mb_x += 1;
            }
            self.first_slice_line = false;
            self.mb_y += 1;
        }
    }

    fn scale_cur_blocks(&mut self, f: impl Fn(i16) -> i16) {
        let o = self.blk_off(self.cur_blk_idx, 0);
        for v in &mut self.blk[o..o + 6 * 64] {
            *v = f(*v);
        }
    }

    /// `vc1_decode_i_blocks_adv`.
    fn decode_i_blocks_adv(&mut self, gb: &mut BitReader) {
        if gb.bits_left() <= 1 {
            return;
        }
        self.set_codingsets(true);
        self.mb_intra = true;
        self.first_slice_line = true;
        self.mb_x = 0;
        self.mb_y = self.start_mb_y;
        if self.start_mb_y != 0 {
            let start = (self.bo() + (2 * self.mb_y as isize - 1) * self.b8_stride as isize - 2) as usize;
            let end = (start + 1 + self.b8_stride).min(self.coded_block.len());
            self.coded_block[start..end].fill(0);
        }
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            self.init_block_index();
            while self.mb_x < self.mb_width {
                let mut mquant = self.pq;
                self.update_block_index();
                self.clear_cur_blocks();
                let mb_pos = self.mb_pos();
                self.set_cur_mb_type(mb_pos as isize + self.mb_off, MB_TYPE_INTRA);
                for i in 0..4 {
                    self.set_mv_at(1, self.block_index[i] + self.blocks_off, [0, 0]);
                }
                if self.fieldtx_is_raw {
                    self.fieldtx_plane[mb_pos] = gb.read_bit() as u8;
                }
                if gb.bits_left() <= 1 {
                    return;
                }
                let mut cbp = VLCS.msmp4_mb_i.get(gb);
                self.ac_pred =
                    if self.acpred_is_raw { gb.read_bit() != 0 } else { self.acpred_plane[mb_pos] != 0 };
                if self.condover == CONDOVER_SELECT && self.overflg_is_raw {
                    self.over_flags_plane[mb_pos] = gb.read_bit() as u8;
                }
                self.get_mquant(gb, &mut mquant);
                self.set_qscale(mb_pos, mquant);
                self.y_dc_scale = WMV3_DC_SCALE_TABLE[(mquant.unsigned_abs() as usize) & 31] as i32;
                for k in 0..6 {
                    self.set_vmbt(self.block_index[k], 1);
                    let mut val = (cbp >> (5 - k)) & 1;
                    if k < 4 {
                        val = self.coded_block_pred(k, val);
                    }
                    cbp |= val << (5 - k);
                    self.a_avail = !self.first_slice_line || k == 2 || k == 3;
                    self.c_avail = self.mb_x != 0 || k == 1 || k == 3;
                    let d = BlkDst::Ring(BLOCK_MAP[k]);
                    let mut b = self.load_blk(d);
                    let cs = if k < 4 { self.codingset } else { self.codingset2 };
                    let _ = self.decode_i_block_adv(gb, &mut b, k, val != 0, cs, mquant);
                    self.inv_trans_8x8(&mut b);
                    self.store_blk(d, &b);
                }
                if self.overlap && (self.pq >= 9 || self.condover != CONDOVER_NONE) {
                    self.i_overlap_filter();
                }
                self.put_blocks_clamped(true);
                if self.loop_filter {
                    self.i_loop_filter();
                }
                if gb.bits_left() < 0 {
                    return;
                }
                self.inc_blk_idx();
                self.mb_x += 1;
            }
            self.first_slice_line = false;
            self.mb_y += 1;
        }
    }

    /// Shifts the cbp/ttblk/is_intra/luma_mv row history by one MB row.
    fn shift_rows(&mut self, with_luma_mv: bool) {
        let ms = self.mb_stride;
        self.cbp_base.copy_within(ms..3 * ms, 0);
        self.ttblk_base.copy_within(ms..3 * ms, 0);
        self.is_intra_base.copy_within(ms..3 * ms, 0);
        if with_luma_mv {
            self.luma_mv_base.copy_within(ms..3 * ms, 0);
        }
    }

    /// `vc1_decode_p_blocks`.
    fn decode_p_blocks(&mut self, gb: &mut BitReader) {
        self.set_codingsets(false);
        let apply_loop_filter = self.loop_filter;
        self.first_slice_line = true;
        self.cbp_base.fill(0);
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            self.init_block_index();
            while self.mb_x < self.mb_width {
                self.update_block_index();
                if (self.fcm == ILACE_FIELD || (self.fcm == PROGRESSIVE && self.mv_type_is_raw) || self.skip_is_raw)
                    && gb.bits_left() < 1
                {
                    return;
                }
                let ok = if self.fcm == ILACE_FIELD {
                    let ok = self.decode_p_mb_intfi(gb);
                    if apply_loop_filter {
                        self.p_loop_filter();
                    }
                    ok
                } else if self.fcm == ILACE_FRAME {
                    let ok = self.decode_p_mb_intfr(gb);
                    if apply_loop_filter {
                        self.p_intfr_loop_filter();
                    }
                    ok
                } else {
                    let ok = self.decode_p_mb(gb);
                    if apply_loop_filter {
                        self.p_loop_filter();
                    }
                    ok
                };
                if !ok || gb.bits_left() < 0 {
                    return;
                }
                self.inc_blk_idx();
                self.mb_x += 1;
            }
            self.shift_rows(true);
            self.first_slice_line = false;
            self.mb_y += 1;
        }
    }

    /// `vc1_decode_b_blocks`.
    fn decode_b_blocks(&mut self, gb: &mut BitReader) {
        self.set_codingsets(false);
        self.first_slice_line = true;
        self.mb_y = self.start_mb_y;
        while self.mb_y < self.end_mb_y {
            self.mb_x = 0;
            self.init_block_index();
            while self.mb_x < self.mb_width {
                self.update_block_index();
                if (self.fcm == ILACE_FIELD || self.skip_is_raw || self.dmb_is_raw) && gb.bits_left() < 1 {
                    return;
                }
                if self.fcm == ILACE_FIELD {
                    self.decode_b_mb_intfi(gb);
                    if self.loop_filter {
                        self.b_intfi_loop_filter();
                    }
                } else if self.fcm == ILACE_FRAME {
                    self.decode_b_mb_intfr(gb);
                    if self.loop_filter {
                        self.p_intfr_loop_filter();
                    }
                } else {
                    self.decode_b_mb(gb);
                    if self.loop_filter {
                        self.i_loop_filter();
                    }
                }
                if gb.bits_left() < 0 {
                    return;
                }
                self.mb_x += 1;
            }
            self.shift_rows(false);
            self.first_slice_line = false;
            self.mb_y += 1;
        }
    }

    /// `vc1_decode_skip_blocks`.
    fn decode_skip_blocks(&mut self) {
        let Some(last) = self.last.as_ref() else { return };
        let Some(cur) = self.cur.as_mut() else { return };
        self.first_slice_line = true;
        let ls = self.linesize;
        let uvls = self.uvlinesize;
        for mb_y in self.start_mb_y..self.end_mb_y {
            // dest after init/update_block_index at mb_x = 0
            let mut d0 = (mb_y * ls * 16) as isize;
            let mut d1 = (mb_y * uvls * 8) as isize;
            if self.field_mode && !(self.second_field ^ self.tff) {
                d0 += (self.mb_width * 16) as isize;
                d1 += (self.mb_width * 8) as isize;
            }
            let copy = |dst: &mut Vec<u8>, src: &Vec<u8>, d: isize, s: usize, n: usize| {
                let d = d as usize;
                if d + n <= dst.len() && s + n <= src.len() {
                    dst[d..d + n].copy_from_slice(&src[s..s + n]);
                }
            };
            copy(&mut cur.pic.data[0], &last.pic.data[0], d0, mb_y * 16 * ls, ls * 16);
            copy(&mut cur.pic.data[1], &last.pic.data[1], d1, mb_y * 8 * uvls, uvls * 8);
            copy(&mut cur.pic.data[2], &last.pic.data[2], d1, mb_y * 8 * uvls, uvls * 8);
            self.first_slice_line = false;
        }
    }

    /// `ff_vc1_decode_blocks`.
    pub(crate) fn decode_blocks(&mut self, gb: &mut BitReader) {
        self.esc3_level_length = 0;
        if self.x8_type {
            let dquant = 2 * self.pq + self.halfpq;
            let qoff = self.pq * (!self.pquantizer) as i32;
            let loop_filter = self.loop_filter;
            if let (Some(x8), Some(cur)) = (self.x8.as_mut(), self.cur.as_mut()) {
                x8.decode_picture(&mut cur.pic, gb, dquant, qoff, loop_filter, &mut self.qscale_table);
            }
            return;
        }
        self.cur_blk_idx = 0;
        self.left_blk_idx = -1;
        self.topleft_blk_idx = 1;
        self.top_blk_idx = 2;
        match self.pict_type {
            PICT_I => {
                if self.profile == PROFILE_ADVANCED {
                    self.decode_i_blocks_adv(gb);
                } else {
                    self.decode_i_blocks(gb);
                }
            }
            PICT_P => {
                if self.p_frame_skipped {
                    self.decode_skip_blocks();
                } else {
                    self.decode_p_blocks(gb);
                }
            }
            _ => {
                if self.bi_type {
                    if self.profile == PROFILE_ADVANCED {
                        self.decode_i_blocks_adv(gb);
                    } else {
                        self.decode_i_blocks(gb);
                    }
                } else {
                    self.decode_b_blocks(gb);
                }
            }
        }
    }
}

//! H.263 macroblock layer as RV10/RV20 use it: macroblock headers, motion
//! vector prediction and decoding, coefficient blocks with advanced intra
//! coding, B-frame direct mode, OBMC look-ahead and the in-loop filter.
//!
//! Ported from FFmpeg libavcodec/ituh263dec.c (`ff_h263_decode_mb`,
//! `h263_decode_block`, `h263_pred_acdc`, `ff_h263_decode_motion`,
//! `h263_decode_dquant`, `set_direct_mv`, `preview_obmc`), h263.c
//! (`ff_h263_pred_motion`, `ff_h263_update_motion_val`,
//! `ff_h263_loop_filter`) and rv10.c (`ff_rv_decode_dc`) at commit 2da55bf;
//! LGPL-2.1-or-later.

use super::dsp::{h263_h_loop_filter, h263_v_loop_filter};
use super::tables::{ALTERNATE_HORIZONTAL_SCAN, ALTERNATE_VERTICAL_SCAN, MODIFIED_QUANT_TAB, ZIGZAG_DIRECT};
use super::*;
use crate::bits::{mid_pred, sign_extend};

impl Rv1020Decoder {
    #[inline]
    fn mv_idx(&self, b8: isize) -> usize {
        (MV_BASE as isize + b8) as usize
    }

    #[inline]
    pub(crate) fn cur_mv(&self, dir: usize, b8: isize) -> [i32; 2] {
        let v = self.cur.as_ref().unwrap().mv[dir][self.mv_idx(b8)];
        [v[0] as i32, v[1] as i32]
    }

    #[inline]
    fn set_cur_mv(&mut self, dir: usize, b8: isize, v: [i32; 2]) {
        let idx = self.mv_idx(b8);
        self.cur.as_mut().unwrap().mv[dir][idx] = [v[0] as i16, v[1] as i16];
    }

    /// Sets the four vectors of the macroblock whose top-left b8 index is `b8`.
    fn set_mb_mv(&mut self, dir: usize, b8: isize, v: [i32; 2]) {
        let wrap = self.g.b8_stride as isize;
        for p in [b8, b8 + 1, b8 + wrap, b8 + wrap + 1] {
            self.set_cur_mv(dir, p, v);
        }
    }

    fn set_cur_mb_type(&mut self, xy: usize, t: u32) {
        self.cur.as_mut().unwrap().mb_type[xy] = t;
    }

    /// `ff_h263_pred_motion`: the predictor and the b8 index of `block`.
    /// Like FFmpeg it zeroes the left neighbour's vector of block 2 at the
    /// start of a slice line (it writes through its `A` pointer).
    pub(crate) fn pred_motion(&mut self, block: usize, dir: usize) -> (i32, i32, isize) {
        const OFF: [isize; 4] = [2, 1, 1, -1];
        let wrap = self.g.b8_stride as isize;
        let xy = self.block_index[block];
        let mut a = self.cur_mv(dir, xy - 1);
        let (px, py);
        if self.first_slice_line && block < 3 {
            // h263_pred is 0 for RealVideo: no "mb_x + 1 == resync_mb_x" case.
            if block == 0 {
                if self.mb_x == self.resync_mb_x {
                    px = 0;
                    py = 0;
                } else {
                    px = a[0];
                    py = a[1];
                }
            } else if block == 1 {
                px = a[0];
                py = a[1];
            } else {
                let b = self.cur_mv(dir, xy - wrap);
                let c = self.cur_mv(dir, xy + OFF[block] - wrap);
                if self.mb_x == self.resync_mb_x {
                    a = [0, 0];
                    self.set_cur_mv(dir, xy - 1, a);
                }
                px = mid_pred(a[0], b[0], c[0]);
                py = mid_pred(a[1], b[1], c[1]);
            }
        } else {
            let b = self.cur_mv(dir, xy - wrap);
            let c = self.cur_mv(dir, xy + OFF[block] - wrap);
            px = mid_pred(a[0], b[0], c[0]);
            py = mid_pred(a[1], b[1], c[1]);
        }
        (px, py, xy)
    }

    /// `ff_h263_decode_motion` (f_code 1, no UMV): `0xffff` on error.
    fn decode_motion(&self, gb: &mut BitReader, pred: i32) -> i32 {
        let code = gb.get_vlc2(&tables().mv.table, H263_MV_VLC_BITS, 2);
        if code == 0 {
            return pred;
        }
        if code < 0 {
            return 0xffff;
        }
        let sign = gb.get_bits1();
        let mut val = code;
        if sign != 0 {
            val = -val;
        }
        val += pred;
        if !self.h263_long_vectors {
            val = sign_extend(val, 5 + 1);
        } else {
            // Horrible H.263 long vector mode.
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
    fn decode_dquant(&mut self, gb: &mut BitReader) {
        const QUANT_TAB: [i32; 4] = [-1, -2, 1, 2];
        let qscale = if self.modified_quant {
            if gb.get_bits1() != 0 {
                MODIFIED_QUANT_TAB[gb.get_bits1() as usize][self.qscale as usize] as i32
            } else {
                gb.get_bits(5) as i32
            }
        } else {
            self.qscale + QUANT_TAB[gb.get_bits(2) as usize]
        };
        self.set_qscale(qscale);
    }

    /// `h263_pred_acdc`: AC/DC prediction of advanced intra coding.
    fn pred_acdc(&mut self, n: usize) {
        let base = self.dc_base() as isize;
        let xy = self.block_index[n];
        let (wrap, scale) = if n < 4 { (self.g.b8_stride as isize, self.y_dc_scale) } else { (self.g.mb_stride as isize, self.c_dc_scale) };
        let at = |i: isize| (base + i) as usize;
        let mut a = self.dc_val[at(xy - 1)] as i32;
        let mut c = self.dc_val[at(xy - wrap)] as i32;
        // No prediction outside the slice's first line.
        if self.first_slice_line && n != 3 {
            if n != 2 {
                c = 1024;
            }
            if n != 1 && self.mb_x == self.resync_mb_x {
                a = 1024;
            }
        }
        let block = &mut self.block[n];
        let pred_dc;
        if self.ac_pred {
            let mut p = 1024;
            if self.h263_aic_dir {
                // Left prediction.
                if a != 1024 {
                    let ac2 = &self.ac_val[at(xy - 1)];
                    for i in 1..8 {
                        block[i << 3] = block[i << 3].wrapping_add(ac2[i]);
                    }
                    p = a;
                }
            } else if c != 1024 {
                // Top prediction.
                let ac2 = &self.ac_val[at(xy - wrap)];
                for i in 1..8 {
                    block[i] = block[i].wrapping_add(ac2[i + 8]);
                }
                p = c;
            }
            pred_dc = p;
        } else if a != 1024 && c != 1024 {
            pred_dc = (a + c) >> 1;
        } else if a != 1024 {
            pred_dc = a;
        } else {
            pred_dc = c;
        }
        // The prediction is assumed positive.
        block[0] = (block[0] as i32 * scale + pred_dc) as i16;
        if block[0] < 0 {
            block[0] = 0;
        } else {
            block[0] |= 1;
        }
        self.dc_val[at(xy)] = block[0];
        let acv = &mut self.ac_val[at(xy)];
        for i in 1..8 {
            acv[i] = block[i << 3];
        }
        for i in 1..8 {
            acv[8 + i] = block[i];
        }
    }

    /// `ff_rv_decode_dc`.
    fn rv_decode_dc(&self, gb: &mut BitReader, n: usize) -> i32 {
        let t = tables();
        if n < 4 {
            gb.get_vlc2(&t.rv_dc_lum.table, DC_VLC_BITS, 2)
        } else {
            let code = gb.get_vlc2(&t.rv_dc_chrom.table, DC_VLC_BITS, 2);
            if code < 0 { -1 } else { code }
        }
    }

    /// `h263_decode_block`: 0 on success, -1 on error.
    fn decode_block(&mut self, gb: &mut BitReader, n: usize, coded: bool) -> i32 {
        let t = tables();
        let mut rl = &t.rl_inter;
        let mut scan: &[u8; 64] = &ZIGZAG_DIRECT;
        let mut i: i32;
        if self.h263_aic && self.mb_intra {
            if !coded {
                // not_coded
                self.pred_acdc(n);
                self.block_last_index[n] = 0;
                return 0;
            }
            rl = &t.rl_intra_aic;
            if self.ac_pred {
                scan = if self.h263_aic_dir { &ALTERNATE_VERTICAL_SCAN } else { &ALTERNATE_HORIZONTAL_SCAN };
            }
            i = 0;
        } else if self.mb_intra {
            // DC coefficient.
            let level;
            if !self.rv20 {
                if self.rv10_version == 3 && self.pict_type == PICT_I {
                    let component = if n <= 3 { 0 } else { n - 4 + 1 };
                    let mut l = self.last_dc[component];
                    if self.rv10_first_dc_coded[component] {
                        let diff = self.rv_decode_dc(gb, n);
                        if diff < 0 {
                            return -1;
                        }
                        l += diff;
                        l &= 0xff; // handle wrap round
                        self.last_dc[component] = l;
                    } else {
                        self.rv10_first_dc_coded[component] = true;
                    }
                    level = l;
                } else {
                    let l = gb.get_bits(8) as i32;
                    level = if l == 255 { 128 } else { l };
                }
            } else {
                // A zero `level & 0x7F` is an "illegal dc" FFmpeg only logs
                // unless strict error recognition is requested.
                let l = gb.get_bits(8) as i32;
                level = if l == 255 { 128 } else { l };
            }
            self.block[n][0] = level as i16;
            i = 1;
        } else {
            i = 0;
        }
        if !coded {
            self.block_last_index[n] = i - 1;
            return 0;
        }

        i -= 1; // offset by -1 to allow direct indexing of the scan table
        loop {
            let (mut level, mut run) = gb.get_rl_vlc(rl, TEX_VLC_BITS, 2);
            if run == 66 {
                if level != 0 {
                    // Illegal AC VLC code.
                    return -1;
                }
                // Escape.
                run = gb.get_bits(7) as i32 + 1;
                level = gb.get_bits(8) as u8 as i8 as i32;
                if level == -128 {
                    if !self.rv20 {
                        level = gb.get_sbits(12);
                    } else {
                        level = gb.get_bits(5) as i32;
                        level |= gb.get_sbits(6) * (1 << 5);
                    }
                }
            } else if gb.get_bits1() != 0 {
                level = -level;
            }
            i += run;
            if i >= 64 {
                // Redo the update without the last flag.
                i = i - run + ((run - 1) & 63) + 1;
                if i < 64 {
                    // Only the last marker, no overrun.
                    self.block[n][scan[i as usize] as usize] = level as i16;
                    break;
                }
                // Run overflow.
                return -1;
            }
            self.block[n][scan[i as usize] as usize] = level as i16;
        }
        if self.mb_intra && self.h263_aic {
            self.pred_acdc(n);
        }
        self.block_last_index[n] = i;
        0
    }

    /// `set_direct_mv` (ituh263dec.c): returns the macroblock type bits.
    fn set_direct_mv(&mut self) -> u32 {
        let mb_index = self.mb_x + self.mb_y * self.g.mb_stride;
        let Some(p) = self.next.as_ref().map(Arc::clone) else { return MB_TYPE_DIRECT2 | MB_TYPE_16X16 | MB_TYPE_BIDIR_MV };
        let colocated = p.mb_type[mb_index];
        let set_one = |s: &mut Self, i: usize| {
            let xy = s.mv_idx(s.block_index[i]);
            let time_pp = s.pp_time as i32;
            let time_pb = s.pb_time as i32;
            for k in 0..2 {
                let pv = p.mv[0][xy][k] as i32;
                if ((pv + 32) as u32) < 64 {
                    s.mv[0][i][k] = s.direct_scale_mv[0][(pv + 32) as usize] as i32;
                    s.mv[1][i][k] = s.direct_scale_mv[1][(pv + 32) as usize] as i32;
                } else {
                    s.mv[0][i][k] = pv * time_pb / time_pp;
                    s.mv[1][i][k] = pv * (time_pb - time_pp) / time_pp;
                }
            }
        };
        if colocated & MB_TYPE_8X8 != 0 {
            self.mv_type = MV_TYPE_8X8;
            for i in 0..4 {
                set_one(self, i);
            }
            MB_TYPE_DIRECT2 | MB_TYPE_8X8 | MB_TYPE_BIDIR_MV
        } else {
            set_one(self, 0);
            for i in 1..4 {
                self.mv[0][i] = self.mv[0][0];
                self.mv[1][i] = self.mv[1][0];
            }
            self.mv_type = MV_TYPE_8X8;
            MB_TYPE_DIRECT2 | MB_TYPE_16X16 | MB_TYPE_BIDIR_MV
        }
    }

    /// `preview_obmc`: decodes the next macroblock's vectors ahead of time
    /// for overlapped block motion compensation, then rewinds.
    fn preview_obmc(&mut self, gb: &mut BitReader) {
        let saved = gb.clone();
        let t = tables();
        let xy = self.mb_x + 1 + self.mb_y * self.g.mb_stride;
        for i in 0..4 {
            self.block_index[i] += 2;
        }
        self.block_index[4] += 1;
        self.block_index[5] += 1;
        self.mb_x += 1;

        let mut skipped = false;
        let mut cbpc;
        loop {
            if gb.get_bits1() != 0 {
                // Skipped macroblock.
                let b0 = self.block_index[0];
                self.set_mb_mv(0, b0, [0, 0]);
                self.set_cur_mb_type(xy, MB_TYPE_SKIP | MB_TYPE_16X16 | MB_TYPE_FORWARD_MV);
                skipped = true;
                cbpc = 0;
                break;
            }
            cbpc = gb.get_vlc2(&t.inter_mcbpc.table, INTER_MCBPC_VLC_BITS, 2);
            if cbpc != 20 {
                break;
            }
        }
        if !skipped {
            if cbpc & 4 != 0 {
                self.set_cur_mb_type(xy, MB_TYPE_INTRA);
            } else {
                let _ = gb.get_vlc2(&t.cbpy.table, CBPY_VLC_BITS, 1);
                if cbpc & 8 != 0 {
                    let n = if self.modified_quant {
                        if gb.get_bits1() != 0 { 1 } else { 5 }
                    } else {
                        2
                    };
                    gb.skip_bits(n);
                }
                if cbpc & 16 == 0 {
                    self.set_cur_mb_type(xy, MB_TYPE_16X16 | MB_TYPE_FORWARD_MV);
                    let (px, py, idx) = self.pred_motion(0, 0);
                    let mx = self.decode_motion(gb, px);
                    let my = self.decode_motion(gb, py);
                    self.set_mb_mv(0, idx, [mx, my]);
                } else {
                    self.set_cur_mb_type(xy, MB_TYPE_8X8 | MB_TYPE_FORWARD_MV);
                    for i in 0..4 {
                        let (px, py, idx) = self.pred_motion(i, 0);
                        let mx = self.decode_motion(gb, px);
                        let my = self.decode_motion(gb, py);
                        self.set_cur_mv(0, idx, [mx, my]);
                    }
                }
            }
        }

        for i in 0..4 {
            self.block_index[i] -= 2;
        }
        self.block_index[4] -= 1;
        self.block_index[5] -= 1;
        self.mb_x -= 1;
        *gb = saved;
    }

    /// `ff_h263_decode_mb`: returns `SLICE_OK`, `SLICE_END` or `SLICE_ERROR`.
    pub(crate) fn decode_mb(&mut self, gb: &mut BitReader) -> i32 {
        let t = tables();
        let xy = self.mb_x + self.mb_y * self.g.mb_stride;
        let cbpc: i32;
        let dquant: bool;
        let mut cbp: i32;

        enum Next {
            Blocks,
            Intra,
            End,
        }

        let next = if self.pict_type == PICT_P {
            let mut c;
            let mut skip = false;
            loop {
                if gb.get_bits1() != 0 {
                    // Skipped macroblock.
                    self.mb_intra = false;
                    self.block_last_index = [-1; 6];
                    self.mv_dir = MV_DIR_FORWARD;
                    self.mv_type = MV_TYPE_16X16;
                    self.set_cur_mb_type(xy, MB_TYPE_SKIP | MB_TYPE_16X16 | MB_TYPE_FORWARD_MV);
                    self.mv[0][0] = [0, 0];
                    self.mb_skipped = !(self.obmc | self.loop_filter);
                    skip = true;
                    c = 0;
                    break;
                }
                c = gb.get_vlc2(&t.inter_mcbpc.table, INTER_MCBPC_VLC_BITS, 2);
                if c < 0 {
                    return SLICE_ERROR;
                }
                if c != 20 {
                    break;
                }
            }
            cbpc = c;
            if skip {
                dquant = false;
                cbp = 0;
                Next::End
            } else {
                self.block = [[0; 64]; 6];
                dquant = cbpc & 8 != 0;
                self.mb_intra = cbpc & 4 != 0;
                if self.mb_intra {
                    cbp = 0;
                    Next::Intra
                } else {
                    let cbpy = gb.get_vlc2(&t.cbpy.table, CBPY_VLC_BITS, 1);
                    if cbpy < 0 {
                        return SLICE_ERROR;
                    }
                    let cbpy = cbpy ^ 0xF;
                    cbp = (cbpc & 3) | (cbpy << 2);
                    if dquant {
                        self.decode_dquant(gb);
                    }
                    self.mv_dir = MV_DIR_FORWARD;
                    if cbpc & 16 == 0 {
                        self.set_cur_mb_type(xy, MB_TYPE_16X16 | MB_TYPE_FORWARD_MV);
                        self.mv_type = MV_TYPE_16X16;
                        let (px, py, _) = self.pred_motion(0, 0);
                        let mx = self.decode_motion(gb, px);
                        if mx >= 0xffff {
                            return SLICE_ERROR;
                        }
                        let my = self.decode_motion(gb, py);
                        if my >= 0xffff {
                            return SLICE_ERROR;
                        }
                        self.mv[0][0] = [mx, my];
                    } else {
                        self.set_cur_mb_type(xy, MB_TYPE_8X8 | MB_TYPE_FORWARD_MV);
                        self.mv_type = MV_TYPE_8X8;
                        for i in 0..4 {
                            let (px, py, idx) = self.pred_motion(i, 0);
                            let mx = self.decode_motion(gb, px);
                            if mx >= 0xffff {
                                return SLICE_ERROR;
                            }
                            let my = self.decode_motion(gb, py);
                            if my >= 0xffff {
                                return SLICE_ERROR;
                            }
                            self.mv[0][i] = [mx, my];
                            self.set_cur_mv(0, idx, [mx, my]);
                        }
                    }
                    Next::Blocks
                }
            }
        } else if self.pict_type == PICT_B {
            let b8 = 2 * (self.mb_x + self.mb_y * self.g.b8_stride) as isize;
            self.set_mb_mv(0, b8, [0, 0]);
            self.set_mb_mv(1, b8, [0, 0]);
            let mut mb_type;
            loop {
                mb_type = gb.get_vlc2(&t.mbtype_b.table, H263_MBTYPE_B_VLC_BITS, 2);
                if mb_type < 0 {
                    return SLICE_ERROR;
                }
                if mb_type != 0 {
                    break;
                }
            }
            let mut mb_type = mb_type as u32;
            self.mb_intra = is_intra(mb_type);
            if mb_type & MB_TYPE_CBP != 0 {
                self.block = [[0; 64]; 6];
                cbpc = gb.get_vlc2(&t.cbpc_b.table, CBPC_B_VLC_BITS, 1);
                if self.mb_intra {
                    dquant = mb_type & MB_TYPE_QUANT != 0;
                    cbp = 0;
                    Next::Intra
                } else {
                    let cbpy = gb.get_vlc2(&t.cbpy.table, CBPY_VLC_BITS, 1);
                    if cbpy < 0 {
                        return SLICE_ERROR;
                    }
                    let cbpy = cbpy ^ 0xF;
                    cbp = (cbpc & 3) | (cbpy << 2);
                    dquant = false;
                    if let Err(e) = self.b_vectors(gb, xy, &mut mb_type) {
                        return e;
                    }
                    Next::Blocks
                }
            } else {
                cbpc = 0;
                cbp = 0;
                dquant = false;
                if let Err(e) = self.b_vectors(gb, xy, &mut mb_type) {
                    return e;
                }
                Next::Blocks
            }
        } else {
            // I picture.
            let mut c;
            loop {
                c = gb.get_vlc2(&t.intra_mcbpc.table, INTRA_MCBPC_VLC_BITS, 2);
                if c < 0 {
                    return SLICE_ERROR;
                }
                if c != 8 {
                    break;
                }
            }
            cbpc = c;
            self.block = [[0; 64]; 6];
            dquant = cbpc & 4 != 0;
            self.mb_intra = true;
            cbp = 0;
            Next::Intra
        };

        let decode_blocks = match next {
            Next::End => false,
            Next::Blocks => true,
            Next::Intra => {
                self.set_cur_mb_type(xy, MB_TYPE_INTRA);
                if self.h263_aic {
                    self.ac_pred = gb.get_bits1() != 0;
                    if self.ac_pred {
                        self.set_cur_mb_type(xy, MB_TYPE_INTRA | MB_TYPE_ACPRED);
                        self.h263_aic_dir = gb.get_bits1() != 0;
                    }
                } else {
                    self.ac_pred = false;
                }
                let cbpy = gb.get_vlc2(&t.cbpy.table, CBPY_VLC_BITS, 1);
                if cbpy < 0 {
                    return SLICE_ERROR;
                }
                cbp = (cbpc & 3) | (cbpy << 2);
                if dquant {
                    self.decode_dquant(gb);
                }
                true
            }
        };

        if decode_blocks {
            for i in 0..6 {
                if self.decode_block(gb, i, cbp & 32 != 0) < 0 {
                    return SLICE_ERROR;
                }
                cbp += cbp;
            }
            if self.obmc && !self.mb_intra && self.pict_type == PICT_P && self.mb_x + 1 < self.g.mb_width && self.mb_num_left != 1 {
                self.preview_obmc(gb);
            }
        }

        // end:
        if gb.bits_left() < 0 {
            return SLICE_ERROR;
        }
        // Per-macroblock end of slice check.
        let mut v = gb.show_bits(16);
        if gb.bits_left() < 16 {
            v >>= 16 - gb.bits_left();
        }
        if v == 0 {
            return SLICE_END;
        }
        SLICE_OK
    }

    /// The B-frame vector part of `ff_h263_decode_mb` (after cbp).
    fn b_vectors(&mut self, gb: &mut BitReader, xy: usize, mb_type: &mut u32) -> std::result::Result<(), i32> {
        if *mb_type & MB_TYPE_QUANT != 0 {
            self.decode_dquant(gb);
        }
        if *mb_type & MB_TYPE_DIRECT2 != 0 {
            self.mv_dir = MV_DIR_FORWARD | MV_DIR_BACKWARD | MV_DIRECT;
            *mb_type |= self.set_direct_mv();
        } else {
            self.mv_dir = 0;
            self.mv_type = MV_TYPE_16X16;
            if *mb_type & MB_TYPE_FORWARD_MV != 0 {
                let (px, py, idx) = self.pred_motion(0, 0);
                self.mv_dir = MV_DIR_FORWARD;
                let mx = self.decode_motion(gb, px);
                if mx >= 0xffff {
                    return Err(SLICE_ERROR);
                }
                let my = self.decode_motion(gb, py);
                if my >= 0xffff {
                    return Err(SLICE_ERROR);
                }
                self.mv[0][0] = [mx, my];
                self.set_mb_mv(0, idx, [mx, my]);
            }
            if *mb_type & MB_TYPE_BACKWARD_MV != 0 {
                let (px, py, idx) = self.pred_motion(0, 1);
                self.mv_dir |= MV_DIR_BACKWARD;
                let mx = self.decode_motion(gb, px);
                if mx >= 0xffff {
                    return Err(SLICE_ERROR);
                }
                let my = self.decode_motion(gb, py);
                if my >= 0xffff {
                    return Err(SLICE_ERROR);
                }
                self.mv[1][0] = [mx, my];
                self.set_mb_mv(1, idx, [mx, my]);
            }
        }
        self.set_cur_mb_type(xy, *mb_type);
        Ok(())
    }

    /// `ff_h263_update_motion_val` (frame pictures).
    pub(crate) fn update_motion_val(&mut self) {
        if self.mv_type != MV_TYPE_8X8 {
            let v = if self.mb_intra { [0, 0] } else { self.mv[0][0] };
            let xy = self.block_index[0];
            self.set_mb_mv(0, xy, v);
        }
    }

    /// `ff_h263_loop_filter` for the current macroblock.
    pub(crate) fn h263_loop_filter(&mut self) {
        let g = self.g;
        let xy = self.mb_y * g.mb_stride + self.mb_x;
        let (mb_x, mb_y) = (self.mb_x, self.mb_y);
        let qscale = self.qscale;
        let cqt = self.chroma_qscale_table;
        let cur = self.cur.as_mut().unwrap();
        let skip = |t: u32| t & MB_TYPE_SKIP != 0;
        let [py, pu, pv] = &mut cur.planes;
        let ls = py.stride;
        let uvls = pu.stride;
        let dest_y = py.idx(mb_x * 16, mb_y * 16);
        let dest_cb = pu.idx(mb_x * 8, mb_y * 8);
        let dest_cr = pv.idx(mb_x * 8, mb_y * 8);

        let qp_c;
        if !skip(cur.mb_type[xy]) {
            qp_c = qscale;
            h263_v_loop_filter(&mut py.data, dest_y + 8 * ls, ls, qp_c);
            h263_v_loop_filter(&mut py.data, dest_y + 8 * ls + 8, ls, qp_c);
        } else {
            qp_c = 0;
        }

        if mb_y != 0 {
            let qp_tt = if skip(cur.mb_type[xy - g.mb_stride]) { 0 } else { cur.qscale[xy - g.mb_stride] as i32 };
            let qp_tc = if qp_c != 0 { qp_c } else { qp_tt };
            if qp_tc != 0 {
                let chroma_qp = cqt[(qp_tc & 31) as usize] as i32;
                h263_v_loop_filter(&mut py.data, dest_y, ls, qp_tc);
                h263_v_loop_filter(&mut py.data, dest_y + 8, ls, qp_tc);
                h263_v_loop_filter(&mut pu.data, dest_cb, uvls, chroma_qp);
                h263_v_loop_filter(&mut pv.data, dest_cr, uvls, chroma_qp);
            }
            if qp_tt != 0 {
                h263_h_loop_filter(&mut py.data, dest_y - 8 * ls + 8, ls, qp_tt);
            }
            if mb_x != 0 {
                let qp_dt = if qp_tt != 0 || skip(cur.mb_type[xy - 1 - g.mb_stride]) {
                    qp_tt
                } else {
                    cur.qscale[xy - 1 - g.mb_stride] as i32
                };
                if qp_dt != 0 {
                    let chroma_qp = cqt[(qp_dt & 31) as usize] as i32;
                    h263_h_loop_filter(&mut py.data, dest_y - 8 * ls, ls, qp_dt);
                    h263_h_loop_filter(&mut pu.data, dest_cb - 8 * uvls, uvls, chroma_qp);
                    h263_h_loop_filter(&mut pv.data, dest_cr - 8 * uvls, uvls, chroma_qp);
                }
            }
        }

        if qp_c != 0 {
            h263_h_loop_filter(&mut py.data, dest_y + 8, ls, qp_c);
            if mb_y + 1 == g.mb_height {
                h263_h_loop_filter(&mut py.data, dest_y + 8 * ls + 8, ls, qp_c);
            }
        }

        if mb_x != 0 {
            let qp_lc = if qp_c != 0 || skip(cur.mb_type[xy - 1]) { qp_c } else { cur.qscale[xy - 1] as i32 };
            if qp_lc != 0 {
                h263_h_loop_filter(&mut py.data, dest_y, ls, qp_lc);
                if mb_y + 1 == g.mb_height {
                    let chroma_qp = cqt[(qp_lc & 31) as usize] as i32;
                    h263_h_loop_filter(&mut py.data, dest_y + 8 * ls, ls, qp_lc);
                    h263_h_loop_filter(&mut pu.data, dest_cb, uvls, chroma_qp);
                    h263_h_loop_filter(&mut pv.data, dest_cr, uvls, chroma_qp);
                }
            }
        }
    }
}

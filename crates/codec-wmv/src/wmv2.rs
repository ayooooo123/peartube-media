//! WMV2 video decoder ported from libavcodec/wmv2dec.c, wmv2.c, wmv2dsp.c.

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};
use crate::bits::BitReader;
use crate::idct;
use crate::msmpeg4::{self, Picture, PredContext, MsVersion, make_frame, y_dc_scale, c_dc_scale, rl_decode_loop, MB_I_VLC, DC_VLC, RL_TABLES, INTRA_SCAN, INTRA_H_SCAN, INTRA_V_SCAN, DC_MAX};
use crate::tables::*;

pub const CODEC_ID_WMV2: &str = "wmv2";

pub struct Wmv2Decoder {
    codec_id: CodecId,
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    pred: PredContext,
    qscale_table: Vec<i32>,
    pending: Option<Frame>,
    last_picture: Option<Picture>,
    // extradata flags:
    fps: u32,
    bit_rate: u32,
    mspel_bit: bool,
    loop_filter: bool,
    abt_flag: bool,
    j_type_bit: bool,
    top_left_mv_flag: bool,
    per_mb_rl_bit: bool,
    slice_height: usize,
    // per-frame state:
    pict_type: u8,
    qscale: usize,
    j_type: bool,
    rl_table_index: usize,
    rl_chroma_table_index: usize,
    dc_table_index: usize,
    per_mb_rl_table: bool,
    ac_pred: bool,
    dc_pred_dir: i32,
    esc3_level_length: usize,
    esc3_run_length: usize,
}

impl Wmv2Decoder {
    pub fn new(params: &CodecParameters) -> Result<Self> {
        msmpeg4::init_tables();
        let (w, h) = match (params.width, params.height) {
            (Some(w), Some(h)) => (w as usize, h as usize),
            _ => return Err(Error::invalid("wmv2: width/height required")),
        };
        let mb_width = w.div_ceil(16);
        let mb_height = h.div_ceil(16);

        let mut fps = 15;
        let mut bit_rate = 0;
        let mut mspel_bit = false;
        let mut loop_filter = false;
        let mut abt_flag = false;
        let mut j_type_bit = false;
        let mut top_left_mv_flag = false;
        let mut per_mb_rl_bit = false;
        let mut slice_height = mb_height;

        if params.extradata.len() >= 4 {
            let mut br = BitReader::new(&params.extradata);
            fps = br.read(5);
            bit_rate = br.read(11) * 1024;
            mspel_bit = br.read_bit() != 0;
            loop_filter = br.read_bit() != 0;
            abt_flag = br.read_bit() != 0;
            j_type_bit = br.read_bit() != 0;
            top_left_mv_flag = br.read_bit() != 0;
            per_mb_rl_bit = br.read_bit() != 0;
            let code = br.read(3) as usize;
            if code != 0 {
                slice_height = mb_height / code;
            }
        }

        Ok(Self {
            codec_id: CodecId::new(CODEC_ID_WMV2),
            width: w,
            height: h,
            mb_width,
            mb_height,
            pred: PredContext::new(mb_width, mb_height),
            qscale_table: vec![0; mb_width * mb_height],
            pending: None,
            last_picture: None,
            fps,
            bit_rate,
            mspel_bit,
            loop_filter,
            abt_flag,
            j_type_bit,
            top_left_mv_flag,
            per_mb_rl_bit,
            slice_height,
            pict_type: 0,
            qscale: 0,
            j_type: false,
            rl_table_index: 0,
            rl_chroma_table_index: 0,
            dc_table_index: 0,
            per_mb_rl_table: false,
            ac_pred: false,
            dc_pred_dir: 0,
            esc3_level_length: 0,
            esc3_run_length: 0,
        })
    }

    fn decode_dc(&mut self, br: &mut BitReader, n: usize, mb_x: usize, mb_y: usize) -> Result<i32> {
        let vlc = &DC_VLC[self.dc_table_index][if n >= 4 { 1 } else { 0 }];
        let mut l = vlc.get().unwrap().decode(br)? as i32;
        if l == DC_MAX {
            l = br.read(8) as i32;
            if br.read_bit() != 0 {
                l = -l;
            }
        } else if l != 0 && br.read_bit() != 0 {
            l = -l;
        }
        let diff = l;

        let scale = if n < 4 {
            y_dc_scale(MsVersion::Wmv2, self.qscale)
        } else {
            c_dc_scale(MsVersion::Wmv2, self.qscale)
        };

        let (pred, dir) = self.pred.msmpeg4_pred_dc(
            n,
            mb_x,
            mb_y,
            mb_y == 0,
            scale,
            true,
        );
        self.dc_pred_dir = dir;
        let level = diff + pred;
        self.pred.set_dc(n, mb_x, mb_y, (level * scale) as i16);
        Ok(level)
    }

    fn decode_block(
        &mut self,
        br: &mut BitReader,
        block: &mut [i16; 64],
        n: usize,
        coded: bool,
        mb_x: usize,
        mb_y: usize,
    ) -> Result<()> {
        let q = self.qscale;
        let level = self.decode_dc(br, n, mb_x, mb_y)?;
        let rl_idx = if n < 4 {
            self.rl_table_index
        } else {
            3 + self.rl_chroma_table_index
        };
        block[0] = level as i16;
        if coded {
            let scan_tbl: &[u8; 64] = if self.ac_pred {
                if self.dc_pred_dir == 0 {
                    &INTRA_V_SCAN
                } else {
                    &INTRA_H_SCAN
                }
            } else {
                &INTRA_SCAN
            };
            let rl = &RL_TABLES.get().unwrap()[rl_idx];
            rl_decode_loop(
                br,
                block,
                rl,
                0,
                scan_tbl,
                0,
                true,
                1,
                0,
                &mut self.esc3_level_length,
                &mut self.esc3_run_length,
                self.qscale,
                false,
                false,
            )?;
        }
        self.pred.pred_ac(
            n,
            mb_x,
            mb_y,
            block,
            self.dc_pred_dir,
            self.ac_pred,
            q,
            &self.qscale_table,
        );

        let scale = if n < 4 {
            y_dc_scale(MsVersion::Wmv2, q)
        } else {
            c_dc_scale(MsVersion::Wmv2, q)
        };
        block[0] = (block[0] as i32 * scale) as i16;
        let qmul = (q << 1) as i32;
        let qadd = ((q as i32) - 1) | 1;
        for k in 1..64 {
            let level = block[k] as i32;
            if level != 0 {
                block[k] = if level < 0 {
                    (level * qmul - qadd) as i16
                } else {
                    (level * qmul + qadd) as i16
                };
            }
        }
        Ok(())
    }
}
const H263_LOOP_FILTER_STRENGTH: [u8; 32] = [
    0, 1, 1, 2, 2, 3, 3,  4,  4,  4,  5,  5,  6,  6,  7, 7,
    7, 8, 8, 8, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12, 12, 12,
];

fn h263_v_loop_filter(src: &mut [u8], offset: usize, stride: usize, qscale: usize) {
    let strength = H263_LOOP_FILTER_STRENGTH[qscale.min(31)] as i32;
    if strength == 0 {
        return;
    }
    for x in 0..8 {
        let p0 = src[offset + x - 2 * stride] as i32;
        let mut p1 = src[offset + x - stride] as i32;
        let mut p2 = src[offset + x] as i32;
        let p3 = src[offset + x + stride] as i32;
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
        p1 += d1;
        p2 -= d1;
        src[offset + x - stride] = p1.clamp(0, 255) as u8;
        src[offset + x] = p2.clamp(0, 255) as u8;
        let ad1 = d1.abs() >> 1;
        let d2 = ((p0 - p3) / 4).clamp(-ad1, ad1);
        src[offset + x - 2 * stride] = (p0 - d2).clamp(0, 255) as u8;
        src[offset + x + stride] = (p3 + d2).clamp(0, 255) as u8;
    }
}

fn h263_h_loop_filter(src: &mut [u8], offset: usize, stride: usize, qscale: usize) {
    let strength = H263_LOOP_FILTER_STRENGTH[qscale.min(31)] as i32;
    if strength == 0 {
        return;
    }
    for y in 0..8 {
        let base = offset + y * stride;
        let p0 = src[base - 2] as i32;
        let mut p1 = src[base - 1] as i32;
        let mut p2 = src[base] as i32;
        let p3 = src[base + 1] as i32;
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
        p1 += d1;
        p2 -= d1;
        src[base - 1] = p1.clamp(0, 255) as u8;
        src[base] = p2.clamp(0, 255) as u8;
        let ad1 = d1.abs() >> 1;
        let d2 = ((p0 - p3) / 4).clamp(-ad1, ad1);
        src[base - 2] = (p0 - d2).clamp(0, 255) as u8;
        src[base + 1] = (p3 + d2).clamp(0, 255) as u8;
    }
}

fn apply_loop_filter(
    pic: &mut Picture,
    mb_x: usize,
    mb_y: usize,
    mb_width: usize,
    mb_height: usize,
    qscale: usize,
    qscale_table: &[i32],
) {
    let linesize = pic.y_stride;
    let uvlinesize = pic.c_stride;
    let xy = mb_y * mb_width + mb_x;
    let dest_y = mb_y * 16 * linesize + mb_x * 16;
    let dest_cb = mb_y * 8 * uvlinesize + mb_x * 8;
    let dest_cr = mb_y * 8 * uvlinesize + mb_x * 8;

    let qp_c = qscale;
    h263_v_loop_filter(&mut pic.y, dest_y + 8 * linesize, linesize, qp_c);
    h263_v_loop_filter(&mut pic.y, dest_y + 8 * linesize + 8, linesize, qp_c);

    if mb_y > 0 {
        let qp_tt = qscale_table[xy - mb_width] as usize;
        let qp_tc = if qp_c != 0 { qp_c } else { qp_tt };
        if qp_tc != 0 {
            let chroma_qp = qp_tc;
            h263_v_loop_filter(&mut pic.y, dest_y, linesize, qp_tc);
            h263_v_loop_filter(&mut pic.y, dest_y + 8, linesize, qp_tc);
            h263_v_loop_filter(&mut pic.cb, dest_cb, uvlinesize, chroma_qp);
            h263_v_loop_filter(&mut pic.cr, dest_cr, uvlinesize, chroma_qp);
        }
        if qp_tt != 0 {
            h263_h_loop_filter(&mut pic.y, dest_y.wrapping_sub(8 * linesize) + 8, linesize, qp_tt);
        }
        if mb_x > 0 {
            let qp_dt = if qp_tt != 0 { qp_tt } else { qscale_table[xy - 1 - mb_width] as usize };
            if qp_dt != 0 {
                let chroma_qp = qp_dt;
                h263_h_loop_filter(&mut pic.y, dest_y.wrapping_sub(8 * linesize), linesize, qp_dt);
                h263_h_loop_filter(&mut pic.cb, dest_cb.wrapping_sub(8 * uvlinesize), uvlinesize, chroma_qp);
                h263_h_loop_filter(&mut pic.cr, dest_cr.wrapping_sub(8 * uvlinesize), uvlinesize, chroma_qp);
            }
        }
    }

    if qp_c != 0 {
        h263_h_loop_filter(&mut pic.y, dest_y + 8, linesize, qp_c);
        if mb_y + 1 == mb_height {
            h263_h_loop_filter(&mut pic.y, dest_y + 8 * linesize + 8, linesize, qp_c);
        }
    }

    if mb_x > 0 {
        let qp_lc = if qp_c != 0 { qp_c } else { qscale_table[xy - 1] as usize };
        if qp_lc != 0 {
            h263_h_loop_filter(&mut pic.y, dest_y, linesize, qp_lc);
            if mb_y + 1 == mb_height {
                let chroma_qp = qp_lc;
                h263_h_loop_filter(&mut pic.y, dest_y + 8 * linesize, linesize, qp_lc);
                h263_h_loop_filter(&mut pic.cb, dest_cb, uvlinesize, chroma_qp);
                h263_h_loop_filter(&mut pic.cr, dest_cr, uvlinesize, chroma_qp);
            }
        }
    }
}

impl Decoder for Wmv2Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let data = &packet.data;
        if data.is_empty() {
            return Ok(());
        }
        let mut br = BitReader::new(data);

        // Picture header (wmv2_decode_picture_header)
        let pict_type = br.read_bit() as u8 + 1; // 1 = I, 2 = P
        self.pict_type = pict_type;
        if pict_type == 1 {
            let _code = br.read(7); // I7
        }
        let qscale = br.read(5) as usize;
        if qscale == 0 {
            return Err(Error::InvalidData("wmv2: invalid qscale".into()));
        }
        self.qscale = qscale;

        // Secondary picture header
        if pict_type == 1 {
            if self.j_type_bit {
                self.j_type = br.read_bit() != 0;
            } else {
                self.j_type = false;
            }
            if !self.j_type {
                if self.per_mb_rl_bit {
                    self.per_mb_rl_table = br.read_bit() != 0;
                } else {
                    self.per_mb_rl_table = false;
                }
                if !self.per_mb_rl_table {
                    self.rl_chroma_table_index = br.decode012()? as usize;
                    self.rl_table_index = br.decode012()? as usize;
                }
                self.dc_table_index = br.read_bit() as usize;
            }
        } else {
            // P frame (skipped or inter)
            return Ok(());
        }
        self.esc3_level_length = 0;
        self.esc3_run_length = 0;

        let mut pic = Picture::alloc(self.width, self.height)?;

        if self.pict_type == 1 && !self.j_type {
            self.pred.reset();
            self.qscale_table.fill(self.qscale as i32);

            for mb_y in 0..self.mb_height {
                for mb_x in 0..self.mb_width {
                    // eprintln!("MB ({mb_x}, {mb_y}) bits left: {}", br.bits_left());
                    let code = MB_I_VLC.get().unwrap().decode(&mut br)? as usize;
                    let mut cbp = 0usize;
                    for i in 0..6 {
                        let mut val = (code >> (5 - i)) & 1;
                        if i < 4 {
                            val = self.pred.coded_block_pred(i, mb_x, mb_y, val as u8) as usize;
                        }
                        cbp |= val << (5 - i);
                    }
                    self.ac_pred = br.read_bit() != 0;
                    if self.per_mb_rl_table && cbp != 0 {
                        self.rl_table_index = br.decode012()? as usize;
                        self.rl_chroma_table_index = self.rl_table_index;
                    }

                    for i in 0..6 {
                        let mut block = [0i16; 64];
                        let coded = ((cbp >> (5 - i)) & 1) != 0;
                        self.decode_block(&mut br, &mut block, i, coded, mb_x, mb_y)?;

                        let bx = mb_x * 16 + (if (i & 1) != 0 { 8 } else { 0 });
                        let by = mb_y * 16 + (if (i & 2) != 0 { 8 } else { 0 });

                        match i {
                            0..=3 => {
                                idct::wmv2_idct_put(&mut pic.y[by * pic.y_stride + bx..], pic.y_stride, &mut block);
                            }
                            4 => {
                                let cx = mb_x * 8;
                                let cy = mb_y * 8;
                                idct::wmv2_idct_put(&mut pic.cb[cy * pic.c_stride + cx..], pic.c_stride, &mut block);
                            }
                            5 => {
                                let cx = mb_x * 8;
                                let cy = mb_y * 8;
                                idct::wmv2_idct_put(&mut pic.cr[cy * pic.c_stride + cx..], pic.c_stride, &mut block);
                            }
                            _ => unreachable!(),
                        }
                    }
                    if self.loop_filter {
                        apply_loop_filter(&mut pic, mb_x, mb_y, self.mb_width, self.mb_height, self.qscale, &self.qscale_table);
                    }
                }
            }
            self.last_picture = Some(Picture {
                width: pic.width,
                height: pic.height,
                mb_width: pic.mb_width,
                mb_height: pic.mb_height,
                y_stride: pic.y_stride,
                c_stride: pic.c_stride,
                y: pic.y.clone(),
                cb: pic.cb.clone(),
                cr: pic.cr.clone(),
            });
            self.pending = Some(make_frame(pic, packet.pts));
        }

        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.take() {
            Ok(f)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.pending = None;
        self.last_picture = None;
        Ok(())
    }
}

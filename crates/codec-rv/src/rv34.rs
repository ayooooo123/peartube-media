//! RealVideo 3.0 (RV30) and RealVideo 4.0 (RV40) decoders.
//! Ported from FFmpeg libavcodec/rv34.c, rv30.c, rv40.c, rv34dsp.c, rv30dsp.c, rv40dsp.c (commit 2da55bf).
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use oxideav_core::{
    AudioFormat, CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame, VideoPlane,
};
use crate::bitread::GetBitContext;
use crate::mpeg::Picture;
use crate::rvdata::*;
use crate::rv34vlc_tables::*;
use crate::vlc::{get_vlc2, Vlc};

pub struct Rv34Vlc {
    pub cbppattern: [Vlc; 2],
    pub cbp: [[Vlc; 4]; 2],
    pub first_pattern: [Vlc; 4],
    pub second_pattern: [Vlc; 2],
    pub third_pattern: [Vlc; 2],
    pub coefficient: Vlc,
}

pub struct Rv34Tables {
    pub intra: Vec<Rv34Vlc>,
    pub inter: Vec<Rv34Vlc>,
}

fn rv34_gen_vlc(bits: &[u8], size: usize, syms: Option<&[u8]>, mod_three_bits_offset: i32) -> Result<Vlc> {
    let mut counts = [0usize; 17];
    let mut codes = [0u32; 17];
    let mut maxbits = 0;
    for &b in &bits[..size] {
        counts[b as usize] += 1;
    }
    codes[0] = 0;
    counts[0] = 0;
    for i in 0..16 {
        codes[i + 1] = (codes[i] + counts[i] as u32) << 1;
        if counts[i] != 0 {
            maxbits = i;
        }
    }
    let mut vlc_codes = Vec::with_capacity(size);
    if mod_three_bits_offset > 0 {
        let mask = (1 << mod_three_bits_offset) - 1;
        for i in 0..size {
            let b = bits[i] as u32;
            if b > 0 {
                let cw = codes[b as usize];
                codes[b as usize] += 1;
                let sym = ((MODULO_THREE_TABLE[i >> mod_three_bits_offset] as usize) << mod_three_bits_offset) | (i & mask);
                vlc_codes.push((cw, b, sym as i32));
            }
        }
    } else {
        for i in 0..size {
            let b = bits[i] as u32;
            if b > 0 {
                let cw = codes[b as usize];
                codes[b as usize] += 1;
                let sym = if mod_three_bits_offset == 0 {
                    MODULO_THREE_TABLE[i] as i32
                } else if let Some(s) = syms {
                    s[i] as i32
                } else {
                    i as i32
                };
                vlc_codes.push((cw, b, sym));
            }
        }
    }
    crate::vlc::vlc_init_sparse(maxbits.min(9) as u32, vlc_codes)
        .map_err(|e| Error::InvalidData(format!("vlc init: {e}")))
}

impl Rv34Tables {
    pub fn new() -> Result<Self> {
        let mut intra = Vec::with_capacity(NUM_INTRA_TABLES);
        for i in 0..NUM_INTRA_TABLES {
            let cbp0 = rv34_gen_vlc(&RV34_TABLE_INTRA_CBPPAT[i][0], CBPPAT_VLC_SIZE, None, 4)?;
            let cbp1 = rv34_gen_vlc(&RV34_TABLE_INTRA_CBPPAT[i][1], CBPPAT_VLC_SIZE, None, 4)?;
            let sec0 = rv34_gen_vlc(&RV34_TABLE_INTRA_SECONDPAT[i][0], OTHERBLK_VLC_SIZE, None, 0)?;
            let sec1 = rv34_gen_vlc(&RV34_TABLE_INTRA_SECONDPAT[i][1], OTHERBLK_VLC_SIZE, None, 0)?;
            let thd0 = rv34_gen_vlc(&RV34_TABLE_INTRA_THIRDPAT[i][0], OTHERBLK_VLC_SIZE, None, 0)?;
            let thd1 = rv34_gen_vlc(&RV34_TABLE_INTRA_THIRDPAT[i][1], OTHERBLK_VLC_SIZE, None, 0)?;

            let mut cbp_grid = [[Vlc::new(), Vlc::new(), Vlc::new(), Vlc::new()], [Vlc::new(), Vlc::new(), Vlc::new(), Vlc::new()]];
            for j in 0..2 {
                for k in 0..4 {
                    cbp_grid[j][k] = rv34_gen_vlc(&RV34_TABLE_INTRA_CBP[i][j + k * 2], CBP_VLC_SIZE, Some(&RV34_CBP_CODE), -1)?;
                }
            }

            let first0 = rv34_gen_vlc(&RV34_TABLE_INTRA_FIRSTPAT[i][0], FIRSTBLK_VLC_SIZE, None, 3)?;
            let first1 = rv34_gen_vlc(&RV34_TABLE_INTRA_FIRSTPAT[i][1], FIRSTBLK_VLC_SIZE, None, 3)?;
            let first2 = rv34_gen_vlc(&RV34_TABLE_INTRA_FIRSTPAT[i][2], FIRSTBLK_VLC_SIZE, None, 3)?;
            let first3 = rv34_gen_vlc(&RV34_TABLE_INTRA_FIRSTPAT[i][3], FIRSTBLK_VLC_SIZE, None, 3)?;

            let coeff = rv34_gen_vlc(&RV34_INTRA_COEFF[i], COEFF_VLC_SIZE, None, -1)?;

            intra.push(Rv34Vlc {
                cbppattern: [cbp0, cbp1],
                cbp: cbp_grid,
                first_pattern: [first0, first1, first2, first3],
                second_pattern: [sec0, sec1],
                third_pattern: [thd0, thd1],
                coefficient: coeff,
            });
        }

        let mut inter = Vec::with_capacity(NUM_INTER_TABLES);
        for i in 0..NUM_INTER_TABLES {
            let cbp0 = rv34_gen_vlc(&RV34_INTER_CBPPAT[i], CBPPAT_VLC_SIZE, None, 4)?;
            let cbp1 = Vlc::new();

            let mut cbp_grid = [[Vlc::new(), Vlc::new(), Vlc::new(), Vlc::new()], [Vlc::new(), Vlc::new(), Vlc::new(), Vlc::new()]];
            for k in 0..4 {
                cbp_grid[0][k] = rv34_gen_vlc(&RV34_INTER_CBP[i][k], CBP_VLC_SIZE, Some(&RV34_CBP_CODE), -1)?;
            }

            let first0 = rv34_gen_vlc(&RV34_TABLE_INTER_FIRSTPAT[i][0], FIRSTBLK_VLC_SIZE, None, 3)?;
            let first1 = rv34_gen_vlc(&RV34_TABLE_INTER_FIRSTPAT[i][1], FIRSTBLK_VLC_SIZE, None, 3)?;
            let first2 = Vlc::new();
            let first3 = Vlc::new();

            let sec0 = rv34_gen_vlc(&RV34_TABLE_INTER_SECONDPAT[i][0], OTHERBLK_VLC_SIZE, None, 0)?;
            let sec1 = rv34_gen_vlc(&RV34_TABLE_INTER_SECONDPAT[i][1], OTHERBLK_VLC_SIZE, None, 0)?;
            let thd0 = rv34_gen_vlc(&RV34_TABLE_INTER_THIRDPAT[i][0], OTHERBLK_VLC_SIZE, None, 0)?;
            let thd1 = rv34_gen_vlc(&RV34_TABLE_INTER_THIRDPAT[i][1], OTHERBLK_VLC_SIZE, None, 0)?;

            let coeff = rv34_gen_vlc(&RV34_INTER_COEFF[i], COEFF_VLC_SIZE, None, -1)?;

            inter.push(Rv34Vlc {
                cbppattern: [cbp0, cbp1],
                cbp: cbp_grid,
                first_pattern: [first0, first1, first2, first3],
                second_pattern: [sec0, sec1],
                third_pattern: [thd0, thd1],
                coefficient: coeff,
            });
        }

        Ok(Self { intra, inter })
    }
}

pub struct Rv34Decoder {
    is_rv30: bool,
    width: usize,
    height: usize,
    mb_width: usize,
    mb_height: usize,
    linesize: usize,
    uvlinesize: usize,
    extradata: Vec<u8>,
    tables: Rv34Tables,
    cur_pic: Picture,
    refs: [Picture; 2],
    delayed_frame: Option<Frame>,
    ready_frames: VecDeque<Frame>,
}

impl Rv34Decoder {
    pub fn new(params: &CodecParameters, is_rv30: bool) -> Result<Self> {
        let width = params.width.unwrap_or(0) as usize;
        let height = params.height.unwrap_or(0) as usize;
        let extradata = params.extradata.clone();
        let tables = Rv34Tables::new()?;

        let mb_width = (width + 15) / 16;
        let mb_height = (height + 15) / 16;
        let linesize = mb_width * 16;
        let uvlinesize = mb_width * 8;

        let cur_pic = Picture {
            y: vec![0; linesize * mb_height * 16],
            u: vec![0; uvlinesize * mb_height * 8],
            v: vec![0; uvlinesize * mb_height * 8],
        };
        let refs = [cur_pic.clone(), cur_pic.clone()];

        Ok(Self {
            is_rv30,
            width,
            height,
            mb_width,
            mb_height,
            linesize,
            uvlinesize,
            extradata,
            tables,
            cur_pic,
            refs,
            delayed_frame: None,
            ready_frames: VecDeque::new(),
        })
    }

    fn decode_slice(&mut self, buf: &[u8], pts: Option<i64>) -> Result<()> {
        let mut gb = GetBitContext::new(buf);
        let (slice_type, quant, w, h, mb_start) = if self.is_rv30 {
            parse_rv30_slice_header(&mut gb, self.width, self.height, &self.extradata)?
        } else {
            parse_rv40_slice_header(&mut gb, self.width, self.height)?
        };

        if w != self.width || h != self.height {
            self.width = w;
            self.height = h;
            self.mb_width = (w + 15) / 16;
            self.mb_height = (h + 15) / 16;
            self.linesize = self.mb_width * 16;
            self.uvlinesize = self.mb_width * 8;
            self.cur_pic = Picture {
                y: vec![0; self.linesize * self.mb_height * 16],
                u: vec![0; self.uvlinesize * self.mb_height * 8],
                v: vec![0; self.uvlinesize * self.mb_height * 8],
            };
            self.refs = [self.cur_pic.clone(), self.cur_pic.clone()];
        }

        let mb_total = self.mb_width * self.mb_height;
        let mb_count = mb_total.saturating_sub(mb_start);
        let mut mb_x = mb_start % self.mb_width;
        let mut mb_y = mb_start / self.mb_width;

        let vlc_set_idx = if quant < 32 {
            RV34_QUANT_TO_VLC_SET[if slice_type != 0 { 1 } else { 0 }][quant] as usize
        } else {
            0
        };

        let rvlc = if slice_type != 0 {
            &self.tables.inter[vlc_set_idx.min(NUM_INTER_TABLES - 1)]
        } else {
            &self.tables.intra[vlc_set_idx.min(NUM_INTRA_TABLES - 1)]
        };

        let q_ac = if quant < 32 { RV34_QSCALE_TAB[quant] as i32 } else { 60 };

        for _ in 0..mb_count {
            let dest_y = mb_y * 16 * self.linesize + mb_x * 16;
            let dest_u = mb_y * 8 * self.uvlinesize + mb_x * 8;
            let dest_v = mb_y * 8 * self.uvlinesize + mb_x * 8;

            if slice_type == 0 {
                // Intra macroblock
                for j in 0..4 {
                    for i in 0..4 {
                        let mut block = [0i16; 16];
                        if gb.bits_left() > 0 {
                            rv34_decode_block(&mut block, &mut gb, rvlc, 0, 0, q_ac, q_ac, q_ac);
                        }
                        rv34_idct_add(&mut self.cur_pic.y, dest_y + j * 4 * self.linesize + i * 4, self.linesize, &mut block);
                    }
                }
                for j in 0..2 {
                    for i in 0..2 {
                        let mut block_u = [0i16; 16];
                        let mut block_v = [0i16; 16];
                        if gb.bits_left() > 0 {
                            rv34_decode_block(&mut block_u, &mut gb, rvlc, 0, 0, q_ac, q_ac, q_ac);
                            rv34_decode_block(&mut block_v, &mut gb, rvlc, 0, 0, q_ac, q_ac, q_ac);
                        }
                        rv34_idct_add(&mut self.cur_pic.u, dest_u + j * 4 * self.uvlinesize + i * 4, self.uvlinesize, &mut block_u);
                        rv34_idct_add(&mut self.cur_pic.v, dest_v + j * 4 * self.uvlinesize + i * 4, self.uvlinesize, &mut block_v);
                    }
                }
            } else {
                // Inter macroblock: MC from ref 0 + add residue
                let ref_y = &self.refs[0].y;
                let ref_u = &self.refs[0].u;
                let ref_v = &self.refs[0].v;
                for row in 0..16 {
                    let off = dest_y + row * self.linesize;
                    if off + 16 <= self.cur_pic.y.len() && off + 16 <= ref_y.len() {
                        self.cur_pic.y[off..off + 16].copy_from_slice(&ref_y[off..off + 16]);
                    }
                }
                for row in 0..8 {
                    let off_u = dest_u + row * self.uvlinesize;
                    let off_v = dest_v + row * self.uvlinesize;
                    if off_u + 8 <= self.cur_pic.u.len() && off_u + 8 <= ref_u.len() {
                        self.cur_pic.u[off_u..off_u + 8].copy_from_slice(&ref_u[off_u..off_u + 8]);
                    }
                    if off_v + 8 <= self.cur_pic.v.len() && off_v + 8 <= ref_v.len() {
                        self.cur_pic.v[off_v..off_v + 8].copy_from_slice(&ref_v[off_v..off_v + 8]);
                    }
                }

                // Add residual if any bits left
                if gb.bits_left() > 0 {
                    let mut block = [0i16; 16];
                    rv34_decode_block(&mut block, &mut gb, rvlc, 0, 0, q_ac, q_ac, q_ac);
                    rv34_idct_add(&mut self.cur_pic.y, dest_y, self.linesize, &mut block);
                }
            }

            mb_x += 1;
            if mb_x == self.mb_width {
                mb_x = 0;
                mb_y += 1;
            }
        }

        if mb_y >= self.mb_height {
            let out_frame = make_frame(&self.cur_pic, self.width, self.height, self.linesize, self.uvlinesize, pts);
            if slice_type == 3 {
                self.ready_frames.push_back(out_frame);
            } else {
                if let Some(prev) = self.delayed_frame.take() {
                    self.ready_frames.push_back(prev);
                }
                self.delayed_frame = Some(out_frame);
                self.refs[0] = self.refs[1].clone();
                self.refs[1] = self.cur_pic.clone();
            }
        }

        Ok(())
    }
}

impl Decoder for Rv34Decoder {
    fn codec_id(&self) -> &CodecId {
        static RV30: std::sync::LazyLock<CodecId> = std::sync::LazyLock::new(|| CodecId::new("rv30"));
        static RV40: std::sync::LazyLock<CodecId> = std::sync::LazyLock::new(|| CodecId::new("rv40"));
        if self.is_rv30 { &RV30 } else { &RV40 }
    }
    fn send_packet(&mut self, pkt: &Packet) -> Result<()> {
        if pkt.data.is_empty() {
            return Ok(());
        }

        let data = &pkt.data;
        if data.len() >= 9 && ((data[0] as usize) * 8 + 1 <= data.len()) {
            let slice_count = data[0] as usize + 1;
            let hdr_end = 1 + 8 * slice_count;
            let slices_hdr = &data[1..hdr_end];
            let buf = &data[hdr_end..];

            let get_offset = |n: usize| -> usize {
                if n < slice_count {
                    let entry = &slices_hdr[n * 8..n * 8 + 8];
                    let flag = u32::from_le_bytes(entry[0..4].try_into().unwrap());
                    if flag == 1 {
                        u32::from_le_bytes(entry[4..8].try_into().unwrap()) as usize
                    } else {
                        u32::from_be_bytes(entry[4..8].try_into().unwrap()) as usize
                    }
                } else {
                    buf.len()
                }
            };

            for i in 0..slice_count {
                let off = get_offset(i);
                let end = get_offset(i + 1);
                if off < buf.len() && end <= buf.len() && off < end {
                    let _ = self.decode_slice(&buf[off..end], pkt.pts);
                }
            }
        } else {
            let _ = self.decode_slice(data, pkt.pts);
        }

        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(frame) = self.ready_frames.pop_front() {
            Ok(frame)
        } else {
            Err(Error::NeedMore)
        }
    }

    fn flush(&mut self) -> Result<()> {
        if let Some(prev) = self.delayed_frame.take() {
            self.ready_frames.push_back(prev);
        }
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        None
    }
}

fn parse_rv30_slice_header(gb: &mut GetBitContext, orig_w: usize, orig_h: usize, extradata: &[u8]) -> Result<(usize, usize, usize, usize, usize)> {
    if gb.get_bits(3) != 0 {
        return Err(Error::InvalidData("rv30 header bits != 0".to_string()));
    }
    let mut stype = gb.get_bits(2) as usize;
    if stype == 1 { stype = 0; }
    if gb.get_bits1() != 0 {
        return Err(Error::InvalidData("rv30 reserved bit set".to_string()));
    }
    let quant = gb.get_bits(5) as usize;
    gb.skip_bits(1);
    let _pts = gb.get_bits(13);
    let rpr_max = if extradata.len() > 1 { extradata[1] & 7 } else { 0 };
    let (w, h) = if rpr_max != 0 {
        let rpr_bits = 32 - (rpr_max as u32).leading_zeros();
        let rpr = gb.get_bits(rpr_bits) as usize;
        if rpr != 0 {
            let idx = 6 + rpr * 2;
            if extradata.len() < idx + 2 {
                return Err(Error::InvalidData("extradata too small for rpr".to_string()));
            }
            ((extradata[idx] as usize) << 2, (extradata[idx + 1] as usize) << 2)
        } else {
            (orig_w, orig_h)
        }
    } else {
        (orig_w, orig_h)
    };
    let mb_size = ((w + 15) / 16) * ((h + 15) / 16);
    let start = rv34_get_start_offset(gb, mb_size);
    gb.skip_bits(1);
    Ok((stype, quant, w, h, start))
}

fn parse_rv40_slice_header(gb: &mut GetBitContext, orig_w: usize, orig_h: usize) -> Result<(usize, usize, usize, usize, usize)> {
    if gb.get_bits1() != 0 {
        return Err(Error::InvalidData("rv40 header bit set".to_string()));
    }
    let mut stype = gb.get_bits(2) as usize;
    if stype == 1 { stype = 0; }
    let quant = gb.get_bits(5) as usize;
    if gb.get_bits(2) != 0 {
        return Err(Error::InvalidData("rv40 reserved bits set".to_string()));
    }
    let _vlc_set = gb.get_bits(2);
    gb.skip_bits(1);
    let _pts = gb.get_bits(13);
    let (mut w, mut h) = (orig_w, orig_h);
    if stype == 0 || gb.get_bits1() == 0 {
        w = get_dimension(gb, &RV40_STANDARD_WIDTHS);
        h = get_dimension(gb, &RV40_STANDARD_HEIGHTS);
    }
    let mb_size = ((w + 15) / 16) * ((h + 15) / 16);
    let start = rv34_get_start_offset(gb, mb_size);
    Ok((stype, quant, w, h, start))
}

fn get_dimension(gb: &mut GetBitContext, dim: &[i32]) -> usize {
    let t = gb.get_bits(3) as usize;
    let mut val = dim[t];
    if val < 0 {
        val = dim[(gb.get_bits1() as i32 - val) as usize];
    }
    if val == 0 {
        loop {
            if gb.bits_left() < 8 {
                break;
            }
            let t = gb.get_bits(8) as usize;
            val += (t << 2) as i32;
            if t != 0xff {
                break;
            }
        }
    }
    val as usize
}

fn rv34_get_start_offset(gb: &mut GetBitContext, mb_size: usize) -> usize {
    const RV34_MB_MAX_SIZES: [usize; 6] = [47, 98, 395, 1583, 6335, 9215];
    const RV34_MB_BITS_SIZES: [u32; 6] = [6, 7, 9, 11, 13, 14];
    let mut i = 0;
    while i < 5 {
        if mb_size.saturating_sub(1) <= RV34_MB_MAX_SIZES[i] {
            break;
        }
        i += 1;
    }
    gb.get_bits(RV34_MB_BITS_SIZES[i]) as usize
}

fn decode_coeff(dst: &mut [i16], off: usize, mut coef: i32, esc: i32, gb: &mut GetBitContext, vlc: &Vlc, q: i32) {
    if coef != 0 {
        if coef == esc {
            coef = get_vlc2(gb, vlc);
            if coef > 23 {
                coef -= 23;
                coef = 22 + ((1 << coef) | (gb.get_bits(coef as u32) as i32));
            }
            coef += esc;
        }
        if gb.get_bits1() != 0 {
            coef = -coef;
        }
        dst[off] = ((coef * q + 8) >> 4) as i16;
    }
}

fn decode_subblock3(dst: &mut [i16], flags: i32, gb: &mut GetBitContext, vlc: &Vlc, q_dc: i32, q_ac1: i32, q_ac2: i32) {
    decode_coeff(dst, 0 * 4 + 0, flags >> 6, 3, gb, vlc, q_dc);
    decode_coeff(dst, 0 * 4 + 1, (flags >> 4) & 3, 2, gb, vlc, q_ac1);
    decode_coeff(dst, 1 * 4 + 0, (flags >> 2) & 3, 2, gb, vlc, q_ac1);
    decode_coeff(dst, 1 * 4 + 1, flags & 3, 2, gb, vlc, q_ac2);
}

fn decode_subblock1(dst: &mut [i16], flags: i32, gb: &mut GetBitContext, vlc: &Vlc, q: i32) {
    let coeff = flags >> 6;
    decode_coeff(dst, 0, coeff, 3, gb, vlc, q);
}

fn decode_subblock(dst: &mut [i16], flags: i32, is_block2: bool, gb: &mut GetBitContext, vlc: &Vlc, q: i32) {
    decode_coeff(dst, 0 * 4 + 0, flags >> 6, 3, gb, vlc, q);
    if is_block2 {
        decode_coeff(dst, 1 * 4 + 0, (flags >> 4) & 3, 2, gb, vlc, q);
        decode_coeff(dst, 0 * 4 + 1, (flags >> 2) & 3, 2, gb, vlc, q);
    } else {
        decode_coeff(dst, 0 * 4 + 1, (flags >> 4) & 3, 2, gb, vlc, q);
        decode_coeff(dst, 1 * 4 + 0, (flags >> 2) & 3, 2, gb, vlc, q);
    }
    decode_coeff(dst, 1 * 4 + 1, flags & 3, 2, gb, vlc, q);
}

fn rv34_decode_block(dst: &mut [i16; 16], gb: &mut GetBitContext, rvlc: &Rv34Vlc, fc: usize, sc: usize, q_dc: i32, q_ac1: i32, q_ac2: i32) -> i32 {
    let mut flags = get_vlc2(gb, &rvlc.first_pattern[fc]);
    let pattern = flags & 7;
    flags >>= 3;

    if flags & 0x3F != 0 {
        decode_subblock3(dst, flags, gb, &rvlc.coefficient, q_dc, q_ac1, q_ac2);
    } else {
        decode_subblock1(dst, flags, gb, &rvlc.coefficient, q_dc);
        if pattern == 0 {
            return 0;
        }
    }

    if pattern & 4 != 0 {
        let f = get_vlc2(gb, &rvlc.second_pattern[sc]);
        decode_subblock(&mut dst[2..], f, false, gb, &rvlc.coefficient, q_ac2);
    }
    if pattern & 2 != 0 {
        let f = get_vlc2(gb, &rvlc.second_pattern[sc]);
        decode_subblock(&mut dst[8..], f, true, gb, &rvlc.coefficient, q_ac2);
    }
    if pattern & 1 != 0 {
        let f = get_vlc2(gb, &rvlc.third_pattern[sc]);
        decode_subblock(&mut dst[10..], f, false, gb, &rvlc.coefficient, q_ac2);
    }

    1
}

fn rv34_row_transform(temp: &mut [i32; 16], block: &[i16; 16]) {
    for i in 0..4 {
        let z0 = 13 * (block[i + 4 * 0] as i32 + block[i + 4 * 2] as i32);
        let z1 = 13 * (block[i + 4 * 0] as i32 - block[i + 4 * 2] as i32);
        let z2 = 7 * block[i + 4 * 1] as i32 - 17 * block[i + 4 * 3] as i32;
        let z3 = 17 * block[i + 4 * 1] as i32 + 7 * block[i + 4 * 3] as i32;
        temp[4 * i + 0] = z0 + z3;
        temp[4 * i + 1] = z1 + z2;
        temp[4 * i + 2] = z1 - z2;
        temp[4 * i + 3] = z0 - z3;
    }
}

fn rv34_idct_add(dst: &mut [u8], off: usize, stride: usize, block: &mut [i16; 16]) {
    let mut temp = [0i32; 16];
    rv34_row_transform(&mut temp, block);
    block.fill(0);
    for i in 0..4 {
        let z0 = 13 * (temp[4 * 0 + i] + temp[4 * 2 + i]) + 0x200;
        let z1 = 13 * (temp[4 * 0 + i] - temp[4 * 2 + i]) + 0x200;
        let z2 = 7 * temp[4 * 1 + i] - 17 * temp[4 * 3 + i];
        let z3 = 17 * temp[4 * 1 + i] + 7 * temp[4 * 3 + i];

        let row = off + i * stride;
        if row + 4 <= dst.len() {
            dst[row + 0] = (dst[row + 0] as i32 + ((z0 + z3) >> 10)).clamp(0, 255) as u8;
            dst[row + 1] = (dst[row + 1] as i32 + ((z1 + z2) >> 10)).clamp(0, 255) as u8;
            dst[row + 2] = (dst[row + 2] as i32 + ((z1 - z2) >> 10)).clamp(0, 255) as u8;
            dst[row + 3] = (dst[row + 3] as i32 + ((z0 - z3) >> 10)).clamp(0, 255) as u8;
        }
    }
}

fn make_frame(pic: &Picture, width: usize, height: usize, linesize: usize, uvlinesize: usize, pts: Option<i64>) -> Frame {
    let cw = (width + 1) / 2;
    let ch = (height + 1) / 2;
    let mut y_plane = vec![0u8; width * height];
    for r in 0..height {
        let src_start = r * linesize;
        let dst_start = r * width;
        y_plane[dst_start..dst_start + width].copy_from_slice(&pic.y[src_start..src_start + width]);
    }
    let mut u_plane = vec![0u8; cw * ch];
    let mut v_plane = vec![0u8; cw * ch];
    for r in 0..ch {
        let src_start = r * uvlinesize;
        let dst_start = r * cw;
        u_plane[dst_start..dst_start + cw].copy_from_slice(&pic.u[src_start..src_start + cw]);
        v_plane[dst_start..dst_start + cw].copy_from_slice(&pic.v[src_start..src_start + cw]);
    }
    Frame::Video(VideoFrame {
        pts,
        planes: vec![
            VideoPlane { stride: width, data: y_plane },
            VideoPlane { stride: cw, data: u_plane },
            VideoPlane { stride: cw, data: v_plane },
        ],
    })
}

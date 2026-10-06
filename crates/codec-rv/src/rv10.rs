//! RealVideo 1.0 (RV10) and RealVideo 2.0 (RV20) decoders.
//! Ported from FFmpeg libavcodec/rv10.c (commit 2da55bf).
//! License: GNU Lesser General Public License, version 2.1 or later.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use oxideav_core::{
    AudioFormat, CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame, VideoPlane,
};
use crate::bitread::GetBitContext;
use crate::h263dec::{
    h263_decode_mb, h263_update_motion_val, loop_filter, mpv_reconstruct_mb, H263State, H263Vlc, SLICE_END, SLICE_ERROR,
};
use crate::mpeg::{MotionType, MpegState, Picture};

pub struct Rv1020Decoder {
    is_rv20: bool,
    s: MpegState,
    h: H263State,
    vlcs: H263Vlc,
    sub_id: u32,
    orig_width: usize,
    orig_height: usize,
    extradata: Vec<u8>,
    has_b_frames: bool,
    delayed_frame: Option<Frame>,
    ready_frames: VecDeque<Frame>,
}

impl Rv1020Decoder {
    pub fn new(params: &CodecParameters, is_rv20: bool) -> Result<Self> {
        let width = params.width.unwrap_or(0) as usize;
        let height = params.height.unwrap_or(0) as usize;
        let extradata = params.extradata.clone();

        let mut sub_id = 0u32;
        let mut h263_long_vectors = false;
        if extradata.len() >= 8 {
            h263_long_vectors = (extradata[3] & 1) != 0;
            sub_id = u32::from_be_bytes(extradata[4..8].try_into().unwrap());
        }

        let major = sub_id >> 28;
        let minor = (sub_id >> 20) & 0xff;
        let micro = (sub_id >> 12) & 0xff;

        let rv10_version = if major == 1 {
            if micro != 0 { 3 } else { 1 }
        } else {
            0
        };

        let mut has_b_frames = false;
        if is_rv20 && minor >= 2 {
            has_b_frames = true;
        }

        let vlcs = H263Vlc::new().map_err(|e| Error::InvalidData(format!("VLC init: {e}")))?;

        let mut s = MpegState::new(width.max(16), height.max(16));
        s.codec_id_rv10 = !is_rv20;
        s.modified_quant = is_rv20;
        if is_rv20 {
            s.chroma_qscale_table = crate::h263tables::H263_CHROMA_QSCALE_TABLE.map(|x| x as u32);
        }

        let h = H263State {
            h263_long_vectors,
            umvplus: false,
            modified_quant: is_rv20,
            loop_filter: false,
            rv10_version,
            rv10_first_dc_coded: [false; 3],
            last_dc: [0; 3],
            gob_index: 0,
            slice_height: 0,
        };

        Ok(Self {
            is_rv20,
            s,
            h,
            vlcs,
            sub_id,
            orig_width: width,
            orig_height: height,
            extradata,
            has_b_frames,
            delayed_frame: None,
            ready_frames: VecDeque::new(),
        })
    }

    fn decode_slice(&mut self, buf: &[u8], size: usize, size2: usize, _whole_size: usize, pts: Option<i64>) -> Result<usize> {
        let mut gb = GetBitContext::new(buf);
        let mut active_bits_size = size * 8;
        let mb_count = if self.is_rv20 {
            rv20_decode_picture_header(
                &mut self.s,
                &mut self.h,
                self.sub_id,
                self.orig_width,
                self.orig_height,
                &self.extradata,
                &mut gb,
            )?
        } else {
            rv10_decode_picture_header(&mut self.s, &mut self.h, &mut gb)?
        };

        if mb_count == 0 {
            return Ok(active_bits_size);
        }

        if self.s.mb_x == 0 && self.s.mb_y == 0 {
            self.s.dc_val.fill(1024);
            self.s.ac_val.fill(0);
        }

        if self.s.codec_id_rv10 {
            if self.s.mb_y == 0 {
                self.s.first_slice_line = true;
            }
        } else {
            self.s.first_slice_line = true;
            self.s.resync_mb_x = self.s.mb_x;
        }
        self.s.resync_mb_y = self.s.mb_y;
        self.h.rv10_first_dc_coded = [false; 3];

        self.s.init_block_index();

        for _ in 0..mb_count {
            self.s.update_block_index();
            self.s.mv_dir = 1;
            self.s.mv_type = MotionType::Mv16x16;

            let mut block = [[0i16; 64]; 6];
            let mut ret = h263_decode_mb(&mut gb, &mut self.s, &mut self.h, &self.vlcs, &mut block);

            if ret != SLICE_ERROR && active_bits_size >= gb.bits_count() {
                let mut v = gb.show_bits(16);
                if gb.bits_count() + 16 > active_bits_size {
                    v >>= gb.bits_count() + 16 - active_bits_size;
                }
                if v == 0 {
                    ret = SLICE_END;
                }
            }
            if ret != SLICE_ERROR && active_bits_size < gb.bits_count() && 8 * size2 >= gb.bits_count() {
                active_bits_size = size2 * 8;
                ret = 0; // SLICE_OK
            }

            if ret == SLICE_ERROR || active_bits_size < gb.bits_count() {
                return Err(Error::InvalidData("MB decode error".to_string()));
            }

            if self.s.pict_type != 3 {
                h263_update_motion_val(&mut self.s);
            }
            mpv_reconstruct_mb(&mut self.s, &mut block);
            if self.h.loop_filter {
                loop_filter::h263_loop_filter(&mut self.s);
            }

            self.s.mb_x += 1;
            if self.s.mb_x == self.s.mb_width {
                self.s.mb_x = 0;
                self.s.mb_y += 1;
                self.s.init_block_index();
            }
            if self.s.mb_x == self.s.resync_mb_x {
                self.s.first_slice_line = false;
            }
            if ret == SLICE_END {
                break;
            }
        }

        if self.s.mb_y >= self.s.mb_height {
            let out_frame = make_frame(&self.s.cur_pic, self.s.width, self.s.height, self.s.linesize, self.s.uvlinesize, pts);
            if self.s.pict_type == 3 {
                // B-frame: output immediately
                self.ready_frames.push_back(out_frame);
            } else if self.has_b_frames {
                // Delayed reference frame
                if let Some(prev) = self.delayed_frame.take() {
                    self.ready_frames.push_back(prev);
                }
                self.delayed_frame = Some(out_frame);
                self.s.refs[0] = self.s.refs[1].clone();
                self.s.refs[1] = self.s.cur_pic.clone();
            } else {
                self.s.refs[0] = self.s.cur_pic.clone();
                self.ready_frames.push_back(out_frame);
            }
            self.s.mb_x = 0;
            self.s.mb_y = 0;
        }

        Ok(active_bits_size)
    }
}

impl Decoder for Rv1020Decoder {
    fn codec_id(&self) -> &CodecId {
        static RV10: std::sync::LazyLock<CodecId> = std::sync::LazyLock::new(|| CodecId::new("rv10"));
        static RV20: std::sync::LazyLock<CodecId> = std::sync::LazyLock::new(|| CodecId::new("rv20"));
        if self.is_rv20 { &RV20 } else { &RV10 }
    }
    fn send_packet(&mut self, pkt: &Packet) -> Result<()> {
        if pkt.data.is_empty() {
            return Ok(());
        }

        let data = &pkt.data;
        if data.len() >= 9 && ((data[0] as usize) * 8 + 1 <= data.len()) {
            let slice_count = data[0] as usize + 1;
            let hdr_end = 1 + 8 * slice_count;
            let slices_hdr = &data[5..hdr_end];
            let buf = &data[hdr_end..];
            let buf_size = buf.len();

            let get_offset = |n: usize| -> usize {
                if n < slice_count {
                    let entry = &slices_hdr[n * 8..n * 8 + 4];
                    u32::from_le_bytes(entry.try_into().unwrap()) as usize
                } else {
                    buf_size
                }
            };

            let mut i = 0;
            while i < slice_count {
                let off = get_offset(i);
                if off >= buf_size {
                    return Err(Error::InvalidData("Slice offset overflow".to_string()));
                }
                let size = if i + 1 == slice_count {
                    buf_size - off
                } else {
                    get_offset(i + 1).saturating_sub(off)
                };
                let size2 = if i + 2 >= slice_count {
                    buf_size - off
                } else {
                    get_offset(i + 2).saturating_sub(off)
                };
                if size == 0 {
                    return Err(Error::InvalidData("Empty slice size".to_string()));
                }
                let max_size = size.max(size2);
                let slice_buf = &buf[off..(off + max_size).min(buf_size)];
                let ret = self.decode_slice(slice_buf, size, size2, buf_size, pkt.pts)?;
                if ret > 8 * size {
                    i += 1;
                }
                i += 1;
            }
        } else {
            self.decode_slice(data, data.len(), data.len(), data.len(), pkt.pts)?;
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
        self.s.mb_x = 0;
        self.s.mb_y = 0;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        None
    }
}

fn rv10_decode_picture_header(s: &mut MpegState, h: &mut H263State, gb: &mut GetBitContext) -> Result<usize> {
    let _marker = gb.get_bits1();
    if gb.get_bits1() != 0 {
        s.pict_type = 2; // P
    } else {
        s.pict_type = 1; // I
    }
    let pb_frame = gb.get_bits1();
    if pb_frame != 0 {
        return Err(Error::InvalidData("PB-frame not supported".to_string()));
    }
    let qscale = gb.get_bits(5);
    if qscale == 0 {
        return Err(Error::InvalidData("Invalid qscale 0".to_string()));
    }
    s.set_qscale(qscale);

    if s.pict_type == 1 {
        if h.rv10_version == 3 {
            h.last_dc[0] = gb.get_bits(8) as i32;
            h.last_dc[1] = gb.get_bits(8) as i32;
            h.last_dc[2] = gb.get_bits(8) as i32;
        }
    }

    let mb_xy = s.mb_x + s.mb_y * s.mb_width;
    let mb_count;
    if gb.show_bits(12) == 0 || (mb_xy > 0 && mb_xy < s.mb_width * s.mb_height) {
        s.mb_x = gb.get_bits(6) as usize;
        s.mb_y = gb.get_bits(6) as usize;
        mb_count = gb.get_bits(12) as usize;
    } else {
        s.mb_x = 0;
        s.mb_y = 0;
        mb_count = s.mb_width * s.mb_height;
    }
    gb.skip_bits(3);
    if s.mb_x >= s.mb_width || s.mb_y >= s.mb_height {
        return Err(Error::InvalidData("MB pos error".to_string()));
    }
    let total_mbs = s.mb_width * s.mb_height;
    let mb_xy = s.mb_y * s.mb_width + s.mb_x;
    Ok(mb_count.min(total_mbs.saturating_sub(mb_xy)))
}

fn rv20_decode_picture_header(
    s: &mut MpegState,
    h: &mut H263State,
    sub_id: u32,
    orig_w: usize,
    orig_h: usize,
    extradata: &[u8],
    gb: &mut GetBitContext,
) -> Result<usize> {
    const PICT_TYPES: [i32; 4] = [1, 1, 2, 3];
    let p_idx = gb.get_bits(2) as usize;
    s.pict_type = PICT_TYPES[p_idx];

    if gb.get_bits1() != 0 {
        return Err(Error::InvalidData("reserved bit set".to_string()));
    }

    let qscale = gb.get_bits(5);
    if qscale == 0 {
        return Err(Error::InvalidData("Invalid qscale 0".to_string()));
    }
    s.set_qscale(qscale);

    let minor = (sub_id >> 20) & 0xff;
    if minor >= 2 {
        h.loop_filter = gb.get_bits1() != 0;
    }

    let _seq = if minor <= 1 {
        (gb.get_bits(8) as i32) << 7
    } else {
        (gb.get_bits(13) as i32) << 2
    };

    let rpr_max = if extradata.len() > 1 { extradata[1] & 7 } else { 0 };
    if rpr_max != 0 {
        let rpr_bits = 32 - (rpr_max as u32).leading_zeros();
        let f = gb.get_bits(rpr_bits) as usize;
        let (new_w, new_h) = if f != 0 {
            if extradata.len() < 8 + 2 * f {
                return Err(Error::InvalidData("Extradata too small for RPR".to_string()));
            }
            (4 * extradata[6 + 2 * f] as usize, 4 * extradata[7 + 2 * f] as usize)
        } else {
            (orig_w, orig_h)
        };
        if new_w != s.width || new_h != s.height {
            s.width = new_w;
            s.height = new_h;
            s.mb_width = (new_w + 15) / 16;
            s.mb_height = (new_h + 15) / 16;
            s.mb_stride = s.mb_width + 1;
            s.b8_stride = s.mb_width * 2 + 1;
            s.linesize = s.mb_width * 16;
            s.uvlinesize = s.mb_width * 8;
        }
    }

    let total_mbs = s.mb_width * s.mb_height;
    let mb_pos = decode_mba(gb, total_mbs);
    if mb_pos >= total_mbs {
        return Err(Error::InvalidData("Invalid MBA".to_string()));
    }
    s.mb_x = mb_pos % s.mb_width;
    s.mb_y = mb_pos / s.mb_width;
    s.no_rounding = gb.get_bits1() != 0;

    if minor <= 1 && s.pict_type == 3 {
        gb.skip_bits(5);
    }

    s.h263_aic = s.pict_type == 1;
    if s.h263_aic {
        s.y_dc_scale = crate::h263tables::AIC_DC_SCALE_TABLE[s.qscale as usize] as u32;
        s.c_dc_scale = crate::h263tables::AIC_DC_SCALE_TABLE[s.qscale as usize] as u32;
    } else {
        s.y_dc_scale = crate::h263tables::MPEG1_DC_SCALE_TABLE[s.qscale as usize] as u32;
        s.c_dc_scale = crate::h263tables::MPEG1_DC_SCALE_TABLE[s.qscale as usize] as u32;
    }
    h.loop_filter = true;

    Ok(total_mbs - mb_pos)
}

fn decode_mba(gb: &mut GetBitContext, mb_num: usize) -> usize {
    const MBA_MAX: [usize; 6] = [47, 98, 395, 1583, 6335, 9215];
    const MBA_LENGTH: [u32; 6] = [6, 7, 9, 11, 13, 14];
    let mut i = 0;
    while i < 5 {
        if mb_num.saturating_sub(1) <= MBA_MAX[i] {
            break;
        }
        i += 1;
    }
    gb.get_bits(MBA_LENGTH[i]) as usize
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

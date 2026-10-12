// Ported from FFmpeg (commit 2da55bf): libavcodec/vp56.c
// (ff_vp56_decode_frame), vp5.c and vp6.c (the decoders' init: flip and
// alpha), and libavformat/flvdec.c (flv_set_video_codec: the VP6
// adjustment byte FFmpeg's FLV demuxer moves to the extradata).
// License: LGPL-2.1-or-later

//! The `vp5`, `vp6`, `vp6f` and `vp6a` decoders. VP5 and VP6 (AVI, EA)
//! store pictures bottom up; VP6F and VP6A (Flash) top down. VP6A packets
//! carry a second VP6 frame for the alpha plane.
//!
//! FLV keeps a byte before each VP6F/VP6A frame that FFmpeg's FLV demuxer
//! moves into the extradata, where the decoder crops the picture with it
//! (right by its high nibble, bottom by its low one). OxideAV's FLV demuxer
//! leaves the byte in the packet and names the codec without a container
//! tag, so a tagless `vp6f`/`vp6a` stream is read FFmpeg's FLV way: the
//! byte comes off each packet and serves as that extradata.

use std::collections::VecDeque;
use std::sync::Arc;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, PixelFormat, Result, VideoFrame, VideoPlane};

use crate::vp56::{Avctx, Flavor, Picture, Vp56};

/// The decoder of one of the four codec ids.
pub struct Vp56Decoder {
    codec_id: CodecId,
    flip: bool,
    has_alpha: bool,
    /// FLV packaging: an adjustment byte before each frame.
    flv: bool,
    avctx: Avctx,
    main: Vp56,
    alpha: Option<Vp56>,
    ready: VecDeque<VideoFrame>,
    /// The size of the frame last returned.
    returned: Option<(u32, u32)>,
    flushed: bool,
}

impl Vp56Decoder {
    /// The decoder for `params.codec_id`: `vp5`, `vp6`, `vp6f` or `vp6a`.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        let id = params.codec_id.as_str();
        let (flavor, flip, has_alpha) = match id {
            "vp5" => (Flavor::Vp5, true, false),
            "vp6" => (Flavor::Vp6, true, false),
            "vp6f" => (Flavor::Vp6, false, false),
            "vp6a" => (Flavor::Vp6, false, true),
            other => return Err(Error::unsupported(format!("vp56: no decoder for {other}"))),
        };
        let flv = matches!(id, "vp6f" | "vp6a") && params.tag.is_none();
        let dim = |v: Option<u32>| v.and_then(|v| i32::try_from(v).ok()).unwrap_or(0);
        let avctx = Avctx {
            width: dim(params.width),
            height: dim(params.height),
            coded_width: 0,
            coded_height: 0,
            extradata: if flv { vec![0] } else { params.extradata.clone() },
        };
        Ok(Self {
            codec_id: params.codec_id.clone(),
            flip,
            has_alpha,
            flv,
            avctx,
            main: Vp56::new(flavor, false),
            // FFmpeg's alpha context: VP6, flipped as the main one is not
            // (both unflipped for VP6A).
            alpha: has_alpha.then(|| Vp56::new(Flavor::Vp6, true)),
            ready: VecDeque::new(),
            returned: None,
            flushed: false,
        })
    }

    /// ff_vp56_decode_frame on one frame.
    fn decode(&mut self, buf: &[u8], pts: Option<i64>) -> Result<VideoFrame> {
        let invalid = |e: crate::vp56::Invalid| Error::invalid(e.0);
        let mut remaining = buf.len() as i64;
        let mut alpha_offset = remaining;
        let mut frame = buf;
        if self.has_alpha {
            if remaining < 3 {
                return Err(Error::invalid("vp6a: no alpha offset"));
            }
            alpha_offset = i64::from(u32::from_be_bytes([0, buf[0], buf[1], buf[2]]));
            frame = &buf[3..];
            remaining -= 3;
            if remaining < alpha_offset {
                return Err(Error::invalid("vp6a: alpha offset past the packet"));
            }
        }
        let size_change = self.main.parse_header(&mut self.avctx, frame, alpha_offset).map_err(invalid)?;
        if size_change {
            for ctx in std::iter::once(&mut self.main).chain(self.alpha.as_mut()) {
                ctx.prev = None;
                ctx.golden = None;
            }
            self.main.key = true;
        }
        // ff_get_buffer at the coded size.
        let (cw, ch) = (self.avctx.coded_width, self.avctx.coded_height);
        if cw <= 0 || ch <= 0 || self.avctx.width <= 0 || self.avctx.height <= 0 {
            return Err(Error::invalid("vp56: no picture size"));
        }
        let mut pic = Picture::new(cw as usize, ch as usize, self.has_alpha);
        if let Some(alpha) = self.alpha.as_mut() {
            alpha.key = self.main.key;
        }
        if size_change {
            self.main.size_changed(&self.avctx).map_err(invalid)?;
            if let Some(alpha) = self.alpha.as_mut() {
                alpha.size_changed(&self.avctx).map_err(invalid)?;
            }
        }
        if let Some(alpha) = self.alpha.as_mut() {
            let saved = self.avctx.clone();
            let alpha_frame = usize::try_from(alpha_offset).ok().and_then(|o| frame.get(o..)).unwrap_or(&[]);
            match alpha.parse_header(&mut self.avctx, alpha_frame, remaining - alpha_offset) {
                Ok(false) => {}
                Ok(true) => {
                    self.avctx = Avctx { extradata: std::mem::take(&mut self.avctx.extradata), ..saved };
                    return Err(Error::invalid("vp6a: alpha frame of another size"));
                }
                Err(e) => return Err(invalid(e)),
            }
        }
        self.main.discard_frame = false;
        let keep_main = self.main.decode_mbs(&mut pic);
        let keep_alpha = self.alpha.as_mut().map(|a| a.decode_mbs(&mut pic));
        let pic = Arc::new(pic);
        for (ctx, keep) in std::iter::once((&mut self.main, keep_main)).chain(self.alpha.as_mut().zip(keep_alpha)) {
            if keep {
                if ctx.key || ctx.golden_frame {
                    ctx.golden = Some(pic.clone());
                }
                ctx.prev = Some(pic.clone());
            }
        }
        if self.main.discard_frame {
            return Err(Error::invalid("vp56: damaged frame"));
        }
        Ok(self.output(&pic, pts))
    }

    /// The picture as FFmpeg returns it: its top-left `width` x `height`
    /// (in memory order: the bottom rows of the bitstream's picture,
    /// reversed, for the flipped codecs).
    fn output(&mut self, pic: &Picture, pts: Option<i64>) -> VideoFrame {
        let (w, h) = (self.avctx.width as usize, self.avctx.height as usize);
        let planes = if self.has_alpha { 4 } else { 3 };
        let out = (0..planes)
            .map(|p| {
                let (pw, ph) = if p == 1 || p == 2 { (w.div_ceil(2), h.div_ceil(2)) } else { (w, h) };
                let (stride, rows) = (pic.stride[p], pic.height[p]);
                let mut data = Vec::with_capacity(pw * ph);
                for i in 0..ph.min(rows) {
                    let r = if self.flip { rows - 1 - i } else { i };
                    let start = r * stride;
                    data.extend_from_slice(&pic.planes[p][start..start + pw.min(stride)]);
                }
                data.resize(pw * ph, 0);
                VideoPlane { stride: pw, data }
            })
            .collect();
        self.returned = Some((w as u32, h as u32));
        VideoFrame { pts, planes: out }
    }
}

impl Decoder for Vp56Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let data = &packet.data[..];
        let frame = if self.flv {
            let Some((&adjust, rest)) = data.split_first() else {
                return Err(Error::invalid("vp6: empty FLV packet"));
            };
            self.avctx.extradata = vec![adjust];
            rest
        } else {
            data
        };
        let frame = self.decode(frame, packet.pts)?;
        self.ready.push_back(frame);
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(f) => Ok(Frame::Video(f)),
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.flushed = true;
        Ok(())
    }

    /// After a seek: no frame to predict from until the next key frame.
    fn reset(&mut self) -> Result<()> {
        self.ready.clear();
        self.flushed = false;
        for ctx in std::iter::once(&mut self.main).chain(self.alpha.as_mut()) {
            ctx.prev = None;
            ctx.golden = None;
        }
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        Some(if self.has_alpha { PixelFormat::Yuva420P } else { PixelFormat::Yuv420P })
    }

    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        self.returned
    }
}

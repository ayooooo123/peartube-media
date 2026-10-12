// Ported from FFmpeg (commit 2da55bf); see each module for its files.
// License: LGPL-2.1-or-later

//! The `svq3` decoder (Sorenson Video 3, QuickTime `SVQ3`) for PearTube
//! media: libavcodec/svq3.c with the H.264 intra prediction, half-pel and
//! third-pel motion compensation and transforms it uses, including B
//! pictures and watermarked streams. It registers ahead of oxideav-svq's
//! `svq3`. Frames are full-range 4:2:0 (FFmpeg's yuvj420p), output in
//! FFmpeg's order: a reference picture when the next one arrives, a B
//! picture at once, the last reference at flush.
#![forbid(unsafe_code)]

mod decoder;
mod dsp;
mod getbits;
mod tables;

use std::collections::VecDeque;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder, Error, Frame, Packet, PixelFormat,
    Result, RuntimeContext, VideoFrame, VideoPlane,
};

/// Resolution and capability priority, as the other PearTube decoders.
const PRIORITY: i32 = 50;

/// Registers `svq3` under QuickTime's `SVQ3` sample entry (isom_tags.c).
pub fn register_codecs(reg: &mut CodecRegistry) {
    let mut caps = CodecCapabilities::video("svq3_pear_sw")
        .with_lossy(true)
        .with_intra_only(false)
        .with_priority(PRIORITY)
        .with_max_size(4096, 4096);
    caps.accepted_pixel_formats = vec![PixelFormat::YuvJ420P];
    reg.register(
        CodecInfo::new(CodecId::new("svq3"))
            .capabilities(caps)
            .with_resolution_priority(PRIORITY)
            .decoder(make_decoder)
            .tags([CodecTag::fourcc(b"SVQ3")]),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-svq3", register);

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Svq3::new(params)?))
}

/// The svq3 decoder.
pub struct Svq3 {
    id: CodecId,
    dec: decoder::Svq3Decoder,
    ready: VecDeque<VideoFrame>,
    eof: bool,
}

impl Svq3 {
    /// The decoder for the stream `params` describe: the SEQH header in
    /// its extradata (the ImageDescription), else its width and height.
    pub fn new(params: &CodecParameters) -> Result<Self> {
        Ok(Self {
            id: CodecId::new("svq3"),
            dec: decoder::Svq3Decoder::new(&params.extradata, params.width, params.height)?,
            ready: VecDeque::new(),
            eof: false,
        })
    }

    /// Queues pool picture `idx`, cropped to the stream's size.
    fn emit(&mut self, idx: usize) {
        let (w, h) = (self.dec.width, self.dec.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (ys, cs, y_origin, c_origin) = self.dec.strides();
        let (y, u, v, pts) = self.dec.picture(idx);
        let plane = |src: &[u8], origin: usize, stride: usize, pw: usize, ph: usize| {
            let mut data = Vec::with_capacity(pw * ph);
            for r in 0..ph {
                data.extend_from_slice(&src[origin + r * stride..origin + r * stride + pw]);
            }
            VideoPlane { stride: pw, data }
        };
        let planes = vec![plane(y, y_origin, ys, w, h), plane(u, c_origin, cs, cw, ch), plane(v, c_origin, cs, cw, ch)];
        self.ready.push_back(VideoFrame { pts, planes });
    }
}

impl Decoder for Svq3 {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        if let Some(idx) = self.dec.decode(&packet.data, packet.pts)? {
            self.emit(idx);
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(f) => Ok(Frame::Video(f)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    /// FFmpeg's empty packet: the last reference picture comes out.
    fn flush(&mut self) -> Result<()> {
        if !self.eof {
            self.eof = true;
            if let Some(idx) = self.dec.flush() {
                self.emit(idx);
            }
        }
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.dec.reset();
        self.ready.clear();
        self.eof = false;
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        Some(PixelFormat::YuvJ420P)
    }

    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        Some((self.dec.width as u32, self.dec.height as u32))
    }
}

// Ported from FFmpeg (commit 2da55bf); see each module for its files.
// License: LGPL-2.1-or-later

//! The `svq1` decoder (Sorenson Video 1, QuickTime `SVQ1`) for PearTube
//! media: libavcodec/svq1dec.c with hpeldsp's half-pel motion
//! compensation. It registers ahead of oxideav-svq's `svq1`. Frames are
//! 4:1:0 planar (FFmpeg's yuv410p), one per packet, as FFmpeg outputs
//! them; a packet FFmpeg rejects is an error and no frame.
#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod tables;

use std::collections::VecDeque;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder, Error, Frame, Packet, PixelFormat,
    Result, RuntimeContext, VideoFrame, VideoPlane,
};

/// Resolution and capability priority, as the other PearTube decoders.
const PRIORITY: i32 = 50;

/// Registers `svq1` under QuickTime's `SVQ1`, `svq1` and `svqi` sample
/// entries (isom_tags.c) and AVI's `svq1` (riff.c).
pub fn register_codecs(reg: &mut CodecRegistry) {
    let mut caps = CodecCapabilities::video("svq1_pear_sw")
        .with_lossy(true)
        .with_intra_only(false)
        .with_priority(PRIORITY)
        .with_max_size(4096, 4096);
    caps.accepted_pixel_formats = vec![PixelFormat::Yuv410P];
    reg.register(
        CodecInfo::new(CodecId::new("svq1"))
            .capabilities(caps)
            .with_resolution_priority(PRIORITY)
            .decoder(make_decoder)
            .tags([CodecTag::fourcc(b"SVQ1"), CodecTag::fourcc(b"svq1"), CodecTag::fourcc(b"svqi")]),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-svq1", register);

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(Svq1::new(params)))
}

/// The svq1 decoder.
pub struct Svq1 {
    id: CodecId,
    dec: decoder::Svq1Decoder,
    ready: VecDeque<VideoFrame>,
    eof: bool,
}

impl Svq1 {
    /// The decoder for the stream `params` describe. Only whether the
    /// stream has extradata matters to SVQ1 (it marks old streams whose
    /// inter means ±128 are swapped).
    pub fn new(params: &CodecParameters) -> Self {
        Self {
            id: CodecId::new("svq1"),
            dec: decoder::Svq1Decoder::new(&params.extradata, params.width.unwrap_or(0), params.height.unwrap_or(0)),
            ready: VecDeque::new(),
            eof: false,
        }
    }
}

/// The picture's planes cropped to its size: luma, then chroma at a
/// quarter of each dimension (rounded up).
fn frame(pic: &decoder::Picture, pts: Option<i64>) -> VideoFrame {
    let plane = |p: usize, w: usize, h: usize| {
        let (stride, samples) = &pic.planes[p];
        let mut data = Vec::with_capacity(w * h);
        for row in samples.chunks(*stride).take(h) {
            data.extend_from_slice(&row[..w]);
        }
        VideoPlane { stride: w, data }
    };
    let (w, h) = (pic.width, pic.height);
    let (cw, ch) = (w.div_ceil(4), h.div_ceil(4));
    VideoFrame { pts, planes: vec![plane(0, w, h), plane(1, cw, ch), plane(2, cw, ch)] }
}

impl Decoder for Svq1 {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.is_empty() {
            return Ok(());
        }
        let pic = self.dec.decode(&packet.data)?;
        self.ready.push_back(frame(pic, packet.pts));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.ready.pop_front() {
            Some(f) => Ok(Frame::Video(f)),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    /// svq1_flush: the reference picture is dropped.
    fn reset(&mut self) -> Result<()> {
        self.dec.flush();
        self.ready.clear();
        self.eof = false;
        Ok(())
    }

    fn output_pixel_format(&self) -> Option<PixelFormat> {
        Some(PixelFormat::Yuv410P)
    }

    fn output_video_dimensions(&self) -> Option<(u32, u32)> {
        Some(self.dec.dims)
    }
}

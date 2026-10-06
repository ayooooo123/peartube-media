//! FFmpeg's `AVSubtitle` as the decoder ports produce it, and its rendering
//! to the RGBA canvas frames `oxideav-sub-image` decoders emit.
//!
//! The canvas is what FFmpeg's sub2video paints for each `AVSubtitle`
//! (`fftools/ffmpeg_filter.c: sub2video_update` / `sub2video_copy_rect`,
//! commit 2da55bf, LGPL-2.1-or-later): a transparent picture the size the
//! decoder reports, with every bitmap rectangle's palette words copied to its
//! position, later rectangles over earlier ones, rectangles that do not fit
//! skipped. The generic end-time rule of `avcodec_decode_subtitle2`
//! (`libavcodec/decode.c`: a subtitle with rectangles and no end lasts the
//! packet's duration) is applied here too.

use std::collections::VecDeque;
use std::time::Duration;

use oxideav_core::{Error, Frame, Packet, Result, TimeBase, VideoFrame, VideoPlane};

/// Largest canvas side accepted from a stream.
pub(crate) const MAX_SIDE: usize = 16384;
/// Largest canvas accepted from a stream, in RGBA bytes.
pub(crate) const MAX_CANVAS_BYTES: usize = 256 << 20;

/// One `AVSubtitleRect` of type `SUBTITLE_BITMAP`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub linesize: usize,
    /// `data[0]`: palette indices, `linesize` bytes per row.
    pub pixels: Vec<u8>,
    /// `data[1]`: the 256-entry palette (`AVPALETTE_SIZE`), each word as
    /// the R, G, B, A bytes sub2video writes; entries past `nb_colors` are
    /// zero.
    pub palette: Vec<[u8; 4]>,
}

impl Rect {
    /// A rectangle with an all-zero 256-entry palette and no pixels.
    pub(crate) fn empty() -> Self {
        Self { palette: vec![[0; 4]; 256], ..Self::default() }
    }
}

/// One `AVSubtitle`.
#[derive(Clone, Debug)]
pub(crate) struct Subtitle {
    /// `AVSubtitle.pts` in the packet's time base; `None` for
    /// `AV_NOPTS_VALUE`.
    pub pts: Option<i64>,
    pub start_display_time: u32,
    /// Milliseconds from `pts`; `u32::MAX` means until the next subtitle.
    pub end_display_time: u32,
    pub rects: Vec<Rect>,
}

impl Subtitle {
    /// `get_subtitle_defaults` with the packet's pts, as
    /// `avcodec_decode_subtitle2` hands the decoder its `AVSubtitle`.
    pub(crate) fn for_packet(pts: Option<i64>) -> Self {
        Self { pts, start_display_time: 0, end_display_time: 0, rects: Vec::new() }
    }
}

/// Paints `sub` on a transparent `width x height` canvas the way
/// sub2video does.
pub(crate) fn paint(sub: &Subtitle, width: usize, height: usize) -> Vec<u8> {
    let mut canvas = vec![0u8; width * height * 4];
    for r in &sub.rects {
        let (x, y, w, h) = (i64::from(r.x), i64::from(r.y), i64::from(r.w), i64::from(r.h));
        if x < 0 || x + w > width as i64 || y < 0 || y + h > height as i64 {
            continue;
        }
        let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
        for row in 0..h {
            let Some(src) = r.pixels.get(row * r.linesize..row * r.linesize + w) else { break };
            let dst = &mut canvas[((y + row) * width + x) * 4..((y + row) * width + x + w) * 4];
            for (px, &index) in dst.chunks_exact_mut(4).zip(src) {
                px.copy_from_slice(&r.palette[usize::from(index)]);
            }
        }
    }
    canvas
}

/// Turns each decoded `AVSubtitle` into one RGBA canvas frame at its start,
/// carrying its display duration when the subtitle has an end.
pub(crate) struct CanvasFrames {
    width: usize,
    height: usize,
    frames: VecDeque<Frame>,
}

impl CanvasFrames {
    /// sub2video's initial canvas: the stream's declared size, else
    /// 720x576.
    pub(crate) fn new(width: Option<u32>, height: Option<u32>) -> Self {
        let pick = |v: Option<u32>, default: usize| v.filter(|&v| v > 0).map_or(default, |v| v as usize);
        Self { width: pick(width, 720), height: pick(height, 576), frames: VecDeque::new() }
    }

    /// Queues the canvas frame of `sub`, decoded from `packet` by a decoder
    /// whose `AVCodecContext` size is `decoder_width x decoder_height`.
    pub(crate) fn push(&mut self, mut sub: Subtitle, packet: &Packet, decoder_width: i32, decoder_height: i32) -> Result<()> {
        // avcodec_decode_subtitle2: rectangles without an end last the
        // packet's duration.
        if !sub.rects.is_empty() && sub.end_display_time == 0 {
            if let Some(d) = packet.duration.filter(|&d| d != 0) {
                sub.end_display_time = packet.time_base.rescale(d, TimeBase::new(1, 1000)) as u32;
            }
        }
        // sub2video keeps the previous canvas side when the decoder reports 0.
        if decoder_width > 0 {
            self.width = decoder_width as usize;
        }
        if decoder_height > 0 {
            self.height = decoder_height as usize;
        }
        if self.width > MAX_SIDE || self.height > MAX_SIDE || self.width * self.height * 4 > MAX_CANVAS_BYTES {
            return Err(Error::invalid("subtitle canvas exceeds the size cap"));
        }
        let ms = TimeBase::new(1, 1000);
        let pts = sub.pts.map(|p| p.saturating_add(ms.rescale(i64::from(sub.start_display_time), packet.time_base)));
        let canvas = paint(&sub, self.width, self.height);
        let mut frame = VideoFrame { pts, planes: vec![VideoPlane { stride: self.width * 4, data: canvas }] };
        if sub.end_display_time != u32::MAX {
            let shown = sub.end_display_time.saturating_sub(sub.start_display_time);
            frame.set_display_duration(Duration::from_millis(u64::from(shown)));
        }
        self.frames.push_back(Frame::Video(frame));
        Ok(())
    }

    pub(crate) fn pop(&mut self) -> Option<Frame> {
        self.frames.pop_front()
    }

    pub(crate) fn clear(&mut self) {
        self.frames.clear();
    }
}

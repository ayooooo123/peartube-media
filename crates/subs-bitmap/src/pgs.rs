//! HDMV Presentation Graphic Stream (PGS) subtitle decoder.
//!
//! Port of FFmpeg `libavcodec/pgssubdec.c` (commit 2da55bf; header: GNU
//! Lesser General Public License 2.1 or later), with the palette conversion
//! of `libavutil/colorspace.h` (same licence). The epoch state (palettes and
//! objects that live until an acquisition point), the 2-object presentation,
//! the run-length decoder and every error path follow the C code, including
//! what it does on malformed input: segments read past their end into the
//! rest of the packet, palette slots keep entries from earlier epochs, the
//! run-length data fills pixels linearly whatever the line markers say.
//! Each `AVSubtitle` becomes one RGBA canvas frame (see [`crate::subtitle`]).

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet};

use crate::bytes::Bytes;
use crate::colorspace::{Matrix, rgba, ycbcr_to_rgb};
use crate::subtitle::{CanvasFrames, MAX_CANVAS_BYTES, MAX_SIDE, Rect, Subtitle};

const MAX_EPOCH_PALETTES: usize = 8;
const MAX_EPOCH_OBJECTS: usize = 64;
const MAX_OBJECT_REFS: usize = 2;

const PALETTE_SEGMENT: u8 = 0x14;
const OBJECT_SEGMENT: u8 = 0x15;
const PRESENTATION_SEGMENT: u8 = 0x16;
const WINDOW_SEGMENT: u8 = 0x17;
const DISPLAY_SEGMENT: u8 = 0x80;

/// `AVERROR(ENOMEM)`: the one error that aborts a packet's decode.
struct OutOfMemory;

/// A segment parser's return: `Err(None)` for the errors the decode loop
/// ignores, `Err(Some(OutOfMemory))` for the one that ends the packet.
type SegmentResult<T = ()> = std::result::Result<T, Option<OutOfMemory>>;

#[derive(Clone, Copy, Default)]
struct ObjectRef {
    id: i32,
    composition_flag: u8,
    x: i32,
    y: i32,
}

#[derive(Default)]
struct Presentation {
    palette_id: i32,
    object_count: usize,
    objects: [ObjectRef; MAX_OBJECT_REFS],
    pts: Option<i64>,
}

#[derive(Default)]
struct Object {
    id: i32,
    w: i32,
    h: i32,
    /// `rle`: `None` for a NULL buffer. Holds the bytes received so far;
    /// the C buffer's tail past them reads as zeros here.
    rle: Option<Vec<u8>>,
    rle_remaining_len: u32,
}

struct Palette {
    id: i32,
    clut: [[u8; 4]; 256],
}

/// `PGSSubContext` plus the `AVCodecContext` size the decoder sets.
struct Context {
    presentation: Presentation,
    /// All `MAX_EPOCH_PALETTES` slots exist from the start; `flush_cache`
    /// only resets the count, so a reused slot keeps its old entries.
    palettes: Vec<Palette>,
    palette_count: usize,
    objects: Vec<Object>,
    object_count: usize,
    width: i32,
    height: i32,
}

impl Context {
    fn new(width: i32, height: i32) -> Self {
        Self {
            presentation: Presentation::default(),
            palettes: (0..MAX_EPOCH_PALETTES).map(|_| Palette { id: 0, clut: [[0; 4]; 256] }).collect(),
            palette_count: 0,
            objects: (0..MAX_EPOCH_OBJECTS).map(|_| Object::default()).collect(),
            object_count: 0,
            width,
            height,
        }
    }

    fn flush_cache(&mut self) {
        for object in &mut self.objects[..self.object_count] {
            object.rle = None;
            object.rle_remaining_len = 0;
        }
        self.object_count = 0;
        self.palette_count = 0;
    }

    fn find_object(&self, id: i32) -> Option<usize> {
        self.objects[..self.object_count].iter().position(|o| o.id == id)
    }

    fn find_palette(&self, id: i32) -> Option<usize> {
        self.palettes[..self.palette_count].iter().position(|p| p.id == id)
    }

    /// `ff_set_dimensions`: `av_image_check_size2` with no pixel format and
    /// the default `max_pixels`, then this crate's canvas caps.
    fn set_dimensions(&mut self, w: i32, h: i32) -> bool {
        let (wu, hu) = (w as u32 as u64, h as u32 as u64);
        let stride = 8 * wu + 1024;
        let ffmpeg_ok = wu != 0
            && hu != 0
            && wu <= i32::MAX as u64
            && hu <= i32::MAX as u64
            && stride < i32::MAX as u64
            && stride * (hu + 128) < i32::MAX as u64
            && wu * hu <= i32::MAX as u64;
        let capped = ffmpeg_ok && wu as usize <= MAX_SIDE && hu as usize <= MAX_SIDE && (wu * hu * 4) as usize <= MAX_CANVAS_BYTES;
        (self.width, self.height) = if capped { (w, h) } else { (0, 0) };
        capped
    }

    /// `parse_object_segment`.
    fn parse_object_segment(&mut self, packet: &[u8], at: usize, size: usize) -> SegmentResult {
        if size <= 4 {
            return Err(None);
        }
        let mut size = size - 4;
        let mut buf = Bytes::new(packet, at);
        let id = i32::from(buf.be16());
        let index = match self.find_object(id) {
            Some(i) => i,
            None => {
                if self.object_count >= MAX_EPOCH_OBJECTS {
                    return Err(None);
                }
                self.object_count += 1;
                self.objects[self.object_count - 1].id = id;
                self.object_count - 1
            }
        };
        let (width, height) = (self.width, self.height);
        let object = &mut self.objects[index];
        buf.skip(1); // object version
        let sequence_desc = buf.u8();

        if sequence_desc & 0x80 == 0 {
            // Additional RLE data.
            if size as u64 > u64::from(object.rle_remaining_len) {
                return Err(None);
            }
            if let Some(rle) = &mut object.rle {
                rle.extend((0..size).map(|i| packet.get(buf.pos() + i).copied().unwrap_or(0)));
            }
            object.rle_remaining_len -= size as u32;
            return Ok(());
        }

        if size <= 7 {
            return Err(None);
        }
        size -= 7;
        // Stored size includes the width/height fields.
        let rle_bitmap_len = buf.be24().wrapping_sub(4);
        if size as u64 > u64::from(rle_bitmap_len) {
            return Err(None);
        }
        let w = u32::from(buf.be16());
        let h = u32::from(buf.be16());
        if (width as u32) < w || (height as u32) < h || w == 0 || h == 0 {
            return Err(None);
        }
        object.w = w as i32;
        object.h = h as i32;
        // av_fast_padded_malloc of rle_bitmap_len + padding fails past
        // INT_MAX (a 24-bit size below 4 wraps to ~4 GiB).
        if u64::from(rle_bitmap_len) + 64 > i32::MAX as u64 {
            object.rle = None;
            object.rle_remaining_len = 0;
            return Err(Some(OutOfMemory));
        }
        object.rle = Some((0..size).map(|i| packet.get(buf.pos() + i).copied().unwrap_or(0)).collect());
        object.rle_remaining_len = rle_bitmap_len - size as u32;
        Ok(())
    }

    /// `parse_palette_segment`.
    fn parse_palette_segment(&mut self, packet: &[u8], at: usize, size: usize) -> SegmentResult {
        let mut buf = Bytes::new(packet, at);
        let end = at + size;
        let id = i32::from(buf.u8());
        let index = match self.find_palette(id) {
            Some(i) => i,
            None => {
                if self.palette_count >= MAX_EPOCH_PALETTES {
                    return Err(None);
                }
                self.palette_count += 1;
                self.palettes[self.palette_count - 1].id = id;
                self.palette_count - 1
            }
        };
        buf.skip(1); // palette version
        // Default to BT.709; BT.601 at 576 lines or fewer.
        let matrix = if self.height <= 0 || self.height > 576 { Matrix::Bt709 } else { Matrix::Bt601 };
        while buf.pos() < end {
            let color_id = buf.u8();
            let y = buf.u8();
            let cr = buf.u8();
            let cb = buf.u8();
            let alpha = buf.u8();
            let [r, g, b] = ycbcr_to_rgb(y, cb, cr, matrix);
            self.palettes[index].clut[usize::from(color_id)] = rgba(r, g, b, alpha);
        }
        Ok(())
    }

    /// `parse_presentation_segment`.
    fn parse_presentation_segment(&mut self, packet: &[u8], at: usize, size: usize, pts: Option<i64>) -> SegmentResult {
        let mut buf = Bytes::new(packet, at);
        let end = at + size;
        let w = i32::from(buf.be16());
        let h = i32::from(buf.be16());
        self.presentation.pts = pts;
        if !self.set_dimensions(w, h) {
            return Err(None);
        }
        buf.skip(1); // frame rate
        let _id_number = buf.be16();
        // Epoch boundaries: any state but "normal" releases objects and
        // palettes.
        let state = buf.u8() >> 6;
        if state != 0 {
            self.flush_cache();
        }
        buf.skip(1); // palette_update_flag
        self.presentation.palette_id = i32::from(buf.u8());
        self.presentation.object_count = usize::from(buf.u8());
        if self.presentation.object_count > MAX_OBJECT_REFS {
            self.presentation.object_count = 2;
        }
        for i in 0..self.presentation.object_count {
            if (end as i64) - (buf.pos() as i64) < 8 {
                self.presentation.object_count = i;
                return Err(None);
            }
            let object = &mut self.presentation.objects[i];
            object.id = i32::from(buf.be16());
            let _window_id = buf.u8();
            object.composition_flag = buf.u8();
            object.x = i32::from(buf.be16());
            object.y = i32::from(buf.be16());
            if object.composition_flag & 0x80 != 0 {
                // Cropping, read and not applied (FFmpeg's TODO).
                for _ in 0..4 {
                    buf.be16();
                }
            }
            if object.x > self.width || object.y > self.height {
                object.x = 0;
                object.y = 0;
            }
        }
        Ok(())
    }

    /// `display_end_segment`: `Ok(true)` when it produced a subtitle.
    fn display_end_segment(&mut self, sub: &mut Subtitle) -> SegmentResult<bool> {
        let pts = self.presentation.pts.or(sub.pts);
        *sub = Subtitle { pts, start_display_time: 0, end_display_time: u32::MAX, rects: Vec::new() };
        self.presentation.pts = None;
        if self.presentation.object_count == 0 {
            return Ok(true);
        }
        let Some(palette) = self.find_palette(self.presentation.palette_id) else {
            // avsubtitle_free zeroes the whole AVSubtitle, pts included.
            *sub = Subtitle { pts: Some(0), start_display_time: 0, end_display_time: 0, rects: Vec::new() };
            return Err(None);
        };
        for i in 0..self.presentation.object_count {
            let reference = self.presentation.objects[i];
            let mut rect = Rect::empty();
            rect.palette.copy_from_slice(&self.palettes[palette].clut);
            let Some(index) = self.find_object(reference.id) else {
                sub.rects.push(rect);
                continue;
            };
            rect.x = reference.x;
            rect.y = reference.y;
            let object = &self.objects[index];
            if let Some(rle) = &object.rle {
                rect.w = object.w;
                rect.h = object.h;
                rect.linesize = object.w as usize;
                match decode_rle(rle, object.w as usize, object.h as usize) {
                    Some(pixels) => rect.pixels = pixels,
                    None => {
                        rect.w = 0;
                        rect.h = 0;
                    }
                }
            }
            sub.rects.push(rect);
        }
        Ok(true)
    }

    /// `decode`: the subtitle the packet completes, if any. `Err` is the
    /// packet's error return (too short, or out of memory).
    fn decode(&mut self, packet: &[u8], pts: Option<i64>) -> std::result::Result<Option<Subtitle>, ()> {
        let mut sub = Subtitle::for_packet(pts);
        let mut got = false;
        if packet.len() < 3 {
            return Err(());
        }
        let end = packet.len();
        let mut buf = Bytes::new(packet, 0);
        while buf.pos() < end {
            let segment_type = buf.u8();
            let segment_length = usize::from(buf.be16());
            if segment_type != DISPLAY_SEGMENT && segment_length as i64 > end as i64 - buf.pos() as i64 {
                break;
            }
            let at = buf.pos();
            let ret = match segment_type {
                PALETTE_SEGMENT => self.parse_palette_segment(packet, at, segment_length),
                OBJECT_SEGMENT => self.parse_object_segment(packet, at, segment_length),
                PRESENTATION_SEGMENT => self.parse_presentation_segment(packet, at, segment_length, sub.pts),
                WINDOW_SEGMENT => Ok(()),
                DISPLAY_SEGMENT => {
                    if got {
                        Err(None)
                    } else {
                        self.display_end_segment(&mut sub).map(|g| got = g)
                    }
                }
                _ => Err(None),
            };
            if let Err(Some(OutOfMemory)) = ret {
                return Err(());
            }
            buf.skip(segment_length);
        }
        Ok(got.then_some(sub))
    }
}

/// `decode_rle`: `None` when the data leaves pixels unset.
fn decode_rle(rle: &[u8], w: usize, h: usize) -> Option<Vec<u8>> {
    let area = w * h;
    let mut pixels = vec![0u8; area];
    let mut pixel_count = 0usize;
    let mut line_count = 0usize;
    let mut buf = Bytes::new(rle, 0);
    while buf.pos() < rle.len() && line_count < h {
        let mut color = buf.u8();
        let mut run = 1usize;
        if color == 0 {
            let flags = buf.u8();
            run = usize::from(flags & 0x3f);
            if flags & 0x40 != 0 {
                run = (run << 8) + usize::from(buf.u8());
            }
            color = if flags & 0x80 != 0 { buf.u8() } else { 0 };
        }
        if run > 0 && pixel_count + run <= area {
            pixels[pixel_count..pixel_count + run].fill(color);
            pixel_count += run;
        } else if run == 0 {
            // New line; misaligned lines are only logged.
            line_count += 1;
        }
    }
    (pixel_count >= area).then_some(pixels)
}

/// The PGS decoder behind codec ids `hdmv_pgs_subtitle` and `pgs`.
pub(crate) struct PgsDecoder {
    codec_id: CodecId,
    ctx: Context,
    canvas: CanvasFrames,
}

pub(crate) fn make_decoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    let size = |v: Option<u32>| v.and_then(|v| i32::try_from(v).ok()).unwrap_or(0);
    let mut ctx = Context::new(0, 0);
    // avcodec_parameters_to_context copies the stream's size.
    let (w, h) = (size(params.width), size(params.height));
    if w > 0 && h > 0 {
        ctx.set_dimensions(w, h);
    }
    Ok(Box::new(PgsDecoder {
        codec_id: params.codec_id.clone(),
        ctx,
        canvas: CanvasFrames::new(params.width, params.height),
    }))
}

impl Decoder for PgsDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> oxideav_core::Result<()> {
        match self.ctx.decode(&packet.data, packet.pts) {
            Ok(Some(sub)) => self.canvas.push(sub, packet, self.ctx.width, self.ctx.height),
            Ok(None) => Ok(()),
            Err(()) => Err(Error::invalid("PGS: undecodable packet")),
        }
    }

    fn receive_frame(&mut self) -> oxideav_core::Result<Frame> {
        self.canvas.pop().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> oxideav_core::Result<()> {
        Ok(())
    }

    fn reset(&mut self) -> oxideav_core::Result<()> {
        // FFmpeg's PGS decoder has no flush callback: the epoch state
        // survives a seek, queued output does not.
        self.canvas.clear();
        Ok(())
    }
}

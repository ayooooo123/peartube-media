//! CVD and Philips OGT/SVCD subtitles, ported from VLC cvdsub.c,
//! svcdsub.c and video_chroma/yuvp.c at
//! 2e358f3098c2f2b7621d1dc568de8b61ad786322.
//!
//! Original copyright 2003, 2004, 2008 VLC authors and VideoLAN;
//! Rocky Bernstein, Gildas Bazin, Julio Sanchez Fernandez, Laurent Aimar.
//! The original headers are LGPL-2.1-or-later, without any warranty.
//! Input retains the MPEG private-stream prefix: one byte for CVD,
//! five bytes (0x70, channel, fragment, image ID) for OGT.
//!
//! Valid streams decode as VLC decodes them, including its reading of
//! truncated data as zeros and its clipping of regions at the canvas
//! edges. Deliberate differences, only on damaged input: the decoder
//! starts zeroed (VLC's CVD state is uninitialised memory); sizes are
//! capped; an OGT header is read from the assembled unit, not only its
//! first packet; and an unfinished OGT image is replaced by a new one, an
//! orphan continuation rejected, where VLC concatenates or reuses the
//! previous header.

use std::{borrow::Cow, time::Duration};
use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};
use crate::subtitle::{CanvasFrames, MAX_CANVAS_BYTES, MAX_SIDE, Rect, Subtitle};

const MAX_SPU: usize = u16::MAX as usize + 4;
fn invalid() -> Error { Error::invalid("CVD/OGT: malformed subtitle packet") }
fn be16(data: &[u8], at: usize) -> Result<usize> {
    let bytes = data.get(at..at + 2).ok_or_else(invalid)?;
    Ok(usize::from(u16::from_be_bytes([bytes[0], bytes[1]])))
}
struct Bits<'a> { data: &'a [u8], at: usize }
impl Bits<'_> {
    /// vlc_bits.h `bs_read`: bits beyond the buffer read as zero.
    fn get(&mut self, count: usize) -> u8 {
        let mut value = 0;
        for _ in 0..count {
            let bit = self.data.get(self.at / 8).map_or(0, |byte| (byte >> (7 - self.at % 8)) & 1);
            value = (value << 1) | bit;
            self.at += 1;
        }
        value
    }
    fn align(&mut self) { self.at = self.at.div_ceil(8) * 8; }
}

/// VLC yuvp.c's fixed-point conversion, including zeroing the RGB of a
/// completely transparent palette entry. Not FFmpeg's colour matrix.
fn rgba([y, cb, cr, alpha]: [u8; 4]) -> [u8; 4] {
    if alpha == 0 { return [0; 4]; }
    let y = (i32::from(y) - 16) * 1192;
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    let clamp = |v: i32| (v >> 10).clamp(0, 255) as u8;
    [clamp(y + 1634 * cr + 512), clamp(y - 401 * cb - 832 * cr + 512), clamp(y + 2066 * cb + 512), alpha]
}

#[derive(Default)]
struct Image {
    x: usize, y: usize, width: usize, height: usize,
    duration: u32,
    palette: [[u8; 4]; 4],
}
fn allocate(image: &Image) -> Result<Vec<u8>> {
    if image.width == 0 || image.height == 0 || image.width > MAX_SIDE || image.height > MAX_SIDE || image.width * image.height * 4 > MAX_CANVAS_BYTES { return Err(invalid()); }
    Ok(vec![0; image.width * image.height])
}
fn emit(canvas: &mut CanvasFrames, image: &Image, pixels: &[u8], packet: &Packet, pts: Option<i64>) -> Result<()> {
    let palette = image.palette.map(rgba);
    let mut sub = Subtitle::for_packet(pts);
    // VLC's zero duration is ephemeral, not a zero-length FFmpeg cue.
    sub.end_display_time = u32::MAX;
    // VLC blends the part of a region inside the video; sub2video's
    // rejection of a whole overflowing rectangle does not apply.
    let (width, height) = canvas.size();
    let visible = (image.width.min(width.saturating_sub(image.x)), image.height.min(height.saturating_sub(image.y)));
    sub.rects.push(Rect { x: image.x as i32, y: image.y as i32, w: visible.0 as i32, h: visible.1 as i32, linesize: image.width, pixels: Cow::Borrowed(pixels), palette: Cow::Borrowed(&palette) });
    let mut frame = canvas.render(sub, packet, 0, 0)?;
    if image.duration != 0 {
        let Frame::Video(video) = &mut frame else { unreachable!() };
        // FROM_SCALE_NZ in VLC uses integer microseconds, not rounded ms.
        video.set_display_duration(Duration::from_micros(u64::from(image.duration) * 1_000_000 / 90_000));
    }
    canvas.queue(frame);
    Ok(())
}

fn cvd_image(data: &[u8], image: &mut Image) -> Result<Vec<u8>> {
    // ParseHeader/ParseMetaInfo: four-byte fields between the metadata
    // offset and the SPU size; absent fields keep their previous values.
    let end = be16(data, 0)? + 4;
    let meta = be16(data, 2)?;
    for field in data.get(meta.min(end)..end).unwrap_or_default().chunks_exact(4) {
        match field[0] {
            0x04 => image.duration = u32::from_be_bytes([0, field[1], field[2], field[3]]),
            0x17 | 0x1f => {
                let x = (usize::from(field[1] & 15) << 6) + usize::from(field[2] >> 2);
                let y = (usize::from(field[2] & 3) << 8) + usize::from(field[3]);
                if field[0] == 0x17 { image.x = x; image.y = y; }
                else {
                    image.width = x.checked_sub(image.x).map_or(0, |v| v + 1);
                    image.height = y.checked_sub(image.y).map_or(0, |v| v + 1);
                }
            }
            0x24..=0x27 => image.palette[usize::from(field[0] - 0x24)][..3].copy_from_slice(&field[1..]),
            0x37 => {
                image.palette[0][3] = (field[3] & 15) << 4;
                image.palette[1][3] = (field[3] >> 4) << 4;
                image.palette[2][3] = (field[2] & 15) << 4;
                image.palette[3][3] = (field[2] >> 4) << 4;
            }
            // VLC's renderer ignores highlight/field-offset metadata.
            _ => {}
        }
    }
    let mut pixels = allocate(image)?;
    // RenderImage reads both fields sequentially from byte 4 to the end of
    // the assembled SPU, byte-aligning each row; it needs image data.
    let mut bits = Bits { data: data.get(4..).filter(|rest| !rest.is_empty()).ok_or_else(invalid)?, at: 0 };
    for field in 0..2 {
        for y in (field..image.height).step_by(2) {
            let mut x = 0;
            while x < image.width {
                let value = bits.get(4);
                // A zero nibble fills the rest of the row with the next
                // nibble's colour. Colours 4..=15 have no palette entry,
                // which yuvp.c skips: they stay transparent.
                let (count, color) = if value == 0 { (image.width - x, bits.get(4)) } else { (usize::from(value >> 2).min(image.width - x), value & 3) };
                pixels[y * image.width + x..y * image.width + x + count].fill(color);
                x += count;
            }
            bits.align();
        }
    }
    Ok(pixels)
}

fn ogt_image(data: &[u8]) -> Result<(Image, Vec<u8>)> {
    if data.len() < 4 { return Err(invalid()); }
    let mut at = 4;
    let mut image = Image::default();
    if data[2] & 8 != 0 {
        image.duration = u32::from_be_bytes(data.get(at..at + 4).ok_or_else(invalid)?.try_into().unwrap()); at += 4;
    }
    image.x = be16(data, at)?; image.y = be16(data, at + 2)?;
    image.width = be16(data, at + 4)?; image.height = be16(data, at + 6)?; at += 8;
    for (color, wire) in image.palette.iter_mut().zip(data.get(at..at + 16).ok_or_else(invalid)?.chunks_exact(4)) {
        *color = [wire[0], wire[2], wire[1], wire[3]];
    }
    at += 16;
    let command = *data.get(at).ok_or_else(invalid)?; at += 1;
    if command != 0 { at += 4; }
    let second = be16(data, at)?; at += 2;
    let mut pixels = allocate(&image)?;
    for field in 0..2 {
        let start = at + if field == 0 { 0 } else { second };
        let mut bits = Bits { data: data.get(start..).unwrap_or_default(), at: 0 };
        for y in (field..image.height).step_by(2) {
            let mut x = 0;
            while x < image.width {
                let color = bits.get(2);
                let count = if color == 0 { usize::from(bits.get(2)) + 1 } else { 1 };
                let count = count.min(image.width - x);
                pixels[y * image.width + x..y * image.width + x + count].fill(color);
                x += count;
            }
            bits.align();
        }
    }
    Ok((image, pixels))
}

struct VcdDecoder {
    codec: CodecId, cvd: bool, image: Image,
    pending: Vec<u8>, pending_pts: Option<i64>, pending_size: usize,
    /// OGT: an image's first packet has arrived.
    started: bool, canvas: CanvasFrames,
}
impl VcdDecoder {
    fn display(&mut self, data: &[u8], packet: &Packet, pts: Option<i64>) -> Result<()> {
        if self.cvd {
            let pixels = cvd_image(data, &mut self.image)?;
            // CVD's legacy coordinate convention adjusts x only, not width.
            let x = self.image.x;
            self.image.x = x * 3 / 4;
            let result = emit(&mut self.canvas, &self.image, &pixels, packet, pts);
            self.image.x = x;
            result
        } else {
            let (image, pixels) = ogt_image(data)?;
            emit(&mut self.canvas, &image, &pixels, packet, pts)
        }
    }
    fn packet(&mut self, packet: &Packet) -> Result<()> {
        let data = &packet.data;
        let (body, complete) = if self.cvd {
            if data.first().is_none_or(|&id| id > 3) { return Err(invalid()); }
            let body = &data[1..];
            if self.pending.is_empty() {
                // Reassemble/ParseHeader: a new SPU needs a PTS and its
                // four-byte size and metadata-offset header.
                if packet.pts.is_none() || body.len() < 4 { return Err(invalid()); }
                self.pending_size = be16(body, 0)? + 4;
                self.pending_pts = packet.pts;
            }
            (body, self.pending.len() + body.len() >= self.pending_size)
        } else {
            if data.len() < 5 || data[0] != 0x70 { return Err(invalid()); }
            // Packet 0 starts an image; bit 7 marks its last packet. Like
            // VLC, other packet and image numbers are not checked. Unlike
            // VLC, which appends a new image to an unfinished one and decodes
            // an orphan continuation with the previous header, a new image
            // replaces an unfinished one and an orphan is rejected.
            if data[2] & 0x7f == 0 {
                self.pending.clear(); self.pending_pts = packet.pts; self.started = true;
            } else if !self.started { return Err(invalid()); }
            (&data[5..], data[2] & 0x80 != 0)
        };
        if body.len() > MAX_SPU - self.pending.len() { return Err(invalid()); }
        if complete && self.pending.is_empty() {
            self.started = false;
            return self.display(body, packet, self.pending_pts);
        }
        self.pending.extend_from_slice(body);
        if complete {
            let mut data = std::mem::take(&mut self.pending);
            let result = self.display(&data, packet, self.pending_pts);
            data.clear(); self.pending = data; self.started = false;
            result?;
        }
        Ok(())
    }
}
impl Decoder for VcdDecoder {
    fn codec_id(&self) -> &CodecId { &self.codec }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let result = self.packet(packet);
        if result.is_err() { self.pending.clear(); self.pending_pts = None; self.started = false; }
        result
    }
    fn receive_frame(&mut self) -> Result<Frame> { self.canvas.pop().ok_or(Error::NeedMore) }
    fn flush(&mut self) -> Result<()> { self.pending.clear(); self.started = false; Ok(()) }
    fn reset(&mut self) -> Result<()> {
        self.flush()?; self.pending_pts = None; self.image = Image::default(); self.canvas.clear(); Ok(())
    }
}
fn make(params: &CodecParameters, cvd: bool) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(VcdDecoder { codec: params.codec_id.clone(), cvd, image: Image::default(), pending: Vec::new(), pending_pts: None, pending_size: 0, started: false, canvas: CanvasFrames::new(params.width, params.height) }))
}
pub(crate) fn make_cvd(params: &CodecParameters) -> Result<Box<dyn Decoder>> { make(params, true) }
pub(crate) fn make_ogt(params: &CodecParameters) -> Result<Box<dyn Decoder>> { make(params, false) }

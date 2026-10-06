//! DVD/HD subpictures, ported from FFmpeg libavcodec/dvdsubdec.c and
//! dvdsub.c at 2da55bf (both headers verified LGPL-2.1-or-later).
//! Default FFmpeg behavior: all subtitles, palette from codec extradata,
//! guessed grayscale otherwise; no implicit filesystem/IFO access.

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result};
use crate::colorspace::{Matrix, ycbcr_to_rgb};
use crate::subtitle::{CanvasFrames, MAX_CANVAS_BYTES, MAX_SIDE, Rect, Subtitle};

fn invalid() -> Error { Error::invalid("DVD subtitle: malformed subpicture") }

struct Bits<'a> { data: &'a [u8], at: usize }
impl Bits<'_> {
    fn get(&mut self, count: usize) -> usize {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | usize::from((self.data.get(self.at / 8).copied().unwrap_or(0) >> (7 - self.at % 8)) & 1);
            self.at += 1;
        }
        value
    }
}

fn offset(data: &[u8], at: usize, size: usize) -> Result<usize> {
    let bytes = data.get(at..at.checked_add(size).ok_or_else(invalid)?).ok_or_else(invalid)?;
    Ok(bytes.iter().fold(0usize, |value, &byte| (value << 8) | usize::from(byte)))
}

fn rle(pixels: &mut [u8], width: usize, height: usize, field: usize, data: &[u8], start: usize, eight: bool) -> Result<()> {
    if start >= data.len() { return Err(invalid()); }
    let mut bits = Bits { data: &data[start..], at: 0 };
    for y in (field..height).step_by(2) {
        let mut x = 0;
        while x < width {
            if bits.at > bits.data.len() * 8 { return Err(invalid()); }
            let (run, color) = if eight {
                let has_run = bits.get(1) != 0;
                let depth = if bits.get(1) == 0 { 2 } else { 8 };
                let color = bits.get(depth);
                let run = if !has_run { 1 } else if bits.get(1) == 0 { bits.get(3) + 2 } else {
                    match bits.get(7) { 0 => usize::MAX, run => run + 9 }
                };
                (run, color)
            } else {
                let mut value = 0;
                for threshold in [1, 4, 16, 64] {
                    if value >= threshold { break; }
                    value = (value << 4) | bits.get(4);
                }
                (if value < 4 { usize::MAX } else { value >> 2 }, value & 3)
            };
            if run != usize::MAX && run > width - x { return Err(invalid()); }
            let run = run.min(width - x);
            pixels[y * width + x..y * width + x + run].fill(color as u8);
            x += run;
        }
        bits.at = bits.at.div_ceil(8) * 8;
    }
    Ok(())
}

struct DvdDecoder {
    codec_id: CodecId,
    palette: Option<[[u8; 3]; 16]>,
    colormap: [u8; 4],
    alpha: [u8; 256],
    pending: Vec<u8>,
    width: i32,
    height: i32,
    canvas: CanvasFrames,
}

enum Decoded { More, Empty, Subtitle(Subtitle<'static>) }

impl DvdDecoder {
    fn palette(&self) -> Vec<[u8; 4]> {
        let mut out = vec![[0; 4]; 256];
        if let Some(palette) = &self.palette {
            for (i, color) in out[..4].iter_mut().enumerate() {
                let [r, g, b] = palette[usize::from(self.colormap[i])];
                *color = [r, g, b, self.alpha[i].wrapping_mul(17)];
            }
        } else {
            let mut used = [false; 16];
            for i in 0..4 { if self.alpha[i] != 0 { used[usize::from(self.colormap[i])] = true; } }
            let count = used.iter().filter(|&&v| v).count();
            if count == 0 { return out; }
            let levels: &[u8] = match count { 1 => &[255], 2 => &[0, 255], 3 => &[0, 128, 255], _ => &[0, 85, 170, 255] };
            let mut chosen = [None; 16];
            let mut next = 0;
            for i in 0..4 {
                if self.alpha[i] == 0 { continue; }
                let entry = &mut chosen[usize::from(self.colormap[i])];
                let level = *entry.get_or_insert_with(|| { let v = ((255u16 * u16::from(levels[next])) >> 8) as u8; next += 1; v });
                out[i] = [level, level, level, self.alpha[i].wrapping_mul(17)];
            }
        }
        out
    }

    fn decode(&mut self, data: &[u8], pts: Option<i64>) -> Result<Decoded> {
        if data.len() < 10 { return Ok(Decoded::Empty); }
        let big = data[..2] == [0, 0];
        let size_bytes = if big { 4 } else { 2 };
        let size = offset(data, if big { 2 } else { 0 }, size_bytes)?;
        let mut command = offset(data, if big { 6 } else { 2 }, size_bytes)?;
        if command > data.len() - 2 - size_bytes {
            return Ok(if command > size { Decoded::Empty } else { Decoded::More });
        }
        let mut subtitle = Subtitle::for_packet(pts);
        let mut menu = false;
        let mut eight = false;
        let mut yuv = None;
        while command > 0 && command < data.len() - 2 - size_bytes {
            let date = offset(data, command, 2)? as u32;
            let next = offset(data, command + 2, size_bytes)?;
            let mut at = command + 2 + size_bytes;
            let (mut first, mut second) = (None, None);
            let (mut x1, mut y1, mut x2, mut y2) = (0i32, 0i32, 0i32, 0i32);
            while at < data.len() {
                let code = data[at]; at += 1;
                match code {
                    0 => menu = true,
                    1 => subtitle.start_display_time = (date << 10) / 90,
                    2 => subtitle.end_display_time = (date << 10) / 90,
                    3 | 4 => {
                        let value = offset(data, at, 2)?;
                        for i in 0..4 {
                            let value = ((value >> (4 * i)) & 15) as u8;
                            if code == 3 { self.colormap[i] = value; } else { self.alpha[i] = value; }
                        }
                        at += 2;
                    }
                    5 | 0x85 => {
                        let bytes = data.get(at..at + 6).ok_or_else(invalid)?;
                        x1 = (i32::from(bytes[0]) << 4) | i32::from(bytes[1] >> 4);
                        x2 = (i32::from(bytes[1] & 15) << 8) | i32::from(bytes[2]);
                        y1 = (i32::from(bytes[3]) << 4) | i32::from(bytes[4] >> 4);
                        y2 = (i32::from(bytes[4] & 15) << 8) | i32::from(bytes[5]);
                        eight |= code == 0x85;
                        at += 6;
                    }
                    6 | 0x86 => {
                        let size = if code == 6 { 2 } else { 4 };
                        first = Some(offset(data, at, size)?);
                        second = Some(offset(data, at + size, size)?);
                        at += size * 2;
                    }
                    0x83 => { yuv = Some(data.get(at..at + 768).ok_or_else(invalid)?); at += 768; }
                    0x84 => {
                        let bytes = data.get(at..at + 256).ok_or_else(invalid)?;
                        for (alpha, byte) in self.alpha.iter_mut().zip(bytes) { *alpha = 255 - byte; }
                        at += 256;
                    }
                    _ => break,
                }
            }
            if let (Some(first), Some(second)) = (first, second) {
                if first >= data.len() || second >= data.len() { return Err(invalid()); }
                let width = (x2 - x1 + 1).max(0) as usize;
                let height = (y2 - y1 + 1).max(0) as usize;
                if width > MAX_SIDE || height > MAX_SIDE || width * height * 4 > MAX_CANVAS_BYTES { return Err(invalid()); }
                if width > 0 && height > 1 {
                    let mut pixels = vec![0; width * height];
                    rle(&mut pixels, width, height, 0, data, first, eight)?;
                    rle(&mut pixels, width, height, 1, data, second, eight)?;
                    let palette = if eight {
                        let mut palette = vec![[0; 4]; 256];
                        for (i, triple) in yuv.ok_or_else(invalid)?.chunks_exact(3).enumerate() {
                            let [r, g, b] = ycbcr_to_rgb(triple[0], triple[2], triple[1], Matrix::Bt601);
                            palette[i] = [r, g, b, self.alpha[i]];
                        }
                        palette
                    } else { self.palette() };
                    subtitle.rects.clear();
                    subtitle.rects.push(Rect { x: x1, y: y1, w: width as i32, h: height as i32, linesize: width, pixels: pixels.into(), palette: palette.into() });
                }
            }
            if next <= command { break; }
            command = next;
        }
        let Some(rect) = subtitle.rects.first_mut() else { return Ok(Decoded::Empty) };
        if !menu {
            let (width, height) = (rect.w as usize, rect.h as usize);
            let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
            for y in 0..height {
                for x in 0..width {
                    if rect.palette[usize::from(rect.pixels[y * width + x])][3] != 0 {
                        left = left.min(x); top = top.min(y); right = right.max(x + 1); bottom = bottom.max(y + 1);
                    }
                }
            }
            if left == width { return Ok(Decoded::Empty); }
            let cropped_width = right - left;
            let pixels = rect.pixels.to_mut();
            for y in top..bottom { pixels.copy_within(y * width + left..y * width + right, (y - top) * cropped_width); }
            pixels.truncate(cropped_width * (bottom - top));
            rect.x += left as i32; rect.y += top as i32;
            rect.w = cropped_width as i32; rect.h = (bottom - top) as i32; rect.linesize = cropped_width;
        }
        Ok(Decoded::Subtitle(subtitle))
    }
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let mut palette = None;
    let (mut width, mut height) = (params.width.unwrap_or(0) as i32, params.height.unwrap_or(0) as i32);
    for line in String::from_utf8_lossy(&params.extradata).split(['\n', '\r']) {
        if let Some(value) = line.strip_prefix("palette:") {
            let mut colors = [[0; 3]; 16];
            for (color, hex) in colors.iter_mut().zip(value.split(|c: char| c == ',' || c.is_ascii_whitespace()).filter(|v| !v.is_empty())) {
                let rgb = u32::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0);
                *color = [(rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8];
            }
            palette = Some(colors);
        } else if let Some(value) = line.strip_prefix("size:") {
            if let Some((w, h)) = value.trim().split_once('x') {
                width = w.trim().parse().map_err(|_| invalid())?;
                height = h.trim().parse().map_err(|_| invalid())?;
                if width <= 0 || height <= 0 || width as usize > MAX_SIDE || height as usize > MAX_SIDE || width as usize * height as usize * 4 > MAX_CANVAS_BYTES { return Err(invalid()); }
            }
        }
    }
    Ok(Box::new(DvdDecoder { codec_id: params.codec_id.clone(), palette, colormap: [0; 4], alpha: [0; 256], pending: Vec::new(), width, height, canvas: CanvasFrames::new(params.width, params.height) }))
}

impl Decoder for DvdDecoder {
    fn codec_id(&self) -> &CodecId { &self.codec_id }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut cached = std::mem::take(&mut self.pending);
        let data = if cached.is_empty() { packet.data.as_slice() } else {
            if packet.data.len() >= 65536 - cached.len() { return Err(invalid()); }
            cached.extend_from_slice(&packet.data); &cached
        };
        match self.decode(data, packet.pts)? {
            Decoded::More => {
                if cached.is_empty() {
                    if packet.data.len() >= 65536 { return Err(invalid()); }
                    cached.extend_from_slice(&packet.data);
                }
                self.pending = cached;
            }
            Decoded::Empty => { cached.clear(); self.pending = cached; }
            Decoded::Subtitle(subtitle) => {
                cached.clear(); self.pending = cached;
                self.canvas.push(subtitle, packet, self.width, self.height)?;
            }
        }
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> { self.canvas.pop().ok_or(Error::NeedMore) }
    fn flush(&mut self) -> Result<()> { Ok(()) }
    fn reset(&mut self) -> Result<()> { self.pending.clear(); self.canvas.clear(); Ok(()) }
}

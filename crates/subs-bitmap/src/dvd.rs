//! DVD/HD subpictures, ported from FFmpeg libavcodec/dvdsubdec.c and
//! dvdsub.c at 2da55bf (both headers verified LGPL-2.1-or-later).
//! Default FFmpeg behavior: all subtitles, palette from codec extradata,
//! guessed grayscale otherwise; no implicit filesystem/IFO access. The
//! extradata is read as dvdsub_parse_extradata reads it: `size:` with
//! sscanf("%dx%d"), the palette with strtoul. Deliberate difference: a
//! canvas (`size:`, the stream's or a subpicture's own) larger than
//! 4096x4096 is refused where FFmpeg would take it.

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

/// What decoding a subpicture unit leaves.
enum Decoded {
    /// The unit continues in the next packet.
    More,
    /// No subtitle, and dvdsub_decode drops its reassembly buffer.
    Empty,
    /// No subtitle, but dvdsub_decode keeps the buffer it reassembled: a
    /// unit it discards, or one without a visible pixel. A packet that
    /// stood alone left nothing to keep.
    Nothing,
    Subtitle(Subtitle<'static>),
}

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
            return Ok(if command > size { Decoded::Nothing } else { Decoded::More });
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
            if left == width { return Ok(Decoded::Nothing); }
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

/// C's isspace in the "C" locale, as av_isspace tests it.
fn is_space(byte: u8) -> bool { matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') }

/// `strtoul(s, &end, 16)` stored to a uint32_t: leading space, a sign, a
/// `0x`/`0X` prefix before hex digits, ULONG_MAX (64-bit) on overflow.
/// `None` when nothing converts: strtoul returns 0 and `end` stays at `s`.
fn strtoul_hex(s: &[u8]) -> Option<(u32, usize)> {
    let mut at = s.iter().take_while(|&&byte| is_space(byte)).count();
    let negative = s.get(at) == Some(&b'-');
    if matches!(s.get(at), Some(b'+' | b'-')) { at += 1; }
    if s.get(at) == Some(&b'0') && matches!(s.get(at + 1), Some(b'x' | b'X')) && s.get(at + 2).is_some_and(u8::is_ascii_hexdigit) {
        at += 2;
    }
    let digits = at;
    let mut value = Some(0u64);
    while let Some(digit) = s.get(at).and_then(|&byte| char::from(byte).to_digit(16)) {
        value = value.and_then(|v| v.checked_mul(16)).and_then(|v| v.checked_add(u64::from(digit)));
        at += 1;
    }
    if at == digits { return None; }
    let value = match value { None => u64::MAX, Some(v) if negative => v.wrapping_neg(), Some(v) => v };
    Some((value as u32, at))
}

/// ff_dvdsub_parse_palette: 16 strtoul values separated by commas and
/// space. A value that does not convert leaves the parse where it is, so
/// it and every later entry read 0.
fn parse_palette(mut s: &[u8]) -> [[u8; 3]; 16] {
    let mut palette = [[0; 3]; 16];
    for color in &mut palette {
        let (value, used) = strtoul_hex(s).unwrap_or((0, 0));
        s = &s[used..];
        while s.first().is_some_and(|&byte| byte == b',' || is_space(byte)) { s = &s[1..]; }
        // dvdsubdec.c uses the low 24 bits as RGB.
        *color = [(value >> 16) as u8, (value >> 8) as u8, value as u8];
    }
    palette
}

/// One sscanf `%d`: leading space, a sign, decimal digits (saturating).
fn scan_int(s: &[u8]) -> Option<(i64, usize)> {
    let mut at = s.iter().take_while(|&&byte| is_space(byte)).count();
    let negative = s.get(at) == Some(&b'-');
    if matches!(s.get(at), Some(b'+' | b'-')) { at += 1; }
    let digits = at;
    let mut value = 0i64;
    while let Some(&byte) = s.get(at).filter(|byte| byte.is_ascii_digit()) {
        value = value.saturating_mul(10).saturating_add(i64::from(byte - b'0'));
        at += 1;
    }
    (at > digits).then_some((if negative { -value } else { value }, at))
}

/// `sscanf(s, "%dx%d") == 2`: the `x` must follow the width directly;
/// whatever follows the height is ignored.
fn scan_size(s: &[u8]) -> Option<(i64, i64)> {
    let (width, at) = scan_int(s)?;
    let rest = s.get(at..)?.strip_prefix(b"x")?;
    Some((width, scan_int(rest)?.0))
}

/// dvdsub_parse_extradata: the extradata as a C string (it ends at a NUL),
/// line by line. `palette:` and `size:` read on past their own line, as the
/// C parsers do. Returns the last palette and the last size; a size that
/// scans but ff_set_dimensions refuses fails the decoder's opening, as do
/// sizes over the 4096x4096 canvas cap.
fn parse_extradata(extradata: &[u8]) -> Result<(Option<[[u8; 3]; 16]>, Option<(i32, i32)>)> {
    let text = extradata.split(|&byte| byte == 0).next().unwrap_or_default();
    let (mut palette, mut size) = (None, None);
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if let Some(value) = rest.strip_prefix(b"palette:") {
            palette = Some(parse_palette(value));
        } else if let Some((width, height)) = rest.strip_prefix(b"size:").and_then(scan_size) {
            // av_image_check_size2 refuses a side of 0 or below; within
            // the cap nothing else can fail.
            let side = 1..=MAX_SIDE as i64;
            if !side.contains(&width) || !side.contains(&height) { return Err(invalid()); }
            size = Some((width as i32, height as i32));
        }
        at += rest.iter().position(|&byte| byte == b'\n' || byte == b'\r').unwrap_or(rest.len());
        at += text[at..].iter().take_while(|&&byte| byte == b'\n' || byte == b'\r').count();
    }
    Ok((palette, size))
}

pub(crate) fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let (palette, size) = parse_extradata(&params.extradata)?;
    // avctx starts with the stream's size; the extradata's replaces it.
    let (width, height) = size.unwrap_or((params.width.unwrap_or(0) as i32, params.height.unwrap_or(0) as i32));
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
            Decoded::Nothing => self.pending = cached,
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

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::TimeBase;

    /// A 2x2 subpicture of colour 1, opaque or wholly transparent.
    fn spu(opaque: bool) -> Vec<u8> {
        let mut spu = vec![0, 0, 0, 8, 0x00, 0x01, 0x00, 0x01];
        spu.extend_from_slice(&[0, 0, 0, 8, 0x03, 0x00, 0x10, 0x04, 0x00, if opaque { 0xf0 } else { 0x00 }]);
        spu.extend_from_slice(&[0x05, 0x00, 0x00, 0x01, 0x00, 0x00, 0x01, 0x06, 0x00, 0x04, 0x00, 0x06, 0x01, 0xff]);
        let size = spu.len() as u16;
        spu[..2].copy_from_slice(&size.to_be_bytes());
        spu
    }

    fn frames(decoder: &mut dyn Decoder, data: &[u8]) -> usize {
        let mut packet = Packet::new(0, TimeBase::new(1, 90_000), data.to_vec());
        packet.pts = Some(0);
        decoder.send_packet(&packet).unwrap();
        std::iter::from_fn(|| decoder.receive_frame().ok()).count()
    }

    /// dvdsub_decode keeps the units it reassembled when they show nothing
    /// (dvdsubdec.c:546-553): what follows is appended to them, and that
    /// first unit decodes again, here hiding the next subtitle as FFmpeg
    /// does. A lone packet that shows nothing leaves nothing behind.
    #[test]
    fn reassembled_unit_without_visible_pixels_stays_buffered() {
        let mut decoder = make_decoder(&CodecParameters::subtitle(CodecId::new("dvd_subtitle"))).unwrap();
        let (transparent, opaque) = (spu(false), spu(true));
        assert_eq!(frames(&mut *decoder, &opaque), 1, "a lone opaque unit shows");
        assert_eq!(frames(&mut *decoder, &transparent), 0);
        assert_eq!(frames(&mut *decoder, &opaque), 1, "a lone transparent unit keeps nothing");
        assert_eq!(frames(&mut *decoder, &transparent[..10]), 0, "the unit continues");
        assert_eq!(frames(&mut *decoder, &transparent[10..]), 0);
        assert_eq!(frames(&mut *decoder, &opaque), 0, "appended to the kept unit, which decodes again");
    }
}

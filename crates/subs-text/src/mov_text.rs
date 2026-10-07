// Copyright (c) 2012 Philip Langdale <philipl@overt.org>
//
// Derived from FFmpeg at commit 2da55bf: libavcodec/movtextdec.c.
// Changed for PearTube on 2026-10-06 and 2026-10-07 (ported to safe Rust
// and modified).
//
// This file is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// This file is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with this file (crates/subs-text/LICENSE); if not,
// write to the Free Software Foundation, Inc., 51 Franklin Street,
// Fifth Floor, Boston, MA 02110-1301 USA

//! 3GPP TS 26.245 Timed Text (`mov_text`) decoder.
//!
//! Ported to safe Rust from FFmpeg's `libavcodec/movtextdec.c` (commit
//! 2da55bf, LGPL-2.1-or-later — header verified): the sample-entry default
//! style (`tx3g`/`text` extradata) becomes the ASS `Default` style
//! (`mov_text_init`), and each sample's text with its style (`styl`),
//! highlight (`hlit`, `hclr`) and wrap (`twrp`) boxes becomes an ASS event
//! (`text_to_ass`). The event is shown through [`crate::ass_text`] like
//! every other format FFmpeg decodes to ASS.

use std::collections::HashMap;

use oxideav_core::{CodecParameters, Decoder, Error, Packet, Result};

use crate::ass_split::AssHeader;
use crate::ass_text::{default_header, ffmpeg_default_header, AssEvent, AssEventDecoder, EventSource};
use crate::{MOV_TEXT_CODEC_ID, TEXT_CODEC_ID};

const STYLE_FLAG_BOLD: u8 = 1 << 0;
const STYLE_FLAG_ITALIC: u8 = 1 << 1;
const STYLE_FLAG_UNDERLINE: u8 = 1 << 2;

/// `tx3g` sample-entry size FFmpeg requires before the style parse.
const BOX_SIZE_INITIAL: usize = 40;

const STYL_BOX: u8 = 1 << 0;
const HLIT_BOX: u8 = 1 << 1;
const HCLR_BOX: u8 = 1 << 2;
const TWRP_BOX: u8 = 1 << 3;

#[derive(Clone, Default, Debug)]
struct StyleBox {
    start: u16,
    end: u16,
    bold: bool,
    italic: bool,
    underline: bool,
    /// `0xBBGGRR`.
    color: u32,
    alpha: u8,
    fontsize: u8,
    font_id: u16,
}

#[derive(Clone, Debug)]
struct MovTextDefault {
    style: StyleBox,
    font: Vec<u8>,
    alignment: i32,
}

#[derive(Clone)]
struct MovTextContext {
    styles: Vec<StyleBox>,
    hlit_start: u16,
    hlit_end: u16,
    hlit_color: [u8; 4],
    /// The `ftab` names by font id. FFmpeg writes a `\fn` for every entry
    /// with a style's id and the renderer keeps the last; the last is kept
    /// here, so duplicate ids cost one override, not one per entry.
    fonts: HashMap<u16, Vec<u8>>,
    wrap_flag: u8,
    d: MovTextDefault,
    box_flags: u8,
}

impl Default for MovTextContext {
    fn default() -> Self {
        Self {
            styles: Vec::new(),
            hlit_start: 0,
            hlit_end: 0,
            hlit_color: [0; 4],
            fonts: HashMap::new(),
            wrap_flag: 0,
            d: MovTextDefault { style: StyleBox::default(), font: crate::ass_text::DEFAULT_FONT.as_bytes().to_vec(), alignment: 0 },
            box_flags: 0,
        }
    }
}

/// `RGB_TO_BGR`.
fn rgb_to_bgr(c: u32) -> u32 {
    (c & 0xff) << 16 | (c & 0xff00) | ((c >> 16) & 0xff)
}

struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.data.len().saturating_sub(self.pos) < n {
            return Err(Error::invalid("mov_text truncated"));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn be16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn be24(&mut self) -> Result<u32> {
        let b = self.take(3)?;
        Ok(u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]))
    }

    fn be32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn be64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
}

/// `mov_text_parse_style_record`.
fn parse_style_record(style: &mut StyleBox, cur: &mut ByteCursor) -> Result<()> {
    style.font_id = cur.be16()?;
    let flags = cur.u8()?;
    style.bold = flags & STYLE_FLAG_BOLD != 0;
    style.italic = flags & STYLE_FLAG_ITALIC != 0;
    style.underline = flags & STYLE_FLAG_UNDERLINE != 0;
    style.fontsize = cur.u8()?;
    style.color = rgb_to_bgr(cur.be24()?);
    style.alpha = cur.u8()?;
    Ok(())
}

/// `mov_text_tx3g`: the sample entry's default style and font table.
fn parse_tx3g(extradata: &[u8], m: &mut MovTextContext) -> Result<()> {
    m.fonts.clear();
    let mut remaining = extradata.len() as i64 - BOX_SIZE_INITIAL as i64;
    if remaining < 0 {
        return Err(Error::invalid("tx3g extradata too short"));
    }
    let mut cur = ByteCursor::new(extradata);
    cur.take(4)?; // display flags
    let h_align = cur.u8()? as i8;
    let v_align = cur.u8()? as i8;
    let alignment = match (h_align, v_align) {
        (0, 0) => Some(7),
        (0, 1) => Some(4),
        (0, -1) => Some(1),
        (1, 0) => Some(8),
        (1, 1) => Some(5),
        (1, -1) => Some(2),
        (-1, 0) => Some(9),
        (-1, 1) => Some(6),
        (-1, -1) => Some(3),
        _ => None,
    };
    if let Some(alignment) = alignment {
        m.d.alignment = alignment;
    }
    cur.take(4)?; // background colour and alpha
    cur.take(8)?; // BoxRecord
    cur.take(4)?; // StyleRecord start/end
    parse_style_record(&mut m.d.style, &mut cur)?;
    cur.take(8)?; // FontRecord size, `ftab`
    m.d.font = crate::ass_text::DEFAULT_FONT.as_bytes().to_vec();
    let entries = cur.be16()?;
    if entries == 0 {
        return Ok(());
    }
    remaining -= 3 * i64::from(entries);
    if remaining < 0 {
        return Err(Error::invalid("tx3g font table truncated"));
    }
    for _ in 0..entries {
        let font_id = cur.be16()?;
        let length = usize::from(cur.u8()?);
        remaining -= length as i64;
        if remaining < 0 {
            m.fonts.clear();
            return Err(Error::invalid("tx3g font name truncated"));
        }
        m.fonts.insert(font_id, cur.take(length)?.to_vec());
    }
    // FFmpeg takes the last entry with the default style's id.
    if let Some(font) = m.fonts.get(&m.d.style.font_id) {
        m.d.font = font.clone();
    }
    Ok(())
}

fn styles_equivalent(a: &StyleBox, b: &StyleBox) -> bool {
    a.bold == b.bold
        && a.italic == b.italic
        && a.underline == b.underline
        && a.color == b.color
        && a.alpha == b.alpha
        && a.fontsize == b.fontsize
        && a.font_id == b.font_id
}

/// `decode_styl`: style ranges, dropping empty ones and ones equal to the
/// default style, merging adjacent equal ones. `Ok(false)` is FFmpeg's -1
/// (box too small for its entry count).
fn decode_styl(body: &[u8], m: &mut MovTextContext) -> Result<bool> {
    let mut cur = ByteCursor::new(body);
    let entries = usize::from(cur.be16()?);
    if 2 + entries * 12 > body.len() {
        return Ok(false);
    }
    m.box_flags |= STYL_BOX;
    m.styles.clear();
    for _ in 0..entries {
        let mut style = StyleBox { start: cur.be16()?, end: cur.be16()?, ..StyleBox::default() };
        if style.end < style.start || m.styles.last().is_some_and(|last| style.start < last.end) {
            m.styles.clear();
            return Err(Error::invalid("mov_text style ranges invalid"));
        }
        if style.start == style.end {
            cur.take(8)?;
            continue;
        }
        parse_style_record(&mut style, &mut cur)?;
        if styles_equivalent(&style, &m.d.style) {
            continue;
        }
        if let Some(last) = m.styles.last_mut() {
            if style.start == last.end && styles_equivalent(&style, last) {
                last.end = style.end;
                continue;
            }
        }
        m.styles.push(style);
    }
    Ok(true)
}

/// `get_utf8_length_at`: the length of the `GET_UTF8` sequence at `i` (a
/// lead byte's leading ones, up to six, each continuation `10xxxxxx`), 0
/// when it is invalid.
fn utf8_length_at(text: &[u8], i: usize) -> usize {
    let lead = text[i];
    if lead < 0x80 {
        return 1;
    }
    if lead & 0xc0 == 0x80 || lead >= 0xfe {
        return 0;
    }
    let len = lead.leading_ones() as usize;
    let complete = (1..len).all(|k| text.get(i + k).is_some_and(|b| b & 0xc0 == 0x80));
    if complete {
        len
    } else {
        0
    }
}

/// `text_to_ass`.
fn text_to_ass(m: &MovTextContext, text: &[u8]) -> Vec<u8> {
    let d = &m.d.style;
    let mut out = Vec::with_capacity(text.len() + 32);
    let mut color = d.color;
    if !text.is_empty() && m.box_flags & TWRP_BOX != 0 {
        out.extend_from_slice(if m.wrap_flag == 1 { b"{\\q1}" } else { b"{\\q2}" });
    }
    let mut entry = 0usize;
    let mut text_pos = 0usize;
    let mut i = 0usize;
    while i < text.len() {
        if m.box_flags & STYL_BOX != 0 && entry < m.styles.len() {
            if text_pos == usize::from(m.styles[entry].end) {
                out.extend_from_slice(b"{\\r}");
                color = d.color;
                entry += 1;
            }
            if let Some(style) = m.styles.get(entry).filter(|s| text_pos == usize::from(s.start)) {
                if style.bold != d.bold {
                    out.extend_from_slice(format!("{{\\b{}}}", u8::from(style.bold)).as_bytes());
                }
                if style.italic != d.italic {
                    out.extend_from_slice(format!("{{\\i{}}}", u8::from(style.italic)).as_bytes());
                }
                if style.underline != d.underline {
                    out.extend_from_slice(format!("{{\\u{}}}", u8::from(style.underline)).as_bytes());
                }
                if style.fontsize != d.fontsize {
                    out.extend_from_slice(format!("{{\\fs{}}}", style.fontsize).as_bytes());
                }
                if style.font_id != d.font_id {
                    if let Some(font) = m.fonts.get(&style.font_id) {
                        out.extend_from_slice(b"{\\fn");
                        out.extend_from_slice(font);
                        out.push(b'}');
                    }
                }
                if d.color != style.color {
                    color = style.color;
                    out.extend_from_slice(format!("{{\\1c&H{color:X}&}}").as_bytes());
                }
                if d.alpha != style.alpha {
                    out.extend_from_slice(format!("{{\\1a&H{:02X}&}}", 255 - style.alpha).as_bytes());
                }
            }
        }
        if m.box_flags & HLIT_BOX != 0 {
            if text_pos == usize::from(m.hlit_start) {
                if m.box_flags & HCLR_BOX != 0 {
                    let c = m.hlit_color;
                    out.extend_from_slice(format!("{{\\2c&H{:02x}{:02x}{:02x}&}}", c[2], c[1], c[0]).as_bytes());
                } else {
                    out.extend_from_slice(b"{\\1c&H000000&}{\\2c&HFFFFFF&}");
                }
            }
            if text_pos == usize::from(m.hlit_end) {
                if m.box_flags & HCLR_BOX != 0 {
                    out.extend_from_slice(format!("{{\\2c&H{:X}&}}", d.color).as_bytes());
                } else {
                    out.extend_from_slice(format!("{{\\1c&H{color:X}&}}{{\\2c&H{:X}&}}", d.color).as_bytes());
                }
            }
        }
        let len = utf8_length_at(text, i).max(1).min(text.len() - i);
        match text[i] {
            b'\r' => {}
            b'\n' => out.extend_from_slice(b"\\N"),
            _ => out.extend_from_slice(&text[i..i + len]),
        }
        i += len;
        text_pos += 1;
    }
    out
}

/// `mov_text_decode_frame`: the event a sample decodes to, `None` for the
/// empty end-of-cue sample.
fn decode_sample(m: &mut MovTextContext, packet: &[u8]) -> Result<Option<Vec<u8>>> {
    if packet.len() < 2 {
        return Err(Error::invalid("mov_text packet too short"));
    }
    if packet.len() == 2 {
        return if packet == [0, 0] { Ok(None) } else { Err(Error::invalid("mov_text bad empty packet")) };
    }
    let text_length = usize::from(u16::from_be_bytes([packet[0], packet[1]]));
    let end = (2 + text_length).min(packet.len());
    let text = &packet[2..end];
    m.styles.clear();
    m.box_flags = 0;
    if text_length + 2 < packet.len() {
        let mut p = end;
        while packet.len() - p >= 8 {
            let mut cur = ByteCursor::new(&packet[p..]);
            let mut size = u64::from(cur.be32()?);
            let kind = cur.be32()?;
            let header = if size == 1 {
                if packet.len() - p - 8 < 8 {
                    break;
                }
                size = cur.be64()?;
                16
            } else {
                8
            };
            if size < header {
                return Err(Error::invalid("mov_text box size invalid"));
            }
            p += header as usize;
            let size = size - header;
            if ((packet.len() - p) as u64) < size {
                break;
            }
            let body = &packet[p..p + size as usize];
            // A box too small for its base size, or failing to decode, is
            // skipped, as in FFmpeg.
            let _ = match &kind.to_be_bytes() {
                b"styl" if body.len() >= 2 => decode_styl(body, m).map(|_| ()),
                b"hlit" if body.len() >= 4 => {
                    m.box_flags |= HLIT_BOX;
                    m.hlit_start = u16::from_be_bytes([body[0], body[1]]);
                    m.hlit_end = u16::from_be_bytes([body[2], body[3]]);
                    Ok(())
                }
                b"hclr" if body.len() >= 4 => {
                    m.box_flags |= HCLR_BOX;
                    m.hlit_color.copy_from_slice(&body[..4]);
                    Ok(())
                }
                b"twrp" if !body.is_empty() => {
                    m.box_flags |= TWRP_BOX;
                    m.wrap_flag = body[0];
                    Ok(())
                }
                _ => Ok(()),
            };
            p += size as usize;
        }
    }
    Ok(Some(text_to_ass(m, text)))
}

struct MovText(MovTextContext);

impl EventSource for MovText {
    fn event(&mut self, packet: &Packet, _text: &[u8]) -> Result<Option<AssEvent>> {
        Ok(decode_sample(&mut self.0, &packet.data)?.map(AssEvent::default_style))
    }
}

/// `mov_text_init`: the header the sample entry's default style makes.
fn header(m: &MovTextContext) -> AssHeader {
    let d = &m.d.style;
    default_header(
        &String::from_utf8_lossy(&m.d.font),
        i32::from(d.fontsize),
        (255 - u32::from(d.alpha)) << 24 | d.color,
        d.bold,
        d.italic,
        d.underline,
        m.d.alignment,
    )
}

/// Build a mov_text / QuickTime-text decoder.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let id = params.codec_id.as_str();
    if id != MOV_TEXT_CODEC_ID && id != TEXT_CODEC_ID {
        return Err(Error::unsupported(format!("not a mov_text codec id: {id}")));
    }
    let mut ctx = MovTextContext::default();
    let header = match parse_tx3g(&params.extradata, &mut ctx) {
        Ok(()) => header(&ctx),
        Err(_) => ffmpeg_default_header(),
    };
    Ok(Box::new(MovTextDecoder::new(params.codec_id.clone(), header, MovText(ctx))))
}

type MovTextDecoder = AssEventDecoder<MovText>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn styles_and_highlights_become_ass_overrides() {
        let mut m = MovTextContext::default();
        m.d.style.color = 0xffffff;
        m.d.style.alpha = 0xff;
        // "Hello" with chars 1..3 bold red, a highlight over 4..5.
        let mut sample = vec![0, 5];
        sample.extend_from_slice(b"Hello");
        sample.extend_from_slice(&[0, 0, 0, 22]);
        sample.extend_from_slice(b"styl");
        sample.extend_from_slice(&[0, 1, 0, 1, 0, 3, 0, 1, STYLE_FLAG_BOLD, 0, 0xff, 0, 0, 0xff]);
        sample.extend_from_slice(&[0, 0, 0, 12]);
        sample.extend_from_slice(b"hlit");
        sample.extend_from_slice(&[0, 4, 0, 5]);
        let ass = decode_sample(&mut m, &sample).unwrap().unwrap();
        assert_eq!(String::from_utf8(ass).unwrap(), r"H{\b1}{\1c&HFF&}el{\r}l{\1c&H000000&}{\2c&HFFFFFF&}o");
        assert_eq!(decode_sample(&mut m, &[0, 0]).unwrap(), None);
        assert!(decode_sample(&mut m, &[0, 1]).is_err());
    }
}

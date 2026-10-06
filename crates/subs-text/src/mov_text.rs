//! 3GPP TS 26.245 Timed Text (`mov_text`) decoder.
//!
//! Ported to safe Rust from FFmpeg's `libavcodec/movtextdec.c`
//! (commit 2da55bf, LGPL-2.1-or-later — header verified). The port keeps
//! FFmpeg's structure: a per-track default style from the `tx3g`
//! sample-entry extradata, per-sample style boxes (`styl`), highlight
//! (`hlit`) + highlight color (`hclr`), and the wrap flag (`twrp`), all
//! walked over the sample's text bytes to emit styled cues.
//!
//! Output representation: [`SubtitleCue`] segments instead of the ASS
//! string FFmpeg builds — the bold/italic/underline/font-size/font-face/
//! color switches FFmpeg emits as `{\b1}` etc. map to `Segment::Bold`,
//! `Segment::Italic`, `Segment::Underline`, `Segment::Font`, and
//! `Segment::Color`. Karaoke: FFmpeg's movtextdec has no karaoke; the
//! `hlit`/`hclr` highlight pair is rendered as a `Segment::Color` range.
//! Highlight *without* `hclr` inverts (white-on-black) per the FFmpeg
//! comment, which carries no color information worth a segment — the
//! text is emitted plain.

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, Segment, SubtitleCue,
    TimeBase,
};
use std::collections::VecDeque;

use crate::{MOV_TEXT_CODEC_ID, TEXT_CODEC_ID};

const STYLE_FLAG_BOLD: u8 = 1 << 0;
const STYLE_FLAG_ITALIC: u8 = 1 << 1;
const STYLE_FLAG_UNDERLINE: u8 = 1 << 2;

const BOX_SIZE_INITIAL: usize = 40;

const STYL_BOX: u8 = 1 << 0;
const HLIT_BOX: u8 = 1 << 1;
const HCLR_BOX: u8 = 1 << 2;
const TWRP_BOX: u8 = 1 << 3;
const KROK_BOX: u8 = 1 << 4;

/// `tx3g` sample-entry size FFmpeg requires before the style parse.
const TX3G_PREAMBLE: usize = BOX_SIZE_INITIAL;

/// Untrusted-input allocation caps.
const MAX_STYLES: usize = 8192;
const MAX_FONTS: usize = 256;
const MAX_CUE_BYTES: usize = 1 << 20;

#[derive(Clone, Default, Debug)]
struct StyleBox {
    start: u16,
    end: u16,
    bold: bool,
    italic: bool,
    underline: bool,
    color: u32,
    alpha: u8,
    fontsize: u8,
    font_id: u16,
}

#[derive(Clone, Default, Debug)]
struct FontRecord {
    font_id: u16,
    font: String,
}
#[derive(Clone, Default, Debug)]
struct KaraokeRecord {
    highlight_end_time: u32,
    start_char: u16,
    end_char: u16,
}

#[derive(Clone, Default, Debug)]
struct MovTextDefault {
    style: StyleBox,
    font: String,
    back_color: u32,
    back_alpha: u8,
}

#[derive(Default)]
struct MovTextContext {
    styles: Vec<StyleBox>,
    hlit_start: u16,
    hlit_end: u16,
    hlit_color: [u8; 4],
    fonts: Vec<FontRecord>,
    wrap_flag: u8,
    d: MovTextDefault,
    box_flags: u8,
    krok: Vec<KaraokeRecord>,
    readorder: u64,
}

/// Per FFmpeg's `RGB_TO_BGR` (kept for parity; the RGB/BGR swap is
/// irrelevant for `Segment::Color` which carries `(r, g, b)`, but the
/// arithmetic is preserved so behavior matches on odd colors).
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

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
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
        Ok((b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32)
    }

    fn be32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn be64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        Ok(u64::from_be_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn i8(&mut self) -> Result<i8> {
        Ok(self.u8()? as i8)
    }
}

/// Parse a style record (font id, face flags, size, RGBA color) —
/// FFmpeg's `mov_text_parse_style_record`.
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

/// Parse the `tx3g` sample-entry extradata — FFmpeg's `mov_text_tx3g`.
fn parse_tx3g(extradata: &[u8], m: &mut MovTextContext) -> Result<()> {
    m.d = MovTextDefault::default();
    if extradata.len() < TX3G_PREAMBLE {
        // FFmpeg: init with no default style on a broken header
        return Err(Error::invalid("tx3g extradata too short"));
    }
    let mut cur = ByteCursor::new(extradata);
    cur.take(4)?; // display flags
    let h_align = cur.i8()?;
    let v_align = cur.i8()?;
    let _alignment = match (h_align, v_align) {
        (0, 0) => 7,   // TOP_LEFT
        (0, 1) => 4,   // MIDDLE_LEFT
        (0, -1) => 1,  // BOTTOM_LEFT
        (1, 0) => 8,   // TOP_CENTER
        (1, 1) => 5,   // MIDDLE_CENTER
        (1, -1) => 2,  // BOTTOM_CENTER
        (-1, 0) => 9,  // TOP_RIGHT
        (-1, 1) => 6,  // MIDDLE_RIGHT
        (-1, -1) => 3, // BOTTOM_RIGHT
        _ => 2,
    };
    m.d.back_color = rgb_to_bgr(cur.be24()?);
    m.d.back_alpha = cur.u8()?;
    cur.take(8)?; // BoxRecord (default text box)
    cur.take(4)?; // StyleRecord start
    parse_style_record(&mut m.d.style, &mut cur)?;
    cur.take(4)?; // FontRecord size
    cur.take(4)?; // ftab
    m.d.font.clear();
    let ftab_entries = cur.be16()?;
    if ftab_entries == 0 {
        return Ok(());
    }
    if ftab_entries as usize > MAX_FONTS {
        return Err(Error::invalid("tx3g font table too large"));
    }
    let mut default_font_index: Option<usize> = None;
    for i in 0..ftab_entries as usize {
        let font_id = cur.be16()?;
        if font_id == m.d.style.font_id {
            default_font_index = Some(i);
        }
        let font_length = cur.u8()? as usize;
        let font = cur.take(font_length)?;
        let font = String::from_utf8_lossy(font).into_owned();
        m.fonts.push(FontRecord { font_id, font });
    }
    if let Some(i) = default_font_index {
        m.d.font = m.fonts[i].font.clone();
    }
    Ok(())
}

fn decode_twrp(cur: &mut ByteCursor, m: &mut MovTextContext) -> Result<()> {
    m.box_flags |= TWRP_BOX;
    m.wrap_flag = cur.u8()?;
    Ok(())
}

fn decode_hlit(cur: &mut ByteCursor, m: &mut MovTextContext) -> Result<()> {
    m.box_flags |= HLIT_BOX;
    m.hlit_start = cur.be16()?;
    m.hlit_end = cur.be16()?;
    Ok(())
}

fn decode_hclr(cur: &mut ByteCursor, m: &mut MovTextContext) -> Result<()> {
    m.box_flags |= HCLR_BOX;
    m.hlit_color = cur.take(4)?.try_into().expect("4 bytes");
    Ok(())
}
fn decode_krok(cur: &mut ByteCursor, m: &mut MovTextContext) -> Result<()> {
    m.box_flags |= KROK_BOX;
    let _highlight_start_time = cur.be32()?;
    let count = cur.be16()? as usize;
    if count > MAX_STYLES {
        return Err(Error::invalid("too many karaoke entries"));
    }
    m.krok.clear();
    for _ in 0..count {
        let highlight_end_time = cur.be32()?;
        let start_char = cur.be16()?;
        let end_char = cur.be16()?;
        m.krok.push(KaraokeRecord {
            highlight_end_time,
            start_char,
            end_char,
        });
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

/// FFmpeg's `decode_styl`: read style entries, dropping empty ranges,
/// styles equal to the default, and merging adjacent equivalents.
fn decode_styl(cur: &mut ByteCursor, m: &mut MovTextContext, size: usize) -> Result<()> {
    let style_entries = cur.be16()? as usize;
    if 2 + style_entries * 12 > size || style_entries > MAX_STYLES {
        return Err(Error::invalid("mov_text styl size invalid"));
    }
    let mut styles: Vec<StyleBox> = Vec::with_capacity(style_entries);
    m.box_flags |= STYL_BOX;
    for i in 0..style_entries {
        let mut style = StyleBox::default();
        style.start = cur.be16()?;
        style.end = cur.be16()?;
        if style.end < style.start || (!styles.is_empty() && style.start < styles[i - 1].end) {
            m.styles.clear();
            return Err(Error::invalid("mov_text style ranges invalid"));
        }
        if style.start == style.end {
            // Applies to no character; skip
            cur.take(8)?;
            continue;
        }
        parse_style_record(&mut style, cur)?;
        if styles_equivalent(&style, &m.d.style) {
            // Equivalent to the default style
            continue;
        }
        if let Some(last) = styles.last_mut() {
            if style.start == last.end && styles_equivalent(&style, last) {
                last.end = style.end;
                continue;
            }
        }
        styles.push(style);
    }
    m.styles = styles;
    Ok(())
}

/// Emit text from `text[pos]` with style switches at the recorded byte
/// offsets — FFmpeg's `text_to_ass` translated to segments.
fn text_to_segments(m: &MovTextContext, text: &[u8]) -> Result<Vec<Segment>> {
    const PUSH: u8 = 0;
    const POP: u8 = 1;
    let mut out: Vec<(u8, Segment)> = Vec::new();
    let default_style = &m.d.style;
    let mut entry = 0usize;
    let mut text_pos = 0usize;
    let mut push_font: Option<String> = None;
    let mut pop_depth: usize = 0;

    // We walk bytes with a UTF-8 decoder that tolerates invalid bytes the
    // same way FFmpeg does (skip one byte on error).
    let mut i = 0usize;
    while i < text.len() {
        // style switch at byte boundary `text_pos`
        if m.box_flags & STYL_BOX != 0 && entry < m.styles.len() {
            let style = &m.styles[entry];
            if text_pos == style.end as usize {
                // {\r} — reset to default: pop every pushed override
                while pop_depth > 0 {
                    out.push((POP, Segment::Text(String::new())));
                    pop_depth -= 1;
                }
                let _ = default_style;
                entry += 1;
            }
            if entry < m.styles.len() {
                let style = &m.styles[entry];
                if text_pos == style.start as usize {
                    let mut segs: Vec<Segment> = Vec::new();
                    let mut opened = false;
                    if style.bold != default_style.bold {
                        push_font = None;
                        out.push((PUSH, Segment::Bold(Vec::new())));
                        pop_depth += 1;
                        opened = true;
                    }
                    let _ = opened;
                    if style.italic != default_style.italic {
                        out.push((PUSH, Segment::Italic(Vec::new())));
                        pop_depth += 1;
                    }
                    if style.underline != default_style.underline {
                        out.push((PUSH, Segment::Underline(Vec::new())));
                        pop_depth += 1;
                    }
                    if style.fontsize != default_style.fontsize {
                        segs.push(Segment::Font {
                            family: None,
                            size: Some(style.fontsize as f32),
                            children: Vec::new(),
                        });
                        // font is a leaf container without children; to keep
                        // nesting sane we wrap text as we emit below.
                        out.push((PUSH, segs.remove(0)));
                        pop_depth += 1;
                    }
                    if style.font_id != default_style.font_id {
                        if let Some(f) = m.fonts.iter().find(|f| f.font_id == style.font_id) {
                            if !f.font.is_empty() {
                                push_font = Some(f.font.clone());
                                out.push((PUSH, Segment::Font { family: Some(f.font.clone()), size: None, children: Vec::new() }));
                                pop_depth += 1;
                            }
                        }
                    }
                    if default_style.color != style.color {
                        let c = style.color;
                        let rgb = (
                            (c >> 16 & 0xff) as u8,
                            (c >> 8 & 0xff) as u8,
                            (c & 0xff) as u8,
                        );
                        out.push((PUSH, Segment::Color { rgb, children: Vec::new() }));
                        pop_depth += 1;
                    }
                    if default_style.alpha != style.alpha {
                        // alpha-only difference: no text-visible segment
                        // (the IR has no per-run alpha; the renderer's
                        // style table does) — safe to skip.
                    }
                    let _ = segs;
                }
            }
        }
        if m.box_flags & HLIT_BOX != 0 {
            if text_pos == m.hlit_start as usize {
                if m.box_flags & HCLR_BOX != 0 {
                    let rgb = (m.hlit_color[2], m.hlit_color[1], m.hlit_color[0]);
                    out.push((PUSH, Segment::Color { rgb, children: Vec::new() }));
                    pop_depth += 1;
                }
                // Without HCLR FFmpeg emits an inversion pair; nothing a
                // text cue can carry — skip.
            }
            if text_pos == m.hlit_end as usize && m.box_flags & HCLR_BOX != 0 {
                out.push((POP, Segment::Text(String::new())));
                pop_depth = pop_depth.saturating_sub(1);
            }
        }
        if m.box_flags & KROK_BOX != 0 {
            for k in &m.krok {
                if text_pos == k.start_char as usize {
                    let cs = (k.highlight_end_time / 10).max(1);
                    out.push((PUSH, Segment::Karaoke { cs, children: Vec::new() }));
                    pop_depth += 1;
                }
                if text_pos == k.end_char as usize {
                    out.push((POP, Segment::Text(String::new())));
                    pop_depth = pop_depth.saturating_sub(1);
                }
            }
        }

        // one UTF-8 scalar, FFmpeg's get_utf8_length_at semantics
        let (ch, len) = decode_utf8_at(text, i);
        match ch {
            '\r' => {}
            '\n' => out.push((PUSH, Segment::LineBreak)),
            _ => out.push((PUSH, Segment::Text(ch.to_string()))),
        }
        i += len;
        text_pos += 1;
    }
    let _ = pop_depth;
    let _ = push_font.take();

    // Now nest the flat PUSH/POP list into a segment tree.
    Ok(nest(&out))
}

#[allow(clippy::type_complexity)]
fn nest(flat: &[(u8, Segment)]) -> Vec<Segment> {
    const PUSH: u8 = 0;
    // Stack of (container, children)
    let mut stack: Vec<(&str, Vec<Segment>)> = Vec::new();
    let mut root: Vec<Segment> = Vec::new();
    for (op, seg) in flat {
        if *op == PUSH {
            match seg {
                Segment::LineBreak => {
                    if let Some((_, children)) = stack.last_mut() {
                        children.push(Segment::LineBreak);
                    } else {
                        root.push(Segment::LineBreak);
                    }
                }
                Segment::Text(s) if !s.is_empty() => {
                    if let Some((_, children)) = stack.last_mut() {
                        children.push(Segment::Text(s.clone()));
                    } else {
                        root.push(Segment::Text(s.clone()));
                    }
                }
                Segment::Text(_) => {}
                other => {
                    let kind: &str = match other {
                        Segment::Bold(_) => "b",
                        Segment::Italic(_) => "i",
                        Segment::Underline(_) => "u",
                        Segment::Strike(_) => "s",
                        Segment::Color { .. } => "c",
                        Segment::Font { .. } => "f",
                        Segment::Karaoke { .. } => "k",
                        _ => "?",
                    };
                    stack.push((kind, Vec::new()));
                }
            }
        } else {
            if let Some((kind, children)) = stack.pop() {
                let built = match kind {
                    "b" => Segment::Bold(children),
                    "i" => Segment::Italic(children),
                    "u" => Segment::Underline(children),
                    "s" => Segment::Strike(children),
                    "c" => {
                        if let Some(Segment::Color { rgb, .. }) = flat_get(flat, seg) {
                            Segment::Color { rgb: *rgb, children }
                        } else {
                            Segment::Bold(children)
                        }
                    }
                    "f" => {
                        if let Some(Segment::Font { family, size, .. }) = flat_get(flat, seg) {
                            Segment::Font { family: family.clone(), size: *size, children }
                        } else {
                            Segment::Bold(children)
                        }
                    }
                    "k" => {
                        if let Some(Segment::Karaoke { cs, .. }) = flat_get(flat, seg) {
                            Segment::Karaoke { cs: *cs, children }
                        } else {
                            Segment::Bold(children)
                        }
                    }
                    _ => Segment::Bold(children),
                };
                if let Some((_, children)) = stack.last_mut() {
                    children.push(built);
                } else {
                    root.push(built);
                }
            }
        }
    }
    // Unclosed containers (malformed input): flush as-is.
    while let Some((kind, children)) = stack.pop() {
        let built = match kind {
            "b" => Segment::Bold(children),
            "i" => Segment::Italic(children),
            "u" => Segment::Underline(children),
            "s" => Segment::Strike(children),
            "k" => Segment::Karaoke { cs: 0, children },
            _ => Segment::Bold(children),
        };
        if let Some((_, ch2)) = stack.last_mut() {
            ch2.push(built);
        } else {
            root.push(built);
        }
    }
    root
}

fn flat_get<'s>(flat: &'s [(u8, Segment)], needle: &Segment) -> Option<&'s Segment> {
    flat.iter().rev().find(|(_, s)| std::ptr::eq(s, needle)).map(|(_, s)| s)
}

/// Decode one UTF-8 scalar at `i`, FFmpeg-style: on an invalid sequence
/// consume exactly one byte and return U+FFFD (FFmpeg logs and steps one
/// byte; the replacement avoids emitting raw invalid UTF-8 into cues).
fn decode_utf8_at(data: &[u8], i: usize) -> (char, usize) {
    let b0 = data[i];
    if b0 < 0x80 {
        return (b0 as char, 1);
    }
    let len = if b0 & 0xe0 == 0xc0 {
        2
    } else if b0 & 0xf0 == 0xe0 {
        3
    } else if b0 & 0xf8 == 0xf0 {
        4
    } else {
        return ('\u{fffd}', 1);
    };
    if i + len > data.len() {
        return ('\u{fffd}', 1);
    }
    match std::str::from_utf8(&data[i..i + len]) {
        Ok(s) => match s.chars().next() {
            Some(c) => (c, len),
            None => ('\u{fffd}', 1),
        },
        Err(_) => ('\u{fffd}', 1),
    }
}

/// Decode one sample — FFmpeg's `mov_text_decode_frame`.
fn decode_sample(m: &mut MovTextContext, packet: &[u8]) -> Result<Option<SubtitleCue>> {
    if packet.len() < 2 {
        return Err(Error::invalid("mov_text packet too short"));
    }
    // A size-2 packet with value zero marks the end of the previous
    // non-empty subtitle: drop it (duration information already known).
    if packet.len() == 2 {
        if packet[0] == 0 && packet[1] == 0 {
            return Ok(None);
        }
        return Err(Error::invalid("mov_text bad empty packet"));
    }
    let text_length = u16::from_be_bytes([packet[0], packet[1]]) as usize;
    let end = (2 + text_length).min(packet.len());
    let text = &packet[2..end];

    m.styles.clear();
    m.krok.clear();
    m.box_flags = 0;

    if text_length + 2 < packet.len() {
        // style boxes follow the text
        let mut cur = ByteCursor::new(&packet[end..]);
        loop {
            if cur.remaining() < 8 {
                break;
            }
            let mut size = cur.be32()? as usize;
            let typ = cur.be32()?;
            let (size_var, long_size) = if size == 1 {
                let v = cur.be64()? as usize;
                (16usize, v)
            } else {
                (8usize, size)
            };
            size = long_size;
            if size < size_var {
                return Err(Error::invalid("tsmb size invalid"));
            }
            size -= size_var;
            if cur.remaining() < size {
                break;
            }
            let body = cur.take(size)?;
            match &typ.to_be_bytes() {
                b"styl" => {
                    let mut body_cur = ByteCursor::new(body);
                    decode_styl(&mut body_cur, m, size)?;
                }
                b"hlit" => {
                    let mut body_cur = ByteCursor::new(body);
                    decode_hlit(&mut body_cur, m)?;
                }
                b"hclr" => {
                    let mut body_cur = ByteCursor::new(body);
                    decode_hclr(&mut body_cur, m)?;
                }
                b"twrp" => {
                    let mut body_cur = ByteCursor::new(body);
                    decode_twrp(&mut body_cur, m)?;
                }
                b"krok" => {
                    let mut body_cur = ByteCursor::new(body);
                    decode_krok(&mut body_cur, m)?;
                }
                _ => {}
            }
        }
        let mut segments = text_to_segments(m, text)?;
        if !m.d.font.is_empty() || m.d.style.fontsize > 0 {
            segments = vec![Segment::Font {
                family: if m.d.font.is_empty() { None } else { Some(m.d.font.clone()) },
                size: if m.d.style.fontsize > 0 { Some(m.d.style.fontsize as f32) } else { None },
                children: segments,
            }];
        }
        m.readorder = m.readorder.wrapping_add(1);
        return Ok(Some(SubtitleCue {
            start_us: 0,
            end_us: 0,
            style_ref: None,
            positioning: None,
            segments,
        }));
    }
    let mut segments = text_to_segments(m, text)?;
    if !m.d.font.is_empty() || m.d.style.fontsize > 0 {
        segments = vec![Segment::Font {
            family: if m.d.font.is_empty() { None } else { Some(m.d.font.clone()) },
            size: if m.d.style.fontsize > 0 { Some(m.d.style.fontsize as f32) } else { None },
            children: segments,
        }];
    }
    m.readorder = m.readorder.wrapping_add(1);
    Ok(Some(SubtitleCue {
        start_us: 0,
        end_us: 0,
        style_ref: None,
        positioning: None,
        segments,
    }))
}

/// Build a mov_text / QuickTime-text decoder.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let id = params.codec_id.as_str();
    if id != MOV_TEXT_CODEC_ID && id != TEXT_CODEC_ID {
        return Err(Error::unsupported(format!("not a mov_text codec id: {id}")));
    }
    let mut ctx = MovTextContext::default();
    if id == MOV_TEXT_CODEC_ID && !params.extradata.is_empty() {
        // A failed extradata parse leaves the default style empty, like
        // FFmpeg's `ff_ass_subtitle_header_default` fallback.
        let _ = parse_tx3g(&params.extradata, &mut ctx);
    }
    Ok(Box::new(MovTextDecoder {
        codec_id: params.codec_id.clone(),
        ctx,
        pending: VecDeque::new(),
        eof: false,
    }))
}

pub struct MovTextDecoder {
    codec_id: CodecId,
    ctx: MovTextContext,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for MovTextDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("mov_text packet too large"));
        }
        let mut cue = match decode_sample(&mut self.ctx, &packet.data)? {
            Some(c) => c,
            None => return Ok(()),
        };
        let start_us = packet
            .pts
            .map(|pts| packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000)))
            .unwrap_or(0);
        let end_us = if cue.end_us > cue.start_us {
            start_us + (cue.end_us - cue.start_us)
        } else if let Some(dur) = packet.duration {
            start_us + packet.time_base.rescale(dur, TimeBase::new(1, 1_000_000))
        } else {
            start_us
        };
        cue.start_us = start_us;
        cue.end_us = end_us;
        self.pending.push_back(Frame::Subtitle(cue));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(f);
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.eof = false;
        Ok(())
    }
}

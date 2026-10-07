// Copyright (c) 2010 Aurelien Jacobs <aurel@gnuage.org>
//
// Derived from FFmpeg at commit 2da55bf: libavcodec/ass_split.c.
// Changed for PearTube on 2026-10-07: ported to safe Rust and modified.
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

//! ASS script header, dialogue and override-code splitting.
//!
//! Ported to safe Rust from FFmpeg's `libavcodec/ass_split.c` at commit
//! 2da55bf (LGPL-2.1-or-later — header verified): `ff_ass_split` (the style
//! sections of a script header, including its prefix-matched keys and
//! format-ordered fields), `ff_ass_split_dialog` (the packet form
//! `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`),
//! `ff_ass_style_get`, and `ff_ass_split_override_codes`, whose callbacks
//! decide what text a cue shows and how it is styled.

use std::collections::HashMap;
use std::sync::Arc;

use crate::scan::{strcspn, Scan};

/// Longest font name kept, in bytes. FFmpeg's `\fn` override reads at most
/// 127 (`%127[^\\}]`); header names are kept to the same length, so a cue
/// costs the same whatever font names a script declares.
pub const MAX_FONT_NAME: usize = 127;

/// A font name as cues share it: lossy UTF-8, at most [`MAX_FONT_NAME`]
/// bytes (cut at a character boundary).
pub fn font_name(bytes: &[u8]) -> Arc<str> {
    let name = String::from_utf8_lossy(bytes);
    let mut end = name.len().min(MAX_FONT_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    Arc::from(&name[..end])
}

/// `ASSStyle`: the fields a cue's rendering uses. Absent fields are zero,
/// as FFmpeg's zeroed structure leaves them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssStyle {
    pub name: Option<String>,
    pub font_name: Option<Arc<str>>,
    pub font_size: i32,
    /// `&HAABBGGRR` as parsed.
    pub primary_color: u32,
    pub bold: i32,
    pub italic: i32,
    pub underline: i32,
    pub strikeout: i32,
    /// Numpad alignment (V4 alignments are converted).
    pub alignment: i32,
}

/// The style table of a script header, indexed by name.
#[derive(Clone, Debug, Default)]
pub struct AssHeader {
    pub styles: Vec<AssStyle>,
    /// The first style of each name, as `ff_ass_style_get`'s scan finds it.
    by_name: HashMap<String, usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Section {
    ScriptInfo,
    V4PlusStyles,
    V4Styles,
    Events,
}

const SECTIONS: [(Section, &str, Option<&str>); 4] = [
    (Section::ScriptInfo, "Script Info", None),
    (Section::V4PlusStyles, "V4+ Styles", Some("Style")),
    (Section::V4Styles, "V4 Styles", Some("Style")),
    (Section::Events, "Events", Some("Dialogue")),
];

#[derive(Clone, Copy, PartialEq)]
enum Field {
    Name,
    Fontname,
    Fontsize,
    PrimaryColour,
    Bold,
    Italic,
    Underline,
    StrikeOut,
    Alignment,
    /// A field FFmpeg parses that no cue uses.
    Other,
}

const V4PLUS_FIELDS: [(&str, Field); 23] = [
    ("Name", Field::Name),
    ("Fontname", Field::Fontname),
    ("Fontsize", Field::Fontsize),
    ("PrimaryColour", Field::PrimaryColour),
    ("SecondaryColour", Field::Other),
    ("OutlineColour", Field::Other),
    ("BackColour", Field::Other),
    ("Bold", Field::Bold),
    ("Italic", Field::Italic),
    ("Underline", Field::Underline),
    ("StrikeOut", Field::StrikeOut),
    ("ScaleX", Field::Other),
    ("ScaleY", Field::Other),
    ("Spacing", Field::Other),
    ("Angle", Field::Other),
    ("BorderStyle", Field::Other),
    ("Outline", Field::Other),
    ("Shadow", Field::Other),
    ("Alignment", Field::Alignment),
    ("MarginL", Field::Other),
    ("MarginR", Field::Other),
    ("MarginV", Field::Other),
    ("Encoding", Field::Other),
];

const V4_FIELDS: [(&str, Field); 18] = [
    ("Name", Field::Name),
    ("Fontname", Field::Fontname),
    ("Fontsize", Field::Fontsize),
    ("PrimaryColour", Field::PrimaryColour),
    ("SecondaryColour", Field::Other),
    ("TertiaryColour", Field::Other),
    ("BackColour", Field::Other),
    ("Bold", Field::Bold),
    ("Italic", Field::Italic),
    ("BorderStyle", Field::Other),
    ("Outline", Field::Other),
    ("Shadow", Field::Other),
    ("Alignment", Field::Alignment),
    ("MarginL", Field::Other),
    ("MarginR", Field::Other),
    ("MarginV", Field::Other),
    ("AlphaLevel", Field::Other),
    ("Encoding", Field::Other),
];

const EVENT_FIELDS: [(&str, Field); 10] = [
    ("Layer", Field::Other),
    ("Start", Field::Other),
    ("End", Field::Other),
    ("Style", Field::Other),
    ("Name", Field::Other),
    ("MarginL", Field::Other),
    ("MarginR", Field::Other),
    ("MarginV", Field::Other),
    ("Effect", Field::Other),
    ("Text", Field::Other),
];

fn fields_of(section: Section) -> &'static [(&'static str, Field)] {
    match section {
        Section::ScriptInfo => &[],
        Section::V4PlusStyles => &V4PLUS_FIELDS,
        Section::V4Styles => &V4_FIELDS,
        Section::Events => &EVENT_FIELDS,
    }
}

/// C `strncmp(a, b, n) == 0` over NUL-terminated byte strings.
fn strncmp_eq(a: &[u8], b: &[u8], n: usize) -> bool {
    for i in 0..n {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return false;
        }
        if x == 0 {
            return true;
        }
    }
    true
}

fn is_eol(b: u8) -> bool {
    b == b'\r' || b == b'\n' || b == 0
}

fn skip_space(s: &[u8], mut i: usize) -> usize {
    while s.get(i) == Some(&b' ') {
        i += 1;
    }
    i
}

fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// `convert_int`: `sscanf("%d")` from the field start (not bounded by the
/// field length); unparsed fields keep their zero.
fn convert_int(s: &[u8]) -> Option<i32> {
    Scan::new(s).int(0).map(|v| v as i32)
}

fn convert_color(s: &[u8]) -> Option<u32> {
    let mut scan = Scan::new(s);
    if scan.lit_str(b"&H").is_some() {
        if let Some(v) = scan.hex(8) {
            return Some(v as u32);
        }
    }
    convert_int(s).map(|v| v as u32)
}

struct SectionState {
    order: Option<Vec<i32>>,
}

/// `ff_ass_split` restricted to what the decoders use: the style table.
pub fn split_header(header: &[u8]) -> AssHeader {
    let buf = header.strip_prefix(b"\xef\xbb\xbf").unwrap_or(header);
    let buf = crate::scan::c_str(buf);
    let mut styles = Vec::new();
    let mut states: [SectionState; 4] = std::array::from_fn(|_| SectionState { order: None });
    let mut i = 0usize;
    while i < buf.len() {
        let mut scan = Scan::new(&buf[i..]);
        let name = scan.lit(b'[').and_then(|_| {
            scan.set(15, |b| b.is_ascii_alphanumeric() || b == b'+' || b == b' ')
        });
        let header = name.filter(|_| scan.lit(b']').is_some() && scan.any().is_some());
        i += strcspn(&buf[i..], b"\n");
        i += usize::from(i < buf.len());
        if let Some(name) = header {
            for (index, (_, section_name, _)) in SECTIONS.iter().enumerate() {
                if name == section_name.as_bytes() {
                    i = split_section(buf, i, index, &mut states, &mut styles);
                }
            }
        }
    }
    AssHeader::new(styles)
}

/// `ass_split_section`: parses lines until the next `[` line. The current
/// section can change on a key that prefixes another section's line key,
/// as FFmpeg's `strncmp(buf, fields_header, len)` does.
fn split_section(buf: &[u8], mut i: usize, mut current: usize, states: &mut [SectionState; 4], out: &mut Vec<AssStyle>) -> usize {
    while i < buf.len() {
        let line = &buf[i..];
        if line[0] == b'[' {
            return i;
        }
        let comment = line[0] == b';' || (line[0] == b'!' && at(line, 1) == b':');
        if !comment {
            let len = strcspn(line, b":\r\n");
            let fields_header = SECTIONS[current].2;
            if at(line, len) == b':' && fields_header.is_none_or(|h| !strncmp_eq(line, h.as_bytes(), len)) {
                if let Some(index) = SECTIONS.iter().position(|(_, _, h)| h.is_some_and(|h| strncmp_eq(line, h.as_bytes(), len))) {
                    current = index;
                }
            }
            parse_line(line, current, states, out);
        }
        i += strcspn(&buf[i..], b"\n");
        i += usize::from(i < buf.len());
    }
    i
}

fn parse_line(line: &[u8], current: usize, states: &mut [SectionState; 4], out: &mut Vec<AssStyle>) {
    let (section, _, fields_header) = SECTIONS[current];
    let fields = fields_of(section);
    let state = &mut states[current];
    if fields_header.is_some() && state.order.is_none() && line.starts_with(b"Format") && at(line, 6) == b':' {
        let mut order = Vec::new();
        let mut p = 7;
        while !is_eol(at(line, p)) {
            p = skip_space(line, p);
            let len = strcspn(&line[p..], b", \r\n");
            let token = &line[p..];
            let index = fields.iter().position(|(name, _)| strncmp_eq(token, name.as_bytes(), len));
            order.push(index.map_or(-1, |x| x as i32));
            p = skip_space(line, p + len + usize::from(at(line, p + len) == b','));
        }
        state.order = Some(order);
        return;
    }
    // Script Info keys (prefix-matched by FFmpeg) style no cue.
    let Some(header) = fields_header else { return };
    let hlen = header.len();
    if !(line.starts_with(header.as_bytes()) && at(line, hlen) == b':') {
        return;
    }
    let order = state.order.get_or_insert_with(|| (0..fields.len() as i32).collect()).clone();
    let mut style = AssStyle::default();
    let mut p = hlen + 1;
    let number = order.len();
    let mut k = 0;
    while !is_eol(at(line, p)) && k < number {
        let last = k == number - 1;
        p = skip_space(line, p);
        let len = strcspn(&line[p..], if last { b"\r\n" } else { b",\r\n" });
        if section != Section::Events {
            if let Some(&(_, field)) = usize::try_from(order[k]).ok().and_then(|f| fields.get(f)) {
                let value = &line[p..p + len];
                let rest = &line[p..];
                match field {
                    Field::Name => style.name = Some(String::from_utf8_lossy(value).into_owned()),
                    Field::Fontname => style.font_name = Some(font_name(value)),
                    Field::Fontsize => style.font_size = convert_int(rest).unwrap_or(style.font_size),
                    Field::PrimaryColour => style.primary_color = convert_color(rest).unwrap_or(style.primary_color),
                    Field::Bold => style.bold = convert_int(rest).unwrap_or(style.bold),
                    Field::Italic => style.italic = convert_int(rest).unwrap_or(style.italic),
                    Field::Underline => style.underline = convert_int(rest).unwrap_or(style.underline),
                    Field::StrikeOut => style.strikeout = convert_int(rest).unwrap_or(style.strikeout),
                    Field::Alignment => {
                        if let Some(a) = convert_int(rest) {
                            style.alignment = if section == Section::V4Styles {
                                // convert_alignment: V4 to V4+ numpad.
                                a.wrapping_add((a & 4) >> 1).wrapping_sub(5 * i32::from(a & 8 != 0))
                            } else {
                                a
                            };
                        }
                    }
                    Field::Other => {}
                }
            }
        }
        p += len;
        if !last && at(line, p) != 0 {
            p += 1;
        }
        p = skip_space(line, p);
        k += 1;
    }
    if section != Section::Events && out.len() < crate::text_common::MAX_CUES {
        out.push(style);
    }
}

impl AssHeader {
    pub fn new(styles: Vec<AssStyle>) -> Self {
        let mut by_name = HashMap::with_capacity(styles.len());
        for (i, style) in styles.iter().enumerate() {
            if let Some(name) = &style.name {
                by_name.entry(name.clone()).or_insert(i);
            }
        }
        Self { styles, by_name }
    }

    /// `ff_ass_style_get`: the first style named `style` ("Default" when
    /// empty).
    pub fn style(&self, style: &str) -> Option<&AssStyle> {
        let style = if style.is_empty() { "Default" } else { style };
        self.by_name.get(style).map(|&i| &self.styles[i])
    }
}

/// `ASSDialog` fields of an event in packet form.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssDialog<'a> {
    pub readorder: i32,
    pub layer: i32,
    pub style: &'a [u8],
    pub name: &'a [u8],
    pub effect: &'a [u8],
    pub text: &'a [u8],
}

/// `ff_ass_split_dialog`: nine comma-separated fields, each after leading
/// spaces, the last running to the end.
pub fn split_dialog(buf: &[u8]) -> AssDialog<'_> {
    let buf = crate::scan::c_str(buf);
    let mut fields: [&[u8]; 9] = [&[]; 9];
    let mut p = 0;
    for (i, field) in fields.iter_mut().enumerate() {
        p = skip_space(buf, p);
        let len = if i == 8 { buf.len() - p } else { strcspn(&buf[p..], b",") };
        *field = &buf[p..p + len];
        p += len;
        if p < buf.len() {
            p += 1;
        }
    }
    AssDialog {
        readorder: convert_int(fields[0]).unwrap_or(0),
        layer: convert_int(fields[1]).unwrap_or(0),
        style: fields[2],
        name: fields[3],
        effect: fields[7],
        text: fields[8],
    }
}

/// `ASSCodesCallbacks`: what `split_override_codes` reports. Every method
/// defaults to FFmpeg's absent callback.
pub trait OverrideCallbacks {
    fn text(&mut self, _text: &[u8]) {}
    fn new_line(&mut self, _forced: bool) {}
    /// `style` is `b`, `i`, `s` or `u`; `close` 1 for `0`, 0 for `1`, -1
    /// for no argument.
    fn style(&mut self, _style: u8, _close: i32) {}
    /// `color` 0xFFFFFFFF when reset; `color_id` 0 for `\c`, else 1..=4.
    fn color(&mut self, _color: u32, _color_id: u32) {}
    fn alpha(&mut self, _alpha: i32, _alpha_id: u32) {}
    fn font_name(&mut self, _name: Option<&[u8]>) {}
    fn font_size(&mut self, _size: i32) {}
    fn alignment(&mut self, _alignment: i32) {}
    fn cancel_overrides(&mut self, _style: &[u8]) {}
    fn move_to(&mut self, _x1: i32, _y1: i32, _x2: i32, _y2: i32, _t1: i32, _t2: i32) {}
    fn origin(&mut self, _x: i32, _y: i32) {}
    fn karaoke(&mut self, _duration: u32) {}
    fn end(&mut self) {}
}

fn sep(scan: &mut Scan) -> bool {
    matches!(scan.peek(), b'\\' | b'}') && scan.any().is_some()
}

/// One override code at `\` inside a `{\...}` block: calls the callback
/// and returns `len`, the bytes up to and including its separator.
fn override_code(cb: &mut dyn OverrideCallbacks, buf: &[u8]) -> usize {
    // \b \i \s \u
    {
        let mut s = Scan::new(buf);
        if s.lit(b'\\').is_some() {
            if let Some(style) = s.set(1, |b| b"bisu".contains(&b)) {
                let style = style[0];
                if let Some(c) = s.set(1, |b| b"01\\}".contains(&b)) {
                    let close = match c[0] {
                        b'0' => 1,
                        b'1' => 0,
                        _ => -1,
                    };
                    cb.style(style, close);
                    return s.pos() + usize::from(close != -1);
                }
            }
        }
    }
    // colours: \c \c&H..& \Nc \Nc&H..&
    {
        let try_color = |numbered: bool, valued: bool| -> Option<(u32, u32, usize)> {
            let mut s = Scan::new(buf);
            s.lit(b'\\')?;
            let id = if numbered { u32::from(s.set(1, |b| b"1234".contains(&b))?[0] - b'0') } else { 0 };
            s.lit(b'c')?;
            let color = if valued {
                s.lit_str(b"&H")?;
                let v = s.hex(0)? as u32;
                s.lit(b'&')?;
                v
            } else {
                0xFFFF_FFFF
            };
            sep(&mut s).then_some((color, id, s.pos()))
        };
        if let Some((color, id, len)) =
            try_color(false, false).or_else(|| try_color(false, true)).or_else(|| try_color(true, false)).or_else(|| try_color(true, true))
        {
            cb.color(color, id);
            return len;
        }
    }
    // alpha: \alpha \alpha&H..& \Na \Na&H..&
    {
        let try_alpha = |numbered: bool, valued: bool| -> Option<(i32, u32, usize)> {
            let mut s = Scan::new(buf);
            s.lit(b'\\')?;
            let id = if numbered {
                let id = u32::from(s.set(1, |b| b"1234".contains(&b))?[0] - b'0');
                s.lit(b'a')?;
                id
            } else {
                s.lit_str(b"alpha")?;
                0
            };
            let alpha = if valued {
                s.lit_str(b"&H")?;
                let v = s.hex(2)? as u32 as i32;
                s.lit(b'&')?;
                v
            } else {
                -1
            };
            sep(&mut s).then_some((alpha, id, s.pos()))
        };
        if let Some((alpha, id, len)) =
            try_alpha(false, false).or_else(|| try_alpha(false, true)).or_else(|| try_alpha(true, false)).or_else(|| try_alpha(true, true))
        {
            cb.alpha(alpha, id);
            return len;
        }
    }
    // \fn
    {
        let mut s = Scan::new(buf);
        if s.lit_str(b"\\fn").is_some() {
            let mut reset = s;
            if sep(&mut reset) {
                cb.font_name(None);
                return reset.pos();
            }
            if let Some(name) = s.set(127, |b| b != b'\\' && b != b'}') {
                if sep(&mut s) {
                    cb.font_name(Some(name));
                    return s.pos();
                }
            }
        }
    }
    // \fs
    {
        let mut s = Scan::new(buf);
        if s.lit_str(b"\\fs").is_some() {
            let mut reset = s;
            if sep(&mut reset) {
                cb.font_size(-1);
                return reset.pos();
            }
            if let Some(size) = s.uint(0) {
                if sep(&mut s) {
                    cb.font_size(size as u32 as i32);
                    return s.pos();
                }
            }
        }
    }
    // \a \aNN \an \anN
    {
        let try_align = |legacy: bool, valued: bool| -> Option<(i32, usize)> {
            let mut s = Scan::new(buf);
            s.lit_str(if legacy { b"\\a" } else { b"\\an" })?;
            let an = if valued { s.uint(if legacy { 2 } else { 1 })? as u32 as i32 } else { -1 };
            sep(&mut s).then_some((an, s.pos()))
        };
        if let Some((mut an, len)) =
            try_align(true, false).or_else(|| try_align(true, true)).or_else(|| try_align(false, false)).or_else(|| try_align(false, true))
        {
            if an != -1 && at(buf, 2) != b'n' {
                an = (an & 3) + if an & 4 != 0 { 6 } else if an & 8 != 0 { 3 } else { 0 };
            }
            cb.alignment(an);
            return len;
        }
    }
    // \r \rStyle
    {
        let mut s = Scan::new(buf);
        if s.lit_str(b"\\r").is_some() {
            let mut reset = s;
            if sep(&mut reset) {
                cb.cancel_overrides(b"");
                return reset.pos();
            }
            if let Some(style) = s.set(127, |b| b != b'\\' && b != b'}') {
                if sep(&mut s) {
                    cb.cancel_overrides(style);
                    return s.pos();
                }
            }
        }
    }
    // \move(x1,y1,x2,y2[,t1,t2]) \pos(x,y) \org(x,y)
    {
        let ints = |prefix: &[u8], n: usize| -> Option<(Vec<i32>, usize)> {
            let mut s = Scan::new(buf);
            s.lit_str(prefix)?;
            let mut v = Vec::with_capacity(n);
            for k in 0..n {
                if k > 0 {
                    s.lit(b',')?;
                }
                v.push(s.int(0)? as i32);
            }
            s.lit(b')')?;
            sep(&mut s).then_some((v, s.pos()))
        };
        if let Some((v, len)) = ints(b"\\move(", 4) {
            cb.move_to(v[0], v[1], v[2], v[3], -1, -1);
            return len;
        }
        if let Some((v, len)) = ints(b"\\move(", 6) {
            cb.move_to(v[0], v[1], v[2], v[3], v[4], v[5]);
            return len;
        }
        if let Some((v, len)) = ints(b"\\pos(", 2) {
            cb.move_to(v[0], v[1], v[0], v[1], -1, -1);
            return len;
        }
        if let Some((v, len)) = ints(b"\\org(", 2) {
            cb.origin(v[0], v[1]);
            return len;
        }
    }
    // \kfN \koN \kN \KN
    {
        let karaoke = |prefix: &[u8]| -> Option<(u32, usize)> {
            let mut s = Scan::new(buf);
            s.lit_str(prefix)?;
            let duration = s.uint(0)? as u32;
            sep(&mut s).then_some((duration, s.pos()))
        };
        if let Some((duration, len)) = karaoke(b"\\kf").or_else(|| karaoke(b"\\ko")).or_else(|| karaoke(b"\\k")).or_else(|| karaoke(b"\\K")) {
            cb.karaoke(duration);
            return len;
        }
    }
    // unknown code: skip to the next `\` or `}`
    strcspn(&buf[1.min(buf.len())..], b"\\}") + 2
}

/// `ff_ass_split_override_codes`. `Err` is FFmpeg's `AVERROR_INVALIDDATA`
/// for an override block not closed by `}`: everything before it has been
/// reported and `end` is not called.
pub fn split_override_codes(cb: &mut dyn OverrideCallbacks, text: &[u8]) -> Result<(), ()> {
    let buf = crate::scan::c_str(text);
    let mut i = 0usize;
    let mut pending: Option<usize> = None;
    let is_new_line = |i: usize| at(buf, i) == b'\\' && matches!(at(buf, i + 1), b'n' | b'N');
    let is_block = |i: usize| at(buf, i) == b'{' && at(buf, i + 1) == b'\\';
    while i < buf.len() {
        if let Some(start) = pending {
            if is_new_line(i) || is_block(i) {
                cb.text(&buf[start..i]);
                pending = None;
            }
        }
        if is_new_line(i) {
            cb.new_line(at(buf, i + 1) == b'N');
            i += 2;
        } else if is_block(i) {
            i += 1;
            while at(buf, i) == b'\\' {
                let len = override_code(cb, &buf[i..]);
                i += len - 1;
            }
            if at(buf, i) != b'}' {
                return Err(());
            }
            i += 1;
        } else {
            if pending.is_none() {
                pending = Some(i);
            }
            i += 1;
        }
    }
    if let Some(start) = pending {
        cb.text(&buf[start..]);
    }
    cb.end();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl OverrideCallbacks for Log {
        fn text(&mut self, text: &[u8]) {
            self.0.push(format!("text {}", String::from_utf8_lossy(text)));
        }
        fn new_line(&mut self, forced: bool) {
            self.0.push(format!("nl {forced}"));
        }
        fn style(&mut self, style: u8, close: i32) {
            self.0.push(format!("style {} {close}", style as char));
        }
        fn color(&mut self, color: u32, id: u32) {
            self.0.push(format!("color {color:x} {id}"));
        }
        fn alignment(&mut self, an: i32) {
            self.0.push(format!("an {an}"));
        }
        fn move_to(&mut self, x1: i32, y1: i32, x2: i32, y2: i32, t1: i32, t2: i32) {
            self.0.push(format!("move {x1} {y1} {x2} {y2} {t1} {t2}"));
        }
        fn cancel_overrides(&mut self, style: &[u8]) {
            self.0.push(format!("reset {}", String::from_utf8_lossy(style)));
        }
        fn end(&mut self) {
            self.0.push("end".into());
        }
    }

    fn log(text: &str) -> (Vec<String>, Result<(), ()>) {
        let mut log = Log::default();
        let result = split_override_codes(&mut log, text.as_bytes());
        (log.0, result)
    }

    #[test]
    fn override_codes_match_ffmpeg_callbacks() {
        assert_eq!(
            log(r"{\an5\pos(352,54)\move(352,-101,352,54,0,400)\bord3\2c&HD4B5CB&\fad(300,0)}i\Nj{\b1}k{\b}{\c}{comment}"),
            (vec![
                "an 5".into(),
                "move 352 54 352 54 -1 -1".into(),
                "move 352 -101 352 54 0 400".into(),
                "color d4b5cb 2".into(),
                "text i".into(),
                "nl true".into(),
                "text j".into(),
                "style b 0".into(),
                "text k".into(),
                "style b -1".into(),
                "color ffffffff 0".into(),
                "text {comment}".into(),
                "end".into(),
            ], Ok(()))
        );
        // A legacy \a6 (top, centre) converts to numpad 8; \r resets.
        assert_eq!(log(r"{\a6\rAlt}x\n").0, vec!["an 8", "reset Alt", "text x", "nl false", "end"]);
        // An unterminated block stops the event after its parsed codes,
        // without `end`.
        assert_eq!(log(r"a{\b1 b").0, vec!["text a".to_string(), "style b 0".to_string()]);
        assert_eq!(log(r"a{\b1 b").1, Err(()));
    }

    #[test]
    fn styles_follow_format_order_and_v4_alignment() {
        let header = split_header(
            b"[Script Info]\r\nScriptType: v4.00\r\n\r\n[V4 Styles]\r\nFormat: Name, Fontname, PrimaryColour, Bold, Alignment\r\n\
              Style: Default ,Arial,16776960,-1,7\r\n[Events]\r\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n",
        );
        assert_eq!(header.styles.len(), 1);
        let style = &header.styles[0];
        assert_eq!(style.name.as_deref(), Some("Default "));
        assert_eq!((style.primary_color, style.bold, style.alignment), (0xffff00, -1, 9));
        assert!(header.style("Default").is_none());
        let dialog = split_dialog(b"7,0, Yellow,Name,0,0,0,fx,  Hi, there");
        assert_eq!((dialog.readorder, dialog.style, dialog.text), (7, &b"Yellow"[..], &b"Hi, there"[..]));
    }
}

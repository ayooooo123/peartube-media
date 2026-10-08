// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
// Copyright (C) 2009 Grigori Goronzy <greg@geekmind.org>
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Adapted from libass 0.17.5 (4a05d81), ass_font.c and ass_shaper.c.
// FreeType/HarfBuzz/FriBidi replaced by ttf-parser/rustybuzz/unicode-bidi.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use unicode_bidi::{BidiInfo, Level};
use unicode_script::{Script, UnicodeScript};

use crate::fontselect::{FontOptions, FontSelector, SelectedFace};
use crate::outline::{GlyphOutliner, Outline, Vector, stroke};
use crate::sfnt::{STYLE_BOLD, STYLE_ITALIC};

#[derive(Clone, Debug, PartialEq)]
pub struct TextStyle {
    pub family: String,
    pub size: f64,
    pub weight: i32,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    pub spacing: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    /// CSS uses em sizing. ASS uses Windows ascender + descender sizing.
    pub em_size: bool,
    pub language: Option<String>,
}

impl Default for TextStyle {
    fn default() -> Self {
        Self { family: "sans-serif".into(), size: 20.0, weight: 400, italic: false, underline: false, strike: false, spacing: 0.0, scale_x: 1.0, scale_y: 1.0, em_size: false, language: None }
    }
}

#[derive(Clone, Debug)]
pub struct Span {
    pub range: Range<usize>,
    pub style: TextStyle,
}

/// A positioned glyph in logical cluster order; offsets and advances are pixels.
#[derive(Clone, Debug)]
pub struct Glyph {
    pub cluster: usize,
    pub end: usize,
    pub span: usize,
    pub level: Level,
    pub advance: f64,
    pub offset: (f64, f64),
    pub ascender: f64,
    pub descender: f64,
    pub outline: Arc<Outline>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct OutlineKey {
    face: usize,
    glyph: u16,
    size: u64,
    weight: i32,
    flags: u8,
}

pub struct Shaper {
    pub fonts: FontSelector,
    outlines: HashMap<OutlineKey, Arc<Outline>>,
    outline_points: usize,
    buffer: Option<rustybuzz::UnicodeBuffer>,
}

/// The line metrics libass gets after overriding FreeType's metrics with GDI's.
fn metrics(face: &ttf_parser::Face<'_>) -> (f64, f64) {
    if let Some(os2) = face.tables().os2 {
        let a = f64::from(os2.windows_ascender());
        let d = -f64::from(os2.windows_descender());
        if a + d > 0.0 { return (a, d); }
    }
    let (a, d) = (f64::from(face.ascender()), -f64::from(face.descender()));
    if a + d > 0.0 { return (a, d); }
    let bbox = face.global_bounding_box();
    (f64::from(bbox.y_max), -f64::from(bbox.y_min))
}

fn font_scale(face: &ttf_parser::Face<'_>, style: &TextStyle) -> f64 {
    let (a, d) = metrics(face);
    let units = if style.em_size { f64::from(face.units_per_em()) } else { a + d };
    // FreeType's REAL_DIM request and 16.16 scale, with a 26.6 size input.
    let requested = (style.size.clamp(0.0, 8192.0) * 64.0) as i64;
    ((requested as f64 * 65536.0 / units.max(1.0)).round() / 65536.0) / 64.0
}

impl Shaper {
    pub fn new(options: &FontOptions) -> Self {
        Self { fonts: FontSelector::new(options), outlines: HashMap::new(), outline_points: 0, buffer: None }
    }

    pub fn add_font(&mut self, bytes: Arc<[u8]>) -> bool {
        if !self.fonts.add_font(bytes) { return false; }
        self.outlines.clear();
        self.outline_points = 0;
        true
    }

    fn outline(&mut self, selected: &SelectedFace, face: &ttf_parser::Face<'_>, glyph: u16, style: &TextStyle) -> Arc<Outline> {
        let vertical = style.family.starts_with('@');
        let flags = u8::from(style.italic) | u8::from(style.underline) << 1 | u8::from(style.strike) << 2 | u8::from(style.em_size) << 3 | u8::from(vertical) << 4;
        let key = OutlineKey { face: selected.id, glyph, size: style.size.to_bits(), weight: style.weight, flags };
        if let Some(outline) = self.outlines.get(&key) { return outline.clone(); }
        let scale = font_scale(face, style) * 64.0;
        let mut builder = GlyphOutliner::new(scale, scale);
        face.outline_glyph(ttf_parser::GlyphId(glyph), &mut builder);
        let mut outline = builder.finish().unwrap_or_default();
        if style.italic && selected.meta.style_flags & STYLE_ITALIC == 0 {
            let slant = if selected.meta.is_postscript { 0x2d24 } else { 0x5700 } as f64 / 65536.0;
            for p in &mut outline.points { p.x = (f64::from(p.x) - slant * f64::from(p.y)).round() as i32; }
        }
        if style.weight > selected.meta.weight + 150 && selected.meta.style_flags & STYLE_BOLD == 0 {
            let half_strength = (f64::from(face.units_per_em()) * scale / 128.0).round() as i32;
            if half_strength > 0 {
                if let Some((a, b)) = stroke(&outline, half_strength, half_strength, 4) {
                    outline.points.extend(a.points);
                    outline.segments.extend(a.segments);
                    outline.points.extend(b.points);
                    outline.segments.extend(b.segments);
                    for p in &mut outline.points { p.x += half_strength; p.y -= half_strength; }
                }
            }
        }
        let advance = f64::from(face.glyph_hor_advance(ttf_parser::GlyphId(glyph)).unwrap_or(0)) * scale;
        if vertical {
            let desc = face.tables().os2.map_or(0.0, |o| f64::from(o.typographic_descender()) * scale);
            let advance = f64::from(face.glyph_ver_advance(ttf_parser::GlyphId(glyph)).unwrap_or(face.units_per_em())) * scale;
            outline.rotate_90(Vector { x: (advance + desc).round() as i32, y: -desc.round() as i32 });
        }
        for decoration in [style.underline.then(|| face.underline_metrics()).flatten(), style.strike.then(|| face.strikeout_metrics()).flatten()].into_iter().flatten() {
            let height = (f64::from(decoration.thickness) * scale).round().max(1.0) as i32;
            let top = (-f64::from(decoration.position) * scale).round() as i32 - height / 2;
            if selected.meta.is_postscript { outline.add_rect(0, top + height, advance.round() as i32, top); }
            else { outline.add_rect(0, top, advance.round() as i32, top + height); }
        }
        if self.outline_points + outline.points.len() > 1 << 20 {
            self.outlines.clear();
            self.outline_points = 0;
        }
        self.outline_points += outline.points.len();
        let outline = Arc::new(outline);
        self.outlines.insert(key, outline.clone());
        outline
    }

    /// Shape in font/script/bidi/style runs. Clusters retain original byte
    /// offsets so a line break or karaoke boundary cannot split a ligature.
    pub fn shape(&mut self, text: &str, spans: &[Span], base_rtl: Option<bool>, kerning: bool) -> Vec<Glyph> {
        if text.len() > 64 << 10 || spans.is_empty() { return Vec::new(); }
        if self.fonts.is_empty() { return bitmap_fallback(text, spans, base_rtl); }
        let chars: Vec<_> = text.char_indices().take(8192).collect();
        let bidi = BidiInfo::new(text, base_rtl.map(|rtl| if rtl { Level::rtl() } else { Level::ltr() }));
        let mut scripts: Vec<_> = chars.iter().map(|(_, ch)| ch.script()).collect();
        let weak = |s| matches!(s, Script::Common | Script::Inherited | Script::Unknown);
        let mut previous = Script::Unknown;
        for script in &mut scripts { if weak(*script) { *script = previous; } else { previous = *script; } }
        previous = Script::Unknown;
        for script in scripts.iter_mut().rev() { if weak(*script) { *script = previous; } else { previous = *script; } }
        let mut selected: Vec<Option<Arc<SelectedFace>>> = Vec::with_capacity(chars.len());
        let mut span_ids = Vec::with_capacity(chars.len());
        let mut span_id = 0;
        for &(at, ch) in &chars {
            while span_id + 1 < spans.len() && at >= spans[span_id].range.end { span_id += 1; }
            span_ids.push(span_id);
            let style = &spans[span_id].style;
            let code = if ch.is_control() || matches!(ch, '\u{200c}' | '\u{200d}') { 0 } else { ch as u32 };
            selected.push(self.fonts.select(&style.family, style.weight, style.italic, code)
                .or_else(|| self.fonts.select(&style.family, style.weight, style.italic, 0)));
        }
        let mut out = Vec::new();
        let mut start = 0;
        while start < chars.len() {
            let Some(selected_face) = &selected[start] else { start += 1; continue };
            let level = bidi.levels[chars[start].0];
            let span_id = span_ids[start];
            let style = &spans[span_id].style;
            let mut end = start + 1;
            while end < chars.len() && span_ids[end] == span_id && scripts[end] == scripts[start]
                && bidi.levels[chars[end].0] == level && selected[end].as_ref().is_some_and(|f| f.id == selected_face.id)
                && chars[end - 1].1 != '\n' && chars[end].1 != '\n' { end += 1; }
            let Some(face) = selected_face.face() else { start = end; continue };
            let scale = font_scale(&face, style);
            let (asc, desc) = metrics(&face);
            let mut hb_face = rustybuzz::Face::from_face(face.clone());
            let ppem = (f64::from(face.units_per_em()) * scale).round().clamp(1.0, 65535.0) as u16;
            hb_face.set_pixels_per_em(Some((ppem, ppem)));
            let mut buffer = self.buffer.take().unwrap_or_default();
            for &(at, ch) in &chars[start..end] { buffer.add(ch, at as u32); }
            buffer.set_pre_context(&text[..chars[start].0]);
            buffer.set_post_context(&text[chars.get(end).map_or(text.len(), |c| c.0)..]);
            buffer.set_direction(if level.is_rtl() { rustybuzz::Direction::RightToLeft } else { rustybuzz::Direction::LeftToRight });
            if let Some(script) = rustybuzz::Script::from_iso15924_tag(ttf_parser::Tag(scripts[start].as_iso15924_tag())) { buffer.set_script(script); }
            if let Some(lang) = style.language.as_ref().and_then(|l| l.parse().ok()) { buffer.set_language(lang); }
            buffer.guess_segment_properties();
            let feature = |tag: &[u8; 4], enabled: bool| rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(tag), u32::from(enabled), ..);
            let features = [feature(b"kern", kerning), feature(b"liga", style.spacing == 0.0), feature(b"clig", style.spacing == 0.0), feature(b"vert", style.family.starts_with('@')), feature(b"vkna", style.family.starts_with('@'))];
            let shaped = rustybuzz::shape(&hb_face, &features, buffer);
            let run_end = chars.get(end).map_or(text.len(), |c| c.0);
            let mut cluster_ends: Vec<_> = shaped.glyph_infos().iter().map(|g| g.cluster as usize).collect();
            cluster_ends.sort_unstable();
            cluster_ends.dedup();
            cluster_ends.push(run_end);
            for (info, pos) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
                let cluster = info.cluster as usize;
                let ch = text[cluster..].chars().next().unwrap_or(' ');
                let outline = if ch.is_control() { Arc::new(Outline::default()) } else { self.outline(selected_face, &face, info.glyph_id as u16, style) };
                let mut advance = f64::from(pos.x_advance) * scale;
                if !style.em_size {
                    // Unhinted libass measures at 256 px, then scales down;
                    // fitting advances at the displayed size shifts whole lines.
                    let nominal_scale = ((256.0 * 64.0 * 65536.0 / (asc + desc).max(1.0)).round() / 65536.0) / 64.0;
                    let nominal = f64::from(face.glyph_hor_advance(ttf_parser::GlyphId(info.glyph_id as u16)).unwrap_or(0)) * nominal_scale;
                    advance += (nominal.round() - nominal) * style.size / 256.0;
                }
                if ch == '\n' { advance = 0.0; }
                let next = cluster_ends.partition_point(|&c| c <= cluster);
                out.push(Glyph { cluster, end: cluster_ends.get(next).copied().unwrap_or(run_end), span: span_id, level,
                    advance: (advance + style.spacing) * style.scale_x,
                    offset: (f64::from(pos.x_offset) * scale * style.scale_x, -f64::from(pos.y_offset) * scale * style.scale_y),
                    ascender: asc * scale * style.scale_y, descender: desc * scale * style.scale_y, outline });
            }
            self.buffer = Some(shaped.clear());
            start = end;
        }
        // Stable sort keeps a cluster's glyph order, including Indic marks.
        out.sort_by_key(|g| g.cluster);
        out
    }
}

/// Visual glyph order for one line, preserving each shaped cluster.
pub fn visual_order(glyphs: &[Glyph]) -> Vec<usize> {
    let mut clusters: Vec<Range<usize>> = Vec::new();
    for (i, glyph) in glyphs.iter().enumerate() {
        if let Some(last) = clusters.last_mut().filter(|r| glyphs[r.start].cluster == glyph.cluster) { last.end = i + 1; }
        else { clusters.push(i..i + 1); }
    }
    let levels: Vec<_> = clusters.iter().map(|r| glyphs[r.start].level).collect();
    BidiInfo::reorder_visual(&levels).into_iter().flat_map(|i| clusters[i].clone()).collect()
}

/// Last resort only: no runtime or embedded font loaded. It shares the
/// normal layout and raster path, rather than retaining a second renderer.
fn bitmap_fallback(text: &str, spans: &[Span], base_rtl: Option<bool>) -> Vec<Glyph> {
    let bidi = BidiInfo::new(text, base_rtl.map(|rtl| if rtl { Level::rtl() } else { Level::ltr() }));
    let mut out = Vec::new();
    let mut span = 0;
    for (cluster, ch) in text.char_indices().take(8192) {
        while span + 1 < spans.len() && cluster >= spans[span].range.end { span += 1; }
        let style = &spans[span].style;
        let font = if style.weight >= 600 { oxideav_subtitle::font::BitmapFont::default_bold() } else { oxideav_subtitle::font::BitmapFont::default_regular() };
        let (w, h) = (font.cell_w as usize, font.cell_h as usize);
        let mut pixels = vec![0; w * h * 4];
        font.draw_glyph(ch, &mut pixels, w as u32, h as u32, 0, font.bearing_y as i32, [255; 4]);
        let scale = style.size.clamp(0.0, 8192.0) / h as f64;
        let mut outline = Outline::default();
        if !ch.is_control() {
            for y in 0..h { for x in 0..w {
                if pixels[(y * w + x) * 4 + 3] == 0 { continue; }
                let shear = if style.italic { (h - y) as f64 * 0.2 } else { 0.0 };
                outline.add_rect(((x as f64 + shear) * scale * 64.0) as i32,
                    ((y as f64 - f64::from(font.bearing_y)) * scale * 64.0) as i32,
                    (((x + 1) as f64 + shear) * scale * 64.0) as i32,
                    (((y + 1) as f64 - f64::from(font.bearing_y)) * scale * 64.0) as i32);
            } }
        }
        out.push(Glyph { cluster, end: cluster + ch.len_utf8(), span, level: bidi.levels[cluster],
            advance: if ch == '\n' { 0.0 } else { (w as f64 * scale + style.spacing) * style.scale_x },
            offset: (0.0, 0.0), ascender: f64::from(font.bearing_y) * scale * style.scale_y,
            descender: (h as f64 - f64::from(font.bearing_y)) * scale * style.scale_y, outline: Arc::new(outline) });
    }
    out
}

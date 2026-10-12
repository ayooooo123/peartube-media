// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
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
// Adapted from libass 0.17.5 (4a05d81), ass_render.c: layout, transforms,
// collision placement and image ordering. Rasterization uses ab_glyph.

use std::ops::Range;
use std::sync::Arc;
use crate::bitmap::{self, Bitmap};
use crate::drawing;
use crate::fontselect::FontOptions;
use crate::outline::{Outline, stroke};
use crate::parse::{self, Clip, KaraokeKind, Parsed, Pen};
use crate::shaper::{Glyph, Shaper, Span, visual_order};
use crate::track::{Event, Feature, RenderPriv, Track};

const MAX_PIXELS: usize = 4096 * 4096;
const MAX_ACTIVE: usize = 64;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Image {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Straight-alpha RGBA.
    pub rgba: Vec<u8>,
}

#[derive(Default)]
pub struct Frame {
    pub image: Image,
    pub animated: bool,
    /// The next start/end boundary, in milliseconds.
    pub next_time: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default)]
struct Bounds { x: f64, y: f64, w: f64, h: f64 }
impl Bounds {
    fn overlaps(self, other: Self) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }
}

struct Paint { bitmap: Bitmap, colours: [u32; 2], split: f64, clip: Option<ClipMask> }
#[derive(Clone)]
enum ClipMask { Rect([f64; 4], bool), Vector(Arc<Bitmap>, bool) }
struct EventImage { index: usize, paints: Vec<Paint>, bounds: Bounds, collision: bool, down: bool, animated: bool }
struct Line { range: Range<usize>, width: f64, asc: f64, desc: f64 }

pub struct Renderer {
    pub shaper: Shaper,
    dimensions: (u32, u32),
    generation: i32,
}

impl Renderer {
    pub fn new(options: &FontOptions) -> Self { Self { shaper: Shaper::new(options), dimensions: (0, 0), generation: 0 } }

    pub fn render(&mut self, track: &mut Track, time: i64, width: u32, height: u32) -> Frame {
        if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS as u64 { return Frame::default(); }
        track.lazy_init();
        if self.dimensions != (width, height) { self.dimensions = (width, height); self.generation = self.generation.wrapping_add(1); }
        // Embedded script fonts are consumed once; attachments are added by the caller.
        for font in std::mem::take(&mut track.fonts) { self.shaper.add_font(font.data.into()); }
        let mut active: Vec<_> = track.events.iter().enumerate().filter(|(_, e)| e.start <= time && time < e.start.saturating_add(e.duration)).map(|(i, _)| i).take(MAX_ACTIVE).collect();
        active.sort_by_key(|&i| (track.events[i].layer, track.events[i].read_order));
        let mut events: Vec<_> = active.into_iter().filter_map(|i| self.event(track, &track.events[i], i, time, width, height)).collect();
        let mut layer_start = 0;
        while layer_start < events.len() {
            let layer = track.events[events[layer_start].index].layer;
            let mut layer_end = layer_start + 1;
            while layer_end < events.len() && track.events[events[layer_end].index].layer == layer { layer_end += 1; }
            self.collisions(track, &mut events[layer_start..layer_end]);
            layer_start = layer_end;
        }
        let mut rgba = vec![0; width as usize * height as usize * 4];
        let mut animated = false;
        for event in events {
            animated |= event.animated;
            for paint in event.paints { blend(&mut rgba, width, height, &paint); }
        }
        let next_time = track.events.iter().flat_map(|e| [e.start, e.start.saturating_add(e.duration)]).filter(|&t| t > time).min();
        Frame { image: crop(rgba, width, height), animated, next_time }
    }

    fn collisions(&self, track: &mut Track, events: &mut [EventImage]) {
        let mut used = Vec::new();
        let mut fixed = vec![false; events.len()];
        for (i, image) in events.iter_mut().enumerate() {
            if !image.collision || image.bounds.w <= 0.0 || image.bounds.h <= 0.0 { continue; }
            let event = &mut track.events[image.index];
            if let Some(saved) = event.render_priv.filter(|p| p.render_id == self.generation && f64::from(p.height) == image.bounds.h.round()) {
                let b = Bounds { x: f64::from(saved.left), y: f64::from(saved.top), w: f64::from(saved.width), h: f64::from(saved.height) };
                if !used.iter().any(|&u| b.overlaps(u)) { shift(image, b.y - image.bounds.y); used.push(b); fixed[i] = true; }
                else { event.render_priv = None; }
            }
        }
        for (i, image) in events.iter_mut().enumerate() {
            if fixed[i] || !image.collision || image.bounds.w <= 0.0 || image.bounds.h <= 0.0 { continue; }
            used.sort_by(|a, b| a.y.total_cmp(&b.y));
            let mut bounds = image.bounds;
            if image.down {
                for &b in &used { if bounds.overlaps(b) { bounds.y = b.y + b.h; } }
            } else {
                for &b in used.iter().rev() { if bounds.overlaps(b) { bounds.y = b.y - bounds.h; } }
            }
            shift(image, bounds.y - image.bounds.y);
            used.push(bounds);
            track.events[image.index].render_priv = Some(RenderPriv { top: bounds.y.round() as i32, left: bounds.x.round() as i32, width: bounds.w.round() as i32, height: bounds.h.round() as i32, render_id: self.generation });
        }
    }

    fn event(&mut self, track: &Track, event: &Event, index: usize, time: i64, width: u32, height: u32) -> Option<EventImage> {
        let mut parsed = parse::parse(track, event, time)?;
        let style = track.styles.get(event.style)?;
        let (sx, sy) = (f64::from(width) / f64::from(track.play_res_x.max(1)), f64::from(height) / f64::from(track.play_res_y.max(1)));
        let bx = if track.scaled_border_and_shadow { sx } else { 1.0 };
        let by = if track.scaled_border_and_shadow { sy } else { 1.0 };
        let margin_l = f64::from(if event.margin_l != 0 { event.margin_l } else { style.margin_l }) * sx;
        let margin_r = f64::from(if event.margin_r != 0 { event.margin_r } else { style.margin_r }) * sx;
        let margin_v = f64::from(if event.margin_v != 0 { event.margin_v } else { style.margin_v }) * sy;
        let max_width = (f64::from(width) - margin_l - margin_r).max(1.0);
        let effect = String::from_utf8_lossy(&event.effect);
        let effects: Vec<_> = effect.split(';').collect();
        if effect.starts_with("Banner;") { parsed.wrap = 2; parsed.collisions = false; parsed.animated = true; }
        if effect.starts_with("Scroll up;") || effect.starts_with("Scroll down;") { parsed.collisions = false; parsed.animated = true; }
        let mut spans = Vec::with_capacity(parsed.runs.len());
        for run in &parsed.runs {
            let mut text_style = run.pen.text.clone();
            text_style.size = (text_style.size * sy).clamp(0.0, 8192.0);
            text_style.spacing *= sx;
            text_style.language = track.language.as_ref().map(|s| String::from_utf8_lossy(s).into_owned());
            spans.push(Span { range: run.range.clone(), style: text_style });
        }
        let encoding = parsed.runs.last().map_or(0, |r| r.pen.encoding);
        let mut glyphs = self.shaper.shape(&parsed.text, &spans, if encoding == -1 { None } else { Some(encoding == 177 || encoding == 178) }, track.kerning);
        for (i, run) in parsed.runs.iter().enumerate() {
            let Some(drawing) = &run.drawing else { continue };
            glyphs.retain(|g| g.span != i);
            if let Some((outline, bbox)) = drawing::parse(&drawing.text) {
                if bbox.x_min > bbox.x_max || bbox.y_min > bbox.y_max { continue; }
                let factor = 2f64.powi(1 - drawing.scale.clamp(1, 30));
                let asc = f64::from(bbox.y_max - bbox.y_min) / 64.0 * factor;
                let shift = f64::from(bbox.y_max) * factor + drawing.baseline * 64.0;
                let matrix = [[factor * sx * run.pen.text.scale_x, 0.0, 0.0], [0.0, factor * sy * run.pen.text.scale_y, -shift * sy * run.pen.text.scale_y]];
                if let Some(outline) = Outline::transform_2d(&outline, &matrix) {
                    glyphs.push(Glyph { cluster: run.range.start, end: run.range.end, span: i, level: unicode_bidi::Level::ltr(), advance: f64::from(bbox.x_max) / 64.0 * factor * sx * run.pen.text.scale_x, offset: (0.0, 0.0), ascender: (asc + drawing.baseline) * sy * run.pen.text.scale_y, descender: -drawing.baseline * sy * run.pen.text.scale_y, outline: Arc::new(outline) });
                }
            }
        }
        glyphs.sort_by_key(|g| g.cluster);
        if glyphs.is_empty() { return None; }
        let lines = layout_lines(&parsed.text, &glyphs, max_width, parsed.wrap, track.feature(Feature::WrapUnicode));
        let total_height: f64 = lines.iter().map(|l| l.asc + l.desc).sum();
        let block_width = lines.iter().map(|l| l.width).fold(0.0, f64::max);
        let horizontal = parsed.alignment & 3;
        let vertical = parsed.alignment & 12;
        let anchor_x = match horizontal { 1 => 0.0, 3 => block_width, _ => block_width / 2.0 };
        let anchor_y = match vertical { 4 => 0.0, 8 => total_height / 2.0, _ => total_height };
        let (mut x, mut y) = if let Some([px, py]) = parsed.position { (px * sx - anchor_x, py * sy - anchor_y) }
            else { (margin_l + match horizontal { 1 => 0.0, 3 => max_width - block_width, _ => (max_width - block_width) / 2.0 }, match vertical { 4 => margin_v, 8 => (f64::from(height) - total_height) / 2.0, _ => f64::from(height) - margin_v - total_height }) };
        let elapsed = time.saturating_sub(event.start) as f64;
        let effect_num = |i: usize| effects.get(i).and_then(|s| s.parse::<f64>().ok()).filter(|v| v.is_finite()).unwrap_or(0.0);
        if effect.starts_with("Banner;") {
            let delay = (effect_num(1) / sx).max(1.0).trunc() * sx;
            x = if effect_num(2) != 0.0 { elapsed / delay * sx - block_width } else { f64::from(width) - elapsed / delay * sx };
        } else if effect.starts_with("Scroll up;") || effect.starts_with("Scroll down;") {
            let (top, bottom) = (effect_num(1).min(effect_num(2)), effect_num(1).max(effect_num(2)));
            let delay = (effect_num(3) / sy).max(1.0).trunc() * sy;
            y = if effect.starts_with("Scroll up;") { (bottom - elapsed / delay) * sy } else { (top + elapsed / delay) * sy - total_height };
            parsed.clip = Some(Clip::Rect { bounds: [0.0, top, f64::from(track.play_res_x), bottom], inverse: false });
        }
        let origin = parsed.origin.map(|[ox, oy]| [ox * sx, oy * sy]).unwrap_or([x + anchor_x, y + anchor_y]);
        let clip = clip_mask(&parsed, sx, sy);
        let mut positions = vec![(0.0, 0.0); glyphs.len()];
        let mut baseline = y;
        for line in &lines {
            baseline += line.asc;
            let align = if style.justify != 0 { style.justify } else { horizontal };
            let mut cursor = x + match align { 1 => 0.0, 3 => block_width - line.width, _ => (block_width - line.width) / 2.0 };
            for local in visual_order(&glyphs[line.range.clone()]) {
                let i = line.range.start + local;
                positions[i] = (cursor + glyphs[i].offset.0, baseline + glyphs[i].offset.1);
                cursor += glyphs[i].advance;
            }
            baseline += line.desc;
        }
        let mut karaoke_bounds = std::collections::HashMap::<u32, (f64, f64)>::new();
        for (g, &(gx, _)) in glyphs.iter().zip(&positions) {
            if let Some(k) = parsed.runs[g.span].pen.karaoke {
                let bound = karaoke_bounds.entry(k.group).or_insert((gx, gx));
                bound.0 = bound.0.min(gx); bound.1 = bound.1.max(gx + g.advance);
            }
        }
        let visible: Vec<_> = lines.iter().flat_map(|l| l.range.clone()).collect();
        let mut shadows = Vec::new(); let mut borders = Vec::new(); let mut fills = Vec::new();
        let mut max_border: f64 = 0.0;
        for (run_id, run) in parsed.runs.iter().enumerate() {
            let pen = &run.pen;
            let (mut fill, mut border) = (None, None);
            let xborder = (pen.border[0] * bx).clamp(0.0, 1024.0);
            let yborder = (pen.border[1] * by).clamp(0.0, 1024.0);
            max_border = max_border.max(yborder);
            // VSFilter/libass includes the shadow displacement when finding
            // the rotation origin, even for the fill and border.
            let rotation_origin = [origin[0] - pen.shadow[0] * bx, origin[1] - pen.shadow[1] * by];
            for &i in &visible {
                let g = &glyphs[i];
                if g.span != run_id || g.outline.is_empty() { continue; }
                let (gx, gy) = positions[i];
                let mut path = if run.drawing.is_some() { (*g.outline).clone() }
                    else { Outline::transform_2d(&g.outline, &[[pen.text.scale_x, 0.0, 0.0], [0.0, pen.text.scale_y, 0.0]])? };
                let matrix = transform(pen, gx, gy, rotation_origin, g.ascender);
                let border_path = if pen.border_style == 3 {
                    let mut box_path = Outline::default();
                    box_path.add_rect((-xborder * 64.0) as i32, ((-g.ascender - yborder) * 64.0) as i32, ((g.advance + xborder) * 64.0) as i32, ((g.descender + yborder) * 64.0) as i32);
                    Some((box_path, Outline::default()))
                } else if xborder > 0.0 || yborder > 0.0 { stroke(&path, (xborder * 64.0) as i32, (yborder * 64.0) as i32, 4) } else { None };
                path = Outline::transform_3d(&path, &matrix)?;
                if let Some(bm) = bitmap::outline_to_bitmap(Some(&path), None) { merge_mask(&mut fill, bm); }
                if let Some((a, b)) = border_path {
                    let a = Outline::transform_3d(&a, &matrix)?;
                    let b = Outline::transform_3d(&b, &matrix)?;
                    if let Some(bm) = bitmap::outline_to_bitmap(Some(&a), Some(&b)) { merge_mask(&mut border, bm); }
                }
            }
            let Some(mut fill) = fill else { continue };
            let padding = if pen.be <= 3 { pen.be } else { (2.0 * f64::from(pen.be).sqrt()).ceil() as i32 };
            if padding > 0 { fill = pad(fill, padding)?; if let Some(b) = border.take() { border = pad(b, padding); } }
            // libass maps \blur to Gaussian sigma, then quantizes the radius.
            let radius = pen.blur * 2.0 / 256.0f64.ln().sqrt();
            let sigma = (((radius / 32.0).ln_1p() * 256.0).round_ties_even() / 256.0).exp_m1() * 32.0;
            if border.is_none() || pen.border_style == 3 {
                bitmap::synth_blur(&mut fill, pen.be, sigma * sigma, sigma * sigma);
            }
            if let Some(bm) = &mut border { bitmap::synth_blur(bm, pen.be, sigma * sigma, sigma * sigma); }
            let colours = pen.colours.map(|c| fade(c, pen.fade));
            let split = if let Some(k) = pen.karaoke {
                let bounds = karaoke_bounds.get(&k.group).copied().unwrap_or((x, x));
                if k.kind == KaraokeKind::Sweep { bounds.0 + (bounds.1 - bounds.0) * if elapsed < k.start { 0.0 } else if elapsed >= k.end { 1.0 } else { (elapsed - k.start) / (k.end - k.start) } }
                else if elapsed >= k.start { f64::INFINITY } else { f64::NEG_INFINITY }
            } else { f64::INFINITY };
            if pen.shadow != [0.0; 2] {
                let mut shadow = border.as_ref().unwrap_or(&fill).clone();
                shadow.left = shadow.left.saturating_add((pen.shadow[0] * bx).floor() as i32);
                shadow.top = shadow.top.saturating_add((pen.shadow[1] * by).floor() as i32);
                bitmap::shift_bitmap(&mut shadow, ((pen.shadow[0] * bx).rem_euclid(1.0) * 64.0) as i32, ((pen.shadow[1] * by).rem_euclid(1.0) * 64.0) as i32);
                shadows.push(Paint { bitmap: shadow, colours: [colours[3]; 2], split: f64::INFINITY, clip: clip.clone() });
            }
            if let Some(mut border) = border {
                let opaque_fill = colours[0] & 255 == 0 && colours[1] & 255 == 0;
                if pen.border_style != 3 && !opaque_fill { bitmap::fix_outline(&fill, &mut border); }
                if !pen.karaoke.is_some_and(|k| k.kind == KaraokeKind::Outline && elapsed < k.start) {
                    borders.push(Paint { bitmap: border, colours: [colours[2]; 2], split: f64::INFINITY, clip: clip.clone() });
                }
            }
            fills.push(Paint { bitmap: fill, colours: [colours[0], colours[1]], split, clip: clip.clone() });
        }
        shadows.extend(borders); shadows.extend(fills);
        Some(EventImage { index, paints: shadows, bounds: Bounds { x: x.round(), y: (y - max_border).round(), w: block_width.round(), h: (total_height + 2.0 * max_border).round() }, collision: parsed.collisions, down: vertical != 0, animated: parsed.animated })
    }
}

fn shift(image: &mut EventImage, amount: f64) {
    let amount = amount.round() as i32;
    image.bounds.y += f64::from(amount);
    for paint in &mut image.paints { paint.bitmap.top = paint.bitmap.top.saturating_add(amount); }
}

fn transform(pen: &Pen, x: f64, y: f64, origin: [f64; 2], ascender: f64) -> [[f64; 3]; 3] {
    let [rx, ry, rz] = pen.rotation.map(f64::to_radians);
    let (sz, cz) = (-rz).sin_cos(); let (sx, cx) = (-rx).sin_cos(); let (sy, cy) = ry.sin_cos();
    let [fax, fay] = pen.shear;
    let x1 = [1.0, fax, (x - origin[0] + ascender * fax) * 64.0];
    let y1 = [fay, 1.0, (y - origin[1]) * 64.0];
    let distance = 20000.0;
    let mut matrix = [[0.0; 3]; 3];
    for i in 0..3 {
        let x2 = x1[i] * cz - y1[i] * sz;
        let y2 = x1[i] * sz + y1[i] * cz;
        let y3 = y2 * cx; let z3 = y2 * sx;
        let x4 = x2 * cy - z3 * sy;
        let z4 = x2 * sy + z3 * cy + if i == 2 { distance } else { 0.0 };
        matrix[0][i] = x4 * distance + z4 * origin[0] * 64.0;
        matrix[1][i] = y3 * distance + z4 * origin[1] * 64.0;
        matrix[2][i] = z4;
    }
    matrix
}

fn layout_lines(text: &str, glyphs: &[Glyph], max_width: f64, wrap: i32, unicode: bool) -> Vec<Line> {
    let breaks: std::collections::HashSet<_> = if unicode { unicode_linebreak::linebreaks(text).map(|(i, _)| i).collect() } else { Default::default() };
    let ch = |i: usize| text[glyphs[i].cluster..].chars().next().unwrap_or(' ');
    let mut ranges: Vec<(Range<usize>, bool)> = Vec::new();
    let (mut start, mut at, mut width, mut last_break) = (0, 0, 0.0, None);
    while at < glyphs.len() {
        if ch(at) == '\n' {
            ranges.push((start..at, true)); start = at + 1; width = 0.0; last_break = None;
        } else {
            if wrap != 2 && width + glyphs[at].advance >= max_width && ch(at) != ' ' {
                if let Some(br) = last_break.filter(|&b| b > start) {
                    ranges.push((start..br, false)); start = br; at = br; width = 0.0; last_break = None; continue;
                }
            }
            width += glyphs[at].advance;
            let next_cluster = at + 1 == glyphs.len() || glyphs[at + 1].cluster != glyphs[at].cluster;
            if next_cluster && (ch(at) == ' ' || breaks.contains(&glyphs[at].end)) { last_break = Some(at + 1); }
        }
        at += 1;
    }
    ranges.push((start..glyphs.len(), true));
    let measure = |r: &Range<usize>| glyphs[r.clone()].iter().map(|g| g.advance).sum::<f64>();
    if wrap == 0 || wrap == 3 {
        for _ in 0..glyphs.len() {
            let mut changed = false;
            for i in 0..ranges.len().saturating_sub(1) {
                if ranges[i].1 { continue; }
                let (a, b) = (ranges[i].0.clone(), ranges[i + 1].0.clone());
                let mut word = a.end;
                while word > a.start && ch(word - 1) == ' ' { word -= 1; }
                while word > a.start && ch(word - 1) != ' ' && !breaks.contains(&glyphs[word - 1].end) { word -= 1; }
                if word <= a.start { continue; }
                let (old_a, old_b) = (measure(&a), measure(&b));
                let (new_a, new_b) = (measure(&(a.start..word)), measure(&(word..b.end)));
                if (new_a - new_b).abs() < (old_a - old_b).abs() && new_b <= max_width {
                    ranges[i].0.end = word; ranges[i + 1].0.start = word; changed = true;
                }
            }
            if !changed { break; }
        }
    }
    ranges.into_iter().map(|(mut range, _)| {
        while range.start < range.end && ch(range.start) == ' ' { range.start += 1; }
        while range.start < range.end && ch(range.end - 1) == ' ' { range.end -= 1; }
        let asc = glyphs[range.clone()].iter().map(|g| g.ascender).fold(0.0, f64::max);
        let desc = glyphs[range.clone()].iter().map(|g| g.descender).fold(0.0, f64::max);
        let (asc, desc) = if range.is_empty() { (glyphs[0].ascender * 0.5, glyphs[0].descender * 0.5) } else { (asc, desc) };
        Line { width: measure(&range), range, asc, desc }
    }).collect()
}

fn clip_mask(parsed: &Parsed, sx: f64, sy: f64) -> Option<ClipMask> {
    match parsed.clip.as_ref()? {
        Clip::Rect { bounds: [x0, y0, x1, y1], inverse } => Some(ClipMask::Rect([x0 * sx, y0 * sy, x1 * sx, y1 * sy], *inverse)),
        Clip::Vector { text, scale, inverse } => {
            let (outline, _) = drawing::parse(text)?;
            let factor = 2f64.powi(1 - (*scale).clamp(1, 30));
            let outline = Outline::transform_2d(&outline, &[[factor * sx, 0.0, 0.0], [0.0, factor * sy, 0.0]])?;
            Some(ClipMask::Vector(Arc::new(bitmap::outline_to_bitmap(Some(&outline), None)?), *inverse))
        }
    }
}

fn merge_mask(dst: &mut Option<Bitmap>, src: Bitmap) {
    let Some(old) = dst.take() else { *dst = Some(src); return };
    let left = old.left.min(src.left); let top = old.top.min(src.top);
    let right = (i64::from(old.left) + i64::from(old.w)).max(i64::from(src.left) + i64::from(src.w));
    let bottom = (i64::from(old.top) + i64::from(old.h)).max(i64::from(src.top) + i64::from(src.h));
    let Some(mut joined) = i32::try_from(right - i64::from(left)).ok().zip(i32::try_from(bottom - i64::from(top)).ok()).and_then(|(w,h)| Bitmap::alloc(w,h)) else { *dst = Some(old); return };
    joined.left = left; joined.top = top;
    for bm in [old, src] {
        for y in 0..bm.h as usize {
            let start = (i64::from(bm.top) - i64::from(top)) as usize * joined.stride + y * joined.stride + (i64::from(bm.left) - i64::from(left)) as usize;
            for (d, &s) in joined.buffer[start..start + bm.w as usize].iter_mut().zip(bm.row(y)) { *d = d.saturating_add(s); }
        }
    }
    *dst = Some(joined);
}

fn pad(bitmap: Bitmap, n: i32) -> Option<Bitmap> {
    let mut out = Bitmap::alloc(bitmap.w.checked_add(2 * n)?, bitmap.h.checked_add(2 * n)?)?;
    out.left = bitmap.left.saturating_sub(n); out.top = bitmap.top.saturating_sub(n);
    for y in 0..bitmap.h as usize {
        let at = (y + n as usize) * out.stride + n as usize;
        out.buffer[at..at + bitmap.w as usize].copy_from_slice(bitmap.row(y));
    }
    Some(out)
}

fn fade(colour: u32, fade: f64) -> u32 {
    let a = colour & 255; let f = fade.clamp(0.0, 255.0) as u32;
    (colour & !255) | (a + f - (a * f + 127) / 255)
}

fn coverage(mask: &Bitmap, x: i32, y: i32) -> u8 {
    let (x, y) = (i64::from(x) - i64::from(mask.left), i64::from(y) - i64::from(mask.top));
    if x < 0 || y < 0 || x >= i64::from(mask.w) || y >= i64::from(mask.h) { 0 } else { mask.buffer[y as usize * mask.stride + x as usize] }
}

fn blend(rgba: &mut [u8], width: u32, height: u32, paint: &Paint) {
    let mask = &paint.bitmap;
    let x0 = mask.left.clamp(0, width as i32); let y0 = mask.top.clamp(0, height as i32);
    let x1 = (i64::from(mask.left) + i64::from(mask.w)).clamp(0, i64::from(width)) as i32;
    let y1 = (i64::from(mask.top) + i64::from(mask.h)).clamp(0, i64::from(height)) as i32;
    for y in y0..y1 { for x in x0..x1 {
        let mut coverage = u32::from(coverage(mask, x, y));
        let clip = match &paint.clip {
            None => 255,
            Some(ClipMask::Rect([x0,y0,x1,y1], inverse)) => {
                let inside = f64::from(x) >= *x0 && f64::from(x) < *x1 && f64::from(y) >= *y0 && f64::from(y) < *y1;
                if inside != *inverse { 255 } else { 0 }
            }
            Some(ClipMask::Vector(mask, inverse)) => { let a = self::coverage(mask, x, y); u32::from(if *inverse { 255 - a } else { a }) }
        };
        coverage = coverage * clip / 255;
        let colour = paint.colours[usize::from(f64::from(x) >= paint.split)].to_be_bytes();
        let alpha = coverage * (255 - u32::from(colour[3])) / 255;
        if alpha == 0 { continue; }
        let at = (y as usize * width as usize + x as usize) * 4;
        let dst = &mut rgba[at..at + 4];
        let da = u32::from(dst[3]); let inv = 255 - alpha; let oa = alpha + da * inv / 255;
        for c in 0..3 { dst[c] = ((u32::from(colour[c]) * alpha + u32::from(dst[c]) * da * inv / 255) / oa) as u8; }
        dst[3] = oa as u8;
    } }
}

pub fn crop(rgba: Vec<u8>, width: u32, height: u32) -> Image {
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for (i, px) in rgba.chunks_exact(4).enumerate() {
        if px[3] == 0 { continue; }
        let (x, y) = (i as u32 % width, i as u32 / width);
        left = left.min(x); top = top.min(y); right = right.max(x + 1); bottom = bottom.max(y + 1);
    }
    if left >= right { return Image::default(); }
    let mut pixels = Vec::with_capacity((right - left) as usize * (bottom - top) as usize * 4);
    for y in top..bottom { pixels.extend_from_slice(&rgba[(y as usize * width as usize + left as usize) * 4..(y as usize * width as usize + right as usize) * 4]); }
    Image { x: left as i32, y: top as i32, width: right - left, height: bottom - top, rgba: pixels }
}

//! WebVTT cue layout (W3C §7.3–§7.4), using runtime fonts and shared
//! shaping. CSS boxes, ruby, bidi and cue placement remain WebVTT-specific.

use subs_text::webvtt_css::{cascade, Rgba, Style, StyleSheet, DEFAULT_BACKGROUND};
use subs_text::webvtt_cue::{Kind, Node};
use unicode_bidi::BidiInfo;
use subs_render::{bitmap::{Bitmap, outline_to_bitmap}, outline::Outline, shaper::{Shaper, Span, TextStyle}};

use crate::backend::SubtitleImage;

/// Default line-height as a multiple of the 5vh font size.
pub(crate) const LINE: f32 = 1.2;
/// Most characters a cue lays out (a hostile cue cannot cost more).
const MAX_ATOMS: usize = 8192;
/// Largest side of a rendered block, in pixels.
const MAX_SIDE: i64 = 8192;
/// Most pixels of a rendered block: the text canvas's bound
/// (`subs::MAX_CANVAS_PIXELS`); a taller block could not show anyway.
const MAX_PIXELS: i64 = 4096 * 4096;

/// How a line sits in the cue box.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Align {
    Start,
    Center,
    End,
    Left,
    Right,
}

/// A cue's text drawn as one block.
pub(crate) struct Block {
    /// The drawn pixels, cropped: `x`/`y` from the block's top-left (the
    /// cue box's start, the first line's top).
    pub image: SubtitleImage,
    /// The block's height: its line boxes'.
    pub height: i64,
    /// The first line box's height (the step snap-to-lines moves by).
    pub first_line: i64,
}

/// Whether the cue text's base direction is right-to-left: its first
/// strong character's (§3 computed position alignment).
pub(crate) fn is_rtl(nodes: &[Node]) -> bool {
    let text = subs_text::webvtt_cue::plain_text(nodes);
    unicode_bidi::get_base_direction(text.as_str()) == unicode_bidi::Direction::Rtl
}

/// One drawing style: the cascade's, with the opacity of its ancestors
/// folded in.
#[derive(Clone, Debug)]
struct Pen {
    style: Style,
    alpha: f32,
}

/// One character in logical order.
#[derive(Clone, Debug)]
struct Char {
    ch: char,
    pen: usize,
    /// The elements whose background box contains it, outermost first.
    boxes: Vec<usize>,
    advance: f32,
    ascender: f32,
    descender: f32,
    masks: Vec<Bitmap>,
}

impl Char {
    fn new(ch: char, pen: usize, boxes: Vec<usize>) -> Self {
        Self { ch, pen, boxes, advance: 0.0, ascender: 0.0, descender: 0.0, masks: Vec::new() }
    }
}
#[derive(Clone, Debug)]
enum Atom {
    Char(Char),
    /// A ruby base with its annotation, laid out as one unit.
    Ruby { base: Vec<Char>, text: Vec<Char>, rt_pen: usize, rt_box: Option<usize> },
    Break,
}

/// The background box of an element: its colour and opacity.
#[derive(Clone, Debug)]
struct BoxPaint {
    color: Rgba,
    alpha: f32,
}

struct Flat {
    pens: Vec<Pen>,
    boxes: Vec<BoxPaint>,
    atoms: Vec<Atom>,
    remaining: usize,
}

fn flatten(nodes: &[Node], styles: &[Style]) -> Flat {
    let mut flat = Flat { pens: Vec::new(), boxes: Vec::new(), atoms: Vec::new(), remaining: MAX_ATOMS };
    let root = &styles[0];
    flat.pens.push(Pen { style: root.clone(), alpha: root.opacity });
    let mut root_boxes = Vec::new();
    if let Some(color) = root.background {
        flat.boxes.push(BoxPaint { color, alpha: root.opacity });
        root_boxes.push(0);
    }
    let mut next_style = 1;
    walk(nodes, styles, &mut next_style, 0, &root_boxes, &mut flat, None);
    flat
}

/// `ruby`: when inside a ruby element, where its base and text go.
fn walk(
    nodes: &[Node],
    styles: &[Style],
    next_style: &mut usize,
    pen: usize,
    boxes: &[usize],
    flat: &mut Flat,
    mut ruby: Option<(&mut Vec<Char>, &mut Vec<Char>, &mut Option<(usize, Option<usize>)>)>,
) {
    for node in nodes {
        if flat.remaining == 0 {
            return;
        }
        match node {
            Node::Text(text) => {
                for ch in text.chars().take(flat.remaining) {
                    flat.remaining -= 1;
                    if ch == '\n' {
                        flat.atoms.push(Atom::Break);
                        continue;
                    }
                    let c = Char::new(ch, pen, boxes.to_vec());
                    match ruby.as_mut() {
                        Some((base, _, _)) => base.push(c),
                        None => flat.atoms.push(Atom::Char(c)),
                    }
                }
            }
            Node::Timestamp(_) => {}
            Node::Element { kind, children, .. } => {
                let style = styles.get(*next_style).cloned().unwrap_or_else(|| flat.pens[pen].style.clone());
                *next_style += 1;
                let alpha = flat.pens[pen].alpha * style.opacity;
                let background = if *kind == Kind::RubyText { Some(style.background.unwrap_or(DEFAULT_BACKGROUND)) } else { style.background };
                flat.pens.push(Pen { style, alpha });
                let child_pen = flat.pens.len() - 1;
                let mut child_boxes = boxes.to_vec();
                let own_box = background.map(|color| {
                    flat.boxes.push(BoxPaint { color, alpha });
                    flat.boxes.len() - 1
                });
                match kind {
                    Kind::Ruby if ruby.is_none() => {
                        let (mut base, mut text, mut rt) = (Vec::new(), Vec::new(), None);
                        child_boxes.extend(own_box);
                        walk(children, styles, next_style, child_pen, &child_boxes, flat, Some((&mut base, &mut text, &mut rt)));
                        let (rt_pen, rt_box) = rt.unwrap_or((child_pen, None));
                        if !base.is_empty() || !text.is_empty() {
                            flat.atoms.push(Atom::Ruby { base, text, rt_pen, rt_box });
                        }
                    }
                    Kind::RubyText => {
                        // The annotation's characters go to the ruby's text,
                        // in the annotation's pen; its inner elements still
                        // take their place in the cascade's order.
                        *next_style += count_elements(children);
                        if let Some((_, text, rt)) = ruby.as_mut() {
                            **rt = Some((child_pen, own_box));
                            collect_chars(children, child_pen, text, &mut flat.remaining);
                        }
                    }
                    _ => {
                        child_boxes.extend(own_box);
                        let inner = ruby.as_mut().map(|(b, t, r)| (&mut **b, &mut **t, &mut **r));
                        walk(children, styles, next_style, child_pen, &child_boxes, flat, inner);
                    }
                }
            }
        }
    }
}

fn count_elements(nodes: &[Node]) -> usize {
    nodes.iter().map(|n| match n {
        Node::Element { children, .. } => 1 + count_elements(children),
        _ => 0,
    }).sum()
}

/// The characters of an annotation, in one pen.
fn collect_chars(nodes: &[Node], pen: usize, out: &mut Vec<Char>, remaining: &mut usize) {
    for node in nodes {
        if *remaining == 0 { return; }
        match node {
            Node::Text(text) => {
                for ch in text.chars().filter(|&c| c != '\n').take(*remaining) {
                    out.push(Char::new(ch, pen, Vec::new()));
                    *remaining -= 1;
                }
            }
            Node::Element { children, .. } => collect_chars(children, pen, out, remaining),
            Node::Timestamp(_) => {}
        }
    }
}


/// Where a line may break before `next`.
fn breakable(prev: char, next: char) -> bool {
    fn cjk(c: char) -> bool {
        matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}' | '\u{ac00}'..='\u{d7af}' | '\u{f900}'..='\u{faff}' | '\u{ff00}'..='\u{ffef}')
    }
    prev == ' ' || cjk(prev) || cjk(next)
}


/// A unit on a line: a character or a ruby, with its width.
#[derive(Clone, Debug)]
struct Unit {
    atom: usize,
    width: f32,
    /// The bidi level of its (first) character.
    level: u8,
}

struct Layout<'a> {
    flat: &'a Flat,
    line_height: f32,
}

impl Layout<'_> {
    fn size(&self, pen: usize) -> f32 {
        self.flat.pens[pen].style.size
    }

    fn advance(&self, c: &Char) -> f32 {
        c.advance
    }

    fn ruby_metrics(&self, text: &[Char], pen: usize) -> (f32, f32) {
        if text.is_empty() { return (0.0, 0.0); }
        let (asc, desc) = text.iter().fold((0.0f32, 0.0f32),
            |(a, d), c| (a.max(c.ascender), d.max(c.descender)));
        // Layout and painting share one rounded line box. Rounding only
        // the reserved height lets small-font descenders enter the base box.
        let height = (self.line_height * self.size(pen) * 0.5).max(asc + desc).ceil();
        (height, asc + (height - asc - desc) / 2.0)
    }

    fn width(&self, atom: &Atom) -> f32 {
        match atom {
            Atom::Char(c) => self.advance(c),
            Atom::Ruby { base, text, .. } => {
                let base_w: f32 = base.iter().map(|c| self.advance(c)).sum();
                let text_w: f32 = text.iter().map(|c| self.advance(c)).sum();
                base_w.max(text_w)
            }
            Atom::Break => 0.0,
        }
    }
}

fn shape_chars(chars: &mut [&mut Char], pens: &[Pen], shaper: &mut Shaper, size: f32) {
    let mut text = String::new();
    let mut spans: Vec<Span> = Vec::new();
    let mut offsets = Vec::with_capacity(chars.len());
    let mut last_pen = None;
    for c in chars.iter() {
        offsets.push(text.len());
        let start = text.len();
        text.push(c.ch);
        if last_pen == Some(c.pen) { spans.last_mut().unwrap().range.end = text.len(); }
        else {
            let style = &pens[c.pen].style;
            spans.push(Span { range: start..text.len(), style: TextStyle {
                family: style.family.clone().unwrap_or_else(|| "sans-serif".into()),
                size: f64::from(size * style.size), weight: if style.bold { 700 } else { 400 },
                italic: style.italic, underline: style.underline, strike: style.line_through,
                em_size: true, ..TextStyle::default()
            } });
            last_pen = Some(c.pen);
        }
    }
    for glyph in shaper.shape(&text, &spans, None, true) {
        let Ok(i) = offsets.binary_search(&glyph.cluster) else { continue };
        let c = &mut chars[i];
        let x = f64::from(c.advance) + glyph.offset.0;
        if let Some(outline) = Outline::transform_2d(&glyph.outline, &[[1.0, 0.0, x * 64.0], [0.0, 1.0, glyph.offset.1 * 64.0]]) {
            if let Some(mask) = outline_to_bitmap(Some(&outline), None) { c.masks.push(mask); }
        }
        c.advance += glyph.advance as f32;
        c.ascender = c.ascender.max(glyph.ascender as f32);
        c.descender = c.descender.max(glyph.descender as f32);
    }
}

fn shape(flat: &mut Flat, shaper: &mut Shaper, size: f32) {
    let mut start = 0;
    while start < flat.atoms.len() {
        if matches!(flat.atoms[start], Atom::Char(_)) {
            let end = flat.atoms[start..].iter().position(|a| !matches!(a, Atom::Char(_))).map_or(flat.atoms.len(), |i| start + i);
            let mut chars: Vec<_> = flat.atoms[start..end].iter_mut().filter_map(|a| if let Atom::Char(c) = a { Some(c) } else { None }).collect();
            shape_chars(&mut chars, &flat.pens, shaper, size);
            start = end;
        } else {
            if let Atom::Ruby { base, text, .. } = &mut flat.atoms[start] {
                shape_chars(&mut base.iter_mut().collect::<Vec<_>>(), &flat.pens, shaper, size);
                shape_chars(&mut text.iter_mut().collect::<Vec<_>>(), &flat.pens, shaper, size * 0.5);
            }
            start += 1;
        }
    }
}

/// One laid-out line, in visual order.
struct Line {
    units: Vec<Unit>,
    rtl: bool,
    width: f32,
    /// The base text's line box, and the room ruby text takes above it.
    height: f32,
    ruby: f32,
    ascender: f32,
}

/// Lays out and draws `nodes`, styled by `sheet` (the cue's identifier
/// is `id`), lines at most `box_width` pixels wide, aligned by `align`.
/// `reversed`: the lines stack upward (the first lowest), as a
/// `vertical:lr` cue's turn needs. `None` when nothing is visible.
pub(crate) fn render(nodes: &[Node], sheet: &StyleSheet, id: &str, box_width: i64, align: Align, reversed: bool, font_size: f32, shaper: &mut Shaper) -> Option<Block> {
    if box_width <= 0 || box_width > MAX_SIDE {
        return None;
    }
    let styles = cascade(sheet, nodes, id);
    let mut flat = flatten(nodes, &styles);
    shape(&mut flat, shaper, font_size);
    let layout = Layout { flat: &flat, line_height: font_size * LINE };
    let lines = lines(&layout, box_width as f32);
    if lines.is_empty() {
        return None;
    }
    let height: f32 = lines.iter().map(|l| l.height + l.ruby).sum();
    let height = height.ceil() as i64;
    if height > MAX_SIDE || height.saturating_mul(box_width) > MAX_PIXELS {
        return None;
    }
    let first_line = (lines[0].height + lines[0].ruby).round() as i64;
    let (w, h) = (box_width as usize, height as usize);
    let mut canvas = Canvas { rgba: vec![0; w * h * 4], width: w, height: h };
    let mut top = 0.0f32;
    let order: Vec<&Line> = if reversed { lines.iter().rev().collect() } else { lines.iter().collect() };
    for line in order {
        let x0 = match (align, line.rtl) {
            (Align::Left, _) | (Align::Start, false) | (Align::End, true) => 0.0,
            (Align::Right, _) | (Align::End, false) | (Align::Start, true) => box_width as f32 - line.width,
            (Align::Center, _) => (box_width as f32 - line.width) / 2.0,
        };
        draw_line(&layout, &mut canvas, line, x0, top);
        top += line.height + line.ruby;
    }
    let image = crate::subs::visible_image(&canvas.rgba, w, h)?;
    Some(Block { image, height, first_line })
}

/// Breaks the atoms into lines (logical order), then reorders each line.
fn lines(layout: &Layout, max_width: f32) -> Vec<Line> {
    let atoms = &layout.flat.atoms;
    let mut out = Vec::new();
    let mut start = 0;
    while start <= atoms.len() {
        let end = atoms[start..].iter().position(|a| matches!(a, Atom::Break)).map_or(atoms.len(), |i| start + i);
        // A final line break ends the last line; it opens no new one.
        if start == end && end == atoms.len() && start > 0 {
            break;
        }
        out.extend(paragraph(layout, start..end, max_width));
        start = end + 1;
    }
    out
}

/// The character a unit stands for in bidi analysis.
fn bidi_char(atom: &Atom) -> char {
    match atom {
        Atom::Char(c) => c.ch,
        Atom::Ruby { base, .. } => base.first().map_or('\u{fffc}', |c| c.ch),
        Atom::Break => '\n',
    }
}

fn paragraph(layout: &Layout, range: std::ops::Range<usize>, max_width: f32) -> Vec<Line> {
    let atoms = &layout.flat.atoms[range.clone()];
    if atoms.is_empty() {
        // An empty line between breaks keeps its height.
        return vec![Line { units: Vec::new(), rtl: false, width: 0.0, height: layout.line_height, ruby: 0.0, ascender: layout.line_height * 0.8 }];
    }
    let text: String = atoms.iter().map(bidi_char).collect();
    let bidi = BidiInfo::new(&text, None);
    let Some(para) = bidi.paragraphs.first() else { return Vec::new() };
    let rtl = para.level.is_rtl();
    // Byte offset of each atom's character.
    let offsets: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    let widths: Vec<f32> = atoms.iter().map(|a| layout.width(a)).collect();
    // Greedy breaking in logical order: at spaces and around CJK.
    let mut lines: Vec<std::ops::Range<usize>> = Vec::new();
    let (mut line_start, mut used, mut last_break) = (0, 0.0f32, None);
    let chars: Vec<char> = atoms.iter().map(bidi_char).collect();
    let mut i = 0;
    while i < atoms.len() {
        if i > line_start && breakable(chars[i - 1], chars[i]) {
            last_break = Some(i);
        }
        let over = used + widths[i] > max_width + 0.01 && chars[i] != ' ';
        if over && i > line_start {
            let at = last_break.filter(|&b| b > line_start).unwrap_or(i);
            lines.push(line_start..at);
            // Spaces at a break are not carried to the next line.
            line_start = at;
            while line_start < atoms.len() && chars[line_start] == ' ' {
                line_start += 1;
            }
            i = i.max(line_start);
            used = widths[line_start..i].iter().sum();
            last_break = None;
            continue;
        }
        used += widths[i];
        i += 1;
    }
    if line_start < atoms.len() {
        lines.push(line_start..atoms.len());
    }
    lines
        .into_iter()
        .map(|line| {
            let byte_range = offsets[line.start]..offsets.get(line.end).copied().unwrap_or(text.len());
            let levels = bidi.reordered_levels_per_char(para, byte_range);
            // Trailing spaces hang: not part of the width.
            let mut end = line.end;
            while end > line.start && chars[end - 1] == ' ' {
                end -= 1;
            }
            let mut units: Vec<Unit> = (line.start..end)
                .map(|i| Unit { atom: range.start + i, width: widths[i], level: levels[i].number() })
                .collect();
            reorder(&mut units);
            let width = units.iter().map(|u| u.width).sum();
            let (mut scale, mut ruby) = (0.0f32, 0.0f32);
            for unit in &units {
                match &layout.flat.atoms[unit.atom] {
                    Atom::Char(c) => scale = scale.max(layout.size(c.pen)),
                    Atom::Ruby { base, text, rt_pen, .. } => {
                        for c in base {
                            scale = scale.max(layout.size(c.pen));
                        }
                        ruby = ruby.max(layout.ruby_metrics(text, *rt_pen).0);
                    }
                    Atom::Break => {}
                }
            }
            let scale = if scale > 0.0 { scale } else { layout.size(0) };
            let metrics = units.iter().flat_map(|u| match &layout.flat.atoms[u.atom] {
                Atom::Char(c) => std::slice::from_ref(c),
                Atom::Ruby { base, .. } => base.as_slice(),
                Atom::Break => &[],
            }).fold((0.0f32, 0.0f32), |(a, d), c| (a.max(c.ascender), d.max(c.descender)));
            let height = (layout.line_height * scale).max(metrics.0 + metrics.1).ceil();
            let ascender = metrics.0 + (height - metrics.0 - metrics.1) / 2.0;
            Line { units, rtl, width, height, ruby: ruby.ceil(), ascender }
        })
        .collect()
}

/// Rule L2 of the Unicode Bidirectional Algorithm: from the highest level
/// down to the lowest odd one, reverse every run at that level or above.
fn reorder(units: &mut [Unit]) {
    let Some(highest) = units.iter().map(|u| u.level).max() else { return };
    let lowest_odd = units.iter().map(|u| u.level).filter(|l| l % 2 == 1).min().unwrap_or(highest + 1);
    let mut level = highest;
    while level >= lowest_odd && level > 0 {
        let mut i = 0;
        while i < units.len() {
            if units[i].level >= level {
                let start = i;
                while i < units.len() && units[i].level >= level {
                    i += 1;
                }
                units[start..i].reverse();
            } else {
                i += 1;
            }
        }
        level -= 1;
    }
}

struct Canvas {
    rgba: Vec<u8>,
    width: usize,
    height: usize,
}

impl Canvas {
    fn blend(&mut self, x: i64, y: i64, color: Rgba, alpha: f32) {
        if x < 0 || y < 0 || x as usize >= self.width || y as usize >= self.height {
            return;
        }
        let a = (f32::from(color[3]) * alpha).round().clamp(0.0, 255.0) as u32;
        if a == 0 {
            return;
        }
        let i = (y as usize * self.width + x as usize) * 4;
        let px = &mut self.rgba[i..i + 4];
        let (da, inv) = (u32::from(px[3]), 255 - a);
        let out_a = a + da * inv / 255;
        if out_a == 0 {
            return;
        }
        for c in 0..3 {
            let s = u32::from(color[c]) * a;
            let d = u32::from(px[c]) * da * inv / 255;
            px[c] = ((s + d) / out_a).min(255) as u8;
        }
        px[3] = out_a.min(255) as u8;
    }

    fn fill(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, color: Rgba, alpha: f32) {
        for y in (y0.round() as i64).max(0)..(y1.round() as i64).min(self.height as i64) {
            for x in (x0.round() as i64).max(0)..(x1.round() as i64).min(self.width as i64) {
                self.blend(x, y, color, alpha);
            }
        }
    }

    fn glyph(&mut self, c: &Char, x: f32, baseline: f32, color: Rgba, alpha: f32) {
        for mask in &c.masks {
            let left = x.round() as i64 + i64::from(mask.left);
            let top = baseline.round() as i64 + i64::from(mask.top);
            for y in 0..mask.h {
                for (dx, &coverage) in mask.row(y as usize).iter().enumerate() {
                    if coverage != 0 { self.blend(left + dx as i64, top + i64::from(y), color, alpha * f32::from(coverage) / 255.0); }
                }
            }
        }
    }
}

fn draw_line(layout: &Layout, canvas: &mut Canvas, line: &Line, x0: f32, top: f32) {
    let flat = layout.flat;
    let base_top = top + line.ruby;
    let baseline = base_top + line.ascender;
    // Unit positions.
    let mut xs = Vec::with_capacity(line.units.len());
    let mut x = x0;
    for unit in &line.units {
        xs.push(x);
        x += unit.width;
    }
    // Background boxes, outermost first: each spans its characters on
    // the line (a box per line, as inline boxes break).
    let mut extents: Vec<Option<(f32, f32)>> = vec![None; flat.boxes.len()];
    for (unit, &ux) in line.units.iter().zip(&xs) {
        let owners: &[usize] = match &flat.atoms[unit.atom] {
            Atom::Char(c) => &c.boxes,
            Atom::Ruby { base, .. } => base.first().map_or(&[][..], |c| &c.boxes),
            Atom::Break => &[],
        };
        for &b in owners {
            let e = extents[b].get_or_insert((ux, ux + unit.width));
            e.0 = e.0.min(ux);
            e.1 = e.1.max(ux + unit.width);
        }
    }
    for (b, extent) in extents.iter().enumerate() {
        if let Some((a, z)) = extent {
            let paint = &flat.boxes[b];
            canvas.fill(*a, base_top, *z, base_top + line.height, paint.color, paint.alpha);
        }
    }
    // Text: shadows first, then glyphs and decorations.
    for pass in 0..2 {
        for (unit, &ux) in line.units.iter().zip(&xs) {
            match &flat.atoms[unit.atom] {
                Atom::Char(c) => draw_chars(layout, canvas, std::slice::from_ref(c), ux, baseline, pass),
                Atom::Ruby { base, text, rt_pen, rt_box } => {
                    let base_w: f32 = base.iter().map(|c| layout.advance(c)).sum();
                    draw_chars(layout, canvas, base, ux + (unit.width - base_w) / 2.0, baseline, pass);
                    let text_w: f32 = text.iter().map(|c| layout.advance(c)).sum();
                    let (rt_height, rt_ascender) = layout.ruby_metrics(text, *rt_pen);
                    let rt_top = base_top - rt_height;
                    if pass == 0 {
                        if let Some(b) = rt_box {
                            let paint = &flat.boxes[*b];
                            let left = ux + (unit.width - text_w) / 2.0;
                            canvas.fill(left, rt_top, left + text_w, base_top, paint.color, paint.alpha);
                        }
                    }
                    let rt_baseline = rt_top + rt_ascender;
                    draw_chars(layout, canvas, text, ux + (unit.width - text_w) / 2.0, rt_baseline, pass);
                }
                Atom::Break => {}
            }
        }
    }
}

/// Shadows precede glyphs; decorations are part of the shaped outlines.
fn draw_chars(layout: &Layout, canvas: &mut Canvas, chars: &[Char], mut x: f32, baseline: f32, pass: u8) {
    for c in chars {
        let pen = &layout.flat.pens[c.pen];
        if pass == 0 {
            if let Some(shadow) = pen.style.shadow {
                canvas.glyph(c, x + shadow.dx, baseline + shadow.dy, shadow.color.unwrap_or(pen.style.color), pen.alpha);
            }
        } else { canvas.glyph(c, x, baseline, pen.style.color, pen.alpha); }
        x += c.advance;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subs_text::webvtt_cue::parse;

    fn render(nodes: &[Node], sheet: &StyleSheet, id: &str, width: i64, align: Align, reversed: bool) -> Option<Block> {
        thread_local! { static SHAPER: std::cell::RefCell<Shaper> = std::cell::RefCell::new(Shaper::new(&subs_render::FontOptions::default())); }
        SHAPER.with(|s| super::render(nodes, sheet, id, width, align, reversed, 20.0, &mut s.borrow_mut()))
    }

    fn block(text: &str, css: &str, width: i64, align: Align) -> Block {
        render(&parse(text), &StyleSheet::parse(css), "", width, align, false).expect("something visible")
    }

    fn pixel(b: &Block, x: i64, y: i64) -> [u8; 4] {
        let (ix, iy) = (x - i64::from(b.image.x), y - i64::from(b.image.y));
        if ix < 0 || iy < 0 || ix >= i64::from(b.image.width) || iy >= i64::from(b.image.height) {
            return [0; 4];
        }
        let i = (iy as usize * b.image.width as usize + ix as usize) * 4;
        b.image.rgba[i..i + 4].try_into().unwrap()
    }


    #[test]
    fn styles_from_the_sheet_reach_the_pixels() {
        let css = "::cue { background-color: transparent; color: lime } ::cue(.big) { font-size: 200%; color: #ff0000 } ::cue(u) { text-decoration: line-through }";
        let b = block("a<c.big>b</c>", css, 100, Align::Left);
        let normal = block("ab", css, 100, Align::Left);
        assert!(b.height > normal.height && b.image.width > normal.image.width);
        let colours: std::collections::BTreeSet<[u8; 3]> = b.image.rgba.chunks_exact(4).filter(|px| px[3] > 0).map(|px| px[..3].try_into().unwrap()).collect();
        assert_eq!(colours, [[0, 255, 0], [255, 0, 0]].into_iter().collect());
        let faded = block("x", "::cue { opacity: 0.5; background: none }", 100, Align::Left);
        assert!(faded.image.rgba.chunks_exact(4).all(|px| px[3] <= 128), "opacity halves the coverage alpha");
    }

    #[test]
    fn text_shadow_translates_the_glyph_coverage() {
        let glyph = block("Shade", "::cue { background: transparent }", 100, Align::Left);
        let shadow = block("Shade", "::cue { color: transparent; background: transparent; text-shadow: 2px 2px red }", 100, Align::Left);
        assert_eq!((shadow.image.x, shadow.image.y), (glyph.image.x + 2, glyph.image.y + 2));
        assert_eq!((shadow.image.width, shadow.image.height), (glyph.image.width, glyph.image.height));
        assert!(shadow.image.rgba.chunks_exact(4).zip(glyph.image.rgba.chunks_exact(4)).all(|(s, g)| s[3] == g[3]));
    }

    /// Lines break in logical order, then each paragraph reorders by its
    /// own direction; `start` is the right edge of a right-to-left one.
    #[test]
    fn right_to_left_paragraphs_reorder_and_align_to_the_right() {
        let flat_rtl = parse("שלום abc");
        assert!(is_rtl(&flat_rtl));
        assert!(!is_rtl(&parse("abc שלום")));
        let b = render(&flat_rtl, &StyleSheet::default(), "", 200, Align::Start, false).unwrap();
        // The text is flush right, independent of the selected face's advance.
        assert_eq!(i64::from(b.image.x) + i64::from(b.image.width), 200);
    }

    #[test]
    fn ruby_text_sits_above_its_base_at_half_size() {
        let b = block("<ruby>ab<rt>xyz</rt></ruby>", "::cue { background: transparent } ::cue(rt) { background: transparent }", 100, Align::Left);
        let plain = block("ab", "::cue { background: transparent }", 100, Align::Left);
        let annotation_height = b.height - plain.height;
        assert!(annotation_height > 0);
        let rows_with_ink = |from: i64, to: i64| (from..to).any(|y| (0..100).any(|x| pixel(&b, x, y)[3] > 0));
        assert!(rows_with_ink(0, annotation_height), "annotation above");
        assert!(rows_with_ink(annotation_height, b.height), "base below");
    }

    #[test]
    fn long_text_wraps_at_spaces_and_cjk() {
        let single = block("aaaa", "", 100, Align::Left);
        let b = block("aaaa aaaa", "", i64::from(single.image.width) + 2, Align::Left);
        assert_eq!(b.height, 2 * single.height, "wrap at the space");
        let cjk = block("漢字漢字漢字", "", 32, Align::Left);
        assert!(cjk.height > cjk.first_line, "CJK breaks without spaces");
        assert!(render(&parse(""), &StyleSheet::default(), "", 100, Align::Left, false).is_none());
    }

    #[test]
    fn long_text_and_ruby_share_the_character_bound() {
        let texts = [
            "W".repeat(MAX_ATOMS + 1),
            format!("<ruby>{}<rt>{}</rt></ruby>", "b".repeat(MAX_ATOMS / 2), "r".repeat(MAX_ATOMS)),
            "<ruby>b<rt>r</rt></ruby>".repeat(MAX_ATOMS),
        ];
        for text in texts {
            let nodes = parse(&text);
            let flat = flatten(&nodes, &cascade(&StyleSheet::default(), &nodes, ""));
            let count: usize = flat.atoms.iter().map(|atom| match atom {
                Atom::Ruby { base, text, .. } => base.len() + text.len(),
                _ => 1,
            }).sum();
            assert!(count <= MAX_ATOMS, "cue lays out {count} characters");
        }
    }

    /// Hostile cue text and styles (deep nesting, 8x text, endless lines,
    /// mixed directions, ruby without a base, 2000 seeded mutations of a
    /// rich cue) draw without a panic, inside the block bounds.
    #[test]
    fn hostile_cues_draw_within_bounds() {
        let big = StyleSheet::parse("::cue { font-size: 800%; text-shadow: 999999px -999999px red } ::cue(rt) { font-size: 0.01% } ::cue(c) { opacity: 0 }");
        let seed = "<v.a Roger><c.b><i><b><u>Hi</u></b></i></c> שלום <ruby>漢<rt>kan</rt></ruby>\n<00:00:01.000>x</v>";
        let cases = [
            "<c>".repeat(5000),
            "x\n".repeat(9000),
            "ab ".repeat(4000),
            "<rt>no base</rt><ruby><rt>only text</rt></ruby>".to_string(),
            "\u{202e}abc\u{2067}שלום\u{2069}def\u{200f}".to_string(),
        ];
        let check = |text: &str, sheet: &StyleSheet, width: i64| {
            if let Some(b) = render(&parse(text), sheet, "", width, Align::Start, false) {
                assert!(b.height <= MAX_SIDE && b.height * width <= MAX_PIXELS);
                assert_eq!(b.image.rgba.len(), b.image.width as usize * b.image.height as usize * 4);
                assert!(i64::from(b.image.x) + i64::from(b.image.width) <= width && i64::from(b.image.y) + i64::from(b.image.height) <= b.height);
            }
        };
        for case in &cases {
            for sheet in [&StyleSheet::default(), &big] {
                for width in [1, 7, 320, 4096] {
                    check(case, sheet, width);
                }
            }
        }
        let mut state = 0x5eed_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..2000 {
            let mut bytes = seed.as_bytes().to_vec();
            for _ in 0..=next() % 4 {
                let at = (next() % (bytes.len() as u64 + 1)) as usize;
                match next() % 3 {
                    0 if at < bytes.len() => bytes[at] = b"<>/.&;\n v:c rubyt"[(next() % 17) as usize],
                    1 => bytes.insert(at, b"<>/.&;\n"[(next() % 7) as usize]),
                    _ if at < bytes.len() => {
                        bytes.remove(at);
                    }
                    _ => {}
                }
            }
            check(&String::from_utf8_lossy(&bytes), &big, 320);
        }
    }
}

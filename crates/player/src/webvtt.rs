//! WebVTT cue layout, as W3C WebVTT §7 lays cues out
//! (<https://www.w3.org/TR/webvtt1/>): the cue box from the cue's size,
//! position and alignment (§7.2; `start`/`end` follow the cue text's
//! direction), its line (snapped to lines, or a percentage, with the line
//! alignment), vertical cues (`vertical:rl` / `lr`), cues in regions
//! (§7.1: on the region's `rgba(0,0,0,0.8)` box, stacked from its bottom,
//! clipped to its `lines`, scrolled up over 0.433 s with CSS's `ease` when
//! the region scrolls), and the moves that keep a cue off the cues and
//! regions already shown.
//!
//! The text is the cue's own (§6.4 nodes, from the packet), drawn with
//! the track's `STYLE` sheet by [`crate::webvtt_text`]. Vertical text is
//! the horizontal rendering turned a quarter clockwise, as browsers set
//! Latin text in vertical writing modes. A region's line is the default
//! font's 20-pixel line box (§7.1's 6vh assumes the 5vh cue font this
//! renderer does not scale to).

use std::time::Duration;

use oxideav_core::{Packet, PacketMetadata, Segment, SubtitleCue};
use subs_text::webvtt_css::{StyleSheet, DEFAULT_BACKGROUND};
use subs_text::webvtt_cue::Node;
use subs_text::webvtt_settings::{header_regions, header_text, CueAlign, CueSettings, LineAlign, PositionAlign, Region, Vertical};

use crate::backend::SubtitleImage;
use crate::webvtt_text::{self, Align, LINE};

/// Regions of a track considered at most (each is an obstacle for every
/// cue placed).
const MAX_REGIONS: usize = 64;
/// How long a scrolling region takes to move its lines up (§7.1).
pub(crate) const SCROLL: Duration = Duration::from_millis(433);

/// A rectangle on the text canvas, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl Rect {
    pub(crate) fn of(image: &SubtitleImage) -> Rect {
        Rect { x: i64::from(image.x), y: i64::from(image.y), w: i64::from(image.width), h: i64::from(image.height) }
    }

    fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.w && other.x < self.x + self.w && self.y < other.y + other.h && other.y < self.y + self.h
    }

    fn within(&self, outer: &Rect) -> bool {
        self.x >= outer.x && self.y >= outer.y && self.x + self.w <= outer.x + outer.w && self.y + self.h <= outer.y + outer.h
    }
}

/// How a cue finds its place on screen when it comes up.
#[derive(Clone, Debug)]
pub(crate) enum Layout {
    /// Snapped to lines: from where it was rendered, moves by `step`
    /// pixels (down for positive) along the line axis until it overlaps
    /// nothing up and stays on the canvas, back in the other direction
    /// past an edge, and is not shown when neither finds a place.
    /// `vertical` is the writing direction.
    Snap { step: i64, vertical: Vertical },
    /// Positioned by percentage: moves to the closest place where it
    /// overlaps nothing and stays on the canvas, if there is one.
    Closest,
    /// Shown in a region, stacked under its earlier cues.
    Region(RegionSlot),
}

/// A cue in a region: its rendered lines, and where the region is.
#[derive(Clone, Debug)]
pub(crate) struct RegionSlot {
    /// The region's index in the track.
    pub region: usize,
    /// The region box on the canvas at its full `lines`; cues outside it
    /// are clipped.
    pub rect: Rect,
    /// The cue's lines on the region's background, the region's width:
    /// `x` on the canvas, `y` from the top of the block.
    pub block: SubtitleImage,
    /// The block's height: its line boxes'.
    pub block_height: i64,
    /// The region scrolls (`scroll:up`): its lines move up as cues join.
    pub scroll_up: bool,
}

/// A decoded text cue, rendered, and how it is placed.
pub(crate) struct TextCue {
    pub image: SubtitleImage,
    pub layout: Layout,
}

/// What the player knows of a WebVTT cue when it decodes it: its
/// settings, its identifier and its text.
#[derive(Clone, Debug)]
pub(crate) struct CueInfo {
    pub settings: CueSettings,
    pub id: String,
    pub nodes: Vec<Node>,
}

impl CueInfo {
    /// A cue the packet did not describe: the decoder's text, no settings.
    fn from_decoded(cue: &SubtitleCue) -> CueInfo {
        fn walk(segments: &[Segment], out: &mut String) {
            for s in segments {
                match s {
                    Segment::Text(t) | Segment::Raw(t) => out.push_str(t),
                    Segment::LineBreak => out.push('\n'),
                    Segment::Bold(c) | Segment::Italic(c) | Segment::Underline(c) | Segment::Strike(c) => walk(c, out),
                    Segment::Color { children, .. } | Segment::Font { children, .. } | Segment::Voice { children, .. }
                    | Segment::Class { children, .. } | Segment::Karaoke { children, .. } => walk(children, out),
                    Segment::Timestamp { .. } => {}
                }
            }
        }
        let mut text = String::new();
        walk(&cue.segments, &mut text);
        CueInfo { settings: CueSettings::default(), id: String::new(), nodes: vec![Node::Text(text)] }
    }
}

/// What a WebVTT track places and styles its cues with.
pub(crate) struct WebVttTrack {
    regions: Vec<Region>,
    sheet: StyleSheet,
    shaper: std::cell::RefCell<subs_render::shaper::Shaper>,
}

impl WebVttTrack {
    /// From the stream's extradata: the file header, WebM's
    /// `CodecPrivate` or MP4's sample entry, whose `REGION` blocks define
    /// its regions and `STYLE` blocks its style sheet.
    pub fn new(extradata: &[u8]) -> WebVttTrack {
        Self::with_fonts(extradata, &subs_render::FontOptions::default())
    }

    pub fn with_fonts(extradata: &[u8], fonts: &subs_render::FontOptions) -> WebVttTrack {
        let mut regions = header_regions(extradata);
        regions.truncate(MAX_REGIONS);
        WebVttTrack { regions, sheet: StyleSheet::from_header(header_text(extradata)),
            shaper: std::cell::RefCell::new(subs_render::shaper::Shaper::new(fonts)) }
    }

    pub fn add_font(&self, data: std::sync::Arc<[u8]>) {
        self.shaper.borrow_mut().add_font(data);
    }

    /// Each cue `packet` decodes to, in order: an MP4 sample's cue boxes
    /// (each with text, as the decoder decodes them), or the packet's own
    /// text with its metadata's settings (Matroska, WebM, `.vtt`).
    pub fn packet_cues(&self, packet: &Packet, metadata: &PacketMetadata) -> Vec<CueInfo> {
        let parse = |settings: &[u8]| CueSettings::parse(settings, &self.regions);
        // The text up to its first NUL, its character set decided for the
        // cue alone, as the decoder reads it.
        let text = |bytes: &[u8]| {
            let until_nul = bytes.split(|&b| b == 0).next().unwrap_or_default();
            subs_text::webvtt_cue::parse(&subs_text::text_common::decode_subtitle_text(until_nul))
        };
        match subs_text::webvtt::mp4_sample_cues(&packet.data) {
            Some(cues) => cues
                .iter()
                .filter(|(payload, _)| !payload.is_empty())
                .map(|(payload, m)| CueInfo { settings: parse(&m.settings), id: String::from_utf8_lossy(&m.identifier).into_owned(), nodes: text(payload) })
                .collect(),
            None => {
                let (settings, id) = metadata.webvtt.as_ref().map_or_else(Default::default, |m| (parse(&m.settings), String::from_utf8_lossy(&m.identifier).into_owned()));
                vec![CueInfo { settings, id, nodes: text(&packet.data) }]
            }
        }
    }

    /// The region boxes on a `canvas`-sized text canvas that cues keep
    /// off: a region is on screen with or without cues (§7.1).
    pub fn region_rects(&self, canvas: (u32, u32)) -> Vec<Rect> {
        self.regions.iter().map(|r| region_rect(r, canvas)).filter(|r| r.h > 0).collect()
    }

    fn region_index(&self, region: &Region) -> Option<usize> {
        self.regions.iter().rposition(|r| r.id == region.id)
    }
}

/// A line box of the default font.
fn line_height(height: u32) -> i64 {
    (height as f32 * 0.05 * LINE).ceil() as i64
}

fn region_rect(region: &Region, (cw, ch): (u32, u32)) -> Rect {
    let w = (region.width / 100.0 * f64::from(cw)).round() as i64;
    let h = i64::from(region.lines).saturating_mul(line_height(ch)).min(i64::from(ch));
    let x = (region.viewport_anchor.0 / 100.0 * f64::from(cw) - region.region_anchor.0 * w as f64 / 100.0).round() as i64;
    let y = (region.viewport_anchor.1 / 100.0 * f64::from(ch) - region.region_anchor.1 * h as f64 / 100.0).round() as i64;
    Rect { x, y, w, h }
}

fn text_align(align: CueAlign) -> Align {
    match align {
        CueAlign::Start => Align::Start,
        CueAlign::Center => Align::Center,
        CueAlign::End => Align::End,
        CueAlign::Left => Align::Left,
        CueAlign::Right => Align::Right,
    }
}

const NOTHING: SubtitleImage = SubtitleImage { x: 0, y: 0, width: 0, height: 0, rgba: Vec::new() };

/// A decoded WebVTT `cue` laid out on a `canvas`-sized text canvas, per
/// `info` (`None`: the packet did not describe it).
pub(crate) fn layout_cue(cue: &SubtitleCue, info: Option<&CueInfo>, track: &WebVttTrack, canvas: (u32, u32)) -> TextCue {
    let fallback;
    let info = match info {
        Some(info) => info,
        None => {
            fallback = CueInfo::from_decoded(cue);
            &fallback
        }
    };
    let (cw, ch) = canvas;
    let settings = &info.settings;
    let rtl = webvtt_text::is_rtl(&info.nodes);
    let align = text_align(settings.align);
    let render = |width: i64, reversed: bool| webvtt_text::render(&info.nodes, &track.sheet, &info.id, width, align, reversed, ch as f32 * 0.05, &mut track.shaper.borrow_mut());
    // No line boxes: the cue is not shown (an empty image never is).
    let none = || TextCue { image: NOTHING, layout: Layout::Closest };

    if let Some(index) = settings.region.as_ref().and_then(|r| track.region_index(r)) {
        let region = &track.regions[index];
        let rect = region_rect(region, canvas);
        let Some(block) = render(rect.w, false) else { return none() };
        // §7.1: the offset is the computed position of the region width,
        // less the region width as the position alignment says, as a
        // percentage of the region width.
        let mut offset = settings.computed_position() * region.width / 100.0;
        match settings.computed_position_align(rtl) {
            PositionAlign::Center => offset -= region.width / 2.0,
            PositionAlign::LineRight => offset -= region.width,
            _ => {}
        }
        let left = (offset / 100.0 * rect.w as f64).round() as i64;
        let Some(image) = on_region_box(&block.image, rect.w, block.height, left) else { return none() };
        let image = SubtitleImage { x: clamp_i32(rect.x), ..image };
        let slot = RegionSlot { region: index, rect, block: image, block_height: block.height, scroll_up: region.scroll_up };
        return TextCue { image: NOTHING, layout: Layout::Region(slot) };
    }

    let size = settings.computed_size(rtl);
    let start = settings.box_start(rtl);
    let horizontal = settings.vertical == Vertical::Horizontal;
    // The cue box: `size` of the width (horizontal) or height (vertical).
    let extent = if horizontal { cw } else { ch };
    let box_len = (size / 100.0 * f64::from(extent)).round() as i64;
    let box_start = (start / 100.0 * f64::from(extent)).round() as i64;
    let Some(block) = render(box_len, settings.vertical == Vertical::GrowingRight) else { return none() };
    let step = block.first_line.max(1);
    // The image, where it sits along the line axis within the cue box,
    // and across it within the block (`across` long).
    let across = block.height;
    let (mut image, offset_along, offset_across) = if horizontal {
        let (x, y) = (i64::from(block.image.x), i64::from(block.image.y));
        (block.image, x, y)
    } else {
        // A quarter turn clockwise: a point (x, y) of the lines lands at
        // (across - 1 - y, x).
        let x = across - i64::from(block.image.y) - i64::from(block.image.height);
        let y = i64::from(block.image.x);
        (turn_cw(&block.image), y, x)
    };
    let line = settings.computed_line();
    let (layout, block_pos) = if settings.snap_to_lines {
        let mut line = (line + 0.5).floor().clamp(-1e9, 1e9) as i64;
        let full = if horizontal { i64::from(ch) } else { i64::from(cw) };
        if settings.vertical == Vertical::GrowingLeft {
            line = -(line + 1);
        }
        let mut position = step.saturating_mul(line);
        let mut step = step;
        if settings.vertical == Vertical::GrowingLeft {
            position = position - across + step;
        }
        if line < 0 {
            position = position.saturating_add(full);
            step = -step;
        }
        (Layout::Snap { step, vertical: settings.vertical }, position)
    } else {
        let dimension = if horizontal { i64::from(ch) } else { i64::from(cw) };
        let mut at = (line / 100.0 * dimension as f64).round() as i64;
        match settings.line_align {
            LineAlign::Center => at -= across / 2,
            LineAlign::End => at -= across,
            LineAlign::Start => {}
        }
        (Layout::Closest, at)
    };
    let (along, across_at) = (box_start.saturating_add(offset_along), block_pos.saturating_add(offset_across));
    if horizontal {
        (image.x, image.y) = (clamp_i32(along), clamp_i32(across_at));
    } else {
        (image.x, image.y) = (clamp_i32(across_at), clamp_i32(along));
    }
    TextCue { image, layout }
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// `lines` (`x`/`y` within its block, shifted `left` pixels) over the
/// region's background, `width` wide and `height` tall (§7.4: a region
/// box is `rgba(0,0,0,0.8)`); `None` when the region has no width.
fn on_region_box(lines: &SubtitleImage, width: i64, height: i64, left: i64) -> Option<SubtitleImage> {
    if width <= 0 || height <= 0 || width > 8192 || height > 8192 {
        return None;
    }
    let (w, h) = (width as usize, height as usize);
    let mut rgba: Vec<u8> = DEFAULT_BACKGROUND.iter().copied().cycle().take(w * h * 4).collect();
    for row in 0..lines.height as usize {
        for col in 0..lines.width as usize {
            let (x, y) = (i64::from(lines.x) + left + col as i64, i64::from(lines.y) + row as i64);
            if x < 0 || y < 0 || x >= width || y >= height {
                continue;
            }
            let s = (row * lines.width as usize + col) * 4;
            let d = (y as usize * w + x as usize) * 4;
            over(&mut rgba[d..d + 4], &lines.rgba[s..s + 4]);
        }
    }
    Some(SubtitleImage { x: 0, y: 0, width: w as u32, height: h as u32, rgba })
}

/// Straight-alpha `src` over `dst`.
fn over(dst: &mut [u8], src: &[u8]) {
    let (sa, da) = (u32::from(src[3]), u32::from(dst[3]));
    if sa == 0 {
        return;
    }
    let out = sa + da * (255 - sa) / 255;
    for c in 0..3 {
        dst[c] = ((u32::from(src[c]) * sa + u32::from(dst[c]) * da * (255 - sa) / 255) / out).min(255) as u8;
    }
    dst[3] = out.min(255) as u8;
}

/// `image` turned a quarter clockwise: its rows become columns, the first
/// row the rightmost.
fn turn_cw(image: &SubtitleImage) -> SubtitleImage {
    let (w, h) = (image.width as usize, image.height as usize);
    let mut rgba = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let src = (y * w + x) * 4;
            let (nx, ny) = (h - 1 - y, x);
            let dst = (ny * h + nx) * 4;
            rgba[dst..dst + 4].copy_from_slice(&image.rgba[src..src + 4]);
        }
    }
    SubtitleImage { x: 0, y: 0, width: image.height, height: image.width, rgba }
}

/// Moves `image` per `layout` off `obstacles` within `canvas` (§7.2
/// "adjust the positions of boxes"). False when it is not to be shown.
pub(crate) fn place(image: &mut SubtitleImage, layout: &Layout, obstacles: &[Rect], canvas: Rect) -> bool {
    if image.width == 0 {
        return false;
    }
    let free = |r: &Rect| r.within(&canvas) && !obstacles.iter().any(|o| o.overlaps(r));
    match layout {
        Layout::Region(_) => true,
        Layout::Snap { step, vertical } => {
            let specified = (image.x, image.y);
            let mut step = *step;
            if step == 0 {
                return true;
            }
            let mut switched = false;
            let mut moved = Rect::of(image);
            loop {
                // A cue far off the canvas jumps to the first step that
                // brings it in, past places that cannot be free (a hostile
                // line number would take billions of steps).
                let s = step.abs();
                let (pos, len, lo, hi) = match vertical {
                    Vertical::Horizontal => (moved.y, moved.h, canvas.y, canvas.y + canvas.h),
                    _ => (moved.x, moved.w, canvas.x, canvas.x + canvas.w),
                };
                let jump = if step < 0 && pos + len > hi + s {
                    -((pos + len - hi) / s) * s
                } else if step > 0 && pos + s < lo {
                    ((lo - pos) / s) * s
                } else {
                    0
                };
                match vertical {
                    Vertical::Horizontal => moved.y += jump,
                    _ => moved.x += jump,
                }
                if free(&moved) {
                    image.x = clamp_i32(moved.x);
                    image.y = clamp_i32(moved.y);
                    return true;
                }
                // The first line box: the top line, or the rightmost
                // (`rl`) or leftmost (`lr`) column.
                let past_edge = match vertical {
                    Vertical::Horizontal => (step < 0 && moved.y < canvas.y) || (step > 0 && moved.y + s > canvas.y + canvas.h),
                    Vertical::GrowingLeft => {
                        let left = moved.x + moved.w - s;
                        (step < 0 && left < canvas.x) || (step > 0 && left + s > canvas.x + canvas.w)
                    }
                    Vertical::GrowingRight => (step < 0 && moved.x < canvas.x) || (step > 0 && moved.x + s > canvas.x + canvas.w),
                };
                if past_edge {
                    if switched {
                        return false;
                    }
                    moved.x = i64::from(specified.0);
                    moved.y = i64::from(specified.1);
                    step = -step;
                    switched = true;
                    continue;
                }
                match vertical {
                    Vertical::Horizontal => moved.y += step,
                    _ => moved.x += step,
                }
            }
        }
        Layout::Closest => {
            let at = Rect::of(image);
            if free(&at) {
                return true;
            }
            // The closest free place touches an obstacle or the canvas in
            // each coordinate, or keeps the cue's own.
            let mut xs = vec![at.x, canvas.x, canvas.x + canvas.w - at.w];
            let mut ys = vec![at.y, canvas.y, canvas.y + canvas.h - at.h];
            for o in obstacles {
                xs.extend([o.x - at.w, o.x + o.w]);
                ys.extend([o.y - at.h, o.y + o.h]);
            }
            let mut best: Option<(i128, i64, i64)> = None;
            for &y in &ys {
                for &x in &xs {
                    let candidate = Rect { x, y, ..at };
                    if !free(&candidate) {
                        continue;
                    }
                    let (dx, dy) = (i128::from(x - at.x), i128::from(y - at.y));
                    let key = (dx * dx + dy * dy, y, x);
                    if best.is_none_or(|b| key < b) {
                        best = Some(key);
                    }
                }
            }
            if let Some((_, y, x)) = best {
                image.x = clamp_i32(x);
                image.y = clamp_i32(y);
            }
            true
        }
    }
}

/// CSS's `ease` timing function, `cubic-bezier(0.25, 0.1, 0.25, 1)`: the
/// progress of a transition `t` (0..=1) of the way through its time.
pub(crate) fn ease(t: f64) -> f64 {
    if t <= 0.0 {
        return 0.0;
    }
    if t >= 1.0 {
        return 1.0;
    }
    let (x1, y1, x2, y2) = (0.25, 0.1, 0.25, 1.0);
    let bezier = |p1: f64, p2: f64, s: f64| 3.0 * p1 * s * (1.0 - s).powi(2) + 3.0 * p2 * s * s * (1.0 - s) + s.powi(3);
    // x(s) is increasing: bisect for the s whose x is t.
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..40 {
        let mid = (lo + hi) / 2.0;
        if bezier(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bezier(y1, y2, (lo + hi) / 2.0)
}

/// The cues of one region, in the order they came up, stacked from the
/// region's bottom (the latest lowest), `lift` pixels below their resting
/// place (a scroll under way), and clipped to the region box. The region
/// box is as tall as its lines; `lines:0` shows nothing, as browsers clip
/// the region's content to its height.
pub(crate) fn stack_region(slots: &[&RegionSlot], lift: i64) -> Vec<SubtitleImage> {
    let Some(first) = slots.first() else { return Vec::new() };
    let rect = first.rect;
    let total: i64 = slots.iter().map(|s| s.block_height).sum();
    let mut top = rect.y + rect.h - total + lift;
    slots
        .iter()
        .map(|slot| {
            let mut placed = SubtitleImage { y: clamp_i32(top + i64::from(slot.block.y)), ..slot.block.clone() };
            top += slot.block_height;
            clip(&mut placed, rect);
            placed
        })
        .collect()
}

/// Cuts `image` down to its part within `rect` (empty when none is).
fn clip(image: &mut SubtitleImage, rect: Rect) {
    let r = Rect::of(image);
    let (x0, y0) = (r.x.max(rect.x), r.y.max(rect.y));
    let (x1, y1) = ((r.x + r.w).min(rect.x + rect.w), (r.y + r.h).min(rect.y + rect.h));
    if x0 >= x1 || y0 >= y1 {
        *image = NOTHING;
        return;
    }
    if (x0, y0, x1, y1) == (r.x, r.y, r.x + r.w, r.y + r.h) {
        return;
    }
    let (w, row) = ((x1 - x0) as usize, image.width as usize * 4);
    let mut rgba = Vec::with_capacity(w * (y1 - y0) as usize * 4);
    for y in y0..y1 {
        let start = (y - r.y) as usize * row + (x0 - r.x) as usize * 4;
        rgba.extend_from_slice(&image.rgba[start..start + w * 4]);
    }
    *image = SubtitleImage { x: clamp_i32(x0), y: clamp_i32(y0), width: w as u32, height: (y1 - y0) as u32, rgba };
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: i64 = 640;
    const H: i64 = 360;
    const CANVAS: Rect = Rect { x: 0, y: 0, w: W, h: H };

    fn line_height() -> i64 { super::line_height(H as u32) }

    fn cue() -> SubtitleCue {
        SubtitleCue { start_us: 0, end_us: 1_000_000, style_ref: None, positioning: None, segments: Vec::new() }
    }

    fn laid(text: &str, settings: &str, header: &[u8]) -> TextCue {
        let track = WebVttTrack::new(header);
        let info = CueInfo { settings: CueSettings::parse(settings.as_bytes(), &track.regions), id: String::new(), nodes: subs_text::webvtt_cue::parse(text) };
        layout_cue(&cue(), Some(&info), &track, (W as u32, H as u32))
    }

    /// `text` with `settings`, placed alone on the canvas.
    fn placed(text: &str, settings: &str) -> Rect {
        let TextCue { mut image, layout } = laid(text, settings, b"");
        assert!(place(&mut image, &layout, &[], CANVAS), "{settings}: not shown");
        Rect::of(&image)
    }

    #[test]
    fn snapped_lines_count_from_the_top_or_the_bottom() {
        let step = placed("top", "line:0").h;
        assert_eq!(placed("top", "line:0").y, 0);
        assert_eq!(placed("third", "line:2").y, 2 * step);
        // Line -1 starts a line height above the bottom.
        let last = placed("last", "line:-1");
        assert_eq!(last.y, H - step);
        // No settings: the last line too (the computed line is -1).
        assert_eq!(placed("plain", "").y, H - step);
        // A cue past the bottom moves up until it fits.
        assert!(placed("long", "line:1000").within(&CANVAS));
    }

    #[test]
    fn percentage_lines_align_the_cue_box_on_the_line() {
        let middle = placed("middle", "line:50%,center");
        assert!((middle.y + middle.h / 2 - H / 2).abs() <= 1, "{middle:?}");
        let bottom = placed("bottom", "line:100%,end");
        assert_eq!(bottom.y + bottom.h, H);
        assert_eq!(placed("top", "line:0%").y, 0);
    }

    #[test]
    fn size_position_and_alignment_set_the_cue_box() {
        // align:start, size:50%: a box over the right half, the text (on
        // its background box) at the box's start.
        let right = placed("right half", "align:start size:50%");
        assert_eq!(right.x, W / 2);
        // align:end: the left half, text at its end.
        let left = placed("left half", "align:end size:50%");
        assert_eq!(left.x + left.w, W / 2);
        // position:10% line-left: the box starts at 10%.
        assert_eq!(placed("at ten", "position:10%,line-left align:left").x, W / 10);
    }

    /// `start` and `end` anchor by the text's direction (§3: the computed
    /// position alignment), and the line follows it inside the box.
    #[test]
    fn right_to_left_text_turns_start_and_end_around() {
        // RTL `start`, size 50%: auto position 50%, line-right: the box is
        // the left half, the text flush with its right edge.
        let rtl_start = placed("שלום", "align:start size:50%");
        assert_eq!(rtl_start.x + rtl_start.w, W / 2);
        let rtl_end = placed("שלום", "align:end size:50%");
        assert_eq!(rtl_end.x, W / 2);
        // `left` does not depend on direction.
        assert_eq!(placed("שלום", "align:left size:50% position:0%").x, 0);
    }

    #[test]
    fn vertical_cues_turn_and_take_their_line_across() {
        // vertical:rl, line auto: the last line of a right-to-left stack,
        // the leftmost line box (§7.2: line -1, growing left).
        let rl = placed("vertical text", "vertical:rl");
        assert!(rl.h > rl.w, "{rl:?}");
        assert_eq!(rl.x, 0);
        assert_eq!(rl.w, placed("vertical text", "line:0").h);
        // FATE's "Title Wrap": lr, line 0 (left edge), 20% down, 60% tall.
        let lr = placed("Some time ago in a rather distant place....", "vertical:lr line:0 position:20% size:60% align:start");
        assert_eq!(lr.x, 0);
        assert!(lr.y == H / 5 && lr.y + lr.h <= H / 5 + H * 3 / 5, "{lr:?}");
        // Wrapped columns grow right from the left edge.
        assert!(lr.w > rl.w && lr.w % rl.w == 0);
        // rl, line 0: the right edge.
        let right = placed("x", "vertical:rl line:0");
        assert_eq!(right.x + right.w, W);
    }

    #[test]
    fn snapped_cues_stack_off_the_cues_up() {
        let TextCue { image: mut first, layout } = laid("first", "line:-1", b"");
        let TextCue { image: mut second, .. } = laid("second", "line:-1", b"");
        assert!(place(&mut first, &layout, &[], CANVAS));
        assert!(place(&mut second, &layout, &[Rect::of(&first)], CANVAS));
        assert_eq!(second.y + second.height as i32, first.y);
        // With no free line left, the cue is not shown.
        let TextCue { mut image, layout } = laid("third", "line:-1", b"");
        assert!(!place(&mut image, &layout, &[CANVAS], CANVAS));
    }

    #[test]
    fn positioned_cues_move_to_the_closest_free_place() {
        let TextCue { image: mut first, layout } = laid("same place", "line:50%", b"");
        assert!(place(&mut first, &layout, &[], CANVAS));
        let TextCue { image: mut second, .. } = laid("same place", "line:50%", b"");
        assert!(place(&mut second, &layout, &[Rect::of(&first)], CANVAS));
        // Equally close above and below: the higher place.
        assert_eq!(second.y + i32::try_from(second.height).unwrap(), first.y);
        assert_eq!(second.x, first.x);
    }

    #[test]
    fn region_cues_stack_on_the_region_box() {
        let header = b"WEBVTT\n\nREGION\nid:fred width:40% lines:2 regionanchor:0%,100% viewportanchor:10%,90%\n";
        let slot = |text: &str| match laid(text, "region:fred align:left", header).layout {
            Layout::Region(slot) => slot,
            other => panic!("not in the region: {other:?}"),
        };
        let (a, b, c) = (slot("one"), slot("two"), slot("three"));
        let rect = a.rect;
        assert_eq!(rect, Rect { x: 64, y: 324 - 2 * line_height(), w: 256, h: 2 * line_height() });
        let images = stack_region(&[&a, &b], 0);
        // Each line fills the region's width on its background.
        assert!(images.iter().all(|i| Rect::of(i).within(&rect) && i64::from(i.width) == rect.w));
        // The region's box where no cue box is (the line's right end); the
        // cue's own box darkens it further where its text is (0.8 over 0.8).
        let right_end = (images[0].width as usize - 1) * 4;
        assert_eq!(images[0].rgba[right_end..right_end + 4], DEFAULT_BACKGROUND);
        assert_eq!(images[0].rgba[..4], [0, 0, 0, 244]);
        // The latest is lowest, at the region's bottom.
        assert_eq!(i64::from(images[1].y) + i64::from(images[1].height), rect.y + rect.h);
        assert_eq!(images[0].y + images[0].height as i32, images[1].y);
        // A third line pushes the first out of the two-line region.
        let images = stack_region(&[&a, &b, &c], 0);
        assert_eq!(images[0].width, 0);
        assert!(images[1].width > 0 && images[2].width > 0);
        // Half a cue into a scroll: the newest is half below the box.
        let lift = c.block_height / 2;
        let images = stack_region(&[&a, &b, &c], lift);
        assert_eq!(i64::from(images[2].height), c.block_height - lift);
        assert_eq!(images[1].y + images[1].height as i32, images[2].y);
        assert!(images.iter().filter(|i| i.width > 0).all(|i| Rect::of(i).within(&rect)));
    }

    /// `lines:0`: the region box has no height, so its cues show nothing
    /// and it keeps no other cue away (browsers clip region content to the
    /// region's height).
    #[test]
    fn a_region_without_lines_shows_nothing() {
        let header = b"WEBVTT\n\nREGION\nid:none lines:0 width:50%\n";
        let track = WebVttTrack::new(header);
        assert!(track.region_rects((W as u32, H as u32)).is_empty());
        let Layout::Region(slot) = laid("hidden", "region:none", header).layout else { panic!("in the region") };
        assert_eq!(slot.rect.h, 0);
        assert!(stack_region(&[&slot], 0).iter().all(|i| i.width == 0));
    }

    #[test]
    fn the_scroll_eases_as_css_ease_does() {
        assert_eq!(ease(0.0), 0.0);
        assert!((ease(1.0) - 1.0).abs() < 1e-9);
        // cubic-bezier(0.25, 0.1, 0.25, 1) at half time: about 0.8024.
        assert!((ease(0.5) - 0.8024).abs() < 1e-3, "{}", ease(0.5));
        assert!((1..100).all(|i| ease(f64::from(i) / 100.0) >= ease(f64::from(i - 1) / 100.0)));
    }

    #[test]
    fn hostile_lines_place_quickly_or_not_at_all() {
        for settings in ["line:999999999999", "line:-999999999999", "vertical:rl line:999999999999", "vertical:lr line:-999999999999"] {
            let TextCue { mut image, layout } = laid("x", settings, b"");
            if place(&mut image, &layout, &[], CANVAS) {
                assert!(Rect::of(&image).within(&CANVAS), "{settings}");
            }
        }
        // No line boxes: never shown.
        let TextCue { mut image, layout } = laid("x", "size:0%", b"");
        assert!(!place(&mut image, &layout, &[], CANVAS));
    }
}

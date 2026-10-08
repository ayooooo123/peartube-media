//! WebVTT cue placement, as W3C WebVTT §7 lays cues out
//! (<https://www.w3.org/TR/webvtt1/>): the cue box from the cue's size,
//! position and alignment (§7.2), its line (snapped to lines, or a
//! percentage, with the line alignment), vertical cues (`vertical:rl` /
//! `lr`), cues in regions stacked from the region's bottom (§7.1), and the
//! moves that keep a cue off the cues and regions already shown.
//!
//! The text itself is the compositor's (bitmap font, outlined, no
//! background box), as for every text subtitle: a cue box here is the
//! cue's visible pixels, and a line is the compositor's line height.
//! Vertical text is the horizontal rendering turned a quarter clockwise,
//! as browsers set Latin text in vertical writing modes. A cue without
//! settings keeps the compositor's bottom placement, which is where the
//! specification's last line puts it.

use oxideav_core::{CuePosition, Packet, PacketMetadata, SubtitleCue, TextAlign};
use oxideav_subtitle::compositor::Compositor;
use oxideav_subtitle::font::BitmapFont;
use subs_text::webvtt_settings::{header_regions, CueAlign, CueSettings, LineAlign, PositionAlign, Region, Vertical};

use crate::backend::SubtitleImage;

/// Regions of a track considered at most (each is an obstacle for every
/// cue placed).
const MAX_REGIONS: usize = 64;

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
    /// Where it was rendered (text that is not WebVTT).
    Fixed,
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
    /// The region box on the canvas; cues outside it are clipped.
    pub rect: Rect,
    /// The cue's lines: `x` on the canvas, `y` from the top of its block.
    pub block: SubtitleImage,
    /// The block's height: its lines times the line height.
    pub block_height: i64,
}

/// A decoded text cue, rendered, and how it is placed.
pub(crate) struct TextCue {
    pub image: SubtitleImage,
    pub layout: Layout,
}

/// What a WebVTT track places its cues with.
pub(crate) struct WebVttTrack {
    regions: Vec<Region>,
}

impl WebVttTrack {
    /// From the stream's extradata: the file header, WebM's
    /// `CodecPrivate` or MP4's sample entry, whose `REGION` blocks define
    /// its regions.
    pub fn new(extradata: &[u8]) -> WebVttTrack {
        let mut regions = header_regions(extradata);
        regions.truncate(MAX_REGIONS);
        WebVttTrack { regions }
    }

    /// The settings of each cue `packet` decodes to, in order: an MP4
    /// sample's cue boxes (each with text, as the decoder decodes them),
    /// or the packet's own (Matroska, WebM, `.vtt`).
    pub fn packet_settings(&self, packet: &Packet, metadata: &PacketMetadata) -> Vec<Option<CueSettings>> {
        let parse = |settings: &[u8]| CueSettings::parse(settings, &self.regions);
        match subs_text::webvtt::mp4_sample_cues(&packet.data) {
            Some(cues) => cues.iter().filter(|(text, _)| !text.is_empty()).map(|(_, m)| Some(parse(&m.settings))).collect(),
            None => vec![metadata.webvtt.as_ref().map(|m| parse(&m.settings))],
        }
    }

    /// The region boxes on a `canvas`-sized text canvas: every region is
    /// on screen, with or without cues (§7.1).
    pub fn region_rects(&self, canvas: (u32, u32)) -> Vec<Rect> {
        self.regions.iter().map(|r| region_rect(r, canvas)).collect()
    }

    fn region_index(&self, region: &Region) -> Option<usize> {
        self.regions.iter().rposition(|r| r.id == region.id)
    }
}

/// The compositor's line height: a line box.
fn line_height() -> i64 {
    let comp = Compositor::new(1, 1);
    i64::from(comp.line_height_px.max(BitmapFont::default_regular().cell_h))
}

fn region_rect(region: &Region, (cw, ch): (u32, u32)) -> Rect {
    let w = (region.width / 100.0 * f64::from(cw)).round() as i64;
    let h = i64::from(region.lines).saturating_mul(line_height());
    let x = (region.viewport_anchor.0 / 100.0 * f64::from(cw) - region.region_anchor.0 * w as f64 / 100.0).round() as i64;
    let y = (region.viewport_anchor.1 / 100.0 * f64::from(ch) - region.region_anchor.1 * h as f64 / 100.0).round() as i64;
    Rect { x, y, w, h }
}

fn text_align(align: CueAlign) -> TextAlign {
    match align {
        CueAlign::Start => TextAlign::Start,
        CueAlign::Center => TextAlign::Center,
        CueAlign::End => TextAlign::End,
        CueAlign::Left => TextAlign::Left,
        CueAlign::Right => TextAlign::Right,
    }
}

const NOTHING: SubtitleImage = SubtitleImage { x: 0, y: 0, width: 0, height: 0, rgba: Vec::new() };

/// `cue` laid out on a `canvas`-sized text canvas per `settings` (`None`:
/// the cue has none).
pub(crate) fn layout_cue(cue: &SubtitleCue, settings: Option<&CueSettings>, track: &WebVttTrack, canvas: (u32, u32)) -> TextCue {
    let (cw, ch) = canvas;
    let Some(settings) = settings.filter(|s| **s != CueSettings::default()) else {
        // The compositor's bottom line: snapped like the specification's
        // last line, moving up off cues already shown.
        let image = render(cue, i64::from(cw), i64::from(ch)).unwrap_or(NOTHING);
        return TextCue { image, layout: Layout::Snap { step: -line_height(), vertical: Vertical::Horizontal } };
    };
    let step = line_height();
    let mut cue = cue.clone();
    cue.positioning = Some(CuePosition { align: text_align(settings.align), ..CuePosition::default() });

    if let Some(index) = settings.region.as_ref().and_then(|r| track.region_index(r)) {
        let region = &track.regions[index];
        let rect = region_rect(region, canvas);
        // No line boxes: the cue is not shown (an empty image never is).
        let Some(mut block) = render(&cue, rect.w, i64::from(ch)) else { return TextCue { image: NOTHING, layout: Layout::Closest } };
        // §7.1: the offset is the computed position of the region width,
        // less the region width as the position alignment says, as a
        // percentage of the region width.
        let mut offset = settings.computed_position() * region.width / 100.0;
        match settings.computed_position_align() {
            PositionAlign::Center => offset -= region.width / 2.0,
            PositionAlign::LineRight => offset -= region.width,
            _ => {}
        }
        let lines = (i64::from(block.height) + step - 1) / step;
        let block_height = lines * step;
        block.x = clamp_i32(rect.x + i64::from(block.x) + (offset / 100.0 * rect.w as f64).round() as i64);
        block.y = clamp_i32(block_height - i64::from(block.height));
        let slot = RegionSlot { region: index, rect, block, block_height };
        return TextCue { image: NOTHING, layout: Layout::Region(slot) };
    }

    let size = settings.computed_size();
    let start = settings.box_start();
    let horizontal = settings.vertical == Vertical::Horizontal;
    // The cue box: `size` of the width (horizontal) or height (vertical).
    let (extent, along) = if horizontal { (cw, ch) } else { (ch, ch) };
    let box_len = (size / 100.0 * f64::from(extent)).round() as i64;
    let box_start = (start / 100.0 * f64::from(extent)).round() as i64;
    let Some(rendered) = render(&cue, box_len, i64::from(along)) else { return TextCue { image: NOTHING, layout: Layout::Closest } };
    let mut image = match settings.vertical {
        Vertical::Horizontal => rendered,
        Vertical::GrowingLeft => rotate_cw(&rendered),
        Vertical::GrowingRight => rotate_cw(&reverse_lines(&cue, box_len, i64::from(along))),
    };
    let (w, h) = (i64::from(image.width), i64::from(image.height));
    // Along the line: the box start, plus where the text sits in the box
    // (horizontal x becomes vertical y).
    if horizontal {
        image.x = clamp_i32(box_start + i64::from(image.x));
    } else {
        image.y = clamp_i32(box_start + i64::from(image.x));
    }
    let line = settings.computed_line();
    let layout = if settings.snap_to_lines {
        let mut line = (line + 0.5).floor().clamp(-1e9, 1e9) as i64;
        let full = if horizontal { i64::from(ch) } else { i64::from(cw) };
        if settings.vertical == Vertical::GrowingLeft {
            line = -(line + 1);
        }
        let mut position = step.saturating_mul(line);
        let mut step = step;
        if settings.vertical == Vertical::GrowingLeft {
            position = position - w + step;
        }
        if line < 0 {
            position = position.saturating_add(full);
            step = -step;
        }
        if horizontal {
            image.y = clamp_i32(position);
        } else {
            image.x = clamp_i32(position);
        }
        Layout::Snap { step, vertical: settings.vertical }
    } else {
        let (dimension, length) = if horizontal { (i64::from(ch), h) } else { (i64::from(cw), w) };
        let mut at = (line / 100.0 * dimension as f64).round() as i64;
        match settings.line_align {
            LineAlign::Center => at -= length / 2,
            LineAlign::End => at -= length,
            LineAlign::Start => {}
        }
        if horizontal {
            image.y = clamp_i32(at);
        } else {
            image.x = clamp_i32(at);
        }
        Layout::Closest
    };
    TextCue { image, layout }
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

/// The cue's lines wrapped at `width` on a `width x height` canvas,
/// cropped to its visible pixels; `None` when nothing is visible (no line
/// boxes: the cue is not shown).
fn render(cue: &SubtitleCue, width: i64, height: i64) -> Option<SubtitleImage> {
    let (width, height) = (u32::try_from(width).ok()?, u32::try_from(height).ok()?);
    if width == 0 || height == 0 {
        return None;
    }
    let rgba = Compositor::new(width, height).render(cue);
    crate::subs::visible_image(&rgba, width as usize, height as usize)
}

/// [`render`] with the order of the lines reversed (the first line
/// last): before a quarter turn clockwise, the lines of `vertical:lr`
/// then run left to right. The compositor sets lines a line height apart
/// from its bottom margin; each line's pixels stay within its band.
fn reverse_lines(cue: &SubtitleCue, width: i64, height: i64) -> SubtitleImage {
    let Some(rendered) = render(cue, width, height) else { return NOTHING };
    let comp = Compositor::new(1, 1);
    let font = BitmapFont::default_regular();
    let step = line_height();
    let outline = i64::from(comp.outline_px.min(2));
    let below = i64::from(font.cell_h - font.bearing_y.min(font.cell_h));
    let last_baseline = (height - i64::from(comp.bottom_margin_px) - below).max(0);
    // Bands run up from just under the last line's glyphs.
    let bottom = last_baseline + below + outline;
    let top = i64::from(rendered.y);
    let bands = ((bottom - top + step - 1) / step).max(1);
    let (w, row_bytes) = (rendered.width as usize, rendered.width as usize * 4);
    let mut rgba = vec![0u8; bands as usize * step as usize * row_bytes];
    for band in 0..bands {
        // Band `band` from the top moves to `bands - 1 - band`.
        let src_top = bottom - (bands - band) * step;
        let dst_top = (bands - 1 - band) * step;
        for r in 0..step {
            let src_row = src_top + r - top;
            if src_row < 0 || src_row >= i64::from(rendered.height) {
                continue;
            }
            let src = &rendered.rgba[src_row as usize * row_bytes..][..row_bytes];
            rgba[(dst_top + r) as usize * row_bytes..][..row_bytes].copy_from_slice(src);
        }
    }
    let reordered = crate::subs::visible_image(&rgba, w, bands as usize * step as usize).unwrap_or(NOTHING);
    SubtitleImage { x: rendered.x + reordered.x, y: 0, ..reordered }
}

/// `image` turned a quarter clockwise: its rows become columns, the first
/// row the rightmost. Its `x` (where the text sits along the line) stays.
fn rotate_cw(image: &SubtitleImage) -> SubtitleImage {
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
    SubtitleImage { x: image.x, y: 0, width: image.height, height: image.width, rgba }
}

/// Moves `image` per `layout` off `obstacles` within `canvas` (§7.2
/// "adjust the positions of boxes"). False when it is not to be shown.
pub(crate) fn place(image: &mut SubtitleImage, layout: &Layout, obstacles: &[Rect], canvas: Rect) -> bool {
    if image.width == 0 {
        return false;
    }
    let free = |r: &Rect| r.within(&canvas) && !obstacles.iter().any(|o| o.overlaps(r));
    match layout {
        Layout::Fixed | Layout::Region(_) => true,
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

/// The cues of one region, in the order they came up, stacked from the
/// region's bottom (the latest lowest) and clipped to the region box.
pub(crate) fn stack_region(slots: &[&RegionSlot]) -> Vec<SubtitleImage> {
    let Some(first) = slots.first() else { return Vec::new() };
    let rect = first.rect;
    let total: i64 = slots.iter().map(|s| s.block_height).sum();
    let mut top = rect.y + rect.h - total;
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
        *image = SubtitleImage { x: 0, y: 0, width: 0, height: 0, rgba: Vec::new() };
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
    use oxideav_core::Segment;

    const W: i64 = 640;
    const H: i64 = 360;
    const CANVAS: Rect = Rect { x: 0, y: 0, w: W, h: H };

    fn cue(text: &str) -> SubtitleCue {
        SubtitleCue { start_us: 0, end_us: 1_000_000, style_ref: None, positioning: None, segments: vec![Segment::Text(text.into())] }
    }

    fn laid(text: &str, settings: &str, header: &[u8]) -> TextCue {
        let track = WebVttTrack::new(header);
        let settings = CueSettings::parse(settings.as_bytes(), &track.regions);
        layout_cue(&cue(text), Some(&settings), &track, (W as u32, H as u32))
    }

    /// `text` with `settings`, placed alone on the canvas.
    fn placed(text: &str, settings: &str) -> Rect {
        let TextCue { mut image, layout } = laid(text, settings, b"");
        assert!(place(&mut image, &layout, &[], CANVAS), "{settings}: not shown");
        Rect::of(&image)
    }

    #[test]
    fn snapped_lines_count_from_the_top_or_the_bottom() {
        let step = line_height();
        assert_eq!(placed("top", "line:0").y, 0);
        assert_eq!(placed("third", "line:2").y, 2 * step);
        // Line -1 starts a line height above the bottom.
        let last = placed("last", "line:-1");
        assert_eq!(last.y, H - step);
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
        // align:start, size:50%: a box over the right half, text at its
        // start (auto position 50%, line-left).
        let right = placed("right half", "align:start size:50%");
        assert!(right.x >= W / 2 && right.x <= W / 2 + 10 && right.x + right.w <= W, "{right:?}");
        // align:end: the left half, text at its end.
        let left = placed("left half", "align:end size:50%");
        assert!(left.x + left.w <= W / 2 && left.x + left.w >= W / 2 - 10, "{left:?}");
        // position:10% line-left: the box starts at 10%.
        let at = placed("at ten", "position:10%,line-left align:left");
        assert!(at.x >= W / 10 && at.x <= W / 10 + 10, "{at:?}");
    }

    #[test]
    fn vertical_cues_turn_and_take_their_line_across() {
        // vertical:rl, line auto: the last line of a right-to-left stack,
        // the leftmost line box (§7.2: line -1, growing left); the glyphs
        // stand at the right of that box.
        let rl = placed("vertical text", "vertical:rl");
        assert!(rl.h > rl.w, "{rl:?}");
        assert!(rl.x >= 0 && rl.x + rl.w == line_height(), "{rl:?}");
        // FATE's "Title Wrap": lr, line 0 (left edge), 20% down, 60% tall.
        let lr = placed("Some time ago in a rather distant place....", "vertical:lr line:0 position:20% size:60% align:start");
        assert_eq!(lr.x, 0);
        assert!(lr.y >= H / 5 && lr.y <= H / 5 + 10 && lr.y + lr.h <= H / 5 + H * 3 / 5, "{lr:?}");
        // rl, line 0: the right edge.
        let right = placed("x", "vertical:rl line:0");
        assert_eq!(right.x + right.w, W);
    }

    #[test]
    fn snapped_cues_stack_off_the_cues_up() {
        let TextCue { image: first, layout } = laid("first", "line:-1", b"");
        let TextCue { image: mut second, .. } = laid("second", "line:-1", b"");
        let mut first = first;
        assert!(place(&mut first, &layout, &[], CANVAS));
        assert!(place(&mut second, &layout, &[Rect::of(&first)], CANVAS));
        assert_eq!(i64::from(second.y), i64::from(first.y) - line_height());
        // With no free line left, the cue is not shown.
        let full = [CANVAS];
        let TextCue { mut image, layout } = laid("third", "line:-1", b"");
        assert!(!place(&mut image, &layout, &full, CANVAS));
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
    fn region_cues_stack_from_the_region_bottom() {
        let header = b"WEBVTT\n\nREGION\nid:fred width:40% lines:2 regionanchor:0%,100% viewportanchor:10%,90%\n";
        let slot = |text: &str| match laid(text, "region:fred align:left", header).layout {
            Layout::Region(slot) => slot,
            other => panic!("not in the region: {other:?}"),
        };
        let (a, b, c) = (slot("one"), slot("two"), slot("three"));
        let rect = a.rect;
        assert_eq!(rect, Rect { x: 64, y: 324 - 2 * line_height(), w: 256, h: 2 * line_height() });
        let images = stack_region(&[&a, &b]);
        assert!(images.iter().all(|i| Rect::of(i).within(&rect)), "{images:?}");
        // The latest is lowest; both start at the region's left (align:left).
        assert!(images[1].y > images[0].y);
        assert!(images.iter().all(|i| i64::from(i.x) >= rect.x && i64::from(i.x) <= rect.x + 10));
        // A third line scrolls the first out of the two-line region.
        let images = stack_region(&[&a, &b, &c]);
        assert_eq!(images[0].width, 0);
        assert!(images[1].width > 0 && images[2].width > 0);
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

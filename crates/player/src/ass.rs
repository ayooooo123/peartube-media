//! SubtitleCue to the shared runtime-font renderer. Genuine ASS packets
//! bypass this conversion, retaining every override and event field.
use std::cell::RefCell;
use std::fmt::Write;
use std::sync::Arc;
use oxideav_core::{Segment, SubtitleCue, TextAlign};
use subs_render::{FontOptions, Renderer, Track, track::{Event, Feature, Style}};
use crate::backend::SubtitleImage;

thread_local! {
    static RENDERER: RefCell<Renderer> = RefCell::new(Renderer::new(&FontOptions::default()));
}

pub(crate) fn configure(fonts: &FontOptions, attachments: Vec<Arc<[u8]>>) {
    RENDERER.with(|slot| {
        let mut renderer = Renderer::new(fonts);
        for data in attachments { renderer.shaper.add_font(data); }
        *slot.borrow_mut() = renderer;
    });
}

pub(crate) fn default_track() -> Track {
    let mut track = Track::new();
    track.play_res_x = 384;
    track.play_res_y = 288;
    track.scaled_border_and_shadow = true;
    track.styles[0] = Style { name: b"Default".to_vec(), font_name: b"Arial".to_vec(), font_size: 16.0,
        primary_colour: 0xffffff00, secondary_colour: 0xffffff00, scale_x: 1.0, scale_y: 1.0,
        border_style: 1, alignment: 2, margin_l: 10, margin_r: 10, margin_v: 10, ..Style::default() };
    track.set_feature(Feature::WrapUnicode, true);
    track
}

fn literal(out: &mut String, text: &str, preserve_spaces: bool) {
    for ch in text.chars().take(8192) {
        if out.len() >= 64 << 10 { break; }
        match ch {
            '\n' => out.push_str("\\N"),
            '{' => out.push_str("\\{"), '}' => out.push_str("\\}"),
            // End a literal backslash's token before the next source character.
            '\\' => out.push_str("\\{}"),
            ' ' if preserve_spaces => out.push_str("\\h"),
            _ => out.push(ch),
        }
    }
}

#[derive(Clone, Default)]
struct Pen { bold: bool, italic: bool, underline: bool, strike: bool, colour: Option<(u8,u8,u8)>, family: Option<String>, size: Option<f32> }
impl Pen {
    fn tags(&self, out: &mut String) {
        out.push_str("{\\r");
        for (name, flag) in [("b", self.bold), ("i", self.italic), ("u", self.underline), ("s", self.strike)] {
            if flag { let _ = write!(out, "\\{name}1"); }
        }
        if let Some((r,g,b)) = self.colour { let _ = write!(out, "\\c&H{b:02X}{g:02X}{r:02X}&"); }
        if let Some(family) = &self.family { let _ = write!(out, "\\fn{}", family.replace(['\\','{','}'], "")); }
        if let Some(size) = self.size.filter(|s| s.is_finite() && *s > 0.0) { let _ = write!(out, "\\fs{}", size.min(8192.0)); }
        out.push('}');
    }
}

fn segments(items: &[Segment], pen: &Pen, out: &mut String, captions: bool, depth: u8) {
    if depth >= 32 { return; }
    for segment in items {
        if out.len() >= 64 << 10 { return; }
        let mut child = pen.clone();
        let children = match segment {
            Segment::Text(text) | Segment::Raw(text) => { literal(out, text, captions); continue; }
            Segment::LineBreak => { out.push_str("\\N"); continue; }
            Segment::Timestamp { .. } => continue,
            Segment::Bold(c) => { child.bold = true; c }
            Segment::Italic(c) => { child.italic = true; c }
            Segment::Underline(c) => { child.underline = true; c }
            Segment::Strike(c) => { child.strike = true; c }
            Segment::Color { rgb, children } => { child.colour = Some(*rgb); children }
            Segment::Font { family, size, children } => {
                if family.is_some() { child.family.clone_from(family); }
                if size.is_some() { child.size = *size; }
                children
            }
            Segment::Karaoke { cs, children } => { let _ = write!(out, "{{\\k{cs}}}"); children }
            Segment::Voice { children, .. } | Segment::Class { children, .. } => children,
        };
        child.tags(out);
        segments(children, &child, out, captions, depth + 1);
        pen.tags(out);
    }
}

pub(crate) fn event(cue: &SubtitleCue) -> Event {
    let mut text = String::new();
    let captions = cue.style_ref.as_deref() == Some(subs_cc::STATE_STYLE);
    let pen = if captions { Pen { family: Some("monospace".into()), ..Pen::default() } } else { Pen::default() };
    pen.tags(&mut text);
    if let Some(position) = &cue.positioning {
        let align = match position.align { TextAlign::Start | TextAlign::Left => 1, TextAlign::End | TextAlign::Right => 3, _ => 2 };
        // Caption origins are percentages from the top of the display,
        // not an ASS bottom-baseline anchor in script pixels.
        let align = if captions { align + 6 } else { align };
        let _ = write!(text, "{{\\an{align}}}");
        if let (Some(x), Some(y)) = (position.x, position.y) {
            if x.is_finite() && y.is_finite() {
                let (x, y) = if captions { (x * 3.84, y * 2.88) } else { (x, y) };
                let _ = write!(text, "{{\\pos({x},{y})}}");
            }
        }
    }
    segments(&cue.segments, &pen, &mut text, captions, 0);
    Event { start: cue.start_us / 1000, duration: cue.end_us.saturating_sub(cue.start_us).max(1) / 1000,
        text: text.into_bytes(), ..Event::default() }
}

pub(crate) fn image(image: subs_render::Image) -> SubtitleImage {
    SubtitleImage { x: image.x, y: image.y, width: image.width, height: image.height, rgba: image.rgba }
}

pub(crate) fn render(track: &mut Track, time: i64, canvas: (u32, u32)) -> subs_render::Frame {
    RENDERER.with(|r| r.borrow_mut().render(track, time, canvas.0, canvas.1))
}

pub(crate) fn render_cue(cue: &SubtitleCue, canvas: (u32, u32)) -> SubtitleImage {
    let mut track = default_track();
    let mut event = event(cue);
    event.duration = event.duration.max(1);
    track.events.push(event);
    image(render(&mut track, cue.start_us / 1000, canvas).image)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CuePosition;

    #[test]
    fn caption_origins_use_top_left_percentages() {
        let mut cue = SubtitleCue {
            start_us: 0, end_us: i64::MAX,
            style_ref: Some(subs_cc::STATE_STYLE.into()),
            positioning: Some(CuePosition { x: Some(10.0), y: Some(0.0), align: TextAlign::Left, size: None }),
            segments: vec![Segment::Text("Caption".into())],
        };
        let first = render_cue(&cue, (384, 288));
        assert!(first.width > 0 && first.height > 0, "a top-row caption must not be clipped away");
        assert!((38..=42).contains(&first.x) && (0..=8).contains(&first.y));
        cue.positioning = Some(CuePosition { x: Some(60.0), y: Some(50.0), align: TextAlign::Left, size: None });
        let moved = render_cue(&cue, (384, 288));
        assert!((moved.x - first.x - 192).abs() <= 1);
        assert!((moved.y - first.y - 144).abs() <= 1);
    }
}

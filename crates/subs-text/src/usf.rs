//! Universal Subtitle Format (`usf`) decoder.
//!
//! Ported to safe Rust from VLC's `modules/codec/subsusf.c`
//! (LGPL-2.1-or-later — header verified).
//!
//! USF carries structured XML subtitles with metadata, style sheets,
//! positioned text, and rich markup. In Matroska containers it travels
//! under CodecID `S_TEXT/USF`. Extradata carries the `<USFSubtitles>`
//! header with style definitions; individual packets carry `<subtitle>`
//! or `<text>` elements.

use std::collections::{HashMap, VecDeque};

use oxideav_core::{
    CodecId, CodecParameters, CuePosition, Decoder, Error, Frame, Packet, Result,
    Segment, SubtitleCue, TextAlign, TimeBase,
};

use crate::xml::{decode_entities, Scanner, Token};
use crate::USF_CODEC_ID;

const MAX_CUE_BYTES: usize = 1 << 20;
const MAX_STYLES: usize = 1024;

/// A parsed USF style definition.
#[derive(Clone, Debug, Default)]
pub struct UsfStyle {
    pub name: String,
    pub font_face: Option<String>,
    pub font_size: Option<f32>,
    pub color: Option<(u8, u8, u8)>,
    pub italic: bool,
    pub bold: bool,
    pub underline: bool,
    pub align: TextAlign,
    pub margin_x: Option<f32>,
    pub margin_y: Option<f32>,
}

#[derive(Default)]
pub struct UsfContext {
    pub styles: HashMap<String, UsfStyle>,
    pub original_width: Option<u32>,
    pub original_height: Option<u32>,
}

impl UsfContext {
    pub fn parse_header(&mut self, data: &[u8]) -> Result<()> {
        let mut scanner = Scanner::new(data);
        let mut current_style: Option<UsfStyle> = None;

        while let Some(tok) = scanner.next()? {
            match tok {
                Token::Start { name, attrs, self_closing } => {
                    if name.eq_ignore_ascii_case("resolution") {
                        for (k, v) in attrs {
                            if k.eq_ignore_ascii_case("x") {
                                self.original_width = v.parse().ok();
                            } else if k.eq_ignore_ascii_case("y") {
                                self.original_height = v.parse().ok();
                            }
                        }
                    } else if name.eq_ignore_ascii_case("style") {
                        if self.styles.len() < MAX_STYLES {
                            let mut style = UsfStyle::default();
                            for (k, v) in attrs {
                                if k.eq_ignore_ascii_case("name") {
                                    style.name = v.to_string();
                                }
                            }
                            if self_closing {
                                if !style.name.is_empty() {
                                    self.styles.insert(style.name.clone(), style);
                                }
                            } else {
                                current_style = Some(style);
                            }
                        }
                    } else if let Some(st) = &mut current_style {
                        if name.eq_ignore_ascii_case("fontstyle") {
                            for (k, v) in attrs {
                                if k.eq_ignore_ascii_case("face") {
                                    st.font_face = Some(v.to_string());
                                } else if k.eq_ignore_ascii_case("size") {
                                    st.font_size = v.parse().ok();
                                } else if k.eq_ignore_ascii_case("color") {
                                    st.color = parse_color(v);
                                } else if k.eq_ignore_ascii_case("italic") {
                                    st.italic = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                } else if k.eq_ignore_ascii_case("bold") {
                                    st.bold = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                } else if k.eq_ignore_ascii_case("underline") {
                                    st.underline = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                }
                            }
                        } else if name.eq_ignore_ascii_case("position") {
                            for (k, v) in attrs {
                                if k.eq_ignore_ascii_case("alignment") {
                                    st.align = parse_alignment(v);
                                } else if k.eq_ignore_ascii_case("horizontal-margin") {
                                    st.margin_x = v.trim_end_matches('%').parse().ok();
                                } else if k.eq_ignore_ascii_case("vertical-margin") {
                                    st.margin_y = v.trim_end_matches('%').parse().ok();
                                }
                            }
                        }
                    }
                }
                Token::End(name) => {
                    if name.eq_ignore_ascii_case("style") {
                        if let Some(st) = current_style.take() {
                            if !st.name.is_empty() {
                                self.styles.insert(st.name.clone(), st);
                            }
                        }
                    }
                }
                Token::Text(_) => {}
            }
        }
        Ok(())
    }
}

pub fn parse_color(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    if s.len() == 6 {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some((r, g, b))
    } else if s.len() == 8 {
        let r = u8::from_str_radix(&s[2..4], 16).ok()?;
        let g = u8::from_str_radix(&s[4..6], 16).ok()?;
        let b = u8::from_str_radix(&s[6..8], 16).ok()?;
        Some((r, g, b))
    } else {
        None
    }
}

pub fn parse_alignment(s: &str) -> TextAlign {
    let s = s.trim();
    if s.eq_ignore_ascii_case("TopLeft")
        || s.eq_ignore_ascii_case("MiddleLeft")
        || s.eq_ignore_ascii_case("BottomLeft")
    {
        TextAlign::Left
    } else if s.eq_ignore_ascii_case("TopRight")
        || s.eq_ignore_ascii_case("MiddleRight")
        || s.eq_ignore_ascii_case("BottomRight")
    {
        TextAlign::Right
    } else {
        TextAlign::Center
    }
}

/// Parse timestamp string in seconds or `HH:MM:SS.mmm` into microseconds.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches("npt:");
    if s.is_empty() {
        return None;
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            2 => {
                let mins: f64 = parts[0].parse().ok()?;
                let secs: f64 = parts[1].parse().ok()?;
                Some(((mins * 60.0 + secs) * 1_000_000.0) as i64)
            }
            3 => {
                let hours: f64 = parts[0].parse().ok()?;
                let mins: f64 = parts[1].parse().ok()?;
                let secs: f64 = parts[2].parse().ok()?;
                Some((((hours * 60.0 + mins) * 60.0 + secs) * 1_000_000.0) as i64)
            }
            _ => None,
        }
    } else {
        let secs: f64 = s.parse().ok()?;
        Some((secs * 1_000_000.0) as i64)
    }
}

/// Decode USF XML payload into one or more SubtitleCue events.
pub fn decode_usf_payload(
    ctx: &mut UsfContext,
    data: &[u8],
    packet: &Packet,
) -> Result<Vec<SubtitleCue>> {
    let mut scanner = Scanner::new(data);
    let mut cues = Vec::new();

    let packet_start = packet.pts.map(|pts| {
        packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000))
    });
    let packet_end = match (packet_start, packet.duration) {
        (Some(start), Some(dur)) => {
            Some(start + packet.time_base.rescale(dur, TimeBase::new(1, 1_000_000)))
        }
        _ => None,
    };

    enum Container {
        Root,
        Bold,
        Italic,
        Underline,
        Strike,
        Font {
            family: Option<String>,
            size: Option<f32>,
        },
        Color((u8, u8, u8)),
        Karaoke(u32),
    }

    struct CueBuilder {
        start_us: Option<i64>,
        end_us: Option<i64>,
        style_ref: Option<String>,
        positioning: Option<CuePosition>,
        stack: Vec<(Container, Vec<Segment>)>,
    }

    impl CueBuilder {
        fn new() -> Self {
            Self {
                start_us: None,
                end_us: None,
                style_ref: None,
                positioning: None,
                stack: vec![(Container::Root, Vec::new())],
            }
        }

        fn push_segment(&mut self, seg: Segment) {
            if let Some((_, children)) = self.stack.last_mut() {
                children.push(seg);
            }
        }

        fn push_container(&mut self, c: Container) {
            self.stack.push((c, Vec::new()));
        }

        fn pop_container(&mut self) {
            if self.stack.len() > 1 {
                let (c, children) = self.stack.pop().unwrap();
                let seg = match c {
                    Container::Root => return,
                    Container::Bold => Segment::Bold(children),
                    Container::Italic => Segment::Italic(children),
                    Container::Underline => Segment::Underline(children),
                    Container::Strike => Segment::Strike(children),
                    Container::Font { family, size } => Segment::Font {
                        family,
                        size,
                        children,
                    },
                    Container::Color(rgb) => Segment::Color { rgb, children },
                    Container::Karaoke(cs) => Segment::Karaoke { cs, children },
                };
                self.push_segment(seg);
            }
        }

        fn finish(
            mut self,
            fallback_start: Option<i64>,
            fallback_end: Option<i64>,
            ctx: &UsfContext,
        ) -> Option<SubtitleCue> {
            while self.stack.len() > 1 {
                self.pop_container();
            }
            let (_, mut segments) = self.stack.pop()?;
            if segments.is_empty() {
                return None;
            }
            let start_us = self.start_us.or(fallback_start).unwrap_or(0);
            let end_us = self.end_us.or(fallback_end).unwrap_or(start_us);

            let style_name = self.style_ref.as_deref().unwrap_or("Default");
            if let Some(st) = ctx.styles.get(style_name) {
                if self.positioning.is_none() {
                    self.positioning = Some(CuePosition {
                        x: st.margin_x,
                        y: st.margin_y,
                        align: st.align,
                        size: None,
                    });
                }
                if st.font_face.is_some() || st.font_size.is_some() {
                    segments = vec![Segment::Font {
                        family: st.font_face.clone(),
                        size: st.font_size,
                        children: segments,
                    }];
                }
            }

            Some(SubtitleCue {
                start_us,
                end_us,
                style_ref: self.style_ref,
                positioning: self.positioning,
                segments,
            })
        }
    }

    let mut current_cue: Option<CueBuilder> = None;
    let mut in_metadata = false;
    let mut in_styles = false;
    let mut current_style: Option<UsfStyle> = None;
    while let Some(tok) = scanner.next()? {
        match tok {
            Token::Start { name, attrs, self_closing } => {
                if name.eq_ignore_ascii_case("USFSubtitles") {
                    // Whole document embedded
                } else if name.eq_ignore_ascii_case("metadata") {
                    if !self_closing {
                        in_metadata = true;
                    }
                } else if name.eq_ignore_ascii_case("styles") {
                    if !self_closing {
                        in_styles = true;
                    }
                } else if name.eq_ignore_ascii_case("style") {
                    if ctx.styles.len() < MAX_STYLES {
                        let mut style = UsfStyle::default();
                        for (k, v) in &attrs {
                            if k.eq_ignore_ascii_case("name") {
                                style.name = v.to_string();
                            }
                        }
                        if self_closing {
                            if !style.name.is_empty() {
                                ctx.styles.insert(style.name.clone(), style);
                            }
                        } else {
                            current_style = Some(style);
                        }
                    }
                } else if in_styles && current_style.is_some() {
                    if let Some(st) = &mut current_style {
                        if name.eq_ignore_ascii_case("fontstyle") {
                            for (k, v) in attrs {
                                if k.eq_ignore_ascii_case("face") {
                                    st.font_face = Some(v.to_string());
                                } else if k.eq_ignore_ascii_case("size") {
                                    st.font_size = v.parse().ok();
                                } else if k.eq_ignore_ascii_case("color") {
                                    st.color = parse_color(v);
                                } else if k.eq_ignore_ascii_case("italic") {
                                    st.italic = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                } else if k.eq_ignore_ascii_case("bold") {
                                    st.bold = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                } else if k.eq_ignore_ascii_case("underline") {
                                    st.underline = v.eq_ignore_ascii_case("yes")
                                        || v.eq_ignore_ascii_case("true")
                                        || v == "1";
                                }
                            }
                        } else if name.eq_ignore_ascii_case("position") {
                            for (k, v) in attrs {
                                if k.eq_ignore_ascii_case("alignment") {
                                    st.align = parse_alignment(v);
                                } else if k.eq_ignore_ascii_case("horizontal-margin") {
                                    st.margin_x = v.trim_end_matches('%').parse().ok();
                                } else if k.eq_ignore_ascii_case("vertical-margin") {
                                    st.margin_y = v.trim_end_matches('%').parse().ok();
                                }
                            }
                        }
                    }
                } else if name.eq_ignore_ascii_case("subtitle") {
                    let mut b = CueBuilder::new();
                    let mut start = None;
                    let mut stop = None;
                    let mut dur = None;
                    for (k, v) in attrs {
                        if k.eq_ignore_ascii_case("start") {
                            start = parse_timestamp(v);
                        } else if k.eq_ignore_ascii_case("stop") {
                            stop = parse_timestamp(v);
                        } else if k.eq_ignore_ascii_case("duration") {
                            dur = parse_timestamp(v);
                        } else if k.eq_ignore_ascii_case("style") {
                            b.style_ref = Some(v.to_string());
                        } else if k.eq_ignore_ascii_case("alignment") {
                            let align = parse_alignment(v);
                            let pos = b.positioning.get_or_insert_with(CuePosition::default);
                            pos.align = align;
                        }
                    }
                    b.start_us = start;
                    b.end_us = stop.or_else(|| {
                        match (start, dur) {
                            (Some(s), Some(d)) => Some(s + d),
                            _ => None,
                        }
                    });
                    if self_closing {
                        if let Some(cue) = b.finish(packet_start, packet_end, ctx) {
                            cues.push(cue);
                        }
                    } else {
                        current_cue = Some(b);
                    }
                } else if name.eq_ignore_ascii_case("text") {
                    if current_cue.is_none() {
                        let mut b = CueBuilder::new();
                        for (k, v) in &attrs {
                            if k.eq_ignore_ascii_case("style") {
                                b.style_ref = Some(v.to_string());
                            } else if k.eq_ignore_ascii_case("alignment") {
                                let align = parse_alignment(v);
                                let pos = b.positioning.get_or_insert_with(CuePosition::default);
                                pos.align = align;
                            }
                        }
                        current_cue = Some(b);
                    }
                } else if let Some(b) = &mut current_cue {
                    if name.eq_ignore_ascii_case("b") {
                        b.push_container(Container::Bold);
                    } else if name.eq_ignore_ascii_case("i") {
                        b.push_container(Container::Italic);
                    } else if name.eq_ignore_ascii_case("u") {
                        b.push_container(Container::Underline);
                    } else if name.eq_ignore_ascii_case("s") {
                        b.push_container(Container::Strike);
                    } else if name.eq_ignore_ascii_case("br") {
                        b.push_segment(Segment::LineBreak);
                    } else if name.eq_ignore_ascii_case("font") {
                        let mut family = None;
                        let mut size = None;
                        let mut color = None;
                        for (k, v) in attrs {
                            if k.eq_ignore_ascii_case("face") {
                                family = Some(v.to_string());
                            } else if k.eq_ignore_ascii_case("size") {
                                size = v.parse().ok();
                            } else if k.eq_ignore_ascii_case("color") {
                                color = parse_color(v);
                            }
                        }
                        if let Some(c) = color {
                            b.push_container(Container::Color(c));
                        }
                        if family.is_some() || size.is_some() {
                            b.push_container(Container::Font { family, size });
                        }
                    } else if name.eq_ignore_ascii_case("k") {
                        let mut cs = 0;
                        for (k, v) in attrs {
                            if k.eq_ignore_ascii_case("t") || k.eq_ignore_ascii_case("d") {
                                if let Ok(val) = v.parse::<u32>() {
                                    cs = val;
                                }
                            }
                        }
                        b.push_container(Container::Karaoke(cs));
                    }
                }
            }
            Token::End(name) => {
                if name.eq_ignore_ascii_case("metadata") {
                    in_metadata = false;
                } else if name.eq_ignore_ascii_case("styles") {
                    in_styles = false;
                } else if name.eq_ignore_ascii_case("style") {
                    if let Some(st) = current_style.take() {
                        if !st.name.is_empty() {
                            ctx.styles.insert(st.name.clone(), st);
                        }
                    }
                } else
                if name.eq_ignore_ascii_case("subtitle") {
                    if let Some(b) = current_cue.take() {
                        if let Some(cue) = b.finish(packet_start, packet_end, ctx) {
                            cues.push(cue);
                        }
                    }
                } else if name.eq_ignore_ascii_case("text") {
                    // In single-text packet mode, text end can finalize the cue
                    if let Some(b) = &current_cue {
                        if b.start_us.is_none() && packet_start.is_some() {
                            if let Some(b) = current_cue.take() {
                                if let Some(cue) = b.finish(packet_start, packet_end, ctx) {
                                    cues.push(cue);
                                }
                            }
                        }
                    }
                } else if let Some(b) = &mut current_cue {
                    if name.eq_ignore_ascii_case("b")
                        || name.eq_ignore_ascii_case("i")
                        || name.eq_ignore_ascii_case("u")
                        || name.eq_ignore_ascii_case("s")
                        || name.eq_ignore_ascii_case("font")
                        || name.eq_ignore_ascii_case("k")
                    {
                        b.pop_container();
                    }
                }
            }
            Token::Text(s) => {
                if !in_metadata && !in_styles {
                    let clean = decode_entities(&s);
                    if let Some(b) = &mut current_cue {
                        b.push_segment(Segment::Text(clean));
                    } else if !clean.trim().is_empty() {
                        // Plain text packet without wrapper
                        let mut b = CueBuilder::new();
                        b.push_segment(Segment::Text(clean));
                        if let Some(cue) = b.finish(packet_start, packet_end, ctx) {
                            cues.push(cue);
                        }
                    }
                }
            }
        }
    }

    if let Some(b) = current_cue.take() {
        if let Some(cue) = b.finish(packet_start, packet_end, ctx) {
            cues.push(cue);
        }
    }

    Ok(cues)
}

pub struct UsfDecoder {
    codec_id: CodecId,
    ctx: UsfContext,
    pending: VecDeque<Frame>,
    eof: bool,
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != USF_CODEC_ID {
        return Err(Error::unsupported(format!("not a usf codec id: {}", params.codec_id)));
    }
    let mut ctx = UsfContext::default();
    if !params.extradata.is_empty() {
        let _ = ctx.parse_header(&params.extradata);
    }
    Ok(Box::new(UsfDecoder {
        codec_id: params.codec_id.clone(),
        ctx,
        pending: VecDeque::new(),
        eof: false,
    }))
}

impl Decoder for UsfDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("USF packet too large"));
        }
        let cues = decode_usf_payload(&mut self.ctx, &packet.data, packet)?;
        for cue in cues {
            self.pending.push_back(Frame::Subtitle(cue));
        }
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

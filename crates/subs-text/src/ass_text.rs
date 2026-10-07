//! ASS events to displayed cues, and the decoder shared by every text
//! format FFmpeg decodes to ASS.
//!
//! The shown text is what FFmpeg's `text` subtitle encoder makes of an
//! event (`libavcodec/srtenc.c` at commit 2da55bf, LGPL-2.1-or-later —
//! header verified): the `text` and `new_line` callbacks of
//! `ff_ass_split_override_codes`, every override block hidden. Styling uses
//! the callbacks FFmpeg's SubRip encoder maps to markup — the event style
//! from the script header (`srt_style_apply`), `\b \i \u \s`, primary
//! colour, font name and size, alignment and `\r` — with the renderer's
//! state semantics: a style switch holds until it is switched back, `\r`
//! returns to the event's style and the first alignment override wins.

use std::collections::VecDeque;

use oxideav_core::{CodecId, CuePosition, Decoder, Error, Frame, Packet, Result, Segment, SubtitleCue, TextAlign, TimeBase};

use crate::ass_split::{split_override_codes, AssHeader, AssStyle, OverrideCallbacks};

/// FFmpeg's `ASS_DEFAULT_*` style values.
pub const DEFAULT_FONT: &str = "Arial";
pub const DEFAULT_FONT_SIZE: i32 = 16;
pub const DEFAULT_COLOR: u32 = 0xffffff;
pub const DEFAULT_ALIGNMENT: i32 = 2;

/// Largest packet a text decoder accepts.
pub const MAX_CUE_BYTES: usize = 1 << 20;

/// The `Default` style `ff_ass_subtitle_header` writes for decoders that
/// convert other formats to ASS.
pub fn default_header(font: &str, font_size: i32, color: u32, bold: bool, italic: bool, underline: bool, alignment: i32) -> AssHeader {
    AssHeader {
        styles: vec![AssStyle {
            name: Some("Default".into()),
            font_name: Some(font.into()),
            font_size,
            primary_color: color,
            bold: -i32::from(bold),
            italic: -i32::from(italic),
            underline: -i32::from(underline),
            strikeout: 0,
            alignment,
        }],
    }
}

/// `ff_ass_subtitle_header_default`.
pub fn ffmpeg_default_header() -> AssHeader {
    default_header(DEFAULT_FONT, DEFAULT_FONT_SIZE, DEFAULT_COLOR, false, false, false, DEFAULT_ALIGNMENT)
}

#[derive(Clone, Debug, Default, PartialEq)]
struct RunStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    color: Option<(u8, u8, u8)>,
    font_name: Option<String>,
    font_size: Option<f32>,
}

fn bgr(color: u32) -> (u8, u8, u8) {
    (color as u8, (color >> 8) as u8, (color >> 16) as u8)
}

impl RunStyle {
    /// What `srt_style_apply` opens for `style`.
    fn of(style: Option<&AssStyle>) -> Self {
        let Some(st) = style else { return Self::default() };
        let c = st.primary_color & 0xFF_FFFF;
        Self {
            bold: st.bold != 0,
            italic: st.italic != 0,
            underline: st.underline != 0,
            strike: st.strikeout != 0,
            color: (c != DEFAULT_COLOR).then(|| bgr(c)),
            font_name: st.font_name.clone().filter(|f| f != DEFAULT_FONT),
            font_size: (st.font_size != DEFAULT_FONT_SIZE).then_some(st.font_size as f32),
        }
    }

    fn wrap(&self, segment: Segment) -> Segment {
        let mut s = segment;
        if self.strike {
            s = Segment::Strike(vec![s]);
        }
        if self.underline {
            s = Segment::Underline(vec![s]);
        }
        if self.italic {
            s = Segment::Italic(vec![s]);
        }
        if self.bold {
            s = Segment::Bold(vec![s]);
        }
        if let Some(rgb) = self.color {
            s = Segment::Color { rgb, children: vec![s] };
        }
        if self.font_name.is_some() || self.font_size.is_some() {
            s = Segment::Font { family: self.font_name.clone(), size: self.font_size, children: vec![s] };
        }
        s
    }
}

struct Builder<'h> {
    header: &'h AssHeader,
    base: RunStyle,
    state: RunStyle,
    segments: Vec<Segment>,
    alignment: i32,
    aligned_inline: bool,
}

impl OverrideCallbacks for Builder<'_> {
    fn text(&mut self, text: &[u8]) {
        let text = String::from_utf8_lossy(text).into_owned();
        self.segments.push(self.state.wrap(Segment::Text(text)));
    }

    fn new_line(&mut self, _forced: bool) {
        self.segments.push(Segment::LineBreak);
    }

    fn style(&mut self, style: u8, close: i32) {
        let (state, base) = match style {
            b'b' => (&mut self.state.bold, self.base.bold),
            b'i' => (&mut self.state.italic, self.base.italic),
            b'u' => (&mut self.state.underline, self.base.underline),
            _ => (&mut self.state.strike, self.base.strike),
        };
        *state = if close == -1 { base } else { close == 0 };
    }

    fn color(&mut self, color: u32, color_id: u32) {
        if color_id <= 1 {
            self.state.color = if color == 0xFFFF_FFFF { self.base.color } else { Some(bgr(color)) };
        }
    }

    fn font_name(&mut self, name: Option<&[u8]>) {
        self.state.font_name = match name {
            Some(name) => Some(String::from_utf8_lossy(name).into_owned()),
            None => self.base.font_name.clone(),
        };
    }

    fn font_size(&mut self, size: i32) {
        self.state.font_size = if size < 0 { self.base.font_size } else { Some(size as f32) };
    }

    fn alignment(&mut self, alignment: i32) {
        if !self.aligned_inline && (1..=9).contains(&alignment) {
            self.alignment = alignment;
            self.aligned_inline = true;
        }
    }

    fn cancel_overrides(&mut self, style: &[u8]) {
        self.state = if style.is_empty() {
            self.base.clone()
        } else {
            match self.header.style(&String::from_utf8_lossy(style)) {
                Some(named) => RunStyle::of(Some(named)),
                None => self.base.clone(),
            }
        };
    }
}

/// The cue an ASS event (`style` from the event, `text` its Text field)
/// shows from `start_us` to `end_us`.
pub fn event_to_cue(header: &AssHeader, style: &[u8], text: &[u8], start_us: i64, end_us: i64) -> SubtitleCue {
    let event_style = header.style(&String::from_utf8_lossy(style));
    let base = RunStyle::of(event_style);
    let mut builder = Builder {
        header,
        state: base.clone(),
        base,
        segments: Vec::new(),
        alignment: event_style.map_or(DEFAULT_ALIGNMENT, |s| s.alignment),
        aligned_inline: false,
    };
    // An unterminated override block ends the shown text there, as it ends
    // FFmpeg's encode of the event.
    let _ = split_override_codes(&mut builder, text);
    let align = match (builder.alignment - 1).rem_euclid(3) {
        0 if (1..=9).contains(&builder.alignment) => Some(TextAlign::Left),
        2 if (1..=9).contains(&builder.alignment) => Some(TextAlign::Right),
        _ => None,
    };
    SubtitleCue {
        start_us,
        end_us,
        style_ref: None,
        positioning: align.map(|align| CuePosition { x: None, y: None, align, size: None }),
        segments: builder.segments,
    }
}

/// FFmpeg's checks before a decoded subtitle is returned: the event must be
/// UTF-8 (`utf8_check`, without `-sub_charenc`).
fn checked_utf8(bytes: &[u8]) -> Result<&[u8]> {
    std::str::from_utf8(bytes).map(|_| bytes).map_err(|_| Error::invalid("invalid UTF-8 in decoded subtitle text"))
}

/// A packet's text as FFmpeg's decoders read it: up to the first NUL.
pub fn packet_text(packet: &Packet) -> &[u8] {
    crate::scan::c_str(&packet.data)
}

/// The ASS event one packet decodes to.
pub struct AssEvent {
    pub style: Vec<u8>,
    pub text: Vec<u8>,
}

impl AssEvent {
    /// An event in the `Default` style, as `ff_ass_add_rect` writes them.
    pub fn default_style(text: Vec<u8>) -> Self {
        Self { style: b"Default".to_vec(), text }
    }
}

/// Converts packets of one format to ASS events (`None`: FFmpeg's decoder
/// returns no subtitle for this packet).
pub trait EventSource: Send {
    fn event(&mut self, packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>>;

    /// `flush`/`reset`: forget per-stream state.
    fn reset(&mut self) {}
}

/// A decoder emitting one cue per packet its [`EventSource`] converts.
pub struct AssEventDecoder<S: EventSource> {
    codec_id: CodecId,
    header: AssHeader,
    source: S,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl<S: EventSource> AssEventDecoder<S> {
    pub fn new(codec_id: CodecId, header: AssHeader, source: S) -> Self {
        Self { codec_id, header, source, pending: VecDeque::new(), eof: false }
    }
}

impl<S: EventSource + 'static> Decoder for AssEventDecoder<S> {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("subtitle packet exceeds 1 MiB"));
        }
        let text = packet_text(packet);
        if text.is_empty() {
            return Ok(());
        }
        let Some(event) = self.source.event(packet, text)? else { return Ok(()) };
        let style = checked_utf8(&event.style)?;
        let ass = checked_utf8(&event.text)?;
        let start_us = packet.time_base.rescale(packet.pts.unwrap_or(0), TimeBase::new(1, 1_000_000));
        let end_us = crate::text_common::subtitle_end_us(packet, start_us);
        let cue = event_to_cue(&self.header, style, ass, start_us, end_us);
        self.pending.push_back(Frame::Subtitle(cue));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        self.pending.pop_front().ok_or(if self.eof { Error::Eof } else { Error::NeedMore })
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.eof = false;
        self.source.reset();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(cue: &SubtitleCue) -> String {
        fn walk(segments: &[Segment], out: &mut String) {
            for s in segments {
                match s {
                    Segment::Text(t) | Segment::Raw(t) => out.push_str(t),
                    Segment::LineBreak => out.push('\n'),
                    Segment::Bold(c) | Segment::Italic(c) | Segment::Underline(c) | Segment::Strike(c)
                    | Segment::Color { children: c, .. } | Segment::Font { children: c, .. } => walk(c, out),
                    _ => {}
                }
            }
        }
        let mut out = String::new();
        walk(&cue.segments, &mut out);
        out
    }

    #[test]
    fn styles_switch_like_a_renderer_and_overrides_stay_hidden() {
        let header = crate::ass_split::split_header(
            b"[V4+ Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, Bold, Alignment\nStyle: Default,Arial,16,&H00FFFFFF,0,2\nStyle: Sign,Arial,16,&H0000FFFF,-1,9\n",
        );
        let cue = event_to_cue(&header, b"Sign", br"{\pos(1,2)}A{\b0}B{\r}C{\rDefault}D\Ne", 0, 1);
        assert_eq!(shown(&cue), "ABCD\ne");
        let yellow = |s: Segment| Segment::Color { rgb: (255, 255, 0), children: vec![s] };
        let expected = [
            yellow(Segment::Bold(vec![Segment::Text("A".into())])),
            yellow(Segment::Text("B".into())),
            yellow(Segment::Bold(vec![Segment::Text("C".into())])),
            Segment::Text("D".into()),
        ];
        assert_eq!(format!("{:?}", &cue.segments[..4]), format!("{expected:?}"));
        assert_eq!(cue.positioning.map(|p| p.align), Some(TextAlign::Right));
        // The first alignment override wins over the style's.
        let cue = event_to_cue(&header, b"Sign", br"{\an7}x{\an3}", 0, 1);
        assert_eq!(cue.positioning.map(|p| p.align), Some(TextAlign::Left));
    }
}

// Copyright (C) 2006 Evgeniy Stepanov <eugeni.stepanov@gmail.com>
//
// This file is part of libass.
//
// Permission to use, copy, modify, and distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
// ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
// ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
// OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
//
// Derived from libass 0.17.5 (commit 4a05d81): libass/ass.c,
// libass/ass_types.h and libass/ass_priv.h. Changed for PearTube on
// 2026-10-08: ported to safe Rust; character set conversion and file
// reading left out; allocations bounded.

//! A subtitle track: the script header, styles and events, parsed as
//! libass parses `.ass`/`.ssa` scripts and Matroska's ASS chunks.

use std::collections::HashSet;

use crate::utils::*;

/// Styles a track keeps at most (a hostile header cannot grow it further).
pub const MAX_STYLES: usize = 4096;
/// Events a track holds at most; later ones are dropped.
pub const MAX_EVENTS: usize = 1 << 16;
/// Embedded font data a track decodes at most, in all.
pub const MAX_FONT_BYTES: usize = 64 << 20;

const ASS_STYLE_FORMAT: &[u8] = b"Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding";
const ASS_EVENT_FORMAT: &[u8] = b"Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";
const SSA_STYLE_FORMAT: &[u8] = b"Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding";
const SSA_EVENT_FORMAT: &[u8] = b"Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub name: Vec<u8>,
    pub font_name: Vec<u8>,
    pub font_size: f64,
    /// RGBA, the alpha byte transparency (0 opaque).
    pub primary_colour: u32,
    pub secondary_colour: u32,
    pub outline_colour: u32,
    pub back_colour: u32,
    pub bold: i32,
    pub italic: i32,
    pub underline: i32,
    pub strike_out: i32,
    /// 1.0 is 100%.
    pub scale_x: f64,
    pub scale_y: f64,
    pub spacing: f64,
    pub angle: f64,
    pub border_style: i32,
    pub outline: f64,
    pub shadow: f64,
    /// `VALIGN_* | HALIGN_*`.
    pub alignment: i32,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    pub encoding: i32,
    pub blur: f64,
    pub justify: i32,
}

impl Style {
    /// `set_default_style`: VSFilter's defaults.
    fn libass_default() -> Style {
        Style {
            name: b"Default".to_vec(),
            font_name: b"Arial".to_vec(),
            font_size: 18.0,
            primary_colour: 0xffff_ff00,
            secondary_colour: 0x00ff_ff00,
            outline_colour: 0,
            back_colour: 0x0000_0080,
            bold: 200,
            scale_x: 1.0,
            scale_y: 1.0,
            border_style: 1,
            outline: 2.0,
            shadow: 3.0,
            alignment: 2,
            margin_l: 20,
            margin_r: 20,
            margin_v: 20,
            ..Style::default()
        }
    }
}

/// Where a shown event was placed, kept so it stays there while up
/// (`RenderPriv`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderPriv {
    pub top: i32,
    pub height: i32,
    pub left: i32,
    pub width: i32,
    pub render_id: i32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Event {
    /// Milliseconds.
    pub start: i64,
    pub duration: i64,
    pub read_order: i32,
    pub layer: i32,
    pub style: usize,
    pub name: Vec<u8>,
    pub margin_l: i32,
    pub margin_r: i32,
    pub margin_v: i32,
    pub effect: Vec<u8>,
    pub text: Vec<u8>,
    pub render_priv: Option<RenderPriv>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrackType {
    #[default]
    Unknown,
    Ass,
    Ssa,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum YCbCrMatrix {
    #[default]
    Default,
    Unknown,
    None,
    Bt601Tv,
    Bt601Pc,
    Bt709Tv,
    Bt709Pc,
    Smpte240mTv,
    Smpte240mPc,
    FccTv,
    FccPc,
}

/// `ASS_Feature`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    IncompatibleExtensions = 0,
    BidiBrackets = 1,
    WholeTextLayout = 2,
    WrapUnicode = 3,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ParserState {
    #[default]
    Unknown,
    Info,
    Styles,
    Events,
    Fonts,
}

const SINFO_LANGUAGE: u32 = 1 << 0;
const SINFO_PLAYRESX: u32 = 1 << 1;
const SINFO_PLAYRESY: u32 = 1 << 2;
const SINFO_TIMER: u32 = 1 << 3;
const SINFO_WRAPSTYLE: u32 = 1 << 4;
const SINFO_SCALEDBORDER: u32 = 1 << 5;
const SINFO_COLOURMATRIX: u32 = 1 << 6;
const SINFO_KERNING: u32 = 1 << 7;
const SINFO_SCRIPTTYPE: u32 = 1 << 8;
const SINFO_LAYOUTRESX: u32 = 1 << 9;
const SINFO_LAYOUTRESY: u32 = 1 << 10;
const GENBY_FFMPEG: u32 = 1 << 14;

#[derive(Clone, Debug, Default)]
struct ParserPriv {
    state: ParserState,
    fontname: Option<Vec<u8>>,
    fontdata: Vec<u8>,
    read_orders: HashSet<i32>,
    check_readorder: bool,
    header_flags: u32,
    feature_flags: u32,
    prune_next_ts: i64,
}

/// A font a script embeds (`[Fonts]`), decoded.
#[derive(Clone, Debug, PartialEq)]
pub struct EmbeddedFont {
    pub name: Vec<u8>,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub styles: Vec<Style>,
    pub events: Vec<Event>,
    pub style_format: Option<Vec<u8>>,
    pub event_format: Option<Vec<u8>>,
    pub track_type: TrackType,
    pub play_res_x: i32,
    pub play_res_y: i32,
    pub timer: f64,
    pub wrap_style: i32,
    pub scaled_border_and_shadow: bool,
    pub kerning: bool,
    pub language: Option<Vec<u8>>,
    pub ycbcr_matrix: YCbCrMatrix,
    pub default_style: usize,
    pub layout_res_x: i32,
    pub layout_res_y: i32,
    /// `[Fonts]` of the script, when fonts are extracted.
    pub fonts: Vec<EmbeddedFont>,
    /// `ass_set_extract_fonts`.
    pub extract_fonts: bool,
    /// `ass_set_style_overrides`: `[Style.]Field=Value` entries.
    pub style_overrides: Vec<Vec<u8>>,
    font_bytes: usize,
    parser: ParserPriv,
}

impl Default for Track {
    fn default() -> Self {
        Track::new()
    }
}

/// A `,`-separated token at `*at` (`next_token`), leading spaces skipped,
/// trailing ones too when `rtrim`. `None` at the end of `s`.
fn next_token<'a>(s: &'a [u8], at: &mut usize, rtrim: bool) -> Option<&'a [u8]> {
    let start = skip_spaces(s, *at);
    if start >= s.len() {
        *at = start;
        return None;
    }
    let mut end = start;
    while end < s.len() && s[end] != b',' {
        end += 1;
    }
    *at = if end < s.len() { end + 1 } else { end };
    let end = if rtrim { rskip_spaces(s, end, start) } else { end };
    Some(&s[start..end])
}

/// `string2timecode`: `h:mm:ss.cc` in milliseconds (0 when malformed).
fn string2timecode(s: &[u8]) -> i64 {
    // sscanf("%d:%d:%d.%d"): each number skips white space and takes a
    // sign; the separators must follow at once.
    let mut values = [0i64; 4];
    let mut p = 0;
    for (i, sep) in [Some(b':'), Some(b':'), Some(b'.'), None].into_iter().enumerate() {
        let (v, n) = strtoll(&s[p..], 10);
        if n == 0 {
            return 0;
        }
        values[i] = v.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
        p += n;
        if let Some(sep) = sep {
            if s.get(p) != Some(&sep) {
                return 0;
            }
            p += 1;
        }
    }
    let [h, m, sec, cs] = values;
    ((h.wrapping_mul(60).wrapping_add(m)).wrapping_mul(60).wrapping_add(sec)).wrapping_mul(1000).wrapping_add(cs.wrapping_mul(10))
}

fn parse_ycbcr_matrix(s: &[u8]) -> YCbCrMatrix {
    let start = skip_spaces(s, 0);
    if start >= s.len() {
        return YCbCrMatrix::Default;
    }
    let end = rskip_spaces(s, s.len(), start);
    let value = &s[start..end.min(start + 15)];
    match value.to_ascii_lowercase().as_slice() {
        b"none" => YCbCrMatrix::None,
        b"tv.601" => YCbCrMatrix::Bt601Tv,
        b"pc.601" => YCbCrMatrix::Bt601Pc,
        b"tv.709" => YCbCrMatrix::Bt709Tv,
        b"pc.709" => YCbCrMatrix::Bt709Pc,
        b"tv.240m" => YCbCrMatrix::Smpte240mTv,
        b"pc.240m" => YCbCrMatrix::Smpte240mPc,
        b"tv.fcc" => YCbCrMatrix::FccTv,
        b"pc.fcc" => YCbCrMatrix::FccPc,
        _ => YCbCrMatrix::Unknown,
    }
}

fn atof(s: &[u8]) -> f64 {
    strtod(s).0
}

/// C `atoi`.
fn atoi(s: &[u8]) -> i32 {
    strtoll(s, 10).0 as i32
}

fn set_style_alpha(style: &mut Style, front: i32, back: i32) {
    let front = front.clamp(0, 0xff) as u32;
    let back = back.clamp(0, 0xff) as u32;
    style.primary_colour = (style.primary_colour & 0xffff_ff00) | front;
    style.secondary_colour = (style.secondary_colour & 0xffff_ff00) | front;
    style.outline_colour = (style.outline_colour & 0xffff_ff00) | front;
    style.back_colour = (style.back_colour & 0xffff_ff00) | back;
}

/// `format_line_compare`: the same fields (`Actor` reads as `Name`).
fn format_line_compare(fmt1: &[u8], fmt2: &[u8]) -> bool {
    let (mut a, mut b) = (0, 0);
    loop {
        a = skip_spaces(fmt1, a);
        b = skip_spaces(fmt2, b);
        if a >= fmt1.len() || b >= fmt2.len() {
            break;
        }
        let (Some(t1), Some(t2)) = (next_token(fmt1, &mut a, true), next_token(fmt2, &mut b, true)) else { break };
        let alias = |t: &'static [u8], tok: &[u8]| -> bool { tok == t };
        let t1: &[u8] = if alias(b"Actor", t1) { b"Name" } else { t1 };
        let t2: &[u8] = if alias(b"Actor", t2) { b"Name" } else { t2 };
        if t1.len() != t2.len() || !t1.eq_ignore_ascii_case(t2) {
            return false;
        }
    }
    fmt1.get(a) == fmt2.get(b)
}

impl Track {
    /// `ass_new_track`: no styles but the libass default, no events.
    pub fn new() -> Track {
        Track {
            styles: vec![Style::libass_default()],
            events: Vec::new(),
            style_format: None,
            event_format: None,
            track_type: TrackType::Unknown,
            play_res_x: 0,
            play_res_y: 0,
            timer: 0.0,
            wrap_style: 0,
            scaled_border_and_shadow: false,
            kerning: false,
            language: None,
            ycbcr_matrix: YCbCrMatrix::Default,
            default_style: 0,
            layout_res_x: 0,
            layout_res_y: 0,
            fonts: Vec::new(),
            extract_fonts: true,
            style_overrides: Vec::new(),
            font_bytes: 0,
            parser: ParserPriv { check_readorder: true, prune_next_ts: i64::MAX, ..ParserPriv::default() },
        }
    }

    /// `ass_read_memory`: a whole script; `None` when it is not one (no
    /// `[V4 Styles]`/`[V4+ Styles]` section nor `ScriptType`).
    pub fn parse(data: &[u8]) -> Option<Track> {
        Track::parse_with(data, Vec::new())
    }

    /// [`Track::parse`] with style overrides applied (`force_style`).
    pub fn parse_with(data: &[u8], style_overrides: Vec<Vec<u8>>) -> Option<Track> {
        let mut track = Track::new();
        track.style_overrides = style_overrides;
        track.process_text(data);
        for (i, event) in track.events.iter_mut().enumerate() {
            event.read_order = i as i32;
        }
        if track.track_type == TrackType::Unknown {
            return None;
        }
        track.process_force_style();
        Some(track)
    }

    pub fn feature(&self, feature: Feature) -> bool {
        self.parser.feature_flags & (1 << feature as u32) != 0
    }

    /// `ass_track_set_feature`.
    pub fn set_feature(&mut self, feature: Feature, enable: bool) {
        let supported = (1 << Feature::BidiBrackets as u32) | (1 << Feature::WrapUnicode as u32) | (1 << Feature::WholeTextLayout as u32);
        let requested = match feature {
            Feature::IncompatibleExtensions => supported,
            f => 1 << f as u32,
        };
        if enable {
            self.parser.feature_flags |= requested;
        } else {
            self.parser.feature_flags &= !requested;
        }
    }

    /// `ass_lookup_style`.
    pub fn lookup_style(&self, name: &[u8]) -> usize {
        let mut name = name;
        while name.first() == Some(&b'*') {
            name = &name[1..];
        }
        let name: &[u8] = if name.eq_ignore_ascii_case(b"Default") { b"Default" } else { name };
        self.styles.iter().rposition(|s| s.name == name).unwrap_or(self.default_style)
    }

    fn process_event_tail(&self, event: &mut Event, s: &[u8], n_ignored: usize) -> bool {
        let format = self.event_format.clone().unwrap_or_default();
        let mut q = 0;
        for _ in 0..n_ignored {
            if next_token(&format, &mut q, false).is_none() {
                break;
            }
        }
        let mut p = 0;
        loop {
            let Some(tname) = next_token(&format, &mut q, true) else { return false };
            if tname.eq_ignore_ascii_case(b"Text") {
                let mut text = s[p.min(s.len())..].to_vec();
                while matches!(text.last(), Some(b'\r' | b'\t' | b' ')) {
                    text.pop();
                }
                event.text = text;
                event.duration = event.duration.wrapping_sub(event.start);
                return true;
            }
            let Some(token) = next_token(s, &mut p, false) else { return false };
            let tname: &[u8] = if tname.eq_ignore_ascii_case(b"End") {
                b"Duration"
            } else if tname.eq_ignore_ascii_case(b"Actor") {
                b"Name"
            } else {
                tname
            };
            match tname.to_ascii_lowercase().as_slice() {
                b"layer" => event.layer = parse_int_header(token),
                b"style" => event.style = self.lookup_style(token),
                b"name" => event.name = token.to_vec(),
                b"effect" => event.effect = token.to_vec(),
                b"marginl" => event.margin_l = parse_int_header(token),
                b"marginr" => event.margin_r = parse_int_header(token),
                b"marginv" => event.margin_v = parse_int_header(token),
                b"start" => event.start = string2timecode(token),
                b"duration" => event.duration = string2timecode(token),
                _ => {}
            }
        }
    }

    /// `ass_process_force_style`.
    pub fn process_force_style(&mut self) {
        let overrides = std::mem::take(&mut self.style_overrides);
        for entry in &overrides {
            let Some(eq) = entry.iter().rposition(|&b| b == b'=') else { continue };
            let (field, token) = (&entry[..eq], &entry[eq + 1..]);
            if field.eq_ignore_ascii_case(b"PlayResX") {
                self.play_res_x = parse_int_header(token);
            } else if field.eq_ignore_ascii_case(b"PlayResY") {
                self.play_res_y = parse_int_header(token);
            } else if field.eq_ignore_ascii_case(b"LayoutResX") {
                self.layout_res_x = parse_int_header(token);
            } else if field.eq_ignore_ascii_case(b"LayoutResY") {
                self.layout_res_y = parse_int_header(token);
            } else if field.eq_ignore_ascii_case(b"Timer") {
                self.timer = atof(token);
            } else if field.eq_ignore_ascii_case(b"WrapStyle") {
                self.wrap_style = parse_int_header(token);
            } else if field.eq_ignore_ascii_case(b"ScaledBorderAndShadow") {
                self.scaled_border_and_shadow = parse_bool(token);
            } else if field.eq_ignore_ascii_case(b"Kerning") {
                self.kerning = parse_bool(token);
            } else if field.eq_ignore_ascii_case(b"YCbCr Matrix") {
                self.ycbcr_matrix = parse_ycbcr_matrix(token);
            }
            let (style, tname) = match field.iter().rposition(|&b| b == b'.') {
                Some(dot) => (Some(&field[..dot]), &field[dot + 1..]),
                None => (None, field),
            };
            for target in self.styles.iter_mut() {
                if style.is_some_and(|s| !target.name.eq_ignore_ascii_case(s)) {
                    continue;
                }
                match tname.to_ascii_lowercase().as_slice() {
                    b"fontname" => target.font_name = token.to_vec(),
                    b"primarycolour" => target.primary_colour = parse_color_header(token),
                    b"secondarycolour" => target.secondary_colour = parse_color_header(token),
                    b"outlinecolour" => target.outline_colour = parse_color_header(token),
                    b"backcolour" => target.back_colour = parse_color_header(token),
                    b"alphalevel" => {
                        let alpha = parse_int_header(token);
                        set_style_alpha(target, alpha, alpha);
                    }
                    b"fontsize" => target.font_size = atof(token),
                    b"bold" => target.bold = parse_int_header(token),
                    b"italic" => target.italic = parse_int_header(token),
                    b"underline" => target.underline = parse_int_header(token),
                    b"strikeout" => target.strike_out = parse_int_header(token),
                    b"spacing" => target.spacing = atof(token),
                    b"angle" => target.angle = atof(token),
                    b"borderstyle" => target.border_style = parse_int_header(token),
                    b"alignment" => target.alignment = parse_int_header(token),
                    b"justify" => target.justify = parse_int_header(token),
                    b"marginl" => target.margin_l = parse_int_header(token),
                    b"marginr" => target.margin_r = parse_int_header(token),
                    b"marginv" => target.margin_v = parse_int_header(token),
                    b"encoding" => target.encoding = parse_int_header(token),
                    b"scalex" => target.scale_x = atof(token),
                    b"scaley" => target.scale_y = atof(token),
                    b"outline" => target.outline = atof(token),
                    b"shadow" => target.shadow = atof(token),
                    b"blur" => target.blur = atof(token),
                    _ => {}
                }
            }
        }
        self.style_overrides = overrides;
    }

    fn process_style(&mut self, s: &[u8]) {
        if self.style_format.is_none() {
            self.style_format = Some(if self.track_type == TrackType::Ssa { SSA_STYLE_FORMAT } else { ASS_STYLE_FORMAT }.to_vec());
        }
        if self.styles.len() >= MAX_STYLES {
            return;
        }
        let format = self.style_format.clone().unwrap_or_default();
        let mut style = Style { scale_x: 100.0, scale_y: 100.0, ..Style::default() };
        let mut ssa_alpha = 0;
        let mut font_name_set = false;
        let (mut q, mut p) = (0, 0);
        loop {
            let Some(tname) = next_token(&format, &mut q, true) else { break };
            let Some(token) = next_token(s, &mut p, false) else { break };
            match tname.to_ascii_lowercase().as_slice() {
                b"name" => {
                    let mut t = token;
                    while t.first() == Some(&b'*') {
                        t = &t[1..];
                    }
                    style.name = t.to_vec();
                }
                b"fontname" => {
                    style.font_name = token.to_vec();
                    font_name_set = true;
                }
                b"primarycolour" => style.primary_colour = parse_color_header(token),
                b"secondarycolour" => style.secondary_colour = parse_color_header(token),
                b"outlinecolour" => style.outline_colour = parse_color_header(token),
                b"backcolour" => {
                    style.back_colour = parse_color_header(token);
                    // SSA uses BackColour for both outline and shadow.
                    if self.track_type == TrackType::Ssa {
                        style.outline_colour = style.back_colour;
                    }
                }
                b"alphalevel" => ssa_alpha = parse_int_header(token),
                b"fontsize" => style.font_size = atof(token),
                b"bold" => style.bold = parse_int_header(token),
                b"italic" => style.italic = parse_int_header(token),
                b"underline" => style.underline = parse_int_header(token),
                b"strikeout" => style.strike_out = parse_int_header(token),
                b"spacing" => style.spacing = atof(token),
                b"angle" => style.angle = atof(token),
                b"borderstyle" => style.border_style = parse_int_header(token),
                b"alignment" => {
                    style.alignment = parse_int_header(token);
                    if self.track_type == TrackType::Ass {
                        style.alignment = numpad2align(style.alignment);
                    } else if style.alignment == 8 {
                        // VSFilter compatibility.
                        style.alignment = 3;
                    } else if style.alignment == 4 {
                        style.alignment = 11;
                    }
                }
                b"marginl" => style.margin_l = parse_int_header(token),
                b"marginr" => style.margin_r = parse_int_header(token),
                b"marginv" => style.margin_v = parse_int_header(token),
                b"encoding" => style.encoding = parse_int_header(token),
                b"scalex" => style.scale_x = atof(token),
                b"scaley" => style.scale_y = atof(token),
                b"outline" => style.outline = atof(token),
                b"shadow" => style.shadow = atof(token),
                _ => {}
            }
        }
        // VSFilter: BackColour's alpha is always 0x80 in SSA.
        if self.track_type == TrackType::Ssa {
            set_style_alpha(&mut style, ssa_alpha, 0x80);
        }
        style.scale_x = style.scale_x.max(0.0) / 100.0;
        style.scale_y = style.scale_y.max(0.0) / 100.0;
        style.spacing = style.spacing.max(0.0);
        style.outline = style.outline.max(0.0);
        style.shadow = style.shadow.max(0.0);
        style.bold = i32::from(style.bold != 0);
        style.italic = i32::from(style.italic != 0);
        style.underline = i32::from(style.underline != 0);
        style.strike_out = i32::from(style.strike_out != 0);
        if style.name.is_empty() {
            style.name = b"Default".to_vec();
        }
        if !font_name_set {
            style.font_name = b"Arial".to_vec();
        }
        let is_default = style.name == b"Default";
        self.styles.push(style);
        if is_default {
            self.default_style = self.styles.len() - 1;
        }
    }

    fn custom_format_line_compatibility(&mut self, fmt: &[u8], std: &[u8]) {
        if self.parser.header_flags & SINFO_SCALEDBORDER == 0 && !format_line_compare(fmt, std) {
            self.scaled_border_and_shadow = true;
        }
    }

    fn process_styles_line(&mut self, s: &[u8]) {
        if let Some(rest) = s.strip_prefix(b"Format:") {
            let p = &rest[skip_spaces(rest, 0)..];
            self.style_format = Some(p.to_vec());
            let std = if self.track_type == TrackType::Ass { ASS_STYLE_FORMAT } else { SSA_STYLE_FORMAT };
            self.custom_format_line_compatibility(p, std);
        } else if let Some(rest) = s.strip_prefix(b"Style:") {
            let p = &rest[skip_spaces(rest, 0)..];
            self.process_style(p);
        }
    }

    fn info_flag(&mut self, flag: u32) {
        self.parser.header_flags |= flag;
    }

    fn process_info_line(&mut self, s: &[u8]) {
        if let Some(v) = s.strip_prefix(b"PlayResX:") {
            self.info_flag(SINFO_PLAYRESX);
            self.play_res_x = parse_int_header(v);
        } else if let Some(v) = s.strip_prefix(b"PlayResY:") {
            self.info_flag(SINFO_PLAYRESY);
            self.play_res_y = parse_int_header(v);
        } else if let Some(v) = s.strip_prefix(b"LayoutResX:") {
            self.info_flag(SINFO_LAYOUTRESX);
            self.layout_res_x = parse_int_header(v);
        } else if let Some(v) = s.strip_prefix(b"LayoutResY:") {
            self.info_flag(SINFO_LAYOUTRESY);
            self.layout_res_y = parse_int_header(v);
        } else if let Some(v) = s.strip_prefix(b"Timer:") {
            self.info_flag(SINFO_TIMER);
            self.timer = atof(v);
        } else if let Some(v) = s.strip_prefix(b"WrapStyle:") {
            self.info_flag(SINFO_WRAPSTYLE);
            self.wrap_style = parse_int_header(v);
        } else if let Some(v) = s.strip_prefix(b"ScaledBorderAndShadow:") {
            self.info_flag(SINFO_SCALEDBORDER);
            self.scaled_border_and_shadow = parse_bool(v);
        } else if let Some(v) = s.strip_prefix(b"Kerning:") {
            self.info_flag(SINFO_KERNING);
            self.kerning = parse_bool(v);
        } else if let Some(v) = s.strip_prefix(b"YCbCr Matrix:") {
            self.info_flag(SINFO_COLOURMATRIX);
            self.ycbcr_matrix = parse_ycbcr_matrix(v);
        } else if let Some(v) = s.strip_prefix(b"Language:") {
            self.info_flag(SINFO_LANGUAGE);
            let start = v.iter().position(|&c| !is_space(c)).unwrap_or(v.len());
            self.language = Some(v[start..].iter().take(2).copied().collect());
        } else if let Some(v) = s.strip_prefix(b"ScriptType:") {
            self.info_flag(SINFO_SCRIPTTYPE);
            // VSFilter: no check for the leading `v`; the value read
            // backwards from its last non-space.
            let end = rskip_spaces(v, v.len(), 0);
            let mut len = end;
            if len < 4 {
                return;
            }
            let mut ver = TrackType::Ssa;
            let mut p = end;
            if v[p - 1] == b'+' {
                ver = TrackType::Ass;
                len -= 1;
                p -= 1;
            }
            if len >= 4 && &v[p - 4..p] == b"4.00" {
                self.track_type = ver;
            }
        } else if let Some(v) = s.strip_prefix(b"; Script generated by ") {
            if v.starts_with(b"FFmpeg/Lavc") {
                self.parser.header_flags |= GENBY_FFMPEG;
            }
        }
    }

    fn event_format_fallback(&mut self) {
        self.parser.state = ParserState::Events;
        self.event_format = Some(if self.track_type == TrackType::Ssa { SSA_EVENT_FORMAT } else { ASS_EVENT_FORMAT }.to_vec());
    }

    /// `detect_legacy_conv_subs`: FFmpeg's 2014–2020 SubRip conversions,
    /// which expected `ScaledBorderAndShadow: yes`.
    fn detect_legacy_conv_subs(&self) -> bool {
        self.parser.header_flags == (SINFO_SCRIPTTYPE | SINFO_PLAYRESX | SINFO_PLAYRESY | GENBY_FFMPEG)
            && self.styles.len() == 2
            && self.styles[1].name.starts_with(b"Default")
    }

    fn process_events_line(&mut self, s: &[u8]) {
        if let Some(rest) = s.strip_prefix(b"Format:") {
            let p = &rest[skip_spaces(rest, 0)..];
            self.event_format = Some(p.to_vec());
            let std = if self.track_type == TrackType::Ass { ASS_EVENT_FORMAT } else { SSA_EVENT_FORMAT };
            self.custom_format_line_compatibility(p, std);
            if self.detect_legacy_conv_subs() {
                self.scaled_border_and_shadow = true;
            }
        } else if let Some(rest) = s.strip_prefix(b"Dialogue:") {
            if self.event_format.is_none() {
                self.event_format_fallback();
            }
            if self.events.len() >= MAX_EVENTS {
                return;
            }
            let p = &rest[skip_spaces(rest, 0)..];
            let mut event = Event::default();
            if self.process_event_tail(&mut event, p, 0) {
                self.update_prune_ts(event.start.wrapping_add(event.duration));
                self.events.push(event);
            }
        }
    }

    fn update_prune_ts(&mut self, ts: i64) {
        self.parser.prune_next_ts = self.parser.prune_next_ts.min(ts);
    }

    fn decode_font(&mut self) {
        let name = self.parser.fontname.take().unwrap_or_default();
        let data = std::mem::take(&mut self.parser.fontdata);
        if data.len() % 4 == 1 {
            return;
        }
        // UUEncode-like: each byte carries 6 bits, offset by 33.
        let mut out = Vec::with_capacity(data.len() / 4 * 3 + 2);
        for chunk in data.chunks(4) {
            let mut value: u32 = 0;
            for (i, &c) in chunk.iter().enumerate() {
                value |= (u32::from(c.wrapping_sub(33)) & 63) << (6 * (3 - i));
            }
            out.push((value >> 16) as u8);
            if chunk.len() >= 3 {
                out.push((value >> 8) as u8);
            }
            if chunk.len() >= 4 {
                out.push(value as u8);
            }
        }
        if self.extract_fonts && self.font_bytes + out.len() <= MAX_FONT_BYTES {
            self.font_bytes += out.len();
            self.fonts.push(EmbeddedFont { name, data: out });
        }
    }

    fn process_fonts_line(&mut self, s: &[u8]) {
        if let Some(rest) = s.strip_prefix(b"fontname:") {
            if self.parser.fontname.is_some() {
                self.decode_font();
            }
            self.parser.fontname = Some(rest[skip_spaces(rest, 0)..].to_vec());
            return;
        }
        if self.parser.fontname.is_none() {
            return;
        }
        if self.parser.fontdata.len() + s.len() > MAX_FONT_BYTES / 3 * 4 + 4 {
            self.parser.fontname = None;
            self.parser.fontdata = Vec::new();
            return;
        }
        self.parser.fontdata.extend_from_slice(s);
    }

    fn process_line(&mut self, s: &[u8]) {
        let s = &s[skip_spaces(s, 0)..];
        if starts_with_ignore_case(s, b"[Script Info]") {
            self.parser.state = ParserState::Info;
        } else if starts_with_ignore_case(s, b"[V4 Styles]") {
            self.parser.state = ParserState::Styles;
            self.track_type = TrackType::Ssa;
        } else if starts_with_ignore_case(s, b"[V4+ Styles]") {
            self.parser.state = ParserState::Styles;
            self.track_type = TrackType::Ass;
        } else if starts_with_ignore_case(s, b"[Events]") {
            self.parser.state = ParserState::Events;
        } else if starts_with_ignore_case(s, b"[Fonts]") {
            self.parser.state = ParserState::Fonts;
        } else {
            match self.parser.state {
                ParserState::Info => self.process_info_line(s),
                ParserState::Styles => self.process_styles_line(s),
                ParserState::Events => self.process_events_line(s),
                ParserState::Fonts => self.process_fonts_line(s),
                ParserState::Unknown => {}
            }
        }
    }

    /// `process_text`: lines split at CR/LF; a UTF-8 BOM at a line start is
    /// skipped. Text up to a NUL, as in C.
    fn process_text(&mut self, data: &[u8]) {
        let data = data.split(|&b| b == 0).next().unwrap_or_default();
        let mut p = 0;
        loop {
            loop {
                if p < data.len() && (data[p] == b'\r' || data[p] == b'\n') {
                    p += 1;
                } else if data[p.min(data.len())..].starts_with(b"\xef\xbb\xbf") {
                    p += 3;
                } else {
                    break;
                }
            }
            let mut q = p;
            while q < data.len() && data[q] != b'\r' && data[q] != b'\n' {
                q += 1;
            }
            if q == p {
                break;
            }
            self.process_line(&data[p..q]);
            if q >= data.len() {
                break;
            }
            p = q + 1;
        }
        // No explicit end-of-font marker in SSA/ASS.
        if self.parser.fontname.is_some() {
            self.decode_font();
        }
    }

    /// `ass_process_data`.
    pub fn process_data(&mut self, data: &[u8]) {
        self.process_text(data);
    }

    /// `ass_process_codec_private`: a Matroska ASS track's header.
    pub fn process_codec_private(&mut self, data: &[u8]) {
        self.process_text(data);
        // Probably an mkv produced by ancient mkvtoolnix: no [Events] and
        // Format: headers.
        if self.event_format.is_none() {
            self.event_format_fallback();
        }
        self.process_force_style();
    }

    /// `ass_process_chunk`: one Matroska ASS event (`ReadOrder, Layer,
    /// Style, Name, MarginL, MarginR, MarginV, Effect, Text`) shown from
    /// `timecode` for `duration` milliseconds. A ReadOrder belonging to an
    /// admitted event is a duplicate until that event is pruned or flushed.
    pub fn process_chunk(&mut self, data: &[u8], timecode: i64, duration: i64) {
        if self.event_format.is_none() || self.events.len() >= MAX_EVENTS {
            return;
        }
        let s = data.split(|&b| b == 0).next().unwrap_or_default();
        let mut p = 0;
        let Some(token) = next_token(s, &mut p, false) else { return };
        let read_order = atoi(token);
        if self.parser.check_readorder && self.parser.read_orders.contains(&read_order) {
            return;
        }
        let Some(token) = next_token(s, &mut p, false) else { return };
        let mut event = Event { read_order, layer: parse_int_header(token), ..Event::default() };
        if !self.process_event_tail(&mut event, &s[p..], 3) {
            return;
        }
        // Rejected chunks own no event for pruning to retire.
        if self.parser.check_readorder {
            self.parser.read_orders.insert(read_order);
        }
        event.start = timecode;
        event.duration = duration;
        self.update_prune_ts(timecode.wrapping_add(duration));
        self.events.push(event);
    }

    /// `ass_set_check_readorder`.
    pub fn set_check_readorder(&mut self, check: bool) {
        self.parser.check_readorder = check;
    }

    /// `ass_flush_events`.
    pub fn flush_events(&mut self) {
        self.events.clear();
        self.parser.read_orders.clear();
    }

    /// `ass_prune_events`: drops events that ended before `deadline`.
    pub fn prune_events(&mut self, deadline: i64) {
        if deadline < self.parser.prune_next_ts {
            return;
        }
        let check = self.parser.check_readorder;
        let mut next = i64::MAX;
        let read_orders = &mut self.parser.read_orders;
        self.events.retain(|e| {
            let end = e.start.wrapping_add(e.duration);
            if end < deadline {
                if check {
                    read_orders.remove(&e.read_order);
                }
                false
            } else {
                next = next.min(end);
                true
            }
        });
        self.parser.prune_next_ts = next;
    }

    /// `ass_lazy_track_init`: a missing PlayResX/PlayResY from the other,
    /// or 384x288.
    pub fn lazy_init(&mut self) {
        if self.play_res_x > 0 && self.play_res_y > 0 {
            return;
        }
        if self.play_res_x <= 0 && self.play_res_y <= 0 {
            self.play_res_x = 384;
            self.play_res_y = 288;
        } else if self.play_res_y <= 0 && self.play_res_x == 1280 {
            self.play_res_y = 1024;
        } else if self.play_res_y <= 0 {
            let x = (self.play_res_x as u32).wrapping_sub(1);
            self.play_res_y = (x.wrapping_sub(x / 4) as i32).max(1);
        } else if self.play_res_x <= 0 && self.play_res_y == 1024 {
            self.play_res_x = 1280;
        } else if self.play_res_x <= 0 {
            let y = self.play_res_y as u32;
            self.play_res_x = (y + y / 3).min(i32::MAX as u32) as i32;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCRIPT: &[u8] = b"\xef\xbb\xbf[Script Info]\r\nScriptType: v4.00+\r\nPlayResX: 1280\r\nPlayResY: 720\r\nScaledBorderAndShadow: yes\r\n\r\n[V4+ Styles]\r\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\r\nStyle: Sign,DejaVu Sans,48,&H00F9FDFB,&H00003FFF,&H0A093346,&HDC0A0E10,-1,0,0,0,99,100,0,0,1,2.5,1,8,110,110,40,0\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\nDialogue: 1,0:00:01.50,0:00:03.00,Sign,,0,0,0,,{\\pos(10,20)}Hello, world \r\nComment: 0,0:00:00.00,0:00:01.00,Sign,,0,0,0,,no\r\n";

    #[test]
    fn scripts_parse_as_libass_parses_them() {
        let track = Track::parse(SCRIPT).unwrap();
        assert_eq!((track.track_type, track.play_res_x, track.play_res_y, track.scaled_border_and_shadow), (TrackType::Ass, 1280, 720, true));
        assert_eq!(track.styles.len(), 2);
        let sign = &track.styles[1];
        assert_eq!((sign.name.as_slice(), sign.font_name.as_slice(), sign.font_size), (&b"Sign"[..], &b"DejaVu Sans"[..], 48.0));
        assert_eq!(sign.primary_colour, 0xFBFDF900);
        assert_eq!((sign.bold, sign.scale_x, sign.outline, sign.alignment), (1, 0.99, 2.5, 6));
        assert_eq!(track.events.len(), 1);
        let e = &track.events[0];
        assert_eq!((e.start, e.duration, e.layer, e.style), (1500, 1500, 1, 1));
        assert_eq!(e.text, b"{\\pos(10,20)}Hello, world");
    }

    #[test]
    fn matroska_chunks_add_events_once() {
        let mut track = Track::new();
        let header = b"[Script Info]\nScriptType: v4.00+\n\n[V4+ Styles]\nStyle: Default,Arial,20,&Hffffff,&Hffffff,&H0,&H0,0,0,0,0,100,100,0,0,1,1,0,2,10,10,10,0\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
        track.process_codec_private(header);
        track.process_chunk(b"0,0,Default,,0,0,0,,first", 1000, 500);
        track.process_chunk(b"0,0,Default,,0,0,0,,again", 2000, 500);
        track.process_chunk(b"1,2,Default,,0,0,0,,second", 2000, 500);
        let texts: Vec<_> = track.events.iter().map(|e| (e.text.clone(), e.layer, e.start, e.duration)).collect();
        assert_eq!(texts, [(b"first".to_vec(), 0, 1000, 500), (b"second".to_vec(), 2, 2000, 500)]);
        track.prune_events(1600);
        assert_eq!(track.events.len(), 1);
        track.process_chunk(b"0,0,Default,,0,0,0,,first again", 3000, 500);
        assert_eq!(track.events.len(), 2);
    }

    #[test]
    fn read_orders_follow_admitted_chunk_lifetimes() {
        let mut track = Track::new();
        track.process_codec_private(b"[Script Info]\nScriptType: v4.00+\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n");
        track.process_chunk(b"10,0,Default,,0,0,0,,first", 1000, 500);
        track.process_chunk(b"11,0,Default,,0,0,0,,live", 1200, 2000);

        // Reject one missing layer and one incomplete event tail.
        track.process_chunk(b"20", 1000, 500);
        track.process_chunk(b"21,0,Default", 1000, 500);
        assert_eq!(track.parser.read_orders, HashSet::from([10, 11]),
            "rejected chunks must not reserve ReadOrder values");
        track.process_chunk(b"20,0,Default,,0,0,0,,layer retry", 1000, 500);
        track.process_chunk(b"21,0,Default,,0,0,0,,tail retry", 1000, 500);
        track.process_chunk(b"10,0,Default,,0,0,0,,duplicate", 2000, 900);
        let events: Vec<_> = track.events.iter()
            .map(|e| (e.read_order, e.text.as_slice(), e.start, e.duration)).collect();
        assert_eq!(events, [
            (10, b"first".as_slice(), 1000, 500),
            (11, b"live".as_slice(), 1200, 2000),
            (20, b"layer retry".as_slice(), 1000, 500),
            (21, b"tail retry".as_slice(), 1000, 500),
        ]);

        track.prune_events(1500);
        assert_eq!(track.parser.read_orders, HashSet::from([10, 11, 20, 21]),
            "pruning retains events ending at the deadline");
        track.prune_events(1501);
        assert_eq!(track.parser.read_orders, HashSet::from([11]));
        track.process_chunk(b"10,0,Default,,0,0,0,,reused", 2000, 500);
        track.process_chunk(b"11,0,Default,,0,0,0,,duplicate live", 2000, 500);
        let events: Vec<_> = track.events.iter()
            .map(|e| (e.read_order, e.text.as_slice(), e.start, e.duration)).collect();
        assert_eq!(events, [
            (11, b"live".as_slice(), 1200, 2000),
            (10, b"reused".as_slice(), 2000, 500),
        ]);

        // Player uses this reset after a seek; both live IDs may replay.
        track.flush_events();
        assert!(track.events.is_empty() && track.parser.read_orders.is_empty());
        track.process_chunk(b"10,0,Default,,0,0,0,,first", 1000, 500);
        track.process_chunk(b"11,0,Default,,0,0,0,,live", 1200, 2000);
        assert_eq!(track.parser.read_orders, HashSet::from([10, 11]));
        let events: Vec<_> = track.events.iter()
            .map(|e| (e.read_order, e.text.as_slice(), e.start, e.duration)).collect();
        assert_eq!(events, [
            (10, b"first".as_slice(), 1000, 500),
            (11, b"live".as_slice(), 1200, 2000),
        ]);
    }

    #[test]
    fn ssa_styles_take_vsfilter_quirks() {
        let script = b"[Script Info]\nScriptType: v4.00\n\n[V4 Styles]\nFormat: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, TertiaryColour, BackColour, Bold, Italic, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, AlphaLevel, Encoding\nStyle: Default,Verdana,28,16777215,65535,0,0,-1,0,1,2,0,6,30,30,28,0,0\n";
        let track = Track::parse(script).unwrap();
        let s = &track.styles[1];
        assert_eq!(track.track_type, TrackType::Ssa);
        assert_eq!(s.primary_colour, 0xffff_ff00);
        // BackColour's alpha forced to 0x80; outline colour from BackColour.
        assert_eq!((s.back_colour & 0xff, s.outline_colour), (0x80, 0));
        assert_eq!(s.alignment, 6);
    }

    #[test]
    fn embedded_fonts_decode() {
        // "AB" encodes as two 6-bit groups each: 'A'=0x41.
        let mut track = Track::new();
        let encoded: Vec<u8> = {
            let bytes = [0x41u8, 0x42, 0x43];
            let v = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
            (0..4).map(|i| ((v >> (18 - 6 * i)) & 63) as u8 + 33).collect()
        };
        let mut script = b"[Script Info]\nScriptType: v4.00+\n[Fonts]\nfontname: a_0.ttf\n".to_vec();
        script.extend_from_slice(&encoded);
        script.push(b'\n');
        track.process_data(&script);
        assert_eq!(track.fonts, [EmbeddedFont { name: b"a_0.ttf".to_vec(), data: b"ABC".to_vec() }]);
    }
}

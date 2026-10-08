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
// Adapted from libass 0.17.5 (4a05d81), ass_parse.c and ass_render.c.
// Bounded byte parsing; Rust spans replace per-character render state.

use std::ops::Range;
use crate::shaper::TextStyle;
use crate::track::{Event, Style, Track};
use crate::utils::{numpad2align, strtod, strtoll};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum KaraokeKind { Step, Sweep, Outline }

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Karaoke {
    pub group: u32,
    pub kind: KaraokeKind,
    pub start: f64,
    pub end: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pen {
    pub text: TextStyle,
    pub colours: [u32; 4],
    pub border: [f64; 2],
    pub shadow: [f64; 2],
    pub rotation: [f64; 3],
    pub shear: [f64; 2],
    pub blur: f64,
    pub be: i32,
    pub border_style: i32,
    pub encoding: i32,
    pub fade: f64,
    pub karaoke: Option<Karaoke>,
}

impl Pen {
    fn from_style(style: &Style) -> Self {
        Self {
            text: TextStyle { family: String::from_utf8_lossy(&style.font_name).into_owned(), size: style.font_size,
                weight: weight(style.bold), italic: style.italic != 0, underline: style.underline != 0, strike: style.strike_out != 0,
                spacing: style.spacing, scale_x: style.scale_x, scale_y: style.scale_y, ..TextStyle::default() },
            colours: [style.primary_colour, style.secondary_colour, style.outline_colour, style.back_colour],
            border: [style.outline; 2], shadow: [style.shadow; 2], rotation: [0.0, 0.0, style.angle], shear: [0.0; 2],
            blur: style.blur, be: 0, border_style: style.border_style, encoding: style.encoding, fade: 0.0, karaoke: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Drawing {
    pub text: Vec<u8>,
    pub scale: i32,
    pub baseline: f64,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub range: Range<usize>,
    pub pen: Pen,
    pub drawing: Option<Drawing>,
}

#[derive(Clone, Debug)]
pub enum Clip {
    Rect { bounds: [f64; 4], inverse: bool },
    Vector { text: Vec<u8>, scale: i32, inverse: bool },
}

#[derive(Clone, Debug)]
pub struct Parsed {
    pub text: String,
    pub runs: Vec<Run>,
    pub alignment: i32,
    pub position: Option<[f64; 2]>,
    pub origin: Option<[f64; 2]>,
    pub clip: Option<Clip>,
    pub wrap: i32,
    pub animated: bool,
    pub collisions: bool,
}

struct State<'a> {
    track: &'a Track,
    event: &'a Event,
    style: usize,
    now: f64,
    parsed: Parsed,
    pen: Pen,
    drawing: i32,
    baseline: f64,
    alignment_set: bool,
    fade_set: bool,
    karaoke_end: f64,
    karaoke_group: u32,
}

fn weight(value: i32) -> i32 { match value { -1 | 1 => 700, n if n <= 0 => 400, n => n } }
fn number(s: &str) -> f64 { let n = strtod(s.as_bytes()).0; if n.is_finite() { n.clamp(-1e9, 1e9) } else { 0.0 } }
fn integer(s: &str) -> i32 { strtoll(s.as_bytes(), 10).0.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32 }
fn hex(s: &str) -> u32 { strtoll(s.trim_start_matches(['&', 'H']).as_bytes(), 16).0 as u32 }
fn mix(old: f64, new: f64, power: f64) -> f64 { old * (1.0 - power) + new * power }
fn progress(now: f64, start: f64, end: f64) -> f64 {
    if now < start { 0.0 } else if now >= end { 1.0 } else { (now - start) / (end - start) }
}

fn colour(old: u32, new: u32, power: f64, alpha: bool) -> u32 {
    let mut out = old.to_be_bytes();
    let to = new.to_be_bytes();
    for i in if alpha { 3..4 } else { 0..3 } { out[i] = mix(f64::from(out[i]), f64::from(to[i]), power) as u8; }
    u32::from_be_bytes(out)
}

impl State<'_> {
    fn tags(&mut self, text: &str, power: f64, depth: u8) {
        if depth >= 16 { return; }
        let mut rest = text;
        while let Some(slash) = rest.find('\\') {
            rest = rest[slash + 1..].trim_start_matches([' ', '\t']);
            let end_name = rest.find(['(', '\\']).unwrap_or(rest.len());
            let name_value = &rest[..end_name];
            let mut args: Vec<&str> = Vec::new();
            let after = if rest.as_bytes().get(end_name) == Some(&b'(') {
                let body = &rest[end_name + 1..];
                let end = body.find(')').unwrap_or(body.len());
                let body = &body[..end];
                let (prefix, tags) = body.find('\\').map_or((body, None), |slash| {
                    let comma = body[..slash].rfind(',');
                    comma.map_or(("", Some(body)), |c| (&body[..c], Some(&body[c + 1..])))
                });
                args.extend(prefix.split(',').map(str::trim).filter(|s| !s.is_empty()).take(8));
                if let Some(tags) = tags { args.push(tags.trim()); }
                end_name + 1 + end + usize::from(end_name + 1 + end < rest.len())
            } else { end_name };
            rest = &rest[after..];
            const TAGS: &[&str] = &["xbord", "ybord", "xshad", "yshad", "alpha", "iclip", "fscx", "fscy", "fade", "blur", "bord", "move", "clip", "shad", "fax", "fay", "fsp", "frx", "fry", "frz", "pos", "fad", "org", "pbo", "fsc", "fn", "fs", "fr", "an", "1c", "2c", "3c", "4c", "1a", "2a", "3a", "4a", "be", "kt", "kf", "ko", "fe", "a", "c", "r", "b", "i", "s", "u", "p", "q", "k", "K", "t"];
            let Some(&tag) = TAGS.iter().find(|tag| name_value.starts_with(**tag)) else { continue };
            let value = name_value[tag.len()..].trim();
            if !value.is_empty() { args.push(value); }
            let value = args.first().copied().unwrap_or("");
            let n = number(value);
            let style = &self.track.styles[self.style];
            let p = &mut self.pen;
            match tag {
                "xbord" | "ybord" => { let i = usize::from(tag == "ybord"); p.border[i] = if args.is_empty() { style.outline } else { mix(p.border[i], n, power).max(0.0) }; }
                "xshad" | "yshad" => { let i = usize::from(tag == "yshad"); p.shadow[i] = if args.is_empty() { style.shadow } else { mix(p.shadow[i], n, power) }; }
                "bord" | "shad" => {
                    let defaults = if tag == "bord" { style.outline } else { style.shadow };
                    for v in if tag == "bord" { &mut p.border } else { &mut p.shadow } { *v = if args.is_empty() { defaults } else { mix(*v, n, power).max(0.0) }; }
                }
                "fax" | "fay" => { let i = usize::from(tag == "fay"); p.shear[i] = if args.is_empty() { 0.0 } else { mix(p.shear[i], n, power) }; }
                "frx" | "fry" | "frz" | "fr" => {
                    let i = match tag { "frx" => 0, "fry" => 1, _ => 2 };
                    p.rotation[i] = if args.is_empty() { if i == 2 { style.angle } else { 0.0 } } else { mix(p.rotation[i], n, power) };
                }
                "blur" => p.blur = if args.is_empty() { 0.0 } else { mix(p.blur, n, power).clamp(0.0, 100.0) },
                "be" => p.be = if args.is_empty() { 0 } else { (mix(f64::from(p.be), n, power) + 0.5).clamp(0.0, 127.0) as i32 },
                "fscx" => p.text.scale_x = if args.is_empty() { style.scale_x } else { mix(p.text.scale_x, n / 100.0, power).clamp(0.0, 1000.0) },
                "fscy" => p.text.scale_y = if args.is_empty() { style.scale_y } else { mix(p.text.scale_y, n / 100.0, power).clamp(0.0, 1000.0) },
                "fsc" => { p.text.scale_x = style.scale_x; p.text.scale_y = style.scale_y; }
                "fsp" => p.text.spacing = if args.is_empty() { style.spacing } else { mix(p.text.spacing, n, power) },
                "fs" => {
                    let size = if value.starts_with(['+', '-']) { p.text.size * (1.0 + power * n / 10.0) } else { mix(p.text.size, n, power) };
                    p.text.size = if args.is_empty() || size <= 0.0 { style.font_size } else { size.min(8192.0) };
                }
                "fn" => p.text.family = if args.is_empty() || value == "0" { String::from_utf8_lossy(&style.font_name).into_owned() } else { value.into() },
                "b" => p.text.weight = weight(if args.is_empty() || !(integer(value) == 0 || integer(value) == 1 || integer(value) >= 100) { style.bold } else { integer(value) }),
                "i" | "u" | "s" => {
                    let base = match tag { "i" => style.italic, "u" => style.underline, _ => style.strike_out };
                    let val = if args.is_empty() || !(0..=1).contains(&integer(value)) { base != 0 } else { integer(value) != 0 };
                    match tag { "i" => p.text.italic = val, "u" => p.text.underline = val, _ => p.text.strike = val }
                }
                "fe" => p.encoding = if args.is_empty() { style.encoding } else { integer(value) },
                "alpha" => {
                    let defaults = [style.primary_colour, style.secondary_colour, style.outline_colour, style.back_colour];
                    for (i, c) in p.colours.iter_mut().enumerate() { *c = colour(*c, if args.is_empty() { defaults[i] } else { hex(value) & 255 }, if args.is_empty() { 1.0 } else { power }, true); }
                }
                "c" | "1c" | "2c" | "3c" | "4c" | "1a" | "2a" | "3a" | "4a" => {
                    let i = if tag == "c" { 0 } else { usize::from(tag.as_bytes()[0] - b'1') };
                    let alpha = tag.ends_with('a');
                    let defaults = [style.primary_colour, style.secondary_colour, style.outline_colour, style.back_colour];
                    let val = if args.is_empty() { defaults[i] } else if alpha { hex(value) & 255 } else { hex(value).swap_bytes() };
                    p.colours[i] = colour(p.colours[i], val, if args.is_empty() { 1.0 } else { power }, alpha);
                }
                "an" | "a" if !self.alignment_set => {
                    let v = integer(value);
                    self.parsed.alignment = if tag == "an" && (1..=9).contains(&v) { numpad2align(v) }
                        else if tag == "a" && (1..=11).contains(&v) { if v & 3 == 0 { 5 } else { v } } else { style.alignment };
                    self.alignment_set = true;
                }
                "pos" if args.len() == 2 && self.parsed.position.is_none() => {
                    self.parsed.position = Some([number(args[0]), number(args[1])]); self.parsed.collisions = false;
                }
                "move" if (args.len() == 4 || args.len() == 6) && self.parsed.position.is_none() => {
                    let (mut t1, mut t2) = if args.len() == 6 { (number(args[4]), number(args[5])) } else { (0.0, 0.0) };
                    if t1 > t2 { std::mem::swap(&mut t1, &mut t2); }
                    if t1 <= 0.0 && t2 <= 0.0 { t1 = 0.0; t2 = self.event.duration as f64; }
                    let k = progress(self.now, t1, t2);
                    self.parsed.position = Some([mix(number(args[0]), number(args[2]), k), mix(number(args[1]), number(args[3]), k)]);
                    self.parsed.animated = true; self.parsed.collisions = false;
                }
                "org" if args.len() == 2 && self.parsed.origin.is_none() => {
                    self.parsed.origin = Some([number(args[0]), number(args[1])]); self.parsed.collisions = false;
                }
                "fad" | "fade" if !self.fade_set && (args.len() == 2 || args.len() == 7) => {
                    let (a, t) = if args.len() == 2 {
                        ([255.0, 0.0, 255.0], [0.0, number(args[0]), self.event.duration as f64 - number(args[1]), self.event.duration as f64])
                    } else { ([number(args[0]), number(args[1]), number(args[2])], [number(args[3]), number(args[4]), number(args[5]), number(args[6])]) };
                    p.fade = if self.now < t[0] { a[0] } else if self.now < t[1] { mix(a[0], a[1], progress(self.now, t[0], t[1])) }
                        else if self.now < t[2] { a[1] } else if self.now < t[3] { mix(a[1], a[2], progress(self.now, t[2], t[3])) } else { a[2] };
                    self.fade_set = true; self.parsed.animated = true;
                }
                "clip" | "iclip" => {
                    let inverse = tag == "iclip";
                    if args.len() == 4 {
                        let target = [number(args[0]), number(args[1]), number(args[2]), number(args[3])];
                        let old = match &self.parsed.clip { Some(Clip::Rect { bounds, .. }) => *bounds, _ => [0.0, 0.0, f64::from(self.track.play_res_x), f64::from(self.track.play_res_y)] };
                        self.parsed.clip = Some(Clip::Rect { bounds: std::array::from_fn(|i| mix(old[i], target[i], power)), inverse });
                    } else if (args.len() == 1 || args.len() == 2) && !matches!(self.parsed.clip, Some(Clip::Vector { .. })) {
                        self.parsed.clip = Some(Clip::Vector { text: args[args.len() - 1].as_bytes().to_vec(), scale: if args.len() == 2 { integer(args[0]) } else { 1 }, inverse });
                    }
                }
                "t" if (1..=4).contains(&args.len()) => {
                    let tags = args[args.len() - 1];
                    if !tags.contains('\\') { continue; }
                    let (t1, mut t2, accel) = match args.len() {
                        4 => (number(args[0]), number(args[1]), number(args[2])),
                        3 => (number(args[0]), number(args[1]), 1.0),
                        2 => (0.0, 0.0, number(args[0])),
                        _ => (0.0, 0.0, 1.0),
                    };
                    if t2 == 0.0 { t2 = self.event.duration as f64; }
                    let k = progress(self.now, t1, t2).powf(accel).clamp(0.0, 1.0);
                    self.parsed.animated = true; self.parsed.collisions = false;
                    self.tags(tags, if k.is_finite() { k } else { 0.0 }, depth + 1);
                }
                "r" => {
                    let index = self.track.styles.iter().rposition(|s| s.name == value.as_bytes()).filter(|_| !args.is_empty()).unwrap_or(self.event.style);
                    self.style = index.min(self.track.styles.len() - 1);
                    let (fade, karaoke) = (self.pen.fade, self.pen.karaoke);
                    self.pen = Pen::from_style(&self.track.styles[self.style]);
                    self.pen.fade = fade; self.pen.karaoke = karaoke;
                }
                "p" => self.drawing = integer(value).clamp(0, 30),
                "pbo" => self.baseline = n,
                "q" => self.parsed.wrap = if args.is_empty() || !(0..=3).contains(&integer(value)) { self.track.wrap_style } else { integer(value) },
                "kt" => self.karaoke_end = n * 10.0,
                "k" | "K" | "kf" | "ko" => {
                    let duration = if args.is_empty() { 1000.0 } else { n * 10.0 };
                    self.karaoke_group = self.karaoke_group.saturating_add(1);
                    p.karaoke = Some(Karaoke { group: self.karaoke_group, kind: match tag { "K" | "kf" => KaraokeKind::Sweep, "ko" => KaraokeKind::Outline, _ => KaraokeKind::Step }, start: self.karaoke_end, end: self.karaoke_end + duration });
                    self.karaoke_end += duration; self.parsed.animated = true;
                }
                _ => {}
            }
        }
    }

    fn append(&mut self, text: &str, drawing: Option<Drawing>) {
        let start = self.parsed.text.len();
        self.parsed.text.push_str(text);
        let end = self.parsed.text.len();
        if drawing.is_none() {
            if let Some(last) = self.parsed.runs.last_mut().filter(|r| r.drawing.is_none() && r.pen == self.pen) { last.range.end = end; return; }
        }
        self.parsed.runs.push(Run { range: start..end, pen: self.pen.clone(), drawing });
    }
}

pub fn parse(track: &Track, event: &Event, time: i64) -> Option<Parsed> {
    if event.text.len() > 64 << 10 || event.style >= track.styles.len() { return None; }
    let style = &track.styles[event.style];
    let mut state = State { track, event, style: event.style, now: time.saturating_sub(event.start) as f64,
        parsed: Parsed { text: String::new(), runs: Vec::new(), alignment: style.alignment, position: None, origin: None, clip: None, wrap: track.wrap_style, animated: false, collisions: true },
        pen: Pen::from_style(style), drawing: 0, baseline: 0.0, alignment_set: false, fade_set: false, karaoke_end: 0.0, karaoke_group: 0 };
    let text = String::from_utf8_lossy(&event.text);
    let mut rest = text.as_ref();
    while !rest.is_empty() && state.parsed.runs.len() < 4096 && state.parsed.text.len() < 64 << 10 {
        if rest.starts_with('{') {
            if let Some(end) = rest.find('}') { state.tags(&rest[1..end], 1.0, 0); rest = &rest[end + 1..]; continue; }
        }
        if state.drawing != 0 {
            let end = rest.find('{').unwrap_or(rest.len()).max(rest.chars().next()?.len_utf8());
            let drawing = Drawing { text: rest[..end].as_bytes().to_vec(), scale: state.drawing, baseline: state.baseline };
            state.append("\u{fffc}", Some(drawing)); rest = &rest[end..]; continue;
        }
        let (ch, consumed) = if rest.starts_with('\\') {
            match rest.as_bytes().get(1) {
                Some(b'N') => ('\n', 2),
                Some(b'n') => (if state.parsed.wrap == 2 { '\n' } else { ' ' }, 2),
                Some(b'h') => ('\u{a0}', 2),
                Some(b'{') => ('{', 2), Some(b'}') => ('}', 2),
                _ => ('\\', 1),
            }
        } else {
            let ch = rest.chars().next()?;
            (if matches!(ch, '\t' | '\r' | '\n') { ' ' } else { ch }, ch.len_utf8())
        };
        if ch == '\0' { break; }
        let mut buf = [0; 4];
        state.append(ch.encode_utf8(&mut buf), None);
        rest = &rest[consumed..];
    }
    Some(state.parsed)
}

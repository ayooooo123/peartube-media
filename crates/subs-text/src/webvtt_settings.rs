//! WebVTT cue settings and regions: what places a cue on the video.
//!
//! Clean-room implementation from W3C WebVTT: The Web Video Text Tracks
//! Format (Candidate Recommendation, <https://www.w3.org/TR/webvtt1/>):
//! §6.2 (region settings parsing), §6.3 (cue settings parsing) and the
//! computed values of §3 (computed line, position and position
//! alignment). FFmpeg keeps the settings only as packet side data; its
//! decoder and libass ignore them, so placement follows the browser
//! semantics this specification defines.
//!
//! Settings come from untrusted input: every byte sequence parses, an
//! invalid setting is skipped as the specification says, and every
//! number kept is finite and within the range the specification allows.

use crate::webvtt::box_at;

/// A cue's writing direction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vertical {
    #[default]
    Horizontal,
    /// `vertical:rl`: lines stack right to left.
    GrowingLeft,
    /// `vertical:lr`: lines stack left to right.
    GrowingRight,
}

/// Where the cue box sits on its line (`line:N,start|center|end`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LineAlign {
    #[default]
    Start,
    Center,
    End,
}

/// What the position anchors (`position:N%,line-left|center|line-right`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PositionAlign {
    LineLeft,
    Center,
    LineRight,
    /// From the text alignment (the computed position alignment).
    #[default]
    Auto,
}

/// The cue text alignment (`align:`). WebVTT's default is centered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CueAlign {
    Start,
    #[default]
    Center,
    End,
    Left,
    Right,
}

/// A WebVTT region (§6.2), from a `REGION` block of the file header.
#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub id: Vec<u8>,
    /// Percentage of the video width, 0..=100.
    pub width: f64,
    /// Lines of text the region shows.
    pub lines: u32,
    /// The point of the region at `viewport_anchor`, as percentages of
    /// the region's width and height.
    pub region_anchor: (f64, f64),
    /// Where `region_anchor` sits, as percentages of the video.
    pub viewport_anchor: (f64, f64),
    /// `scroll:up`: lines scroll up as cues are added.
    pub scroll_up: bool,
}

impl Default for Region {
    fn default() -> Self {
        Region { id: Vec::new(), width: 100.0, lines: 3, region_anchor: (0.0, 100.0), viewport_anchor: (0.0, 100.0), scroll_up: false }
    }
}

/// A cue's settings (§6.3), with the specification's defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct CueSettings {
    /// The region the cue is shown in, if any.
    pub region: Option<Region>,
    pub vertical: Vertical,
    /// The line: a line number when `snap_to_lines`, else a percentage;
    /// `None` is `auto`.
    pub line: Option<f64>,
    pub snap_to_lines: bool,
    pub line_align: LineAlign,
    /// Percentage, 0..=100; `None` is `auto`.
    pub position: Option<f64>,
    pub position_align: PositionAlign,
    /// Percentage, 0..=100.
    pub size: f64,
    pub align: CueAlign,
}

impl Default for CueSettings {
    fn default() -> Self {
        CueSettings {
            region: None,
            vertical: Vertical::Horizontal,
            line: None,
            snap_to_lines: true,
            line_align: LineAlign::Start,
            position: None,
            position_align: PositionAlign::Auto,
            size: 100.0,
            align: CueAlign::Center,
        }
    }
}

/// The tokens of `input` split on ASCII whitespace.
fn tokens(input: &[u8]) -> impl Iterator<Item = &[u8]> {
    input.split(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\x0c' | b'\r')).filter(|t| !t.is_empty())
}

/// `name:value`, both non-empty; `None` for anything else.
fn name_value(setting: &[u8]) -> Option<(&[u8], &[u8])> {
    let colon = setting.iter().position(|&b| b == b':')?;
    (colon > 0 && colon + 1 < setting.len()).then(|| (&setting[..colon], &setting[colon + 1..]))
}

/// `value` split at its first comma.
fn split_comma(value: &[u8]) -> (&[u8], Option<&[u8]>) {
    match value.iter().position(|&b| b == b',') {
        Some(i) => (&value[..i], Some(&value[i + 1..])),
        None => (value, None),
    }
}

/// An ASCII string of digits, `-` and `.` as a number.
fn number(s: &[u8]) -> Option<f64> {
    std::str::from_utf8(s).ok()?.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// "Parse a percentage string": digits, optionally `.` and digits, then
/// `%`, in 0..=100.
fn percentage(s: &[u8]) -> Option<f64> {
    let digits = s.strip_suffix(b"%")?;
    let (int, frac) = match digits.iter().position(|&b| b == b'.') {
        Some(i) => (&digits[..i], Some(&digits[i + 1..])),
        None => (digits, None),
    };
    let all_digits = |part: &[u8]| !part.is_empty() && part.iter().all(u8::is_ascii_digit);
    if !all_digits(int) || frac.is_some_and(|f| !all_digits(f)) {
        return None;
    }
    number(digits).filter(|v| (0.0..=100.0).contains(v))
}

/// A line number (`line:` without `%`), validated as §6.3 says.
fn line_number(s: &[u8]) -> Option<f64> {
    if s.iter().any(|&b| !(b == b'-' || b == b'.' || b.is_ascii_digit())) {
        return None;
    }
    if s.iter().skip(1).any(|&b| b == b'-') {
        return None;
    }
    let dots: Vec<usize> = s.iter().enumerate().filter(|&(_, &b)| b == b'.').map(|(i, _)| i).collect();
    match dots.as_slice() {
        [] => {}
        [i] => {
            let digit = |j: Option<usize>| j.and_then(|j| s.get(j)).is_some_and(u8::is_ascii_digit);
            if *i == 0 || *i + 1 == s.len() || !digit(i.checked_sub(1)) || !digit(Some(i + 1)) {
                return None;
            }
        }
        _ => return None,
    }
    number(s)
}

impl CueSettings {
    /// "Parse the WebVTT cue settings" from a cue's settings bytes (what
    /// follows the end time on its timing line), `regions` being the
    /// file's. Settings apply in order; an invalid one is skipped.
    pub fn parse(settings: &[u8], regions: &[Region]) -> CueSettings {
        let mut cue = CueSettings::default();
        for setting in tokens(settings) {
            let Some((name, value)) = name_value(setting) else { continue };
            match name {
                b"region" => cue.region = regions.iter().rev().find(|r| r.id == value).cloned(),
                b"vertical" => {
                    match value {
                        b"rl" => cue.vertical = Vertical::GrowingLeft,
                        b"lr" => cue.vertical = Vertical::GrowingRight,
                        _ => {}
                    }
                    if cue.vertical != Vertical::Horizontal {
                        cue.region = None;
                    }
                }
                b"line" => {
                    let (linepos, linealign) = split_comma(value);
                    if !linepos.iter().any(u8::is_ascii_digit) {
                        continue;
                    }
                    let percent = linepos.last() == Some(&b'%');
                    let Some(number) = (if percent { percentage(linepos) } else { line_number(linepos) }) else { continue };
                    let align = match linealign {
                        Some(b"start") => LineAlign::Start,
                        Some(b"center") => LineAlign::Center,
                        Some(b"end") => LineAlign::End,
                        Some(_) => continue,
                        None => cue.line_align,
                    };
                    cue.line_align = align;
                    cue.line = Some(number);
                    cue.snap_to_lines = !percent;
                    cue.region = None;
                }
                b"position" => {
                    let (colpos, colalign) = split_comma(value);
                    let Some(number) = percentage(colpos) else { continue };
                    let align = match colalign {
                        Some(b"line-left") => PositionAlign::LineLeft,
                        Some(b"center") => PositionAlign::Center,
                        Some(b"line-right") => PositionAlign::LineRight,
                        Some(_) => continue,
                        None => cue.position_align,
                    };
                    cue.position_align = align;
                    cue.position = Some(number);
                }
                b"size" => {
                    let Some(number) = percentage(value) else { continue };
                    cue.size = number;
                    if number != 100.0 {
                        cue.region = None;
                    }
                }
                b"align" => match value {
                    b"start" => cue.align = CueAlign::Start,
                    b"center" => cue.align = CueAlign::Center,
                    b"end" => cue.align = CueAlign::End,
                    b"left" => cue.align = CueAlign::Left,
                    b"right" => cue.align = CueAlign::Right,
                    _ => {}
                },
                _ => {}
            }
        }
        cue
    }

    /// The computed position: the position, or from the text alignment
    /// when `auto`.
    pub fn computed_position(&self) -> f64 {
        match (self.position, self.align) {
            (Some(position), _) => position,
            (None, CueAlign::Left) => 0.0,
            (None, CueAlign::Right) => 100.0,
            (None, _) => 50.0,
        }
    }

    /// The computed position alignment, for left-to-right cue text (this
    /// renderer lays out no right-to-left text).
    pub fn computed_position_align(&self) -> PositionAlign {
        match (self.position_align, self.align) {
            (PositionAlign::Auto, CueAlign::Left | CueAlign::Start) => PositionAlign::LineLeft,
            (PositionAlign::Auto, CueAlign::Right | CueAlign::End) => PositionAlign::LineRight,
            (PositionAlign::Auto, CueAlign::Center) => PositionAlign::Center,
            (align, _) => align,
        }
    }

    /// The computed line: the line, else -1 (the last line of the only
    /// track shown) for `auto` with snapping, 100 without.
    pub fn computed_line(&self) -> f64 {
        match self.line {
            Some(line) if !self.snap_to_lines && !(0.0..=100.0).contains(&line) => 100.0,
            Some(line) => line,
            None if !self.snap_to_lines => 100.0,
            None => -1.0,
        }
    }

    /// The cue box size the position leaves room for, 0..=100.
    pub fn computed_size(&self) -> f64 {
        let position = self.computed_position();
        let maximum = match self.computed_position_align() {
            PositionAlign::LineLeft => 100.0 - position,
            PositionAlign::LineRight => position,
            _ if position <= 50.0 => position * 2.0,
            _ => (100.0 - position) * 2.0,
        };
        self.size.min(maximum)
    }

    /// Where the cue box starts along its line, a percentage of the video
    /// width (horizontal) or height (vertical).
    pub fn box_start(&self) -> f64 {
        let (position, size) = (self.computed_position(), self.computed_size());
        match self.computed_position_align() {
            PositionAlign::LineLeft => position,
            PositionAlign::LineRight => position - size,
            _ => position - size / 2.0,
        }
    }
}

/// "Collect WebVTT region settings" (§6.2) from a `REGION` block's lines.
fn parse_region(settings: &[u8]) -> Region {
    let mut region = Region::default();
    for setting in tokens(settings) {
        let Some((name, value)) = name_value(setting) else { continue };
        let pair = |value: &[u8]| {
            let (x, y) = split_comma(value);
            Some((percentage(x)?, percentage(y?)?))
        };
        match name {
            b"id" => region.id = value.to_vec(),
            b"width" => {
                if let Some(width) = percentage(value) {
                    region.width = width;
                }
            }
            b"lines" => {
                if value.iter().all(u8::is_ascii_digit) {
                    region.lines = value.iter().fold(0u32, |n, &d| n.saturating_mul(10).saturating_add(u32::from(d - b'0')));
                }
            }
            b"regionanchor" => {
                if let Some(anchor) = pair(value) {
                    region.region_anchor = anchor;
                }
            }
            b"viewportanchor" => {
                if let Some(anchor) = pair(value) {
                    region.viewport_anchor = anchor;
                }
            }
            b"scroll" => {
                if value == b"up" {
                    region.scroll_up = true;
                }
            }
            _ => {}
        }
    }
    region
}

/// The regions a WebVTT header defines, in order: its `REGION` blocks
/// that name an `id`. `header` is the file header (the `WEBVTT` block and
/// what follows it up to the first cue, as a `.vtt` file and WebM's
/// `CodecPrivate` carry it) or an MP4 `wvtt` sample entry's boxes, whose
/// `vttC` box holds that header.
pub fn header_regions(header: &[u8]) -> Vec<Region> {
    let mut at = 0;
    let mut text = header;
    while let Some(child) = box_at(header, at) {
        if &child.kind == b"vttC" {
            text = &header[child.body];
            break;
        }
        at = child.end;
    }
    let text = text.strip_prefix(b"\xef\xbb\xbf").unwrap_or(text);
    let mut regions = Vec::new();
    let mut lines = text.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).peekable();
    while let Some(line) = lines.next() {
        if line.strip_prefix(b"REGION").is_some_and(|rest| rest.iter().all(|&b| b == b' ' || b == b'\t')) {
            let mut block = Vec::new();
            while let Some(line) = lines.next_if(|l| !l.is_empty()) {
                block.extend_from_slice(line);
                block.push(b'\n');
            }
            let region = parse_region(&block);
            if !region.id.is_empty() {
                regions.push(region);
            }
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> CueSettings {
        CueSettings::parse(s.as_bytes(), &[])
    }

    #[test]
    fn settings_parse_as_the_specification_says() {
        let s = parse("line:-2,end position:10%,line-right size:35.5% align:start vertical:rl");
        assert_eq!(s.line, Some(-2.0));
        assert!(s.snap_to_lines);
        assert_eq!(s.line_align, LineAlign::End);
        assert_eq!((s.position, s.position_align), (Some(10.0), PositionAlign::LineRight));
        assert_eq!(s.size, 35.5);
        assert_eq!((s.align, s.vertical), (CueAlign::Start, Vertical::GrowingLeft));
        let s = parse("line:62.5%");
        assert_eq!((s.line, s.snap_to_lines), (Some(62.5), false));
        // Invalid settings are skipped, and the defaults stay.
        for bad in ["line:1-", "line:--1", "line:1.", "line:.5", "line:1.2.3", "line:abc", "line:101%", "line:5,top",
                    "position:50", "position:101%", "position:5%,left", "size:-1%", "size:1e2%", "align:middle", ":x", "line:", "x"] {
            assert_eq!(parse(bad), CueSettings::default(), "{bad}");
        }
        assert_eq!(parse("line:0").line, Some(0.0));
    }

    #[test]
    fn computed_values() {
        assert_eq!(parse("").computed_line(), -1.0);
        assert_eq!(parse("align:left").computed_position(), 0.0);
        assert_eq!(parse("align:end size:50%").box_start(), 0.0);
        assert_eq!(parse("align:start size:50%").box_start(), 50.0);
        // A centered cue at 20% has room for 40%.
        let s = parse("position:20%");
        assert_eq!((s.computed_size(), s.box_start()), (40.0, 0.0));
    }

    #[test]
    fn regions_come_from_region_blocks_and_drop_out_as_settings_say() {
        let header = b"WEBVTT\n\nREGION\nid:fred width:40%\nlines:2 regionanchor:0%,100%\nviewportanchor:10%,90% scroll:up\n\nREGION\nwidth:10%\n\nREGION\nid:fred lines:5\n";
        let regions = header_regions(header);
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0], Region { id: b"fred".to_vec(), width: 40.0, lines: 2, region_anchor: (0.0, 100.0), viewport_anchor: (10.0, 90.0), scroll_up: true });
        // The last region of an id is the one cues use.
        assert_eq!(CueSettings::parse(b"region:fred", &regions).region.unwrap().lines, 5);
        assert!(CueSettings::parse(b"region:fred size:50%", &regions).region.is_none());
        assert!(CueSettings::parse(b"region:fred vertical:lr", &regions).region.is_none());
        assert!(CueSettings::parse(b"region:fred line:3", &regions).region.is_none());
        assert!(CueSettings::parse(b"region:bill", &regions).region.is_none());
        // The pre-standard `Region:` header syntax defines no region.
        assert!(header_regions(b"WEBVTT\nRegion: id=fred width=40%\n").is_empty());
        // An MP4 sample entry's vttC box holds the header.
        let vttc = [&(8 + header.len() as u32).to_be_bytes()[..], b"vttC", header].concat();
        assert_eq!(header_regions(&vttc), regions);
    }
}

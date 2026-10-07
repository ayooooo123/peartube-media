//! MicroDVD: standalone `.sub` demuxer and the `microdvd` decoder.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! headers verified):
//! - `libavformat/microdvddec.c` — probe, frame-number timing (23.976 fps
//!   unless a leading `{1}{1}fps` line gives the rate), `{DEFAULT}{}` style
//!   line as extradata;
//! - `libavcodec/microdvddec.c` — `{y:}`/`{c:}`/`{f:}`/`{s:}`/`{P:}`/`{o:}`
//!   tags and `|` line breaks to ASS, the default style from extradata;
//! - `libavutil/rational.c` — `av_d2q`/`av_reduce` for the frame rate.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

use crate::ass_text::{default_header, AssEvent, AssEventDecoder, EventSource, DEFAULT_ALIGNMENT, DEFAULT_COLOR, DEFAULT_FONT, DEFAULT_FONT_SIZE};
use crate::scan::{strtol, strtol_hex, Scan};
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const CODEC_ID: &str = "microdvd";
pub const CONTAINER_NAME: &str = "microdvd";

const BOM: &[u8] = b"\xef\xbb\xbf";

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

fn frame_pair(line: &[u8]) -> Option<(Scan<'_>, i64)> {
    let mut s = Scan::new(line);
    s.lit(b'{')?;
    let frame = s.int(0)? as i32;
    s.lit(b'}')?;
    Some((s, i64::from(frame)))
}

/// One of the three line forms `microdvd_probe` accepts.
fn probe_line(line: &[u8]) -> bool {
    let empty_end = frame_pair(line).is_some_and(|(mut s, _)| s.lit_str(b"{}").and_then(|_| s.any()).is_some());
    let with_end = frame_pair(line).is_some_and(|(mut s, _)| {
        s.lit(b'{').and_then(|_| s.int(0)).and_then(|_| s.lit(b'}')).and_then(|_| s.any()).is_some()
    });
    let default = Scan::new(line).lit_str(b"{DEFAULT}{}").is_some_and(|_| line.len() > 11 && line[11] != 0);
    empty_end || with_end || default
}

/// `microdvd_probe`: three consecutive event (or style) lines.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    let mut rest: &[u8] = &text;
    for _ in 0..3 {
        if !probe_line(rest) {
            return 0;
        }
        // ff_subtitles_next_line: every '\r', then one '\n'.
        let mut next = crate::scan::strcspn(rest, b"\r\n");
        while rest.get(next) == Some(&b'\r') {
            next += 1;
        }
        if rest.get(next) == Some(&b'\n') {
            next += 1;
        }
        rest = &rest[next.min(rest.len())..];
    }
    MAX_PROBE_SCORE
}

/// `av_reduce`.
fn reduce(num: i64, den: i64, max: i64) -> (i64, i64) {
    let (mut a0, mut a1) = ((0i64, 1i64), (1i64, 0i64));
    let sign = (num < 0) ^ (den < 0);
    let gcd = {
        let (mut a, mut b) = (num.unsigned_abs(), den.unsigned_abs());
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let (mut num, mut den) = (num.unsigned_abs() as i64, den.unsigned_abs() as i64);
    if gcd != 0 {
        num = (num as u64 / gcd) as i64;
        den = (den as u64 / gcd) as i64;
    }
    if num <= max && den <= max {
        a1 = (num, den);
        den = 0;
    }
    while den != 0 {
        let x = num / den;
        let next_den = num - den * x;
        let a2n = x.saturating_mul(a1.0).saturating_add(a0.0);
        let a2d = x.saturating_mul(a1.1).saturating_add(a0.1);
        if a2n > max || a2d > max {
            let mut x = x;
            if a1.0 != 0 {
                x = (max - a0.0) / a1.0;
            }
            if a1.1 != 0 {
                x = x.min((max - a0.1) / a1.1);
            }
            if den.saturating_mul((2 * x * a1.1).saturating_add(a0.1)) > num.saturating_mul(a1.1) {
                a1 = (x * a1.0 + a0.0, x * a1.1 + a0.1);
            }
            break;
        }
        a0 = a1;
        a1 = (a2n, a2d);
        num = den;
        den = next_den;
    }
    (if sign { -a1.0 } else { a1.0 }, a1.1)
}

/// `av_d2q`.
fn d2q(d: f64, max: i64) -> (i64, i64) {
    if d.is_nan() {
        return (0, 0);
    }
    if d.abs() > i32::MAX as f64 + 3.0 {
        return (if d < 0.0 { -1 } else { 1 }, 0);
    }
    let exponent = if d == 0.0 { 0 } else { d.abs().log2().floor() as i64 + 1 };
    let exponent = (exponent - 1).max(0);
    let den = 1i64 << (62 - exponent);
    let num = (d * den as f64 + 0.5).floor() as i64;
    let mut a = reduce(num, den, max);
    if (a.0 == 0 || a.1 == 0) && d != 0.0 && max > 0 && max < i64::from(i32::MAX) {
        a = reduce(num, den, i64::from(i32::MAX));
    }
    a
}

/// `microdvd_read_header` over a whole decoded file: the time base, the
/// `{DEFAULT}` style line and the events.
pub(crate) fn demux_microdvd(text: &[u8]) -> (TimeBase, Option<Vec<u8>>, SubtitleQueue) {
    let mut pts_info = (2997i64, 125i64);
    let mut extradata: Option<Vec<u8>> = None;
    let mut q = SubtitleQueue::default();
    let mut reader = TextReader::new(text);
    let mut buf = Vec::new();
    let mut i = 0;
    while !reader.eof() {
        let pos = reader.pos();
        let len = reader.get_line(2048, &mut buf);
        if len == 0 {
            break;
        }
        let mut line: &[u8] = &buf;
        if line.starts_with(BOM) {
            line = &line[3..];
        }
        line = &line[..crate::scan::strcspn(line, b"\r\n")];
        if line.is_empty() {
            continue;
        }
        let first_lines = i < 3;
        i += 1;
        if first_lines {
            let fps_line = |with_end: bool| -> Option<(i64, f64)> {
                let (mut s, frame) = frame_pair(line)?;
                s.lit(b'{')?;
                if with_end {
                    s.int(0)?;
                }
                s.lit(b'}')?;
                Some((frame, s.float(6)?))
            };
            if let Some((frame, fps)) = fps_line(false).or_else(|| fps_line(true)) {
                if frame <= 1 && fps > 3.0 && fps < 100.0 {
                    pts_info = d2q(fps, 100_000);
                    continue;
                }
            }
            if extradata.is_none() && line.starts_with(b"{DEFAULT}{}") && line.len() > 11 {
                extradata = Some(line[11..].to_vec());
                continue;
            }
        }
        // SKIP_FRAME_ID twice
        let Some(first) = line.iter().position(|&b| b == b'}') else { continue };
        let Some(second) = line[first + 1..].iter().position(|&b| b == b'}') else { continue };
        let p = &line[first + 1 + second + 1..];
        if p.is_empty() {
            continue;
        }
        // get_pts: "{%d}{%c"
        let Some(pts) = frame_pair(line).and_then(|(mut s, frame)| s.lit(b'{').and_then(|_| s.any()).map(|_| frame)) else {
            continue;
        };
        // get_duration: "{%d}{%d}" (the closing brace is not checked).
        let duration = frame_pair(line)
            .and_then(|(mut s, start)| {
                s.lit(b'{')?;
                Some(i64::from(s.int(0)? as i32) - start)
            })
            .unwrap_or(-1);
        match q.insert(p, false) {
            Some(e) => (e.pos, e.pts, e.duration) = (pos, pts, duration),
            None => break,
        }
    }
    let (num, den) = reduce(pts_info.1, pts_info.0, i64::from(i32::MAX));
    let time_base = if num > 0 && den > 0 { TimeBase::new(num, den) } else { TimeBase::new(125, 2997) };
    (time_base, extradata, q)
}

/// Opens a standalone MicroDVD file.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "MicroDVD")?;
    let (time_base, extradata, queue) = demux_microdvd(&decode_text(&raw));
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    params.extradata = extradata.unwrap_or_default();
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets: queue.finalize(time_base),
    }))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Colour, Font, Size, cHarset, stYle, Style (persistent), Position, cOordinate.
const TAGS: &[u8; 8] = b"cfshyYpo";
/// italic, bold, underline, strike-through
const STYLES: &[u8; 4] = b"ibus";

#[derive(Clone, Copy, PartialEq)]
enum Persistence {
    Off,
    On,
    Opened,
}

#[derive(Clone)]
struct Tag {
    key: u8,
    persistent: Persistence,
    data1: u32,
    data2: u32,
    data_string: Vec<u8>,
}

impl Default for Tag {
    fn default() -> Self {
        Self { key: 0, persistent: Persistence::Off, data1: 0, data2: 0, data_string: Vec::new() }
    }
}

type Tags = [Tag; 8];

fn set_tag(tags: &mut Tags, tag: Tag) {
    if let Some(index) = TAGS.iter().position(|&k| k == tag.key) {
        tags[index] = tag;
    }
}

/// `check_for_italic_slash_marker`.
fn italic_slash(tags: &mut Tags, s: &[u8], i: usize) -> usize {
    if s.get(i) == Some(&b'/') {
        let mut tag = tags[4].clone();
        tag.key = b'y';
        tag.data1 |= 1;
        set_tag(tags, tag);
        return i + 1;
    }
    i
}

/// `microdvd_load_tags`: parses the leading `{x:...}` tags of `s` from
/// `i`; returns where the text starts.
fn load_tags(tags: &mut Tags, s: &[u8], i: usize) -> usize {
    let at = |k: usize| s.get(k).copied().unwrap_or(0);
    let mut i = italic_slash(tags, s, i);
    while at(i) == b'{' {
        let start = i;
        let tag_char = at(i + 1);
        let mut tag = Tag::default();
        if tag_char == 0 || at(i + 2) != b':' {
            break;
        }
        i += 3;
        match tag_char {
            b'Y' | b'y' => {
                if tag_char == b'Y' {
                    tag.persistent = Persistence::On;
                }
                while at(i) != 0 && at(i) != b'}' && i - start < 256 {
                    if let Some(index) = STYLES.iter().position(|&c| c == at(i)) {
                        tag.data1 |= 1 << index;
                    }
                    i += 1;
                }
                if at(i) == b'}' {
                    tag.key = tag_char;
                }
            }
            b'C' | b'c' => {
                if tag_char == b'C' {
                    tag.persistent = Persistence::On;
                }
                while at(i) == b'$' || at(i) == b'#' {
                    i += 1;
                }
                let (value, used) = strtol_hex(&s[i.min(s.len())..]);
                tag.data1 = (value as u32) & 0x00ff_ffff;
                i += used;
                if at(i) == b'}' {
                    tag.key = b'c';
                }
            }
            b'F' | b'f' | b'H' => {
                if tag_char == b'F' {
                    tag.persistent = Persistence::On;
                }
                if let Some(len) = s.get(i..).and_then(|r| r.iter().position(|&b| b == b'}')) {
                    tag.data_string = s[i..i + len].to_vec();
                    i += len;
                    tag.key = if tag_char == b'H' { b'h' } else { b'f' };
                }
            }
            b'S' | b's' => {
                if tag_char == b'S' {
                    tag.persistent = Persistence::On;
                }
                let (value, used) = strtol(&s[i.min(s.len())..]);
                tag.data1 = value as u32;
                i += used;
                if at(i) == b'}' {
                    tag.key = b's';
                }
            }
            b'P' => {
                if at(i) != 0 {
                    tag.persistent = Persistence::On;
                    tag.data1 = u32::from(at(i) == b'1');
                    i += 1;
                    if at(i) == b'}' {
                        tag.key = b'p';
                    }
                }
            }
            b'o' => {
                tag.persistent = Persistence::On;
                let (x, used) = strtol(&s[i.min(s.len())..]);
                tag.data1 = x as u32;
                i += used;
                if at(i) == b',' {
                    i += 1;
                    let (y, used) = strtol(&s[i.min(s.len())..]);
                    tag.data2 = y as u32;
                    i += used;
                    if at(i) == b'}' {
                        tag.key = b'o';
                    }
                }
            }
            _ => {}
        }
        if tag.key == 0 {
            return start;
        }
        set_tag(tags, tag);
        i += 1;
    }
    italic_slash(tags, s, i)
}

/// `microdvd_open_tags`.
fn open_tags(out: &mut Vec<u8>, tags: &mut Tags) {
    for tag in tags.iter_mut() {
        if tag.persistent == Persistence::Opened {
            continue;
        }
        match tag.key {
            b'Y' | b'y' => {
                for (index, &style) in STYLES.iter().enumerate() {
                    if tag.data1 & (1 << index) != 0 {
                        out.extend_from_slice(format!("{{\\{}1}}", style as char).as_bytes());
                    }
                }
            }
            b'c' => out.extend_from_slice(format!("{{\\c&H{:06X}&}}", tag.data1).as_bytes()),
            b'f' => {
                out.extend_from_slice(b"{\\fn");
                out.extend_from_slice(&tag.data_string);
                out.push(b'}');
            }
            b's' => out.extend_from_slice(format!("{{\\fs{}}}", tag.data1 as i32).as_bytes()),
            b'p' => {
                if tag.data1 == 0 {
                    out.extend_from_slice(b"{\\an8}");
                }
            }
            b'o' => out.extend_from_slice(format!("{{\\pos({},{})}}", tag.data1 as i32, tag.data2 as i32).as_bytes()),
            _ => {}
        }
        if tag.persistent == Persistence::On {
            tag.persistent = Persistence::Opened;
        }
    }
}

/// `microdvd_close_no_persistent_tags`.
fn close_tags(out: &mut Vec<u8>, tags: &mut Tags) {
    for tag in tags.iter_mut().rev() {
        if tag.persistent != Persistence::Off {
            continue;
        }
        match tag.key {
            b'y' => {
                for (index, &style) in STYLES.iter().enumerate().rev() {
                    if tag.data1 & (1 << index) != 0 {
                        out.extend_from_slice(format!("{{\\{}0}}", style as char).as_bytes());
                    }
                }
            }
            b'c' => out.extend_from_slice(b"{\\c}"),
            b'f' => out.extend_from_slice(b"{\\fn}"),
            b's' => out.extend_from_slice(b"{\\fs}"),
            _ => {}
        }
        tag.key = 0;
    }
}

/// `microdvd_decode_frame`: the event, or `None` for no text.
pub fn microdvd_to_ass(line: &[u8]) -> Option<Vec<u8>> {
    let line = crate::scan::c_str(line);
    let mut tags: Tags = Default::default();
    let mut out = Vec::with_capacity(line.len() + 16);
    let mut i = 0;
    while i < line.len() {
        i = load_tags(&mut tags, line, i);
        open_tags(&mut out, &mut tags);
        while i < line.len() && line[i] != b'|' {
            out.push(line[i]);
            i += 1;
        }
        if i < line.len() {
            close_tags(&mut out, &mut tags);
            out.extend_from_slice(b"\\N");
            i += 1;
        }
    }
    (!out.is_empty()).then_some(out)
}

/// `microdvd_init`: the `Default` style a `{DEFAULT}{}` line sets.
fn header(extradata: &[u8]) -> crate::ass_split::AssHeader {
    let mut font = DEFAULT_FONT.as_bytes().to_vec();
    let (mut size, mut color) = (DEFAULT_FONT_SIZE, DEFAULT_COLOR);
    let (mut bold, mut italic, mut underline, mut alignment) = (false, false, false, DEFAULT_ALIGNMENT);
    if !extradata.is_empty() {
        let mut tags: Tags = Default::default();
        load_tags(&mut tags, crate::scan::c_str(extradata), 0);
        for tag in &tags {
            match tag.key.to_ascii_lowercase() {
                b'y' => {
                    italic |= tag.data1 & 1 != 0;
                    bold |= tag.data1 & 2 != 0;
                    underline |= tag.data1 & 4 != 0;
                }
                b'c' => color = tag.data1,
                b's' => size = tag.data1 as i32,
                b'p' => alignment = 8,
                b'f' => font = tag.data_string.clone(),
                _ => {}
            }
        }
    }
    default_header(&String::from_utf8_lossy(&font), size, color, bold, italic, underline, alignment)
}

struct MicroDvd;

impl EventSource for MicroDvd {
    fn event(&mut self, _packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>> {
        Ok(microdvd_to_ass(text).map(AssEvent::default_style))
    }
}

/// The `microdvd` decoder.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!("not a MicroDVD codec id: {}", params.codec_id)));
    }
    Ok(Box::new(AssEventDecoder::new(params.codec_id.clone(), header(&params.extradata), MicroDvd)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ass(s: &str) -> Option<String> {
        microdvd_to_ass(s.as_bytes()).map(|v| String::from_utf8(v).unwrap())
    }

    #[test]
    fn tags_convert_like_ffmpeg() {
        assert_eq!(ass("{y:i}a|b").as_deref(), Some(r"{\i1}a{\i0}\Nb"));
        assert_eq!(ass("{Y:b}{c:$0000ff}a|b").as_deref(), Some(r"{\c&H0000FF&}{\b1}a{\c}\Nb"));
        assert_eq!(ass("/x|/y").as_deref(), Some(r"{\i1}x{\i0}\N{\i1}y"));
        assert_eq!(ass("{q:z}t").as_deref(), Some("{q:z}t"));
        assert_eq!(ass(""), None);
    }

    #[test]
    fn frame_rate_line_sets_the_time_base() {
        let (tb, extradata, q) = demux_microdvd(b"{1}{1}25\n{DEFAULT}{}{y:i}\n{25}{50}hello\n{50}{}open\n");
        assert_eq!(tb, TimeBase::new(1, 25));
        assert_eq!(extradata.as_deref(), Some(&b"{y:i}"[..]));
        let packets: Vec<_> = q.finalize(tb).into_iter().map(|p| (p.pts.unwrap(), p.duration.unwrap())).collect();
        assert_eq!(packets, vec![(25, 25), (50, -1)]);
        assert_eq!(d2q(23.976, 100_000), (2997, 125));
        assert_eq!(demux_microdvd(b"{0}{1}x\n").0, TimeBase::new(125, 2997));
    }
}

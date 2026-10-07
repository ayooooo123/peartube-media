//! SubRip: standalone `.srt` demuxer and the `subrip` decoder.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! headers verified):
//! - `libavformat/srtdec.c` — probe, cue splitting (timing lines delimit
//!   cues; blank lines do not; index lines are recognised only before a
//!   timing line), empty-cue dropping;
//! - `libavcodec/srtdec.c` and `libavcodec/htmlsubtitles.c` — the SubRip
//!   HTML-like markup to ASS conversion.
//!
//! Packets carry the bare cue text with container timing, as FFmpeg's
//! demuxer and Matroska `S_TEXT/UTF8` blocks do. Cue coordinates
//! (`X1:… Y2:…`) are parsed past and not used for placement.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

use crate::ass_text::{ffmpeg_default_header, AssEvent, AssEventDecoder, EventSource};
use crate::scan::{strtol, Scan};
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const CODEC_ID: &str = "subrip";
pub const CONTAINER_NAME: &str = "srt";

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// `get_event_info`: `(pts_ms, duration_ms)` of a timing line.
fn event_info(line: &[u8]) -> Option<(i64, i64)> {
    let mut s = Scan::new(line);
    let stamp = |s: &mut Scan| -> Option<i64> {
        let h = s.int(0)? as i32;
        s.lit(b':')?;
        let m = s.int(0)? as i32;
        s.lit(b':')?;
        let sec = s.int(0)? as i32;
        s.set(1, |b| b == b',' || b == b'.')?;
        let ms = s.int(0)? as i32;
        Some((i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(sec)) * 1000 + i64::from(ms))
    };
    let start = stamp(&mut s)?;
    s.ws();
    s.lit_str(b"-->")?;
    s.ws();
    let end = stamp(&mut s)?;
    // `int duration`: the difference wraps to 32 bits.
    Some((start, i64::from(end.wrapping_sub(start) as i32)))
}

/// `srt_probe`.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    let mut reader = TextReader::new(&text);
    while matches!(reader.peek(), b'\r' | b'\n') {
        reader.r8();
    }
    let mut line = Vec::new();
    if reader.read_line(64, &mut line).is_none() {
        return 0;
    }
    let (number, used) = strtol(&line);
    if number < 0 || used == 0 {
        return 0;
    }
    if reader.read_line(64, &mut line).is_none() {
        return 0;
    }
    let digits_from = usize::from(line.first() == Some(&b'-'));
    let has_arrow = line.windows(5).any(|w| w == b" --> ");
    if line.get(digits_from).is_some_and(u8::is_ascii_digit) && has_arrow && event_info(&line).is_some() {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// `add_event`.
fn add_event(q: &mut SubtitleQueue, buf: &mut Vec<u8>, line_cache: &mut Vec<u8>, info: (i64, i64, i64), append_cache: bool) {
    if append_cache && !line_cache.is_empty() {
        buf.extend_from_slice(line_cache);
        buf.push(b'\n');
    }
    line_cache.clear();
    while buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if !buf.is_empty() {
        if let Some(event) = q.insert(buf, false) {
            (event.pos, event.pts, event.duration) = info;
        }
        buf.clear();
    }
}

/// `srt_read_header` over a whole decoded file.
pub(crate) fn demux_srt(text: &[u8]) -> SubtitleQueue {
    let mut q = SubtitleQueue::default();
    let mut reader = TextReader::new(text);
    let mut buf = Vec::new();
    let mut line = Vec::new();
    let mut line_cache: Vec<u8> = Vec::new();
    // (pos, pts, duration) of the cue being collected.
    let mut info: Option<(i64, i64, i64)> = None;
    while !reader.eof() {
        let pos = reader.pos();
        let Some(len) = reader.read_line(4096, &mut line) else { break };
        if len == 0 {
            continue;
        }
        match event_info(&line) {
            None => {
                if info.is_none() {
                    continue;
                }
                if !line_cache.is_empty() {
                    buf.extend_from_slice(&line_cache);
                    buf.push(b'\n');
                    line_cache.clear();
                }
                let (number, used) = strtol(&line);
                if number < 0 || used == 0 {
                    buf.extend_from_slice(&line);
                    buf.push(b'\n');
                } else {
                    line_cache.clone_from(&line);
                }
            }
            Some((pts, duration)) => {
                if let Some(previous) = info {
                    let (number, used) = strtol(&line_cache);
                    let standalone_number = number >= 0 && used == line_cache.len();
                    let append_cache = buf.is_empty() && !standalone_number;
                    add_event(&mut q, &mut buf, &mut line_cache, previous, append_cache);
                }
                info = Some((pos, pts, duration));
            }
        }
        if q.len() >= crate::text_common::MAX_CUES {
            break;
        }
    }
    // A trailing number is more likely text (a year) than an index.
    if let Some(last) = info {
        add_event(&mut q, &mut buf, &mut line_cache, last, true);
    }
    q
}

/// Opens a standalone SubRip file.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "SubRip")?;
    let time_base = TimeBase::new(1, 1000);
    let packets = demux_srt(&decode_text(&raw)).finalize(time_base);
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets,
    }))
}

// ---------------------------------------------------------------------------
// Decoder: SubRip markup to ASS (htmlsubtitles.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct FontTag {
    face: Vec<u8>,
    size: u32,
    color: u32,
}

fn is_tag_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'/'
}

/// `scantag`: up to 127 bytes before `>`; `None` at `<`, NUL or overflow.
fn scantag(input: &[u8]) -> Option<(&[u8], usize)> {
    for len in 0..128 {
        match input.get(len).copied().unwrap_or(0) {
            0 | b'<' => return None,
            b'>' => return Some((&input[..len], len + 1)),
            _ => {}
        }
    }
    None
}

fn rstrip_spaces(dst: &mut Vec<u8>) {
    while dst.last() == Some(&b' ') {
        dst.pop();
    }
}

fn hex_upper(v: u32) -> String {
    format!("{v:X}")
}

/// `ff_htmlmarkup_to_ass`.
pub fn htmlmarkup_to_ass(input: &[u8]) -> Vec<u8> {
    let input = crate::scan::c_str(input);
    let at = |i: usize| input.get(i).copied().unwrap_or(0);
    let mut dst: Vec<u8> = Vec::with_capacity(input.len() + 16);
    let mut stack: Vec<FontTag> = vec![FontTag::default()];
    let mut line_start = true;
    let mut an = 0;
    let mut closing_brace_missing = false;
    let mut i = 0usize;
    while i < input.len() {
        let mut end = false;
        match at(i) {
            b'\r' => {}
            b'\n' => {
                if line_start {
                    end = true;
                } else {
                    rstrip_spaces(&mut dst);
                    dst.extend_from_slice(b"\\N");
                    line_start = true;
                }
            }
            b' ' => {
                if !line_start {
                    dst.push(b' ');
                }
            }
            b'{' => {
                // handle_open_brace: drop {\...} and {Y:...} blocks except
                // the first {\anN}.
                let scanbraces = input[i..].starts_with(b"{\\an") && at(i + 4).is_ascii_digit() && at(i + 5) == b'}';
                an += i32::from(scanbraces);
                let mut skipped = false;
                if !closing_brace_missing
                    && ((an != 1 && at(i + 1) == b'\\') || (at(i + 1) != 0 && b"CcFfoPSsYy".contains(&at(i + 1)) && at(i + 2) == b':'))
                {
                    match input.get(i + 2..).and_then(|r| r.iter().position(|&b| b == b'}')) {
                        Some(close) => {
                            i += 2 + close;
                            skipped = true;
                        }
                        None => closing_brace_missing = true,
                    }
                }
                if !skipped {
                    dst.push(b'{');
                }
            }
            b'<' => {
                let mut likely_a_tag = true;
                while at(i + 1) == b'<' {
                    dst.push(b'<');
                    likely_a_tag = false;
                    i += 1;
                }
                let tag_close = at(i + 1) == b'/';
                if tag_close {
                    likely_a_tag = true;
                }
                let scanned = scantag(input.get(i + usize::from(tag_close) + 1..).unwrap_or(&[]));
                match scanned.filter(|&(_, len)| len > 0) {
                    Some((tag, len)) => {
                        let skip = len + usize::from(tag_close);
                        let mut name = tag;
                        while name.first() == Some(&b' ') {
                            likely_a_tag = false;
                            name = &name[1..];
                        }
                        let (name, mut param) = match name.iter().position(|&b| b == b' ') {
                            Some(sp) => (&name[..sp], Some(&name[sp + 1..])),
                            None => (name, None),
                        };
                        if !name.iter().all(|&b| is_tag_char(b)) {
                            likely_a_tag = false;
                        }
                        if name.eq_ignore_ascii_case(b"font") {
                            if tag_close && stack.len() > 1 {
                                let cur = stack.pop().unwrap_or_default();
                                let last = stack.last().cloned().unwrap_or_default();
                                if cur.size != 0 {
                                    if last.size == 0 {
                                        dst.extend_from_slice(b"{\\fs}");
                                    } else if last.size != cur.size {
                                        dst.extend_from_slice(format!("{{\\fs{}}}", last.size as i32).as_bytes());
                                    }
                                }
                                if cur.color & 0xff00_0000 != 0 {
                                    if last.color & 0xff00_0000 == 0 {
                                        dst.extend_from_slice(b"{\\c}");
                                    } else if last.color != cur.color {
                                        dst.extend_from_slice(format!("{{\\c&H{}&}}", hex_upper(last.color & 0xffffff)).as_bytes());
                                    }
                                }
                                if !cur.face.is_empty() {
                                    if last.face.is_empty() {
                                        dst.extend_from_slice(b"{\\fn}");
                                    } else if last.face != cur.face {
                                        dst.extend_from_slice(b"{\\fn");
                                        dst.extend_from_slice(&last.face);
                                        dst.push(b'}');
                                    }
                                }
                            } else if !tag_close && stack.len() < 16 {
                                let mut new_tag = stack.last().cloned().unwrap_or_default();
                                while let Some(p) = param {
                                    let mut p = p;
                                    let lower = |n: usize| p.get(..n).map(<[u8]>::to_ascii_lowercase);
                                    if lower(5).as_deref() == Some(b"size=") {
                                        p = &p[5 + usize::from(p.get(5) == Some(&b'"'))..];
                                        if let Some(size) = Scan::new(p).uint(0) {
                                            new_tag.size = size as u32;
                                            dst.extend_from_slice(format!("{{\\fs{}}}", new_tag.size).as_bytes());
                                        }
                                    } else if lower(6).as_deref() == Some(b"color=") {
                                        p = &p[6 + usize::from(p.get(6) == Some(&b'"'))..];
                                        if let Some(color) = crate::html_color::html_color(p) {
                                            new_tag.color = 0xff00_0000 | color;
                                            dst.extend_from_slice(format!("{{\\c&H{}&}}", hex_upper(new_tag.color & 0xffffff)).as_bytes());
                                        }
                                    } else if lower(5).as_deref() == Some(b"face=") {
                                        let quoted = p.get(5) == Some(&b'"');
                                        p = &p[5 + usize::from(quoted)..];
                                        let len = crate::scan::strcspn(p, if quoted { b"\"" } else { b" " });
                                        new_tag.face = p[..len.min(127)].to_vec();
                                        p = &p[len..];
                                        dst.extend_from_slice(b"{\\fn");
                                        dst.extend_from_slice(&new_tag.face);
                                        dst.push(b'}');
                                    }
                                    param = p.iter().position(|&b| b == b' ').map(|sp| &p[sp + 1..]);
                                }
                                stack.push(new_tag);
                            }
                            i += skip;
                        } else if name.len() == 1 && b"bisu".contains(&name[0].to_ascii_lowercase()) {
                            dst.extend_from_slice(format!("{{\\{}{}}}", name[0].to_ascii_lowercase() as char, u8::from(!tag_close)).as_bytes());
                            i += skip;
                        } else if name.len() >= 2
                            && name[..2].eq_ignore_ascii_case(b"br")
                            && (name.len() == 2 || (name.len() == 3 && name[2] == b'/'))
                        {
                            dst.extend_from_slice(b"\\N");
                            i += skip;
                        } else if likely_a_tag {
                            i += skip;
                        } else {
                            dst.push(b'<');
                        }
                    }
                    None => dst.push(b'<'),
                }
            }
            c => dst.push(c),
        }
        let c = at(i);
        if c != b' ' && c != b'\r' && c != b'\n' {
            line_start = false;
        }
        i += 1;
        if end {
            break;
        }
    }
    while dst.ends_with(b"\\N") {
        dst.truncate(dst.len() - 2);
    }
    rstrip_spaces(&mut dst);
    dst
}

struct SubRip;

impl EventSource for SubRip {
    fn event(&mut self, _packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>> {
        Ok(Some(AssEvent::default_style(htmlmarkup_to_ass(text))))
    }
}

/// The `subrip` decoder: bare cue text, timing from the packet.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!("not a SubRip codec id: {}", params.codec_id)));
    }
    Ok(Box::new(AssEventDecoder::new(params.codec_id.clone(), ffmpeg_default_header(), SubRip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ass(s: &str) -> String {
        String::from_utf8(htmlmarkup_to_ass(s.as_bytes())).unwrap()
    }

    #[test]
    fn markup_converts_like_ffmpeg() {
        assert_eq!(ass("<i>hi</i> <b>x</b>"), r"{\i1}hi{\i0} {\b1}x{\b0}");
        assert_eq!(ass("  a  \n  b \n"), r"a\Nb");
        assert_eq!(ass("a\n\nlost"), "a");
        assert_eq!(ass(r"{\an8}top {\b1}x{\pos(1,2)}"), r"{\an8}top {\b1}x{\pos(1,2)}");
        assert_eq!(ass(r"{\b1}x{\an8}"), r"x{\an8}");
        assert_eq!(ass("<font color=\"red\" size=9>r</font>"), r"{\c&HFF&}{\fs9}r{\fs}{\c}");
        assert_eq!(ass("<<a>> <unknown>t</unknown> < x>"), "<<a>> t < x>");
        assert_eq!(ass("< b>bold"), r"{\b1}bold");
    }

    #[test]
    fn cues_split_on_timing_lines_and_keep_payload_numbers() {
        let text = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n\n5\n\nworld\n2\n00:00:03,000 --> 00:00:04,000\n\n3\n00:00:05,000 --> 00:00:06,000\n2015\n";
        let packets: Vec<_> = demux_srt(text).finalize(TimeBase::new(1, 1000)).into_iter()
            .map(|p| (p.pts.unwrap(), p.duration.unwrap(), String::from_utf8(p.data).unwrap())).collect();
        assert_eq!(packets, vec![(1000, 1000, "hello\n5\nworld".to_string()), (5000, 1000, "2015".to_string())]);
    }
}

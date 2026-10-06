//! SAMI (Synchronized Accessible Media Interchange) subtitle demuxer & decoder.
//!
//! Ported to safe Rust from FFmpeg's:
//! - `libavformat/samidec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//! - `libavformat/subtitles.c` (same commit/license; queue ordering and duplicates)
//! - `libavcodec/samidec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//! - `libavcodec/htmlsubtitles.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//! - `libavcodec/srtenc.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//!
//! SAMI subtitles structure:
//! - Container: chunks delimited by `<SYNC Start=ms>`, sorted by pts/pos, with durations
//!   inferred from adjacent sync points.
//! - Codec: `<P Class=... ID=Source>` tags represent speaker names and persist across
//!   subsequent cues until overwritten. HTML markup (`<b>`, `<i>`, `<u>`, `<s>`, `<font>`)
//!   is parsed into styled subtitle segments, and empty `&nbsp;` events are skipped.

use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, Frame, MediaType, Packet,
    ProbeData, ProbeScore, ReadSeek, Result, Segment, StreamInfo, SubtitleCue, TimeBase,
    MAX_PROBE_SCORE,
};

use crate::text_common::{decode_subtitle_text, TextSubtitleDemuxer, MAX_CUES, MAX_FILE_BYTES};

pub const CODEC_ID: &str = "sami";
pub const CONTAINER_NAME: &str = "sami";

/// Maximum length of a single subtitle cue packet in bytes (1 MiB).
const MAX_CUE_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// Probe for SAMI container: looks for `<SAMI>` or `<sami>` tag.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let buf = data.buf;
    let s = String::from_utf8_lossy(buf);
    let s_lower = s.to_ascii_lowercase();
    if s_lower.contains("<sami") {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// Open a SAMI file as a demuxer.
pub fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut raw = Vec::new();
    input
        .by_ref()
        .take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut raw)
        .map_err(Error::from)?;
    if raw.len() > MAX_FILE_BYTES {
        return Err(Error::invalid("SAMI file exceeds maximum supported size"));
    }

    let text = decode_subtitle_text(&raw);
    let packets = demux_sami_text(&text)?;

    let time_base = TimeBase::new(1, 1_000); // milliseconds
    let mut params = CodecParameters::audio(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    params.sample_rate = None;
    params.channels = None;
    params.sample_format = None;

    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: None,
        start_time: Some(0),
        params,
    };

    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [stream],
        packets,
    }))
}

struct RawPacket {
    pos: usize,
    pts: i64,
    duration: i64,
    data: String,
}

fn demux_sami_text(text: &str) -> Result<VecDeque<Packet>> {
    // Truncate at </BODY if present (case-insensitive)
    let end_limit = match text.to_ascii_lowercase().find("</body") {
        Some(idx) => idx,
        None => text.len(),
    };
    let active_text = &text[..end_limit];

    // Find all <SYNC Start=ms> tags
    let mut sync_indices: Vec<(usize, i64)> = Vec::new();
    let mut search_from = 0;
    let lower = active_text.to_ascii_lowercase();

    while let Some(idx) = lower[search_from..].find("<sync") {
        let abs_start = search_from + idx;
        if let Some(tag_close) = active_text[abs_start..].find('>') {
            let tag = &active_text[abs_start..abs_start + tag_close + 1];
            if let Some(start_ms) = extract_sync_start(tag) {
                sync_indices.push((abs_start, start_ms));
            }
            search_from = abs_start + tag_close + 1;
        } else {
            break;
        }
    }

    if sync_indices.is_empty() {
        return Ok(VecDeque::new());
    }

    let mut raw_packets: Vec<RawPacket> = Vec::with_capacity(sync_indices.len().min(MAX_CUES));
    for (i, &(abs_start, pts)) in sync_indices.iter().enumerate() {
        if raw_packets.len() >= MAX_CUES {
            break;
        }
        let chunk_end = if i + 1 < sync_indices.len() {
            sync_indices[i + 1].0
        } else {
            active_text.len()
        };
        let data = active_text[abs_start..chunk_end].to_string();
        raw_packets.push(RawPacket {
            pos: abs_start,
            pts,
            duration: -1,
            data,
        });
    }

    // Sort by pts ascending, then pos ascending (FFmpeg SUB_SORT_TS_POS)
    raw_packets.sort_unstable_by_key(|p| (p.pts, p.pos));

    // Calculate durations from subsequent packet PTS
    let len = raw_packets.len();
    for i in 0..len {
        if i + 1 < len {
            raw_packets[i].duration = raw_packets[i + 1].pts - raw_packets[i].pts;
        } else {
            raw_packets[i].duration = -1;
        }
    }
    raw_packets.dedup_by(|a, b| a.pts == b.pts && a.duration == b.duration && a.data == b.data);

    let time_base = TimeBase::new(1, 1_000);
    let mut packets = VecDeque::with_capacity(raw_packets.len());
    for p in raw_packets {
        let mut pkt = Packet::new(0, time_base, p.data.into_bytes());
        pkt.pts = Some(p.pts);
        pkt.dts = Some(p.pts);
        if p.duration >= 0 {
            pkt.duration = Some(p.duration);
        }
        pkt.flags.keyframe = true;
        packets.push_back(pkt);
    }

    Ok(packets)
}

fn extract_sync_start(tag: &str) -> Option<i64> {
    let lower = tag.to_ascii_lowercase();
    let idx = lower.find("start=")?;
    let rest = tag[idx + 6..].trim_start();
    let rest = if rest.starts_with('"') || rest.starts_with('\'') {
        &rest[1..]
    } else {
        rest
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<i64>().ok()
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Create a new SAMI decoder instance.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!(
            "not a sami codec id: {}",
            params.codec_id
        )));
    }
    Ok(Box::new(SamiDecoder {
        codec_id: params.codec_id.clone(),
        source: None,
        pending: VecDeque::new(),
        eof: false,
    }))
}

pub struct SamiDecoder {
    codec_id: CodecId,
    source: Option<String>,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for SamiDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("SAMI packet exceeds maximum size"));
        }
        let text = decode_subtitle_text(&packet.data);
        if let Some(cue) = self.decode_packet_text(&text, packet)? {
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
}

impl SamiDecoder {
    fn decode_packet_text(&mut self, text: &str, packet: &Packet) -> Result<Option<SubtitleCue>> {
        // Parse paragraph tags (<P ...>)
        let mut p_indices: Vec<(usize, String)> = Vec::new();
        let mut search_from = 0;
        let lower = text.to_ascii_lowercase();

        while let Some(idx) = lower[search_from..].find("<p") {
            let abs_start = search_from + idx;
            // Ensure not <PRE or similar
            if abs_start + 2 < text.len() {
                let next_char = text.as_bytes()[abs_start + 2];
                if next_char != b'>' && !next_char.is_ascii_whitespace() {
                    search_from = abs_start + 2;
                    continue;
                }
            }
            if let Some(tag_close) = text[abs_start..].find('>') {
                let tag = &text[abs_start..abs_start + tag_close];
                p_indices.push((abs_start + tag_close + 1, tag.to_string()));
                search_from = abs_start + tag_close + 1;
            } else {
                break;
            }
        }

        if p_indices.is_empty() {
            return Ok(None);
        }

        let mut contents: Vec<String> = Vec::new();
        for (i, (content_start, tag)) in p_indices.iter().enumerate() {
            let content_end = if i + 1 < p_indices.len() {
                // Look back from next paragraph tag
                let next_start = p_indices[i + 1].0;
                let slice = &text[..next_start];
                slice.to_ascii_lowercase().rfind("<p").unwrap_or(next_start)
            } else {
                text.len()
            };

            let p_raw = &text[*content_start..content_end];
            let tag_lower = tag.to_ascii_lowercase();
            let is_source = tag_lower.contains("id=source") || tag_lower.contains("id=\"source\"");

            // Check if empty event (&nbsp;) -> skips subtitle
            let trimmed = p_raw.trim_start();
            if trimmed.to_ascii_lowercase().starts_with("&nbsp;") {
                return Ok(None);
            }

            // Extract text: convert <BR> to \N, collapse whitespace, keep other HTML tags
            let p_body = extract_sami_body(p_raw);
            if is_source {
                self.source = Some(p_body.trim().to_string());
            } else {
                contents.push(p_body);
            }
        }

        // Format full ASS string: {\\i1}source{\\i0}\\N + content
        let mut full_ass = String::new();
        if let Some(src) = &self.source {
            if !src.is_empty() {
                let enc_src = htmlmarkup_to_ass(src);
                full_ass.push_str(r"{\i1}");
                full_ass.push_str(&enc_src);
                full_ass.push_str(r"{\i0}\N");
            }
        }

        let content_joined = contents.join(r"\N");
        let enc_content = htmlmarkup_to_ass(&content_joined);
        full_ass.push_str(&enc_content);

        // Strip trailing \N
        while full_ass.ends_with(r"\N") {
            full_ass.truncate(full_ass.len() - 2);
        }

        // Parse ASS override tags into styled Segments
        let segments = ass_to_segments(&full_ass);

        let start_us = packet
            .pts
            .map(|pts| packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000)))
            .unwrap_or(0);

        // If duration is unset (-1), fallback to UINT32_MAX ms (matching FFmpeg)
        let end_us = if let Some(dur) = packet.duration {
            if dur >= 0 {
                start_us + packet.time_base.rescale(dur, TimeBase::new(1, 1_000_000))
            } else {
                start_us + (u32::MAX as i64) * 1_000
            }
        } else {
            start_us + (u32::MAX as i64) * 1_000
        };

        Ok(Some(SubtitleCue {
            start_us,
            end_us,
            style_ref: None,
            positioning: None,
            segments,
        }))
    }
}

/// Extract paragraph body, mapping `<BR>` to `\N` and collapsing whitespace.
fn extract_sami_body(p_text: &str) -> String {
    let mut out = String::with_capacity(p_text.len());
    let mut i = 0;
    let bytes = p_text.as_bytes();
    let mut prev_space = false;

    while i < bytes.len() {
        if bytes[i] == b'<' {
            let slice = &p_text[i..];
            let slice_lower = slice.to_ascii_lowercase();
            if slice_lower.starts_with("<p") {
                let next = slice.as_bytes().get(2).copied().unwrap_or(b'\0');
                if next == b'>' || next.is_ascii_whitespace() {
                    break;
                }
            }
            if slice_lower.starts_with("<br") {
                out.push_str(r"\N");
                if let Some(close_idx) = slice.find('>') {
                    i += close_idx + 1;
                } else {
                    break;
                }
                prev_space = false;
                continue;
            }
        }

        let c = p_text[i..].chars().next().unwrap();
        if c.is_ascii_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
        i += c.len_utf8();
    }

    out
}

// ---------------------------------------------------------------------------
// HTML markup to ASS converter (ported from libavcodec/htmlsubtitles.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
struct FontTag {
    color: u32,
    size: u32,
    face: String,
}

fn htmlmarkup_to_ass(src: &str) -> String {
    let mut dst = String::with_capacity(src.len() * 2);
    let mut stack: Vec<FontTag> = vec![FontTag::default()];
    let mut line_start = true;
    let mut end = false;
    let mut i = 0;
    let chars: Vec<char> = src.chars().collect();

    while i < chars.len() && !end {
        let c = chars[i];
        if c == '\r' {
            i += 1;
            continue;
        }
        if c == '\n' {
            if line_start {
                end = true;
                i += 1;
                continue;
            }
            while dst.ends_with(' ') {
                dst.pop();
            }
            dst.push_str(r"\N");
            line_start = true;
            i += 1;
            continue;
        }
        if c == ' ' {
            if !line_start {
                dst.push(' ');
            }
            i += 1;
            continue;
        }
        if c == '<' {
            let slice: String = chars[i..].iter().collect();
            let tag_close = slice.starts_with("</");
            let start_idx = if tag_close { 2 } else { 1 };
            if let Some(close_rel) = slice[start_idx..].find('>') {
                let tag_body = &slice[start_idx..start_idx + close_rel];
                let skip = start_idx + close_rel + 1;
                let trimmed = tag_body.trim();
                let mut parts = trimmed.splitn(2, |ch: char| ch.is_ascii_whitespace());
                let tagname = parts.next().unwrap_or("").to_ascii_lowercase();
                let param = parts.next().unwrap_or("");

                if tagname == "font" {
                    if tag_close {
                        if stack.len() > 1 {
                            let cur_tag = stack.pop().unwrap();
                            let last_tag = stack.last().unwrap();
                            if cur_tag.size > 0 {
                                if last_tag.size == 0 {
                                    dst.push_str(r"{\fs}");
                                } else if last_tag.size != cur_tag.size {
                                    dst.push_str(&format!(r"{{\fs{}}}", last_tag.size));
                                }
                            }
                            if cur_tag.color > 0 {
                                if last_tag.color == 0 {
                                    dst.push_str(r"{\c}");
                                } else if last_tag.color != cur_tag.color {
                                    dst.push_str(&format!(r"{{\c&H{:06X}&}}", last_tag.color));
                                }
                            }
                            if !cur_tag.face.is_empty() {
                                if last_tag.face.is_empty() {
                                    dst.push_str(r"{\fn}");
                                } else if last_tag.face != cur_tag.face {
                                    dst.push_str(&format!(r"{{\fn{}}}", last_tag.face));
                                }
                            }
                        }
                    } else {
                        let mut new_tag = stack.last().cloned().unwrap_or_default();
                        // Parse font params (color, size, face)
                        if let Some(color_val) = parse_color_attr(param) {
                            new_tag.color = color_val;
                            dst.push_str(&format!(r"{{\c&H{:06X}&}}", color_val));
                        }
                        if let Some(size_val) = parse_size_attr(param) {
                            new_tag.size = size_val;
                            dst.push_str(&format!(r"{{\fs{}}}", size_val));
                        }
                        if let Some(face_val) = parse_face_attr(param) {
                            new_tag.face = face_val.clone();
                            dst.push_str(&format!(r"{{\fn{}}}", face_val));
                        }
                        stack.push(new_tag);
                    }
                    i += skip;
                    continue;
                } else if tagname.len() == 1 && matches!(tagname.as_str(), "b" | "i" | "s" | "u") {
                    let val = if tag_close { 0 } else { 1 };
                    dst.push_str(&format!(r"{{\{}{}}}", tagname, val));
                    i += skip;
                    continue;
                } else if tagname == "br" || (tagname.starts_with("br") && tagname.ends_with('/')) {
                    dst.push_str(r"\N");
                    i += skip;
                    continue;
                } else {
                    // Unrecognized tag: stripped!
                    i += skip;
                    continue;
                }
            } else {
                dst.push('<');
                i += 1;
                continue;
            }
        }

        dst.push(c);
        i += 1;
        line_start = false;
    }

    while dst.ends_with(r"\N") {
        dst.truncate(dst.len() - 2);
    }
    while dst.ends_with(' ') {
        dst.pop();
    }
    dst
}

fn parse_color_attr(param: &str) -> Option<u32> {
    let lower = param.to_ascii_lowercase();
    let idx = lower.find("color=")?;
    let mut rest = param[idx + 6..].trim_start();
    if rest.starts_with('"') || rest.starts_with('\'') {
        rest = &rest[1..];
    }
    let end_idx = rest.find(['"', '\'', ' ', '>', '\t']).unwrap_or(rest.len());
    let color_str = &rest[..end_idx];
    let (r, g, b) = parse_html_color_rgb(color_str)?;
    // In ASS: &HBBGGRR&
    Some(((b as u32) << 16) | ((g as u32) << 8) | (r as u32))
}

fn parse_size_attr(param: &str) -> Option<u32> {
    let lower = param.to_ascii_lowercase();
    let idx = lower.find("size=")?;
    let mut rest = param[idx + 5..].trim_start();
    if rest.starts_with('"') || rest.starts_with('\'') {
        rest = &rest[1..];
    }
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

fn parse_face_attr(param: &str) -> Option<String> {
    let lower = param.to_ascii_lowercase();
    let idx = lower.find("face=")?;
    let mut rest = param[idx + 5..].trim_start();
    let quote = if rest.starts_with('"') {
        rest = &rest[1..];
        Some('"')
    } else if rest.starts_with('\'') {
        rest = &rest[1..];
        Some('\'')
    } else {
        None
    };
    let end_idx = match quote {
        Some(q) => rest.find(q).unwrap_or(rest.len()),
        None => rest.find([' ', '>', '\t']).unwrap_or(rest.len()),
    };
    let face = rest[..end_idx].trim().to_string();
    if face.is_empty() {
        None
    } else {
        Some(face)
    }
}

pub fn parse_html_color_rgb(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_matches(|c| c == '"' || c == '\'');
    let s = s.strip_prefix('#').unwrap_or(s);
    match s.to_ascii_lowercase().as_str() {
        "yellow" => Some((255, 255, 0)),
        "purple" => Some((128, 0, 128)),
        "pink" => Some((255, 192, 203)),
        "red" => Some((255, 0, 0)),
        "orange" => Some((255, 165, 0)),
        "green" => Some((0, 128, 0)),
        "cyan" | "aqua" => Some((0, 255, 255)),
        "blue" => Some((0, 0, 255)),
        "gray" | "grey" => Some((128, 128, 128)),
        "brown" => Some((165, 42, 42)),
        "black" => Some((0, 0, 0)),
        "white" => Some((255, 255, 255)),
        "silver" => Some((192, 192, 192)),
        "lime" => Some((0, 255, 0)),
        "magenta" | "fuchsia" => Some((255, 0, 255)),
        "navy" => Some((0, 0, 128)),
        "olive" => Some((128, 128, 0)),
        "teal" => Some((0, 128, 128)),
        "maroon" => Some((128, 0, 0)),
        hex if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b))
        }
        hex if hex.len() == 3 && hex.chars().all(|c| c.is_ascii_hexdigit()) => {
            let r = u8::from_str_radix(&hex[0..1].repeat(2), 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2].repeat(2), 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3].repeat(2), 16).ok()?;
            Some((r, g, b))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ASS to styled Segments converter (ported from libavcodec/srtenc.c)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum ActiveTag {
    Bold,
    Italic,
    Underline,
    Strike,
    Font,
}

/// Parse ASS override codes into structured `Vec<Segment>`.
fn ass_to_segments(ass_str: &str) -> Vec<Segment> {
    let mut flat_items: Vec<AssItem> = Vec::new();
    let mut i = 0;
    let bytes = ass_str.as_bytes();

    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(close_rel) = ass_str[i..].find('}') {
                let tag = &ass_str[i + 1..i + close_rel];
                i += close_rel + 1;
                parse_ass_tag(tag, &mut flat_items);
                continue;
            }
        }
        if ass_str[i..].starts_with(r"\N") {
            flat_items.push(AssItem::LineBreak);
            i += 2;
            continue;
        }

        let c = ass_str[i..].chars().next().unwrap();
        // Accumulate plain text
        if let Some(AssItem::Text(s)) = flat_items.last_mut() {
            s.push(c);
        } else {
            flat_items.push(AssItem::Text(c.to_string()));
        }
        i += c.len_utf8();
    }

    // Convert flat items with style commands into nested Segment hierarchy
    items_to_segments(&flat_items)
}

enum AssItem {
    Text(String),
    LineBreak,
    StylePush(ActiveTag, Option<(u8, u8, u8)>, Option<f32>, Option<String>),
    StylePop(ActiveTag),
}

fn parse_ass_tag(tag: &str, items: &mut Vec<AssItem>) {
    if let Some(val) = tag.strip_prefix(r"\b") {
        if val == "1" {
            items.push(AssItem::StylePush(ActiveTag::Bold, None, None, None));
        } else {
            items.push(AssItem::StylePop(ActiveTag::Bold));
        }
    } else if let Some(val) = tag.strip_prefix(r"\i") {
        if val == "1" {
            items.push(AssItem::StylePush(ActiveTag::Italic, None, None, None));
        } else {
            items.push(AssItem::StylePop(ActiveTag::Italic));
        }
    } else if let Some(val) = tag.strip_prefix(r"\u") {
        if val == "1" {
            items.push(AssItem::StylePush(ActiveTag::Underline, None, None, None));
        } else {
            items.push(AssItem::StylePop(ActiveTag::Underline));
        }
    } else if let Some(val) = tag.strip_prefix(r"\s") {
        if val == "1" {
            items.push(AssItem::StylePush(ActiveTag::Strike, None, None, None));
        } else {
            items.push(AssItem::StylePop(ActiveTag::Strike));
        }
    } else if let Some(val) = tag.strip_prefix(r"\c&H") {
        let hex = val.trim_end_matches('&');
        if let Ok(ass_color) = u32::from_str_radix(hex, 16) {
            let b = ((ass_color >> 16) & 0xff) as u8;
            let g = ((ass_color >> 8) & 0xff) as u8;
            let r = (ass_color & 0xff) as u8;
            items.push(AssItem::StylePush(
                ActiveTag::Font,
                Some((r, g, b)),
                None,
                None,
            ));
        }
    } else if tag == r"\c" {
        items.push(AssItem::StylePop(ActiveTag::Font));
    } else if let Some(val) = tag.strip_prefix(r"\fs") {
        if val.is_empty() {
            items.push(AssItem::StylePop(ActiveTag::Font));
        } else if let Ok(sz) = val.parse::<f32>() {
            items.push(AssItem::StylePush(ActiveTag::Font, None, Some(sz), None));
        }
    } else if let Some(val) = tag.strip_prefix(r"\fn") {
        if val.is_empty() {
            items.push(AssItem::StylePop(ActiveTag::Font));
        } else {
            items.push(AssItem::StylePush(
                ActiveTag::Font,
                None,
                None,
                Some(val.to_string()),
            ));
        }
    }
}

struct FrameStack {
    tag: Option<ActiveTag>,
    color: Option<(u8, u8, u8)>,
    size: Option<f32>,
    family: Option<String>,
    children: Vec<Segment>,
}

fn items_to_segments(items: &[AssItem]) -> Vec<Segment> {
    let mut stack = vec![FrameStack {
        tag: None,
        color: None,
        size: None,
        family: None,
        children: Vec::new(),
    }];

    for item in items {
        match item {
            AssItem::Text(s) => {
                stack.last_mut().unwrap().children.push(Segment::Text(s.clone()));
            }
            AssItem::LineBreak => {
                stack.last_mut().unwrap().children.push(Segment::LineBreak);
            }
            AssItem::StylePush(tag, color, size, family) => {
                stack.push(FrameStack {
                    tag: Some(*tag),
                    color: *color,
                    size: *size,
                    family: family.clone(),
                    children: Vec::new(),
                });
            }
            AssItem::StylePop(tag) => {
                // Find matching tag in stack
                if let Some(pos) = stack.iter().rposition(|f| f.tag == Some(*tag)) {
                    while stack.len() > pos {
                        let popped = stack.pop().unwrap();
                        let seg = wrap_frame_to_segment(popped);
                        stack.last_mut().unwrap().children.push(seg);
                    }
                }
            }
        }
    }

    // Close remaining open tags (matching srtenc.c srt_end_cb)
    while stack.len() > 1 {
        let popped = stack.pop().unwrap();
        let seg = wrap_frame_to_segment(popped);
        stack.last_mut().unwrap().children.push(seg);
    }

    stack.pop().unwrap().children
}

fn wrap_frame_to_segment(frame: FrameStack) -> Segment {
    match frame.tag {
        Some(ActiveTag::Bold) => Segment::Bold(frame.children),
        Some(ActiveTag::Italic) => Segment::Italic(frame.children),
        Some(ActiveTag::Underline) => Segment::Underline(frame.children),
        Some(ActiveTag::Strike) => Segment::Strike(frame.children),
        Some(ActiveTag::Font) => {
            if let Some(rgb) = frame.color {
                Segment::Color {
                    rgb,
                    children: frame.children,
                }
            } else if frame.size.is_some() || frame.family.is_some() {
                Segment::Font {
                    family: frame.family,
                    size: frame.size,
                    children: frame.children,
                }
            } else {
                Segment::Font {
                    family: None,
                    size: None,
                    children: frame.children,
                }
            }
        }
        None => Segment::Font {
            family: None,
            size: None,
            children: frame.children,
        },
    }
}

/// Render subtitle segments to SRT format with lowercase hex colors, matching FFmpeg's SRT output.
pub fn render_srt_body(segments: &[Segment]) -> String {
    let mut out = String::new();
    append_segments(segments, &mut out);
    out
}

fn append_segments(segments: &[Segment], out: &mut String) {
    for seg in segments {
        match seg {
            Segment::Text(s) => out.push_str(s),
            Segment::LineBreak => out.push('\n'),
            Segment::Bold(c) => {
                out.push_str("<b>");
                append_segments(c, out);
                out.push_str("</b>");
            }
            Segment::Italic(c) => {
                out.push_str("<i>");
                append_segments(c, out);
                out.push_str("</i>");
            }
            Segment::Underline(c) => {
                out.push_str("<u>");
                append_segments(c, out);
                out.push_str("</u>");
            }
            Segment::Strike(c) => {
                out.push_str("<s>");
                append_segments(c, out);
                out.push_str("</s>");
            }
            Segment::Color { rgb, children } => {
                out.push_str(&format!(
                    "<font color=\"#{:02x}{:02x}{:02x}\">",
                    rgb.0, rgb.1, rgb.2
                ));
                append_segments(children, out);
                out.push_str("</font>");
            }
            Segment::Font {
                family,
                size,
                children,
            } => {
                let mut header = String::from("<font");
                if let Some(fam) = family {
                    header.push_str(&format!(" face=\"{}\"", fam));
                }
                if let Some(sz) = size {
                    header.push_str(&format!(" size=\"{}\"", *sz as u32));
                }
                header.push('>');
                out.push_str(&header);
                append_segments(children, out);
                out.push_str("</font>");
            }
            Segment::Voice { children, .. }
            | Segment::Class { children, .. }
            | Segment::Karaoke { children, .. } => {
                append_segments(children, out);
            }
            Segment::Timestamp { .. } => {}
            Segment::Raw(s) => out.push_str(s),
        }
    }
}

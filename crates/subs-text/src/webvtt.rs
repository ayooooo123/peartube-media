// Copyright (c) 2012 Clément Bœsch
//
// Derived from FFmpeg at commit 2da55bf: libavformat/webvttdec.c and
// libavcodec/webvttdec.c.
// Changed for PearTube on 2026-10-06 and 2026-10-07 (ported to safe Rust
// and modified).
//
// This file is free software; you can redistribute it and/or
// modify it under the terms of the GNU Lesser General Public
// License as published by the Free Software Foundation; either
// version 2.1 of the License, or (at your option) any later version.
//
// This file is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Lesser General Public License for more details.
//
// You should have received a copy of the GNU Lesser General Public
// License along with this file (crates/subs-text/LICENSE); if not,
// write to the Free Software Foundation, Inc., 51 Franklin Street,
// Fifth Floor, Boston, MA 02110-1301 USA

//! WebVTT: standalone `.vtt` demuxer and the `webvtt` decoder.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! headers verified):
//! - `libavformat/webvttdec.c` — probe, one packet per cue block holding the
//!   cue text, timing from the cue's timing line (identifier and settings
//!   are dropped); header, `STYLE`, `REGION` and `NOTE` blocks skipped;
//! - `libavcodec/webvttdec.c` — `<b> <i> <u>` to ASS, other tags (`<v>`,
//!   `<c>`, …) hidden, entities and inline cue timestamps (`{\kf}`).
//!
//! Matroska/WebM WebVTT blocks carry the same cue text with container
//! timing, so one decoder serves both.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

use crate::ass_text::{ffmpeg_default_header, AssEvent, AssEventDecoder, EventSource};
use crate::scan::Scan;
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const CODEC_ID: &str = "webvtt";
pub const CONTAINER_NAME: &str = "webvtt";

/// `webvtt_probe`.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    if text.starts_with(b"WEBVTT") && matches!(text.get(6), None | Some(0 | b'\n' | b'\r' | b'\t' | b' ')) {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// The demuxer's `read_ts`: `HH:MM:SS.mmm` or `MM:SS.mmm` in ms.
fn read_ts(s: &[u8]) -> Option<i64> {
    let long = (|| {
        let mut sc = Scan::new(s);
        let h = sc.uint(0)? as u32 as i32;
        sc.lit(b':')?;
        let m = sc.uint(0)? as u32 as i32;
        sc.lit(b':')?;
        let sec = sc.uint(0)? as u32 as i32;
        sc.lit(b'.')?;
        let ms = sc.uint(0)? as u32 as i32;
        Some((i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(sec)) * 1000 + i64::from(ms))
    })();
    long.or_else(|| {
        let mut sc = Scan::new(s);
        let m = sc.uint(0)? as u32 as i32;
        sc.lit(b':')?;
        let sec = sc.uint(0)? as u32 as i32;
        sc.lit(b'.')?;
        let ms = sc.uint(0)? as u32 as i32;
        Some((i64::from(m) * 60 + i64::from(sec)) * 1000 + i64::from(ms))
    })
}

/// `webvtt_read_header` over a whole decoded file. A cue block without
/// valid timing ends the file, as in FFmpeg.
pub(crate) fn demux_webvtt(text: &[u8]) -> SubtitleQueue {
    let mut q = SubtitleQueue::default();
    let mut reader = TextReader::new(text);
    let mut cue = Vec::new();
    loop {
        reader.read_text_chunk(&mut cue);
        if cue.is_empty() {
            break;
        }
        let pos = reader.pos();
        let p: &[u8] = &cue;
        if [&b"WEBVTT"[..], b"STYLE", b"REGION", b"NOTE"].iter().any(|h| p.starts_with(h)) {
            continue;
        }
        let first_line = &p[..crate::scan::strcspn(p, b"\r\n")];
        let mut i = 0;
        if !first_line.windows(3).any(|w| w == b"-->") {
            // A cue identifier line.
            i = first_line.len();
            if p.get(i) == Some(&b'\r') {
                i += 1;
            }
            if p.get(i) == Some(&b'\n') {
                i += 1;
            }
        }
        let Some(start) = read_ts(&p[i..]) else { break };
        let Some(arrow) = p[i..].windows(3).position(|w| w == b"-->") else { break };
        i += arrow + 3;
        while matches!(p.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        let Some(end) = read_ts(&p[i..]) else { break };
        // Cue settings run to the end of the timing line.
        i += crate::scan::strcspn(&p[i..], b"\n\r\t ");
        while matches!(p.get(i), Some(b' ' | b'\t')) {
            i += 1;
        }
        i += crate::scan::strcspn(&p[i..], b"\r\n");
        if p.get(i) == Some(&b'\r') {
            i += 1;
        }
        if p.get(i) == Some(&b'\n') {
            i += 1;
        }
        match q.insert(crate::scan::c_str(&p[i..]), false) {
            Some(event) => (event.pos, event.pts, event.duration) = (pos, start, end.wrapping_sub(start)),
            None => break,
        }
    }
    q
}

/// Opens a standalone WebVTT file.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "WebVTT")?;
    let time_base = TimeBase::new(1, 1000);
    let packets = demux_webvtt(&decode_text(&raw)).finalize(time_base);
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets,
    }))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

const TAG_REPLACE: [(&[u8], &[u8]); 8] = [
    (b"{", b"\\{{}"),
    (b"\\", b"\\\xe2\x81\xa0"),
    (b"&gt;", b">"),
    (b"&lt;", b"<"),
    (b"&lrm;", b"\xe2\x80\x8e"),
    (b"&rlm;", b"\xe2\x80\x8f"),
    (b"&amp;", b"&"),
    (b"&nbsp;", b"\\h"),
];

const VALID_TAGS: [(&[u8], &[u8]); 6] = [
    (b"i", b"{\\i1}"),
    (b"/i", b"{\\i0}"),
    (b"b", b"{\\b1}"),
    (b"/b", b"{\\b0}"),
    (b"u", b"{\\u1}"),
    (b"/u", b"{\\u0}"),
];

/// `parse_webvtt_timestamp`: ms, or `None`.
fn parse_timestamp(buf: &[u8]) -> Option<i64> {
    let long = (|| {
        let mut s = Scan::new(buf);
        let h = s.int(0)? as i32;
        s.lit(b':')?;
        let m = s.int(2)? as i32;
        s.lit(b':')?;
        let sec = s.int(2)? as i32;
        s.lit(b'.')?;
        let ms = s.int(3)? as i32;
        Some((h, m, sec, ms))
    })();
    if let Some((h, m, sec, ms)) = long {
        return (m <= 59 && sec <= 59).then(|| i64::from(h) * 3_600_000 + i64::from(m) * 60_000 + i64::from(sec) * 1000 + i64::from(ms));
    }
    let mut s = Scan::new(buf);
    let m = s.int(2)? as i32;
    s.lit(b':')?;
    let sec = s.int(2)? as i32;
    s.lit(b'.')?;
    let ms = s.int(3)? as i32;
    (m <= 59 && sec <= 59).then(|| i64::from(m) * 60_000 + i64::from(sec) * 1000 + i64::from(ms))
}

/// `read_cue_timestamp`.
fn cue_timestamp(body: &[u8], len: usize, cue_start: i64, cue_end: i64, prev_ts: i64) -> Option<i64> {
    if len < 1 || !body[0].is_ascii_digit() {
        return None;
    }
    if body.iter().take_while(|b| b"0123456789:.".contains(b)).count() != len {
        return None;
    }
    let ts = parse_timestamp(body)?;
    if ts <= cue_start || ts >= cue_end || (prev_ts >= 0 && ts <= prev_ts) {
        return None;
    }
    Some(ts)
}

fn flush_segment(out: &mut Vec<u8>, seg: &mut Vec<u8>, dur_cs: i64) {
    if dur_cs > 0 {
        out.extend_from_slice(format!("{{\\kf{dur_cs}}}").as_bytes());
    }
    out.append(seg);
}

/// `webvtt_event_to_ass` for a cue shown from `cue_start_ms` to
/// `cue_end_ms`.
pub fn webvtt_to_ass(p: &[u8], cue_start_ms: i64, cue_end_ms: i64) -> Vec<u8> {
    let p = crate::scan::c_str(p);
    let mut out = Vec::with_capacity(p.len() + 16);
    let mut seg = Vec::with_capacity(p.len() + 16);
    let mut prev_ts = -1i64;
    let mut start_cs = 0i64;
    let mut i = 0usize;
    while i < p.len() {
        let mut again = false;
        if p[i] == b'<' {
            let Some(close) = p[i..].iter().position(|&b| b == b'>') else { break };
            let len = close + 1;
            if len > 2 {
                if let Some(ts) = cue_timestamp(&p[i + 1..], len - 2, cue_start_ms, cue_end_ms, prev_ts) {
                    // Container times reach the i64 edges; FFmpeg's sums are
                    // kept, saturated rather than overflowing.
                    let end_cs = ts.saturating_sub(cue_start_ms).saturating_add(5) / 10;
                    flush_segment(&mut out, &mut seg, end_cs.saturating_sub(start_cs));
                    start_cs = end_cs;
                    prev_ts = ts;
                    i += len;
                    continue;
                }
            }
            if let Some((_, to)) = VALID_TAGS.iter().find(|(from, _)| p[i + 1..].starts_with(from)) {
                seg.extend_from_slice(to);
            }
            i += len;
            again = true;
        }
        if let Some((from, to)) = TAG_REPLACE.iter().find(|(from, _)| p[i..].starts_with(from)) {
            seg.extend_from_slice(to);
            i += from.len();
            again = true;
        }
        if again {
            continue;
        }
        if p[i] == b'\n' && i + 1 < p.len() {
            seg.extend_from_slice(b"\\N");
        } else if p[i] != b'\r' {
            seg.push(p[i]);
        }
        i += 1;
    }
    let final_cs = if prev_ts < 0 {
        0
    } else {
        (cue_end_ms.saturating_sub(cue_start_ms).saturating_add(5) / 10).saturating_sub(start_cs)
    };
    flush_segment(&mut out, &mut seg, final_cs);
    out
}

struct WebVtt;

impl EventSource for WebVtt {
    fn event(&mut self, packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>> {
        let ms = TimeBase::new(1, 1000);
        let (start, end) = match packet.pts {
            Some(pts) => {
                let start = packet.time_base.rescale(pts, ms);
                (start, start.saturating_add(packet.time_base.rescale(packet.duration.unwrap_or(0), ms)))
            }
            None => (0, 0),
        };
        Ok(Some(AssEvent::default_style(webvtt_to_ass(text, start, end))))
    }
}

/// The `webvtt` decoder: cue text, timing from the packet.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!("not a WebVTT codec id: {}", params.codec_id)));
    }
    Ok(Box::new(AssEventDecoder::new(params.codec_id.clone(), ffmpeg_default_header(), WebVtt)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::{Frame, Segment};

    #[test]
    fn cue_text_converts_like_ffmpeg() {
        let ass = |s: &str| String::from_utf8(webvtt_to_ass(s.as_bytes(), 1000, 3000)).unwrap();
        assert_eq!(ass("<v Roger>Hi <b>x</b> &amp; {c}"), r"Hi {\b1}x{\b0} & \{{}c}");
        // A final newline stays a literal newline, as in FFmpeg.
        assert_eq!(ass("a\nb\n"), "a\\Nb\n");
        assert_eq!(ass("one <00:00:02.000>two"), r"{\kf100}one {\kf100}two");
        assert_eq!(ass("cut <here"), "cut ");
    }

    #[test]
    fn blocks_become_cues_with_identifier_and_settings_dropped() {
        let file = b"WEBVTT\n\nNOTE x\n\n123\n00:01.000 --> 00:02.500 align:end\nhello\nworld\n\n00:00:03.000 --> 00:00:04.000\n\nbad --> timing\n\n00:05.000 --> 00:06.000\nlost\n";
        let packets: Vec<_> = demux_webvtt(file).finalize(TimeBase::new(1, 1000)).into_iter()
            .map(|p| (p.pts.unwrap(), p.duration.unwrap(), String::from_utf8(p.data).unwrap())).collect();
        assert_eq!(packets, vec![(1000, 1500, "hello\nworld".to_string()), (3000, 1000, String::new())]);
    }

    #[test]
    fn container_packets_keep_container_timing() {
        let mut decoder = make_decoder(&CodecParameters::subtitle(CodecId::new(CODEC_ID))).unwrap();
        decoder.send_packet(&Packet::new(0, TimeBase::new(1, 1000), b"<b>Hello</b>\nworld".to_vec()).with_pts(1500).with_duration(750)).unwrap();
        let Frame::Subtitle(cue) = decoder.receive_frame().unwrap() else { panic!() };
        assert_eq!((cue.start_us, cue.end_us), (1_500_000, 2_250_000));
        let expected = [Segment::Bold(vec![Segment::Text("Hello".into())]), Segment::LineBreak, Segment::Text("world".into())];
        assert_eq!(format!("{:?}", cue.segments), format!("{expected:?}"));
        decoder.flush().unwrap();
        assert!(matches!(decoder.receive_frame(), Err(Error::Eof)));
        decoder.reset().unwrap();
        assert!(matches!(decoder.receive_frame(), Err(Error::NeedMore)));
    }
}

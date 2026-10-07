//! SubViewer (version 2): standalone `.sub` demuxer and decoder.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! headers verified):
//! - `libavformat/subviewerdec.c` — probe, `[INFORMATION]` header, event
//!   style lines (`[COLF]`, `[SIZE]`, `[FONT]`, `[STYLE]`) ignored, one
//!   event per timing line with its text lines joined;
//! - `libavcodec/subviewerdec.c` — `[br]` and line breaks to ASS.
//!
//! The container and codec keep the ids the replaced OxideAV
//! implementation registered (`subviewer2`); FFmpeg names both `subviewer`.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
};

use crate::ass_text::{AssEvent, AssEventDecoder, EventSource, ffmpeg_default_header};
use crate::scan::Scan;
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const CODEC_ID: &str = "subviewer2";
pub const CONTAINER_NAME: &str = "subviewer2";

/// `subviewer_probe`: a timing line first (FFmpeg's extension score) or an
/// `[INFORMATION]` header (a third of the maximum).
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    let mut s = Scan::new(&text);
    let timing = (|| {
        for sep in [b':', b':', b'.', b',', b':', b':', b'.'] {
            s.uint(0)?;
            s.lit(sep)?;
        }
        s.uint(0)?;
        s.any()
    })();
    if timing.is_some() {
        50
    } else if text.starts_with(b"[INFORMATION]") {
        33
    } else {
        0
    }
}

/// `read_ts`: `(start_ms, duration_ms)`; the fraction has 1–3 digits.
fn read_ts(line: &[u8]) -> Option<(i64, i64)> {
    let mut s = Scan::new(line);
    let stamp = |s: &mut Scan| -> Option<i64> {
        let h = s.uint(0)? as u32 as i32;
        s.lit(b':')?;
        let m = s.uint(0)? as u32 as i32;
        s.lit(b':')?;
        let sec = s.uint(0)? as u32 as i32;
        s.lit(b'.')?;
        let before = s.pos();
        let ms = s.uint(0)? as u32 as i32;
        let multiplier = match s.pos() - before {
            1 => 100,
            2 => 10,
            3 => 1,
            _ => return None,
        };
        Some((i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(sec)) * 1000 + i64::from(ms.wrapping_mul(multiplier)))
    };
    let start = stamp(&mut s)?;
    s.lit(b',')?;
    let end = stamp(&mut s)?;
    // `int duration`: the difference wraps to 32 bits.
    Some((start, i64::from(end.wrapping_sub(start) as i32)))
}

/// `subviewer_read_header` over a whole decoded file: the header and the
/// events, or `InvalidData` for text before the first timing line.
pub(crate) fn demux_subviewer(text: &[u8]) -> Result<(Vec<u8>, SubtitleQueue)> {
    let mut q = SubtitleQueue::default();
    let mut header: Vec<u8> = Vec::new();
    let mut header_done = false;
    let mut reader = TextReader::new(text);
    let mut buf = Vec::new();
    let mut new_event = true;
    let mut timing: Option<(i64, i64)> = None;
    let mut pos = 0;
    while !reader.eof() {
        let len = reader.get_line(2048, &mut buf);
        if len == 0 {
            break;
        }
        let line = &buf[..crate::scan::strcspn(&buf, b"\r\n")];
        if line.first() == Some(&b'[') && !line.starts_with(b"[br]") {
            let has = |tag: &[u8]| line.windows(tag.len()).any(|w| w == tag);
            if has(b"[COLF]") || has(b"[SIZE]") || has(b"[FONT]") || has(b"[STYLE]") {
                continue;
            }
            if !header_done {
                header.extend_from_slice(line);
                header.push(b'\n');
                if line.starts_with(b"[END INFORMATION]") || line.starts_with(b"[SUBTITLE]") {
                    header_done = true;
                }
            }
        } else if let Some(ts) = read_ts(line) {
            timing = Some(ts);
            new_event = true;
            pos = reader.pos();
        } else if !line.is_empty() {
            let Some((pts, duration)) = timing else {
                return Err(Error::invalid("SubViewer text before the first timing line"));
            };
            if !new_event && q.insert(b"\n", true).is_none() {
                break;
            }
            match q.insert(line, !new_event) {
                Some(event) => {
                    if new_event {
                        (event.pos, event.pts, event.duration) = (pos, pts, duration);
                    }
                }
                None => break,
            }
            new_event = false;
        }
    }
    Ok((if header_done { header } else { Vec::new() }, q))
}

/// Opens a standalone SubViewer file.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "SubViewer")?;
    let time_base = TimeBase::new(1, 1000);
    let (header, queue) = demux_subviewer(&decode_text(&raw))?;
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    params.extradata = header;
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets: queue.finalize(time_base),
    }))
}

/// `subviewer_event_to_ass`.
pub fn subviewer_to_ass(p: &[u8]) -> Vec<u8> {
    let p = crate::scan::c_str(p);
    let mut out = Vec::with_capacity(p.len() + 8);
    let mut i = 0;
    while i < p.len() {
        if p[i..].starts_with(b"[br]") {
            out.extend_from_slice(b"\\N");
            i += 4;
        } else {
            if p[i] == b'\n' && i + 1 < p.len() {
                out.extend_from_slice(b"\\N");
            } else if p[i] != b'\n' && p[i] != b'\r' {
                out.push(p[i]);
            }
            i += 1;
        }
    }
    out
}

struct SubViewer;

impl EventSource for SubViewer {
    fn event(&mut self, _packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>> {
        Ok(Some(AssEvent::default_style(subviewer_to_ass(text))))
    }
}

/// The SubViewer decoder.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!("not a SubViewer codec id: {}", params.codec_id)));
    }
    Ok(Box::new(AssEventDecoder::new(params.codec_id.clone(), ffmpeg_default_header(), SubViewer)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_join_lines_and_skip_style_lines() {
        let file = b"[INFORMATION]\n[TITLE] t\n[END INFORMATION]\n[SUBTITLE]\n[COLF]&HFFFFFF,[SIZE]12\n00:00:01.5,00:00:02.25\nA\nB\n\n00:00:03.00,00:00:04.00\n[br]C\n[SIZE]20\n";
        let (header, q) = demux_subviewer(file).unwrap();
        assert_eq!(header, b"[INFORMATION]\n[TITLE] t\n[END INFORMATION]\n");
        let packets: Vec<_> = q.finalize(TimeBase::new(1, 1000)).into_iter()
            .map(|p| (p.pts.unwrap(), p.duration.unwrap(), String::from_utf8(p.data).unwrap())).collect();
        assert_eq!(packets, vec![(1500, 750, "A\nB".to_string()), (3000, 1000, "[br]C".to_string())]);
        assert_eq!(subviewer_to_ass(b"[br]foo\nbar\n"), br"\Nfoo\Nbar");
        assert!(demux_subviewer(b"text first\n00:00:01.00,00:00:02.00\nx\n").is_err());
    }
}

//! VPlayer subtitle demuxer & decoder.
//!
//! Ported to safe Rust from FFmpeg's:
//! - `libavformat/vplayerdec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//! - `libavcodec/textdec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//!
//! VPlayer structure:
//! - Container: one cue per line, prefixed by `HH:MM:SS` or `HH:MM:SS.CS` timestamp,
//!   followed by a separator in `[':', ' ', '=']` and the cue text.
//! - Codec: `|` characters represent line breaks.

use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, Frame, MediaType, Packet,
    ProbeData, ProbeScore, ReadSeek, Result, Segment, StreamInfo, SubtitleCue, TimeBase,
    MAX_PROBE_SCORE,
};

use crate::text_common::{decode_subtitle_text, TextSubtitleDemuxer, MAX_CUES, MAX_FILE_BYTES};

pub const CODEC_ID: &str = "vplayer";
pub const CONTAINER_NAME: &str = "vplayer";

/// Maximum length of a single subtitle cue packet in bytes (1 MiB).
const MAX_CUE_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// Probe for VPlayer: checks if line starts with `HH:MM:SS` or `H:MM:SS` followed by separator.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let s = String::from_utf8_lossy(data.buf);
    for line in s.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if parse_vplayer_line(trimmed).is_some() {
            return MAX_PROBE_SCORE;
        }
        break;
    }
    0
}

/// Open a VPlayer file as a demuxer.
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
        return Err(Error::invalid("VPlayer file exceeds maximum size"));
    }

    let text = decode_subtitle_text(&raw);
    let packets = demux_vplayer_text(&text)?;

    let time_base = TimeBase::new(1, 100); // centiseconds (10ms)
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

struct RawVpSub {
    pts: i64,
    duration: i64,
    data: String,
}

fn demux_vplayer_text(text: &str) -> Result<VecDeque<Packet>> {
    let mut raw_subs: Vec<RawVpSub> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.trim().is_empty() {
            continue;
        }
        if let Some((pts_start, body)) = parse_vplayer_line(trimmed) {
            if raw_subs.len() >= MAX_CUES {
                break;
            }
            raw_subs.push(RawVpSub {
                pts: pts_start,
                duration: -1,
                data: body.to_string(),
            });
        }
    }

    // Finalize durations
    let len = raw_subs.len();
    for idx in 0..len {
        if raw_subs[idx].duration < 0 && idx + 1 < len {
            raw_subs[idx].duration = raw_subs[idx + 1].pts - raw_subs[idx].pts;
        }
    }

    let time_base = TimeBase::new(1, 100); // 10ms centiseconds
    let mut packets = VecDeque::with_capacity(raw_subs.len());
    for s in raw_subs {
        let mut pkt = Packet::new(0, time_base, s.data.into_bytes());
        pkt.pts = Some(s.pts);
        pkt.dts = Some(s.pts);
        pkt.duration = Some(s.duration);
        pkt.flags.keyframe = true;
        packets.push_back(pkt);
    }

    Ok(packets)
}

/// Parse a VPlayer line into `(pts_centiseconds, body)`.
/// Format: `hh:mm:ss[.cs][: =]body`
fn parse_vplayer_line(line: &str) -> Option<(i64, &str)> {
    let bytes = line.as_bytes();
    // Look for separator char in [':', ' ', '='] that marks the end of the timestamp
    // Timestamp must have at least 2 colons: `hh:mm:ss`
    let mut colon_indices = [0usize; 2];
    let mut colon_count = 0;
    let mut sep_idx = None;

    for (i, &b) in bytes.iter().enumerate() {
        if b == b':' {
            if colon_count < 2 {
                colon_indices[colon_count] = i;
                colon_count += 1;
            } else if sep_idx.is_none() {
                sep_idx = Some(i);
                break;
            }
        } else if colon_count == 2 && sep_idx.is_none() && (b == b' ' || b == b'=') {
            sep_idx = Some(i);
            break;
        }
    }

    let end_ts = sep_idx?;
    let ts_str = &line[..end_ts];
    let body = &line[end_ts + 1..];

    let (hms, cs_str) = match ts_str.find('.') {
        Some(dot) => (&ts_str[..dot], Some(&ts_str[dot + 1..])),
        None => (ts_str, None),
    };

    let parts: Vec<&str> = hms.split(':').collect();
    if parts.len() != 3 {
        return None;
    }
    let hh = parts[0].trim().parse::<i64>().ok()?;
    let mm = parts[1].trim().parse::<i64>().ok()?;
    let ss = parts[2].trim().parse::<i64>().ok()?;

    let cs = if let Some(cs_part) = cs_str {
        cs_part.trim().parse::<i64>().ok()?
    } else {
        0
    };

    let pts_centis = (hh * 3600 + mm * 60 + ss) * 100 + cs;
    Some((pts_centis, body))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Create a new VPlayer decoder instance.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!(
            "not a vplayer codec id: {}",
            params.codec_id
        )));
    }
    Ok(Box::new(VPlayerDecoder {
        codec_id: params.codec_id.clone(),
        pending: VecDeque::new(),
        eof: false,
    }))
}

pub struct VPlayerDecoder {
    codec_id: CodecId,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for VPlayerDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("VPlayer packet exceeds maximum size"));
        }
        let text = decode_subtitle_text(&packet.data);
        let segments = text_to_segments(&text);

        let start_us = packet
            .pts
            .map(|pts| packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000)))
            .unwrap_or(0);

        let end_us = crate::text_common::subtitle_end_us(packet, start_us);

        self.pending.push_back(Frame::Subtitle(SubtitleCue {
            start_us,
            end_us,
            style_ref: None,
            positioning: None,
            segments,
        }));
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

/// Convert VPlayer text to Segments, treating `|` as a line break.
fn text_to_segments(text: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let parts: Vec<&str> = text.split('|').collect();
    for (i, part) in parts.iter().enumerate() {
        if i > 0 {
            segments.push(Segment::LineBreak);
        }
        if !part.is_empty() {
            segments.push(Segment::Text(part.to_string()));
        }
    }
    segments
}

//! SubViewer v1 subtitle demuxer & decoder.
//!
//! Ported to safe Rust from FFmpeg's:
//! - `libavformat/subviewer1dec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//! - `libavformat/subtitles.c` (same commit/license; queue ordering and duplicates)
//! - `libavcodec/textdec.c` (commit 2da55bf, LGPL-2.1-or-later — header verified)
//!
//! SubViewer 1 structure:
//! - Container: `[DELAY]` header followed by timestamped blocks `[HH:MM:SS]`.
//!   An empty line following a timestamp ends the previous cue.
//! - Codec: `|` characters represent line breaks.

use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, Frame, MediaType, Packet,
    ProbeData, ProbeScore, ReadSeek, Result, Segment, StreamInfo, SubtitleCue, TimeBase,
};

use crate::text_common::{decode_subtitle_text, TextSubtitleDemuxer, MAX_CUES, MAX_FILE_BYTES};

pub const CODEC_ID: &str = "subviewer1";
pub const CONTAINER_NAME: &str = "subviewer1";

/// Maximum length of a single subtitle cue packet in bytes (1 MiB).
const MAX_CUE_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// Demuxer
// ---------------------------------------------------------------------------

/// `subviewer1_probe`: the start-of-script marker anywhere in the probe
/// buffer (up to its first NUL), at FFmpeg's extension score. SubViewer 2
/// headers also carry `[DELAY]`, so that alone does not identify version 1.
pub fn probe(data: &ProbeData) -> ProbeScore {
    const MARKER: &[u8] = b"******** START SCRIPT ********";
    let head = &data.buf[..data.buf.iter().position(|&b| b == 0).unwrap_or(data.buf.len())];
    if head.windows(MARKER.len()).any(|w| w == MARKER) {
        50
    } else {
        0
    }
}

/// Open a SubViewer 1 file as a demuxer.
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
        return Err(Error::invalid("SubViewer1 file exceeds maximum size"));
    }

    let text = decode_subtitle_text(&raw);
    let packets = demux_subviewer1_text(&text)?;

    let time_base = TimeBase::new(1, 1); // SubViewer 1 timestamps are whole seconds.
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

struct RawSub {
    pts: i64,
    order: usize,
    duration: i64,
    data: String,
}

fn demux_subviewer1_text(text: &str) -> Result<VecDeque<Packet>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut delay: i64 = 0;
    let mut raw_subs: Vec<RawSub> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("[DELAY]") {
            i += 1;
            if i < lines.len() {
                if let Ok(d) = lines[i].trim().parse::<i32>() {
                    delay = i64::from(d);
                }
            }
            i += 1;
            continue;
        }

        if let Some((hh, mm, ss)) = parse_timestamp_tag(line) {
            let pts_start = hh * 3600 + mm * 60 + ss + delay;
            i += 1;
            if i < lines.len() {
                let sub_line = lines[i].trim_end_matches(['\r', '\n']);
                if sub_line.trim().is_empty() {
                    // Empty line closes the previous cue
                    if let Some(last) = raw_subs.last_mut() {
                        if last.duration < 0 {
                            last.duration = pts_start - last.pts;
                        }
                    }
                } else {
                    if raw_subs.len() >= MAX_CUES {
                        break;
                    }
                    raw_subs.push(RawSub {
                        pts: pts_start,
                        order: i,
                        duration: -1,
                        data: sub_line.to_string(),
                    });
                }
            }
        }
        i += 1;
    }

    raw_subs.sort_unstable_by_key(|s| (s.pts, s.order));

    // Finalize durations
    let len = raw_subs.len();
    for idx in 0..len {
        if raw_subs[idx].duration < 0 && idx + 1 < len {
            raw_subs[idx].duration = raw_subs[idx + 1].pts - raw_subs[idx].pts;
        }
    }
    raw_subs.dedup_by(|a, b| a.pts == b.pts && a.duration == b.duration && a.data == b.data);

    let time_base = TimeBase::new(1, 1);
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

fn parse_timestamp_tag(s: &str) -> Option<(i64, i64, i64)> {
    if !s.starts_with('[') || !s.ends_with(']') {
        return None;
    }
    let inner = &s[1..s.len() - 1];
    let mut parts = inner.split(':');
    let hh = i64::from(parts.next()?.trim().parse::<i32>().ok()?);
    let mm = i64::from(parts.next()?.trim().parse::<i32>().ok()?);
    let ss = i64::from(parts.next()?.trim().parse::<i32>().ok()?);
    if parts.next().is_some() {
        return None;
    }
    Some((hh, mm, ss))
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Create a new SubViewer 1 decoder instance.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!(
            "not a subviewer1 codec id: {}",
            params.codec_id
        )));
    }
    Ok(Box::new(SubViewer1Decoder {
        codec_id: params.codec_id.clone(),
        pending: VecDeque::new(),
        eof: false,
    }))
}

pub struct SubViewer1Decoder {
    codec_id: CodecId,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for SubViewer1Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("SubViewer1 packet exceeds maximum size"));
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

/// Convert SubViewer1 text to Segments, treating `|` as a line break.
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

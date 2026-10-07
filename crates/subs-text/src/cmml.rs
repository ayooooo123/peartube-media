//! Continuous Media Markup Language (`cmml`) decoder.
//!
//! Clean-room implementation from the Xiph CMML specification
//! (<https://wiki.xiph.org/CMML>).
//!
//! In Ogg logical streams, CMML travels under the BOS packet magic
//! `CMML\0\0\0\0`. Content packets carry `<clip>` elements specifying
//! temporal intervals with titles, descriptions, and hyperlinks.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    Segment, SubtitleCue, TimeBase,
};

use crate::xml::{decode_entities, Scanner, Token};
use crate::CMML_CODEC_ID;

const MAX_CUE_BYTES: usize = 1 << 20;

#[derive(Default)]
pub struct CmmlContext {
    pub granulerate_num: u64,
    pub granulerate_den: u64,
    pub granuleshift: u8,
}

impl CmmlContext {
    pub fn parse_ident(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 29 {
            return Err(Error::invalid("CMML ident packet too short"));
        }
        if &data[0..8] != b"CMML\x00\x00\x00\x00" {
            return Err(Error::invalid("invalid CMML magic"));
        }
        self.granulerate_num = u64::from_be_bytes(data[12..20].try_into().unwrap());
        self.granulerate_den = u64::from_be_bytes(data[20..28].try_into().unwrap());
        self.granuleshift = data[28];
        Ok(())
    }
}

/// Parse NPT (Normal Play Time) format: `npt:HH:MM:SS.mmm`, `MM:SS.mmm`, `SS.mmm`, or plain seconds.
pub fn parse_npt_time(s: &str) -> Option<i64> {
    let s = s.trim().trim_start_matches("npt:");
    if s.is_empty() || s.eq_ignore_ascii_case("now") {
        return Some(0);
    }
    if s.contains(':') {
        let parts: Vec<&str> = s.split(':').collect();
        match parts.len() {
            2 => {
                let mins: f64 = parts[0].parse().ok()?;
                let secs: f64 = parts[1].parse().ok()?;
                Some(((mins * 60.0 + secs) * 1_000_000.0) as i64)
            }
            3 => {
                let hours: f64 = parts[0].parse().ok()?;
                let mins: f64 = parts[1].parse().ok()?;
                let secs: f64 = parts[2].parse().ok()?;
                Some((((hours * 60.0 + mins) * 60.0 + secs) * 1_000_000.0) as i64)
            }
            _ => None,
        }
    } else {
        let secs: f64 = s.parse().ok()?;
        Some((secs * 1_000_000.0) as i64)
    }
}

/// Parse CMML payload into SubtitleCue events.
pub fn decode_cmml_payload(
    _ctx: &mut CmmlContext,
    data: &[u8],
    packet: &Packet,
) -> Result<Vec<SubtitleCue>> {
    let packet_start = packet.pts.map(|pts| {
        packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000))
    });
    let packet_end = match (packet_start, packet.duration) {
        (Some(start), Some(dur)) if dur > 0 => {
            Some(start.saturating_add(packet.time_base.rescale(dur, TimeBase::new(1, 1_000_000))))
        }
        _ => None,
    };

    struct ClipBuilder {
        start_us: Option<i64>,
        end_us: Option<i64>,
        title: Option<String>,
        desc: Option<String>,
        link_text: Option<String>,
        raw_text: Vec<String>,
        in_title: bool,
        in_desc: bool,
        in_a: bool,
    }

    impl ClipBuilder {
        fn new() -> Self {
            Self {
                start_us: None,
                end_us: None,
                title: None,
                desc: None,
                link_text: None,
                raw_text: Vec::new(),
                in_title: false,
                in_desc: false,
                in_a: false,
            }
        }

        fn finish(self, fallback_start: Option<i64>, fallback_end: Option<i64>) -> Option<SubtitleCue> {
            let start_us = self.start_us.or(fallback_start).unwrap_or(0);
            let end_us = self.end_us.or(fallback_end).unwrap_or(start_us);

            let mut segments = Vec::new();
            match (self.title, self.desc) {
                (Some(t), Some(d)) if !t.is_empty() && !d.is_empty() => {
                    segments.push(Segment::Text(t));
                    segments.push(Segment::LineBreak);
                    segments.push(Segment::Text(d));
                }
                (Some(t), _) if !t.is_empty() => {
                    segments.push(Segment::Text(t));
                }
                (_, Some(d)) if !d.is_empty() => {
                    segments.push(Segment::Text(d));
                }
                _ => {
                    if let Some(a) = self.link_text {
                        if !a.is_empty() {
                            segments.push(Segment::Text(a));
                        }
                    } else if !self.raw_text.is_empty() {
                        let text = self.raw_text.join(" ");
                        let clean = text.trim();
                        if !clean.is_empty() {
                            segments.push(Segment::Text(clean.to_string()));
                        }
                    }
                }
            }

            if segments.is_empty() {
                return None;
            }

            Some(SubtitleCue {
                start_us,
                end_us,
                style_ref: None,
                positioning: None,
                segments,
            })
        }
    }

    let mut scanner = Scanner::new(data);
    let mut cues = Vec::new();
    let mut current_clip: Option<ClipBuilder> = None;

    while let Some(tok) = scanner.next()? {
        match tok {
            Token::Start { name, attrs, self_closing } => {
                if name.eq_ignore_ascii_case("clip") {
                    let mut clip = ClipBuilder::new();
                    for (k, v) in attrs {
                        if k.eq_ignore_ascii_case("start") {
                            clip.start_us = parse_npt_time(v);
                        } else if k.eq_ignore_ascii_case("end") {
                            clip.end_us = parse_npt_time(v);
                        } else if k.eq_ignore_ascii_case("title") {
                            clip.title = Some(v.to_string());
                        }
                    }
                    if self_closing {
                        if let Some(cue) = clip.finish(packet_start, packet_end) {
                            cues.push(cue);
                        }
                    } else {
                        current_clip = Some(clip);
                    }
                } else if let Some(clip) = &mut current_clip {
                    if name.eq_ignore_ascii_case("title") {
                        clip.in_title = true;
                    } else if name.eq_ignore_ascii_case("desc") {
                        clip.in_desc = true;
                    } else if name.eq_ignore_ascii_case("a") {
                        clip.in_a = true;
                    }
                }
            }
            Token::End(name) => {
                if name.eq_ignore_ascii_case("clip") {
                    if let Some(clip) = current_clip.take() {
                        if let Some(cue) = clip.finish(packet_start, packet_end) {
                            cues.push(cue);
                        }
                    }
                } else if let Some(clip) = &mut current_clip {
                    if name.eq_ignore_ascii_case("title") {
                        clip.in_title = false;
                    } else if name.eq_ignore_ascii_case("desc") {
                        clip.in_desc = false;
                    } else if name.eq_ignore_ascii_case("a") {
                        clip.in_a = false;
                    }
                }
            }
            Token::Text(s) => {
                let clean = decode_entities(&s);
                if let Some(clip) = &mut current_clip {
                    if clip.in_title {
                        let t = clip.title.get_or_insert_with(String::new);
                        t.push_str(&clean);
                    } else if clip.in_desc {
                        let d = clip.desc.get_or_insert_with(String::new);
                        d.push_str(&clean);
                    } else if clip.in_a {
                        let a = clip.link_text.get_or_insert_with(String::new);
                        a.push_str(&clean);
                    } else if !clean.trim().is_empty() {
                        clip.raw_text.push(clean);
                    }
                }
            }
        }
    }

    if let Some(clip) = current_clip.take() {
        if let Some(cue) = clip.finish(packet_start, packet_end) {
            cues.push(cue);
        }
    }

    Ok(cues)
}

pub struct CmmlDecoder {
    codec_id: CodecId,
    ctx: CmmlContext,
    pending: VecDeque<Frame>,
    eof: bool,
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CMML_CODEC_ID {
        return Err(Error::unsupported(format!("not a cmml codec id: {}", params.codec_id)));
    }
    let mut ctx = CmmlContext::default();
    if !params.extradata.is_empty() && params.extradata.starts_with(b"CMML\x00\x00\x00\x00") {
        let _ = ctx.parse_ident(&params.extradata);
    }
    Ok(Box::new(CmmlDecoder {
        codec_id: params.codec_id.clone(),
        ctx,
        pending: VecDeque::new(),
        eof: false,
    }))
}

impl Decoder for CmmlDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("CMML packet too large"));
        }
        if packet.data.is_empty() {
            return Ok(());
        }

        // Header packet identification
        if packet.data.starts_with(b"CMML\x00\x00\x00\x00") {
            let _ = self.ctx.parse_ident(&packet.data);
            return Ok(());
        }
        if packet.data.starts_with(b"<?") || packet.data.starts_with(b"<head") {
            return Ok(());
        }

        let cues = decode_cmml_payload(&mut self.ctx, &packet.data, packet)?;
        for cue in cues {
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

    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.eof = false;
        Ok(())
    }
}

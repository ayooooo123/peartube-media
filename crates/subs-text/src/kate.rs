//! Kate (`kate`) subtitle and overlay decoder.
//!
//! Clean-room implementation from the Xiph OggKate specification
//! (<https://wiki.xiph.org/OggKate>) and libkate bitstream documentation.
//!
//! In Ogg logical streams, Kate starts with BOS packet magic
//! `\x80kate\0\0\0`. In Matroska containers it travels under CodecID `S_KATE`.
//! Text event packets carry start and duration granule offsets along with
//! styled Unicode text.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    Segment, SubtitleCue, TimeBase,
};

use crate::bitpack::BitReader;
use crate::xml::{decode_entities, Scanner, Token};
use crate::KATE_CODEC_ID;

const MAX_CUE_BYTES: usize = 1 << 20;

#[derive(Clone, Debug)]
pub struct KateContext {
    pub gps_numerator: u32,
    pub gps_denominator: u32,
    pub granule_shift: u8,
    pub canvas_width: usize,
    pub canvas_height: usize,
    pub language: String,
    pub category: String,
    pub num_headers: u8,
    pub headers_seen: u8,
}

impl Default for KateContext {
    fn default() -> Self {
        Self {
            gps_numerator: 1000,
            gps_denominator: 1,
            granule_shift: 32,
            canvas_width: 0,
            canvas_height: 0,
            language: String::new(),
            category: String::new(),
            num_headers: 1,
            headers_seen: 0,
        }
    }
}

impl KateContext {
    pub fn parse_id_header(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 64 {
            return Err(Error::invalid("Kate ID header too short"));
        }
        if data[0] != 0x80 || &data[1..8] != b"kate\x00\x00\x00" {
            return Err(Error::invalid("invalid Kate ID header magic"));
        }

        self.num_headers = data[11];
        self.granule_shift = data[15];

        let cw_sh = (data[16] >> 4) as usize;
        let cw_base = ((data[16] & 0x0f) as usize) | ((data[17] as usize) << 4);
        self.canvas_width = cw_base << cw_sh;

        let ch_sh = (data[18] >> 4) as usize;
        let ch_base = ((data[18] & 0x0f) as usize) | ((data[19] as usize) << 4);
        self.canvas_height = ch_base << ch_sh;

        let num = u32::from_le_bytes(data[24..28].try_into().unwrap());
        let den = u32::from_le_bytes(data[28..32].try_into().unwrap());
        if num > 0 && den > 0 {
            self.gps_numerator = num;
            self.gps_denominator = den;
        }

        let lang_end = data[32..48]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(16);
        self.language = String::from_utf8_lossy(&data[32..32 + lang_end]).into_owned();

        let cat_end = data[48..64]
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(16);
        self.category = String::from_utf8_lossy(&data[48..48 + cat_end]).into_owned();

        self.headers_seen = 1;
        Ok(())
    }

    pub fn granule_to_us(&self, granule: u64) -> i64 {
        if self.gps_numerator == 0 {
            return 0;
        }
        ((granule as i128 * self.gps_denominator as i128 * 1_000_000)
            / self.gps_numerator as i128) as i64
    }
}

/// Parse text into segments, handling `\n`, `|`, and inline markup.
pub fn parse_kate_text(raw_text: &str) -> Vec<Segment> {
    if raw_text.contains('<') && raw_text.contains('>') {
        // Attempt lightweight XML scanning for inline markup
        if let Ok(mut scanner) = std::panic::catch_unwind(|| Scanner::new(raw_text.as_bytes())) {
            let mut stack: Vec<(bool, bool, bool, Vec<Segment>)> = vec![(false, false, false, Vec::new())];
            let mut has_markup = false;

            while let Ok(Some(tok)) = scanner.next() {
                match tok {
                    Token::Start { name, self_closing, .. } => {
                        has_markup = true;
                        if name.eq_ignore_ascii_case("br") {
                            if let Some((_, _, _, children)) = stack.last_mut() {
                                children.push(Segment::LineBreak);
                            }
                        } else if !self_closing {
                            let bold = name.eq_ignore_ascii_case("b");
                            let italic = name.eq_ignore_ascii_case("i");
                            let underline = name.eq_ignore_ascii_case("u");
                            stack.push((bold, italic, underline, Vec::new()));
                        }
                    }
                    Token::End(name) => {
                        if stack.len() > 1 {
                            let (bold, italic, underline, children) = stack.pop().unwrap();
                            let mut segs = children;
                            if bold {
                                segs = vec![Segment::Bold(segs)];
                            } else if italic {
                                segs = vec![Segment::Italic(segs)];
                            } else if underline {
                                segs = vec![Segment::Underline(segs)];
                            }
                            if let Some((_, _, _, parent)) = stack.last_mut() {
                                parent.extend(segs);
                            }
                        }
                        let _ = name;
                    }
                    Token::Text(s) => {
                        let decoded = decode_entities(&s);
                        let sub_segs = parse_plain_lines(&decoded);
                        if let Some((_, _, _, children)) = stack.last_mut() {
                            children.extend(sub_segs);
                        }
                    }
                }
            }

            while stack.len() > 1 {
                let (bold, italic, underline, children) = stack.pop().unwrap();
                let mut segs = children;
                if bold {
                    segs = vec![Segment::Bold(segs)];
                } else if italic {
                    segs = vec![Segment::Italic(segs)];
                } else if underline {
                    segs = vec![Segment::Underline(segs)];
                }
                if let Some((_, _, _, parent)) = stack.last_mut() {
                    parent.extend(segs);
                }
            }

            if has_markup {
                let (_, _, _, res) = stack.pop().unwrap();
                if !res.is_empty() {
                    return res;
                }
            }
        }
    }

    parse_plain_lines(raw_text)
}

fn parse_plain_lines(text: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let lines: Vec<&str> = text.split(|c| c == '\n' || c == '|').collect();
    for (i, line) in lines.iter().enumerate() {
        let clean = line.trim_end_matches('\r');
        if !clean.is_empty() {
            segments.push(Segment::Text(clean.to_string()));
        }
        if i + 1 < lines.len() {
            segments.push(Segment::LineBreak);
        }
    }
    segments
}

pub struct KateDecoder {
    codec_id: CodecId,
    ctx: KateContext,
    pending: VecDeque<Frame>,
    eof: bool,
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != KATE_CODEC_ID {
        return Err(Error::unsupported(format!("not a kate codec id: {}", params.codec_id)));
    }
    let mut ctx = KateContext::default();
    if !params.extradata.is_empty() && params.extradata.len() >= 64 {
        let _ = ctx.parse_id_header(&params.extradata);
    }
    Ok(Box::new(KateDecoder {
        codec_id: params.codec_id.clone(),
        ctx,
        pending: VecDeque::new(),
        eof: false,
    }))
}

impl Decoder for KateDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("Kate packet too large"));
        }
        if packet.data.is_empty() {
            return Ok(());
        }

        let ptype = packet.data[0];

        // Header packet
        if ptype & 0x80 != 0 {
            if packet.data.len() >= 8 && &packet.data[1..8] == b"kate\x00\x00\x00" {
                if ptype == 0x80 {
                    let _ = self.ctx.parse_id_header(&packet.data);
                } else {
                    self.ctx.headers_seen = self.ctx.headers_seen.saturating_add(1);
                }
            }
            return Ok(());
        }

        // Data packet
        match ptype {
            0x01 => {
                // keepalive: no event
                Ok(())
            }
            0x7f => {
                // EOS: end of stream
                self.eof = true;
                Ok(())
            }
            0x00 | 0x02 => {
                // Text event (0x00) or repeat (0x02)
                if packet.data.len() < 29 {
                    return Err(Error::invalid("Kate text event too short"));
                }
                let mut reader = BitReader::new(&packet.data[1..]);
                let start = reader.read_u64()?;
                let duration = reader.read_u64()?;
                let _backlink = reader.read_u64()?;
                let text_len = reader.read_u32()? as usize;

                if text_len > MAX_CUE_BYTES {
                    return Err(Error::invalid("Kate text length too large"));
                }

                let text_start = 1 + 8 + 8 + 8 + 4; // 29
                if packet.data.len() < text_start + text_len {
                    return Err(Error::invalid("Kate text truncated"));
                }

                let text_bytes = &packet.data[text_start..text_start + text_len];
                let text_str = String::from_utf8_lossy(text_bytes).into_owned();

                let mut start_us = self.ctx.granule_to_us(start);
                let mut end_us = start_us + self.ctx.granule_to_us(duration);

                if start == 0 && duration == 0 {
                    if let Some(pts) = packet.pts {
                        start_us = packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000));
                        end_us = if let Some(dur) = packet.duration {
                            start_us + packet.time_base.rescale(dur, TimeBase::new(1, 1_000_000))
                        } else {
                            start_us
                        };
                    }
                }

                let segments = parse_kate_text(&text_str);
                if !segments.is_empty() {
                    self.pending.push_back(Frame::Subtitle(SubtitleCue {
                        start_us,
                        end_us,
                        style_ref: None,
                        positioning: None,
                        segments,
                    }));
                }
                Ok(())
            }
            _ => {
                // Unknown data packet type: ignored per spec for future proofing
                Ok(())
            }
        }
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

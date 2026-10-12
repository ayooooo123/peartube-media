// Copyright (c) 2010 Aurelien Jacobs <aurel@gnuage.org>
// Copyright (c) 2012 Clément Bœsch
//
// Derived from FFmpeg at commit 2da55bf: libavcodec/textdec.c and the
// ff_ass_bprint_text_event function of libavcodec/ass.c.
// Changed for PearTube on 2026-10-07: ported to safe Rust and modified.
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

//! Raw text subtitles (`text`, FFmpeg's `AV_CODEC_ID_TEXT`): each packet is
//! one cue's text, as OGM text streams carry it. As FFmpeg's decoder does,
//! the text ends at a NUL, a final newline (or CR LF) is dropped and other
//! newlines break lines. FFmpeg escapes ASS markup characters so they show
//! literally; text segments are literal already. A lone CR, which FFmpeg
//! keeps and nothing draws, is dropped too. A packet with nothing before
//! its first NUL makes no cue.

use std::collections::VecDeque;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result, Segment, SubtitleCue, TimeBase};

use crate::text_common::{decode_subtitle_text, subtitle_end_us};
use crate::TEXT_CODEC_ID;

const MAX_CUE_BYTES: usize = 1 << 20;

/// The cue text of one packet: lines as `ff_ass_bprint_text_event` breaks
/// them.
fn segments(data: &[u8]) -> Vec<Segment> {
    let text = data.iter().position(|&b| b == 0).map_or(data, |nul| &data[..nul]);
    let text = decode_subtitle_text(text);
    let mut segments = Vec::new();
    let mut line = String::new();
    for (at, c) in text.char_indices() {
        match c {
            // A final newline is dropped.
            '\n' if at + 1 < text.len() => {
                segments.push(Segment::Text(std::mem::take(&mut line)));
                segments.push(Segment::LineBreak);
            }
            '\n' | '\r' => {}
            c => line.push(c),
        }
    }
    segments.push(Segment::Text(line));
    segments.retain(|s| !matches!(s, Segment::Text(t) if t.is_empty()));
    segments
}

/// Create a raw text subtitle decoder.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != TEXT_CODEC_ID {
        return Err(Error::unsupported(format!("not a text codec id: {}", params.codec_id)));
    }
    Ok(Box::new(TextDecoder { codec_id: params.codec_id.clone(), pending: VecDeque::new(), eof: false }))
}

pub struct TextDecoder {
    codec_id: CodecId,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for TextDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if packet.data.len() > MAX_CUE_BYTES {
            return Err(Error::invalid("text subtitle packet exceeds maximum size"));
        }
        if packet.data.first().is_none_or(|&b| b == 0) {
            return Ok(());
        }
        let start_us = packet.pts.map(|pts| packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000))).unwrap_or(0);
        let end_us = subtitle_end_us(packet, start_us);
        self.pending.push_back(Frame::Subtitle(SubtitleCue {
            start_us,
            end_us,
            style_ref: None,
            positioning: None,
            segments: segments(&packet.data),
        }));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(f);
        }
        if self.eof {
            Err(Error::Eof)
        } else {
            Err(Error::NeedMore)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines a packet shows, `|` between them.
    fn shown(data: &[u8]) -> String {
        segments(data)
            .iter()
            .map(|s| match s {
                Segment::Text(t) => t.as_str(),
                Segment::LineBreak => "|",
                other => panic!("unexpected segment {other:?}"),
            })
            .collect()
    }

    #[test]
    fn lines_break_like_ffmpegs_text_events() {
        // OGM bots01: CR LF inside, a lone CR before the NUL.
        assert_eq!(shown(b"Year 955\r\nDay 14\r\0junk"), "Year 955|Day 14");
        // A final newline (or CR LF) is dropped.
        assert_eq!(shown(b"one\n"), "one");
        assert_eq!(shown(b"one\r\n"), "one");
        assert_eq!(shown(b"a\n\nb"), "a||b");
        assert_eq!(shown(b" \0"), " ");
    }
}

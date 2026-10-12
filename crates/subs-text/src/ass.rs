// Copyright (c) 2008 Michael Niedermayer
// Copyright (c) 2010 Aurelien Jacobs <aurel@gnuage.org>
// Copyright (c) 2014 Clément Bœsch
//
// Derived from FFmpeg at commit 2da55bf: libavformat/assdec.c and
// libavcodec/assdec.c.
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
//
// You should have received a copy of the GNU Lesser General Public
// License along with this file (crates/subs-text/LICENSE); if not,
// write to the Free Software Foundation, Inc., 51 Franklin Street,
// Fifth Floor, Boston, MA 02110-1301 USA

//! ASS/SSA: standalone `.ass`/`.ssa` demuxer and the `ass`/`ssa` decoders.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! headers verified):
//! - `libavformat/assdec.c` — probe; every line that is not a timed
//!   `Dialogue:` event (with positive duration) belongs to the script header
//!   exported as extradata; events become packets in Matroska's form,
//!   `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`;
//! - `libavcodec/assdec.c` — packets are events; the header (CodecPrivate)
//!   carries the styles they use.
//!
//! The same packets arrive from Matroska `S_TEXT/ASS` and `S_TEXT/SSA`
//! blocks, whose script header is the track's CodecPrivate.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Decoder, Demuxer, Error, MediaType, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

use crate::ass_split::{split_dialog, split_header};
use crate::ass_text::{AssEvent, AssEventDecoder, EventSource};
use crate::scan::{strtol, Scan};
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const ASS_CODEC_ID: &str = "ass";
pub const SSA_CODEC_ID: &str = "ssa";
pub const CONTAINER_NAME: &str = "ass";

/// `ass_probe`.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    let mut reader = TextReader::new(&text);
    while matches!(reader.peek(), b'\r' | b'\n') {
        reader.r8();
    }
    let head: Vec<u8> = (0..13).map(|_| reader.r8()).collect();
    if head == b"[Script Info]" {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// `read_dialogue`: `(start_cs, duration_cs, event)` of a timed event line
/// with positive duration.
fn read_dialogue(line: &[u8], readorder: u32) -> Option<(i64, i64, Vec<u8>)> {
    let mut s = Scan::new(line);
    s.lit_str(b"Dialogue:")?;
    s.ws();
    s.set(0, |b| b != b',')?;
    s.lit(b',')?;
    let stamp = |s: &mut Scan| -> Option<i64> {
        let h = s.int(0)? as i32;
        s.lit(b':')?;
        let m = s.int(0)? as i32;
        s.lit(b':')?;
        let sec = s.int(0)? as i32;
        s.any()?;
        let cs = s.int(0)? as i32;
        Some((i64::from(h) * 3600 + i64::from(m) * 60 + i64::from(sec)) * 100 + i64::from(cs))
    };
    let start = stamp(&mut s)?;
    s.lit(b',')?;
    let end = stamp(&mut s)?;
    s.lit(b',')?;
    let pos = s.pos();
    // `int duration`: the difference wraps to 32 bits.
    let duration = i64::from(end.wrapping_sub(start) as i32);
    if duration <= 0 {
        return None;
    }
    // The layer: `atoi` after "Dialogue: " (an SSA `Marked=N` reads as 0).
    let layer = strtol(line.get(10..).unwrap_or(&[])).0 as i32;
    let mut event = format!("{readorder},{layer},").into_bytes();
    event.extend_from_slice(&line[pos..]);
    while event.last().is_some_and(|&b| b == b'\r' || b == b'\n') {
        event.pop();
    }
    Some((start, duration, event))
}

/// `ass_read_header` over a whole decoded file: `(header, events)`.
pub(crate) fn demux_ass(text: &[u8]) -> (Vec<u8>, SubtitleQueue) {
    let mut q = SubtitleQueue::default();
    q.keep_duplicates = true;
    let mut header = Vec::new();
    let mut reader = TextReader::new(text);
    let mut line = Vec::new();
    let mut readorder: u32 = 0;
    loop {
        let pos = reader.pos();
        // get_line: up to and including '\n'; a NUL ends the line.
        line.clear();
        loop {
            let c = reader.r8();
            if c == 0 {
                break;
            }
            line.push(c);
            if c == b'\n' {
                break;
            }
        }
        if line.is_empty() {
            break;
        }
        match read_dialogue(&line, readorder) {
            Some((pts, duration, event)) => {
                readorder = readorder.wrapping_add(1);
                match q.insert(&event, false) {
                    Some(e) => (e.pos, e.pts, e.duration) = (pos, pts, duration),
                    None => break,
                }
            }
            None => {
                if header.len() + line.len() > crate::text_common::MAX_FILE_BYTES {
                    break;
                }
                header.extend_from_slice(&line);
            }
        }
    }
    (header, q)
}

/// Opens a standalone ASS/SSA script.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "ASS")?;
    let time_base = TimeBase::new(1, 100);
    let (header, queue) = demux_ass(&decode_text(&raw));
    let mut params = CodecParameters::subtitle(CodecId::new(ASS_CODEC_ID));
    params.media_type = MediaType::Subtitle;
    params.extradata = header;
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets: queue.finalize(time_base),
    }))
}

struct AssEvents;

impl EventSource for AssEvents {
    fn event(&mut self, _packet: &Packet, text: &[u8]) -> Result<Option<AssEvent>> {
        let dialog = split_dialog(text);
        Ok(Some(AssEvent { style: dialog.style.to_vec(), text: dialog.text.to_vec() }))
    }
}

/// The `ass`/`ssa` decoder: Matroska-form events, styles from extradata.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if !matches!(params.codec_id.as_str(), ASS_CODEC_ID | SSA_CODEC_ID) {
        return Err(Error::unsupported(format!("not an ASS codec id: {}", params.codec_id)));
    }
    // The script header's character set is decided like a cue's, so its
    // style names match events read the same way.
    let header = split_header(&crate::text_common::cue_text(&params.extradata));
    Ok(Box::new(AssEventDecoder::new(params.codec_id.clone(), header, AssEvents)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_leave_the_header_and_take_matroska_form() {
        let script = b"[Script Info]\r\nScriptType: v4.00\r\n\r\n[Events]\r\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\
            Dialogue: Marked=0,0:00:02.00,0:00:03.25,Cyan,,0000,0000,0000,,b\r\n\
            Dialogue: 3,0:00:01.00,0:00:01.50,Default,,0,0,0,,a, with comma\r\n\
            Dialogue: 0,0:00:05.00,0:00:05.00,Default,,0,0,0,,zero length\r\n\
            Comment: 0,0:00:06.00,0:00:07.00,Default,,0,0,0,,note\r\n";
        let (header, queue) = demux_ass(script);
        let header = String::from_utf8(header).unwrap();
        assert!(header.contains("zero length") && header.contains("Comment:") && header.starts_with("[Script Info]\r\n"));
        let packets: Vec<_> = queue.finalize(TimeBase::new(1, 100)).into_iter()
            .map(|p| (p.pts.unwrap(), p.duration.unwrap(), String::from_utf8(p.data).unwrap())).collect();
        assert_eq!(packets, vec![
            (100, 50, "1,3,Default,,0,0,0,,a, with comma".to_string()),
            (200, 125, "0,0,Cyan,,0000,0000,0000,,b".to_string()),
        ]);
    }
}

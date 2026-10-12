// Copyright (c) 2017 Paul B Mahol
//
// Derived from FFmpeg at commit 2da55bf: libavformat/sccdec.c.
// Changed for PearTube on 2026-10-08: ported to safe Rust and modified.
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

//! Scenarist Closed Captions (`.scc`): EIA-608 byte pairs by SMPTE time
//! code.
//!
//! Ported to safe Rust from FFmpeg at commit 2da55bf (LGPL-2.1-or-later —
//! header verified): `libavformat/sccdec.c` — the probe, and one packet per
//! line of pairs, timed by the line's time code (a frame counted as 33 ms).
//! A line is split before a `9420` (resume caption loading) that follows at
//! least five pairs and precedes a `942c` (erase displayed memory), the
//! first part timed 11 ms per byte. Each pair becomes a `cc_data` triplet
//! (`0xfc` and the two bytes), what the `eia_608` decoder (subs-cc) takes.

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, Demuxer, MediaType, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo,
    TimeBase, MAX_PROBE_SCORE,
};

use crate::scan::Scan;
use crate::text_common::TextSubtitleDemuxer;
use crate::text_reader::{decode_text, probe_text, read_input, SubtitleQueue, TextReader};

pub const CONTAINER_NAME: &str = "scc";
/// The packets' codec: subs-cc's EIA-608 decoder (`eia608::CODEC_ID`).
const CODEC_ID: &str = "eia_608";

/// `scc_probe`: the header, after any line breaks.
pub fn probe(data: &ProbeData) -> ProbeScore {
    let text = probe_text(data.buf);
    let start = text.iter().position(|&b| b != b'\r' && b != b'\n').unwrap_or(text.len());
    if text[start..].starts_with(b"Scenarist_SCC V1.0") {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// `convert`: a hex digit's value, wrapping as the C `uint8_t` arithmetic
/// does for other bytes.
fn convert(x: u8) -> u8 {
    if x >= b'a' {
        x.wrapping_sub(b'a' - 10)
    } else if x >= b'A' {
        x.wrapping_sub(b'A' - 10)
    } else {
        x.wrapping_sub(b'0')
    }
}

/// `sscanf(line, "%d:%d:%d%*[:;]%d")`: hours, minutes, seconds, frames.
fn time_code(line: &[u8]) -> Option<(i32, i32, i32, i32)> {
    let mut s = Scan::new(line);
    let hh = s.int(0)? as i32;
    s.lit(b':')?;
    let mm = s.int(0)? as i32;
    s.lit(b':')?;
    let ss = s.int(0)? as i32;
    s.set(0, |b| b == b':' || b == b';')?;
    let fs = s.int(0)? as i32;
    Some((hh, mm, ss, fs))
}

/// `scc_read_header` over a whole decoded file. A line that does not start
/// with a time code is skipped; at most 4095 bytes of a line count, and at
/// most 1365 pairs go into one packet.
fn demux_scc(text: &[u8]) -> SubtitleQueue {
    let mut q = SubtitleQueue::default();
    let mut reader = TextReader::new(text);
    let mut line = Vec::new();
    let mut out = [0u8; 4096];
    loop {
        let mut pos = reader.pos();
        let len = reader.read_line(4096, &mut line);
        if len.is_none_or(|n| n <= 13) {
            if reader.eof() {
                break;
            }
            continue;
        }
        let Some((hh, mm, ss, fs)) = time_code(&line) else { continue };
        let mut ts = (i64::from(hh) * 3600 + i64::from(mm) * 60 + i64::from(ss)) * 1000 + i64::from(fs) * 33;
        if let Some(sub) = q.last_mut() {
            sub.duration = ts - sub.pts;
        }

        // `av_strtok(line + 12, " ")`: the pairs, separated by spaces.
        let words = &line[12..];
        let mut next = Some(0);
        let mut i = 0;
        while i < 4095 {
            let Some(from) = next else { break };
            let start = from + words[from..].iter().take_while(|&&b| b == b' ').count();
            if start >= words.len() {
                break;
            }
            let end = words[start..].iter().position(|&b| b == b' ').map_or(words.len(), |n| start + n);
            next = (end < words.len()).then_some(end + 1);
            let &[c1, c2, c3, c4, ..] = &words[start..end] else { break };
            let o1 = (u32::from(convert(c2)) | u32::from(convert(c1)) << 4) as u8;
            let o2 = (u32::from(convert(c4)) | u32::from(convert(c3)) << 4) as u8;

            if i > 12 && o1 == 0x94 && o2 == 0x20 {
                let rest = next.map_or(&[][..], |n| &words[n..]);
                let next_is = |pair: &[u8]| rest.get(..4).is_some_and(|w| w.eq_ignore_ascii_case(pair));
                if !next_is(b"942f") && next_is(b"942c") {
                    let Some(sub) = q.insert(&out[..i], false) else { return q };
                    sub.pos = pos;
                    pos += i as i64;
                    sub.pts = ts;
                    sub.duration = i as i64 * 11;
                    ts += sub.duration;
                    i = 0;
                }
            }

            out[i] = 0xfc;
            out[i + 1] = o1;
            out[i + 2] = o2;
            i += 3;
        }

        let Some(sub) = q.insert(&out[..i], false) else { return q };
        sub.pos = pos;
        sub.pts = ts;
        // A packet's duration starts at 0 in FFmpeg; the next line sets it.
        sub.duration = 0;
    }
    q
}

/// Opens a Scenarist Closed Captions file.
pub fn open_demuxer(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let raw = read_input(&mut *input, "SCC")?;
    let time_base = TimeBase::new(1, 1000);
    let queue = demux_scc(&decode_text(&raw));
    let mut params = CodecParameters::subtitle(CodecId::new(CODEC_ID));
    params.media_type = MediaType::Subtitle;
    Ok(Box::new(TextSubtitleDemuxer {
        format_name: CONTAINER_NAME,
        streams: [StreamInfo { index: 0, time_base, duration: None, start_time: Some(0), params }],
        packets: queue.finalize(time_base),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packets(text: &[u8]) -> Vec<(i64, i64, Vec<u8>)> {
        demux_scc(text).finalize(TimeBase::new(1, 1000)).into_iter().map(|p| (p.pts.unwrap(), p.duration.unwrap(), p.data)).collect()
    }

    #[test]
    fn lines_become_triplets_timed_by_their_time_code() {
        let file = b"Scenarist_SCC V1.0\r\n\r\n00:00:01:15\t9420 c1c2\r\n\r\n00:00:03;00\t942c 942c\r\nbad line here!\r\n";
        assert_eq!(
            packets(file),
            [
                (1495, 1505, vec![0xfc, 0x94, 0x20, 0xfc, 0xc1, 0xc2]),
                (3000, 0, vec![0xfc, 0x94, 0x2c, 0xfc, 0x94, 0x2c]),
            ]
        );
    }

    /// A `9420` after five pairs, before a `942c`, starts a new packet; the
    /// first lasts 11 ms per byte. Hex digits of either case convert; other
    /// bytes wrap as C's `uint8_t` arithmetic does; a word shorter than four
    /// bytes ends the line.
    #[test]
    fn a_new_caption_splits_the_line() {
        let file = b"00:00:10:00\t9420 9152 9152 9137 91ae 9420 942C 94F2 ab 9420\n";
        assert_eq!(
            packets(file),
            [
                (10_000, 165, [0x94, 0x20, 0x91, 0x52, 0x91, 0x52, 0x91, 0x37, 0x91, 0xae].chunks(2).flat_map(|p| [0xfc, p[0], p[1]]).collect::<Vec<u8>>()),
                (10_165, 0, vec![0xfc, 0x94, 0x20, 0xfc, 0x94, 0x2c, 0xfc, 0x94, 0xf2]),
            ]
        );
        assert_eq!(convert(b'z'), 35);
        assert_eq!(convert(b'\t'), 217);
    }
}

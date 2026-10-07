// Copyright (c) 2012-2013 Clément Bœsch <u pkh me>
//
// The BOM and UTF-16 handling in `file_bytes` is derived from FFmpeg's
// libavformat/subtitles.c at commit 2da55bf.
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

//! Common text decoding and demuxing helpers for standalone text subtitle formats.
//!
//! A file's bytes are kept as FFmpeg's text reader keeps them: a UTF-8 BOM
//! is skipped and UTF-16 (by BOM) becomes UTF-8; nothing else is converted.
//! The character set is then decided per cue ([`cue_text`]): a cue that is
//! valid UTF-8 shows as UTF-8, as in FFmpeg; a cue that is not, which
//! FFmpeg without `-sub_charenc` rejects ("Invalid UTF-8 in decoded
//! subtitles text"), is read as Windows-1250. One such cue never changes
//! how the others are read.
//!
//! All reads are bounds-checked and capped against untrusted input.

use std::borrow::Cow;
use std::collections::VecDeque;
use oxideav_core::{Demuxer, Error, Packet, Result, StreamInfo};

/// Maximum file size accepted for standalone text subtitles (16 MiB).
pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum number of subtitle cues allowed per file.
pub const MAX_CUES: usize = 65_536;

/// Windows-1250 byte-to-char mapping table for bytes 0x80..=0xFF.
const CP1250_TABLE: [char; 128] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{FFFD}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{FFFD}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{015A}', '\u{0164}', '\u{017D}', '\u{0179}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{015B}', '\u{0165}', '\u{017E}', '\u{017A}',
    '\u{00A0}', '\u{02C7}', '\u{02D8}', '\u{0141}', '\u{00A4}', '\u{0104}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{015E}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{017B}',
    '\u{00B0}', '\u{00B1}', '\u{02DB}', '\u{0142}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{0105}', '\u{015F}', '\u{00BB}', '\u{013D}', '\u{02DD}', '\u{013E}', '\u{017C}',
    '\u{0154}', '\u{00C1}', '\u{00C2}', '\u{0102}', '\u{00C4}', '\u{0139}', '\u{0106}', '\u{00C7}',
    '\u{010C}', '\u{00C9}', '\u{0118}', '\u{00CB}', '\u{011A}', '\u{00CD}', '\u{00CE}', '\u{010E}',
    '\u{0110}', '\u{0143}', '\u{0147}', '\u{00D3}', '\u{00D4}', '\u{0150}', '\u{00D6}', '\u{00D7}',
    '\u{0158}', '\u{016E}', '\u{00DA}', '\u{0170}', '\u{00DC}', '\u{00DD}', '\u{0162}', '\u{00DF}',
    '\u{0155}', '\u{00E1}', '\u{00E2}', '\u{0103}', '\u{00E4}', '\u{013A}', '\u{0107}', '\u{00E7}',
    '\u{010D}', '\u{00E9}', '\u{0119}', '\u{00EB}', '\u{011B}', '\u{00ED}', '\u{00EE}', '\u{010F}',
    '\u{0111}', '\u{0144}', '\u{0148}', '\u{00F3}', '\u{00F4}', '\u{0151}', '\u{00F6}', '\u{00F7}',
    '\u{0159}', '\u{016F}', '\u{00FA}', '\u{0171}', '\u{00FC}', '\u{00FD}', '\u{0163}', '\u{02D9}',
];

/// A standalone file's bytes as FFmpeg's text reader yields them: a UTF-8
/// BOM skipped, UTF-16 (LE/BE, by BOM) converted to UTF-8 (stopping at an
/// invalid surrogate, as `GET_UTF16` does), every other byte unchanged.
pub(crate) fn file_bytes(raw: &[u8]) -> Vec<u8> {
    let utf16 = |big_endian: bool| {
        let units = raw[2..].chunks_exact(2).map(|c| {
            if big_endian { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }
        });
        let mut out = String::new();
        for c in char::decode_utf16(units) {
            match c {
                Ok(c) => out.push(c),
                Err(_) => break,
            }
        }
        out.into_bytes()
    };
    if raw.starts_with(b"\xff\xfe") {
        return utf16(false);
    }
    if raw.starts_with(b"\xfe\xff") {
        return utf16(true);
    }
    raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw).to_vec()
}

/// One cue's text as shown: unchanged when it is valid UTF-8, otherwise
/// read as Windows-1250.
pub(crate) fn cue_text(bytes: &[u8]) -> Cow<'_, [u8]> {
    match std::str::from_utf8(bytes) {
        Ok(_) => Cow::Borrowed(bytes),
        Err(_) => Cow::Owned(decode_windows_1250(bytes).into_bytes()),
    }
}

/// One cue's bytes as a `String` for the decoders that work on text: BOMs
/// and UTF-16 handled as for a file, the character set decided for this cue
/// ([`cue_text`]), line endings (`\r\n`, `\r`) normalized to `\n`.
pub fn decode_subtitle_text(bytes: &[u8]) -> String {
    if bytes.len() > MAX_FILE_BYTES {
        return String::new();
    }
    let bytes = file_bytes(bytes);
    normalize_newlines(&String::from_utf8_lossy(&cue_text(&bytes)))
}

/// First char of the range bytes 0x80..=0xFF stand for in [`file_text`]:
/// Latin Extended-A letters, never white space.
const HIGH_BYTES: u32 = 0x100;

/// A standalone file as text for structure parsing, one char per byte:
/// ASCII as itself, bytes 0x80..=0xFF as U+0100..=U+017F, line endings
/// normalized to `\n`. Parsers see exactly the file's ASCII structure, and
/// [`file_bytes_of`] turns any cue cut from it back into the file's own
/// bytes, so its decoder decides that cue's character set.
pub(crate) fn file_text(raw: &[u8]) -> String {
    let bytes = file_bytes(raw);
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                out.push('\n');
                if bytes.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b if b < 0x80 => out.push(char::from(b)),
            b => out.push(char::from_u32(HIGH_BYTES + u32::from(b - 0x80)).unwrap_or('\u{FFFD}')),
        }
        i += 1;
    }
    out
}

/// The file bytes a piece of [`file_text`] stands for.
pub(crate) fn file_bytes_of(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| match u32::from(c) {
            c @ 0..=0x7f => c as u8,
            c => (c.saturating_sub(HIGH_BYTES) as u8).wrapping_add(0x80),
        })
        .collect()
}

/// Windows-1250 bytes as text.
pub fn decode_windows_1250(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| if b < 0x80 { b as char } else { CP1250_TABLE[(b - 0x80) as usize] })
        .collect()
}

/// Normalizes line endings to `\n`.
pub fn normalize_newlines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

/// FFmpeg's subtitle API stores display duration as unsigned milliseconds.
/// Preserve the demuxer's negative final-cue sentinel through that conversion.
pub(crate) fn subtitle_end_us(packet: &Packet, start_us: i64) -> i64 {
    let duration_ms = packet.time_base.rescale(
        packet.duration.unwrap_or(0),
        oxideav_core::TimeBase::new(1, 1_000),
    ) as u32;
    start_us.saturating_add(i64::from(duration_ms) * 1_000)
}

/// Generic container demuxer for standalone text subtitle formats.
pub struct TextSubtitleDemuxer {
    pub format_name: &'static str,
    pub streams: [StreamInfo; 1],
    pub packets: VecDeque<Packet>,
}

impl Demuxer for TextSubtitleDemuxer {
    fn format_name(&self) -> &str {
        self.format_name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        self.packets.pop_front().ok_or(Error::Eof)
    }
}

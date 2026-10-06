//! Common text decoding and demuxing helpers for standalone text subtitle formats.
//!
//! Subtitle formats in the wild arrive in multiple text encodings:
//! - UTF-8 (with or without BOM)
//! - UTF-16 LE / BE (with BOM)
//! - Windows-1250 / Latin-1 fallback when UTF-8 decoding fails
//!
//! All reads are bounds-checked and capped against untrusted input.

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

/// Decode raw bytes to a String, sniffing BOMs and falling back to Windows-1250 if UTF-8 fails.
/// Also normalizes line endings (`\r\n` and `\r` -> `\n`).
pub fn decode_subtitle_text(bytes: &[u8]) -> String {
    if bytes.len() > MAX_FILE_BYTES {
        return String::new();
    }
    // Check BOMs
    if bytes.starts_with(b"\xef\xbb\xbf") {
        return normalize_newlines(&String::from_utf8_lossy(&bytes[3..]));
    }
    if bytes.starts_with(b"\xff\xfe") {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect();
        return normalize_newlines(&String::from_utf16_lossy(&u16s));
    }
    if bytes.starts_with(b"\xfe\xff") {
        let u16s: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .collect();
        return normalize_newlines(&String::from_utf16_lossy(&u16s));
    }

    // Try valid UTF-8
    if let Ok(s) = std::str::from_utf8(bytes) {
        return normalize_newlines(s);
    }

    // Fallback: Windows-1250
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x80 {
            out.push(b as char);
        } else {
            out.push(CP1250_TABLE[(b - 0x80) as usize]);
        }
    }
    normalize_newlines(&out)
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

//! Text input and event queue shared by the standalone subtitle demuxers.
//!
//! Ported to safe Rust from FFmpeg's `libavformat/subtitles.c` and
//! `libavformat/aviobuf.c` (`ff_get_line`) at commit 2da55bf
//! (LGPL-2.1-or-later — headers verified): the text reader (UTF-8 BOM
//! skipping, UTF-16 conversion), `ff_subtitles_read_line`,
//! `ff_subtitles_read_text_chunk`, and the subtitle queue (insertion,
//! merging, `ff_subtitles_queue_finalize`: sort, duration fill, duplicate
//! drop).
//!
//! The whole input is read up front (capped at [`MAX_FILE_BYTES`]). Unlike
//! FFmpeg without `-sub_charenc`, input that is not valid UTF-8 is
//! converted from Windows-1250 rather than having every cue rejected by the
//! decoder; valid UTF-8 (every FFmpeg reference fixture) is untouched.

use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{Error, Packet, ReadSeek, Result, TimeBase};

use crate::text_common::{MAX_CUES, MAX_FILE_BYTES};

/// Reads a whole standalone subtitle file, rejecting oversized input.
pub(crate) fn read_input(input: &mut dyn ReadSeek, what: &str) -> Result<Vec<u8>> {
    let mut raw = Vec::new();
    input.take((MAX_FILE_BYTES + 1) as u64).read_to_end(&mut raw).map_err(Error::from)?;
    if raw.len() > MAX_FILE_BYTES {
        return Err(Error::invalid(format!("{what} file exceeds maximum size")));
    }
    Ok(raw)
}

/// FFmpeg's text reader over a whole file: a UTF-8 BOM is skipped and
/// UTF-16 (LE/BE, by BOM) is converted to UTF-8, as `ff_text_init_avio`
/// does. Bytes that are not UTF-8 are converted from Windows-1250.
pub(crate) fn decode_text(raw: &[u8]) -> Vec<u8> {
    let utf16 = |big_endian: bool| {
        let units = raw[2..].chunks_exact(2).map(|c| {
            if big_endian { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }
        });
        // GET_UTF16 stops the text at an invalid surrogate sequence.
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
    let body = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    if std::str::from_utf8(body).is_ok() {
        return body.to_vec();
    }
    crate::text_common::decode_windows_1250(body).into_bytes()
}

/// The head of a probe buffer as text: probes read a few lines, so only the
/// first 16 KiB are converted.
pub(crate) fn probe_text(buf: &[u8]) -> Vec<u8> {
    decode_text(&buf[..buf.len().min(16 << 10)])
}

/// `FFTextReader` over already-decoded text. `r8` returns 0 at the end and
/// for an embedded NUL, as FFmpeg's reader does.
pub(crate) struct TextReader<'a> {
    text: &'a [u8],
    pos: usize,
}

impl<'a> TextReader<'a> {
    pub fn new(text: &'a [u8]) -> Self {
        Self { text, pos: 0 }
    }

    pub fn pos(&self) -> i64 {
        self.pos as i64
    }

    pub fn eof(&self) -> bool {
        self.pos >= self.text.len()
    }

    pub fn r8(&mut self) -> u8 {
        match self.text.get(self.pos) {
            Some(&b) => {
                self.pos += 1;
                b
            }
            None => 0,
        }
    }

    pub fn peek(&self) -> u8 {
        self.text.get(self.pos).copied().unwrap_or(0)
    }

    /// `ff_subtitles_read_line`: up to `size - 1` bytes, ending at `\r` or
    /// `\n`; then any `\r`s and one `\n` are skipped. `None` when a NUL
    /// precedes the end of the input (FFmpeg's `AVERROR_INVALIDDATA`).
    pub fn read_line(&mut self, size: usize, line: &mut Vec<u8>) -> Option<usize> {
        line.clear();
        while line.len() + 1 < size {
            let c = self.r8();
            if c == 0 {
                return self.eof().then_some(line.len());
            }
            if c == b'\r' || c == b'\n' {
                break;
            }
            line.push(c);
        }
        while self.peek() == b'\r' {
            self.r8();
        }
        if self.peek() == b'\n' {
            self.r8();
        }
        Some(line.len())
    }

    /// `ff_get_line`: one line including its terminator, at most
    /// `maxlen - 1` bytes kept (the rest of a longer line is consumed);
    /// a `\r\n` pair is consumed whole.
    pub fn get_line(&mut self, maxlen: usize, line: &mut Vec<u8>) -> usize {
        line.clear();
        loop {
            let c = self.r8();
            if c != 0 && line.len() + 1 < maxlen {
                line.push(c);
            }
            if c == b'\n' || c == b'\r' || c == 0 {
                if c == b'\r' && self.peek() == b'\n' {
                    self.r8();
                }
                break;
            }
        }
        line.len()
    }

    /// `ff_subtitles_read_text_chunk`: a block of lines up to an empty line,
    /// without leading or trailing line breaks.
    pub fn read_text_chunk(&mut self, buf: &mut Vec<u8>) {
        buf.clear();
        let mut eol_buf: Vec<u8> = Vec::with_capacity(4);
        let mut last_was_cr = false;
        let mut n = 0usize;
        let mut nb_eol = 0;
        loop {
            let c = self.r8();
            if c == 0 {
                break;
            }
            let is_eol = c == b'\r' || c == b'\n';
            if n == 0 && is_eol {
                continue;
            }
            if is_eol {
                nb_eol += i32::from(c == b'\n' || last_was_cr);
                if nb_eol == 2 {
                    break;
                }
                eol_buf.push(c);
                if eol_buf.len() == 4 {
                    break;
                }
                last_was_cr = c == b'\r';
                continue;
            }
            if !eol_buf.is_empty() {
                buf.extend_from_slice(&eol_buf);
                eol_buf.clear();
                nb_eol = 0;
            }
            buf.push(c);
            n += 1;
        }
    }
}

/// One queued event of `FFDemuxSubtitlesQueue`.
pub(crate) struct QueuedEvent {
    pub pts: i64,
    pub pos: i64,
    pub duration: i64,
    pub data: Vec<u8>,
}

/// `FFDemuxSubtitlesQueue` (one stream, `SUB_SORT_TS_POS`).
#[derive(Default)]
pub(crate) struct SubtitleQueue {
    events: Vec<QueuedEvent>,
    pub keep_duplicates: bool,
}

impl SubtitleQueue {
    /// `ff_subtitles_queue_insert`: a new event (pts 0, pos -1, duration
    /// -1 until the caller sets them) or, with `merge`, `event` appended to
    /// the last one. `None` once [`MAX_CUES`] events are queued.
    pub fn insert(&mut self, event: &[u8], merge: bool) -> Option<&mut QueuedEvent> {
        if merge && !self.events.is_empty() {
            let last = self.events.last_mut()?;
            if last.data.len() + event.len() > MAX_FILE_BYTES {
                return None;
            }
            last.data.extend_from_slice(event);
            return Some(last);
        }
        if self.events.len() >= MAX_CUES {
            return None;
        }
        self.events.push(QueuedEvent { pts: 0, pos: -1, duration: -1, data: event.to_vec() });
        self.events.last_mut()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// `ff_subtitles_queue_finalize` followed by the packets
    /// `ff_subtitles_queue_read_packet` hands out.
    pub fn finalize(mut self, time_base: TimeBase) -> VecDeque<Packet> {
        self.events.sort_by(|a, b| a.pts.cmp(&b.pts).then(a.pos.cmp(&b.pos)));
        for i in 0..self.events.len().saturating_sub(1) {
            let next = self.events[i + 1].pts;
            let this = &mut self.events[i];
            if this.duration < 0 && (next as u64).wrapping_sub(this.pts as u64) <= i64::MAX as u64 {
                this.duration = next.wrapping_sub(this.pts);
            }
        }
        if !self.keep_duplicates {
            self.events.dedup_by(|e, last| {
                e.pts == last.pts && e.duration == last.duration && crate::scan::c_str(&e.data) == crate::scan::c_str(&last.data)
            });
        }
        self.events
            .into_iter()
            .map(|e| {
                let mut packet = Packet::new(0, time_base, e.data);
                packet.pts = Some(e.pts);
                packet.dts = Some(e.pts);
                packet.duration = Some(e.duration);
                packet.flags.keyframe = true;
                packet
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_end_at_carriage_returns_and_one_further_newline() {
        // After its terminator a line also consumes any '\r's and one '\n':
        // "\r\r\n" ends one line, and so does "\n\n".
        let mut reader = TextReader::new(b"a\r\r\nb\n\nc\r\n\r\nd");
        let mut line = Vec::new();
        let mut lines = Vec::new();
        while !reader.eof() {
            reader.read_line(4096, &mut line).unwrap();
            lines.push(String::from_utf8(line.clone()).unwrap());
        }
        assert_eq!(lines, ["a", "b", "c", "", "d"]);
        let mut long = TextReader::new(b"abcdef\n");
        assert_eq!(long.read_line(4, &mut line), Some(3));
        assert_eq!(long.read_line(4, &mut line), Some(3));
        assert_eq!(line, b"def");
        assert_eq!(TextReader::new(b"a\0b").read_line(16, &mut line), None);
    }

    #[test]
    fn chunks_stop_at_a_blank_line_without_surrounding_breaks() {
        let mut reader = TextReader::new(b"\n\nA\r\nB\r\n\r\nC\n");
        let mut chunk = Vec::new();
        reader.read_text_chunk(&mut chunk);
        assert_eq!(chunk, b"A\r\nB");
        reader.read_text_chunk(&mut chunk);
        assert_eq!(chunk, b"C");
        reader.read_text_chunk(&mut chunk);
        assert!(chunk.is_empty());
    }

    #[test]
    fn finalize_sorts_fills_durations_and_drops_exact_duplicates() {
        let mut q = SubtitleQueue::default();
        for (pts, pos, duration, data) in [(20, 2, -1, "b"), (10, 1, 5, "a"), (10, 3, 5, "a"), (30, 4, -1, "c")] {
            let e = q.insert(data.as_bytes(), false).unwrap();
            (e.pts, e.pos, e.duration) = (pts, pos, duration);
        }
        let packets: Vec<_> = q.finalize(TimeBase::new(1, 1000)).into_iter().map(|p| (p.pts, p.duration, p.data)).collect();
        assert_eq!(packets, vec![
            (Some(10), Some(5), b"a".to_vec()),
            (Some(20), Some(10), b"b".to_vec()),
            (Some(30), Some(-1), b"c".to_vec()),
        ]);
    }
}

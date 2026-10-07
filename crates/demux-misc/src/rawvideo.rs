// Ported from FFmpeg (commit 2da55bf): libavformat/rawdec.c
// (ff_raw_video_read_header, ff_raw_read_partial_packet) and the frame
// splitting and key-frame rules of libavcodec/h264_parser.c
// (h264_find_frame_end, parse_nal_units, with h264_sei.c for recovery
// points and h264_ps.c for reference counts), hevc/parser.c
// (hevc_find_frame_end, parse_nal_units) and
// mpegvideo_parser.c (mpeg1_find_frame_end, mpegvideo_extract_headers),
// with avpriv_find_start_code (utils.c).
// License: LGPL-2.1-or-later
//
// Raw video elementary streams: the input is read in 1024-byte pieces and
// cut into the access units FFmpeg's parser for the codec cuts, each
// flagged key as FFmpeg flags it. The streams carry no timestamps; packets
// are numbered in the stream time base.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::Read;

use oxideav_core::{Demuxer, Error, Packet, ReadSeek, Result, StreamInfo};

use crate::parser::{Combine, Parser, Split, END_NOT_FOUND};

/// ff_raw_demuxer_class raw_packet_size
const RAW_PACKET_SIZE: usize = 1024;

/// A raw video demuxer over the splitter of its codec.
pub(crate) struct RawVideoDemuxer<S> {
    format: &'static str,
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    parser: Parser<S>,
    queue: VecDeque<Packet>,
    pos: i64,
    count: i64,
    eof: bool,
}

impl<S: Split + KeyFrame> RawVideoDemuxer<S> {
    pub fn new(format: &'static str, input: Box<dyn ReadSeek>, stream: StreamInfo, split: S) -> Self {
        Self { format, input, streams: vec![stream], parser: Parser::new(split), queue: VecDeque::new(), pos: 0, count: 0, eof: false }
    }

    /// ff_raw_read_partial_packet into the parser.
    fn read_piece(&mut self) -> Result<()> {
        let mut piece = [0u8; RAW_PACKET_SIZE];
        let mut n = 0;
        while n < piece.len() {
            let got = self.input.read(&mut piece[n..])?;
            if got == 0 {
                break;
            }
            n += got;
        }
        let mut units = Vec::new();
        if n == 0 {
            self.eof = true;
            self.parser.flush(&mut units);
        } else {
            if self.parser.split.buffered_bytes() + n > 32 * 1024 * 1024 {
                return Err(Error::invalid("raw video: access unit exceeds 32 MiB"));
            }
            self.parser.push(&piece[..n], None, None, self.pos, &mut units);
            self.pos += n as i64;
        }
        for unit in units {
            let key = S::key_frame(&unit.data, &mut self.parser.split);
            let mut packet = Packet::new(0, self.streams[0].time_base, unit.data);
            packet.pts = Some(self.count);
            packet.dts = Some(self.count);
            packet.duration = Some(1);
            packet.flags.keyframe = key;
            self.count += 1;
            self.queue.push_back(packet);
        }
        Ok(())
    }
}

impl<S: Split + KeyFrame + Send> Demuxer for RawVideoDemuxer<S> {
    fn format_name(&self) -> &str {
        self.format
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = self.queue.pop_front() {
                return Ok(packet);
            }
            if self.eof {
                return Err(Error::Eof);
            }
            self.read_piece()?;
        }
    }
}

/// How a codec's parser flags a unit key: AV_PKT_FLAG_KEY as parse_packet
/// sets it from the parser's key_frame and pict_type.
pub(crate) trait KeyFrame {
    fn buffered_bytes(&self) -> usize;
    fn key_frame(unit: &[u8], split: &mut Self) -> bool;
}

/// avpriv_find_start_code over `buf[p..end]`, carrying `state` across
/// calls: the index just past the start code's code byte, or `end`.
fn find_start_code(buf: &[u8], mut p: usize, end: usize, state: &mut u32) -> usize {
    if p >= end {
        return end;
    }
    for _ in 0..3 {
        let tmp = *state << 8;
        *state = tmp.wrapping_add(u32::from(buf[p]));
        p += 1;
        if tmp == 0x100 || p == end {
            return p;
        }
    }
    while p < end {
        if buf[p - 1] > 1 {
            p += 3;
        } else if buf[p - 2] != 0 {
            p += 2;
        } else if buf[p - 3] != 0 || buf[p - 1] != 1 {
            p += 1;
        } else {
            p += 1;
            break;
        }
    }
    let p = p.min(end) - 4;
    *state = u32::from_be_bytes([buf[p], buf[p + 1], buf[p + 2], buf[p + 3]]);
    p + 4
}

/// get_ue_golomb_long on `bytes` (zeros past their end): the value and
/// the bits left after it, negative when it ran past the end.
fn ue_golomb_long(bytes: &[u8]) -> (u32, i64) {
    let total = bytes.len() as i64 * 8;
    let bit = |k: i64| -> u64 { if k < total { u64::from((bytes[(k / 8) as usize] >> (7 - k % 8)) & 1) } else { 0 } };
    let show: u64 = (0..32).fold(0, |v, k| (v << 1) | bit(k));
    let log = if show == 0 { 31 } else { 31 - (63 - i64::from(show.leading_zeros())) };
    let value = (log..2 * log + 1).fold(0u64, |v, k| (v << 1) | bit(k));
    ((value as u32).wrapping_sub(1), total - (2 * log + 1))
}

// ───────────────────────── MPEG-1/2 video ─────────────────────────

const SEQ_END_CODE: u32 = 0x1B7;
const SEQ_START_CODE: u32 = 0x1B3;
const EXT_START_CODE: u32 = 0x1B5;
const PICTURE_START_CODE: u32 = 0x100;
const SLICE_MIN_START_CODE: u32 = 0x101;
const SLICE_MAX_START_CODE: u32 = 0x1AF;

/// mpegvideo_parser.c: a frame (both fields of a field pair) with the
/// headers before it.
pub(crate) struct MpegVideo {
    pc: Combine,
    frame_start_found: u8,
    /// AVCodecParserContext.pict_type: I until a picture header says.
    pict_type: u8,
}

impl Default for MpegVideo {
    fn default() -> Self {
        Self { pc: Combine::default(), frame_start_found: 0, pict_type: 1 }
    }
}

impl MpegVideo {
    /// mpeg1_find_frame_end. States: 0 frame start, 1 first sequence
    /// extension, 2 first field start, 3 second extension, 4 searching
    /// the end.
    fn find_frame_end(&mut self, buf: &[u8]) -> isize {
        let mut state = self.pc.state;
        if buf.is_empty() {
            return 0;
        }
        let mut i = 0;
        while i < buf.len() {
            if self.frame_start_found & 1 != 0 {
                if state == EXT_START_CODE && (buf[i] & 0xF0) != 0x80 {
                    self.frame_start_found -= 1;
                } else if state == EXT_START_CODE + 2 {
                    if buf[i] & 3 == 3 {
                        self.frame_start_found = 0;
                    } else {
                        self.frame_start_found = (self.frame_start_found + 1) & 3;
                    }
                }
                state = state.wrapping_add(1);
            } else {
                i = find_start_code(buf, i, buf.len(), &mut state) - 1;
                if self.frame_start_found == 0 && (SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&state) {
                    i += 1;
                    self.frame_start_found = 4;
                }
                if state == SEQ_END_CODE {
                    self.frame_start_found = 0;
                    self.pc.state = u32::MAX;
                    return i as isize + 1;
                }
                if self.frame_start_found == 2 && state == SEQ_START_CODE {
                    self.frame_start_found = 0;
                }
                if self.frame_start_found < 4 && state == EXT_START_CODE {
                    self.frame_start_found += 1;
                }
                if self.frame_start_found == 4 && (state & 0xFFFF_FF00) == 0x100 && !(SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&state) {
                    self.frame_start_found = 0;
                    self.pc.state = u32::MAX;
                    return i as isize - 3;
                }
            }
            i += 1;
        }
        self.pc.state = state;
        END_NOT_FOUND
    }
}

impl Split for MpegVideo {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let next = self.find_frame_end(buf);
        match self.pc.combine(next, buf) {
            Some(unit) => (next, Some(unit)),
            None => (buf.len() as isize, None),
        }
    }
}

impl KeyFrame for MpegVideo {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    /// mpegvideo_extract_headers: the picture coding type of the first
    /// picture header before the first slice; key when it is I.
    fn key_frame(unit: &[u8], split: &mut Self) -> bool {
        let mut p = 0;
        while p < unit.len() {
            let mut code = u32::MAX;
            p = find_start_code(unit, p, unit.len(), &mut code);
            let left = unit.len() - p;
            if code == PICTURE_START_CODE {
                if left >= 2 {
                    split.pict_type = (unit[p + 1] >> 3) & 7;
                }
            } else if (SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&code) || (code & 0xFFFF_FF00) != 0x100 {
                break;
            }
        }
        split.pict_type == 1
    }
}

// ───────────────────────── H.264 ─────────────────────────

/// h264_parser.c: an access unit, cut where FFmpeg's parser cuts it.
pub(crate) struct H264 {
    pc: Combine,
    frame_start_found: bool,
    history: [u8; 6],
    history_count: usize,
    last_mb: u32,
    /// SPS reference count and bit depth; PPS captures its SPS's count.
    sps: [Option<(u32, u32)>; 32],
    pps: [Option<(u32, u32)>; 256],
}

impl Default for H264 {
    fn default() -> Self {
        Self {
            pc: Combine::default(), frame_start_found: false,
            history: [0; 6], history_count: 0, last_mb: 0,
            sps: [None; 32], pps: [None; 256],
        }
    }
}

impl H264 {
    /// h264_find_frame_end for Annex B input.
    fn find_frame_end(&mut self, buf: &[u8]) -> isize {
        let n = buf.len() as isize;
        let mut state = self.pc.state;
        if state > 13 {
            state = 7;
        }
        let mut i: isize = 0;
        while i < n {
            let byte = buf[i as usize];
            if state == 7 {
                i += buf[i as usize..].iter().position(|&b| b == 0).unwrap_or(buf.len() - i as usize) as isize;
                if i < n {
                    state = 2;
                }
            } else if state <= 2 {
                if byte == 1 {
                    state ^= 5;
                } else if byte != 0 {
                    state = 7;
                } else {
                    state >>= 1;
                }
            } else if state <= 5 {
                match byte & 0x1F {
                    // SEI, SPS, PPS, AUD
                    6..=9 => {
                        if self.frame_start_found {
                            i += 1;
                            return self.found(i, state);
                        }
                        state = 7;
                    }
                    // slice, data partition A, IDR slice
                    1 | 2 | 5 => {
                        state += 8;
                    }
                    _ => state = 7,
                }
            } else {
                self.history[self.history_count] = byte;
                self.history_count += 1;
                let (mb, left) = ue_golomb_long(&self.history[..self.history_count]);
                if left > 0 || self.history_count > 5 {
                    let last = self.last_mb;
                    self.last_mb = mb;
                    if self.frame_start_found {
                        if mb <= last {
                            i -= self.history_count as isize - 1;
                            self.history_count = 0;
                            return self.found(i, state);
                        }
                    } else {
                        self.frame_start_found = true;
                    }
                    self.history_count = 0;
                    state = 7;
                }
            }
            i += 1;
        }
        self.pc.state = state;
        END_NOT_FOUND
    }

    fn found(&mut self, i: isize, state: u32) -> isize {
        self.pc.state = 7;
        self.frame_start_found = false;
        i - (state & 5) as isize
    }
}

impl Split for H264 {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let next = self.find_frame_end(buf);
        let Some(unit) = self.pc.combine(next, buf) else {
            return (buf.len() as isize, None);
        };
        if next < 0 && next != END_NOT_FOUND {
            // At most the four-byte prefix, header and six history bytes
            // precede this buffer. Replay on the stack, not a heap copy.
            let bytes = self.pc.overread_bytes(next);
            let mut overread = [0u8; 11];
            let len = bytes.len();
            overread[..len].copy_from_slice(bytes);
            self.find_frame_end(&overread[..len]);
        }
        (next, Some(unit))
    }
}

/// The NAL header indices of an Annex B unit, in order.
fn nal_starts(unit: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mut p = 0;
    std::iter::from_fn(move || {
        let mut state = u32::MAX;
        let at = find_start_code(unit, p, unit.len(), &mut state);
        if at >= unit.len() || (state & 0xFFFF_FF00) != 0x100 {
            return None;
        }
        p = at;
        Some(at - 1)
    })
}

/// A NAL's payload after `header` bytes of header, emulation prevention
/// removed, up to the next start code.
fn rbsp(unit: &[u8], from: usize) -> Cow<'_, [u8]> {
    let data = &unit[from.min(unit.len())..];
    let mut zeros = 0;
    let mut end = data.len();
    let mut escaped = false;
    for (i, &b) in data.iter().enumerate() {
        if zeros >= 2 && b <= 2 {
            end = i;
            break;
        }
        if zeros >= 2 && b == 3 {
            escaped = true;
            zeros = 0;
        } else {
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
    }
    let data = &data[..end];
    if !escaped {
        return Cow::Borrowed(data);
    }
    let mut out = Vec::with_capacity(data.len());
    zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    Cow::Owned(out)
}

/// ff_h264_sei_decode as far as recovery points: whether a valid one is
/// in the SEI payload `sei` before a message FFmpeg fails on.
fn sei_recovery_point(sei: &[u8]) -> bool {
    let mut p = 0;
    while sei.len() - p > 2 && (sei[p] != 0 || sei[p + 1] != 0) {
        let mut read = || -> Option<u32> {
            let mut v = 0u32;
            loop {
                let b = *sei.get(p)?;
                p += 1;
                v += u32::from(b);
                if b != 255 {
                    return Some(v);
                }
            }
        };
        let (Some(kind), Some(size)) = (read(), read()) else { return false };
        let size = size as usize;
        if size > sei.len() - p {
            return false;
        }
        let payload = &sei[p..p + size];
        match kind {
            // picture timing: FFmpeg keeps at most 40 bytes
            1 if size > 40 => return false,
            // recovery point: recovery_frame_cnt below 1 << 16
            6 => return ue_golomb_long(payload).0 < 1 << 16,
            // Messages unrelated to recovery do not set the key flag.
            _ => {}
        }
        p += size;
    }
    false
}

/// Bounded header reader for the SPS/PPS fields the keyframe heuristic
/// uses. Exhausted or overlong codes reject that header.
struct Bits<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(bytes: &'a [u8]) -> Self { Self { bytes, pos: 0 } }

    fn read(&mut self, n: usize) -> Option<u32> {
        if n > 32 || self.pos.checked_add(n)? > self.bytes.len() * 8 {
            return None;
        }
        let mut value = 0;
        for _ in 0..n {
            value = (value << 1) | u32::from((self.bytes[self.pos / 8] >> (7 - self.pos % 8)) & 1);
            self.pos += 1;
        }
        Some(value)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.read(1)? == 0 {
            zeros += 1;
            if zeros > 31 { return None; }
        }
        Some((1u32 << zeros) - 1 + self.read(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let v = i64::from(self.ue()?);
        Some(if v & 1 == 0 { -(v / 2) } else { (v + 1) / 2 })
    }
}

impl H264 {
    /// h264_ps.c through ref_frame_count; no picture decoding is needed.
    fn sps_refs(&mut self, bytes: &[u8]) -> Option<()> {
        let mut b = Bits::new(bytes);
        let profile = b.read(8)?;
        b.read(16)?;
        let id = b.ue()? as usize;
        if id >= self.sps.len() { return None; }
        let mut depth = 8;
        if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 144) {
            let chroma = b.ue()?;
            if chroma > 3 || (chroma == 3 && b.read(1)? != 0) { return None; }
            let luma = b.ue()?;
            if luma > 6 || b.ue()? != luma { return None; }
            depth += luma;
            b.read(1)?;
            if b.read(1)? != 0 {
                for i in 0..if chroma == 3 { 12 } else { 8 } {
                    if b.read(1)? == 0 { continue; }
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 { next = (last + b.se()?).rem_euclid(256); }
                        if next != 0 { last = next; }
                    }
                }
            }
        }
        if b.ue()? > 12 { return None; }
        match b.ue()? {
            0 => { if b.ue()? > 12 { return None; } }
            1 => {
                b.read(1)?;
                b.se()?;
                b.se()?;
                let cycle = b.ue()?;
                if cycle >= 256 { return None; }
                for _ in 0..cycle { b.se()?; }
            }
            2 => {}
            _ => return None,
        }
        let refs = b.ue()?;
        if refs > 16 { return None; }
        self.sps[id] = Some((refs, depth));
        Some(())
    }

    fn pps_refs(&mut self, bytes: &[u8]) -> Option<()> {
        let mut b = Bits::new(bytes);
        let id = b.ue()? as usize;
        if id >= self.pps.len() { return None; }
        let (sps_refs, depth) = (*self.sps.get(b.ue()? as usize)?)?;
        if depth == 11 || depth == 13 { return None; }
        b.read(2)?;
        if b.ue()? != 0 { return None; } // FFmpeg rejects FMO.
        let refs_minus1 = b.ue()?;
        if refs_minus1 >= 32 || b.ue()? >= 32 { return None; }
        self.pps[id] = Some((sps_refs, refs_minus1 + 1));
        Some(())
    }

    fn intra_key(&self, bytes: &[u8]) -> Option<bool> {
        let mut b = Bits::new(bytes);
        b.ue()?;
        let slice_type = b.ue()?;
        let (sps_refs, pps_refs) = (*self.pps.get(b.ue()? as usize)?)?;
        Some(slice_type % 5 == 2 && sps_refs <= 1 && pps_refs <= 1)
    }
}

impl KeyFrame for H264 {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    /// parse_nal_units: IDR, recovery point, or FFmpeg's single-reference
    /// I-picture heuristic, using the active SPS/PPS.
    fn key_frame(unit: &[u8], split: &mut Self) -> bool {
        let mut recovery = false;
        for h in nal_starts(unit) {
            match unit[h] & 0x1F {
                5 => return true,
                1 | 2 => return recovery || split.intra_key(&rbsp(unit, h + 1)).unwrap_or(false),
                6 => recovery |= sei_recovery_point(&rbsp(unit, h + 1)),
                7 => { let _ = split.sps_refs(&rbsp(unit, h + 1)); }
                8 => { let _ = split.pps_refs(&rbsp(unit, h + 1)); }
                _ => {}
            }
        }
        false
    }
}

// ───────────────────────── HEVC ─────────────────────────

/// hevc/parser.c: an access unit, cut where FFmpeg's parser cuts it.
#[derive(Default)]
pub(crate) struct Hevc {
    pc: Combine,
    frame_start_found: bool,
}

impl Hevc {
    /// hevc_find_frame_end
    fn find_frame_end(&mut self, buf: &[u8]) -> isize {
        for (i, &byte) in buf.iter().enumerate() {
            self.pc.state64 = (self.pc.state64 << 8) | u64::from(byte);
            if (self.pc.state64 >> 24) & 0xFF_FFFF != 1 {
                continue;
            }
            let nut = (self.pc.state64 >> 17) & 0x3F;
            if (self.pc.state64 >> 11) & 0x3F > 0 {
                continue;
            }
            let i = i as isize;
            let start = |state64: u64| if (state64 >> 48) & 0xFF == 0 { i - 6 } else { i - 5 };
            // VPS..EOB, prefix SEI, reserved 41-44 and 48-55 start a unit
            if (32..=37).contains(&nut) || nut == 39 || (41..=44).contains(&nut) || (48..=55).contains(&nut) {
                if self.frame_start_found {
                    self.frame_start_found = false;
                    return start(self.pc.state64);
                }
            } else if nut <= 9 || (16..=21).contains(&nut) {
                // first_slice_segment_in_pic_flag
                if byte >> 7 != 0 {
                    if !self.frame_start_found {
                        self.frame_start_found = true;
                    } else {
                        self.frame_start_found = false;
                        return start(self.pc.state64);
                    }
                }
            }
        }
        END_NOT_FOUND
    }
}

impl Split for Hevc {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let next = self.find_frame_end(buf);
        match self.pc.combine(next, buf) {
            Some(unit) => (next, Some(unit)),
            None => (buf.len() as isize, None),
        }
    }
}

impl KeyFrame for Hevc {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    /// parse_nal_units: key when the first base-layer slice is IRAP.
    fn key_frame(unit: &[u8], _: &mut Self) -> bool {
        for h in nal_starts(unit) {
            let Some(&second) = unit.get(h + 1) else { break };
            let nut = (unit[h] >> 1) & 0x3F;
            let layer = ((unit[h] & 1) << 5) | (second >> 3);
            if layer > 0 {
                continue;
            }
            if nut <= 9 || (16..=21).contains(&nut) {
                return (16..=23).contains(&nut);
            }
        }
        false
    }
}

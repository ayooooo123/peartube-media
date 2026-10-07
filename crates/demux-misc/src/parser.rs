// Ported from FFmpeg (commit 2da55bf): libavcodec/parser.c
// (av_parser_parse2, ff_fetch_timestamp, ff_combine_frame),
// libavcodec/dvdsub_parser.c, mpegaudio_parser.c with
// mpegaudiodecheader.c (ff_mpa_decode_header), aac_ac3_parser.c with
// ac3_parser.c (ac3_sync, ff_ac3_find_syncword), and from
// libavformat/demux.c the parse_packet loop that drives them and the
// audio path of compute_pkt_fields (update_initial_timestamps,
// update_initial_durations, av_add_stable).
// License: LGPL-2.1-or-later
//
// FFmpeg's parser stage, for demuxers whose packets are not the units a
// decoder takes. A `Parser` is fed one stream's demuxed packets in order
// and returns whole units, each carrying the timestamps FFmpeg's parser
// stage hands the unit's packet; `AudioClock` then times audio units the
// way FFmpeg's demuxer layer does before av_read_frame returns them.

use std::collections::VecDeque;

use oxideav_core::Packet;

/// One codec's unit splitter: the `parse` callback of an FFmpeg parser.
/// It consumes `buf` up to where a unit ends (all of it when none does)
/// and returns the completed unit; an empty `buf` is the end of input.
/// As in FFmpeg, the index can be negative: the unit ended in bytes held
/// from before `buf`, which is then offered again.
pub(crate) trait Split {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>);

    /// AVCodecParserContext.duration and the sample rate the parser has
    /// set on the codec context, for audio parsers.
    fn audio(&self) -> (i64, u32) {
        (0, 0)
    }

    /// av_get_audio_frame_duration for the codec context the parser has
    /// set up: what compute_frame_duration gives a unit in samples when
    /// the parser gave it no duration (0 for none).
    fn fallback(&self) -> i64 {
        0
    }
}

/// A parsed unit and the timestamps of the demuxed packet it inherits.
pub(crate) struct Unit {
    pub data: Vec<u8>,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    /// The parser's duration in samples and the sample rate it knew when
    /// the unit came out (0 when it does not tell).
    pub samples: i64,
    pub sample_rate: u32,
    /// The codec's duration in samples when the parser has none.
    pub fallback: i64,
    /// Where the unit starts in the input (the parser's frame_offset,
    /// which raw demuxers make the packet position and index).
    pub pos: i64,
}

/// AV_PARSER_PTS_NB: demuxed packets whose timestamps are remembered.
const PTS_NB: usize = 4;

/// The timestamp bookkeeping of AVCodecParserContext: which demuxed
/// packet's timestamps a unit inherits. Offsets count input bytes from the
/// first packet's position on.
struct Timestamps {
    fetched_offset: bool,
    fetch_timestamp: bool,
    cur_offset: i64,
    frame_offset: i64,
    next_frame_offset: i64,
    start_index: usize,
    offset: [i64; PTS_NB],
    end: [i64; PTS_NB],
    pts: [Option<i64>; PTS_NB],
    dts: [Option<i64>; PTS_NB],
    out_pts: Option<i64>,
    out_dts: Option<i64>,
}

impl Default for Timestamps {
    fn default() -> Self {
        Self {
            fetched_offset: false,
            // av_parser_init sets fetch_timestamp.
            fetch_timestamp: true,
            cur_offset: 0,
            frame_offset: 0,
            next_frame_offset: 0,
            start_index: 0,
            offset: [0; PTS_NB],
            end: [0; PTS_NB],
            pts: [None; PTS_NB],
            dts: [None; PTS_NB],
            out_pts: None,
            out_dts: None,
        }
    }
}

impl Timestamps {
    /// ff_fetch_timestamp(s, 0, 0, 0): the timestamps of the last packet
    /// that starts at or before the current offset and after the last
    /// unit's start.
    fn fetch(&mut self) {
        self.out_pts = None;
        self.out_dts = None;
        for i in 0..PTS_NB {
            if self.cur_offset >= self.offset[i]
                && (self.frame_offset < self.offset[i] || (self.frame_offset == 0 && self.next_frame_offset == 0))
                && self.end[i] != 0
            {
                self.out_dts = self.dts[i];
                self.out_pts = self.pts[i];
                if self.cur_offset < self.end[i] {
                    break;
                }
            }
        }
    }
}

/// An FFmpeg parser: a [`Split`] plus the timestamp bookkeeping of
/// av_parser_parse2.
pub(crate) struct Parser<S> {
    pub split: S,
    ts: Timestamps,
}

impl<S: Split> Parser<S> {
    pub fn new(split: S) -> Self {
        Self { split, ts: Timestamps::default() }
    }

    /// av_parser_parse2: one call of the splitter on `buf`, which starts
    /// a demuxed packet when `pos` is `Some`. Returns the bytes consumed.
    fn parse2(&mut self, buf: &[u8], pts: Option<i64>, dts: Option<i64>, pos: Option<i64>, out: &mut Vec<Unit>) -> usize {
        let ts = &mut self.ts;
        if !ts.fetched_offset {
            ts.cur_offset = pos.unwrap_or(-1);
            ts.next_frame_offset = ts.cur_offset;
            ts.fetched_offset = true;
        }
        let len = buf.len() as i64;
        if !buf.is_empty() && ts.cur_offset + len != ts.end[ts.start_index] {
            // A new packet: remember where it lies and its timestamps.
            let i = (ts.start_index + 1) & (PTS_NB - 1);
            ts.start_index = i;
            ts.offset[i] = ts.cur_offset;
            ts.end[i] = ts.cur_offset + len;
            ts.pts[i] = pts;
            ts.dts[i] = dts;
        }
        if ts.fetch_timestamp {
            ts.fetch_timestamp = false;
            ts.fetch();
        }
        let (index, unit) = self.split.parse(buf);
        let ts = &mut self.ts;
        if let Some(data) = unit.filter(|data| !data.is_empty()) {
            ts.frame_offset = ts.next_frame_offset;
            ts.next_frame_offset = ts.cur_offset + index as i64;
            ts.fetch_timestamp = true;
            let (samples, sample_rate) = self.split.audio();
            let fallback = self.split.fallback();
            out.push(Unit { data, pts: ts.out_pts, dts: ts.out_dts, samples, sample_rate, fallback, pos: ts.frame_offset });
        }
        let index = index.max(0) as usize;
        ts.cur_offset += index as i64;
        index
    }

    /// parse_packet: feed one demuxed packet, collecting completed units.
    pub fn push(&mut self, data: &[u8], pts: Option<i64>, dts: Option<i64>, pos: i64, out: &mut Vec<Unit>) {
        let mut rest = data;
        let (mut pts, mut dts, mut pos) = (pts, dts, Some(pos));
        while !rest.is_empty() {
            let before = out.len();
            let used = self.parse2(rest, pts, dts, pos, out).min(rest.len());
            (pts, dts, pos) = (None, None, None);
            rest = &rest[used..];
            if used == 0 && out.len() == before {
                // A parser consuming nothing without output would make
                // FFmpeg's loop spin; none of these does.
                break;
            }
        }
    }

    /// parse_packet with flush at the end of input: drain what the
    /// splitter still holds.
    pub fn flush(&mut self, out: &mut Vec<Unit>) {
        loop {
            let before = out.len();
            self.parse2(&[], None, None, None, out);
            if out.len() == before {
                break;
            }
        }
    }
}

/// END_NOT_FOUND (parser.h)
pub(crate) const END_NOT_FOUND: isize = -100;

/// ParseContext with ff_combine_frame: gathers a unit across the buffers
/// a splitter is offered, including bytes it read past a unit's end.
#[derive(Default)]
pub(crate) struct Combine {
    buffer: Vec<u8>,
    index: usize,
    last_index: usize,
    overread: usize,
    overread_index: usize,
    pub state: u32,
    pub state64: u64,
}

impl Combine {
    pub fn buffered_bytes(&self) -> usize {
        self.index + self.overread
    }

    /// After a unit ended `-next` bytes before the buffer just offered,
    /// those bytes (`&pc->buffer[pc->last_index + next]`, `-next` long).
    pub fn overread_bytes(&self, next: isize) -> &[u8] {
        let start = (self.last_index as isize + next).max(0) as usize;
        &self.buffer[start..self.last_index.min(self.buffer.len())]
    }

    /// ff_combine_frame: `next` is where the unit ends in `buf`
    /// ([`END_NOT_FOUND`] when it does not). `None` while the unit is
    /// incomplete (all of `buf` kept), else the whole unit.
    pub fn combine(&mut self, mut next: isize, buf: &[u8]) -> Option<Vec<u8>> {
        // Copy overread bytes from the last unit to the front.
        while self.overread > 0 {
            self.buffer[self.index] = self.buffer[self.overread_index];
            self.index += 1;
            self.overread_index += 1;
            self.overread -= 1;
        }
        if next > buf.len() as isize {
            return None;
        }
        // Flush what remains at the end of input.
        if buf.is_empty() && next == END_NOT_FOUND {
            next = 0;
        }
        self.last_index = self.index;
        if next == END_NOT_FOUND {
            self.buffer.truncate(self.index);
            self.buffer.extend_from_slice(buf);
            self.index += buf.len();
            return None;
        }
        let end = (self.index as isize + next) as usize;
        self.overread_index = end;
        let unit = if self.index > 0 {
            self.buffer.truncate(self.index);
            if next > 0 {
                self.buffer.extend_from_slice(&buf[..next as usize]);
            }
            self.index = 0;
            self.buffer[..end].to_vec()
        } else {
            buf[..next as usize].to_vec()
        };
        if next < -8 {
            self.overread += (-8 - next) as usize;
            next = -8;
        }
        // Store overread bytes.
        while next < 0 {
            let byte = self.buffer[(self.last_index as isize + next) as usize];
            self.state = self.state << 8 | u32::from(byte);
            self.state64 = self.state64 << 8 | u64::from(byte);
            self.overread += 1;
            next += 1;
        }
        Some(unit)
    }
}

/// The largest DVD subpicture unit assembled; FFmpeg allows up to
/// INT_MAX bytes. A unit whose header claims more is dropped.
pub(crate) const MAX_SPU_SIZE: usize = 1 << 20;

/// dvdsub_parser.c: reassembles a DVD subpicture unit from the PES
/// payloads it spans. Its first two bytes give its size, or, when 0
/// (HD-DVD), the four bytes after them.
#[derive(Default)]
pub(crate) struct DvdSub {
    packet: Vec<u8>,
    packet_len: usize,
    packet_index: usize,
    allocated: bool,
}

impl Split for DvdSub {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let n = buf.len();
        if self.packet_index == 0 {
            let be16 = |b: &[u8]| usize::from(u16::from_be_bytes([b[0], b[1]]));
            if n < 2 || (be16(buf) == 0 && n < 6) {
                // Too small to start a unit: passed on as it is.
                return (n as isize, (n > 0).then(|| buf.to_vec()));
            }
            let mut len = be16(buf);
            if len == 0 {
                len = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]) as usize;
            }
            self.packet.clear();
            self.allocated = len <= MAX_SPU_SIZE;
            self.packet_len = len;
        }
        if self.allocated {
            if n <= self.packet_len - self.packet_index {
                self.packet.extend_from_slice(buf);
                self.packet_index += n;
                if self.packet_index >= self.packet_len {
                    self.packet_index = 0;
                    return (n as isize, Some(std::mem::take(&mut self.packet)));
                }
            } else {
                // Erroneous size: the payload overruns the unit.
                self.packet_index = 0;
            }
        }
        (n as isize, None)
    }
}

/// One MPEG audio frame header (ff_mpa_decode_header).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MpaHeader {
    pub codec: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    /// Bytes of the frame, header included.
    pub frame_bytes: usize,
    /// Samples per channel the frame decodes to.
    pub samples: i64,
}

/// ff_mpa_check_header + avpriv_mpegaudio_decode_header +
/// ff_mpa_decode_header on a 32-bit header word. Free format (bitrate
/// index 0) is no frame here, as for FFmpeg's parser.
pub(crate) fn mpa_decode_header(h: u32) -> Option<MpaHeader> {
    if (h & 0xFFE0_0000) != 0xFFE0_0000
        || (h & (3 << 19)) == 1 << 19
        || (h & (3 << 17)) == 0
        || (h & (0xF << 12)) == 0xF << 12
        || (h & (3 << 10)) == 3 << 10
    {
        return None;
    }
    const FREQ: [u32; 3] = [44100, 48000, 32000];
    const BITRATE: [[[u32; 15]; 3]; 2] = [
        [
            [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448],
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384],
            [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        ],
        [
            [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256],
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
        ],
    ];
    let (lsf, mpeg25) = if h & (1 << 20) != 0 { (u32::from(h & (1 << 19) == 0), 0) } else { (1, 1) };
    let layer = 4 - ((h >> 17) & 3);
    let sample_rate = FREQ[((h >> 10) & 3) as usize] >> (lsf + mpeg25);
    let bitrate_index = ((h >> 12) & 0xF) as usize;
    let padding = (h >> 9) & 1;
    if bitrate_index == 0 {
        return None;
    }
    let kbps = BITRATE[lsf as usize][(layer - 1) as usize][bitrate_index];
    let frame_bytes = match layer {
        1 => (kbps * 12000 / sample_rate + padding) * 4,
        2 => kbps * 144_000 / sample_rate + padding,
        _ => kbps * 144_000 / (sample_rate << lsf) + padding,
    };
    let (codec, samples) = match layer {
        1 => ("mp1", 384),
        2 => ("mp2", 1152),
        _ => ("mp3", if lsf != 0 { 576 } else { 1152 }),
    };
    let channels = if (h >> 6) & 3 == 3 { 1 } else { 2 };
    Some(MpaHeader { codec, sample_rate, channels, frame_bytes: frame_bytes as usize, samples })
}

/// mpegaudio_parser.c: MPEG audio layer 1/2/3 frames.
pub(crate) struct MpegAudio {
    pc: Combine,
    header: u32,
    header_count: i32,
    frame_size: usize,
    /// The codec context's codec, which headers of another layer only
    /// replace once two agree.
    pub codec: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    duration: i64,
}

impl MpegAudio {
    pub fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    pub fn new(codec: &'static str) -> Self {
        Self { pc: Combine::default(), header: 0, header_count: 0, frame_size: 0, codec, sample_rate: 0, channels: 0, duration: 0 }
    }

    /// The parser av_parser_init makes after a seek: the codec context it
    /// sets (codec, sample rate, channels) outlives the parser.
    pub fn reset(&self) -> Self {
        Self { sample_rate: self.sample_rate, channels: self.channels, ..Self::new(self.codec) }
    }
}

impl Split for MpegAudio {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        /// header + layer + frequency + lsf/mpeg25
        const SAME_HEADER_MASK: u32 = 0xFFE0_0000 | (3 << 17) | (3 << 10) | (3 << 19);
        let mut state = self.pc.state;
        let mut next = END_NOT_FOUND;
        let flush = buf.is_empty();
        let mut i = 0;
        while i < buf.len() {
            if self.frame_size > 0 {
                let inc = (buf.len() - i).min(self.frame_size);
                i += inc;
                self.frame_size -= inc;
                state = 0;
                if self.frame_size == 0 {
                    next = i as isize;
                    break;
                }
            } else {
                while i < buf.len() {
                    state = (state << 8) | u32::from(buf[i]);
                    i += 1;
                    let Some(h) = mpa_decode_header(state) else {
                        if i > 4 {
                            self.header_count = -2;
                        }
                        continue;
                    };
                    let threshold = i32::from(self.codec != h.codec);
                    if (state & SAME_HEADER_MASK) != (self.header & SAME_HEADER_MASK) && self.header != 0 {
                        self.header_count = -3;
                    }
                    self.header = state;
                    self.header_count += 1;
                    self.frame_size = h.frame_bytes - 4;
                    if self.header_count > threshold {
                        self.sample_rate = h.sample_rate;
                        self.channels = h.channels;
                        self.duration = h.samples;
                        self.codec = h.codec;
                    }
                    break;
                }
            }
        }
        self.pc.state = state;
        let Some(unit) = self.pc.combine(next, buf) else {
            return (buf.len() as isize, None);
        };
        // ID3v1 and APE tags at the end of the input are no frames.
        if flush && ((unit.len() >= 128 && unit.starts_with(b"TAG")) || (unit.len() >= 32 && unit.starts_with(b"APETAGEX"))) {
            return (next, None);
        }
        (next, Some(unit))
    }

    fn audio(&self) -> (i64, u32) {
        (self.duration, self.sample_rate)
    }

    /// get_audio_frame_duration: MP1 and MP2 frames have fixed sizes, MP3
    /// frames one by sample rate.
    fn fallback(&self) -> i64 {
        match self.codec {
            "mp1" => 384,
            "mp2" => 1152,
            "mp3" if self.sample_rate > 0 => if self.sample_rate <= 24000 { 576 } else { 1152 },
            _ => 0,
        }
    }
}

/// E-AC-3 frame types (ac3defs.h)
const EAC3_FRAME_TYPE_DEPENDENT: u8 = 1;
const EAC3_FRAME_TYPE_AC3_CONVERT: u8 = 2;
/// AC3_HEADER_SIZE
const AC3_HEADER_SIZE: isize = 7;

/// ac3_sync: a syncframe header in the last seven bytes of `state`
/// (either byte order): its size, whether it starts a new frame (not an
/// E-AC-3 dependent substream) and whether another header must follow.
fn ac3_sync(state: u64) -> Option<(usize, bool, bool)> {
    let mut b = state.to_be_bytes();
    if b[1] == 0x77 && b[2] == 0x0B {
        b.swap(1, 2);
        b.swap(3, 4);
        b.swap(5, 6);
    }
    let h = crate::ac3::parse_ac3_header(&b[1..])?;
    let new_frame_start = h.frame_type != EAC3_FRAME_TYPE_DEPENDENT;
    let need_next_header = new_frame_start || h.frame_type != EAC3_FRAME_TYPE_AC3_CONVERT;
    Some((h.frame_size, new_frame_start, need_next_header))
}

/// ff_ac3_find_syncword: the first 0x0B77 or 0x770B at an even or odd
/// offset, as FFmpeg scans for it.
fn ac3_find_syncword(buf: &[u8]) -> Option<usize> {
    let mut i = 1;
    while i < buf.len() {
        if buf[i] == 0x77 || buf[i] == 0x0B {
            if buf[i] ^ buf[i - 1] == 0x77 ^ 0x0B {
                return Some(i - 1);
            }
            if buf.get(i + 1).is_some_and(|&b| buf[i] ^ b == 0x77 ^ 0x0B) {
                return Some(i);
            }
        }
        i += 2;
    }
    None
}

/// aac_ac3_parser.c with ac3_parser.c: AC-3 / E-AC-3 frames. An E-AC-3
/// frame takes its dependent substreams along: a unit runs to the next
/// header that starts a frame.
pub(crate) struct Ac3 {
    pc: Combine,
    state: u64,
    remaining_size: isize,
    need_next_header: bool,
    /// The codec context's codec: E-AC-3 once a unit's header says so.
    pub codec: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    duration: i64,
}

impl Ac3 {
    pub fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    pub fn new(codec: &'static str) -> Self {
        Self {
            pc: Combine::default(),
            state: 0,
            remaining_size: 0,
            need_next_header: false,
            codec,
            sample_rate: 0,
            channels: 0,
            duration: 0,
        }
    }

    /// The parser av_parser_init makes after a seek: the codec context it
    /// sets (codec, sample rate, channels) outlives the parser.
    pub fn reset(&self) -> Self {
        Self { sample_rate: self.sample_rate, channels: self.channels, ..Self::new(self.codec) }
    }

    /// The unit's last syncframe sets the codec context, when its CRC
    /// holds (A/52 6.1.2: the sync word alone is no proof).
    fn inspect(&mut self, unit: &[u8]) {
        let Some(offset) = ac3_find_syncword(unit) else { return };
        let mut buf = &unit[offset..];
        let header = loop {
            let Some(h) = crate::ac3::parse_ac3_header(buf).filter(|h| h.frame_size <= buf.len()) else {
                return;
            };
            if buf.len() > h.frame_size {
                buf = &buf[h.frame_size..];
                continue;
            }
            if crate::ac3::crc16_ansi(&buf[2..h.frame_size]) != 0 {
                return;
            }
            break h;
        };
        self.sample_rate = header.sample_rate;
        if header.bitstream_id > 10 {
            self.codec = "eac3";
        }
        self.channels = header.channels;
        self.duration = i64::from(header.num_blocks) * 256;
    }
}

impl Split for Ac3 {
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let n = buf.len() as isize;
        let mut got_frame = false;
        let mut i;
        loop {
            i = END_NOT_FOUND;
            if self.remaining_size <= n {
                if self.remaining_size != 0 && !self.need_next_header {
                    i = self.remaining_size;
                    self.remaining_size = 0;
                } else {
                    // We need a header first.
                    let mut found = None;
                    i = self.remaining_size;
                    while i < n {
                        self.state = (self.state << 8) | u64::from(buf[i as usize]);
                        if let Some(sync) = ac3_sync(self.state) {
                            found = Some(sync);
                            break;
                        }
                        i += 1;
                    }
                    match found {
                        None => i = END_NOT_FOUND,
                        Some((len, new_frame_start, need_next_header)) => {
                            self.need_next_header = need_next_header;
                            got_frame = true;
                            self.state = 0;
                            i -= AC3_HEADER_SIZE - 1;
                            self.remaining_size = len as isize;
                            if !new_frame_start || self.pc.index as isize + i <= 0 {
                                self.remaining_size += i;
                                continue;
                            } else if i < 0 {
                                self.remaining_size += i;
                            }
                        }
                    }
                }
            }
            break;
        }
        let Some(unit) = self.pc.combine(i, buf) else {
            self.remaining_size -= self.remaining_size.min(n);
            return (n, None);
        };
        // FFmpeg reads the parameters of a unit cut at the next header.
        // The unit flushed at the end of the input (`buf` empty) has its
        // own header too: read it rather than keep the frame before's,
        // or a single frame stays untimed and a last frame with fewer
        // blocks lasts as long as the one before it. Without a valid
        // header both keep FFmpeg's values.
        if got_frame || buf.is_empty() {
            self.inspect(&unit);
        }
        (i, Some(unit))
    }

    fn audio(&self) -> (i64, u32) {
        (self.duration, self.sample_rate)
    }

    /// get_audio_frame_duration: AC-3 frames have a fixed size; E-AC-3 has
    /// none to fall back on.
    fn fallback(&self) -> i64 {
        if self.codec == "ac3" { 1536 } else { 0 }
    }
}

/// RELATIVE_TS_BASE (avformat_internal.h): a stream's timestamps before
/// it has one of its own count from here; av_read_frame hands them out
/// counted from zero.
const RELATIVE_TS_BASE: i64 = i64::MAX - (1 << 48);

fn is_relative(ts: i64) -> bool {
    ts >= RELATIVE_TS_BASE - (1 << 48)
}

/// A timestamp as av_read_frame returns it.
pub(crate) fn returned(ts: Option<i64>) -> Option<i64> {
    ts.map(|t| if is_relative(t) { t - RELATIVE_TS_BASE } else { t })
}

/// av_rescale_rnd(a, b, c, AV_ROUND_DOWN) for non-negative operands.
fn rescale_down(a: i64, b: i64, c: i64) -> i64 {
    if a < 0 || b <= 0 || c <= 0 {
        return 0;
    }
    i64::try_from(i128::from(a) * i128::from(b) / i128::from(c)).unwrap_or(i64::MAX)
}

/// av_rescale_q for positive time bases (num, den): to nearest, ties away
/// from zero.
fn rescale_q(a: i64, b: (i64, i64), c: (i64, i64)) -> i64 {
    let num = i128::from(a) * i128::from(b.0) * i128::from(c.1);
    let den = i128::from(b.1) * i128::from(c.0);
    if den <= 0 {
        return a;
    }
    let q = (num.abs() + den / 2) / den;
    i64::try_from(if num < 0 { -q } else { q }).unwrap_or(i64::MAX)
}

/// av_add_stable(ts_tb, ts, inc_tb, 1): `ts` moved on by `inc_tb` without
/// accumulating rounding errors; where a fractional tick count rounds
/// depends on `ts` itself.
fn add_stable(ts: i64, ts_tb: (i64, i64), inc_tb: (i64, i64)) -> i64 {
    let m = i128::from(inc_tb.0) * i128::from(ts_tb.1);
    let d = i128::from(inc_tb.1) * i128::from(ts_tb.0);
    if d <= 0 {
        return ts;
    }
    if m % d == 0 {
        return ts.saturating_add(i64::try_from(m / d).unwrap_or(i64::MAX));
    }
    if m < d {
        return ts;
    }
    let old = rescale_q(ts, ts_tb, inc_tb);
    let old_ts = rescale_q(old, inc_tb, ts_tb);
    rescale_q(old.saturating_add(1), inc_tb, ts_tb).saturating_add(ts.saturating_sub(old_ts))
}

/// compute_pkt_fields (demux.c) for a parsed audio stream (no decoder
/// delay, one frame per packet), with the parse_packet duration of each
/// unit.
pub(crate) struct AudioClock {
    /// The stream time base.
    num: i64,
    den: i64,
    /// pts_wrap_bits
    wrap_bits: u32,
    cur_dts: i64,
    first_dts: Option<i64>,
    initial_durations_done: bool,
}

impl AudioClock {
    pub fn new(num: i64, den: i64, wrap_bits: u32) -> Self {
        Self { num, den, wrap_bits, cur_dts: RELATIVE_TS_BASE, first_dts: None, initial_durations_done: false }
    }

    /// ff_read_frame_flush then avpriv_update_cur_dts: after a seek the
    /// clock runs on from the landing timestamp `ts`, which is absolute,
    /// so no later packet revises those before it.
    pub fn seeked(&mut self, ts: i64) {
        self.cur_dts = ts;
    }

    /// The packet of `unit` on stream `index`, timed. `queue` holds the
    /// packets not yet returned (FFmpeg's packet buffer): FFmpeg revises
    /// this stream's ones when the unit brings its first timestamp or
    /// duration.
    pub fn stamp(&mut self, unit: Unit, index: u32, time_base: oxideav_core::TimeBase, queue: &mut VecDeque<Packet>) -> Packet {
        let mut pts = unit.pts;
        let mut dts = unit.dts;
        // parse_packet's duration from the parser; without one,
        // compute_frame_duration's from the codec context, which moves the
        // clock by the exact fraction (av_add_stable).
        let (samples, exact) = if unit.samples > 0 { (unit.samples, false) } else { (unit.fallback, true) };
        let rate = i64::from(unit.sample_rate);
        let duration = if rate > 0 && samples > 0 { rescale_down(samples, self.den, self.num * rate) } else { 0 };
        if let (Some(p), Some(d)) = (pts, dts) {
            let wrap = 1i64 << self.wrap_bits;
            if self.wrap_bits < 63 && d > i64::MIN + wrap && d - (wrap >> 1) > p {
                if is_relative(self.cur_dts) || d - (wrap >> 1) > self.cur_dts {
                    dts = Some(d - wrap);
                } else {
                    pts = Some(p + wrap);
                }
            }
        }
        if duration > 0 && !queue.is_empty() {
            self.update_initial_durations(queue, index, duration);
        }
        if pts.is_some() || dts.is_some() || duration > 0 {
            if pts.is_none() {
                pts = dts;
            }
            if let Some(p) = pts {
                self.update_initial_timestamps(p, queue, index);
            }
            let p = pts.unwrap_or(self.cur_dts);
            pts = Some(p);
            dts = Some(p);
            self.cur_dts = if exact && duration > 0 {
                add_stable(p, (self.num, self.den), (samples, rate))
            } else {
                // av_add_stable of a whole number of ticks
                p.saturating_add(duration)
            };
        }
        if let Some(d) = dts {
            self.cur_dts = self.cur_dts.max(d);
        }
        let mut packet = Packet::new(index, time_base, unit.data);
        packet.pts = pts;
        packet.dts = dts;
        packet.duration = (duration > 0).then_some(duration);
        packet.flags.keyframe = true;
        packet
    }

    /// update_initial_timestamps: the first timestamp of the stream turns
    /// the relative ones of its waiting packets absolute.
    fn update_initial_timestamps(&mut self, dts: i64, queue: &mut VecDeque<Packet>, index: u32) {
        if self.first_dts.is_some()
            || self.cur_dts < i64::from(i32::MIN) + RELATIVE_TS_BASE
            || dts < i64::from(i32::MIN) + (self.cur_dts - RELATIVE_TS_BASE)
            || is_relative(dts)
        {
            return;
        }
        let first = dts - (self.cur_dts - RELATIVE_TS_BASE);
        self.first_dts = Some(first);
        self.cur_dts = dts;
        let shift = first.wrapping_sub(RELATIVE_TS_BASE);
        for packet in queue.iter_mut().filter(|p| p.stream_index == index) {
            for ts in [&mut packet.pts, &mut packet.dts] {
                if let Some(t) = ts.as_mut().filter(|t| is_relative(**t)) {
                    *t = t.wrapping_add(shift);
                }
            }
        }
    }

    /// update_initial_durations: waiting packets without a duration take
    /// this one, and their timestamps follow from it.
    fn update_initial_durations(&mut self, queue: &mut VecDeque<Packet>, index: u32, duration: i64) {
        let mut cur_dts = RELATIVE_TS_BASE;
        if let Some(first_dts) = self.first_dts {
            if self.initial_durations_done {
                return;
            }
            self.initial_durations_done = true;
            cur_dts = first_dts;
            let mut anchor = None;
            for packet in queue.iter().filter(|p| p.stream_index == index) {
                if packet.pts != packet.dts || packet.dts.is_some() || packet.duration.is_some() {
                    anchor = Some(packet.dts);
                    break;
                }
                cur_dts -= duration;
            }
            if anchor != Some(Some(first_dts)) {
                return;
            }
            self.first_dts = Some(cur_dts);
        } else if self.cur_dts != RELATIVE_TS_BASE {
            return;
        }
        let mut all = true;
        for packet in queue.iter_mut().filter(|p| p.stream_index == index) {
            let dts_open = packet.dts.is_none() || packet.dts == self.first_dts || packet.dts == Some(RELATIVE_TS_BASE);
            if (packet.pts == packet.dts || packet.pts.is_none()) && dts_open && packet.duration.is_none() && cur_dts.checked_add(duration).is_some() {
                packet.dts = Some(cur_dts);
                packet.pts = Some(cur_dts);
                packet.duration = Some(duration);
            } else {
                all = false;
                break;
            }
            cur_dts = packet.dts.unwrap_or(cur_dts) + packet.duration.unwrap_or(0);
        }
        if all {
            self.cur_dts = cur_dts;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(parser: &mut Parser<DvdSub>, packets: &[(&[u8], Option<i64>)]) -> Vec<(Vec<u8>, Option<i64>)> {
        let mut out = Vec::new();
        for (i, (data, pts)) in packets.iter().enumerate() {
            parser.push(data, *pts, *pts, 1000 + 2048 * i as i64, &mut out);
        }
        parser.flush(&mut out);
        out.into_iter().map(|u| (u.data, u.pts)).collect()
    }

    #[test]
    fn spu_spanning_packets_is_one_unit_with_the_first_packets_pts() {
        let spu: Vec<u8> = [0x00, 0x0A].iter().copied().chain(2..10).collect();
        let mut parser = Parser::new(DvdSub::default());
        let got = units(&mut parser, &[(&spu[..4], Some(900)), (&spu[4..7], None), (&spu[7..], None)]);
        assert_eq!(got, vec![(spu, Some(900))]);
    }

    #[test]
    fn payload_overrunning_its_spu_is_dropped_and_the_next_spu_starts_clean() {
        let mut parser = Parser::new(DvdSub::default());
        let overrun = [0x00, 0x04, 1, 2, 3];
        let next = [0x00, 0x03, 7];
        let got = units(&mut parser, &[(&overrun[..], Some(10)), (&next[..], Some(20))]);
        // av_parser_parse2 fetches timestamps again only after it output
        // a unit, so the unit after a dropped one keeps the stale fetch.
        assert_eq!(got, vec![(next.to_vec(), Some(10))]);
    }

    #[test]
    fn oversized_spu_is_not_assembled() {
        let mut parser = Parser::new(DvdSub::default());
        let header = [0x00, 0x00, 0x7F, 0xFF, 0xFF, 0xFF, 1, 2];
        let got = units(&mut parser, &[(&header[..], Some(10))]);
        assert!(got.is_empty());
    }
}

// Ported from FFmpeg (commit 2da55bf): libavformat/rawdec.c
// (ff_raw_video_read_header, ff_raw_read_partial_packet) and the frame
// splitting and key-frame rules of libavcodec/h264_parser.c
// (h264_find_frame_end, parse_nal_units, with h264_sei.c for recovery
// points and h264_ps.c for reference counts), hevc/parser.c
// (hevc_find_frame_end, parse_nal_units, see hevc_parse.rs) and
// mpegvideo_parser.c (mpeg1_find_frame_end, mpegvideo_extract_headers),
// with avpriv_find_start_code (utils.c).
// License: LGPL-2.1-or-later
//
// Raw video elementary streams: the input is read in 1024-byte pieces and
// cut into the access units FFmpeg's parser for the codec cuts, each
// flagged key as FFmpeg flags it. The streams carry no timestamps: MPEG-1/2
// units are timed as FFmpeg's demuxer layer times them (demux.c
// compute_pkt_fields), H.264 and HEVC units stay untimed, as FFmpeg's do,
// with the duration their parser's frame rate gives.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{Demuxer, Error, Packet, ReadSeek, Result, StreamInfo};

use crate::h264_parse::H264Parse;
use crate::hevc_parse::HevcParse;
use crate::parser::{Combine, Parser, Split, Unit, VideoCut, END_NOT_FOUND};
use demux_seek_core::{read_on, Allowance, Index};

/// ff_raw_demuxer_class raw_packet_size
const RAW_PACKET_SIZE: usize = 1024;

/// A raw video demuxer over the splitter of its codec.
pub(crate) struct RawVideoDemuxer<S> {
    format: &'static str,
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    parser: Parser<S>,
    queue: VecDeque<Packet>,
    /// Where each queued unit starts in the input (its frame_offset).
    positions: VecDeque<i64>,
    pos: i64,
    count: i64,
    eof: bool,
    /// AVFMT_GENERIC_INDEX: the key units returned so far.
    index: Index,
    /// What the seek under way may still read.
    allowance: Allowance,
}

/// Where reading was, given back when a seek fails.
struct Reading<S> {
    at: u64,
    parser: Parser<S>,
    queue: VecDeque<Packet>,
    positions: VecDeque<i64>,
    pos: i64,
    count: i64,
    eof: bool,
}

impl<S: Split + Units + Send> RawVideoDemuxer<S> {
    pub fn new(format: &'static str, input: Box<dyn ReadSeek>, stream: StreamInfo, split: S) -> Self {
        let allowance = Allowance::default();
        Self {
            format,
            input: Box::new(allowance.meter(input)),
            streams: vec![stream],
            parser: Parser::new(split),
            queue: VecDeque::new(),
            positions: VecDeque::new(),
            pos: 0,
            count: 0,
            eof: false,
            index: Index::default(),
            allowance,
        }
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
        self.allowance.spend(1, 0)?;
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
            let stamp = self.parser.split.unit(&unit, self.count);
            let mut packet = Packet::new(0, self.streams[0].time_base, unit.data);
            packet.pts = stamp.pts;
            packet.dts = stamp.dts;
            packet.duration = stamp.duration;
            packet.flags.keyframe = stamp.key;
            self.count += 1;
            self.queue.push_back(packet);
            self.positions.push_back(unit.pos);
        }
        self.parser.split.read_done();
        Ok(())
    }

    /// Reads on from `pos` with a new parser (ff_read_frame_flush) and
    /// the clock at `ts` (avpriv_update_cur_dts), or where a flush leaves
    /// it (no `ts`, the seek to the start of the data).
    fn restart(&mut self, pos: i64, ts: Option<i64>) -> Result<()> {
        let split = self.parser.split.reset(ts).ok_or_else(|| Error::unsupported("raw video: no seek"))?;
        self.input.seek(SeekFrom::Start(pos as u64))?;
        self.parser = Parser::new(split);
        self.queue.clear();
        self.positions.clear();
        self.pos = pos;
        self.eof = false;
        Ok(())
    }

    /// seek_frame_generic from the index search's result `found`.
    fn land(&mut self, timestamp: i64, mut found: Option<usize>) -> Result<i64> {
        if found.is_none() || found == Some(self.index.entries().len() - 1) {
            match self.index.entries().last().copied() {
                Some(last) => self.restart(last.pos, Some(last.timestamp))?,
                None => self.restart(0, None)?,
            }
            read_on(timestamp, || self.next_packet().map(|p| (p.flags.keyframe, p.dts)))?;
            found = self.index.search(timestamp, true);
        }
        let Some(i) = found else {
            return Err(Error::invalid("raw video: no key frame to seek to"));
        };
        let e = self.index.entries()[i];
        self.restart(e.pos, Some(e.timestamp))?;
        Ok(e.timestamp)
    }
}

impl<S: Split + Units + Send> Demuxer for RawVideoDemuxer<S> {
    fn format_name(&self) -> &str {
        self.format
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = self.queue.pop_front() {
                let pos = self.positions.pop_front().unwrap_or(-1);
                // av_read_frame indexes every key packet it returns.
                if let (true, Some(dts)) = (packet.flags.keyframe, packet.dts) {
                    self.index.add(pos, dts, 0, 0, true);
                }
                return Ok(packet);
            }
            if self.eof {
                return Err(Error::Eof);
            }
            self.read_piece()?;
        }
    }

    /// seek.c seek_frame_generic with AVSEEK_FLAG_BACKWARD (rawdec.h
    /// FF_DEF_RAWVIDEO_DEMUXER: AVFMT_GENERIC_INDEX): the last key unit at
    /// or before the target among those returned so far; past the last of
    /// them units are read on, within the seek's allowance, until a key
    /// unit starts after the target or more than 1000 others did. A seek
    /// that fails leaves reading where it was. FFmpeg cannot seek raw
    /// H.264 or HEVC: compute_pkt_fields gives their packets no dts
    /// (demux.c:993, onein_oneout), ff_add_index_entry rejects an entry
    /// without one (seek.c:76), so seek_frame_generic finds no index entry
    /// (seek.c:581) and fails, as `ffprobe -read_intervals` reports.
    fn seek_to(&mut self, _stream_index: u32, timestamp: i64) -> Result<i64> {
        let Some(fresh) = self.parser.split.reset(None) else {
            return Err(Error::unsupported(format!(
                "{}: FFmpeg's raw demuxer gives these packets no timestamps to seek by",
                self.format
            )));
        };
        let found = self.index.search(timestamp, true);
        if found.is_none() && self.index.entries().first().is_some_and(|e| timestamp < e.timestamp) {
            return Err(Error::invalid("raw video: seek before the first key frame"));
        }
        let reading = Reading {
            at: self.input.stream_position()?,
            parser: std::mem::replace(&mut self.parser, Parser::new(fresh)),
            queue: std::mem::take(&mut self.queue),
            positions: std::mem::take(&mut self.positions),
            pos: self.pos,
            count: self.count,
            eof: self.eof,
        };
        self.allowance.start();
        let landed = self.land(timestamp, found);
        let landed = self.allowance.finish(landed);
        if landed.is_err() {
            self.input.seek(SeekFrom::Start(reading.at))?;
            (self.parser, self.queue, self.positions) = (reading.parser, reading.queue, reading.positions);
            (self.pos, self.count, self.eof) = (reading.pos, reading.count, reading.eof);
        }
        landed
    }
}

/// What FFmpeg makes of a unit once its codec's parser cut it: the key
/// flag parse_packet sets from the parser's key_frame and pict_type, and
/// the timestamps its demuxer layer gives the packet.
pub(crate) trait Units {
    fn buffered_bytes(&self) -> usize;
    /// The `index`th unit of the stream, counting from 0.
    fn unit(&mut self, unit: &Unit, index: i64) -> Stamp;
    /// The 1024-byte read whose units were just stamped is over.
    fn read_done(&mut self) {}
    /// The splitter a seek leaves (ff_read_frame_flush makes a new parser;
    /// the codec context and the stream's timing state stay), its clock
    /// at `ts` (avpriv_update_cur_dts) when the seek sets one. `None`
    /// where FFmpeg cannot seek these units.
    fn reset(&self, _ts: Option<i64>) -> Option<Self>
    where
        Self: Sized,
    {
        None
    }
}

/// A unit's key flag, and its pts, dts and duration in the stream time
/// base.
pub(crate) struct Stamp {
    key: bool,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
}

impl Stamp {
    /// A unit's key flag, pts, dts and duration.
    pub(crate) fn new(key: bool, pts: Option<i64>, dts: Option<i64>, duration: Option<i64>) -> Self {
        Self { key, pts, dts, duration }
    }

    /// A unit of a raw H.264 or HEVC stream as av_read_frame returns it:
    /// no pts or dts (compute_pkt_fields does not interpolate either
    /// codec), the key flag and duration its parser gave. Where the
    /// parser has no frame rate, compute_frame_duration falls back to the
    /// raw demuxer's 25 fps (avg_frame_rate, AVFMT_NOTIMESTAMPS), as FFmpeg
    /// does while analysing the stream; FFmpeg times the packets after
    /// that by its r_frame_rate guess, one tick of 1/1200000, which plays
    /// as no duration and is not modelled.
    fn untimed(video: VideoCut) -> Self {
        let duration = if video.rate_known { video.duration } else { RAW_VIDEO_CLOCK / 25 };
        Self { key: video.key, pts: None, dts: None, duration: (duration > 0).then_some(duration) }
    }
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
    clock: MpegClock,
}

impl Default for MpegVideo {
    fn default() -> Self {
        Self { pc: Combine::default(), frame_start_found: 0, pict_type: 1, clock: MpegClock::default() }
    }
}

/// ff_mpeg12_frame_rate_tab (mpeg12framerate.c).
pub(crate) const FRAME_RATES: [(i64, i64); 16] = [
    (0, 0), (24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (50, 1), (60000, 1001),
    (60, 1), (15, 1), (5, 1), (10, 1), (12, 1), (15, 1), (0, 0), (0, 0),
];

/// AV_PICTURE_TYPE_B
const PICTURE_TYPE_B: u8 = 3;

/// ff_raw_video_read_header's time base: 1/1200000.
pub(crate) const RAW_VIDEO_CLOCK: i64 = 1_200_000;

/// RELATIVE_TS_BASE (avformat_internal.h): the dts FFmpeg starts a stream
/// without timestamps from. av_read_frame returns a timestamp relative to
/// it (is_relative: above RELATIVE_TS_BASE - 2^48) less it.
pub(crate) const RELATIVE_TS_BASE: i64 = i64::MAX - (1 << 48);

/// How FFmpeg times raw MPEG-1/2 video, which carries no timestamps: what
/// mpegvideo_extract_headers leaves in the parser and codec contexts, and
/// compute_pkt_fields (demux.c) on each parsed unit, in 1/1200000.
///
/// With B-frame delay (has_b_frames), an I- or P-frame's pts is unknown
/// and its dts is the current one, which then moves on by the duration of
/// the I- or P-frame before it; a B-frame's pts and dts are the current
/// dts, which moves on by its own duration (av_add_stable). Without delay
/// every frame is timed like a B-frame. A duration counts fields:
/// 1/(2 x frame rate), times 1 + repeat_pict. The dts runs from
/// RELATIVE_TS_BASE, the origin av_add_stable rounds a fractional tick
/// count from in FFmpeg, and comes out less it.
///
/// FFmpeg's decoder, which avformat_find_stream_info runs on the first
/// picture after a sequence header once the read that ended it is
/// parsed, sets the delay, frame rate and codec from that picture's
/// sequence (mpeg_decode_postinit): they hold from the next read on, over
/// those of any later sequence header that read held.
struct MpegClock {
    /// The last sequence header's frame_rate_code, once there is one.
    frame_rate_code: Option<usize>,
    /// pc->frame_rate: the last sequence header's frame rate.
    frame_rate: (i64, i64),
    /// s1->frame_rate_ext: the last sequence extension's factors.
    frame_rate_ext: (i64, i64),
    /// avctx->framerate: with its sequence extension's factors.
    framerate: (i64, i64),
    progressive_sequence: bool,
    /// avctx->codec_id is MPEG-2: a sequence extension followed the last
    /// sequence header. The raw demuxer starts out MPEG-1.
    mpeg2: bool,
    /// AVCodecParserContext.repeat_pict
    repeat_pict: i64,
    /// avctx->has_b_frames
    has_b_frames: bool,
    /// The last sequence extension's low_delay, which FFmpeg's decoder
    /// turns into has_b_frames.
    low_delay: bool,
    discovery: Discovery,
    /// sti->cur_dts and sti->last_IP_duration
    cur_dts: i64,
    last_ip_duration: i64,
}

/// FFmpeg's decoder during avformat_find_stream_info.
#[derive(Clone, Copy)]
enum Discovery {
    /// No picture after a sequence header parsed yet.
    NoPicture,
    /// The first one is parsed, in the read not over yet: what the decoder
    /// sets from its sequence once it is.
    Pending(DecoderTiming),
    /// The decoder has set it.
    Done,
}

/// What mpeg_decode_postinit sets: avctx->has_b_frames, framerate, and
/// codec_id (MPEG-2 or not).
#[derive(Clone, Copy)]
struct DecoderTiming {
    has_b_frames: bool,
    framerate: (i64, i64),
    mpeg2: bool,
}

impl Default for MpegClock {
    fn default() -> Self {
        Self {
            frame_rate_code: None,
            frame_rate: (0, 0),
            frame_rate_ext: (1, 1),
            // avcodec_alloc_context3
            framerate: (0, 1),
            progressive_sequence: false,
            mpeg2: false,
            repeat_pict: 0,
            has_b_frames: false,
            low_delay: false,
            discovery: Discovery::NoPicture,
            cur_dts: RELATIVE_TS_BASE,
            last_ip_duration: 0,
        }
    }
}

impl MpegClock {
    /// compute_frame_duration in seconds, (0, 0) when it gives none. A
    /// stream without a known frame rate falls back to the raw demuxer's
    /// framerate option, 25 (AVFMT_NOTIMESTAMPS), as FFmpeg does during
    /// stream discovery; afterwards FFmpeg would use its r_frame_rate
    /// estimate, which is not modelled. Both MPEG codecs have
    /// AV_CODEC_PROP_FIELDS: a tick is a field.
    fn frame_duration(&self) -> (i64, i64) {
        let (num, den) = self.framerate;
        if num == 0 {
            return (1, 25);
        }
        if den.saturating_mul(1000) <= num {
            return (0, 0);
        }
        let field = (den, num.saturating_mul(2));
        if self.repeat_pict != 0 { (field.0.saturating_mul(1 + self.repeat_pict), field.1) } else { field }
    }

    /// compute_pkt_fields for a unit of picture type `pict_type`: its pts,
    /// dts and duration, as av_read_frame returns them.
    fn stamp(&mut self, pict_type: u8) -> (Option<i64>, Option<i64>, Option<i64>) {
        if pict_type == PICTURE_TYPE_B {
            self.has_b_frames = true;
        }
        let (num, den) = self.frame_duration();
        // av_rescale_rnd(1, num * tb.den, den * tb.num, AV_ROUND_DOWN)
        let duration = if num > 0 && den > 0 {
            i64::try_from(i128::from(num) * i128::from(RAW_VIDEO_CLOCK) / i128::from(den)).unwrap_or(i64::MAX)
        } else {
            0
        };
        let known = (duration > 0).then_some(duration);
        if self.has_b_frames && pict_type != PICTURE_TYPE_B {
            // Presentation delayed: the dts is the current one, and the
            // next follows the I- or P-frame shown before this one.
            let dts = self.cur_dts;
            if self.last_ip_duration == 0 {
                self.last_ip_duration = duration;
            }
            self.cur_dts = dts.saturating_add(self.last_ip_duration);
            self.last_ip_duration = duration;
            (None, Some(returned(dts)), known)
        } else if duration > 0 {
            let pts = self.cur_dts;
            self.cur_dts = add_stable(pts, num, den);
            (Some(returned(pts)), Some(returned(pts)), known)
        } else {
            (None, None, None)
        }
    }

    /// What mpeg_decode_postinit sets from the sequence parsed last, of
    /// frame_rate_code `code`: the delay low_delay leaves, and the code's
    /// frame rate (24000/1001 for a code the decoder rejects), times the
    /// extension's factors for MPEG-2.
    fn decoder_timing(&self, code: usize) -> DecoderTiming {
        let code = if code == 0 || code > 13 { 1 } else { code };
        let (num, den) = FRAME_RATES[code];
        let framerate = if self.mpeg2 { (num * self.frame_rate_ext.0, den * self.frame_rate_ext.1) } else { (num, den) };
        DecoderTiming { has_b_frames: !self.low_delay, framerate, mpeg2: self.mpeg2 }
    }
}

/// A timestamp as av_read_frame returns it: less RELATIVE_TS_BASE when
/// relative to it.
pub(crate) fn returned(ts: i64) -> i64 {
    if ts > RELATIVE_TS_BASE - (1 << 48) { ts - RELATIVE_TS_BASE } else { ts }
}

/// av_rescale_q(a, b, c), rounding to nearest with ties away from zero.
pub(crate) fn rescale(a: i64, b: (i64, i64), c: (i64, i64)) -> i64 {
    let num = i128::from(a) * i128::from(b.0) * i128::from(c.1);
    let den = i128::from(b.1) * i128::from(c.0);
    if den == 0 {
        return 0;
    }
    let q = (num.abs() + den / 2) / den;
    i64::try_from(if num < 0 { -q } else { q }).unwrap_or(i64::MAX)
}

/// av_add_stable(1/1200000, ts, num/den, 1): `ts` moved on by num/den
/// seconds without accumulating rounding errors. Where a fractional tick
/// count rounds depends on `ts` itself, not only on how far it moved.
pub(crate) fn add_stable(ts: i64, num: i64, den: i64) -> i64 {
    let clock = (1, RAW_VIDEO_CLOCK);
    let (m, d) = (i128::from(num) * i128::from(RAW_VIDEO_CLOCK), i128::from(den));
    if m % d == 0 {
        return i64::try_from(i128::from(ts) + m / d).unwrap_or(i64::MAX);
    }
    if m < d {
        return ts;
    }
    let old = rescale(ts, clock, (num, den));
    let old_ts = rescale(old, (num, den), clock);
    rescale(old.saturating_add(1), (num, den), clock).saturating_add(ts - old_ts)
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

impl MpegVideo {
    /// mpegvideo_extract_headers, up to a unit's first slice: its picture
    /// type, and the frame rate, B-frame delay and repeated fields it
    /// declares. True when it has a picture header.
    fn extract_headers(&mut self, unit: &[u8]) -> bool {
        let clock = &mut self.clock;
        let mut picture = false;
        // picture coding extensions: two make a field pair
        let mut pic_ext = 0;
        let mut p = 0;
        while p < unit.len() {
            let mut code = u32::MAX;
            p = find_start_code(unit, p, unit.len(), &mut code);
            let b = &unit[p..];
            match code {
                PICTURE_START_CODE => {
                    if b.len() >= 2 {
                        self.pict_type = (b[1] >> 3) & 7;
                        picture = true;
                    }
                }
                SEQ_START_CODE => {
                    if b.len() >= 7 {
                        let code = usize::from(b[3] & 0x0F);
                        clock.frame_rate_code = Some(code);
                        clock.frame_rate = FRAME_RATES[code];
                        clock.framerate = clock.frame_rate;
                        clock.mpeg2 = false;
                    }
                }
                EXT_START_CODE if !b.is_empty() => match b[0] >> 4 {
                    // sequence extension
                    1 if b.len() >= 6 => {
                        let (ext_n, ext_d) = (i64::from((b[5] >> 5) & 3), i64::from(b[5] & 0x1F));
                        clock.progressive_sequence = b[1] & (1 << 3) != 0;
                        clock.low_delay = b[5] >> 7 != 0;
                        clock.has_b_frames = !clock.low_delay;
                        clock.frame_rate_ext = (ext_n + 1, ext_d + 1);
                        clock.framerate = (clock.frame_rate.0 * clock.frame_rate_ext.0, clock.frame_rate.1 * clock.frame_rate_ext.1);
                        clock.mpeg2 = true;
                    }
                    // picture coding extension
                    8 if b.len() >= 5 => {
                        let top_field_first = b[3] & (1 << 7) != 0;
                        let repeat_first_field = b[3] & (1 << 1) != 0;
                        let progressive_frame = b[4] & (1 << 7) != 0;
                        clock.repeat_pict = 1;
                        if repeat_first_field {
                            if clock.progressive_sequence {
                                clock.repeat_pict = if top_field_first { 5 } else { 3 };
                            } else if progressive_frame {
                                clock.repeat_pict = 2;
                            }
                        }
                        pic_ext += 1;
                    }
                    _ => {}
                },
                c if (SLICE_MIN_START_CODE..=SLICE_MAX_START_CODE).contains(&c) || (c & 0xFFFF_FF00) != 0x100 => break,
                _ => {}
            }
        }
        if !clock.mpeg2 || pic_ext > 1 {
            clock.repeat_pict = 1;
        }
        picture
    }
}

impl Units for MpegVideo {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    /// Key when the first picture header before the first slice is I;
    /// timed as FFmpeg's demuxer layer times the unit.
    fn unit(&mut self, unit: &Unit, _index: i64) -> Stamp {
        let picture = self.extract_headers(&unit.data);
        let clock = &mut self.clock;
        if let (true, Discovery::NoPicture, Some(code)) = (picture, clock.discovery, clock.frame_rate_code) {
            clock.discovery = Discovery::Pending(clock.decoder_timing(code));
        }
        let (pts, dts, duration) = clock.stamp(self.pict_type);
        Stamp { key: self.pict_type == 1, pts, dts, duration }
    }

    /// avformat_find_stream_info decodes the first picture after a
    /// sequence header once the read that ended it has been parsed, and
    /// FFmpeg's MPEG-1/2 decoder sets has_b_frames to !low_delay (an
    /// MPEG-1 stream is delayed, B-frames seen or not), the frame rate and
    /// the codec, all from that picture's sequence: they hold from the
    /// next read on. The units of that read after it keep the timing the
    /// parser gave them.
    fn read_done(&mut self) {
        let clock = &mut self.clock;
        if let Discovery::Pending(set) = clock.discovery {
            clock.has_b_frames = set.has_b_frames;
            clock.framerate = set.framerate;
            clock.mpeg2 = set.mpeg2;
            clock.discovery = Discovery::Done;
        }
    }

    /// After a seek FFmpeg's new parser knows no sequence yet (its
    /// frame_rate, progressive_sequence, repeat_pict; pict_type I), while
    /// the codec context keeps the frame rate, B-frame delay and codec and
    /// the stream its last_IP_duration. The dts runs on from `ts`, now
    /// absolute, or, back at the start of the data, from where
    /// ff_read_frame_flush leaves it (RELATIVE_TS_BASE).
    fn reset(&self, ts: Option<i64>) -> Option<Self> {
        let c = &self.clock;
        let clock = MpegClock {
            frame_rate: (0, 0),
            progressive_sequence: false,
            repeat_pict: 0,
            cur_dts: ts.unwrap_or(RELATIVE_TS_BASE),
            frame_rate_code: c.frame_rate_code,
            frame_rate_ext: c.frame_rate_ext,
            framerate: c.framerate,
            mpeg2: c.mpeg2,
            has_b_frames: c.has_b_frames,
            low_delay: c.low_delay,
            discovery: c.discovery,
            last_ip_duration: c.last_ip_duration,
        };
        Some(Self { pc: Combine::default(), frame_start_found: 0, pict_type: 1, clock })
    }
}

// ───────────────────────── H.264 ─────────────────────────

/// h264_parser.c: an access unit, cut where FFmpeg's parser cuts it,
/// with what parse_nal_units makes of it.
pub(crate) struct H264 {
    pc: Combine,
    frame_start_found: bool,
    history: [u8; 6],
    history_count: usize,
    last_mb: u32,
    units: H264Parse,
}

impl H264 {
    /// The parser for a stream in `time_base` (the codec context's
    /// pkt_timebase).
    pub fn new(time_base: (i64, i64)) -> Self {
        Self::over(H264Parse::new(time_base))
    }

    /// The parser a seek leaves (ff_read_frame_flush): a new one, on the
    /// codec context the old one set up.
    pub fn fresh(&self) -> Self {
        Self::over(self.units.fresh())
    }

    fn over(units: H264Parse) -> Self {
        Self { pc: Combine::default(), frame_start_found: false, history: [0; 6], history_count: 0, last_mb: 0, units }
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
        self.units.parse_nal_units(&unit);
        (next, Some(unit))
    }

    /// h264_parse's HRD timing, and what parse_nal_units set.
    fn cut(&mut self, pts: Option<i64>, dts: Option<i64>) -> (Option<i64>, Option<i64>, Option<VideoCut>) {
        let (pts, dts) = self.units.timestamps(pts, dts);
        (pts, dts, Some(self.units.video()))
    }
}

impl Units for H264 {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    fn unit(&mut self, unit: &Unit, _index: i64) -> Stamp {
        Stamp::untimed(unit.video.unwrap_or_default())
    }
}

// ───────────────────────── HEVC ─────────────────────────

/// hevc/parser.c: an access unit, cut where FFmpeg's parser cuts it,
/// with what parse_nal_units makes of it.
pub(crate) struct Hevc {
    pc: Combine,
    frame_start_found: bool,
    units: HevcParse,
}

impl Default for Hevc {
    fn default() -> Self {
        Self { pc: Combine::default(), frame_start_found: false, units: HevcParse::new((1, RAW_VIDEO_CLOCK)) }
    }
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
            Some(unit) => {
                self.units.parse_nal_units(&unit);
                (next, Some(unit))
            }
            None => (buf.len() as isize, None),
        }
    }

    /// What parse_nal_units set: the key flag, the frame rate, repeat_pict.
    fn cut(&mut self, pts: Option<i64>, dts: Option<i64>) -> (Option<i64>, Option<i64>, Option<VideoCut>) {
        (pts, dts, Some(self.units.video()))
    }
}

impl Units for Hevc {
    fn buffered_bytes(&self) -> usize {
        self.pc.buffered_bytes()
    }

    fn unit(&mut self, unit: &Unit, _index: i64) -> Stamp {
        Stamp::untimed(unit.video.unwrap_or_default())
    }
}

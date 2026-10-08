// Ported from FFmpeg (commit 2da55bf): libavformat/diracdec.c
// (dirac_probe), libavformat/rawdec.c (ff_raw_video_read_header),
// libavcodec/dirac_parser.c (find_frame_end, unpack_parse_unit,
// dirac_combine_frame, dirac_parse) and the frame size and rate of
// libavcodec/dirac.c (av_dirac_parse_sequence_header,
// parse_source_parameters, dirac_source_parameters_defaults,
// dirac_frame_rate).
// License: LGPL-2.1-or-later
//
// Raw Dirac / VC-2 video (FFmpeg's dirac): each unit is a picture parse
// unit with the parse units before it, cut where FFmpeg's parser cuts it;
// the end of the sequence at the end of the input is a unit of its own.
// FFmpeg's parser times a unit by its picture number: the pts is the
// number, the dts one more than the previous unit's (the first unit's, the
// number less one). Once a picture with references was seen, a unit whose
// pts equals its dts is a B picture, and the parser keeps that picture type
// for every unit after it, so only the units before it are key.
//
// FFmpeg's raw demuxer reads those numbers in its 1/1200000 clock, so the
// 30 pictures of a FATE sample take 25 µs. Here a number counts frames of
// the first sequence header's frame rate on that clock (25 fps without
// one), every unit lasting one frame; the end of the sequence is timed
// where the clock is after the last picture, as FFmpeg's demuxer layer
// times it.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, ProbeData, ProbeScore, ReadSeek, Result,
    StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

use crate::parser::{Split, Unit, VideoCut};
use crate::rawvideo::{RawVideoDemuxer, Stamp, Units, FRAME_RATES, RAW_VIDEO_CLOCK};

/// DIRAC_PARSE_INFO_PREFIX: "BBCD".
const PARSE_INFO_PREFIX: u32 = 0x4242_4344;
/// A parse info header: the prefix, the parse code and two offsets.
const PARSE_INFO_SIZE: usize = 13;
const SEQUENCE_HEADER: u8 = 0x00;
const END_OF_SEQUENCE: u8 = 0x10;
/// unpack_parse_unit's valid_pu_types.
const PARSE_CODES: [u8; 17] =
    [0x00, 0x10, 0x20, 0x30, 0x08, 0x48, 0xC8, 0xE8, 0x0A, 0x0C, 0x0D, 0x0E, 0x4C, 0x09, 0xCC, 0x88, 0xCB];

/// dirac_source_parameters_defaults: the frame width, height and frame
/// rate index of each base video format.
const BASE_FORMATS: [(u32, u32, usize); 21] = [
    (640, 480, 1),
    (176, 120, 9),
    (176, 144, 10),
    (352, 240, 9),
    (352, 288, 10),
    (704, 480, 9),
    (704, 576, 10),
    (720, 480, 4),
    (720, 576, 3),
    (1280, 720, 7),
    (1280, 720, 6),
    (1920, 1080, 4),
    (1920, 1080, 3),
    (1920, 1080, 7),
    (1920, 1080, 6),
    (2048, 1080, 2),
    (4096, 2160, 2),
    (3840, 2160, 7),
    (3840, 2160, 6),
    (7680, 4320, 7),
    (7680, 4320, 6),
];

/// dirac_frame_rate: frame rate indices 9 and 10.
const DIRAC_FRAME_RATES: [(i64, i64); 2] = [(15000, 1001), (25, 2)];

/// dirac_probe.
pub fn probe_dirac(probe: &ProbeData) -> ProbeScore {
    // FFmpeg reads the zero padding after the probe buffer.
    let byte = |i: usize| probe.buf.get(i).copied().unwrap_or(0);
    let rb32 = |i: usize| u32::from_be_bytes([byte(i), byte(i + 1), byte(i + 2), byte(i + 3)]);
    if rb32(0) != PARSE_INFO_PREFIX {
        return 0;
    }
    let size = rb32(5) as usize;
    if size < PARSE_INFO_SIZE {
        return 0;
    }
    if size as u64 + PARSE_INFO_SIZE as u64 > probe.buf.len() as u64 {
        return MAX_PROBE_SCORE / 4;
    }
    if rb32(size) != PARSE_INFO_PREFIX {
        return 0;
    }
    MAX_PROBE_SCORE
}

/// FFmpeg's GetBitContext over a sequence header: zeros past its end.
struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Bits<'_> {
    fn bit(&mut self) -> bool {
        let bit = self.data.get(self.pos / 8).is_some_and(|b| (b >> (7 - self.pos % 8)) & 1 == 1);
        self.pos += 1;
        bit
    }

    fn left(&self) -> isize {
        (self.data.len() * 8) as isize - self.pos as isize
    }

    /// get_interleaved_ue_golomb: data bits each after a 0, ended by a 1.
    fn uint(&mut self) -> u32 {
        let mut v: u32 = 1;
        while !self.bit() && self.left() > 0 {
            v = (v << 1) | u32::from(self.bit());
        }
        v.wrapping_sub(1)
    }
}

/// What the first sequence header says of the stream.
struct Sequence {
    width: u32,
    height: u32,
    /// Frames per second (num, den); (0, 0) for none.
    frame_rate: (i64, i64),
}

/// av_dirac_parse_sequence_header up to the frame rate, on the bytes after
/// the parse info header; `None` where FFmpeg refuses the header.
fn parse_sequence(data: &[u8]) -> Option<Sequence> {
    let mut gb = Bits { data, pos: 0 };
    for _ in 0..4 {
        gb.uint(); // version major and minor, profile, level
    }
    let (mut width, mut height, mut index) = *BASE_FORMATS.get(gb.uint() as usize)?;
    if gb.bit() {
        width = gb.uint();
        height = gb.uint();
    }
    if gb.bit() && gb.uint() > 2 {
        return None; // chroma format
    }
    if gb.bit() && gb.uint() > 1 {
        return None; // source sampling
    }
    let mut frame_rate = (0, 0);
    if gb.bit() {
        index = gb.uint() as usize;
        if index > 10 {
            return None;
        }
        if index == 0 {
            frame_rate = (i64::from(gb.uint()), i64::from(gb.uint()));
        }
    }
    if index > 0 {
        frame_rate = if index <= 8 { FRAME_RATES[index] } else { DIRAC_FRAME_RATES[index - 9] };
    }
    Some(Sequence { width, height, frame_rate })
}

/// compute_frame_duration for one frame of `rate` in 1/1200000, rounded
/// down; the raw demuxer's 25 fps for no rate or one FFmpeg does not time
/// by (a thousand frames per second or more).
fn frame_ticks((num, den): (i64, i64)) -> i64 {
    if num <= 0 || den <= 0 || den.saturating_mul(1000) <= num {
        RAW_VIDEO_CLOCK / 25
    } else {
        den * RAW_VIDEO_CLOCK / num
    }
}

/// unpack_parse_unit: a parse info header's code and offsets.
#[derive(Clone, Copy)]
struct ParseUnit {
    code: u8,
    next: i64,
    prev: i64,
}

/// dirac_parser.c (its DiracParseContext, with what the parser keeps in
/// its AVCodecParserContext and codec context), and the clock FFmpeg's
/// demuxer layer keeps.
pub(crate) struct Dirac {
    state: u32,
    is_synced: bool,
    header_bytes_needed: usize,
    overread_index: usize,
    /// pc->buffer up to pc->index.
    buffer: Vec<u8>,
    dirac_unit_size: usize,
    /// s->pts and s->dts the parser set for the unit it cut last.
    cut: (Option<i64>, Option<i64>),
    /// s->last_pts and s->last_dts: the pair for the unit before (0 and 0
    /// in a new parser).
    last: (Option<i64>, Option<i64>),
    /// s->pict_type is B: set by a unit whose pts equals its dts once
    /// has_b_frames, never cleared.
    b_picture: bool,
    /// avctx->has_b_frames: set by the first picture with references.
    has_b_frames: bool,
    /// One frame in 1/1200000.
    frame: i64,
    /// sti->cur_dts: the clock after the last unit.
    cur_dts: i64,
}

impl Dirac {
    fn new(frame: i64) -> Self {
        Self {
            state: 0,
            is_synced: false,
            header_bytes_needed: 0,
            overread_index: 0,
            buffer: Vec::new(),
            dirac_unit_size: 0,
            cut: (None, None),
            last: (Some(0), Some(0)),
            b_picture: false,
            has_b_frames: false,
            frame,
            cur_dts: 0,
        }
    }

    /// find_frame_end: where the parse info header after the next prefix
    /// ends in `buf`, or -1.
    fn find_frame_end(&mut self, buf: &[u8]) -> isize {
        let mut state = self.state;
        let mut i = 0;
        if !self.is_synced {
            while i < buf.len() {
                state = (state << 8) | u32::from(buf[i]);
                if state == PARSE_INFO_PREFIX {
                    state = u32::MAX;
                    self.is_synced = true;
                    self.header_bytes_needed = 9;
                    break;
                }
                i += 1;
            }
        }
        if self.is_synced {
            while i < buf.len() {
                if state == PARSE_INFO_PREFIX {
                    if buf.len() - i >= self.header_bytes_needed {
                        self.state = u32::MAX;
                        return (i + self.header_bytes_needed) as isize;
                    }
                    self.header_bytes_needed = 9 - (buf.len() - i);
                    break;
                }
                state = (state << 8) | u32::from(buf[i]);
                i += 1;
            }
        }
        self.state = state;
        -1
    }

    /// unpack_parse_unit at `offset` of the buffer: `None` past its end or
    /// for an unknown parse code or offsets below a header's size.
    fn parse_unit(&self, offset: i64) -> Option<ParseUnit> {
        if offset < 0 || (self.buffer.len() as i64 - PARSE_INFO_SIZE as i64) < offset {
            return None;
        }
        let b = &self.buffer[offset as usize..];
        // The offsets are C ints.
        let rb32 = |i: usize| i64::from(u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as i32);
        let code = b[4];
        let (mut next, prev) = (rb32(5), rb32(9));
        if !PARSE_CODES.contains(&code) {
            return None;
        }
        if code == END_OF_SEQUENCE && next == 0 {
            next = PARSE_INFO_SIZE as i64;
        }
        let short = |o: i64| o != 0 && o < PARSE_INFO_SIZE as i64;
        if short(next) || short(prev) {
            return None;
        }
        Some(ParseUnit { code, next, prev })
    }

    /// dirac_combine_frame and the rest of dirac_parse: how much of `buf`
    /// is consumed, and the unit cut, if any.
    fn combine(&mut self, next: isize, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        if self.overread_index > 0 {
            self.buffer.drain(..self.overread_index);
            self.overread_index = 0;
            if buf.is_empty() && self.buffer.get(4) == Some(&END_OF_SEQUENCE) {
                // At the end of the input: untimed, as s->pts and s->dts
                // were reset for it.
                self.cut = (None, None);
                self.last = self.cut;
                return (next, Some(self.buffer.clone()));
            }
        }
        if next < 0 {
            // A unit started, its end not found yet.
            self.buffer.extend_from_slice(buf);
            return (buf.len() as isize, None);
        }
        self.buffer.extend_from_slice(&buf[..next as usize]);
        let index = self.buffer.len() as i64;
        let header = PARSE_INFO_SIZE as i64;
        // The unit ends where the next header says it began, which a
        // prefix inside arithmetic-coded data does not.
        let units = self.parse_unit(index - header).and_then(|pu1| {
            let pu = self.parse_unit(index - header - pu1.prev)?;
            (pu.next == pu1.prev && index >= self.dirac_unit_size as i64 + header + pu1.prev).then_some((pu1, pu))
        });
        let Some((pu1, pu)) = units else {
            self.buffer.truncate(self.buffer.len().saturating_sub(9));
            self.header_bytes_needed = 9;
            return (next - 9, None);
        };
        let current = (index - header - pu1.prev) as usize;
        let start = current - self.dirac_unit_size;
        self.dirac_unit_size += pu.next as usize;
        if pu.code & 0x08 != 0x08 {
            // Data other than a picture waits for the picture after it.
            self.header_bytes_needed = 9;
            return (next, None);
        }
        let (mut pts, mut dts) = (None, None);
        if pu1.prev >= header {
            let b = &self.buffer[current..];
            let number = i64::from(u32::from_be_bytes([b[13], b[14], b[15], b[16]]));
            dts = match self.last {
                (Some(0), Some(0)) => Some(number - 1),
                (_, Some(last)) => Some(last + 1),
                (_, None) => None,
            };
            pts = Some(number);
            if !self.has_b_frames && b[4] & 0x03 != 0 {
                self.has_b_frames = true;
            }
        }
        if self.has_b_frames && pts == dts {
            self.b_picture = true;
        }
        self.cut = (pts, dts);
        self.last = self.cut;
        let unit = self.buffer[start..start + self.dirac_unit_size].to_vec();
        self.dirac_unit_size = 0;
        self.overread_index = self.buffer.len() - PARSE_INFO_SIZE;
        self.header_bytes_needed = 9;
        (next, Some(unit))
    }
}

impl Split for Dirac {
    /// dirac_parse.
    fn parse(&mut self, buf: &[u8]) -> (isize, Option<Vec<u8>>) {
        let next = self.find_frame_end(buf);
        if !self.is_synced && next == -1 {
            return (buf.len() as isize, None);
        }
        self.combine(next, buf)
    }

    /// The picture numbers the parser set, and the key flag parse_packet
    /// gives: key while the picture type is I.
    fn cut(&mut self, _pts: Option<i64>, _dts: Option<i64>) -> (Option<i64>, Option<i64>, Option<VideoCut>) {
        let video = VideoCut { key: !self.b_picture, b_picture: self.b_picture, duration: self.frame, rate_known: true };
        (self.cut.0, self.cut.1, Some(video))
    }
}

impl Units for Dirac {
    fn buffered_bytes(&self) -> usize {
        self.buffer.len()
    }

    /// A picture unit's numbers in frames; an untimed unit where the clock
    /// is (compute_pkt_fields).
    fn unit(&mut self, unit: &Unit, _index: i64) -> Stamp {
        let frames = |n: i64| n.saturating_mul(self.frame);
        let (pts, dts) = match (unit.pts, unit.dts) {
            (None, None) => (Some(self.cur_dts), Some(self.cur_dts)),
            (pts, dts) => (pts.map(frames), dts.map(frames)),
        };
        // A unit shown after it is decoded moves the clock a frame past its
        // dts, any other a frame past its pts.
        if let (Some(p), Some(d)) = (pts, dts) {
            self.cur_dts = p.min(d).saturating_add(self.frame);
        }
        Stamp::new(unit.video.is_some_and(|v| v.key), pts, dts, Some(self.frame))
    }

    /// After a seek FFmpeg's new parser starts empty, at picture type I and
    /// with 0 and 0 for the previous unit; has_b_frames, on the codec
    /// context, stays, and the clock runs from `ts`.
    fn reset(&self, ts: Option<i64>) -> Option<Self> {
        Some(Self { has_b_frames: self.has_b_frames, cur_dts: ts.unwrap_or(0), ..Self::new(self.frame) })
    }
}

/// ff_raw_video_read_header: one Dirac stream in 1/1200000; its frame size,
/// frame rate and first picture number from the first sequence header and
/// picture the first 64 KiB hold.
pub fn open_dirac(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut head = Vec::new();
    (&mut input).take(64 * 1024).read_to_end(&mut head)?;
    input.seek(SeekFrom::Start(0))?;
    let header = |code: fn(u8) -> bool| {
        head.windows(PARSE_INFO_SIZE + 4)
            .position(|w| w[..4] == PARSE_INFO_PREFIX.to_be_bytes() && code(w[4]))
            .map(|at| &head[at + PARSE_INFO_SIZE..])
    };
    let sequence = header(|code| code == SEQUENCE_HEADER).and_then(parse_sequence);
    let frame = frame_ticks(sequence.as_ref().map_or((0, 0), |s| s.frame_rate));
    let first = header(|code| code & 0x08 == 0x08).map_or(0, |b| i64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])));
    let mut params = CodecParameters::video(CodecId::new("dirac"));
    if let Some(s) = &sequence {
        (params.width, params.height) = (Some(s.width), Some(s.height));
    }
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, RAW_VIDEO_CLOCK),
        duration: None,
        start_time: Some(first.saturating_mul(frame)),
    };
    Ok(Box::new(RawVideoDemuxer::new("dirac", input, stream, Dirac::new(frame))))
}

/// FFmpeg's dirac demuxer, found by its probe: FFmpeg gives it no
/// extension.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("dirac", open_dirac);
    reg.register_probe("dirac", probe_dirac);
}

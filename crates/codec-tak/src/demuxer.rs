// The `tak` demuxer, with the TAK parser's framing.
//
// Ported from FFmpeg (commit 2da55bf) libavformat/takdec.c (the metadata
// blocks and the data's end) and libavcodec/tak_parser.c (how the raw
// data is cut into frames, applied here as FFmpeg applies it to the
// demuxer's packets with AVSTREAM_PARSE_FULL_RAW).
// Copyright (c) 2012 Paul B Mahol (takdec.c), (c) 2012 Michael
// Niedermayer (tak_parser.c); LGPL-2.1-or-later (see LICENSE).

//! After the `tBaK` magic come metadata blocks (stream info, the last
//! frame's position, encoder, MD5, padding) up to an end block; the frames
//! follow, to the last frame's end when the file gives it, else to the end
//! of the file. A frame starts where `FF A0` begins a valid frame header
//! whose CRC matches, searching on from the byte after the previous start;
//! a frame runs to the next start. Packets are the frames, stamped from 0
//! by their sample counts. A seek lands on the last frame at or before the
//! target that carries stream info (FFmpeg's generic index keeps those as
//! key frames).

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    SampleFormat, StreamInfo as CoreStreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::bits::Bits;
use crate::tak::{
    StreamInfo, TAK_FRAME_FLAG_HAS_INFO, TAK_MAX_FRAME_HEADER_BYTES, check_crc, decode_frame_header,
    parse_streaminfo_block,
};

const TAK_METADATA_END: u8 = 0;
const TAK_METADATA_STREAMINFO: u8 = 1;
const TAK_METADATA_ENCODER: u8 = 4;
const TAK_METADATA_MD5: u8 = 6;
const TAK_METADATA_LAST_FRAME: u8 = 7;
/// The parser needs this many bytes after a frame start, or 8 at the end.
const NEEDED: usize = TAK_MAX_FRAME_HEADER_BYTES;
const NEEDED_AT_END: usize = 8;
/// How much of the stream a header check reads at most: every header the
/// decoder accepts (at most 6 channels) fits in the parser's 37 bytes.
const HEADER_WINDOW: usize = 64;
const READ_CHUNK: u64 = 64 * 1024;

/// `tak_probe`: FFmpeg's extension-level score for the magic.
pub fn probe(p: &ProbeData) -> u8 {
    if p.buf.starts_with(b"tBaK") { PROBE_SCORE_EXTENSION } else { 0 }
}

fn read_exact_or(input: &mut Box<dyn ReadSeek>, n: usize) -> Result<Vec<u8>> {
    let mut v = Vec::new();
    input.take(n as u64).read_to_end(&mut v)?;
    if v.len() < n {
        return Err(Error::invalid("tak: truncated metadata block"));
    }
    Ok(v)
}

/// `tak_read_header`
pub fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut magic = [0u8; 4];
    if input.read_exact(&mut magic).is_err() || &magic != b"tBaK" {
        // FFmpeg reads such a stream without stream info, and its decoder
        // then refuses it.
        return Err(Error::invalid("tak: no tBaK header"));
    }
    let mut extradata: Option<Vec<u8>> = None;
    let mut last_frame_end: Option<u64> = None;
    let data_start;
    loop {
        let mut head = Vec::with_capacity(4);
        (&mut input).take(4).read_to_end(&mut head)?;
        // At the end of the file FFmpeg reads zeros: an end block.
        head.resize(4, 0);
        let kind = head[0] & 0x7F;
        let size = u32::from_le_bytes([head[1], head[2], head[3], 0]) as usize;
        match kind {
            TAK_METADATA_STREAMINFO | TAK_METADATA_LAST_FRAME | TAK_METADATA_ENCODER => {
                if kind == TAK_METADATA_STREAMINFO && extradata.is_some() {
                    return Err(Error::invalid("tak: second stream info block"));
                }
                if size <= 3 {
                    return Err(Error::invalid("tak: metadata block too short"));
                }
                let block = read_exact_or(&mut input, size - 3)?;
                // The block's CRC: FFmpeg only logs a mismatch.
                let mut crc = Vec::new();
                (&mut input).take(3).read_to_end(&mut crc)?;
                match kind {
                    TAK_METADATA_STREAMINFO => {
                        parse_streaminfo_block(&block).ok_or_else(|| Error::invalid("tak: invalid stream info"))?;
                        extradata = Some(block);
                    }
                    TAK_METADATA_LAST_FRAME => {
                        if size != 11 {
                            return Err(Error::invalid("tak: invalid last frame block"));
                        }
                        let le = |r: std::ops::Range<usize>| r.rev().fold(0u64, |v, i| v << 8 | u64::from(block[i]));
                        last_frame_end = Some(le(0..5) + le(5..8));
                    }
                    _ => {}
                }
            }
            TAK_METADATA_MD5 => {
                if size != 19 {
                    return Err(Error::invalid("tak: invalid MD5 block"));
                }
                read_exact_or(&mut input, 16)?;
                let mut crc = Vec::new();
                (&mut input).take(3).read_to_end(&mut crc)?;
            }
            TAK_METADATA_END => {
                data_start = input.stream_position()?;
                break;
            }
            _ => {
                input.seek(SeekFrom::Current(size as i64))?;
            }
        }
    }
    let Some(extradata) = extradata else { return Err(Error::invalid("tak: no stream info")) };
    let info = parse_streaminfo_block(&extradata).ok_or_else(|| Error::invalid("tak: invalid stream info"))?;
    let mut params = CodecParameters::audio(CodecId::new("tak"));
    params.channels = Some(if info.ch_mask != 0 { info.ch_mask.count_ones() } else { info.channels as u32 } as u16);
    params.sample_rate = Some(info.sample_rate as u32);
    params.sample_format = match info.bps {
        8 => Some(SampleFormat::U8P),
        16 => Some(SampleFormat::S16P),
        24 => Some(SampleFormat::S32P),
        _ => None,
    };
    params.extradata = extradata;
    let stream = CoreStreamInfo {
        index: 0,
        time_base: TimeBase::new(1, i64::from(info.sample_rate)),
        duration: (info.samples > 0).then_some(info.samples),
        start_time: Some(0),
        params,
    };
    input.seek(SeekFrom::Start(data_start))?;
    Ok(Box::new(TakDemuxer {
        input,
        stream,
        data_start,
        data_end: last_frame_end.map(|e| e + data_start),
        splitter: Splitter::new(data_start),
        pts: 0,
        index: Vec::new(),
    }))
}

/// Whether a valid frame starts at `b[0]` (`tak_parse`'s check): the sync,
/// a header that decodes and its CRC. `ti` takes what the header carries,
/// as the parser's stream info does even when the check then fails.
fn frame_starts(b: &[u8], ti: &mut StreamInfo) -> bool {
    if b.len() < 2 || b[0] != 0xFF || b[1] != 0xA0 {
        return false;
    }
    let window = &b[..b.len().min(HEADER_WINDOW)];
    let mut gb = Bits::new(window);
    if !decode_frame_header(&mut gb, ti) {
        return false;
    }
    let header = (gb.tell() / 8) as usize;
    header <= window.len() && check_crc(&window[..header])
}

/// The parser's state: the bytes from the frame being collected on.
struct Splitter {
    buf: Vec<u8>,
    /// The file offset of `buf[0]`.
    pos: u64,
    scan: usize,
    start_found: bool,
    eof: bool,
    /// The parser's stream info (`t->ti`).
    ti: StreamInfo,
    /// The duration and key flag of the frame being collected.
    duration: i64,
    key: bool,
}

impl Splitter {
    fn new(pos: u64) -> Self {
        Self {
            buf: Vec::new(),
            pos,
            scan: 0,
            start_found: false,
            eof: false,
            ti: StreamInfo::default(),
            duration: 0,
            key: false,
        }
    }
}

/// One frame: its bytes, file offset, duration and whether it is a key
/// frame.
struct Frame {
    data: Vec<u8>,
    pos: u64,
    duration: i64,
    key: bool,
}

struct TakDemuxer {
    input: Box<dyn ReadSeek>,
    stream: CoreStreamInfo,
    data_start: u64,
    data_end: Option<u64>,
    splitter: Splitter,
    /// The next frame's pts.
    pts: i64,
    /// Key frames seen: (pts, file offset), in order.
    index: Vec<(i64, u64)>,
}

impl TakDemuxer {
    /// More of the data into the splitter; false at its end.
    fn fill(&mut self) -> Result<bool> {
        let at = self.splitter.pos + self.splitter.buf.len() as u64;
        let limit = match self.data_end {
            Some(end) => end.saturating_sub(at).min(READ_CHUNK),
            None => READ_CHUNK,
        };
        let before = self.splitter.buf.len();
        (&mut self.input).take(limit).read_to_end(&mut self.splitter.buf)?;
        Ok(self.splitter.buf.len() > before)
    }

    /// `tak_parse`: the next frame, or `None` at the end of the data.
    fn next_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            let s = &mut self.splitter;
            let needed = if s.eof { NEEDED_AT_END } else { NEEDED };
            while s.scan + needed <= s.buf.len() {
                let p = s.scan;
                if !s.start_found {
                    if frame_starts(&s.buf[p..], &mut s.ti) {
                        s.start_found = true;
                        s.duration = i64::from(if s.ti.last_frame_samples != 0 {
                            s.ti.last_frame_samples
                        } else {
                            s.ti.frame_samples
                        });
                        s.key = s.ti.flags & TAK_FRAME_FLAG_HAS_INFO != 0;
                    }
                } else {
                    let mut ti = StreamInfo::default();
                    if frame_starts(&s.buf[p..], &mut ti) {
                        let data: Vec<u8> = s.buf.drain(..p).collect();
                        let frame = Frame { data, pos: s.pos, duration: s.duration, key: s.key };
                        s.pos += p as u64;
                        s.scan = 0;
                        s.start_found = false;
                        return Ok(Some(frame));
                    }
                }
                s.scan += 1;
            }
            if s.eof {
                if s.buf.is_empty() {
                    return Ok(None);
                }
                let data = std::mem::take(&mut s.buf);
                let frame = Frame { data, pos: s.pos, duration: s.duration, key: s.key };
                s.pos += frame.data.len() as u64;
                s.scan = 0;
                s.start_found = false;
                return Ok(Some(frame));
            }
            if !self.fill()? {
                self.splitter.eof = true;
            }
        }
    }

    /// The next frame, its pts noted and key frames indexed.
    fn next(&mut self) -> Result<Option<(Frame, i64)>> {
        let Some(frame) = self.next_frame()? else { return Ok(None) };
        let pts = self.pts;
        self.pts += frame.duration;
        if frame.key && self.index.last().is_none_or(|&(p, _)| p < pts) {
            self.index.push((pts, frame.pos));
        }
        Ok(Some((frame, pts)))
    }

    fn restart_at(&mut self, pts: i64, pos: u64) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos))?;
        self.splitter = Splitter::new(pos);
        self.pts = pts;
        Ok(())
    }
}

impl Demuxer for TakDemuxer {
    fn format_name(&self) -> &str {
        "tak"
    }

    fn streams(&self) -> &[CoreStreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let Some((frame, pts)) = self.next()? else { return Err(Error::Eof) };
        let mut packet = Packet::new(0, self.stream.time_base, frame.data);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(frame.duration);
        packet.flags.keyframe = frame.key;
        Ok(packet)
    }

    /// The last key frame at or before `pts`, reading on to find it when it
    /// lies past what has been read.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        if self.index.last().is_none_or(|&(p, _)| p <= pts) {
            while let Some((_, at)) = self.next()? {
                if at > pts {
                    break;
                }
            }
        }
        let i = self.index.partition_point(|&(p, _)| p <= pts);
        let (target, pos) = match i.checked_sub(1).map(|i| self.index[i]) {
            Some(entry) => entry,
            None => (0, self.data_start),
        };
        self.restart_at(target, pos)?;
        Ok(target)
    }
}

/// Registers the `tak` demuxer with its `.tak` extension and probe.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("tak", open);
    reg.register_extension("tak", "tak");
    reg.register_probe("tak", probe);
}

// Ported from FFmpeg libavformat/pva.c (commit 2da55bf), with the
// timestamp seek of libavformat/seek.c (ff_seek_frame_binary); the audio
// is cut into frames by the mpegaudio parser (see parser.rs).
// License: LGPL-2.1-or-later

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use std::collections::VecDeque;

use demux_seek_core::{gen_search, Allowance, Bounds, Index};

use crate::parser::{AudioClock, MpegAudio, Parser};

const PVA_MAGIC: u16 = 0x4156; // "AV"
const PVA_MAX_PAYLOAD_LENGTH: usize = 0x17F8;
const PVA_VIDEO_PAYLOAD: u8 = 0x01;
const PVA_AUDIO_PAYLOAD: u8 = 0x02;

fn pva_check(p: &[u8]) -> Option<usize> {
    if p.len() < 8 {
        return None;
    }
    let magic = u16::from_be_bytes([p[0], p[1]]);
    let streamid = p[2];
    let reserved = p[4];
    let flags = p[5];
    let length = u16::from_be_bytes([p[6], p[7]]) as usize;

    if magic != PVA_MAGIC || streamid == 0 || streamid > 2 || reserved != 0x55 || (flags & 0xE0) != 0 || length > PVA_MAX_PAYLOAD_LENGTH {
        return None;
    }
    Some(length + 8)
}

/// ff_parse_pes_pts (mpeg.h)
fn parse_pes_pts(buf: &[u8]) -> i64 {
    (i64::from(buf[0] & 0x0E) << 29)
        | ((i64::from(u16::from_be_bytes([buf[1], buf[2]])) >> 1) << 15)
        | i64::from(u16::from_be_bytes([buf[3], buf[4]]) >> 1)
}

pub fn probe_pva(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if let Some(len) = pva_check(p) {
        if p.len() >= len + 8 && pva_check(&p[len..]).is_some() {
            PROBE_SCORE_EXTENSION
        } else {
            PROBE_SCORE_EXTENSION / 2
        }
    } else {
        0
    }
}

pub struct PvaDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    /// PES bytes still to come in later PVA audio packets
    /// (continue_pes; FFmpeg resets it to 0 when it goes negative).
    continue_pes: i64,
    /// Per stream: where each PVA packet with a pts starts, by pts
    /// (read_part_of_packet indexes them; pva_read_header adds 0 at 0).
    index: [Index; 2],
    /// What the seek under way may still read.
    allowance: Allowance,
    /// The MP2 frames FFmpeg's mpegaudio parser cuts from the audio
    /// payloads, timed by its demuxer layer (pts_wrap_bits 33).
    audio: Parser<MpegAudio>,
    clock: AudioClock,
    /// Packets parsed and not yet returned.
    queue: VecDeque<Packet>,
    /// Reading has ended (the parser has handed over all it held).
    ended: bool,
    /// The error that ended reading, returned once the queue is empty.
    error: Option<Error>,
}

pub fn open_pva(
    input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let video_params = CodecParameters::video(CodecId::new("mpeg2video"));
    let mut audio_params = CodecParameters::audio(CodecId::new("mp2"));
    audio_params.sample_rate = Some(48000);
    audio_params.channels = Some(2);

    let streams = vec![
        StreamInfo {
            index: 0,
            params: video_params,
            time_base: TimeBase::new(1, 90000),
            duration: None,
            start_time: Some(0),
        },
        StreamInfo {
            index: 1,
            params: audio_params,
            time_base: TimeBase::new(1, 90000),
            duration: None,
            start_time: Some(0),
        },
    ];

    let mut index = [Index::default(), Index::default()];
    for stream in &mut index {
        stream.add(0, 0, 0, 0, true);
    }
    let allowance = Allowance::default();
    Ok(Box::new(PvaDemuxer {
        input: Box::new(allowance.meter(input)),
        streams,
        continue_pes: 0,
        index,
        allowance,
        audio: Parser::new(MpegAudio::new("mp2")),
        clock: AudioClock::new(1, 90_000, 33),
        queue: VecDeque::new(),
        ended: false,
        error: None,
    }))
}

/// `N` bytes as avio reads them: zeros past the end, which it reports.
fn read_padded<const N: usize>(input: &mut dyn ReadSeek) -> Result<([u8; N], bool)> {
    let mut buf = [0u8; N];
    let mut got = 0;
    while got < N {
        match input.read(&mut buf[got..]) {
            Ok(0) => return Ok((buf, true)),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok((buf, false))
}

impl PvaDemuxer {
    /// read_part_of_packet as pva_read_timestamp calls it (read_packet 0,
    /// a new PES expected): the packet's pts, payload length and stream
    /// id, `None` where FFmpeg returns an error.
    fn read_part(&mut self) -> Result<Option<(Option<i64>, i64, u8)>> {
        self.allowance.spend(1, 0)?;
        let startpos = self.input.stream_position()? as i64;
        let (hdr, _) = read_padded::<8>(&mut *self.input)?;
        let syncword = u16::from_be_bytes([hdr[0], hdr[1]]);
        let streamid = hdr[2];
        let flags = hdr[5];
        let mut length = i64::from(u16::from_be_bytes([hdr[6], hdr[7]]));
        if syncword != PVA_MAGIC
            || (streamid != PVA_VIDEO_PAYLOAD && streamid != PVA_AUDIO_PAYLOAD)
            || length > PVA_MAX_PAYLOAD_LENGTH as i64
        {
            return Ok(None);
        }
        let mut pva_pts = None;
        if streamid == PVA_VIDEO_PAYLOAD && flags & 0x10 != 0 {
            let (pts, _) = read_padded::<4>(&mut *self.input)?;
            pva_pts = Some(i64::from(u32::from_be_bytes(pts)));
            length -= 4;
        } else if streamid == PVA_AUDIO_PAYLOAD {
            let (pes, eof) = read_padded::<9>(&mut *self.input)?;
            if eof {
                return Ok(None);
            }
            let pes_signal = (u32::from(pes[0]) << 16) | (u32::from(pes[1]) << 8) | u32::from(pes[2]);
            let pes_flags = u16::from_be_bytes([pes[6], pes[7]]);
            let header_len = usize::from(pes[8]);
            if pes_signal != 1 || header_len == 0 {
                return Ok(None);
            }
            let mut header = vec![0u8; header_len];
            if self.input.read_exact(&mut header).is_err() {
                return Ok(None);
            }
            length -= 9 + header_len as i64;
            if pes_flags & 0x80 != 0 && header[0] & 0xF0 == 0x20 {
                if header_len < 5 {
                    return Ok(None);
                }
                pva_pts = Some(parse_pes_pts(&header));
            }
        }
        if let Some(pts) = pva_pts {
            self.index[usize::from(streamid - 1)].add(startpos, pts, 0, 0, true);
        }
        Ok(Some((pva_pts, length, streamid)))
    }

    /// pva_read_timestamp: from `*pos` on, at most 8 maximal payloads
    /// ahead, the pts of the first packet of `stream` that has one,
    /// stepping a byte on where no packet parses. Like FFmpeg it returns
    /// the last pts it read when it runs out of range, whatever stream
    /// that pts was of.
    fn read_timestamp(&mut self, pos: &mut i64, pos_limit: i64, stream: usize) -> Result<Option<i64>> {
        let limit = pos.saturating_add(PVA_MAX_PAYLOAD_LENGTH as i64 * 8).min(pos.saturating_add(pos_limit));
        let mut res = None;
        while *pos < limit {
            res = None;
            self.input.seek(SeekFrom::Start(*pos as u64))?;
            let Some((pts, length, streamid)) = self.read_part()? else {
                *pos += 1;
                continue;
            };
            res = pts;
            if usize::from(streamid - 1) != stream || pts.is_none() {
                // The payload is passed by an absolute seek, which the
                // metered input does not charge.
                self.allowance.spend(0, length.max(0) as u64)?;
                *pos = self.input.stream_position()? as i64 + length;
                continue;
            }
            break;
        }
        self.continue_pes = 0;
        Ok(res)
    }

    /// ff_seek_frame_binary over pva_read_timestamp.
    fn search(&mut self, stream: usize, timestamp: i64, bounds: Bounds) -> Result<Option<(i64, i64)>> {
        let file_size = self.input.seek(SeekFrom::End(0))? as i64;
        gen_search(timestamp, bounds, 0, file_size, &mut |pos, limit| self.read_timestamp(pos, limit, stream))
    }

    /// pva_read_packet: the next PVA packet's payload as the container
    /// carries it, and where its header starts. read_part_of_packet with
    /// its recovery: an audio packet that should start a PES and does not
    /// is skipped (`trying to recover`), the next one read.
    fn read_pva(&mut self) -> Result<(Packet, i64)> {
        loop {
            let startpos = self.input.stream_position()? as i64;
            let mut hdr = [0u8; 8];
            match self.input.read(&mut hdr) {
                Ok(0) => return Err(Error::Eof),
                Ok(n) if n < 8 => return Err(Error::Eof),
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }
            let magic = u16::from_be_bytes([hdr[0], hdr[1]]);
            let streamid = hdr[2];
            let flags = hdr[5];
            let mut length = i64::from(u16::from_be_bytes([hdr[6], hdr[7]]));
            if magic != PVA_MAGIC || (streamid != PVA_VIDEO_PAYLOAD && streamid != PVA_AUDIO_PAYLOAD) {
                return Err(Error::invalid("pva: invalid syncword or stream id"));
            }
            if length > PVA_MAX_PAYLOAD_LENGTH as i64 {
                return Err(Error::invalid("pva: payload length exceeds maximum"));
            }
            let mut pva_pts: Option<i64> = None;
            if streamid == PVA_VIDEO_PAYLOAD && flags & 0x10 != 0 {
                let (pts, _) = read_padded::<4>(&mut *self.input)?;
                pva_pts = Some(i64::from(u32::from_be_bytes(pts)));
                length -= 4;
            } else if streamid == PVA_AUDIO_PAYLOAD {
                // A PES starts at the start of a PVA audio packet or not
                // at all; the others continue the previous PES.
                if self.continue_pes == 0 {
                    let (pes, eof) = read_padded::<9>(&mut *self.input)?;
                    if eof {
                        return Err(Error::Eof);
                    }
                    let pes_signal = (u32::from(pes[0]) << 16) | (u32::from(pes[1]) << 8) | u32::from(pes[2]);
                    let pes_packet_length = i64::from(u16::from_be_bytes([pes[4], pes[5]]));
                    let pes_flags = u16::from_be_bytes([pes[6], pes[7]]);
                    let header_len = usize::from(pes[8]);
                    if pes_signal != 1 || header_len == 0 {
                        // "expected non empty signaled PES packet, trying
                        // to recover": past the rest of the packet.
                        self.input.seek(SeekFrom::Current(length - 9))?;
                        continue;
                    }
                    let mut header = vec![0u8; header_len];
                    self.input.read_exact(&mut header)?;
                    length -= 9 + header_len as i64;
                    self.continue_pes = pes_packet_length - 3 - header_len as i64;
                    if pes_flags & 0x80 != 0 && header[0] & 0xF0 == 0x20 {
                        if header_len < 5 {
                            self.input.seek(SeekFrom::Current(length))?;
                            return Err(Error::invalid("pva: PES header too short"));
                        }
                        pva_pts = Some(parse_pes_pts(&header));
                    }
                }
                self.continue_pes -= length;
                if self.continue_pes < 0 {
                    // "audio data corruption"
                    self.continue_pes = 0;
                }
            }
            if length < 0 {
                return Err(Error::invalid("pva: payload shorter than its headers"));
            }
            let stream_index = u32::from(streamid - 1);
            if let Some(pts) = pva_pts {
                self.index[stream_index as usize].add(startpos, pts, 0, 0, true);
            }
            // av_get_packet: a cut file ends mid-packet with the bytes it
            // has; nothing at all ends reading.
            let mut payload = Vec::with_capacity(length as usize);
            (&mut *self.input).take(length as u64).read_to_end(&mut payload)?;
            if payload.is_empty() {
                return Err(Error::Eof);
            }
            let mut pkt = Packet {
                stream_index,
                time_base: TimeBase::new(1, 90000),
                pts: pva_pts,
                // pva_read_packet supplies PTS only, not decoder-order time.
                dts: None,
                duration: None,
                flags: Default::default(),
                data: payload,
            };
            pkt.flags.keyframe = true;
            return Ok((pkt, startpos));
        }
    }
}

impl Demuxer for PvaDemuxer {
    fn format_name(&self) -> &str {
        "pva"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// av_read_frame: video payloads as the container carries them, audio
    /// as the MP2 frames FFmpeg's mpegaudio parser cuts from the payloads
    /// (AVSTREAM_PARSE_FULL), timed by its demuxer layer. When reading
    /// ends, at the end of the input or on an error, the parser hands
    /// over what it holds before the end or the error is returned
    /// (read_frame_internal flushes every parser).
    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(packet) = self.queue.pop_front() {
                return Ok(packet);
            }
            if self.ended {
                return Err(self.error.take().unwrap_or(Error::Eof));
            }
            let mut units = Vec::new();
            match self.read_pva() {
                Ok((packet, pos)) if packet.stream_index == 1 => {
                    self.audio.push(&packet.data, packet.pts, None, pos, &mut units);
                }
                Ok((packet, _)) => return Ok(packet),
                Err(e) => {
                    self.audio.flush(&mut units);
                    self.ended = true;
                    if !matches!(e, Error::Eof) {
                        self.error = Some(e);
                    }
                }
            }
            for unit in units {
                let packet = self.clock.stamp(unit, 1, TimeBase::new(1, 90000), &mut self.queue);
                self.queue.push_back(packet);
            }
        }
    }

    /// pva.c has no read_seek: FFmpeg bisects with pva_read_timestamp
    /// (seek.c ff_seek_frame_binary, ff_gen_search with
    /// AVSEEK_FLAG_BACKWARD) within the bounds of the stream's index. Video
    /// pts come in display order, so the landing follows every step of
    /// the search. The search reads within the seek's allowance; a failed
    /// seek, its reposition to the landing included, leaves reading where
    /// it was, the audio PES in progress (continue_pes) included.
    fn seek_to(&mut self, stream_index: u32, timestamp: i64) -> Result<i64> {
        let stream = stream_index as usize;
        let Some(index) = self.index.get(stream) else {
            return Err(Error::invalid("pva: no such stream to seek"));
        };
        let bounds = index.bounds(timestamp);
        let (resume, continue_pes) = (self.input.stream_position()?, self.continue_pes);
        self.allowance.start();
        let found = self.search(stream, timestamp, bounds);
        let landed = match self.allowance.finish(found) {
            Ok(Some((pos, ts))) => self.input.seek(SeekFrom::Start(pos as u64)).map(|_| Some(ts)).map_err(Error::from),
            other => other.map(|_| None),
        };
        match landed {
            Ok(Some(ts)) => {
                self.continue_pes = 0;
                // ff_read_frame_flush, then the clock from the landing.
                self.queue.clear();
                (self.ended, self.error) = (false, None);
                self.audio = Parser::new(self.audio.split.reset());
                self.clock.seeked(ts);
                Ok(ts)
            }
            failed => {
                self.input.seek(SeekFrom::Start(resume))?;
                self.continue_pes = continue_pes;
                Err(failed.err().unwrap_or_else(|| Error::invalid("pva: no timestamp to seek by")))
            }
        }
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("pva", open_pva);
    reg.register_probe("pva", probe_pva);
    reg.register_extension("pva", "pva");
}

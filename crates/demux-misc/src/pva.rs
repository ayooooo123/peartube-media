// Ported from FFmpeg libavformat/pva.c (commit 2da55bf), with the
// timestamp seek of libavformat/seek.c (ff_seek_frame_binary).
// License: LGPL-2.1-or-later

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
    PROBE_SCORE_EXTENSION,
};

use crate::seek::{gen_search, Index};

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
    continue_pes: usize,
    /// Per stream: where each PVA packet with a pts starts, by pts
    /// (read_part_of_packet indexes them; pva_read_header adds 0 at 0).
    index: [Index; 2],
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
    Ok(Box::new(PvaDemuxer {
        input,
        streams,
        continue_pes: 0,
        index,
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
                *pos = self.input.stream_position()? as i64 + length;
                continue;
            }
            break;
        }
        self.continue_pes = 0;
        Ok(res)
    }
}

impl Demuxer for PvaDemuxer {
    fn format_name(&self) -> &str {
        "pva"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
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
        let mut length = u16::from_be_bytes([hdr[6], hdr[7]]) as usize;

        if magic != PVA_MAGIC || (streamid != PVA_VIDEO_PAYLOAD && streamid != PVA_AUDIO_PAYLOAD) {
            return Err(Error::invalid("pva: invalid syncword or stream id"));
        }
        if length > PVA_MAX_PAYLOAD_LENGTH {
            return Err(Error::invalid("pva: payload length exceeds maximum"));
        }

        let pts_flag = (flags & 0x10) != 0;
        let mut pva_pts: Option<i64> = None;

        let stream_index = (streamid - 1) as u32;

        if streamid == PVA_VIDEO_PAYLOAD && pts_flag {
            let mut pts_buf = [0u8; 4];
            self.input.read_exact(&mut pts_buf)?;
            pva_pts = Some(i64::from(u32::from_be_bytes(pts_buf)));
            length = length.saturating_sub(4);
        } else if streamid == PVA_AUDIO_PAYLOAD {
            if self.continue_pes == 0 && length >= 9 {
                let mut pes_hdr = [0u8; 9];
                self.input.read_exact(&mut pes_hdr)?;
                let pes_signal = ((pes_hdr[0] as u32) << 16) | ((pes_hdr[1] as u32) << 8) | (pes_hdr[2] as u32);
                let pes_packet_len = u16::from_be_bytes([pes_hdr[4], pes_hdr[5]]) as usize;
                let pes_flags = u16::from_be_bytes([pes_hdr[6], pes_hdr[7]]);
                let pes_header_data_len = pes_hdr[8] as usize;

                length -= 9;
                if pes_signal == 1 && pes_header_data_len > 0 {
                    let mut pes_data = vec![0u8; pes_header_data_len];
                    self.input.read_exact(&mut pes_data)?;
                    length = length.saturating_sub(pes_header_data_len);

                    if (pes_flags & 0x80) != 0
                        && pes_data.len() >= 5
                        && (pes_data[0] & 0xF0) == 0x20
                    {
                        pva_pts = Some(parse_pes_pts(&pes_data));
                    }
                }
                self.continue_pes = pes_packet_len.saturating_sub(3 + pes_header_data_len);
            }
            self.continue_pes = self.continue_pes.saturating_sub(length);
        }
        if let Some(pts) = pva_pts {
            self.index[stream_index as usize].add(startpos, pts, 0, 0, true);
        }

        let mut payload = vec![0u8; length];
        // A cut file ends mid-packet; FFmpeg's av_get_packet returns
        // the partial read and then errors next round.
        let mut got = 0usize;
        while got < length {
            let n = self.input.read(&mut payload[got..])?;
            if n == 0 {
                break;
            }
            got += n;
        }
        payload.truncate(got);
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
        Ok(pkt)
    }

    /// pva.c has no read_seek: FFmpeg bisects with pva_read_timestamp
    /// (seek.c ff_seek_frame_binary, ff_gen_search with
    /// AVSEEK_FLAG_BACKWARD) within the bounds of the stream's index. Video
    /// pts come in display order, so the landing follows every step of
    /// the search. A failed search leaves reading where it was.
    fn seek_to(&mut self, stream_index: u32, timestamp: i64) -> Result<i64> {
        let stream = stream_index as usize;
        let Some(index) = self.index.get(stream) else {
            return Err(Error::invalid("pva: no such stream to seek"));
        };
        let bounds = index.bounds(timestamp);
        let resume = self.input.stream_position()?;
        let file_size = self.input.seek(SeekFrom::End(0))? as i64;
        let found = gen_search(timestamp, bounds, 0, file_size, &mut |pos, limit| self.read_timestamp(pos, limit, stream));
        match found {
            Ok(Some((pos, ts))) => {
                self.input.seek(SeekFrom::Start(pos as u64))?;
                self.continue_pes = 0;
                Ok(ts)
            }
            Ok(None) => {
                self.input.seek(SeekFrom::Start(resume))?;
                Err(Error::invalid("pva: no timestamp to seek by"))
            }
            Err(e) => {
                self.input.seek(SeekFrom::Start(resume))?;
                Err(e)
            }
        }
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("pva", open_pva);
    reg.register_probe("pva", probe_pva);
    reg.register_extension("pva", "pva");
}

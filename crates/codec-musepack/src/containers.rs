// Ported from FFmpeg libavformat/mpc.c, libavformat/mpc8.c and libavformat/apetag.c
// (commit 2da55bf), LGPL-2.1-or-later.
// Copyright (c) 2006, 2007 Konstantin Shishkov; Copyright (c) 2007 Benjamin Zores.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    StreamInfo, TimeBase, MAX_PROBE_SCORE, PROBE_SCORE_EXTENSION,
};

use crate::bits::BitReader;

fn gcd(mut a: i64, mut b: i64) -> i64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.abs()
}

fn reduced_time_base(num: i64, den: i64) -> TimeBase {
    let g = gcd(num, den).max(1);
    TimeBase::new(num / g, den / g)
}

fn parse_ape_tag(input: &mut Box<dyn ReadSeek>) -> Result<Option<u64>> {
    let file_size = input.seek(SeekFrom::End(0))?;
    if file_size < 32 {
        return Ok(None);
    }
    input.seek(SeekFrom::Start(file_size - 32))?;
    let mut footer = [0u8; 32];
    input.read_exact(&mut footer)?;
    if &footer[0..8] != b"APETAGEX" {
        return Ok(None);
    }
    let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    if version > 2000 {
        return Ok(None);
    }
    let mut tag_bytes = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as u64;
    if tag_bytes > file_size {
        return Ok(None);
    }
    let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());
    if (flags & (1 << 29)) != 0 {
        return Ok(None);
    }
    if (flags & (1 << 31)) != 0 {
        tag_bytes = tag_bytes.saturating_add(32);
    }
    if tag_bytes > file_size {
        return Ok(None);
    }
    Ok(Some(file_size - tag_bytes))
}

fn read_varlen(input: &mut Box<dyn ReadSeek>) -> Result<u64> {
    let mut val = 0u64;
    loop {
        let mut b = [0u8; 1];
        input.read_exact(&mut b)?;
        val = (val << 7) | ((b[0] & 0x7f) as u64);
        if (b[0] & 0x80) == 0 {
            break;
        }
    }
    Ok(val)
}

// ──────────────────────────────── MPC (SV7) ────────────────────────────────

const MPC_RATES: [u32; 4] = [44100, 48000, 37800, 32000];
const DELAY_FRAMES: i64 = 32;

#[derive(Clone, Copy, Debug, Default)]
struct MpcFrame {
    pos: u64,
    _size: usize,
    skip: i32,
}

pub struct MpcDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    fcount: u32,
    curframe: u32,
    lastframe: i32,
    curbits: i32,
    frames: Vec<MpcFrame>,
    frames_noted: usize,
}

pub fn mpc_probe(probe: &ProbeData) -> u8 {
    let d = probe.buf;
    if d.len() >= 4 && d[0] == b'M' && d[1] == b'P' && d[2] == b'+' && (d[3] == 0x17 || d[3] == 0x07) {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

pub fn open_mpc(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut magic = [0u8; 4];
    input.read_exact(&mut magic)?;
    if magic[0] != b'M' || magic[1] != b'P' || magic[2] != b'+' {
        return Err(Error::invalid("mpc: not a Musepack file"));
    }
    let ver = magic[3];
    if ver != 0x07 && ver != 0x17 {
        return Err(Error::invalid(format!("mpc: unsupported version {ver:#x}")));
    }

    let mut fcount_bytes = [0u8; 4];
    input.read_exact(&mut fcount_bytes)?;
    let fcount = u32::from_le_bytes(fcount_bytes);

    let mut extradata = vec![0u8; 16];
    input.read_exact(&mut extradata)?;

    let rate_idx = (extradata[2] & 3) as usize;
    let sample_rate = MPC_RATES[rate_idx];

    let mut params = CodecParameters::audio(CodecId::new("musepack7"));
    params.channels = Some(2);
    params.sample_rate = Some(sample_rate);
    params.extradata = extradata;

    let time_base = reduced_time_base(1152, sample_rate as i64);

    let stream = StreamInfo {
        index: 0,
        params,
        time_base,
        duration: Some(fcount as i64),
        start_time: Some(0),
    };

    let frames = if fcount > 0 && fcount < 10_000_000 {
        vec![MpcFrame::default(); fcount as usize]
    } else {
        Vec::new()
    };

    Ok(Box::new(MpcDemuxer {
        input,
        stream,
        fcount,
        curframe: 0,
        lastframe: -1,
        curbits: 8,
        frames,
        frames_noted: 0,
    }))
}

impl Demuxer for MpcDemuxer {
    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }
    fn format_name(&self) -> &str {
        "mpc"
    }


    fn next_packet(&mut self) -> Result<Packet> {
        if self.curframe >= self.fcount && self.fcount != 0 {
            return Err(Error::Eof);
        }

        if self.curframe != (self.lastframe + 1) as u32 {
            let frame = self
                .frames
                .get(self.curframe as usize)
                .copied()
                .ok_or_else(|| Error::invalid("mpc: frame not available"))?;
            self.input.seek(SeekFrom::Start(frame.pos))?;
            self.curbits = frame.skip;
        }

        self.lastframe = self.curframe as i32;
        let cur = self.curframe;
        self.curframe += 1;
        let mut curbits = self.curbits;
        let pos = self.input.stream_position()?;

        let mut tmp_bytes = [0u8; 4];
        if self.input.read_exact(&mut tmp_bytes).is_err() {
            return Err(Error::Eof);
        }
        let tmp = u32::from_le_bytes(tmp_bytes);

        let size2 = if curbits <= 12 {
            ((tmp >> (12 - curbits)) & 0xFFFFF) as usize
        } else {
            let mut tmp2_bytes = [0u8; 4];
            self.input.read_exact(&mut tmp2_bytes)?;
            let tmp2 = u32::from_le_bytes(tmp2_bytes);
            let combined = ((tmp << (curbits - 12)) | (tmp2 >> (44 - curbits))) & 0xFFFFF;
            combined as usize
        };

        curbits += 20;
        self.input.seek(SeekFrom::Start(pos))?;

        let size = ((size2 + curbits as usize + 31) & !31) >> 3;
        if size > 1024 * 1024 {
            return Err(Error::invalid("mpc: packet too large"));
        }

        if cur as usize == self.frames_noted && (cur as usize) < self.frames.len() {
            self.frames[cur as usize] = MpcFrame {
                pos,
                _size: size,
                skip: curbits - 20,
            };
            self.frames_noted += 1;
        }

        self.curbits = (curbits + size2 as i32) & 0x1F;

        let mut data = vec![0u8; size + 4];
        data[0] = curbits as u8;
        data[1] = if self.curframe > self.fcount && self.fcount != 0 { 1 } else { 0 };
        data[2] = 0;
        data[3] = 0;

        self.input.read_exact(&mut data[4..4 + size])?;
        if self.curbits != 0 {
            self.input.seek(SeekFrom::Current(-4))?;
        }

        let mut pkt = Packet::new(0, self.stream.time_base, data);
        pkt.pts = Some(cur as i64);
        pkt.dts = Some(cur as i64);
        pkt.duration = Some(1);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let mut target = (pts - DELAY_FRAMES).max(0);
        if target >= self.fcount as i64 && self.fcount != 0 {
            return Err(Error::invalid("mpc: seek target out of range"));
        }

        if (target as usize) < self.frames_noted {
            self.curframe = target as u32;
            let frame = self.frames[target as usize];
            self.input.seek(SeekFrom::Start(frame.pos))?;
            self.curbits = frame.skip;
            self.lastframe = self.curframe as i32 - 1;
            return Ok(target);
        }

        let lastframe = self.curframe;
        if self.frames_noted > 0 {
            self.curframe = (self.frames_noted - 1) as u32;
        }
        while (self.curframe as i64) < target {
            match self.next_packet() {
                Ok(_) => {}
                Err(e) => {
                    self.curframe = lastframe;
                    return Err(e);
                }
            }
        }
        target = self.curframe as i64;
        Ok(target)
    }
}

// ──────────────────────────────── MPC8 (SV8) ────────────────────────────────

const TAG_MPCK: u32 = u32::from_le_bytes(*b"MPCK");
const TAG_STREAMHDR: u16 = u16::from_le_bytes(*b"SH");
const TAG_STREAMEND: u16 = u16::from_le_bytes(*b"SE");
const TAG_AUDIOPACKET: u16 = u16::from_le_bytes(*b"AP");
const TAG_SEEKTBLOFF: u16 = u16::from_le_bytes(*b"SO");
const TAG_SEEKTABLE: u16 = u16::from_le_bytes(*b"ST");

const MPC8_RATES: [u32; 4] = [44100, 48000, 37800, 32000];

fn bs_get_v(bs: &[u8], mut p: usize) -> Option<(i64, usize)> {
    let mut v = 0u64;
    let mut br = 0;
    loop {
        if p >= bs.len() || br > 10 {
            return None;
        }
        let c = bs[p];
        p += 1;
        v <<= 7;
        v |= (c & 0x7f) as u64;
        br += 1;
        if (c & 0x80) == 0 {
            break;
        }
    }
    Some((v as i64 - br as i64, p))
}

pub fn mpc8_probe(probe: &ProbeData) -> u8 {
    let buf = probe.buf;
    if buf.len() < 16 {
        return 0;
    }
    let magic = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    if magic != TAG_MPCK {
        return 0;
    }

    let mut p = 4;
    while p + 3 <= buf.len() {
        let b0 = buf[p];
        let b1 = buf[p + 1];
        let header_found = b0 == b'S' && b1 == b'H';
        if !(b0.is_ascii_uppercase() && b1.is_ascii_uppercase()) {
            return 0;
        }
        p += 2;
        let Some((size, next_p)) = bs_get_v(buf, p) else {
            return 0;
        };
        p = next_p;
        if size < 2 {
            return 0;
        }
        if size >= (buf.len() - p + 2) as i64 {
            return PROBE_SCORE_EXTENSION - 1;
        }
        if header_found {
            if !(11..=28).contains(&size) {
                return 0;
            }
            if p + 4 > buf.len() {
                return 0;
            }
            let crc = u32::from_le_bytes(buf[p..p + 4].try_into().unwrap());
            if crc == 0 {
                return 0;
            }
            return MAX_PROBE_SCORE;
        } else {
            p += (size - 2) as usize;
        }
    }
    0
}

#[derive(Clone, Copy, Debug)]
struct IndexEntry {
    pos: u64,
    timestamp: i64,
}

pub struct Mpc8Demuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    header_pos: u64,
    apetag_start: Option<u64>,
    seek_table: Vec<IndexEntry>,
    pts: i64,
}

fn mpc8_get_chunk_header(input: &mut Box<dyn ReadSeek>) -> Result<(u16, i64, usize)> {
    let pos_before = input.stream_position()?;
    let mut tag_bytes = [0u8; 2];
    input.read_exact(&mut tag_bytes)?;
    let tag = u16::from_le_bytes(tag_bytes);
    let size_raw = read_varlen(input)?;
    let pos_after = input.stream_position()?;
    let header_len = (pos_after - pos_before) as usize;
    let payload_size = (size_raw as i64).saturating_sub(header_len as i64);
    Ok((tag, payload_size, header_len))
}

pub fn open_mpc8(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let header_pos = input.stream_position()?;
    let mut magic_bytes = [0u8; 4];
    input.read_exact(&mut magic_bytes)?;
    if u32::from_le_bytes(magic_bytes) != TAG_MPCK {
        return Err(Error::invalid("mpc8: not a Musepack8 file"));
    }

    let mut seek_table = Vec::new();
    let mut tag: u16;
    let mut payload_size: i64;
    let mut _header_len: usize;

    loop {
        let chunk_pos = input.stream_position()?;
        let (t, size, hlen) = mpc8_get_chunk_header(&mut input)?;
        tag = t;
        payload_size = size;
        _header_len = hlen;

        if tag == TAG_STREAMHDR {
            break;
        }

        if tag == TAG_SEEKTBLOFF {
            let off = read_varlen(&mut input)?;
            let target_pos = chunk_pos.saturating_add(off);
            input.seek(SeekFrom::Start(target_pos))?;
            let (st_tag, st_size, _) = mpc8_get_chunk_header(&mut input)?;
            if st_tag == TAG_SEEKTABLE && st_size > 0 && st_size < 1_000_000 {
                let mut st_buf = vec![0u8; st_size as usize];
                input.read_exact(&mut st_buf)?;
                parse_seek_table(&st_buf, header_pos, &mut seek_table);
            }
            input.seek(SeekFrom::Start(chunk_pos + hlen as u64 + payload_size.max(0) as u64))?;
        } else if payload_size > 0 {
            input.seek(SeekFrom::Current(payload_size))?;
        }
    }

    if tag != TAG_STREAMHDR {
        return Err(Error::invalid("mpc8: stream header not found"));
    }

    let sh_pos = input.stream_position()?;
    input.seek(SeekFrom::Current(4))?; // CRC
    let mut ver_buf = [0u8; 1];
    input.read_exact(&mut ver_buf)?;
    if ver_buf[0] != 8 {
        return Err(Error::unsupported(format!("mpc8: unsupported stream version {}", ver_buf[0])));
    }
    let total_samples = read_varlen(&mut input)?;
    let _silence_samples = read_varlen(&mut input)?;

    let mut extradata = vec![0u8; 2];
    input.read_exact(&mut extradata)?;

    let channels = ((extradata[1] >> 4) + 1) as u16;
    let rate_idx = (extradata[0] >> 5) as usize;
    if rate_idx >= MPC8_RATES.len() {
        return Err(Error::invalid("mpc8: invalid sample rate index"));
    }
    let sample_rate = MPC8_RATES[rate_idx];

    let frame_shift = (extradata[1] & 3) * 2;
    let packet_samples = 1152i64 << frame_shift;
    let time_base = reduced_time_base(packet_samples, sample_rate as i64);
    let duration = total_samples as i64 / packet_samples;

    let consumed = (input.stream_position()? - sh_pos) as i64;
    let remaining = payload_size - consumed;
    if remaining > 0 {
        input.seek(SeekFrom::Current(remaining))?;
    }

    let apetag_start = parse_ape_tag(&mut input).ok().flatten();
    input.seek(SeekFrom::Start(sh_pos + payload_size as u64))?;
    loop {
        let chunk_pos = input.stream_position()?;
        if apetag_start.is_some_and(|end| chunk_pos >= end) {
            break;
        }
        let Ok((tag, size, hlen)) = mpc8_get_chunk_header(&mut input) else {
            break;
        };
        if tag == TAG_AUDIOPACKET || tag == TAG_STREAMEND {
            input.seek(SeekFrom::Start(chunk_pos))?;
            break;
        }
        if tag == TAG_SEEKTBLOFF {
            if seek_table.is_empty() {
                let off = read_varlen(&mut input)?;
                let target_pos = chunk_pos.saturating_add(off);
                input.seek(SeekFrom::Start(target_pos))?;
                let (st_tag, st_size, _) = mpc8_get_chunk_header(&mut input)?;
                if st_tag == TAG_SEEKTABLE && st_size > 0 && st_size < 1_000_000 {
                    let mut st_buf = vec![0u8; st_size as usize];
                    input.read_exact(&mut st_buf)?;
                    parse_seek_table(&st_buf, header_pos, &mut seek_table);
                }
            }
            input.seek(SeekFrom::Start(chunk_pos + hlen as u64 + size.max(0) as u64))?;
        } else if size > 0 {
            input.seek(SeekFrom::Current(size))?;
        }
    }

    let mut params = CodecParameters::audio(CodecId::new("musepack8"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.extradata = extradata;

    let stream = StreamInfo {
        index: 0,
        params,
        time_base,
        duration: Some(duration),
        start_time: Some(0),
    };

    Ok(Box::new(Mpc8Demuxer {
        input,
        stream,
        header_pos,
        apetag_start,
        seek_table,
        pts: 0,
    }))
}

fn parse_seek_table(buf: &[u8], header_pos: u64, entries: &mut Vec<IndexEntry>) {
    let mut gb = BitReader::new(buf, buf.len());
    let size = gb.gb_get_v() as usize;
    if size > 1_000_000 {
        return;
    }
    let seekd = gb.get_bits(4);
    let mut ppos = [0u64; 2];
    for i in 0..2 {
        let pos = gb.gb_get_v().wrapping_add(header_pos);
        ppos[1 - i] = pos;
        entries.push(IndexEntry { pos, timestamp: i as i64 });
    }

    for i in 2..size {
        if gb.bits_left() < 13 {
            break;
        }
        let mut t = (gb.get_unary(1, 33) as i32) << 12;
        t += gb.get_bits(12) as i32;
        if (t & 1) != 0 {
            t = -(t & !1);
        }
        let pos = ((t >> 1) as i64).wrapping_add((ppos[0] * 2).wrapping_sub(ppos[1]) as i64) as u64;
        let ts = (i as i64) << seekd;
        entries.push(IndexEntry { pos, timestamp: ts });
        ppos[1] = ppos[0];
        ppos[0] = pos;
    }
}

impl Demuxer for Mpc8Demuxer {
    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }
    fn format_name(&self) -> &str {
        "mpc8"
    }


    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            let pos = self.input.stream_position()?;
            if let Some(tag_start) = self.apetag_start {
                if pos >= tag_start {
                    return Err(Error::Eof);
                }
            }

            let (tag, size, hlen) = match mpc8_get_chunk_header(&mut self.input) {
                Ok(h) => h,
                Err(_) => return Err(Error::Eof),
            };

            if size < 0 || size > 16 * 1024 * 1024 {
                return Err(Error::invalid("mpc8: invalid chunk size"));
            }

            if tag == TAG_AUDIOPACKET {
                let mut data = vec![0u8; size as usize];
                self.input.read_exact(&mut data)?;
                let mut pkt = Packet::new(0, self.stream.time_base, data);
                pkt.pts = Some(self.pts);
                pkt.dts = Some(self.pts);
                pkt.duration = Some(1);
                pkt.flags.keyframe = true;
                self.pts += 1;
                return Ok(pkt);
            }

            if tag == TAG_STREAMEND {
                return Err(Error::Eof);
            }

            if tag == TAG_SEEKTBLOFF {
                if self.seek_table.is_empty() {
                    let off = read_varlen(&mut self.input)?;
                    let target_pos = pos.saturating_add(off);
                    self.input.seek(SeekFrom::Start(target_pos))?;
                    let (st_tag, st_size, _) = mpc8_get_chunk_header(&mut self.input)?;
                    if st_tag == TAG_SEEKTABLE && st_size > 0 && st_size < 1_000_000 {
                        let mut st_buf = vec![0u8; st_size as usize];
                        self.input.read_exact(&mut st_buf)?;
                        parse_seek_table(&st_buf, self.header_pos, &mut self.seek_table);
                    }
                }
                self.input.seek(SeekFrom::Start(pos + hlen as u64 + size.max(0) as u64))?;
            } else if size > 0 {
                self.input.seek(SeekFrom::Current(size))?;
            }
        }
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        if self.seek_table.is_empty() {
            return Err(Error::invalid("mpc8: no seek table"));
        }

        let entry = match self.seek_table.iter().rposition(|e| e.timestamp <= pts) {
            Some(idx) => self.seek_table[idx],
            None => self.seek_table[0],
        };

        self.input.seek(SeekFrom::Start(entry.pos))?;
        self.pts = entry.timestamp;
        Ok(self.pts)
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("mpc", open_mpc);
    reg.register_extension("mpc", "mpc");
    reg.register_probe("mpc", mpc_probe);

    reg.register_demuxer("mpc8", open_mpc8);
    reg.register_extension("mpc8", "mpc8");
    reg.register_probe("mpc8", mpc8_probe);
}

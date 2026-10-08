// Ported from FFmpeg libavformat/ape.c and libavformat/apetag.c (commit 2da55bf).
//
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
//   based upon libdemac from Dave Chapman.
// Copyright (c) FFmpeg developers
//
// This file is part of FFmpeg.
// Licensed under the GNU Lesser General Public License 2.1 or later.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecTag, ContainerRegistry, Demuxer, Error, Packet,
    ProbeData, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase, MAX_PROBE_SCORE,
};

pub const APE_MIN_VERSION: u16 = 3800;
pub const APE_MAX_VERSION: u16 = 3990;

pub const MAC_FORMAT_FLAG_8_BIT: u16 = 1;
pub const MAC_FORMAT_FLAG_HAS_PEAK_LEVEL: u16 = 4;
pub const MAC_FORMAT_FLAG_24_BIT: u16 = 8;
pub const MAC_FORMAT_FLAG_HAS_SEEK_ELEMENTS: u16 = 16;
pub const MAC_FORMAT_FLAG_CREATE_WAV_HEADER: u16 = 32;

pub const APE_EXTRADATA_SIZE: usize = 6;

#[derive(Clone, Debug, Default)]
pub struct APEFrame {
    pub pos: u64,
    pub size: i64,
    pub nblocks: u32,
    pub skip: u32,
    pub pts: i64,
}

/// Monkey's Audio demuxer.
pub struct ApeDemuxer {
    input: Box<dyn ReadSeek>,
    frames: Vec<APEFrame>,
    currentframe: usize,
    totalframes: usize,
    blocksperframe: u32,
    finalframeblocks: u32,
    streams: Vec<StreamInfo>,
    metadata: Vec<(String, String)>,
    file_size: u64,
}

impl ApeDemuxer {
    pub fn new(
        input: Box<dyn ReadSeek>,
        frames: Vec<APEFrame>,
        totalframes: usize,
        blocksperframe: u32,
        finalframeblocks: u32,
        streams: Vec<StreamInfo>,
        metadata: Vec<(String, String)>,
        file_size: u64,
    ) -> Self {
        Self {
            input,
            frames,
            currentframe: 0,
            totalframes,
            blocksperframe,
            finalframeblocks,
            streams,
            metadata,
            file_size,
        }
    }
}

impl Demuxer for ApeDemuxer {
    fn format_name(&self) -> &str {
        "ape"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.currentframe >= self.totalframes {
            return Err(Error::Eof);
        }

        let frame = &self.frames[self.currentframe];
        if frame.pos >= self.file_size {
            return Err(Error::Eof);
        }

        let nblocks = if self.currentframe == self.totalframes - 1 {
            self.finalframeblocks
        } else {
            self.blocksperframe
        };

        self.input.seek(SeekFrom::Start(frame.pos))?;

        let extra_size = 8usize;
        if frame.size <= 0 || frame.size > (i32::MAX - extra_size as i32) as i64 {
            self.currentframe += 1;
            return Err(Error::invalid("ape: invalid packet size"));
        }
        let read_size = frame.size as usize;
        let available = (self.file_size.saturating_sub(frame.pos) as usize).min(read_size);
        let mut payload = vec![0u8; available + extra_size];
        payload[0..4].copy_from_slice(&nblocks.to_le_bytes());
        payload[4..8].copy_from_slice(&frame.skip.to_le_bytes());

        let mut total_read = 0usize;
        while total_read < read_size {
            match self.input.read(&mut payload[extra_size + total_read..]) {
                Ok(0) => break,
                Ok(n) => total_read += n,
                Err(e) => return Err(Error::from(e)),
            }
        }
        if total_read == 0 && read_size > 0 {
            return Err(Error::Eof);
        }
        payload.truncate(extra_size + total_read);
        let mut packet = Packet::new(0, self.streams[0].time_base, payload);
        packet.pts = Some(frame.pts);
        packet.duration = Some(nblocks as i64);
        packet.flags.keyframe = true;

        self.currentframe += 1;
        Ok(packet)
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index != 0 {
            return Err(Error::invalid("ape: invalid stream index"));
        }
        if self.frames.is_empty() {
            return Err(Error::invalid("ape: no frames to seek"));
        }

        let target = pts.max(0);
        let mut chosen = 0;
        for (i, frame) in self.frames.iter().enumerate() {
            if frame.pts <= target {
                chosen = i;
            } else {
                break;
            }
        }

        self.currentframe = chosen;
        self.input.seek(SeekFrom::Start(self.frames[chosen].pos))?;
        Ok(self.frames[chosen].pts)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
}

/// Probes whether the input is a Monkey's Audio stream.
pub fn ape_probe(probe: &ProbeData) -> u8 {
    let buf = probe.buf;
    let mut pos = 0usize;

    // Skip any leading ID3v2 tag
    if buf.len() >= 10 && buf.starts_with(b"ID3") {
        let tag_size = ((buf[6] as usize & 0x7f) << 21)
            | ((buf[7] as usize & 0x7f) << 14)
            | ((buf[8] as usize & 0x7f) << 7)
            | (buf[9] as usize & 0x7f);
        let footer = if buf[5] & 0x10 != 0 { 10 } else { 0 };
        pos = 10 + tag_size + footer;
    }

    if pos + 6 <= buf.len() && &buf[pos..pos + 4] == b"MAC " {
        let version = u16::from_le_bytes([buf[pos + 4], buf[pos + 5]]);
        if version < APE_MIN_VERSION || version > APE_MAX_VERSION {
            return MAX_PROBE_SCORE / 4;
        }
        return MAX_PROBE_SCORE;
    }

    0
}

fn read_u8(input: &mut dyn ReadSeek) -> Result<u8> {
    let mut b = [0u8; 1];
    input.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16_le(input: &mut dyn ReadSeek) -> Result<u16> {
    let mut b = [0u8; 2];
    input.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

fn read_u32_le(input: &mut dyn ReadSeek) -> Result<u32> {
    let mut b = [0u8; 4];
    input.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// Reads APE tags at the end of the input if present.
fn parse_ape_tags(input: &mut dyn ReadSeek, file_size: u64) -> Vec<(String, String)> {
    let mut tags = Vec::new();
    if file_size < 32 {
        return tags;
    }

    if input.seek(SeekFrom::Start(file_size - 32)).is_err() {
        return tags;
    }

    let mut footer = [0u8; 32];
    if input.read_exact(&mut footer).is_err() {
        return tags;
    }

    if &footer[0..8] != b"APETAGEX" {
        return tags;
    }

    let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    if version > 2000 {
        return tags;
    }

    let mut tag_bytes = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as u64;
    let fields = u32::from_le_bytes(footer[16..20].try_into().unwrap());
    let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());

    if (flags & (1 << 29)) != 0 {
        // Tag is a header
        return tags;
    }

    if (flags & (1 << 31)) != 0 {
        tag_bytes = tag_bytes.saturating_add(32);
    }

    if tag_bytes > file_size {
        return tags;
    }

    let tag_start = file_size - tag_bytes;
    if input.seek(SeekFrom::Start(tag_start)).is_err() {
        return tags;
    }

    if (flags & (1 << 31)) != 0 {
        // Skip header
        let _ = input.seek(SeekFrom::Current(32));
    }

    for _ in 0..fields.min(65536) {
        let val_len = match read_u32_le(input) {
            Ok(v) => v as usize,
            Err(_) => break,
        };
        let field_flags = match read_u32_le(input) {
            Ok(f) => f,
            Err(_) => break,
        };
        let mut key_bytes = Vec::new();
        loop {
            match read_u8(input) {
                Ok(0) => break,
                Ok(c) if (0x20..=0x7E).contains(&c) => {
                    if key_bytes.len() < 1024 {
                        key_bytes.push(c);
                    }
                }
                _ => break,
            }
        }
        if key_bytes.is_empty() {
            break;
        }
        let key = String::from_utf8_lossy(&key_bytes).into_owned();

        if (field_flags & 2) != 0 {
            // Binary tag: skip
            if input.seek(SeekFrom::Current(val_len as i64)).is_err() {
                break;
            }
        } else {
            let mut val_bytes = vec![0u8; val_len.min(1024 * 1024)];
            if input.read_exact(&mut val_bytes).is_err() {
                break;
            }
            let val = String::from_utf8_lossy(&val_bytes).into_owned();
            tags.push((key, val));
        }
    }

    tags
}

/// Opens an APE stream / file.
pub fn open_ape(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn oxideav_core::CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut head = [0u8; 10];
    input.read_exact(&mut head)?;

    let mut junklength = 0u64;
    if &head[0..3] == b"ID3" {
        let tag_size = ((head[6] as usize & 0x7f) << 21)
            | ((head[7] as usize & 0x7f) << 14)
            | ((head[8] as usize & 0x7f) << 7)
            | (head[9] as usize & 0x7f);
        let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
        junklength = (10 + tag_size + footer) as u64;
        input.seek(SeekFrom::Start(junklength))?;
    } else {
        input.seek(SeekFrom::Start(0))?;
    }

    let mut tag_buf = [0u8; 4];
    input.read_exact(&mut tag_buf)?;
    if &tag_buf != b"MAC " {
        return Err(Error::invalid("ape: missing MAC signature"));
    }

    let fileversion = read_u16_le(&mut *input)?;
    if !(APE_MIN_VERSION..=APE_MAX_VERSION).contains(&fileversion) {
        return Err(Error::unsupported(format!(
            "ape: unsupported version {}.{:02}",
            fileversion / 1000,
            (fileversion % 1000) / 10
        )));
    }

    let descriptorlength: u32;
    let mut headerlength: u32;
    let seektablelength: u32;
    let wavheaderlength: u32;
    let wavtaillength: u32;
    let compressiontype: u16;
    let formatflags: u16;
    let blocksperframe: u32;
    let finalframeblocks: u32;
    let totalframes: u32;
    let bps: u16;
    let channels: u16;
    let samplerate: u32;

    if fileversion >= 3980 {
        let _padding1 = read_u16_le(&mut *input)?;
        descriptorlength = read_u32_le(&mut *input)?;
        headerlength = read_u32_le(&mut *input)?;
        seektablelength = read_u32_le(&mut *input)?;
        wavheaderlength = read_u32_le(&mut *input)?;
        let _audiodatalength = read_u32_le(&mut *input)?;
        let _audiodatalength_high = read_u32_le(&mut *input)?;
        wavtaillength = read_u32_le(&mut *input)?;
        let mut _md5 = [0u8; 16];
        input.read_exact(&mut _md5)?;

        if descriptorlength > 52 {
            input.seek(SeekFrom::Current((descriptorlength - 52) as i64))?;
        }

        compressiontype = read_u16_le(&mut *input)?;
        formatflags = read_u16_le(&mut *input)?;
        blocksperframe = read_u32_le(&mut *input)?;
        finalframeblocks = read_u32_le(&mut *input)?;
        totalframes = read_u32_le(&mut *input)?;
        bps = read_u16_le(&mut *input)?;
        channels = read_u16_le(&mut *input)?;
        samplerate = read_u32_le(&mut *input)?;

    } else {
        descriptorlength = 0;
        headerlength = 32;

        compressiontype = read_u16_le(&mut *input)?;
        formatflags = read_u16_le(&mut *input)?;
        channels = read_u16_le(&mut *input)?;
        samplerate = read_u32_le(&mut *input)?;
        wavheaderlength = read_u32_le(&mut *input)?;
        wavtaillength = read_u32_le(&mut *input)?;
        totalframes = read_u32_le(&mut *input)?;
        finalframeblocks = read_u32_le(&mut *input)?;

        if (formatflags & MAC_FORMAT_FLAG_HAS_PEAK_LEVEL) != 0 {
            input.seek(SeekFrom::Current(4))?;
            headerlength += 4;
        }

        if (formatflags & MAC_FORMAT_FLAG_HAS_SEEK_ELEMENTS) != 0 {
            let seekelements = read_u32_le(&mut *input)?;
            headerlength += 4;
            seektablelength = seekelements.saturating_mul(4);
        } else {
            seektablelength = totalframes.saturating_mul(4);
        }

        if (formatflags & MAC_FORMAT_FLAG_8_BIT) != 0 {
            bps = 8;
        } else if (formatflags & MAC_FORMAT_FLAG_24_BIT) != 0 {
            bps = 24;
        } else {
            bps = 16;
        }

        if fileversion >= 3950 {
            blocksperframe = 73728 * 4;
        } else if fileversion >= 3900 || (fileversion >= 3800 && compressiontype >= 4000) {
            blocksperframe = 73728;
        } else {
            blocksperframe = 9216;
        }

        if (formatflags & MAC_FORMAT_FLAG_CREATE_WAV_HEADER) == 0 {
            input.seek(SeekFrom::Current(wavheaderlength as i64))?;
        }
    }

    if totalframes == 0 || totalframes > 1_000_000 {
        return Err(Error::invalid(format!("ape: invalid totalframes {totalframes}")));
    }
    if (seektablelength / 4) < totalframes {
        return Err(Error::invalid("ape: seek table too small"));
    }
    if channels == 0 || channels > 2 {
        return Err(Error::invalid(format!("ape: unsupported channel count {channels}")));
    }
    if bps != 8 && bps != 16 && bps != 24 {
        return Err(Error::invalid(format!("ape: unsupported bit depth {bps}")));
    }

    let mut firstframe = junklength
        + descriptorlength as u64
        + headerlength as u64
        + seektablelength as u64
        + wavheaderlength as u64;
    if fileversion < 3810 {
        firstframe += totalframes as u64;
    }

    let mut frames = vec![APEFrame::default(); totalframes as usize];
    frames[0].pos = firstframe;
    frames[0].nblocks = blocksperframe;
    frames[0].skip = 0;

    let _seek0 = read_u32_le(&mut *input)?;
    for i in 1..totalframes as usize {
        let entry = read_u32_le(&mut *input)?;
        frames[i].pos = entry as u64 + junklength;
        frames[i].nblocks = blocksperframe;
        frames[i - 1].size = (frames[i].pos as i64) - (frames[i - 1].pos as i64);
        frames[i].skip = (frames[i].pos.wrapping_sub(frames[0].pos) & 3) as u32;
    }

    let remaining_seek = (seektablelength / 4).saturating_sub(totalframes);
    if remaining_seek > 0 {
        input.seek(SeekFrom::Current((remaining_seek * 4) as i64))?;
    }

    let last_idx = totalframes as usize - 1;
    frames[last_idx].nblocks = finalframeblocks;

    let current_pos = input.stream_position()?;
    let file_size = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(current_pos))?;
    let mut final_size = (file_size as i64)
        .saturating_sub(frames[last_idx].pos as i64)
        .saturating_sub(wavtaillength as i64);
    final_size -= final_size & 3;
    if file_size == 0 || final_size <= 0 {
        final_size = (finalframeblocks as i64) * 8;
    }
    frames[last_idx].size = final_size;

    for frame in &mut frames {
        if frame.skip != 0 {
            frame.pos = frame.pos.saturating_sub(frame.skip as u64);
            frame.size = frame.size.saturating_add(frame.skip as i64);
        }
        if frame.size > (i32::MAX - 3) as i64 {
            return Err(Error::invalid("ape: invalid frame size"));
        }
        frame.size = (frame.size + 3) & !3;
    }

    if fileversion < 3810 {
        for i in 0..totalframes as usize {
            let bits = read_u8(&mut *input)?;
            if i > 0 && bits != 0 {
                frames[i - 1].size += 4;
            }
            frames[i].skip = (frames[i].skip << 3) + bits as u32;
        }
    }

    let mut pts = 0i64;
    for frame in &mut frames {
        frame.pts = pts;
        pts += blocksperframe as i64;
    }

    let total_blocks = if totalframes == 0 {
        0
    } else {
        (totalframes as i64 - 1) * blocksperframe as i64 + finalframeblocks as i64
    };

    let mut extradata = vec![0u8; APE_EXTRADATA_SIZE];
    extradata[0..2].copy_from_slice(&fileversion.to_le_bytes());
    extradata[2..4].copy_from_slice(&compressiontype.to_le_bytes());
    extradata[4..6].copy_from_slice(&formatflags.to_le_bytes());

    let mut params = CodecParameters::audio(CodecId::new("ape"));
    params.tag = Some(CodecTag::fourcc(b"APE "));
    params.channels = Some(channels);
    params.sample_rate = Some(samplerate);
    params.sample_format = Some(match bps {
        8 => SampleFormat::U8P,
        16 => SampleFormat::S16P,
        24 => SampleFormat::S32P,
        _ => SampleFormat::S16P,
    });
    params.extradata = extradata;

    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::from_rate(samplerate),
        duration: Some(total_blocks),
        start_time: Some(0),
    };

    let metadata = parse_ape_tags(&mut *input, file_size);

    Ok(Box::new(ApeDemuxer::new(
        input,
        frames,
        totalframes as usize,
        blocksperframe,
        finalframeblocks,
        vec![stream],
        metadata,
        file_size,
    )))
}

/// Registers the APE demuxer into the container registry.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("ape", open_ape);
    reg.register_extension("ape", "ape");
    reg.register_extension("ape", "apl");
    reg.register_extension("ape", "mac");
    reg.register_probe("ape", ape_probe);
}

// Ported from FFmpeg libavformat/wvdec.c, libavformat/wv.c, libavformat/wv.h, libavformat/apetag.c (commit 2da55bf)
// Copyright (c) 2006, 2011 Konstantin Shishkov
// Copyright (c) 2007 Benjamin Zores <ben@geexbox.org>
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::io::{Read, Seek, SeekFrom};
use demux_seek_core::{read_on, Allowance, Index};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData,
    ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
};

use crate::common::*;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WvHeader {
    pub blocksize: u32,
    pub version: u16,
    pub total_samples: u32,
    pub block_idx: u32,
    pub samples: u32,
    pub flags: u32,
    pub crc: u32,
    pub initial: bool,
    pub final_block: bool,
}

pub fn parse_header(data: &[u8]) -> Result<WvHeader> {
    if data.len() < WV_HEADER_SIZE {
        return Err(Error::invalid("header too short"));
    }
    if &data[0..4] != b"wvpk" {
        return Err(Error::invalid("not a wavpack header"));
    }
    let raw_blocksize = u32::from_le_bytes(data[4..8].try_into().unwrap());
    if raw_blocksize < 24 || raw_blocksize > WV_BLOCK_LIMIT {
        return Err(Error::invalid("invalid block size"));
    }
    let blocksize = raw_blocksize - 24;
    let version = u16::from_le_bytes(data[8..10].try_into().unwrap());
    let total_samples = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let block_idx = u32::from_le_bytes(data[16..20].try_into().unwrap());
    let samples = u32::from_le_bytes(data[20..24].try_into().unwrap());
    let flags = u32::from_le_bytes(data[24..28].try_into().unwrap());
    let crc = u32::from_le_bytes(data[28..32].try_into().unwrap());

    let initial = (flags & WV_INITIAL_BLOCK) != 0;
    let final_block = (flags & WV_FINAL_BLOCK) != 0;

    Ok(WvHeader {
        blocksize,
        version,
        total_samples,
        block_idx,
        samples,
        flags,
        crc,
        initial,
        final_block,
    })
}

pub fn wv_probe(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() <= 32 {
        return 0;
    }
    if &p[..4] == b"wvpk" {
        let bsize = u32::from_le_bytes([p[4], p[5], p[6], p[7]]);
        let ver = u16::from_le_bytes([p[8], p[9]]);
        if (24..=WV_BLOCK_LIMIT).contains(&bsize) && (0x402..=0x410).contains(&ver) {
            return oxideav_core::MAX_PROBE_SCORE;
        }
    }
    0
}

fn parse_ape_tag(reader: &mut (impl Read + Seek + ?Sized)) -> Result<(Option<u64>, Vec<(String, String)>)> {
    let file_size = reader.seek(SeekFrom::End(0))?;
    if file_size < 32 {
        return Ok((None, Vec::new()));
    }

    let mut footer_pos = file_size - 32;
    reader.seek(SeekFrom::Start(footer_pos))?;
    let mut footer = [0u8; 32];
    if reader.read_exact(&mut footer).is_err() {
        return Ok((None, Vec::new()));
    }

    let mut found = &footer[..8] == b"APETAGEX";
    if !found && file_size > 128 + 32 {
        footer_pos = file_size - 128 - 32;
        reader.seek(SeekFrom::Start(footer_pos))?;
        if reader.read_exact(&mut footer).is_ok() && &footer[..8] == b"APETAGEX" {
            found = true;
        }
    }

    if !found {
        return Ok((None, Vec::new()));
    }

    let version = u32::from_le_bytes(footer[8..12].try_into().unwrap());
    if version > 2000 {
        return Ok((None, Vec::new()));
    }
    let tag_bytes = u32::from_le_bytes(footer[12..16].try_into().unwrap()) as u64;
    if tag_bytes < 32 || tag_bytes - 32 > 16 * 1024 * 1024 || tag_bytes > footer_pos + 32 {
        return Ok((None, Vec::new()));
    }
    let fields = u32::from_le_bytes(footer[16..20].try_into().unwrap());
    if fields > 65536 {
        return Ok((None, Vec::new()));
    }
    let flags = u32::from_le_bytes(footer[20..24].try_into().unwrap());
    if (flags & (1 << 29)) != 0 {
        return Ok((None, Vec::new()));
    }

    let total_tag_bytes = if (flags & (1 << 31)) != 0 {
        tag_bytes + 32
    } else {
        tag_bytes
    };
    let tag_start = (footer_pos + 32).saturating_sub(total_tag_bytes);

    let fields_start = if (flags & (1 << 31)) != 0 {
        tag_start + 32
    } else {
        tag_start
    };

    let mut metadata = Vec::new();
    if reader.seek(SeekFrom::Start(fields_start)).is_ok() {
        for _ in 0..fields {
            let mut val_len_buf = [0u8; 4];
            if reader.read_exact(&mut val_len_buf).is_err() {
                break;
            }
            let val_len = u32::from_le_bytes(val_len_buf) as usize;

            let mut item_flags_buf = [0u8; 4];
            if reader.read_exact(&mut item_flags_buf).is_err() {
                break;
            }
            let item_flags = u32::from_le_bytes(item_flags_buf);

            let mut key_bytes = Vec::new();
            let mut b = [0u8; 1];
            loop {
                if reader.read_exact(&mut b).is_err() || b[0] == 0 || key_bytes.len() > 256 {
                    break;
                }
                key_bytes.push(b[0]);
            }
            let key = String::from_utf8_lossy(&key_bytes).into_owned();

            if val_len > 1024 * 1024 {
                break;
            }
            let mut val_bytes = vec![0u8; val_len];
            if reader.read_exact(&mut val_bytes).is_err() {
                break;
            }

            if (item_flags & 6) == 0 {
                let val_str = String::from_utf8_lossy(&val_bytes).into_owned();
                metadata.push((key, val_str));
            }
        }
    }

    Ok((Some(tag_start), metadata))
}

const WV_DEMUX_RATES: [i32; 16] = [
    6000, 8000, 9600, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000,
    192000, -1,
];

pub struct RawWvDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    apetag_start: Option<u64>,
    metadata: Vec<(String, String)>,
    index: Index,
    allowance: Allowance,
    data_offset: u64,
    header: WvHeader,
    block_header: [u8; WV_HEADER_SIZE],
    block_parsed: bool,
    pos: u64,
    multichannel: bool,
    chan: u16,
    rate: u32,
    bpp: u8,
    chmask: u64,
}

impl RawWvDemuxer {
    pub fn open(input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Self> {
        let allowance = Allowance::default();
        let mut input = Box::new(allowance.meter(input));
        let (apetag_start, metadata) = parse_ape_tag(&mut *input).unwrap_or((None, Vec::new()));
        input.seek(SeekFrom::Start(0))?;

        let mut demuxer = Self {
            input,
            streams: Vec::new(),
            apetag_start,
            metadata,
            index: Index::default(),
            allowance,
            data_offset: 0,
            header: WvHeader::default(),
            block_header: [0u8; WV_HEADER_SIZE],
            block_parsed: false,
            pos: 0,
            multichannel: false,
            chan: 0,
            rate: 0,
            bpp: 0,
            chmask: 0,
        };

        loop {
            demuxer.read_block_header()?;
            if demuxer.header.samples == 0 {
                demuxer
                    .input
                    .seek(SeekFrom::Current(demuxer.header.blocksize as i64))?;
            } else {
                break;
            }
        }

        demuxer.data_offset = demuxer.pos;
        if demuxer.chmask != 0 {
            demuxer.chan = demuxer.chmask.count_ones() as u16;
        }

        let extradata = demuxer.header.version.to_le_bytes().to_vec();
        let mut params = CodecParameters::audio(CodecId::new("wavpack"));
        params.sample_rate = Some(demuxer.rate);
        params.channels = Some(demuxer.chan);
        params.sample_format = if (demuxer.header.flags & WV_DSD_DATA) != 0 {
            Some(SampleFormat::F32P)
        } else if (demuxer.header.flags & 3) <= 1 {
            Some(SampleFormat::S16P)
        } else {
            Some(SampleFormat::S32P)
        };
        params.extradata = extradata;

        let time_base = TimeBase::new(1, demuxer.rate as i64);
        let duration = if demuxer.header.total_samples != 0xFFFF_FFFF {
            Some(demuxer.header.total_samples as i64)
        } else {
            None
        };
        let stream_info = StreamInfo {
            index: 0,
            time_base,
            duration,
            start_time: Some(0),
            params,
        };
        demuxer.streams.push(stream_info);

        Ok(demuxer)
    }

    fn read_block_header(&mut self) -> Result<()> {
        self.pos = self.input.stream_position()?;
        if let Some(ape_start) = self.apetag_start {
            if self.pos >= ape_start {
                return Err(Error::Eof);
            }
        }

        match self.input.read_exact(&mut self.block_header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Eof),
            Err(e) => return Err(Error::Io(e)),
        }

        self.header = parse_header(&self.block_header)?;
        if self.header.version < 0x402 || self.header.version > 0x410 {
            return Err(Error::unsupported(format!(
                "WV version 0x{:03X}",
                self.header.version
            )));
        }

        if self.header.samples == 0 {
            return Ok(());
        }

        let flags = self.header.flags;
        let mut rate_x = if (flags & WV_DSD_DATA) != 0 { 4 } else { 1 };
        let bpp = if (flags & WV_DSD_DATA) != 0 {
            0
        } else {
            (((flags & 3) + 1) * 8) as u8
        };
        let mut chan = 1 + if (flags & WV_MONO) != 0 { 0 } else { 1 };
        let mut chmask = if (flags & WV_MONO) != 0 { 4 } else { 3 };
        let mut rate = WV_DEMUX_RATES[((flags >> 23) & 0xF) as usize];
        self.multichannel = !(self.header.initial && self.header.final_block);
        if self.multichannel {
            chan = self.chan;
            chmask = self.chmask;
        }

        if (rate == -1 || chan == 0 || (flags & WV_DSD_DATA) != 0) && !self.block_parsed {
            let block_end = self.input.stream_position()? + self.header.blocksize as u64;
            let mut cur = self.input.stream_position()?;
            while cur < block_end {
                let mut id_buf = [0u8; 1];
                if self.input.read_exact(&mut id_buf).is_err() {
                    break;
                }
                let id = id_buf[0];
                let mut size_buf = [0u8; 1];
                if self.input.read_exact(&mut size_buf).is_err() {
                    break;
                }
                let mut raw_size = size_buf[0] as i32;
                if (id & WP_IDF_LONG) != 0 {
                    let mut size_hi = [0u8; 2];
                    if self.input.read_exact(&mut size_hi).is_err() {
                        break;
                    }
                    raw_size |= (u16::from_le_bytes(size_hi) as i32) << 8;
                }
                let mut size = raw_size << 1;
                if (id & WP_IDF_ODD) != 0 {
                    size -= 1;
                }
                if size < 0 {
                    break;
                }
                let size = size as usize;
                let sub_id = id & WP_IDF_MASK;
                match sub_id {
                    WP_ID_CHANINFO => {
                        if size <= 1 {
                            return Err(Error::invalid("insufficient channel info"));
                        }
                        let mut b = [0u8; 1];
                        self.input.read_exact(&mut b)?;
                        chan = b[0] as u16;
                        match size - 2 {
                            0 => {
                                let mut m = [0u8; 1];
                                self.input.read_exact(&mut m)?;
                                chmask = m[0] as u64;
                            }
                            1 => {
                                let mut m = [0u8; 2];
                                self.input.read_exact(&mut m)?;
                                chmask = u16::from_le_bytes(m) as u64;
                            }
                            2 => {
                                let mut m = [0u8; 3];
                                self.input.read_exact(&mut m)?;
                                chmask = (m[0] as u64) | ((m[1] as u64) << 8) | ((m[2] as u64) << 16);
                            }
                            3 => {
                                let mut m = [0u8; 4];
                                self.input.read_exact(&mut m)?;
                                chmask = u32::from_le_bytes(m) as u64;
                            }
                            4 => {
                                let mut skip = [0u8; 1];
                                self.input.read_exact(&mut skip)?;
                                let mut b2 = [0u8; 1];
                                self.input.read_exact(&mut b2)?;
                                chan |= ((b2[0] & 0xF) as u16) << 8;
                                chan += 1;
                                let mut m = [0u8; 3];
                                self.input.read_exact(&mut m)?;
                                chmask = (m[0] as u64) | ((m[1] as u64) << 8) | ((m[2] as u64) << 16);
                            }
                            5 => {
                                let mut skip = [0u8; 1];
                                self.input.read_exact(&mut skip)?;
                                let mut b2 = [0u8; 1];
                                self.input.read_exact(&mut b2)?;
                                chan |= ((b2[0] & 0xF) as u16) << 8;
                                chan += 1;
                                let mut m = [0u8; 4];
                                self.input.read_exact(&mut m)?;
                                chmask = u32::from_le_bytes(m) as u64;
                            }
                            _ => return Err(Error::invalid("invalid channel info size")),
                        }
                    }
                    WP_ID_DSD_DATA => {
                        if size <= 1 {
                            return Err(Error::invalid("invalid DSD block"));
                        }
                        let mut b = [0u8; 1];
                        self.input.read_exact(&mut b)?;
                        rate_x = 1 << (b[0] & 0x1f);
                        if size > 1 {
                            self.input.seek(SeekFrom::Current((size - 1) as i64))?;
                        }
                    }
                    WP_ID_SAMPLE_RATE => {
                        if size != 3 {
                            return Err(Error::invalid("invalid sample rate size"));
                        }
                        let mut r = [0u8; 3];
                        self.input.read_exact(&mut r)?;
                        rate = (r[0] as i32) | ((r[1] as i32) << 8) | ((r[2] as i32) << 16);
                    }
                    _ => {
                        self.input.seek(SeekFrom::Current(size as i64))?;
                    }
                }
                if (id & WP_IDF_ODD) != 0 {
                    self.input.seek(SeekFrom::Current(1))?;
                }
                cur = self.input.stream_position()?;
            }
            if rate == -1 || (rate as u64).wrapping_mul(rate_x as u64) >= i32::MAX as u64 {
                return Err(Error::invalid("cannot determine sampling rate"));
            }
            self.input
                .seek(SeekFrom::Start(block_end - self.header.blocksize as u64))?;
        }

        if self.bpp == 0 {
            self.bpp = bpp;
        }
        if self.chan == 0 {
            self.chan = chan;
        }
        if self.chmask == 0 {
            self.chmask = chmask;
        }
        if self.rate == 0 {
            self.rate = (rate as u32).wrapping_mul(rate_x);
        }

        if flags != 0 && bpp != self.bpp {
            return Err(Error::invalid("bits per sample differ"));
        }
        if flags != 0 && !self.multichannel && chan != self.chan {
            return Err(Error::invalid("channels differ"));
        }
        if flags != 0
            && rate != -1
            && (flags & WV_DSD_DATA) == 0
            && (rate as u32).wrapping_mul(rate_x) != self.rate
        {
            return Err(Error::invalid("sampling rate differs"));
        }

        Ok(())
    }
}

trait GenericSeek {
    type Reading;
    fn index(&self) -> &Index;
    fn read(&mut self) -> Result<(bool, Option<i64>)>;
    fn restart(&mut self, pos: u64, ts: Option<i64>) -> Result<()>;
    fn data_offset(&self) -> u64;
    fn allowance(&mut self) -> &mut Allowance;
    fn take_reading(&mut self) -> Result<Self::Reading>;
    fn give_back(&mut self, reading: Self::Reading) -> Result<()>;
}

fn seek_generic<D: GenericSeek>(d: &mut D, ts: i64) -> Result<i64> {
    let found = d.index().search(ts, true);
    if found.is_none() && d.index().entries().first().is_some_and(|e| ts < e.timestamp) {
        return Err(Error::invalid("seek before the first frame"));
    }
    let reading = d.take_reading()?;
    d.allowance().start();
    let landed = land(d, ts, found);
    let landed = d.allowance().finish(landed);
    if landed.is_err() {
        d.give_back(reading)?;
    }
    landed
}

fn land<D: GenericSeek>(d: &mut D, ts: i64, mut found: Option<usize>) -> Result<i64> {
    if found.is_none() || found == Some(d.index().entries().len() - 1) {
        match d.index().entries().last().copied() {
            Some(e) => d.restart(e.pos as u64, Some(e.timestamp))?,
            None => {
                let at = d.data_offset();
                d.restart(at, None)?;
            }
        }
        read_on(ts, || d.read())?;
        found = d.index().search(ts, true);
    }
    let Some(i) = found else {
        return Err(Error::invalid("no key frame to seek to"));
    };
    let e = d.index().entries()[i];
    d.restart(e.pos as u64, Some(e.timestamp))?;
    Ok(e.timestamp)
}

impl GenericSeek for RawWvDemuxer {
    type Reading = (u64, bool, WvHeader, [u8; WV_HEADER_SIZE]);

    fn index(&self) -> &Index {
        &self.index
    }

    fn read(&mut self) -> Result<(bool, Option<i64>)> {
        let pkt = self.next_packet()?;
        Ok((pkt.flags.keyframe, pkt.dts))
    }

    fn restart(&mut self, pos: u64, _ts: Option<i64>) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos))?;
        self.block_parsed = true;
        Ok(())
    }

    fn data_offset(&self) -> u64 {
        self.data_offset
    }

    fn allowance(&mut self) -> &mut Allowance {
        &mut self.allowance
    }

    fn take_reading(&mut self) -> Result<Self::Reading> {
        Ok((
            self.input.stream_position()?,
            self.block_parsed,
            self.header,
            self.block_header,
        ))
    }

    fn give_back(&mut self, (pos, block_parsed, header, block_header): Self::Reading) -> Result<()> {
        self.input.seek(SeekFrom::Start(pos))?;
        self.block_parsed = block_parsed;
        self.header = header;
        self.block_header = block_header;
        Ok(())
    }
}

impl Demuxer for RawWvDemuxer {
    fn format_name(&self) -> &str {
        "wv"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if self.block_parsed {
            self.read_block_header()?;
        }

        let pos = self.pos;
        let mut data = Vec::with_capacity(WV_HEADER_SIZE + self.header.blocksize as usize);
        data.extend_from_slice(&self.block_header);
        let cur_len = data.len();
        data.resize(cur_len + self.header.blocksize as usize, 0);
        match self.input.read_exact(&mut data[cur_len..]) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Eof),
            Err(e) => return Err(Error::Io(e)),
        }

        while !self.header.final_block {
            self.read_block_header()?;
            let off = data.len();
            data.extend_from_slice(&self.block_header);
            data.resize(off + WV_HEADER_SIZE + self.header.blocksize as usize, 0);
            match self.input.read_exact(&mut data[off + WV_HEADER_SIZE..]) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(Error::Eof),
                Err(e) => return Err(Error::Io(e)),
            }
        }

        self.block_parsed = true;
        let pts = self.header.block_idx as i64;
        let duration = self.header.samples as i64;

        let tb = self.streams[0].time_base;
        let pkt = Packet::new(0, tb, data)
            .with_pts(pts)
            .with_dts(pts)
            .with_duration(duration)
            .with_keyframe(true);

        self.index.add(pos as i64, pts, 0, 0, true);

        Ok(pkt)
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        seek_generic(self, pts)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }
}

pub fn open_wv(
    input: Box<dyn ReadSeek>,
    codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    Ok(Box::new(RawWvDemuxer::open(input, codecs)?))
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    reg.register_demuxer("wv", open_wv);
    reg.register_probe("wv", wv_probe);
    reg.register_extension("wv", "wv");
}

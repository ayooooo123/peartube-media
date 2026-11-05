// Ported from FFmpeg libavformat/dsfdec.c and libavformat/iff.c (commit 2da55bf).
// Copyright (c) 2014 Peter Ross
// Licensed under LGPL-2.1-or-later.

//! Sony DSF (`.dsf`) and Philips DSDIFF (`.dff`, `FRM8 ... DSD `) demuxers.

#![forbid(unsafe_code)]

use std::io::{Read, SeekFrom};

use oxideav_core::{
    ChannelLayout, CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
    MAX_PROBE_SCORE,
};

const MAX_DSD_CHANNELS: u32 = 16;
const MAX_DSF_BLOCK_ALIGN_PER_CH: u32 = 1_048_576;

pub fn probe_dsf(probe: &ProbeData) -> ProbeScore {
    let b = probe.buf;
    if b.len() < 12 || &b[0..4] != b"DSD " {
        return 0;
    }
    let hdr_size = u64::from_le_bytes(b[4..12].try_into().unwrap());
    if hdr_size == 28 { MAX_PROBE_SCORE } else { 0 }
}

pub fn probe_dff(probe: &ProbeData) -> ProbeScore {
    let b = probe.buf;
    if b.len() < 16 {
        return 0;
    }
    if &b[0..4] == b"FRM8" && &b[12..16] == b"DSD " {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

fn dsf_layout(channel_type: u32, channels: u16) -> ChannelLayout {
    match (channel_type, channels) {
        (1, 1) => ChannelLayout::Mono,
        (2, 2) => ChannelLayout::Stereo,
        (3, 3) => ChannelLayout::Surround30,
        (4, 4) => ChannelLayout::Quad,
        (6, 5) => ChannelLayout::Surround50,
        (7, 6) => ChannelLayout::Surround51,
        _ => ChannelLayout::from_count(channels),
    }
}

struct DsfDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    data_offset: u64,
    data_end: u64,
    data_size: u64,
    audio_size: u64,
    block_align: u64,
    channels: u16,
}

fn read_exact_arr<const N: usize>(r: &mut dyn ReadSeek) -> Result<[u8; N]> {
    let mut buf = [0u8; N];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn open_dsf(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let dsd_hdr = read_exact_arr::<28>(&mut *input)?;
    if &dsd_hdr[0..4] != b"DSD " || u64::from_le_bytes(dsd_hdr[4..12].try_into().unwrap()) != 28 {
        return Err(Error::invalid("dsf: invalid DSD header"));
    }

    let fmt_hdr = read_exact_arr::<52>(&mut *input)?;
    if &fmt_hdr[0..4] != b"fmt " || u64::from_le_bytes(fmt_hdr[4..12].try_into().unwrap()) != 52 {
        return Err(Error::invalid("dsf: invalid fmt chunk"));
    }
    let version = u32::from_le_bytes(fmt_hdr[12..16].try_into().unwrap());
    let format_id = u32::from_le_bytes(fmt_hdr[16..20].try_into().unwrap());
    if version != 1 || format_id != 0 {
        return Err(Error::invalid("dsf: unsupported version or format_id"));
    }
    let channel_type = u32::from_le_bytes(fmt_hdr[20..24].try_into().unwrap());
    let channels_u32 = u32::from_le_bytes(fmt_hdr[24..28].try_into().unwrap());
    if channels_u32 == 0 || channels_u32 > MAX_DSD_CHANNELS {
        return Err(Error::invalid("dsf: invalid channel count"));
    }
    let channels = channels_u32 as u16;
    let dsd_rate = u32::from_le_bytes(fmt_hdr[28..32].try_into().unwrap());
    let sample_rate = dsd_rate / 8;
    if sample_rate == 0 {
        return Err(Error::invalid("dsf: zero sample rate"));
    }
    let codec_id = match u32::from_le_bytes(fmt_hdr[32..36].try_into().unwrap()) {
        1 => "dsd_lsbf_planar",
        8 => "dsd_msbf_planar",
        _ => return Err(Error::invalid("dsf: unknown bit-order field")),
    };
    let sample_count_bits = u64::from_le_bytes(fmt_hdr[36..44].try_into().unwrap());
    let audio_size = (sample_count_bits / 8).saturating_mul(u64::from(channels));
    let per_ch_align = u32::from_le_bytes(fmt_hdr[44..48].try_into().unwrap());
    if per_ch_align == 0 || per_ch_align > MAX_DSF_BLOCK_ALIGN_PER_CH {
        return Err(Error::invalid("dsf: invalid block_align"));
    }
    let block_align = u64::from(per_ch_align) * u64::from(channels);

    let data_hdr = read_exact_arr::<12>(&mut *input)?;
    if &data_hdr[0..4] != b"data" {
        return Err(Error::invalid("dsf: missing data chunk"));
    }
    let chunk_size = u64::from_le_bytes(data_hdr[4..12].try_into().unwrap());
    if chunk_size < 12 {
        return Err(Error::invalid("dsf: truncated data chunk"));
    }
    let data_size = chunk_size - 12;
    let data_offset = input.stream_position()?;
    let data_end = data_offset.saturating_add(data_size);

    let layout = dsf_layout(channel_type, channels);
    let mut params = CodecParameters::audio(CodecId::new(codec_id));
    params.sample_rate = Some(sample_rate);
    params.channels = Some(channels);
    params.channel_layout = Some(layout);
    params.sample_format = Some(SampleFormat::F32P);
    params.bit_rate = Some(u64::from(channels) * 8 * u64::from(sample_rate));

    let duration_samples = (audio_size / u64::from(channels)).min(i64::MAX as u64) as i64;
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, i64::from(sample_rate)),
        duration: (duration_samples > 0).then_some(duration_samples),
        start_time: Some(0),
    };

    Ok(Box::new(DsfDemuxer {
        input,
        stream,
        data_offset,
        data_end,
        data_size,
        audio_size,
        block_align,
        channels,
    }))
}

impl Demuxer for DsfDemuxer {
    fn format_name(&self) -> &str {
        "dsf"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let pos = self.input.stream_position()?;
        if pos >= self.data_end {
            return Err(Error::Eof);
        }
        let ch = u64::from(self.channels);
        let data_pos = pos.saturating_sub(self.data_offset);

        if self.data_size > self.audio_size && pos == self.data_end.saturating_sub(self.block_align) {
            let packet_size = self.audio_size.saturating_sub(data_pos);
            let skip_size = self.data_size.saturating_sub(data_pos).saturating_sub(packet_size);
            if packet_size == 0 || skip_size == 0 || packet_size % ch != 0 {
                return Err(Error::Eof);
            }
            let per_ch = (packet_size / ch) as usize;
            let skip_per_ch = (skip_size / ch) as i64;
            let mut payload = vec![0u8; packet_size as usize];
            for c in 0..usize::from(self.channels) {
                self.input
                    .read_exact(&mut payload[c * per_ch..(c + 1) * per_ch])
                    .map_err(|_| Error::Eof)?;
                let _ = self.input.seek(SeekFrom::Current(skip_per_ch));
            }
            let mut pkt = Packet::new(0, self.stream.time_base, payload);
            let pts = (data_pos / ch) as i64;
            pkt.pts = Some(pts);
            pkt.dts = Some(pts);
            pkt.duration = Some((packet_size / ch) as i64);
            pkt.flags.keyframe = true;
            return Ok(pkt);
        }

        let to_read = (self.data_end - pos).min(self.block_align) as usize;
        if to_read == 0 {
            return Err(Error::Eof);
        }
        let mut buf = vec![0u8; to_read];
        let n = self.input.read(&mut buf)?;
        if n == 0 {
            return Err(Error::Eof);
        }
        buf.truncate(n);
        let mut pkt = Packet::new(0, self.stream.time_base, buf);
        let pts = (data_pos / ch) as i64;
        pkt.pts = Some(pts);
        pkt.dts = Some(pts);
        pkt.duration = Some((self.block_align / ch) as i64);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let ch = u64::from(self.channels);
        let samples_per_block = (self.block_align / ch).max(1);
        let target = pts.max(0) as u64;
        let block_idx = target / samples_per_block;
        let max_blocks = (self.data_size / self.block_align).saturating_sub(1);
        let clamped_idx = block_idx.min(max_blocks);
        let byte_pos = self.data_offset + clamped_idx * self.block_align;
        self.input.seek(SeekFrom::Start(byte_pos))?;
        Ok((clamped_idx * samples_per_block) as i64)
    }
}

struct DffDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    body_pos: u64,
    body_end: u64,
    channels: u16,
    packet_bytes: usize,
}

pub fn open_dff(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let hdr = read_exact_arr::<16>(&mut *input)?;
    if &hdr[0..4] != b"FRM8" || &hdr[12..16] != b"DSD " {
        return Err(Error::invalid("dff: invalid FRM8 DSD header"));
    }
    let frm_size = u64::from_be_bytes(hdr[4..12].try_into().unwrap());
    let file_end = 12u64.saturating_add(frm_size);

    let mut sample_rate: Option<u32> = None;
    let mut channels: Option<u16> = None;
    let mut layout: Option<ChannelLayout> = None;
    let mut cmpr_dsd = false;
    let mut body_pos: Option<u64> = None;
    let mut body_size: u64 = 0;

    while let Ok(pos) = input.stream_position() {
        if pos.saturating_add(12) > file_end {
            break;
        }
        let Ok(ch_hdr) = read_exact_arr::<12>(&mut *input) else { break };
        let id = &ch_hdr[0..4];
        let size = u64::from_be_bytes(ch_hdr[4..12].try_into().unwrap());
        let chunk_start = pos + 12;
        let next = chunk_start.saturating_add(size).saturating_add(size & 1);

        if id == b"PROP" && size >= 4 {
            let prop_type = read_exact_arr::<4>(&mut *input)?;
            if &prop_type == b"SND " {
                let prop_end = chunk_start.saturating_add(size);
                while input.stream_position()? + 12 <= prop_end {
                    let sub = read_exact_arr::<12>(&mut *input)?;
                    let sub_id = &sub[0..4];
                    let sub_size = u64::from_be_bytes(sub[4..12].try_into().unwrap());
                    let sub_start = input.stream_position()?;
                    let sub_next = sub_start.saturating_add(sub_size).saturating_add(sub_size & 1);
                    match sub_id {
                        b"FS  " if sub_size >= 4 => {
                            let b = read_exact_arr::<4>(&mut *input)?;
                            sample_rate = Some(u32::from_be_bytes(b) / 8);
                        }
                        b"CHNL" if sub_size >= 2 => {
                            let b = read_exact_arr::<2>(&mut *input)?;
                            let ch = u16::from_be_bytes(b);
                            if ch == 0 || u32::from(ch) > MAX_DSD_CHANNELS {
                                return Err(Error::invalid("dff: invalid channel count"));
                            }
                            channels = Some(ch);
                            if sub_size >= 2 + u64::from(ch) * 4 {
                                let mut tags = Vec::with_capacity(usize::from(ch));
                                for _ in 0..ch {
                                    tags.push(read_exact_arr::<4>(&mut *input)?);
                                }
                                layout = Some(match tags.as_slice() {
                                    [t0, t1] if t0 == b"SLFT" && t1 == b"SRGT" => {
                                        ChannelLayout::Stereo
                                    }
                                    [t0, t1, t2, t3, t4, t5]
                                        if t0 == b"MLFT"
                                            && t1 == b"MRGT"
                                            && t2 == b"C   "
                                            && t3 == b"LFE "
                                            && t4 == b"LS  "
                                            && t5 == b"RS  " =>
                                    {
                                        ChannelLayout::Surround51
                                    }
                                    _ => ChannelLayout::from_count(ch),
                                });
                            }
                        }
                        b"CMPR" if sub_size >= 4 => {
                            let comp = read_exact_arr::<4>(&mut *input)?;
                            if &comp != b"DSD " {
                                return Err(Error::Unsupported("dff: compressed DST not supported".into()));
                            }
                            cmpr_dsd = true;
                        }
                        _ => {}
                    }
                    input.seek(SeekFrom::Start(sub_next.min(prop_end)))?;
                }
            }
        } else if id == b"DSD " {
            body_pos = Some(chunk_start);
            body_size = size;
            break;
        }
        input.seek(SeekFrom::Start(next))?;
    }

    let (Some(sr), Some(ch), Some(b_pos)) = (sample_rate, channels, body_pos) else {
        return Err(Error::invalid("dff: missing FS, CHNL, or DSD data chunk"));
    };
    if sr == 0 || !cmpr_dsd {
        return Err(Error::invalid("dff: invalid sample rate or missing DSD CMPR"));
    }
    input.seek(SeekFrom::Start(b_pos))?;

    let mut params = CodecParameters::audio(CodecId::new("dsd_msbf"));
    params.sample_rate = Some(sr);
    params.channels = Some(ch);
    params.channel_layout = Some(layout.unwrap_or_else(|| ChannelLayout::from_count(ch)));
    params.sample_format = Some(SampleFormat::F32P);
    params.bit_rate = Some(u64::from(ch) * 8 * u64::from(sr));

    let duration_samples = (body_size / u64::from(ch)).min(i64::MAX as u64) as i64;
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: TimeBase::new(1, i64::from(sr)),
        duration: (duration_samples > 0).then_some(duration_samples),
        start_time: Some(0),
    };

    Ok(Box::new(DffDemuxer {
        input,
        stream,
        body_pos: b_pos,
        body_end: b_pos.saturating_add(body_size),
        channels: ch,
        packet_bytes: 1024 * usize::from(ch),
    }))
}

impl Demuxer for DffDemuxer {
    fn format_name(&self) -> &str {
        "dff"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let pos = self.input.stream_position()?;
        if pos >= self.body_end {
            return Err(Error::Eof);
        }
        let ch = usize::from(self.channels);
        let rem = (self.body_end - pos) as usize;
        let want = (rem.min(self.packet_bytes) / ch) * ch;
        if want == 0 {
            return Err(Error::Eof);
        }
        let mut buf = vec![0u8; want];
        let n = self.input.read(&mut buf)?;
        let whole = (n / ch) * ch;
        if whole == 0 {
            return Err(Error::Eof);
        }
        buf.truncate(whole);
        let pts = ((pos - self.body_pos) / u64::from(self.channels)) as i64;
        let samples = (whole / ch) as i64;
        let mut pkt = Packet::new(0, self.stream.time_base, buf);
        pkt.pts = Some(pts);
        pkt.dts = Some(pts);
        pkt.duration = Some(samples);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let ch = u64::from(self.channels);
        let max_samples = (self.body_end - self.body_pos) / ch;
        let clamped = (pts.max(0) as u64).min(max_samples);
        let byte_pos = self.body_pos + clamped * ch;
        self.input.seek(SeekFrom::Start(byte_pos))?;
        Ok(clamped as i64)
    }
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("dsf", open_dsf);
    reg.register_probe("dsf", probe_dsf);
    reg.register_extension("dsf", "dsf");

    reg.register_demuxer("dff", open_dff);
    reg.register_probe("dff", probe_dff);
    reg.register_extension("dff", "dff");
}

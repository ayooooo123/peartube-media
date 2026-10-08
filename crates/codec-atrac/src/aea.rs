// Port of FFmpeg's AEA (MD STUDIO) demuxer (libavformat/aeadec.c, FFmpeg
// commit 2da55bf), with the timestamps FFmpeg's generic layer gives its
// packets.
// Copyright (c) 2009 Benjamin Larsson; LGPL-2.1-or-later (see LICENSE).

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MAX_PROBE_SCORE,
    Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
};

use crate::demux::{input_len, pcm_seek, read_up_to};

const AT1_SU_SIZE: usize = 212;
const DATA_OFFSET: u64 = 2048;
const SAMPLE_RATE: u32 = 44100;
/// ATRAC1 samples per channel and sound unit: each packet's duration.
const FRAME_SAMPLES: i64 = 512;

/// `aea_read_probe`.
fn probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    if buf.len() <= 2048 + AT1_SU_SIZE {
        return 0;
    }
    // magic 00 08 00 00
    if u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != 0x800 {
        return 0;
    }
    let ch = usize::from(buf[264]);
    if ch != 1 && ch != 2 {
        return 0;
    }
    // the redundant block size mode and info bytes must agree
    let block_size = ch * AT1_SU_SIZE;
    let mut score = 0u32;
    let mut i = 2048 + block_size;
    while i + block_size <= buf.len() {
        if buf[i..i + 2] != buf[i + AT1_SU_SIZE..i + AT1_SU_SIZE + 2] {
            return 0;
        }
        score += 1;
        i += block_size;
    }
    (u32::from(MAX_PROBE_SCORE) / 4 + score).min(u32::from(MAX_PROBE_SCORE)) as ProbeScore
}

struct AeaDemuxer {
    input: Box<dyn ReadSeek>,
    streams: [StreamInfo; 1],
    metadata: Vec<(String, String)>,
    block_align: usize,
    bit_rate: i64,
    next_pts: i64,
    duration_micros: Option<i64>,
}

impl Demuxer for AeaDemuxer {
    fn format_name(&self) -> &str {
        "aea"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// `aea_read_packet`: one block of every channel's sound unit; a short
    /// last read is flagged corrupt, as `av_get_packet` does.
    fn next_packet(&mut self) -> Result<Packet> {
        let data = read_up_to(&mut *self.input, self.block_align)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let corrupt = data.len() < self.block_align;
        let mut packet = Packet::new(0, self.streams[0].time_base, data).with_keyframe(true);
        packet.pts = Some(self.next_pts);
        packet.dts = Some(self.next_pts);
        packet.duration = Some(FRAME_SAMPLES);
        packet.flags.corrupt = corrupt;
        self.next_pts += FRAME_SAMPLES;
        Ok(packet)
    }

    /// `ff_pcm_read_seek`: the block at or before `pts`.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let landed = pcm_seek(
            &mut *self.input,
            DATA_OFFSET,
            pts,
            i64::from(SAMPLE_RATE),
            self.block_align as i64,
            self.bit_rate >> 3,
        )?;
        self.next_pts = landed;
        Ok(landed)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        self.duration_micros
    }
}

/// `aea_read_header`.
fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut head = [0u8; 4 + 256 + 4 + 1];
    input
        .read_exact(&mut head)
        .map_err(|_| Error::invalid("aea: short header"))?;
    let title = &head[4..260];
    let title = &title[..title.iter().position(|&b| b == 0).unwrap_or(title.len())];
    let channels = u16::from(head[264]);
    if !(1..=8).contains(&channels) {
        return Err(Error::invalid(format!(
            "aea: {channels} channels not supported"
        )));
    }
    input.seek(SeekFrom::Start(DATA_OFFSET))?;

    let block_align = AT1_SU_SIZE * usize::from(channels);
    let bit_rate = 146_000 * i64::from(channels);
    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_ATRAC1));
    params.sample_rate = Some(SAMPLE_RATE);
    params.channels = Some(channels);
    params.bit_rate = Some(bit_rate as u64);
    params
        .options
        .insert("block_align", block_align.to_string());

    let len = input_len(&mut *input)?;
    let duration_micros = Some((len.saturating_sub(DATA_OFFSET) as i64) * 8 * 1_000_000 / bit_rate);
    let metadata = if title.is_empty() {
        Vec::new()
    } else {
        vec![(
            "title".to_string(),
            String::from_utf8_lossy(title).into_owned(),
        )]
    };
    Ok(Box::new(AeaDemuxer {
        input,
        streams: [StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, i64::from(SAMPLE_RATE)),
            duration: None,
            start_time: None,
            params,
        }],
        metadata,
        block_align,
        bit_rate,
        next_pts: 0,
        duration_micros,
    }))
}

pub(crate) fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("aea", open);
    reg.register_probe("aea", probe);
    reg.register_extension("aea", "aea");
}

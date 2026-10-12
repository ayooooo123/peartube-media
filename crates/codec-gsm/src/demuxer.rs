// The raw `gsm` demuxer.
//
// Ported from FFmpeg (commit 2da55bf) libavformat/gsmdec.c.
// Copyright (c) 2011 Justin Ruggles; LGPL-2.1-or-later (see LICENSE).

//! A raw GSM file is 33-byte blocks of 160 samples at 8 kHz, nothing else.
//! Packet `n` is the block at byte `33 n`, with pts `n` in units of 1/50 s
//! (FFmpeg's `avpriv_set_pts_info(st, 64, 160, 8000)`). A last block shorter
//! than 33 bytes ends the stream.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::decoder::{GSM_BLOCK_SIZE, GSM_FRAME_SIZE};

const SAMPLE_RATE: u32 = 8000;

/// `gsm_probe`: blocks start with the 0xD magic nibble; more than 32 valid
/// per invalid one.
pub fn probe(p: &ProbeData) -> u8 {
    let buf = p.buf;
    let (mut valid, mut invalid) = (0u32, 0u32);
    let mut i = 0;
    while i + 32 < buf.len() {
        if buf[i] & 0xF0 == 0xD0 {
            valid += 1;
        } else {
            invalid += 1;
        }
        i += GSM_BLOCK_SIZE;
    }
    if valid >> 5 > invalid { PROBE_SCORE_EXTENSION + 1 } else { 0 }
}

/// `gsm_read_header`
pub fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut params = CodecParameters::audio(CodecId::new("gsm"));
    params.channels = Some(1);
    params.sample_rate = Some(SAMPLE_RATE);
    params.bit_rate = Some((GSM_BLOCK_SIZE * 8) as u64 * u64::from(SAMPLE_RATE) / GSM_FRAME_SIZE as u64);
    let len = input.seek(SeekFrom::End(0))?;
    input.seek(SeekFrom::Start(0))?;
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(GSM_FRAME_SIZE as i64, i64::from(SAMPLE_RATE)),
        duration: Some((len / GSM_BLOCK_SIZE as u64) as i64),
        start_time: Some(0),
        params,
    };
    Ok(Box::new(GsmDemuxer { input, stream, blocks: len / GSM_BLOCK_SIZE as u64 }))
}

struct GsmDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
    /// Whole blocks in the file.
    blocks: u64,
}

impl Demuxer for GsmDemuxer {
    fn format_name(&self) -> &str {
        "gsm"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    /// `gsm_read_packet`
    fn next_packet(&mut self) -> Result<Packet> {
        let pos = self.input.stream_position()?;
        let mut data = Vec::with_capacity(GSM_BLOCK_SIZE);
        (&mut self.input).take(GSM_BLOCK_SIZE as u64).read_to_end(&mut data)?;
        if data.len() < GSM_BLOCK_SIZE {
            return Err(Error::Eof);
        }
        let pts = (pos / GSM_BLOCK_SIZE as u64) as i64;
        let mut packet = Packet::new(0, self.stream.time_base, data);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = Some(1);
        packet.flags.keyframe = true;
        Ok(packet)
    }

    /// The block holding `pts` (every block starts a frame), the last one
    /// past the end, as FFmpeg's generic index finds it.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let block = (pts.max(0) as u64).min(self.blocks.saturating_sub(1));
        self.input.seek(SeekFrom::Start(block * GSM_BLOCK_SIZE as u64))?;
        Ok(block as i64)
    }
}

/// Registers the `gsm` demuxer with its `.gsm` extension and probe.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("gsm", open);
    reg.register_extension("gsm", "gsm");
    reg.register_probe("gsm", probe);
}

// The raw `shn` demuxer.
//
// Ported from FFmpeg (commit 2da55bf) libavformat/shortendec.c (the probe)
// and libavformat/rawdec.c (ff_raw_read_partial_packet: 1024-byte packets).
// Copyright (c) 2001 Fabrice Bellard, (c) 2005 Alex Beregszaszi;
// LGPL-2.1-or-later (see LICENSE).

//! A `.shn` file is one Shorten bitstream. Packets are its bytes in 1024-byte
//! pieces with no timestamps; the decoder finds the blocks in them and
//! stamps each in samples. The stream's channels, rate and sample format
//! come from its header, which FFmpeg learns by decoding the start of the
//! stream (here the demuxer reads it when it opens). FFmpeg cannot seek
//! these files; here a seek goes back to the start.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet, ProbeData, ReadSeek, Result,
    StreamInfo, TimeBase, PROBE_SCORE_EXTENSION,
};

use crate::bits::BitReader;
use crate::decoder::parse_stream_header;

/// `RAW_PACKET_SIZE`
const PACKET_SIZE: u64 = 1024;
/// `AV_INPUT_BUFFER_PADDING_SIZE`, which shn_probe leaves out of its reader.
const PADDING: usize = 64;
/// Enough of the stream for its header (at most 16 KiB of WAVE/AIFF header
/// in 9-bit or longer codes).
const HEADER_PROBE: u64 = 64 * 1024;

/// `shn_probe`
pub fn probe(p: &ProbeData) -> u8 {
    let b = p.buf;
    if b.len() < 5 || b[0..4] != *b"ajkg" {
        return 0;
    }
    let Some(size) = b.len().checked_sub(5 + PADDING) else { return 0 };
    let mut gb = BitReader::new(&b[5..], size);
    let version = b[4];
    let (internal_ftype, channels, blocksize) = if version == 0 {
        (gb.ur(4) as i32, gb.ur(0) as i32, 256)
    } else {
        let field = |gb: &mut BitReader| {
            let k = gb.ur(2);
            if k > 31 { None } else { Some(gb.ur(k) as i32) }
        };
        let (Some(t), Some(c), Some(s)) = (field(&mut gb), field(&mut gb), field(&mut gb)) else { return 0 };
        (t, c, s)
    };
    if !matches!(internal_ftype, 2 | 3 | 5) || !(1..=8).contains(&channels) || !(1..=65535).contains(&blocksize) {
        return 0;
    }
    PROBE_SCORE_EXTENSION + 1
}

/// `ff_raw_audio_read_header`, with the stream's layout read from its
/// header.
pub fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut head = Vec::new();
    (&mut input).take(HEADER_PROBE).read_to_end(&mut head)?;
    input.seek(SeekFrom::Start(0))?;
    let header = parse_stream_header(&head)?;
    let mut params = CodecParameters::audio(CodecId::new("shorten"));
    params.channels = Some(header.channels);
    params.sample_rate = Some(header.sample_rate);
    params.sample_format = Some(header.sample_format);
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, i64::from(header.sample_rate.max(1))),
        duration: None,
        start_time: Some(0),
        params,
    };
    Ok(Box::new(ShnDemuxer { input, stream }))
}

struct ShnDemuxer {
    input: Box<dyn ReadSeek>,
    stream: StreamInfo,
}

impl Demuxer for ShnDemuxer {
    fn format_name(&self) -> &str {
        "shn"
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    /// `ff_raw_read_partial_packet`: up to 1024 bytes.
    fn next_packet(&mut self) -> Result<Packet> {
        let mut data = Vec::with_capacity(PACKET_SIZE as usize);
        (&mut self.input).take(PACKET_SIZE).read_to_end(&mut data)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let mut packet = Packet::new(0, self.stream.time_base, data);
        packet.flags.keyframe = true;
        Ok(packet)
    }

    /// Back to the first byte: the decoder (reset) reads the stream from its
    /// header again, and the player skips what lies before `pts`.
    fn seek_to(&mut self, _stream_index: u32, _pts: i64) -> Result<i64> {
        self.input.seek(SeekFrom::Start(0))?;
        Ok(0)
    }
}

/// Registers the `shn` demuxer with its `.shn` extension and probe.
pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("shn", open);
    reg.register_extension("shn", "shn");
    reg.register_probe("shn", probe);
}

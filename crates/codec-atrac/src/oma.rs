// Port of FFmpeg's OMA (Sony OpenMG) demuxer (libavformat/omadec.c, oma.c,
// oma.h, FFmpeg commit 2da55bf), with the timestamps FFmpeg's generic layer
// gives its packets. Not ported: decryption of encrypted (MagicGate) files,
// and AAC / MP3 payloads, which FFmpeg cuts with its parsers.
// Copyright (c) 2008 Maxim Poliakovski, (c) 2008 Benjamin Larsson,
// (c) 2011 David Goldwich; LGPL-2.1-or-later (see LICENSE).

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, MAX_PROBE_SCORE,
    PROBE_SCORE_EXTENSION, Packet, ProbeData, ProbeScore, ReadSeek, Result, StreamInfo, TimeBase,
};

use crate::demux::{pcm_seek, read_up_to, rescale};

const EA3_HEADER_SIZE: usize = 96;
const ID3V2_HEADER_SIZE: usize = 10;

const OMA_CODECID_ATRAC3: u8 = 0;
const OMA_CODECID_ATRAC3P: u8 = 1;
const OMA_CODECID_AAC: u8 = 2;
const OMA_CODECID_MP3: u8 = 3;
const OMA_CODECID_LPCM: u8 = 4;
const OMA_CODECID_ATRAC3PAL: u8 = 33;
const OMA_CODECID_ATRAC3AL: u8 = 34;

/// `ff_oma_srate_tab`, in units of 100 Hz.
const SRATE_TAB: [u32; 8] = [320, 441, 480, 882, 960, 0, 0, 0];

/// Channel counts of `oma_chid_to_native_layout` (mono, stereo, 3.0,
/// 4.0, 5.1 back, 6.1 back, 7.1).
const CHID_CHANNELS: [u16; 7] = [1, 2, 3, 4, 6, 7, 8];

/// `ff_id3v2_match(buf, "ea3")`.
fn id3v2_ea3_match(buf: &[u8]) -> bool {
    buf.len() >= ID3V2_HEADER_SIZE
        && &buf[..3] == b"ea3"
        && buf[3] != 0xff
        && buf[4] != 0xff
        && buf[6..10].iter().all(|b| b & 0x80 == 0)
}

/// The tag's syncsafe body size.
fn id3v2_size(buf: &[u8]) -> usize {
    buf[6..10]
        .iter()
        .fold(0usize, |len, &b| (len << 7) | usize::from(b & 0x7f))
}

/// `oma_read_probe`.
fn probe(p: &ProbeData) -> ProbeScore {
    let buf = p.buf;
    let mut tag_len = 0usize;
    if id3v2_ea3_match(buf) {
        // ff_id3v2_tag_len: the footer flag adds 10 bytes
        tag_len = id3v2_size(buf)
            + ID3V2_HEADER_SIZE
            + if buf[5] & 0x10 != 0 {
                ID3V2_HEADER_SIZE
            } else {
                0
            };
    }
    if buf.len() < tag_len + 5 {
        // the EA3 header comes late, beyond the probe buffer
        return if tag_len != 0 {
            PROBE_SCORE_EXTENSION / 2
        } else {
            0
        };
    }
    // byte 5 may lie in FFmpeg's zeroed probe padding
    let b = &buf[tag_len..];
    if &b[..3] == b"EA3" && b[4] == 0 && b.get(5).copied() == Some(EA3_HEADER_SIZE as u8) {
        MAX_PROBE_SCORE
    } else {
        0
    }
}

/// The payloads this port reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Payload {
    Atrac3,
    Atrac3p,
    Lpcm,
    /// ATRAC3 AL / ATRAC3+ AL: `BLK` blocks with their own timestamps
    /// (`aal_read_packet`).
    Advanced {
        samples: i64,
    },
}

struct OmaDemuxer {
    input: Box<dyn ReadSeek>,
    streams: [StreamInfo; 1],
    payload: Payload,
    content_start: u64,
    block_align: usize,
    byte_rate: i64,
    sample_rate: i64,
}

impl OmaDemuxer {
    /// FFmpeg's `get_audio_frame_duration` for the payload.
    fn duration(&self, size: usize) -> i64 {
        match self.payload {
            Payload::Atrac3 => 1024 * (size / self.block_align).max(1) as i64,
            Payload::Atrac3p => 2048,
            // pcm_s16be stereo
            Payload::Lpcm => (size * 8 / (16 * 2)) as i64,
            Payload::Advanced { samples } => samples,
        }
    }

    /// `aal_read_packet`.
    fn read_advanced(&mut self, samples: i64) -> Result<Packet> {
        let mut head = [0u8; 3];
        match self.input.read_exact(&mut head) {
            Ok(()) => {}
            Err(_) => return Err(Error::Eof),
        }
        if head == [0, 0, 0] {
            return Err(Error::Eof);
        }
        if &head != b"BLK" {
            return Err(Error::invalid("oma: missing BLK block header"));
        }
        let mut rest = [0u8; 1 + 2 + 2 + 4 + 12];
        self.input.read_exact(&mut rest).map_err(|_| Error::Eof)?;
        let packet_size = usize::from(u16::from_be_bytes([rest[1], rest[2]]));
        let pts = i64::from(u32::from_be_bytes([rest[5], rest[6], rest[7], rest[8]]) as i32);
        let data = read_up_to(&mut *self.input, packet_size)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let corrupt = data.len() < packet_size;
        let mut packet = Packet::new(0, self.streams[0].time_base, data).with_keyframe(true);
        packet.pts = Some(pts * samples);
        packet.dts = packet.pts;
        packet.duration = Some(samples);
        packet.flags.corrupt = corrupt;
        Ok(packet)
    }
}

impl Demuxer for OmaDemuxer {
    fn format_name(&self) -> &str {
        "oma"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    /// `read_packet`: one `block_align` frame, timed by its byte position.
    fn next_packet(&mut self) -> Result<Packet> {
        if let Payload::Advanced { samples } = self.payload {
            return self.read_advanced(samples);
        }
        let pos = self.input.stream_position()?;
        let data = read_up_to(&mut *self.input, self.block_align)?;
        if data.is_empty() {
            return Err(Error::Eof);
        }
        let corrupt = data.len() < self.block_align;
        let duration = self.duration(data.len());
        let mut packet = Packet::new(0, self.streams[0].time_base, data).with_keyframe(true);
        if pos >= self.content_start && self.byte_rate > 0 {
            let pts = rescale(
                (pos - self.content_start) as i64,
                self.sample_rate,
                self.byte_rate,
            );
            packet.pts = Some(pts);
            packet.dts = Some(pts);
        }
        packet.duration = Some(duration);
        packet.flags.corrupt = corrupt;
        Ok(packet)
    }

    /// `oma_read_seek`: `ff_pcm_read_seek`, not for the AL payloads.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        if matches!(self.payload, Payload::Advanced { .. }) || self.byte_rate <= 0 {
            return Err(Error::unsupported(
                "oma: seeking this payload is not supported",
            ));
        }
        pcm_seek(
            &mut *self.input,
            self.content_start,
            pts,
            self.sample_rate,
            self.block_align as i64,
            self.byte_rate,
        )
    }
}

/// `ff_id3v2_read` with the `ea3` magic, metadata ignored: skips every
/// consecutive tag.
fn skip_ea3_tags(input: &mut dyn ReadSeek) -> Result<()> {
    loop {
        let start = input.stream_position()?;
        let mut head = [0u8; ID3V2_HEADER_SIZE];
        if input.read_exact(&mut head).is_err() || !id3v2_ea3_match(&head) {
            input.seek(SeekFrom::Start(start))?;
            return Ok(());
        }
        // id3v2_parse skips a footer only in version 4
        let footer = if head[3] == 4 && head[5] & 0x10 != 0 {
            ID3V2_HEADER_SIZE
        } else {
            0
        };
        let end = start + (ID3V2_HEADER_SIZE + id3v2_size(&head) + footer) as u64;
        input.seek(SeekFrom::Start(end))?;
    }
}

/// `oma_read_header`.
fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    skip_ea3_tags(&mut *input)?;
    let mut buf = [0u8; EA3_HEADER_SIZE];
    input
        .read_exact(&mut buf)
        .map_err(|_| Error::invalid("oma: short EA3 header"))?;
    if &buf[..3] != b"EA3" || buf[4] != 0 || usize::from(buf[5]) != EA3_HEADER_SIZE {
        return Err(Error::invalid("oma: couldn't find the EA3 header"));
    }
    let content_start = input.stream_position()?;

    let eid = i16::from_be_bytes([buf[6], buf[7]]);
    if eid != -1 && eid != -128 {
        return Err(Error::unsupported("oma: encrypted files are not supported"));
    }

    let codec_params = u32::from_be_bytes([0, buf[33], buf[34], buf[35]]);
    let srate = || SRATE_TAB[((codec_params >> 13) & 7) as usize] * 100;
    let (codec, payload, sample_rate, channels, framesize, bit_rate, extradata) = match buf[32] {
        OMA_CODECID_ATRAC3 => {
            let sample_rate = srate();
            if sample_rate == 0 {
                return Err(Error::invalid("oma: unsupported sample rate"));
            }
            let framesize = (codec_params & 0x3FF) as usize * 8;
            // stereo coding mode, 1 for joint stereo
            let jsflag = ((codec_params >> 17) & 1) as u16;
            let bit_rate = i64::from(sample_rate) * framesize as i64 / (1024 / 8);
            // the ATRAC3 extradata of a WAV file
            let mut ed = Vec::with_capacity(14);
            ed.extend_from_slice(&1u16.to_le_bytes());
            ed.extend_from_slice(&sample_rate.to_le_bytes());
            ed.extend_from_slice(&jsflag.to_le_bytes());
            ed.extend_from_slice(&jsflag.to_le_bytes());
            ed.extend_from_slice(&1u16.to_le_bytes());
            ed.extend_from_slice(&0u16.to_le_bytes());
            (
                crate::CODEC_ID_ATRAC3,
                Payload::Atrac3,
                sample_rate,
                2,
                framesize,
                bit_rate,
                ed,
            )
        }
        OMA_CODECID_ATRAC3P => {
            let channel_id = ((codec_params >> 10) & 7) as usize;
            if channel_id == 0 {
                return Err(Error::invalid("oma: invalid ATRAC-X channel id"));
            }
            let framesize = (codec_params & 0x3FF) as usize * 8 + 8;
            let sample_rate = srate();
            if sample_rate == 0 {
                return Err(Error::invalid("oma: unsupported sample rate"));
            }
            let bit_rate = i64::from(sample_rate) * framesize as i64 / (2048 / 8);
            (
                crate::CODEC_ID_ATRAC3P,
                Payload::Atrac3p,
                sample_rate,
                CHID_CHANNELS[channel_id - 1],
                framesize,
                bit_rate,
                Vec::new(),
            )
        }
        OMA_CODECID_LPCM => {
            // PCM 44.1 kHz 16 bit stereo big-endian
            (
                "pcm_s16be",
                Payload::Lpcm,
                44100,
                2,
                1024,
                44100 * 32,
                Vec::new(),
            )
        }
        OMA_CODECID_ATRAC3AL => (
            crate::CODEC_ID_ATRAC3AL,
            Payload::Advanced { samples: 1024 },
            44100,
            2,
            4096,
            0,
            Vec::new(),
        ),
        OMA_CODECID_ATRAC3PAL => (
            crate::CODEC_ID_ATRAC3PAL,
            Payload::Advanced { samples: 2048 },
            44100,
            2,
            4096,
            0,
            Vec::new(),
        ),
        OMA_CODECID_AAC | OMA_CODECID_MP3 => {
            return Err(Error::unsupported(
                "oma: AAC and MP3 payloads need FFmpeg's parsers, not ported",
            ));
        }
        other => {
            return Err(Error::unsupported(format!(
                "oma: unsupported codec {other}"
            )));
        }
    };

    let mut params = CodecParameters::audio(CodecId::new(codec));
    params.sample_rate = Some(sample_rate);
    params.channels = Some(channels);
    params.extradata = extradata;
    if bit_rate > 0 {
        params.bit_rate = Some(bit_rate as u64);
    }
    params.options.insert("block_align", framesize.to_string());
    if payload == Payload::Lpcm {
        params.sample_format = Some(oxideav_core::SampleFormat::S16);
    }

    Ok(Box::new(OmaDemuxer {
        input,
        streams: [StreamInfo {
            index: 0,
            time_base: TimeBase::new(1, i64::from(sample_rate)),
            duration: None,
            start_time: Some(0),
            params,
        }],
        payload,
        content_start,
        block_align: framesize,
        byte_rate: bit_rate >> 3,
        sample_rate: i64::from(sample_rate),
    }))
}

pub(crate) fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("oma", open);
    reg.register_probe("oma", probe);
    for ext in ["oma", "omg", "aa3"] {
        reg.register_extension(ext, "oma");
    }
}

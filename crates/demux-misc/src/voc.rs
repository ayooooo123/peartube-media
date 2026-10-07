// Ported from FFmpeg libavformat/vocdec.c, voc_packet.c and voc.c
// (commit 2da55bf).
// License: LGPL-2.1-or-later
//
// Creative Voice File demuxer: 26-byte header ("Creative Voice File\x1A",
// header size, version, checksum), then a stream of blocks. One packet per
// block (Voice data / Extended / New voice data); metadata blocks are
// skipped. Timestamps are in samples (1/sample_rate).

use std::io::{Read, Seek, SeekFrom};
use oxideav_core::{
    CodecId, CodecParameters, CodecResolver, ContainerRegistry, Demuxer, Error, Packet,
    ProbeData, ProbeScore, ReadSeek, Result, SampleFormat, StreamInfo, TimeBase,
    MAX_PROBE_SCORE,
};

const VOC_TYPE_EOF: u8 = 0x00;
const VOC_TYPE_VOICE_DATA: u8 = 0x01;
const VOC_TYPE_VOICE_DATA_CONT: u8 = 0x02;
const VOC_TYPE_EXTENDED: u8 = 0x08;
const VOC_TYPE_NEW_VOICE_DATA: u8 = 0x09;

/// Untrusted-input cap on one block (FFmpeg reads file-size blocks when
/// remaining_size == 0; cap that instead).
const MAX_BLOCK_SIZE: usize = 64 * 1024 * 1024;

/// ff_voc_codec_tags (voc.c)
fn voc_codec_tag(tag: u16) -> Option<&'static str> {
    Some(match tag {
        0x00 => "pcm_u8",
        0x01 => "adpcm_sbpro_4",
        0x02 => "adpcm_sbpro_3",
        0x03 => "adpcm_sbpro_2",
        0x04 => "pcm_s16le",
        0x06 => "pcm_alaw",
        0x07 => "pcm_mulaw",
        0x0200 => "adpcm_ct",
        _ => return None,
    })
}

/// Bits per sample for codecs without a fixed coded size (FFmpeg's
/// av_get_bits_per_sample).
fn bits_per_coded_sample(codec: &str) -> u16 {
    match codec {
        "pcm_u8" | "pcm_s8" => 8,
        "pcm_s16le" | "pcm_s16be" => 16,
        "pcm_s24le" | "pcm_s24be" => 24,
        "pcm_s32le" | "pcm_s32be" | "pcm_f32le" | "pcm_f32be" => 32,
        "pcm_f64le" | "pcm_f64be" => 64,
        "pcm_alaw" | "pcm_mulaw" => 8,
        _ => 0,
    }
}

pub fn probe_voc(probe: &ProbeData) -> ProbeScore {
    let p = probe.buf;
    if p.len() < 26 {
        return 0;
    }
    if !p.starts_with(b"Creative Voice File\x1A") {
        return 0;
    }
    let version = u16::from_le_bytes([p[22], p[23]]);
    let check = u16::from_le_bytes([p[24], p[25]]);
    if (!version).wrapping_add(0x1234) != check {
        // FFmpeg returns 10 for a bad checksum.
        return 10;
    }
    MAX_PROBE_SCORE
}

struct VocDemuxer {
    input: Box<dyn ReadSeek>,
    stream: Option<StreamInfo>,
    /// The first block, read at open to create the stream.
    first: Option<Packet>,
    remaining_size: i64,
    pts: i64,
    /// None until the codec is known (FFmpeg's AV_CODEC_ID_NONE).
    codec: Option<&'static str>,
    sample_rate: u32,
    channels: u16,
    sample_format: Option<SampleFormat>,
}

/// av_get_audio_frame_duration2 (libavcodec/utils.c): only codecs with
/// an exact coded sample width have a duration here. SBPro packets have
/// predictor bytes and no duration rule; after the first packet their
/// timestamps are unknown, not compressed-size-derived sample counts.
fn audio_duration(codec: &str, channels: u16, size: usize) -> Option<i64> {
    let bits = match codec {
        "adpcm_ct" => 4,
        "pcm_alaw" | "pcm_mulaw" | "pcm_u8" | "pcm_s8" => 8,
        "pcm_s16le" => 16,
        _ => return None,
    };
    let samples = (size * 8).checked_div(bits * usize::from(channels))?;
    (samples > 0).then_some(samples as i64)
}

impl VocDemuxer {
    /// ff_voc_get_packet: read one block; returns Ok(None) at EOF.
    #[allow(clippy::type_complexity)]
    fn next_block(&mut self) -> Result<Option<Packet>> {
        let mut max_size = 0usize;
        let mut tmp_codec: Option<u16> = None;
        let mut sample_rate_hint = 0u32;
        let mut channels_hint = 1u16;
        let mut sample_format_hint = SampleFormat::U8;

        while self.remaining_size == 0 {
            if max_size < 4 {
                max_size = 0;
            }
            let mut b = [0u8; 1];
            if self.input.read_exact(&mut b).is_err() {
                return Ok(None);
            }
            let block_type = b[0];
            if block_type == VOC_TYPE_EOF {
                return Ok(None);
            }
            let mut len3 = [0u8; 3];
            self.input.read_exact(&mut len3)?;
            let mut remaining = i64::from(len3[0]) | (i64::from(len3[1]) << 8) | (i64::from(len3[2]) << 16);
            if remaining == 0 {
                // FFmpeg reads to EOF; we cap that at MAX_BLOCK_SIZE.
                let pos = self.input.stream_position()?;
                let end = self.input.seek(SeekFrom::End(0))?;
                self.input.seek(SeekFrom::Start(pos))?;
                remaining = i64::try_from(end.saturating_sub(pos))
                    .unwrap_or(MAX_BLOCK_SIZE as i64)
                    .min(MAX_BLOCK_SIZE as i64);
                if remaining <= 0 {
                    return Ok(None);
                }
            }
            max_size = max_size.saturating_sub(4);

            match block_type {
                VOC_TYPE_VOICE_DATA => {
                    if remaining < 2 {
                        self.remaining_size = 0;
                        return Err(Error::invalid("voc: voice block too small"));
                    }
                    let mut fb = [0u8; 1];
                    self.input.read_exact(&mut fb)?;
                    if self.sample_rate == 0 {
                        let div = 256u32.wrapping_sub(u32::from(fb[0]));
                        if div == 0 {
                            self.remaining_size = 0;
                            return Err(Error::invalid("voc: invalid sample rate divisor"));
                        }
                        self.sample_rate = 1_000_000 / div;
                        if sample_rate_hint != 0 {
                            self.sample_rate = sample_rate_hint;
                        }
                        self.channels = channels_hint;
                        self.sample_format = Some(sample_format_hint);
                    }
                    let mut cb = [0u8; 1];
                    self.input.read_exact(&mut cb)?;
                    tmp_codec = Some(u16::from(cb[0]));
                    self.remaining_size = remaining - 2;
                    max_size = max_size.saturating_sub(2);
                    channels_hint = 1;
                    sample_format_hint = SampleFormat::U8;
                }
                VOC_TYPE_VOICE_DATA_CONT => {
                    self.remaining_size = remaining;
                }
                VOC_TYPE_EXTENDED => {
                    if remaining < 4 {
                        return Err(Error::invalid("voc: extended block too small"));
                    }
                    let mut eb = [0u8; 4];
                    self.input.read_exact(&mut eb)?;
                    let sr = u16::from_le_bytes([eb[0], eb[1]]);
                    channels_hint = u16::from(eb[3]) + 1;
                    let denom = u32::from(channels_hint)
                        .checked_mul(65536u32.wrapping_sub(u32::from(sr)))
                        .filter(|d| *d != 0);
                    sample_rate_hint = match denom {
                        Some(d) => 256_000_000 / d,
                        None => {
                            self.remaining_size = 0;
                            return Err(Error::invalid("voc: invalid extended sample rate"));
                        }
                    };
                    sample_format_hint = SampleFormat::U8;
                    self.remaining_size = 0;
                    max_size = max_size.saturating_sub(4);
                }
                VOC_TYPE_NEW_VOICE_DATA => {
                    if remaining < 12 {
                        self.remaining_size = 0;
                        return Err(Error::invalid("voc: new voice block too small"));
                    }
                    let mut nb = [0u8; 12];
                    self.input.read_exact(&mut nb)?;
                    if self.sample_rate == 0 {
                        self.sample_rate = u32::from_le_bytes([nb[0], nb[1], nb[2], nb[3]]);
                        let bits = u16::from(nb[4]);
                        self.channels = u16::from(nb[5]);
                        self.sample_format = Some(match bits {
                            8 => SampleFormat::U8,
                            16 => SampleFormat::S16,
                            _ => SampleFormat::U8,
                        });
                    }
                    tmp_codec = Some(u16::from_le_bytes([nb[6], nb[7]]));
                    self.remaining_size = remaining - 12;
                    max_size = max_size.saturating_sub(12);
                }
                _ => {
                    // VOC_TYPE_SILENCE / MARKER / ASCII / REPETITION_*:
                    // skip the payload, like FFmpeg's default branch.
                    let skip = usize::try_from(remaining).unwrap_or(usize::MAX).min(MAX_BLOCK_SIZE);
                    std::io::copy(
                        &mut self.input.by_ref().take(skip as u64),
                        &mut std::io::sink(),
                    )?;
                    self.remaining_size = 0;
                    max_size = max_size.saturating_sub(skip);
                }
            }
        }

        if self.sample_rate == 0 {
            return Err(Error::invalid("voc: invalid sample rate"));
        }

        if let Some(tag) = tmp_codec {
            let resolved = voc_codec_tag(tag);
            if self.codec.is_none() {
                self.codec = resolved;
            } else if self.codec != resolved {
                // FFmpeg warns and ignores a mid-stream codec change.
            }
        }
        let codec = self
            .codec
            .ok_or_else(|| Error::codec_not_found("voc: unknown codec tag"))?;

        // Make sure the stream exists and carries the resolved parameters.
        self.ensure_stream(codec)?;

        let size = self.remaining_size.clamp(0, 2048) as usize;
        self.remaining_size -= size as i64;

        let mut data = vec![0u8; size];
        self.input.read_exact(&mut data)?;

        let duration = audio_duration(codec, self.channels, size);
        let pts = (self.pts >= 0).then_some(self.pts);
        match duration {
            Some(d) if self.pts >= 0 => self.pts += d,
            _ => self.pts = -1,
        }

        let stream = self.stream.as_ref().expect("stream created");
        let mut pkt = Packet {
            stream_index: 0,
            time_base: stream.time_base,
            pts,
            dts: pts,
            duration: None,
            flags: Default::default(),
            data,
        };
        pkt.flags.keyframe = true;
        Ok(Some(pkt))
    }

    fn ensure_stream(&mut self, codec: &'static str) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }
        let mut params = CodecParameters::audio(CodecId::new(codec));
        params.sample_rate = Some(self.sample_rate);
        params.channels = Some(self.channels);
        params.sample_format = self.sample_format;
        params.bit_rate = Some(u64::from(self.sample_rate)
            * u64::from(self.channels.max(1))
            * u64::from(bits_per_coded_sample(codec)));
        self.stream = Some(StreamInfo {
            index: 0,
            params,
            time_base: TimeBase::from_rate(self.sample_rate),
            duration: None,
            start_time: Some(0),
        });
        Ok(())
    }
}

impl Demuxer for VocDemuxer {
    fn format_name(&self) -> &str {
        "voc"
    }

    fn streams(&self) -> &[StreamInfo] {
        match &self.stream {
            Some(s) => std::slice::from_ref(s),
            None => &[],
        }
    }

    fn next_packet(&mut self) -> Result<Packet> {
        if let Some(pkt) = self.first.take() {
            return Ok(pkt);
        }
        match self.next_block()? {
            Some(pkt) => Ok(pkt),
            None => Err(Error::Eof),
        }
    }
}

pub fn open_voc(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut head = [0u8; 26];
    input.read_exact(&mut head)?;
    if !head.starts_with(b"Creative Voice File\x1A") {
        return Err(Error::invalid("voc: bad magic"));
    }
    let header_size = u16::from_le_bytes([head[20], head[21]]);
    let skip = i64::from(header_size) - 22;
    if skip != 4 {
        return Err(Error::unsupported(format!(
            "voc: unknown header size {skip}"
        )));
    }
    // The 26-byte read above already consumed magic (20) + header size (2)
    // + version (2) + checksum (2) = FFmpeg's skip(20)+rl16(2)+skip(4).
    // The block area starts right here — but a header_size > 26 means
    // extra extension bytes we must still skip.
    if header_size > 26 {
        input.seek(SeekFrom::Current(i64::from(header_size) - 26))?;
    }

    // FFmpeg creates the stream in the first read_packet (vocdec.c,
    // AVFMTCTX_NOHEADER) and avformat_find_stream_info reads that packet
    // before anyone sees the stream; read the first block here so the
    // stream and its parameters exist from open on.
    let mut demuxer = VocDemuxer {
        input,
        stream: None,
        first: None,
        remaining_size: 0,
        pts: 0,
        codec: None,
        sample_rate: 0,
        channels: 1,
        sample_format: None,
    };
    demuxer.first = demuxer.next_block()?;
    if demuxer.first.is_none() {
        return Err(Error::invalid("voc: no audio data"));
    }
    Ok(Box::new(demuxer))
}

pub fn register(reg: &mut ContainerRegistry) {
    reg.register_demuxer("voc", open_voc);
    reg.register_probe("voc", probe_voc);
    reg.register_extension("voc", "voc");
}

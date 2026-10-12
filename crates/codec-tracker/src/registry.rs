//! Demuxer and decoder for the pipeline. A module is one stream whose only
//! packet is the whole file; the packet's pts is where rendering starts, so
//! a seek re-sends the file stamped with the start of the row it lands in.
//!
//! Copyright (c) 2004-2026, OpenMPT Project Developers and Contributors;
//! Copyright (c) 1997-2003, Olivier Lapicque. BSD-3-Clause (see LICENSE).

use std::io::{Read, SeekFrom};

use oxideav_core::{
    AudioFormat, AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecResolver,
    ContainerRegistry, Decoder, Demuxer, Error, Frame, MAX_PROBE_SCORE, Packet, ProbeData, ReadSeek, Result,
    SampleFormat, StreamInfo, TimeBase,
};

use crate::render::{READ_FRAMES, Renderer, SAMPLE_RATE, Song};
use crate::Format;

/// Largest module file accepted. The biggest published modules are a few
/// tens of megabytes.
pub const MAX_FILE_BYTES: u64 = 128 << 20;

const FORMATS: [Format; 8] =
    [Format::Mod, Format::S3m, Format::Xm, Format::It, Format::Mtm, Format::Six69, Format::Ult, Format::Stm];

fn time_base() -> TimeBase {
    TimeBase::new(1, SAMPLE_RATE as i64)
}

fn score(p: &ProbeData, format: Format) -> u8 {
    if crate::probe(p.buf) == Some(format) { MAX_PROBE_SCORE } else { 0 }
}

fn probe_mod(p: &ProbeData) -> u8 {
    score(p, Format::Mod)
}
fn probe_s3m(p: &ProbeData) -> u8 {
    score(p, Format::S3m)
}
fn probe_xm(p: &ProbeData) -> u8 {
    score(p, Format::Xm)
}
fn probe_it(p: &ProbeData) -> u8 {
    score(p, Format::It)
}
fn probe_mtm(p: &ProbeData) -> u8 {
    score(p, Format::Mtm)
}
fn probe_669(p: &ProbeData) -> u8 {
    score(p, Format::Six69)
}
fn probe_ult(p: &ProbeData) -> u8 {
    score(p, Format::Ult)
}
fn probe_stm(p: &ProbeData) -> u8 {
    score(p, Format::Stm)
}

struct TrackerDemuxer {
    format: Format,
    data: Vec<u8>,
    song: Song,
    stream: StreamInfo,
    metadata: Vec<(String, String)>,
    /// Start of the next packet; `None` once it was sent.
    next_start: Option<u64>,
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    input.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    (&mut input).take(MAX_FILE_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::invalid("tracker: module larger than 128 MiB"));
    }
    let song = Song::load(&data).ok_or_else(|| Error::invalid("tracker: not a MOD, S3M, XM, IT, MTM, 669, ULT or STM module"))?;
    let format = song.format();
    let mut params = CodecParameters::audio(CodecId::new(format.name()));
    params.sample_rate = Some(SAMPLE_RATE);
    params.channels = Some(2);
    params.sample_format = Some(SampleFormat::F32);
    let stream = StreamInfo {
        index: 0,
        params,
        time_base: time_base(),
        duration: Some(song.frames() as i64),
        start_time: Some(0),
    };
    let mut metadata = Vec::new();
    if !song.title().is_empty() {
        metadata.push(("title".to_string(), song.title().to_string()));
    }
    Ok(Box::new(TrackerDemuxer { format, data, song, stream, metadata, next_start: Some(0) }))
}

impl Demuxer for TrackerDemuxer {
    fn format_name(&self) -> &str {
        self.format.name()
    }

    fn streams(&self) -> &[StreamInfo] {
        std::slice::from_ref(&self.stream)
    }

    fn next_packet(&mut self) -> Result<Packet> {
        let start = self.next_start.take().ok_or(Error::Eof)? as i64;
        let mut pkt = Packet::new(0, time_base(), self.data.clone());
        pkt.pts = Some(start);
        pkt.dts = Some(start);
        pkt.flags.keyframe = true;
        Ok(pkt)
    }

    /// Lands on the start of the row playing at `pts`.
    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        let start = self.song.row_at(pts.max(0) as u64).map_or(0, |r| r.frame);
        self.next_start = Some(start);
        Ok(start as i64)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn duration_micros(&self) -> Option<i64> {
        Some((self.song.frames() as u128 * 1_000_000 / SAMPLE_RATE as u128) as i64)
    }
}

struct TrackerDecoder {
    id: CodecId,
    renderer: Option<Renderer>,
    buf: Vec<f32>,
}

/// Decoder factory for every module codec id.
pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(TrackerDecoder { id: params.codec_id.clone(), renderer: None, buf: vec![0.0; READ_FRAMES * 2] }))
}

impl Decoder for TrackerDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    /// Loads the module and starts rendering at the packet's pts.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let song = Song::load(&packet.data).ok_or_else(|| Error::invalid("tracker: module does not load"))?;
        self.renderer = Some(song.renderer_at(packet.pts.unwrap_or(0).max(0) as u64));
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(r) = self.renderer.as_mut() else { return Err(Error::NeedMore) };
        let pts = r.position() as i64;
        let n = r.read(&mut self.buf);
        if n == 0 {
            self.renderer = None;
            return Err(Error::NeedMore);
        }
        let mut bytes = Vec::with_capacity(n * 8);
        for v in &self.buf[..n * 2] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        Ok(Frame::Audio(AudioFrame { samples: n as u32, pts: Some(pts), data: vec![bytes] }))
    }

    /// The song renders from the one packet; there is nothing buffered to
    /// drain beyond what `receive_frame` keeps producing.
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }

    /// Drops the song: draining it frame by frame would render the rest.
    fn reset(&mut self) -> Result<()> {
        self.renderer = None;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(AudioFormat { sample_format: SampleFormat::F32, sample_rate: SAMPLE_RATE, channels: 2 })
    }
}

/// Registers one decoder per format's codec id.
pub fn register_codecs(reg: &mut CodecRegistry) {
    for format in FORMATS {
        reg.register(
            CodecInfo::new(CodecId::new(format.name()))
                .capabilities(
                    CodecCapabilities::audio("tracker_sw")
                        .with_lossy(false)
                        .with_intra_only(true)
                        .with_max_channels(2)
                        .with_max_sample_rate(SAMPLE_RATE)
                        .with_priority(50),
                )
                .with_resolution_priority(50)
                .decoder(make_decoder),
        );
    }
}

/// Registers one demuxer, extension and probe per format.
pub fn register_containers(reg: &mut ContainerRegistry) {
    let probes: [(Format, oxideav_core::ContainerProbeFn); 8] = [
        (Format::Mod, probe_mod),
        (Format::S3m, probe_s3m),
        (Format::Xm, probe_xm),
        (Format::It, probe_it),
        (Format::Mtm, probe_mtm),
        (Format::Six69, probe_669),
        (Format::Ult, probe_ult),
        (Format::Stm, probe_stm),
    ];
    for (format, probe) in probes {
        reg.register_demuxer(format.name(), open);
        reg.register_extension(format.name(), format.name());
        reg.register_probe(format.name(), probe);
    }
}

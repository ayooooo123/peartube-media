//! A synthetic container and decoder for testing consumers of
//! `PacketMetadata::audio_trim`. Every decoded sample encodes its own index
//! in the decoder's output, so a test reads exactly which samples survived
//! trimming ([`index_of`]).
//!
//! A [`Spec`] describes the stream: the container's declared rate (the rate
//! of packet durations and trim counts), the decoder's output rate (an
//! SBR-like decoder outputs more samples per packet than the container
//! declares), how the decoder hands out a packet's samples ([`Mode`]), and
//! each packet's declared duration and trim. Write [`Spec::to_bytes`] to a
//! file with the [`EXTENSION`] extension and open it with [`register`]ed
//! registries.

#![forbid(unsafe_code)]

use oxideav_core::{
    AudioFormat, AudioFrame, AudioTrim, CodecId, CodecInfo, CodecParameters, CodecResolver, Decoder, Demuxer, Error,
    Frame, Packet, PacketMetadata, ReadSeek, Result, RuntimeContext, SampleFormat, StreamInfo, TimeBase,
};
use std::collections::VecDeque;
use std::io::Read;

/// File extension of a fixture stream.
pub const EXTENSION: &str = "rctrim";
const CONTAINER: &str = "refcheck_trim";
const CODEC: &str = "refcheck_trim_pcm";

/// How the decoder hands out a packet's samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// One frame per packet, right after it is sent.
    Direct,
    /// One frame per packet, one packet late; the last comes out at flush.
    Delayed,
    /// Two frames per packet (halves), right after it is sent.
    Split,
    /// As `Direct`, but the first packet decodes to nothing (an Opus
    /// decoder whose pre-skip covers it does that).
    DropFirst,
}

impl Mode {
    const ALL: [(Mode, &'static str); 4] =
        [(Mode::Direct, "direct"), (Mode::Delayed, "delayed"), (Mode::Split, "split"), (Mode::DropFirst, "dropfirst")];
}

/// One packet: its duration at the declared rate, and its trim.
#[derive(Clone, Copy, Debug)]
pub struct FixturePacket {
    pub duration: u32,
    pub skip: u32,
    pub discard: u32,
    /// The rate the trim counts are in; 0 is an invalid trim, which
    /// consumers must ignore.
    pub trim_rate: u32,
}

/// A fixture stream.
#[derive(Clone, Debug)]
pub struct Spec {
    pub channels: u16,
    /// The container's sample rate: packet durations and time base.
    pub declared_rate: u32,
    /// The decoder's output rate: each packet decodes to
    /// `duration * output_rate / declared_rate` samples.
    pub output_rate: u32,
    pub mode: Mode,
    /// The first packet's pts, in declared samples (MP4 edit lists put
    /// priming before zero).
    pub start_pts: i64,
    pub packets: Vec<FixturePacket>,
    /// After a seek, the packet the demuxer lands on carries the rest of
    /// the first packet's skip, `max(skip - (pts - start_pts), 0)`, as
    /// FFmpeg's mov demuxer does (`mov_get_skip_samples`).
    pub skip_after_seek: bool,
    /// The decoder stamps each frame with the pts of the packet it decoded.
    pub stamp: bool,
    /// Packets declare their durations.
    pub durations: bool,
}

impl Spec {
    /// `count` packets of `duration` declared samples with durations,
    /// decoded at the declared rate into frames without pts, without
    /// trims.
    pub fn new(channels: u16, rate: u32, duration: u32, count: usize) -> Spec {
        Spec {
            channels,
            declared_rate: rate,
            output_rate: rate,
            mode: Mode::Direct,
            start_pts: 0,
            packets: vec![FixturePacket { duration, skip: 0, discard: 0, trim_rate: rate }; count],
            skip_after_seek: false,
            stamp: false,
            durations: true,
        }
    }

    /// Output samples (per channel) packet `n` decodes to.
    pub fn output_of(&self, n: usize) -> u64 {
        u64::from(self.packets[n].duration) * u64::from(self.output_rate) / u64::from(self.declared_rate)
    }

    /// Output samples of the whole stream, untrimmed.
    pub fn output_total(&self) -> u64 {
        (0..self.packets.len()).map(|n| self.output_of(n)).sum()
    }

    /// The fixture file's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mode = Mode::ALL.iter().find(|m| m.0 == self.mode).map_or("direct", |m| m.1);
        let mut s = format!(
            "{} {} {} {mode} {} {} {} {}\n",
            self.channels,
            self.declared_rate,
            self.output_rate,
            self.start_pts,
            self.skip_after_seek as u8,
            self.stamp as u8,
            self.durations as u8
        );
        for p in &self.packets {
            s.push_str(&format!("{} {} {} {}\n", p.duration, p.skip, p.discard, p.trim_rate));
        }
        s.into_bytes()
    }

    fn parse(bytes: &[u8]) -> Result<Spec> {
        let bad = || Error::invalid("refcheck trim fixture: malformed spec");
        let text = std::str::from_utf8(bytes).map_err(|_| bad())?;
        let mut lines = text.lines();
        let head: Vec<&str> = lines.next().ok_or_else(bad)?.split_whitespace().collect();
        let [channels, declared, output, mode, start, seek, stamp, durations] = head[..] else { return Err(bad()) };
        let mode = Mode::ALL.iter().find(|m| m.1 == mode).ok_or_else(bad)?.0;
        let mut packets = Vec::new();
        for line in lines {
            let f: Vec<u32> = line.split_whitespace().map(str::parse).collect::<std::result::Result<_, _>>().map_err(|_| bad())?;
            let [duration, skip, discard, trim_rate] = f[..] else { return Err(bad()) };
            packets.push(FixturePacket { duration, skip, discard, trim_rate });
        }
        let spec = Spec {
            channels: channels.parse().map_err(|_| bad())?,
            declared_rate: declared.parse().map_err(|_| bad())?,
            output_rate: output.parse().map_err(|_| bad())?,
            mode,
            start_pts: start.parse().map_err(|_| bad())?,
            packets,
            skip_after_seek: seek == "1",
            stamp: stamp == "1",
            durations: durations == "1",
        };
        if spec.channels == 0 || spec.declared_rate == 0 || spec.output_rate == 0 {
            return Err(bad());
        }
        Ok(spec)
    }
}

/// The value the decoder writes for output sample `index` of `channel`:
/// exact in f32 below 2^24 samples, negated on odd channels.
pub fn value(index: u64, channel: usize) -> f32 {
    let v = index as f32 / 2_097_152.0;
    if channel % 2 == 1 { -v } else { v }
}

/// The output index a channel-0 sample was decoded as.
pub fn index_of(sample: f32) -> u64 {
    (sample * 2_097_152.0).round() as u64
}

/// The output indices of channel 0 of interleaved `pcm`.
pub fn indices(pcm: &[f32], channels: usize) -> Vec<u64> {
    pcm.chunks_exact(channels).map(|f| index_of(f[0])).collect()
}

/// `indices` as contiguous runs `[start, end)`, for exact, readable
/// assertions.
pub fn runs(indices: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &i in indices {
        match out.last_mut() {
            Some(run) if run.1 == i => run.1 += 1,
            _ => out.push((i, i + 1)),
        }
    }
    out
}

/// Installs the fixture container (extension [`EXTENSION`]) and decoder.
pub fn register(ctx: &mut RuntimeContext) {
    ctx.containers.register_demuxer(CONTAINER, open);
    ctx.containers.register_extension(EXTENSION, CONTAINER);
    ctx.codecs.register(CodecInfo::new(CodecId::new(CODEC)).decoder(make_decoder));
}

struct FixtureDemuxer {
    spec: Spec,
    streams: Vec<StreamInfo>,
    /// Per packet: its pts and its first output sample index.
    starts: Vec<(i64, u64)>,
    next: usize,
    metadata: PacketMetadata,
    /// Skip for the next packet after a seek.
    seek_skip: Option<u32>,
}

fn open(mut input: Box<dyn ReadSeek>, _codecs: &dyn CodecResolver) -> Result<Box<dyn Demuxer>> {
    let mut bytes = Vec::new();
    input.read_to_end(&mut bytes)?;
    let spec = Spec::parse(&bytes)?;
    let mut params = CodecParameters::audio(CodecId::new(CODEC));
    params.sample_rate = Some(spec.declared_rate);
    params.channels = Some(spec.channels);
    params.sample_format = Some(SampleFormat::F32);
    let mode = Mode::ALL.iter().position(|m| m.0 == spec.mode).unwrap_or(0) as u8;
    params.extradata = [&[mode, spec.stamp as u8][..], &spec.output_rate.to_le_bytes(), &spec.channels.to_le_bytes()].concat();
    let time_base = TimeBase::new(1, i64::from(spec.declared_rate));
    let stream = StreamInfo { index: 0, time_base, duration: None, start_time: None, params };
    let mut starts = Vec::with_capacity(spec.packets.len());
    let (mut pts, mut index) = (spec.start_pts, 0u64);
    for (n, p) in spec.packets.iter().enumerate() {
        starts.push((pts, index));
        pts += i64::from(p.duration);
        index += spec.output_of(n);
    }
    Ok(Box::new(FixtureDemuxer {
        spec,
        streams: vec![stream],
        starts,
        next: 0,
        metadata: PacketMetadata::default(),
        seek_skip: None,
    }))
}

impl Demuxer for FixtureDemuxer {
    fn format_name(&self) -> &str {
        CONTAINER
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        self.metadata = PacketMetadata::default();
        let n = self.next;
        let Some(p) = self.spec.packets.get(n).copied() else { return Err(Error::Eof) };
        self.next += 1;
        let (pts, first) = self.starts[n];
        let count = self.spec.output_of(n) as u32;
        let data = [&first.to_le_bytes()[..], &count.to_le_bytes()].concat();
        let mut packet = Packet::new(0, self.streams[0].time_base, data);
        packet.pts = Some(pts);
        packet.dts = Some(pts);
        packet.duration = self.spec.durations.then_some(i64::from(p.duration));
        packet.flags.keyframe = true;
        let skip = self.seek_skip.take().unwrap_or(p.skip);
        if skip > 0 || p.discard > 0 {
            self.metadata.audio_trim = Some(AudioTrim { skip_samples: skip, discard_padding: p.discard, sample_rate: p.trim_rate });
        }
        Ok(packet)
    }

    fn packet_metadata(&self) -> PacketMetadata {
        self.metadata.clone()
    }

    fn seek_to(&mut self, _stream_index: u32, pts: i64) -> Result<i64> {
        self.metadata = PacketMetadata::default();
        let n = self.starts.iter().rposition(|&(start, _)| start <= pts).unwrap_or(0);
        self.next = n;
        let landed = self.starts.get(n).map_or(self.spec.start_pts, |s| s.0);
        self.seek_skip = None;
        if self.spec.skip_after_seek && n < self.spec.packets.len() {
            let skip = i64::from(self.spec.packets[0].skip) - (landed - self.spec.start_pts);
            self.seek_skip = Some(skip.clamp(0, i64::from(u32::MAX)) as u32);
        }
        Ok(landed)
    }
}

struct FixtureDecoder {
    id: CodecId,
    mode: Mode,
    stamp: bool,
    format: AudioFormat,
    /// Delayed: the packet not output yet, as (first, count, pts).
    held: Option<(u64, u32, Option<i64>)>,
    sent: usize,
    queue: VecDeque<Frame>,
    flushed: bool,
}

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let e = &params.extradata;
    let bad = || Error::invalid("refcheck trim fixture: extradata");
    if e.len() != 8 {
        return Err(bad());
    }
    let mode = Mode::ALL.get(usize::from(e[0])).ok_or_else(bad)?.0;
    let format = AudioFormat {
        sample_format: SampleFormat::F32,
        sample_rate: u32::from_le_bytes([e[2], e[3], e[4], e[5]]),
        channels: u16::from_le_bytes([e[6], e[7]]),
    };
    Ok(Box::new(FixtureDecoder {
        id: CodecId::new(CODEC),
        mode,
        stamp: e[1] == 1,
        format,
        held: None,
        sent: 0,
        queue: VecDeque::new(),
        flushed: false,
    }))
}

impl FixtureDecoder {
    fn emit(&mut self, first: u64, count: u32, pts: Option<i64>) {
        let channels = self.format.channels as usize;
        let pts = if self.stamp { pts } else { None };
        let frame = |first: u64, count: u32| {
            let mut bytes = Vec::with_capacity(count as usize * channels * 4);
            for i in 0..u64::from(count) {
                for c in 0..channels {
                    bytes.extend_from_slice(&value(first + i, c).to_le_bytes());
                }
            }
            Frame::Audio(AudioFrame { samples: count, pts, data: vec![bytes] })
        };
        if self.mode == Mode::Split {
            let half = count / 2;
            self.queue.push_back(frame(first, half));
            self.queue.push_back(frame(first + u64::from(half), count - half));
        } else {
            self.queue.push_back(frame(first, count));
        }
    }
}

impl Decoder for FixtureDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let d = &packet.data;
        if d.len() != 12 {
            return Err(Error::invalid("refcheck trim fixture: packet"));
        }
        let first = u64::from_le_bytes(d[..8].try_into().unwrap());
        let count = u32::from_le_bytes(d[8..].try_into().unwrap());
        self.sent += 1;
        match self.mode {
            Mode::Delayed => {
                if let Some((f, c, pts)) = self.held.replace((first, count, packet.pts)) {
                    self.emit(f, c, pts);
                }
            }
            Mode::DropFirst if self.sent == 1 => {}
            _ => self.emit(first, count, packet.pts),
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.queue.pop_front() {
            Some(frame) => Ok(frame),
            None if self.flushed => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        if let Some((f, c, pts)) = self.held.take() {
            self.emit(f, c, pts);
        }
        self.flushed = true;
        Ok(())
    }

    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(self.format)
    }
}

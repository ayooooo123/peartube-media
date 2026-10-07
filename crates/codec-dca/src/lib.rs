// Ported from FFmpeg libavcodec/dcadec.c registration surface and
// libavformat/dtsdec.c + dtshddec.c (commit 2da55bf), LGPL-2.1-or-later.

//! Crate registration: the `dca` decoder (all DTS profiles — Core, XCH,
//! XXCH, X96, XBR, EXSS, XLL DTS-HD MA, LBR DTS Express) and the raw
//! `dts` / `dtshd` demuxers.

// Module wiring for the ported FFmpeg DCA files (commit 2da55bf).

pub mod avtx;
pub mod bitreader;
pub mod bitreader_le;
pub mod core;
pub mod crc16;
pub mod dca;
pub mod data;
pub mod decoder;
pub mod dsp;
pub mod exss;
pub mod huffman;
pub mod lbr;
pub mod math;
pub mod vlc;
pub mod xll;

mod demuxer;
pub use demuxer::register_containers;

use crate::decoder::DcaDecoder;
use crate::demuxer::{FrameSplitter, Oversized};
use oxideav_core::{AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, Decoder, Error as CoreError, Frame, Packet, Result as CoreResult, RuntimeContext, SampleFormat, TimeBase};
use std::collections::VecDeque;

/// OxideAV's DTS codec id — every demuxer already maps DTS tags to it, so
/// registering under the same id with a lower priority makes this decoder
/// win resolution without touching crates/codecs (retry guidance).
pub const CODEC_ID_STR_DCA: &str = "dts";

/// Priority over OxideAV's core-only `dts` decoder (OxideAV software sits
/// at 100+; lower wins; contract value 50).
pub const RESOLUTION_PRIORITY: i32 = 50;

/// FFmpeg's decoder name is `dca` (`ffmpeg -decoders`: "dca — DCA (DTS
/// Coherent Acoustics) (codec dts)").
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    demuxer::register_containers(&mut ctx.containers);
}

/// Register the decoder under FFmpeg's `dca` name with OxideAV's `dts`
/// codec id. Container tag claims:
/// - WAVEFORMATEX `wFormatTag` 0x2001 (DTS in RIFF/WAV, mmreg.h).
/// - Matroska `A_DTS` (DTS-HD MA / DTS:X / core — one CodecID).
/// - MP4/QuickTime sample entries `dtsc` (core), `dtsh` (DTS-HD HRA),
///   `dtsl` (DTS-HD MA), `dtse` (DTS Express; ETSI TS 102 114).
/// - MPEG-TS stream types are mapped to the `dts` id by oxideav-mpegts.
pub fn register_codecs(reg: &mut oxideav_core::CodecRegistry) {
    let id = CodecId::new(CODEC_ID_STR_DCA);
    let caps = CodecCapabilities::audio("dca_sw_dec")
        .with_lossy(true)
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(32)
        .with_max_sample_rate(384_000)
        .with_priority(RESOLUTION_PRIORITY);
    reg.register(
        CodecInfo::new(id)
            .capabilities(caps)
            .decoder(make_decoder)
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tag(oxideav_core::CodecTag::wave_format(0x2001))
            .tag(oxideav_core::CodecTag::matroska("A_DTS"))
            .tag(oxideav_core::CodecTag::fourcc(b"dtsc"))
            .tag(oxideav_core::CodecTag::fourcc(b"dtsh"))
            .tag(oxideav_core::CodecTag::fourcc(b"dtsl"))
            .tag(oxideav_core::CodecTag::fourcc(b"dtse")),
    );
}

oxideav_core::register!("codec-dca", register);

fn make_decoder(params: &CodecParameters) -> CoreResult<Box<dyn Decoder>> {
    // dtshd padding (see the dtshd demuxer): trim `initial_padding` leading
    // samples, keep `keep` samples total, like FFmpeg's skip-samples side
    // data.
    // dtshd padding (see the dtshd demuxer): FFmpeg's CLI applies the
    // skip-samples side data when decoding, so the decoder trims
    // `initial_padding` leading samples and keeps `keep` samples total.
    let initial_padding: u64 = params
        .options
        .get("dtshd_initial_padding")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let keep: u64 = params
        .options
        .get("dtshd_keep_samples")
        .and_then(|v| v.parse().ok())
        .unwrap_or(u64::MAX);
    Ok(Box::new(DcaDecoderImpl::new(initial_padding, keep)))
}

/// OxideAV `Decoder` adapter over [`DcaDecoder`].
pub struct DcaDecoderImpl {
    inner: DcaDecoder,
    codec_id: CodecId,
    channels: u16,
    sample_rate: u32,
    bits: u32,
    /// dtshd skip-samples: leading samples to drop, samples to keep.
    trim_head: u64,
    keep: u64,
    emitted: u64,
    /// Absolute decoded-sample position (trim window reference).
    decoded: u64,
    /// Frames cut from the packets so far, and their timing.
    stream: FrameStream,
    /// The packets' time base, which decoded frames are timed in.
    time_base: TimeBase,
    /// Decoded frames not yet received.
    ready: VecDeque<Frame>,
}

/// Packets whose timestamps are kept: every one that may hold the first
/// byte of the frame the stream has not timed yet. FFmpeg's parser keeps
/// 4 (AV_PARSER_PTS_NB) but fetches a frame's timestamp as soon as the
/// frame before it ends; here a frame is timed once its start is known,
/// on the last byte of its marker, so the packet holding its first byte
/// is at most [`demuxer::MARKER_LEN`] packets back (each brings a byte).
const PTS_NB: usize = demuxer::MARKER_LEN;

/// The dca parser stage FFmpeg runs between its demuxers and the decoder,
/// which a demuxer's packets reach here instead (an MPEG-TS PES may hold
/// several frames, or part of one): frames cut from the packets' bytes as
/// they arrive. A frame takes the PTS of the packet its first byte arrived
/// in when no earlier frame started there (ff_fetch_timestamp), else it
/// follows the frame before it by that frame's duration, as libavformat
/// times parsed packets.
#[derive(Default)]
struct FrameStream {
    split: FrameSplitter,
    /// Stream offset of the next input byte.
    fed: u64,
    /// Where the last packets started, and their PTS: those that started
    /// after the current frame's first byte, at most [`PTS_NB`].
    packets: VecDeque<(u64, Option<i64>)>,
    /// The frame in progress: where it starts and the PTS it takes from
    /// its packet, fetched once its start is known.
    current: Option<(u64, Option<i64>)>,
    /// Where the timeline goes on after the last frame, when known.
    next_pts: Option<i64>,
    /// `dca_parse_params`'s LBR sampling rate code.
    lbr_sr_code: Option<u8>,
}

impl FrameStream {
    /// A packet's bytes start at the next input byte.
    fn packet_starts(&mut self, pts: Option<i64>) {
        if self.packets.len() == PTS_NB {
            self.packets.pop_front();
        }
        self.packets.push_back((self.fed, pts));
    }

    /// The PTS of the last packet that started at or before `at`, if it
    /// started after the previous frame's first byte.
    fn fetch(&mut self, at: u64) -> Option<i64> {
        let pts = self.packets.iter().rev().find(|&&(start, _)| start <= at).and_then(|&(_, pts)| pts);
        self.packets.retain(|&(start, _)| start > at);
        pts
    }

    /// Fetch the PTS of the frame in progress when its start is new.
    fn note_start(&mut self) {
        if self.current.is_none() {
            if let Some(at) = self.split.frame_start() {
                self.current = Some((at, self.fetch(at)));
            }
        }
    }

    /// The time of the frame cut at `at`: its packet's PTS, else the end
    /// of the frame before it.
    fn pts_of(&mut self, at: u64) -> Option<i64> {
        let fetched = match self.current.take() {
            Some((start, pts)) if start == at => pts,
            _ => self.fetch(at),
        };
        fetched.or(self.next_pts)
    }
}

/// What the frames decoded in one call came to: the call fails only when
/// some frame failed and none decoded.
#[derive(Default)]
struct Outcome {
    decoded: bool,
    error: Option<&'static str>,
}

impl Outcome {
    fn add(&mut self, result: Result<(), &'static str>) {
        match result {
            Ok(()) => self.decoded = true,
            Err(e) => {
                self.error.get_or_insert(e);
            }
        }
    }

    fn result(self) -> CoreResult<()> {
        match self.error {
            Some(e) if !self.decoded => Err(CoreError::InvalidData(format!("dca: {e}"))),
            _ => Ok(()),
        }
    }
}

/// `samples` at `rate` in ticks of `tb`, rounded down as libavformat
/// rounds parsed packet durations; `None` without a rate or time base.
fn ticks(samples: u64, rate: u32, tb: TimeBase) -> Option<i64> {
    let den = u128::from(rate) * u128::try_from(tb.num()).ok()?;
    if den == 0 {
        return None;
    }
    i64::try_from(u128::from(samples) * u128::try_from(tb.den()).ok()? / den).ok()
}

/// `samples` at `rate` in ticks of `tb`, rounded to the nearest, as
/// `av_rescale_q` moves a decoded frame's pts past skipped samples.
fn ticks_nearest(samples: u64, rate: u32, tb: TimeBase) -> i64 {
    let (Ok(num), Ok(den)) = (u128::try_from(tb.den()), u128::try_from(tb.num())) else { return 0 };
    let den = u128::from(rate) * den;
    if den == 0 {
        return 0;
    }
    i64::try_from((u128::from(samples) * num + den / 2) / den).unwrap_or(i64::MAX)
}

impl DcaDecoderImpl {
    fn new(trim_head: u64, keep: u64) -> Self {
        Self {
            inner: DcaDecoder::new(),
            codec_id: CodecId::new(CODEC_ID_STR_DCA),
            channels: 0,
            sample_rate: 0,
            bits: 0,
            trim_head,
            keep,
            emitted: 0,
            decoded: 0,
            stream: FrameStream::default(),
            time_base: TimeBase::new(1, 1),
            ready: VecDeque::new(),
        }
    }

    /// The decoder's output layout: interleaved, `bits` wide (16 for XLL
    /// 16-bit storage, 32 otherwise — the lossy/fixed path emits 24-bit
    /// samples in 32-bit words and the float/LBR path emits f32).
    fn audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        if self.sample_rate == 0 || self.channels == 0 {
            return None;
        }
        let sample_format = if self.bits == 0 {
            SampleFormat::F32
        } else if self.bits == 16 {
            SampleFormat::S16
        } else {
            SampleFormat::S32
        };
        Some(oxideav_core::AudioFormat {
            sample_format,
            sample_rate: self.sample_rate,
            channels: self.channels,
        })
    }
}

impl Decoder for DcaDecoderImpl {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// Packets are cut into frames as FFmpeg's dca parser cuts the stream
    /// they make up, whether a packet holds several frames (an MPEG-TS PES
    /// can) or a frame spans packets; a frame is decoded once the next one
    /// starts, or at the end of the input.
    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        self.time_base = packet.time_base;
        if !packet.data.is_empty() {
            self.stream.packet_starts(packet.pts);
        }
        let mut outcome = Outcome::default();
        let mut rest = &packet.data[..];
        while !rest.is_empty() {
            let n = rest.len().min(self.stream.split.room());
            self.stream.split.push(&rest[..n]);
            self.stream.fed += n as u64;
            rest = &rest[n..];
            self.decode_complete_frames(&mut outcome);
        }
        outcome.result()
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        self.ready.pop_front().ok_or(CoreError::NeedMore)
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        self.audio_format()
    }

    /// The end of the input: the frame still held is decoded, or is an
    /// error when longer than the decoder takes. Input that never started
    /// a frame since the last one is an error.
    fn flush(&mut self) -> CoreResult<()> {
        let mut outcome = Outcome::default();
        let skipped = self.stream.split.skipped();
        match self.stream.split.finish() {
            Ok(Some((at, frame))) => {
                let pts = self.stream.pts_of(at);
                outcome.add(self.decode_frame(&frame, pts));
            }
            Ok(None) if skipped => outcome.add(Err("no frame starts in the input")),
            Ok(None) => {}
            Err(_) => outcome.add(Err("frame longer than the decoder takes")),
        }
        self.stream = FrameStream::default();
        self.inner.flush();
        outcome.result()
    }

    /// Forget the frame in progress, its timing and the decoded state: the
    /// next packet decodes as the first one would.
    fn reset(&mut self) -> CoreResult<()> {
        self.ready.clear();
        self.stream = FrameStream::default();
        self.decoded = 0;
        self.emitted = 0;
        self.inner.flush();
        Ok(())
    }
}

/// Samples per channel of a decoded frame.
fn pending_samples(frame: &decoder::PendingFrame) -> u64 {
    fn shortest<T>(planes: &[Vec<T>]) -> usize {
        planes.iter().map(Vec::len).min().unwrap_or(0)
    }
    shortest(&frame.planes_f32).max(shortest(&frame.planes_s32)) as u64
}

impl DcaDecoderImpl {
    /// Decode every frame the input so far completes.
    fn decode_complete_frames(&mut self, outcome: &mut Outcome) {
        loop {
            match self.stream.split.next_frame() {
                Ok(Some((at, frame))) => {
                    let pts = self.stream.pts_of(at);
                    outcome.add(self.decode_frame(&frame, pts));
                }
                Ok(None) => {
                    self.stream.note_start();
                    return;
                }
                Err(Oversized { at, head }) => {
                    // Not decodable, but timed like any other frame: the
                    // next one follows it when its header tells how long
                    // it lasts.
                    let pts = self.stream.pts_of(at);
                    let tb = self.time_base;
                    let duration = demuxer::parse_params(&head, &mut self.stream.lbr_sr_code)
                        .and_then(|(samples, rate)| ticks(samples, rate, tb));
                    self.stream.next_pts = pts.zip(duration).map(|(p, d)| p.saturating_add(d));
                    outcome.add(Err("frame longer than the decoder takes"));
                }
            }
        }
    }

    /// Decode one frame presented at `pts`. The frame after it follows by
    /// its duration whether or not it decodes: the parsed one, as FFmpeg's
    /// parser reports it, else the decoded one; with neither, the next
    /// frame's time is unknown.
    fn decode_frame(&mut self, frame: &[u8], pts: Option<i64>) -> Result<(), &'static str> {
        let tb = self.time_base;
        let mut duration = demuxer::parse_params(frame, &mut self.stream.lbr_sr_code)
            .and_then(|(samples, rate)| ticks(samples, rate, tb));
        let result = self.inner.decode_packet(frame, pts);
        if result.is_ok() {
            if let Some(pending) = self.inner.pending.take() {
                duration = duration.or_else(|| ticks(pending_samples(&pending), pending.sample_rate, tb));
                if let Some(frame) = self.convert(pending) {
                    self.ready.push_back(frame);
                }
            }
        }
        self.stream.next_pts = pts.zip(duration).map(|(p, d)| p.saturating_add(d));
        result.map(|_| ())
    }

    /// One decoded frame in the decoder's output layout, trimmed to the
    /// dtshd sample window; `None` when nothing of it remains. Its pts
    /// moves past the samples trimmed from its own head, as FFmpeg's
    /// skip-samples handling moves it: by none once the padding is done.
    fn convert(&mut self, frame: decoder::PendingFrame) -> Option<Frame> {
        self.channels = frame.planes_f32.len().max(frame.planes_s32.len()) as u16;
        self.sample_rate = frame.sample_rate;
        self.bits = frame.bits_per_sample;

        // The decoder's native layout: F32 planes for the float paths, S16
        // for XLL 16-bit storage, S32 (24-bit samples << 8) for XLL 24-bit
        // and the fixed-point core path — FFmpeg's sample_fmt is exactly
        // this (fltp / s16 / s32).
        let planes_s16: Vec<Vec<i16>>;
        let planes_s32: Vec<Vec<i32>>;
        let planes_f32: Vec<Vec<f32>>;
        if frame.planes_s32.is_empty() {
            planes_s16 = Vec::new();
            planes_s32 = Vec::new();
            planes_f32 = frame.planes_f32;
        } else if frame.bits_per_sample == 16 {
            planes_f32 = Vec::new();
            planes_s32 = Vec::new();
            planes_s16 = frame
                .planes_s32
                .iter()
                .map(|plane| plane.iter().map(|&v| v as i16).collect())
                .collect();
        } else {
            planes_f32 = Vec::new();
            planes_s16 = Vec::new();
            planes_s32 = frame.planes_s32;
        }

        let channels = planes_f32
            .len()
            .max(planes_s16.len())
            .max(planes_s32.len());
        fn len_of<T>(p: &[Vec<T>]) -> usize {
            p.iter().map(|v| v.len()).min().unwrap_or(0)
        }
        let mut samples = len_of(&planes_f32)
            .max(len_of(&planes_s16))
            .max(len_of(&planes_s32));
        let decoded_samples = samples;
        // Truncate all planes to the common length.
        let mut planes_f32 = planes_f32;
        let mut planes_s16 = planes_s16;
        let mut planes_s32 = planes_s32;
        for plane in planes_f32.iter_mut() {
            plane.truncate(samples);
        }
        for plane in planes_s16.iter_mut() {
            plane.truncate(samples);
        }
        for plane in planes_s32.iter_mut() {
            plane.truncate(samples);
        }

        // dtshd skip-samples trimming: keep the absolute sample window
        // [trim_head, trim_head + keep).
        let mut head = 0;
        if self.trim_head != 0 || self.keep != u64::MAX {
            let win_start = self.trim_head;
            let win_end = self.trim_head.saturating_add(self.keep);
            let abs_start = self.decoded;
            let abs_end = self.decoded + samples as u64;
            let cut_start = abs_start.max(win_start);
            let cut_end = abs_end.min(win_end);
            if cut_end > cut_start {
                let s = (cut_start - abs_start) as usize;
                let e = (cut_end - abs_start) as usize;
                head = s;
                samples = e - s;
                for plane in planes_f32.iter_mut() {
                    plane.drain(..s);
                    plane.truncate(e);
                }
                for plane in planes_s16.iter_mut() {
                    plane.drain(..s);
                    plane.truncate(e);
                }
                for plane in planes_s32.iter_mut() {
                    plane.drain(..s);
                    plane.truncate(e);
                }
            } else {
                samples = 0;
            }
        }
        self.decoded += decoded_samples as u64;
        self.emitted += samples as u64;
        if samples == 0 {
            return None;
        }

        // Emit ONE interleaved plane in the decoder's native format.
        let interleaved: Vec<u8> = if !planes_f32.is_empty() {
            let mut out = Vec::with_capacity(samples * channels * 4);
            for i in 0..samples {
                for plane in &planes_f32 {
                    out.extend_from_slice(&plane[i].to_le_bytes());
                }
            }
            out
        } else if !planes_s16.is_empty() {
            let mut out = Vec::with_capacity(samples * channels * 2);
            for i in 0..samples {
                for plane in &planes_s16 {
                    out.extend_from_slice(&plane[i].to_le_bytes());
                }
            }
            out
        } else {
            // s32le interleaved (24-bit in 32, << 8 like FFmpeg).
            let mut out = Vec::with_capacity(samples * channels * 4);
            for i in 0..samples {
                for plane in &planes_s32 {
                    out.extend_from_slice(&plane[i].to_le_bytes());
                }
            }
            out
        };

        let audio = AudioFrame {
            samples: samples as u32,
            pts: frame.pts.map(|p| p.saturating_add(ticks_nearest(head as u64, self.sample_rate, self.time_base))),
            data: vec![interleaved],
        };
        Some(Frame::Audio(audio))
    }
}


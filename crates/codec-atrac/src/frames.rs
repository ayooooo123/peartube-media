// The send/receive shape the ATRAC decoders share: FFmpeg's decode loop
// (libavcodec/decode.c, decode_simple_internal, FFmpeg commit 2da55bf)
// over one codec's decode callback.
// Copyright (c) the FFmpeg developers; LGPL-2.1-or-later (see LICENSE).

use std::collections::VecDeque;

use oxideav_core::{
    AudioFormat, AudioFrame, CodecId, CodecParameters, Decoder, Error, Frame, Packet, Result,
    SampleFormat,
};

/// Planar float samples of one frame, one vector per channel.
pub(crate) type Planes = Vec<Vec<f32>>;

/// One codec's decode callback, as FFmpeg's `FF_CODEC_DECODE_CB`.
pub(crate) trait FrameCodec: Send {
    /// Decodes from the start of `data`: the bytes consumed and, when
    /// FFmpeg sets `got_frame`, the frame.
    fn decode(&mut self, data: &[u8]) -> Result<(usize, Option<Planes>)>;
}

/// Builds a codec from the stream parameters (FFmpeg's `init`).
pub(crate) type MakeCodec = fn(&CodecParameters) -> Result<Box<dyn FrameCodec>>;

/// A [`Decoder`] around a [`FrameCodec`]. Each packet is decoded as
/// FFmpeg's decode loop does: every call consumes what the codec reports
/// and the rest is fed again without the packet's timestamp; an error ends
/// the packet and is returned after the frames decoded before it.
pub(crate) struct AudioDecoder {
    codec_id: CodecId,
    params: CodecParameters,
    make: MakeCodec,
    codec: Box<dyn FrameCodec>,
    format: AudioFormat,
    queue: VecDeque<Result<AudioFrame>>,
    eof: bool,
}

/// The codec option FFmpeg's `AVCodecContext::block_align` travels in.
pub(crate) fn block_align(params: &CodecParameters) -> Option<usize> {
    params
        .options
        .get("block_align")
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&b| b > 0)
}

/// The stream's channel count, checked against `max`.
pub(crate) fn channels(params: &CodecParameters, max: u16) -> Result<usize> {
    match params.channels {
        Some(ch) if (1..=max).contains(&ch) => Ok(usize::from(ch)),
        other => Err(Error::invalid(format!(
            "unsupported channel count {other:?}"
        ))),
    }
}

impl AudioDecoder {
    pub(crate) fn open(params: &CodecParameters, make: MakeCodec) -> Result<Box<dyn Decoder>> {
        let codec = make(params)?;
        let sample_rate = params
            .sample_rate
            .filter(|&r| r > 0)
            .ok_or_else(|| Error::invalid("sample rate unknown"))?;
        let channels = params
            .channels
            .ok_or_else(|| Error::invalid("channel count unknown"))?;
        Ok(Box::new(Self {
            codec_id: params.codec_id.clone(),
            params: params.clone(),
            make,
            codec,
            format: AudioFormat {
                sample_format: SampleFormat::F32P,
                sample_rate,
                channels,
            },
            queue: VecDeque::new(),
            eof: false,
        }))
    }
}

impl Decoder for AudioDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut data: &[u8] = &packet.data;
        let mut pts = packet.pts;
        while !data.is_empty() {
            match self.codec.decode(data) {
                Ok((consumed, planes)) => {
                    if let Some(planes) = planes {
                        let samples = planes.first().map_or(0, Vec::len) as u32;
                        let data = planes
                            .iter()
                            .map(|p| p.iter().flat_map(|s| s.to_le_bytes()).collect())
                            .collect();
                        self.queue.push_back(Ok(AudioFrame { samples, pts, data }));
                    }
                    pts = None;
                    if consumed == 0 || consumed >= data.len() {
                        break;
                    }
                    data = &data[consumed..];
                }
                Err(e) => {
                    self.queue.push_back(Err(e));
                    break;
                }
            }
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        match self.queue.pop_front() {
            Some(Ok(frame)) => Ok(Frame::Audio(frame)),
            Some(Err(e)) => Err(e),
            None if self.eof => Err(Error::Eof),
            None => Err(Error::NeedMore),
        }
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    /// The decoder as freshly opened (FFmpeg's ATRAC decoders have no
    /// `flush`; a seek reopens them).
    fn reset(&mut self) -> Result<()> {
        self.codec = (self.make)(&self.params)?;
        self.queue.clear();
        self.eof = false;
        Ok(())
    }

    /// Planar float at the stream's rate and channel count, known from
    /// open on.
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(self.format)
    }
}

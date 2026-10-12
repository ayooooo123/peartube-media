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

/// A [`Decoder`] around a [`FrameCodec`], decoding as FFmpeg's decode loop
/// does: on demand, one frame per `receive_frame`. Each call consumes what
/// the codec reports, and the rest of the packet is fed again without the
/// packet's timestamp; an error drops the rest of the packet. A packet of
/// many small frames therefore costs one frame of memory at a time, not
/// all its frames at once.
pub(crate) struct AudioDecoder {
    codec_id: CodecId,
    params: CodecParameters,
    make: MakeCodec,
    codec: Box<dyn FrameCodec>,
    format: AudioFormat,
    pending: VecDeque<Pending>,
    eof: bool,
}

/// A packet not yet fully decoded: its bytes from `pos` on, and the
/// timestamp of its first frame until that frame is decoded.
struct Pending {
    data: Vec<u8>,
    pos: usize,
    pts: Option<i64>,
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
            pending: VecDeque::new(),
            eof: false,
        }))
    }
}

impl Decoder for AudioDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    /// Holds the packet; `receive_frame` decodes it.
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if !packet.data.is_empty() {
            self.pending.push_back(Pending {
                data: packet.data.clone(),
                pos: 0,
                pts: packet.pts,
            });
        }
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        while let Some(packet) = self.pending.front_mut() {
            let rest = &packet.data[packet.pos..];
            let left = rest.len();
            let pts = packet.pts.take();
            match self.codec.decode(rest) {
                Ok((consumed, planes)) => {
                    if consumed == 0 || consumed >= left {
                        self.pending.pop_front();
                    } else {
                        packet.pos += consumed;
                    }
                    if let Some(planes) = planes {
                        return Ok(Frame::Audio(audio_frame(&planes, pts)));
                    }
                }
                Err(e) => {
                    self.pending.pop_front();
                    return Err(e);
                }
            }
        }
        Err(if self.eof {
            Error::Eof
        } else {
            Error::NeedMore
        })
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    /// The decoder as freshly opened (FFmpeg's ATRAC decoders have no
    /// `flush`; a seek reopens them).
    fn reset(&mut self) -> Result<()> {
        self.codec = (self.make)(&self.params)?;
        self.pending.clear();
        self.eof = false;
        Ok(())
    }

    /// Planar float at the stream's rate and channel count, known from
    /// open on.
    fn output_audio_format(&self) -> Option<AudioFormat> {
        Some(self.format)
    }
}

/// The codec's planes as an [`AudioFrame`]: little-endian `f32` per plane.
fn audio_frame(planes: &Planes, pts: Option<i64>) -> AudioFrame {
    let samples = planes.first().map_or(0, Vec::len) as u32;
    let data = planes
        .iter()
        .map(|p| p.iter().flat_map(|s| s.to_le_bytes()).collect())
        .collect();
    AudioFrame { samples, pts, data }
}

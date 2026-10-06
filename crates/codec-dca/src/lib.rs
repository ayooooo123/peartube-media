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
use oxideav_core::{AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, Decoder, Error as CoreError, Frame, Packet, Result as CoreResult, RuntimeContext, SampleFormat};

/// OxideAV's DTS codec id. Production registers this factory before the
/// upstream decoder: `first_decoder` selects by registration order.
pub const CODEC_ID_STR_DCA: &str = "dts";

/// Tag-resolution priority (lower wins). This does not order decoder factories.
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

    fn send_packet(&mut self, packet: &Packet) -> CoreResult<()> {
        match self.inner.decode_packet(&packet.data, packet.pts) {
            Ok(_produced) => Ok(()),
            Err(e) => Err(CoreError::InvalidData(format!("dca: {e}"))),
        }
    }

    fn receive_frame(&mut self) -> CoreResult<Frame> {
        let Some(frame) = self.inner.pending.take() else {
            return Err(CoreError::NeedMore);
        };
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
            return Err(CoreError::NeedMore);
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
            pts: frame.pts.map(|p| p + (self.trim_head as i64)),
            data: vec![interleaved],
        };
        Ok(Frame::Audio(audio))
    }

    fn output_audio_format(&self) -> Option<oxideav_core::AudioFormat> {
        self.audio_format()
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.inner.flush();
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.flush()
    }
}

// Ported from FFmpeg libavcodec/dcadec.c registration surface and
// libavformat/dtsdec.c + dtshddec.c (commit 2da55bf), LGPL-2.1-or-later.

//! Crate registration: the `dca` decoder (all DTS profiles — Core, XCH,
//! XXCH, X96, XBR, EXSS, XLL DTS-HD MA, LBR DTS Express) and the raw
//! `dts` / `dtshd` demuxers.

// Module wiring for the ported FFmpeg DCA files (commit 2da55bf).

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

use crate::decoder::DcaDecoder;
use oxideav_core::{AudioFrame, CodecCapabilities, CodecId, CodecInfo, CodecParameters, Decoder, Error as CoreError, Frame, Packet, Result as CoreResult, RuntimeContext, SampleFormat};

pub const CODEC_ID_STR_DCA: &str = "dca";

/// Priority over OxideAV's core-only `dts` decoder (OxideAV software sits
/// at 100+; lower wins; contract value 50).
pub const RESOLUTION_PRIORITY: i32 = 50;

/// FFmpeg's decoder name is `dca` (`ffmpeg -decoders`: "dca — DCA (DTS
/// Coherent Acoustics) (codec dts)").
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    demuxer::register_containers(&mut ctx.containers);
}

/// Register the decoder. Container tag claims:
/// - WAVEFORMATEX `wFormatTag` 0x2001 (DTS in RIFF/WAV, mmreg.h).
/// - Matroska `A_DTS` (DTS-HD MA / DTS:X / core — one CodecID).
/// - MP4/QuickTime sample entries `dtsc` (core), `dtsh` (DTS-HD HRA),
///   `dtsl` (DTS-HD MA), `dtse` (DTS Express; ETSI TS 102 114).
/// - MPEG-TS stream type 0x82 is mapped to the `dts` id by oxideav-mpegts.
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
    Ok(Box::new(DcaDecoderImpl::new(
        initial_padding,
        keep,
        params.sample_format,
        params.channels,
    )))
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
    /// The stream-declared output format; frames are emitted interleaved
    /// in this format so frame data always matches the stream parameters
    /// (FFmpeg's decoder sets avctx->sample_fmt dynamically; OxideAV
    /// frames must be self-describing via the stream parameters).
    declared_format: Option<SampleFormat>,
    declared_channels: Option<u16>,
}

impl DcaDecoderImpl {
    fn new(
        trim_head: u64,
        keep: u64,
        declared_format: Option<SampleFormat>,
        declared_channels: Option<u16>,
    ) -> Self {
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
            declared_format,
            declared_channels,
        }
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

        // Normalize to f32 planes first.
        let f32_planes: Vec<Vec<f32>> = if !frame.planes_s32.is_empty() {
            let bits = if frame.bits_per_sample == 16 { 16 } else { 24 };
            frame
                .planes_s32
                .iter()
                .map(|plane| {
                    plane
                        .iter()
                        .map(|&v| {
                            if bits == 16 {
                                v as i16 as f32 / 32768.0
                            } else {
                                // 24-bit sample in 32 bits, like FFmpeg's
                                // s32 output for 24-bit DTS.
                                (v >> 8) as f32 / 8388608.0
                            }
                        })
                        .collect()
                })
                .collect()
        } else {
            frame.planes_f32.clone()
        };

        let channels = f32_planes.len();
        let samples = f32_planes.first().map_or(0, |p| p.len());
        let mut samples = samples;
        let decoded_samples = samples;
        let mut f32_planes = f32_planes;
        for plane in &f32_planes {
            samples = samples.min(plane.len());
        }
        for plane in f32_planes.iter_mut() {
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
                for plane in f32_planes.iter_mut() {
                    plane.drain(..s);
                    plane.truncate(e - s);
                }
                samples = e - s;
            } else {
                samples = 0;
            }
        }
        self.decoded += decoded_samples as u64;
        self.emitted += samples as u64;
        if samples == 0 {
            return Err(CoreError::NeedMore);
        }

        // Emit ONE interleaved plane in the stream-declared format
        // (SampleFormat::S32 declared by the dtshd demuxer, F32 by the raw
        // dts demuxer, S32 by default).
        let format = self.declared_format.unwrap_or(SampleFormat::S32);
        let interleaved: Vec<u8> = match format {
            SampleFormat::F32 | SampleFormat::F32P => {
                let mut out = Vec::with_capacity(samples * channels * 4);
                for i in 0..samples {
                    for plane in &f32_planes {
                        out.extend_from_slice(&plane[i].to_le_bytes());
                    }
                }
                out
            }
            _ => {
                // s32le interleaved (24-bit in 32, << 8 like FFmpeg).
                let mut out = Vec::with_capacity(samples * channels * 4);
                for i in 0..samples {
                    for plane in &f32_planes {
                        let v = ((plane[i].clamp(-1.0, 1.0) * 8388608.0) as i32) << 8;
                        out.extend_from_slice(&v.to_le_bytes());
                    }
                }
                out
            }
        };

        let audio = AudioFrame {
            samples: samples as u32,
            pts: frame.pts.map(|p| p + (self.trim_head as i64)),
            data: vec![interleaved],
        };
        Ok(Frame::Audio(audio))
    }

    fn flush(&mut self) -> CoreResult<()> {
        self.inner.flush();
        Ok(())
    }

    fn reset(&mut self) -> CoreResult<()> {
        self.flush()
    }
}

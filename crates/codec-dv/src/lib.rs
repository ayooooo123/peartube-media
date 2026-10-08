// Ported from FFmpeg (commit 2da55bf); see each module for its files.
// License: LGPL-2.1-or-later

//! DV (IEC 61834, SMPTE 314M DV25/DVCPRO50, SMPTE 370M DVCPRO HD) for
//! PearTube media: the `dvvideo` decoder (libavcodec/dvdec.c with dv.c,
//! dvdata.c, dv_profile.c and the C simple IDCT), the `dvaudio` decoder
//! (dvaudiodec.c, Ulead DV audio in WAV and AVI) and the raw `dv`
//! demuxer (libavformat/dv.c), which emits each DIF frame as a video
//! packet and the frame's audio as 16-bit PCM, as FFmpeg does.
//!
//! The decoder's IDCT is FFmpeg's C simple IDCT: on arm64 FFmpeg's own
//! default is NEON code that rounds differently, so reference tests run
//! FFmpeg with `-idct simple`.
#![forbid(unsafe_code)]

mod audio;
mod decoder;
mod demuxer;
mod idct;
mod profile;
mod tables;
mod vlc;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder, Result, RuntimeContext};

pub use audio::DvAudioDecoder;
pub use decoder::DvVideoDecoder;

/// The FourCCs FFmpeg maps to AV_CODEC_ID_DVVIDEO: libavformat/riff.c
/// (AVI) and isom_tags.c (MOV/MP4).
const FOURCCS: [&[u8; 4]; 30] = [
    b"dvsd", b"dvhd", b"dvh1", b"dvsl", b"dv25", b"dv50", b"cdvc", b"CDVH", b"CDV5", b"dvc ", b"dvcs", b"dvis", b"pdvc", b"SL25",
    b"SLDV", b"AVd1", b"dvcp", b"dvlp", b"dvl ", b"dvpp", b"dv5p", b"dv5n", b"AVdv", b"dvhq", b"dvhp", b"dvh2", b"dvh4", b"dvh5",
    b"dvh6", b"dvh3",
];

/// Resolution and capability priority, as the other PearTube decoders.
const PRIORITY: i32 = 50;

fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(DvVideoDecoder::new(params)))
}

fn make_audio_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    Ok(Box::new(DvAudioDecoder::new(params)?))
}

/// Registers the `dvvideo` decoder under every DV FourCC and `dvaudio`
/// under its WAVE format tags (riff.c; FFmpeg's MOV demuxer turns its
/// 'vdva' and 'dvca' tracks into PCM itself).
pub fn register_codecs(reg: &mut CodecRegistry) {
    let caps = CodecCapabilities::video("dvvideo_sw")
        .with_lossy(true)
        .with_intra_only(true)
        .with_priority(PRIORITY)
        .with_max_size(1440, 1080);
    reg.register(
        CodecInfo::new(CodecId::new("dvvideo"))
            .capabilities(caps)
            .with_resolution_priority(PRIORITY)
            .decoder(make_decoder)
            .tags(FOURCCS.map(CodecTag::fourcc)),
    );
    let caps = CodecCapabilities::audio("dvaudio_sw").with_lossy(true).with_intra_only(true).with_priority(PRIORITY);
    reg.register(
        CodecInfo::new(CodecId::new("dvaudio"))
            .capabilities(caps)
            .with_resolution_priority(PRIORITY)
            .decoder(make_audio_decoder)
            .tags([CodecTag::wave_format(0x0215), CodecTag::wave_format(0x0216)]),
    );
}

/// Registers the `dvvideo` decoder and the raw `dv` demuxer.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    demuxer::register(&mut ctx.containers);
}

oxideav_core::register!("codec-dv", register);

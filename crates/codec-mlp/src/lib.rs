//! MLP (Meridian Lossless Packing) and Dolby TrueHD decoder plus the raw
//! MLP / TrueHD demuxers, for PearTube's player.
//!
//! Ported from FFmpeg's LGPL-2.1-or-later decoders (commit 2da55bf):
//! `libavcodec/mlpdec.c`, `libavcodec/mlpdsp.[ch]`, `libavcodec/mlp.[ch]`,
//! `libavcodec/mlp_parse.[ch]`, `libavcodec/mlp_parser.c`, and
//! `libavformat/mlpdec.c` (the raw demuxers, with `rawdec.c`'s framing).
//! Licensed under LGPL-2.1-or-later; see LICENSE.

mod bitreader;
pub mod checksum;
mod common;
pub mod crc;
pub mod decoder;
mod demuxer;
pub use demuxer::register_containers;
pub mod dsp;
mod error;
mod parse;
mod tables;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecParameters, CodecRegistry, CodecTag, Decoder, RuntimeContext};

pub const CODEC_ID_STR_MLP: &str = "mlp";
pub const CODEC_ID_STR_TRUEHD: &str = "truehd";

/// Register the MLP + TrueHD decoders and the raw `mlp` / `truehd`
/// demuxers with the supplied context.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    demuxer::register_containers(&mut ctx.containers);
}

/// Register the two decoders. Container tag claims:
/// - Matroska `A_MLP` (MLP), `A_TRUEHD` (TrueHD) — `oxideav-mkv` maps
///   `A_TRUEHD` to the `truehd` codec id itself, and the string tags
///   resolve through the codec registry when containers probe.
/// - MP4 sample entry `mlpa` (TrueHD per mp4ra.org / ISO 14496-1).
/// - MPEG-TS stream type 0x83 (TrueHD), MPEG-PS stream ids 0xa0-0xaf (MLP)
///   and 0xb0-0xbf (TrueHD) map by the container demuxers to these ids.
pub fn register_codecs(reg: &mut CodecRegistry) {
    let mlp = CodecId::new(CODEC_ID_STR_MLP);
    let caps = CodecCapabilities::audio("mlp_sw_dec")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(8)
        .with_max_sample_rate(192_000)
        .with_priority(50);
    reg.register(
        CodecInfo::new(mlp.clone())
            .capabilities(caps)
            .decoder(make_mlp_decoder)
            .tag(CodecTag::matroska("A_MLP")),
    );

    let thd = CodecId::new(CODEC_ID_STR_TRUEHD);
    let caps = CodecCapabilities::audio("truehd_sw_dec")
        .with_lossless(true)
        .with_intra_only(true)
        .with_max_channels(8)
        .with_max_sample_rate(192_000)
        .with_priority(50);
    reg.register(
        CodecInfo::new(thd)
            .capabilities(caps)
            .decoder(make_truehd_decoder)
            // MP4/QuickTime sample entry 'mlpa' carries TrueHD
            // (mp4ra.org; FFmpeg isom_tags.c).
            .tag(CodecTag::fourcc(b"mlpa"))
            // Matroska A_TRUEHD (oxideav-mkv maps this string to the
            // "truehd" codec id itself; the tag claim covers registries
            // that hand the raw string through).
            .tag(CodecTag::matroska("A_TRUEHD")),
    );
}

oxideav_core::register!("codec-mlp", register);

fn make_mlp_decoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(decoder::MlpDecoder::new(params, true)))
}

fn make_truehd_decoder(params: &CodecParameters) -> oxideav_core::Result<Box<dyn Decoder>> {
    Ok(Box::new(decoder::MlpDecoder::new(params, false)))
}

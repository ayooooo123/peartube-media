//! Monkey's Audio (.ape) demuxer and decoder, ported from FFmpeg (commit
//! 2da55bf, LGPL-2.1-or-later):
//!
//! - `libavformat/ape.c`: demuxer `ape`, `ape_probe`, `ape_read_seek`
//! - `libavformat/apetag.c`: APE tag parsing
//! - `libavcodec/apedec.c`: decoder `ape` (versions 3.80 through 3.99)
//! - `libavcodec/lossless_audiodsp.c`: `scalarproduct_and_madd_int16`, `_int32`
//! - `libavcodec/bswapdsp.c`: `bswap_buf`
//!
//! Copyright (c) 2007 Benjamin Zores <ben@geexbox.org> based upon libdemac from Dave Chapman.
//! Copyright (c) FFmpeg developers
//!
//! Licensed under the GNU Lesser General Public License 2.1 or later.

#![forbid(unsafe_code)]

pub mod decoder;
pub mod demuxer;
pub mod dsp;
pub mod entropy;
pub mod filter;
pub mod predictor;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry,
    RuntimeContext,
};

pub use decoder::make_decoder;
pub use demuxer::{ape_probe, open_ape};

/// The codec id this crate provides.
pub const CODEC_ID: &str = "ape";

/// Returns codec info for registration.
fn ape_codec_info() -> CodecInfo {
    CodecInfo::new(CodecId::new(CODEC_ID))
        .capabilities(
            CodecCapabilities::audio("ape_sw")
                .with_lossless(true)
                .with_intra_only(true)
                .with_max_channels(2)
                .with_priority(50),
        )
        .with_resolution_priority(50)
        .payload_magic(b"MAC ")
}

/// Registers the APE decoder and tags into the codec registry.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        ape_codec_info()
            .decoder(make_decoder)
            .tags([CodecTag::fourcc(b"APE ")]),
    );
}

/// Registers the APE demuxer into the container registry.
pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register(reg);
}

/// Unified registration entry point for codecs and containers.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-ape", register);

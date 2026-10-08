//! TAK (Tom's lossless Audio Kompressor): the `tak` decoder (8, 16 and
//! 24 bits, mono to 6 channels) and the `.tak` demuxer with FFmpeg's TAK
//! parser framing.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/takdec.c, tak.c, tak.h,
//! takdsp.c, tak_parser.c, the C scalarproduct_int16 of audiodsp.c, the
//! cached little-endian reader of bitstream_template.h, libavutil/crc.c's
//! 24-bit IEEE CRC, and libavformat/takdec.c. Licensed under
//! LGPL-2.1-or-later (see LICENSE).

#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod demuxer;
mod tak;

pub use decoder::TakDecoder;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, ContainerRegistry, RuntimeContext};

/// Capability priority, ahead of any OxideAV claimant.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `tak`. FFmpeg maps no container tag to it; only the `tak`
/// demuxer carries it.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("tak"))
            .capabilities(
                CodecCapabilities::audio("tak_sw")
                    .with_lossless(true)
                    .with_intra_only(true)
                    .with_max_channels(6)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(TakDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY),
    );
}

/// Registers the `tak` demuxer.
pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register(reg);
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-tak", register);

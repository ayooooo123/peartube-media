//! TTA (The Lossless True Audio): the `tta` decoder (8, 16 and 24 bits,
//! up to 16 channels, password-encrypted streams with the `password`
//! option) and the `.tta` demuxer.
//!
//! Before reading payload, the demuxer validates every seek-table size
//! against a 256 MiB budget less the table and a full frame's i32 working
//! samples and output. Rice coding can exceed PCM size; PCM size alone is
//! not a compressed-frame cap. Packet/table growth stays within the
//! validated sizes, and a short last packet still reaches the decoder.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/tta.c, ttadsp.c,
//! ttadata.c, ttadata.h and libavformat/tta.c. Licensed under
//! LGPL-2.1-or-later (see LICENSE).

#![forbid(unsafe_code)]

mod decoder;
mod demuxer;

pub use decoder::TtaDecoder;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry, RuntimeContext,
};

/// Tag-resolution and capability priority, ahead of oxideav-tta.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `tta` with FFmpeg's tags for it: WAVE format 0x77A1 and
/// Matroska `A_TTA1`.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("tta"))
            .capabilities(
                CodecCapabilities::audio("tta_sw")
                    .with_lossless(true)
                    .with_intra_only(true)
                    .with_max_channels(16)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(TtaDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tags([CodecTag::wave_format(0x77A1), CodecTag::matroska("A_TTA1")]),
    );
}

/// Registers the `tta` demuxer.
pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register(reg);
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-tta", register);

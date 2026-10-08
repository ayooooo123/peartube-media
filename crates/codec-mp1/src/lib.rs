//! MPEG audio Layer I: the `mp1` decoder (FFmpeg's fixed-point decoder:
//! mono, stereo, dual channel and joint stereo; MPEG-1, MPEG-2 and MPEG-2.5
//! rates; S16P output).
//!
//! Ported from FFmpeg (commit 2da55bf): the Layer I path of
//! libavcodec/mpegaudiodec_template.c as mpegaudiodec_fixed.c builds it,
//! mpegaudiodecheader.c and .h, mpegaudiotabs.h, the scale factor table of
//! mpegaudiodec_common.c, and get_bits.h's reader; the synthesis filter is
//! the `mpegaudiodsp` port. Licensed under LGPL-2.1-or-later (see LICENSE).

#![forbid(unsafe_code)]

mod bits;
mod decoder;
mod header;

pub use decoder::Mp1Decoder;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, RuntimeContext};

/// Tag-resolution and capability priority, ahead of any OxideAV claimant.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `mp1` on Matroska's `A_MPEG/L1` and QuickTime/CAF's `.mp1`
/// (the tags FFmpeg maps to MP1); the MPEG audio demuxer names Layer I
/// streams `mp1` itself. RIFF's `WAVE_FORMAT_MPEG` stays MP2, as FFmpeg's
/// riff.c maps it.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("mp1"))
            .capabilities(
                CodecCapabilities::audio("mp1_sw")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_max_channels(2)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(Mp1Decoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tags([CodecTag::matroska("A_MPEG/L1"), CodecTag::fourcc(b".mp1")]),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-mp1", register);

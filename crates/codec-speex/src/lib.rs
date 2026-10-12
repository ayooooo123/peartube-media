//! Speex audio decoder: narrowband, wideband and ultra-wideband, in-band
//! stereo, from Ogg (header in the extradata), AVI/WAV and MOV (header
//! inside the extradata), and FLV or other carriers without a header
//! (mode from the sample rate).
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/speexdec.c and
//! speexdata.h, with get_bits.h's reader semantics. Licensed under
//! LGPL-2.1-or-later (see LICENSE); the ported files also carry the
//! Xiph.org BSD notice, kept in their headers.

#![forbid(unsafe_code)]

mod bitread;
mod decoder;
mod tables;

pub use decoder::SpeexDecoder;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, RuntimeContext};

/// Tag-resolution and capability priority: ahead of oxideav-speex.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `speex` with every tag FFmpeg maps to it: MOV/MP4 `spex`
/// and `SPXN`, NSV `SPX `, WAVE format 0xA109, and the Ogg header magic.
/// FLV's Speex codec id maps to `speex` in the FLV demuxer itself.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("speex"))
            .capabilities(
                CodecCapabilities::audio("speex_sw")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_max_channels(2)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(SpeexDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tags([
                CodecTag::fourcc(b"spex"),
                CodecTag::fourcc(b"SPXN"),
                CodecTag::fourcc(b"SPX "),
                CodecTag::wave_format(0xA109),
            ])
            .payload_magic(b"Speex   ".to_vec()),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-speex", register);

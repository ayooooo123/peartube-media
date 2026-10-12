//! DVD-Video and Blu-ray LPCM, ported from FFmpeg (commit 2da55bf,
//! LGPL-2.1-or-later):
//!
//! | Codec id | Ported from | Carried in | Output |
//! |---|---|---|---|
//! | `pcm_dvd` | `pcm-dvd.c` | MPEG-PS private stream 1, substreams 0xA0-0xAF | S16 or S32 (20/24-bit), interleaved |
//! | `pcm_bluray` | `pcm-bluray.c` | MPEG-TS / M2TS stream type 0x80 (HDMV) | S16 or S32 (24-bit), interleaved |
//!
//! Each packet carries its own header, so the decoders report their layout
//! (`output_audio_format`) from the first packet on.

mod bluray;
mod dvd;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, RuntimeContext};

fn lpcm_info(id: &str, max_rate: u32) -> CodecInfo {
    CodecInfo::new(CodecId::new(id)).capabilities(
        CodecCapabilities::audio(format!("{id}_sw"))
            .with_lossless(true)
            .with_intra_only(true)
            .with_max_channels(8)
            .with_max_sample_rate(max_rate),
    )
}

/// Registers the `pcm_dvd` and `pcm_bluray` decoders.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(lpcm_info("pcm_dvd", 96_000).decoder(dvd::make_decoder));
    reg.register(lpcm_info("pcm_bluray", 192_000).decoder(bluray::make_decoder));
}

/// Unified registration entry point.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

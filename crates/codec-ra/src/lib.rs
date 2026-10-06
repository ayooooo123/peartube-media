//! RealAudio decoders: ra_144, ra_288, ralf, cook.
//!
//! Ported from FFmpeg (commit 2da55bf), licensed under LGPL-2.1-or-later.
#![forbid(unsafe_code)]

pub mod bitreader;
pub mod cook;
pub mod cook_tables;
pub mod ra144;
pub mod ra144_tables;
pub mod ra288;
pub mod ra288_tables;
pub mod ralf;
pub mod ralf_tables;
pub mod vlc_len;
use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecTag, RuntimeContext,
};

fn audio_info(id: &str, name: &str, max_channels: u16, max_rate: u32) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(
            CodecCapabilities::audio(name)
                .with_lossy(true)
                .with_intra_only(true)
                .with_max_channels(max_channels)
                .with_max_sample_rate(max_rate),
        )
        .with_resolution_priority(50)
}

/// Register the RealAudio decoder family. Priority 50 puts these ahead of
/// any OxideAV implementation of the same ids (OxideAV software sits at
/// 100+); tag claims cover the containers the formats travel under.
pub fn register(ctx: &mut RuntimeContext) {
    // RealAudio 1.0 (14.4K). RM fourcc "14_4"/"lpcJ", Matroska "A_REAL/14_4".
    ctx.codecs.register(
        audio_info("ra_144", "codec-ra_ra_144", 1, 8000)
            .decoder(ra144::make_decoder)
            .tags([
                CodecTag::fourcc(b"14_4"),
                CodecTag::fourcc(b"lpcJ"),
                CodecTag::matroska("A_REAL/14_4"),
            ]),
    );

    // RealAudio 2.0 (28.8K). RM fourcc "28_8", Matroska "A_REAL/28_8".
    ctx.codecs.register(
        audio_info("ra_288", "codec-ra_ra_288", 1, 8000)
            .decoder(ra288::make_decoder)
            .tags([
                CodecTag::fourcc(b"28_8"),
                CodecTag::matroska("A_REAL/28_8"),
            ]),
    );
    // RealAudio Lossless. RM fourcc "LSD:".
    ctx.codecs.register(
        audio_info("ralf", "codec-ra_ralf", 2, 96_000)
            .decoder(ralf::make_decoder)
            .tags([CodecTag::fourcc(b"LSD:")]),
    );

    // RealAudio G2 (Cook). RM fourcc "cook", Matroska "A_REAL/COOK".
    ctx.codecs.register(
        audio_info("cook", "codec-ra_cook", 2, 96_000)
            .decoder(cook::make_decoder)
            .tags([
                CodecTag::fourcc(b"cook"),
                CodecTag::matroska("A_REAL/COOK"),
            ]),
    );
}

oxideav_core::register!("codec-ra", register);

//! GSM 06.10 full-rate speech (`gsm`) and its Microsoft variant (`gsm_ms`,
//! two frames per 65-byte block, and the MSN Audio rates), and the raw
//! `.gsm` demuxer.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/gsmdec.c,
//! gsmdec_template.c, msgsmdec.c, gsmdec_data.c, gsmdec_data.h, gsm.h and
//! libavformat/gsmdec.c. Licensed under LGPL-2.1-or-later (see LICENSE).

#![forbid(unsafe_code)]

mod decoder;
mod demuxer;
mod tables;

pub use decoder::GsmDecoder;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry, RuntimeContext,
};

/// Tag-resolution and capability priority, ahead of any OxideAV claimant.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Registers `gsm` (MOV/CAF `agsm`, AIFF `GSM `) and `gsm_ms` (WAVE formats
/// 0x0031, 0x0032 and 0x1500), FFmpeg's tags for them.
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("gsm"))
            .capabilities(
                CodecCapabilities::audio("gsm_sw")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_max_channels(1)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(GsmDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tags([CodecTag::fourcc(b"agsm"), CodecTag::fourcc(b"GSM ")]),
    );
    reg.register(
        CodecInfo::new(CodecId::new("gsm_ms"))
            .capabilities(
                CodecCapabilities::audio("gsm_ms_sw")
                    .with_lossy(true)
                    .with_intra_only(false)
                    .with_max_channels(1)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(GsmDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tags([CodecTag::wave_format(0x0031), CodecTag::wave_format(0x0032), CodecTag::wave_format(0x1500)]),
    );
}

/// Registers the raw `gsm` demuxer.
pub fn register_containers(reg: &mut ContainerRegistry) {
    demuxer::register(reg);
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-gsm", register);

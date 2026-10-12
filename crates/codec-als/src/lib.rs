//! MPEG-4 Audio Lossless Coding: the `mp4als` decoder (8 to 32 bits and
//! 32-bit float, Rice and BGMC entropy coding, long-term prediction, joint
//! stereo, multichannel coding, channel sorting).
//!
//! `send_packet` retains compressed bytes, never a packet's expanded PCM.
//! Each `receive_frame` decodes one ALS frame, without limiting how many
//! frames a packet may hold. Only its first output carries the packet pts;
//! `reset` discards pending input and resets the frame counter.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/alsdec.c, bgmc.c,
//! bgmc.h, mlz.c, mlz.h, the ALS part of mpeg4audio.c, and
//! libavutil/softfloat_ieee754.h, with get_bits.h's reader. Licensed under
//! LGPL-2.1-or-later (see LICENSE).

#![forbid(unsafe_code)]

mod bgmc;
mod bgmc_tables;
mod bits;
mod config;
mod decoder;
mod mlz;

pub use decoder::AlsDecoder;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ProbeContext, RuntimeContext};

/// Tag-resolution and capability priority, ahead of any OxideAV claimant.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// MPEG-4 audio (object type indication 0x40) is ALS when its
/// AudioSpecificConfig says audio object type 36; above the plain claim
/// AAC makes on the same tag, and nothing for any other object type.
fn probe(ctx: &ProbeContext) -> f32 {
    match ctx.header.and_then(config::audio_object_type) {
        Some(config::AOT_ALS) => 2.0,
        _ => 0.0,
    }
}

/// Registers `mp4als` on MPEG-4 audio's object type indication 0x40, how
/// MP4 and MOV carry it (FFmpeg maps no other container tag to ALS).
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("mp4als"))
            .capabilities(
                CodecCapabilities::audio("als_sw")
                    .with_lossless(true)
                    .with_intra_only(false)
                    .with_max_channels(config::MAX_CHANNELS as u16)
                    .with_priority(RESOLUTION_PRIORITY),
            )
            .decoder(|params| Ok(Box::new(AlsDecoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .probe(probe)
            .tag(CodecTag::mp4_object_type(0x40)),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-als", register);

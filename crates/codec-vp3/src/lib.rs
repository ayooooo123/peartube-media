//! On2 VP3 (VP30, VP31) and VP4 (VP40) video decoders.
//!
//! Ported from FFmpeg (commit 2da55bf): libavcodec/vp3.c, vp3dsp.c,
//! vp3data.h, vp4data.h, the C half-pel averages of hpeldsp.c,
//! videodsp_template.c (emulated_edge_mc), vlc.c and get_bits.h, plus
//! `ff_zigzag_direct` (mathtables.c) and the JPEG chrominance quant table
//! (jpegquanttables.c). Theora, which vp3.c also decodes, is left to
//! OxideAV's `oxideav-theora` and not registered here.
//!
//! Licensed under LGPL-2.1-or-later; see LICENSE.

#![forbid(unsafe_code)]

mod bitread;
mod decoder;
mod dsp;
mod tables;
mod vlc;

pub use decoder::Vp3Decoder;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, PixelFormat, RuntimeContext,
};

/// Tag-resolution and capability priority: ahead of OxideAV's own
/// decoders.
pub const RESOLUTION_PRIORITY: i32 = 50;

fn video_caps(implementation: &'static str) -> CodecCapabilities {
    let mut caps = CodecCapabilities::video(implementation)
        .with_lossy(true)
        .with_intra_only(false)
        .with_priority(RESOLUTION_PRIORITY)
        .with_max_size(16384, 16384);
    caps.accepted_pixel_formats = vec![PixelFormat::Yuv420P];
    caps
}

/// Registers `vp3` (VP30, VP31) and `vp4` (VP40).
pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        CodecInfo::new(CodecId::new("vp3"))
            .capabilities(video_caps("vp3_sw"))
            .decoder(|params| Ok(Box::new(Vp3Decoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tag(CodecTag::fourcc(b"VP30"))
            .tag(CodecTag::fourcc(b"VP31")),
    );
    reg.register(
        CodecInfo::new(CodecId::new("vp4"))
            .capabilities(video_caps("vp4_sw"))
            .decoder(|params| Ok(Box::new(Vp3Decoder::new(params)?)))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .tag(CodecTag::fourcc(b"VP40")),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-vp3", register);

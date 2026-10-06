//! RealVideo decoders (RV10, RV20, RV30, RV40).
//!
//! Ported from FFmpeg (commit 2da55bf):
//! - RV10/RV20: libavcodec/rv10.c with the H.263 pieces it runs on
//!   (ituh263dec.c, h263.c, h263data.c, h263dsp.c, mpeg4video.c,
//!   mpegvideo.c, mpegvideo_dec.c, mpegvideo_motion.c,
//!   mpegvideo_unquantize.c, mpegvideodata.c, simple_idct_template.c,
//!   hpeldsp.c, rl.c)
//! - RV30/RV40: libavcodec/rv34.c, rv30.c, rv40.c, rv34dsp.c, rv30dsp.c,
//!   rv40dsp.c, rv34vlc.h, rv40vlc2.h, rv34data.h, rv30data.h, rv40data.h,
//!   h264pred.c, h264chroma.c and h264qpel.c
//! - shared: get_bits.h, golomb.c/golomb.h, vlc.c, videodsp_template.c
//!
//! Licensed under LGPL-2.1-or-later; see LICENSE.

#![forbid(unsafe_code)]

pub mod bits;
mod golomb_tables;
pub mod picture;
pub mod rv10;
pub mod rv34;
pub mod vlc;

use oxideav_core::{
    CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, PixelFormat, RuntimeContext,
};

pub const RESOLUTION_PRIORITY: i32 = 50;

pub fn video_caps(implementation: &'static str) -> CodecCapabilities {
    let mut caps = CodecCapabilities::video(implementation)
        .with_lossy(true)
        .with_intra_only(false)
        .with_priority(RESOLUTION_PRIORITY)
        .with_max_size(16384, 16384);
    caps.accepted_pixel_formats = vec![PixelFormat::Yuv420P];
    caps
}

fn video_info(id: &'static str, implementation: &'static str) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(video_caps(implementation))
        .with_resolution_priority(RESOLUTION_PRIORITY)
}

pub fn register_codecs(reg: &mut CodecRegistry) {
    reg.register(
        video_info("rv10", "rv10_sw")
            .decoder(|params| Ok(Box::new(rv10::Rv1020Decoder::new(params, false)?)))
            .tag(CodecTag::fourcc(b"RV10"))
            .tag(CodecTag::matroska("V_REAL/RV10")),
    );
    reg.register(
        video_info("rv20", "rv20_sw")
            .decoder(|params| Ok(Box::new(rv10::Rv1020Decoder::new(params, true)?)))
            .tag(CodecTag::fourcc(b"RV20"))
            .tag(CodecTag::matroska("V_REAL/RV20")),
    );
    reg.register(
        video_info("rv30", "rv30_sw")
            .decoder(|params| Ok(Box::new(rv34::Rv34Decoder::new(params, true)?)))
            .tag(CodecTag::fourcc(b"RV30"))
            .tag(CodecTag::matroska("V_REAL/RV30")),
    );
    reg.register(
        video_info("rv40", "rv40_sw")
            .decoder(|params| Ok(Box::new(rv34::Rv34Decoder::new(params, false)?)))
            .tag(CodecTag::fourcc(b"RV40"))
            .tag(CodecTag::matroska("V_REAL/RV40")),
    );
}

pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
}

oxideav_core::register!("codec-rv", register);

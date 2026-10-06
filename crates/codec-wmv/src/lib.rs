//! Pure-Rust **WMV1 / WMV2 / WMV3 / VC-1** video decoders.
//!
//! Ported from FFmpeg commit 2da55bf (libavcodec): `wmv2dec.c`, `wmv2.c`,
//! `wmv2dsp.c`, `intrax8.c`, `intrax8dsp.c`, `msmpeg4dec.c`, `msmpeg4.c`,
//! `msmpeg4data.c`, `vc1dec.c`, `vc1.c`, `vc1_block.c`, `vc1_loopfilter.c`,
//! `vc1_mc.c`, `vc1_pred.c`, `vc1dsp.c`, `vc1data.c`, `vc1_parser.c` and the
//! h263/mpegvideo/idct pieces they need. LGPL-2.1-or-later; see LICENSE.
//!
//! Every byte comes from untrusted peers: `#![forbid(unsafe_code)]`, checked
//! arithmetic at bitstream-derived indices, `Error::InvalidData` instead of
//! panics, and dimension caps of 16384 per side / 256 MiB per frame.

#![forbid(unsafe_code)]
#![allow(dead_code, unused_variables, unused_mut, unused_imports, unused_assignments)]

pub mod tables;

mod bits;
mod idct;
mod msmpeg4;
mod vc1;
mod wmv2;
mod x8;

pub use msmpeg4::{CODEC_ID_MSMPEG4V1, CODEC_ID_MSMPEG4V2, CODEC_ID_MSMPEG4V3, CODEC_ID_WMV1};
pub use vc1::{CODEC_ID_VC1, CODEC_ID_WMV3};
pub use wmv2::CODEC_ID_WMV2;

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, PixelFormat};

pub const CODEC_ID_MSMPEG4V1_STR: &str = "msmpeg4v1";
pub const CODEC_ID_MSMPEG4V2_STR: &str = "msmpeg4v2";
pub const CODEC_ID_MSMPEG4V3_STR: &str = "msmpeg4v3";
pub const CODEC_ID_WMV1_STR: &str = "wmv1";
pub const CODEC_ID_WMV2_STR: &str = "wmv2";
pub const CODEC_ID_WMV3_STR: &str = "wmv3";
pub const CODEC_ID_VC1_STR: &str = "vc1";

/// Resolution priority for our FFmpeg-derived software decoders. Lower wins;
/// OxideAV software implementations sit at 100+, so 50 takes precedence.
pub const RESOLUTION_PRIORITY: i32 = 50;

/// Maximum accepted frame side (untrusted input).
pub const MAX_DIM: u32 = 16384;
/// Maximum accepted frame area in pixels (untrusted input).
pub const MAX_PIXELS: u64 = 8192 * 8192;

pub fn video_caps(implementation: &'static str) -> CodecCapabilities {
    let mut caps = CodecCapabilities::video(implementation)
        .with_lossy(true)
        .with_intra_only(false)
        .with_priority(RESOLUTION_PRIORITY)
        .with_max_size(MAX_DIM, MAX_DIM);
    caps.accepted_pixel_formats = vec![PixelFormat::Yuv420P];
    caps
}

fn video_info(id: &'static str, implementation: &'static str) -> CodecInfo {
    CodecInfo::new(CodecId::new(id))
        .capabilities(video_caps(implementation))
        .with_resolution_priority(RESOLUTION_PRIORITY)
}

/// Register the WMV/VC-1 decoder family with the codec registry.
pub fn register_codecs(reg: &mut CodecRegistry) {
    // WMV1: FFmpeg FourCC 'WMV1' (AVI / MKV-VFW). Carried in ASF via the
    // BITMAPINFOHEADER FourCC, which demuxers resolve through this claim.
    reg.register(
        video_info(CODEC_ID_WMV1_STR, "wmv1_sw")
            .decoder(|params| Ok(Box::new(msmpeg4::MsMpeg4Decoder::new_wmv1(params)?)))
            .tag(CodecTag::fourcc(b"WMV1")),
    );
    // WMV2: FourCC 'WMV2'.
    reg.register(
        video_info(CODEC_ID_WMV2_STR, "wmv2_sw")
            .decoder(|params| Ok(Box::new(wmv2::Wmv2Decoder::new(params)?)))
            .tag(CodecTag::fourcc(b"WMV2")),
    );
    // WMV3: FourCC 'WMV3' (Simple/Main profile; extradata = sequence header).
    reg.register(
        video_info(CODEC_ID_WMV3_STR, "wmv3_sw")
            .decoder(|params| Ok(Box::new(vc1::Vc1Decoder::new_wmv3(params)?)))
            .tag(CodecTag::fourcc(b"WMV3"))
            .tag(CodecTag::fourcc(b"WVP2")),
    );
    // VC-1: FourCC 'WVC1'/'WMVA' (AVI/MKV-VFW/MP4), Matroska "V_VC1".
    reg.register(
        video_info(CODEC_ID_VC1_STR, "vc1_sw")
            .decoder(|params| Ok(Box::new(vc1::Vc1Decoder::new_vc1(params)?)))
            .tags([
                CodecTag::fourcc(b"WVC1"),
                CodecTag::fourcc(b"WMVA"),
                CodecTag::fourcc(b"VC-1"),
            ]),
    );
    // Legacy MS-MPEG-4 v1/v2/v3 FourCCs decode here too (they share the
    // msmpeg4 core; the FourCCs feed the same decoder family).
    reg.register(
        video_info(CODEC_ID_MSMPEG4V1_STR, "msmpeg4v1_pear_sw")
            .decoder(|params| Ok(Box::new(msmpeg4::MsMpeg4Decoder::new_v1(params)?)))
            .tags([CodecTag::fourcc(b"MP41"), CodecTag::fourcc(b"MPG4")]),
    );
    reg.register(
        video_info(CODEC_ID_MSMPEG4V2_STR, "msmpeg4v2_pear_sw")
            .decoder(|params| Ok(Box::new(msmpeg4::MsMpeg4Decoder::new_v2(params)?)))
            .tag(CodecTag::fourcc(b"MP42")),
    );
    reg.register(
        video_info(CODEC_ID_MSMPEG4V3_STR, "msmpeg4v3_pear_sw")
            .decoder(|params| Ok(Box::new(msmpeg4::MsMpeg4Decoder::new_v3(params)?)))
            .tag(CodecTag::fourcc(b"MP43")),
    );
}

/// Install the codecs into a [`oxideav_core::RuntimeContext`].
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    crate::demuxers::register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-wmv", register);

pub mod demuxers;

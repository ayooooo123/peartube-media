//! Pure-Rust **MS-MPEG-4 v1/v2/v3 / WMV1 / WMV2 / WMV3 / VC-1** video
//! decoders and the raw VC-1 (`vc1`) / RCV (`vc1test`) demuxers.
//!
//! Ported from FFmpeg commit 2da55bf (libavcodec): `wmv2dec.c`, `wmv2dsp.c`,
//! `intrax8.c`, `intrax8dsp.c`, `intrax8huf.h`, `msmpeg4dec.c`, `msmpeg4.c`,
//! `msmpeg4data.c`, `msmpeg4_vc1_data.c`, `h263dec.c`, `h263.c`,
//! `h263dsp.c`, `ituh263dec.c`, `mpeg4videodec.c`, `mpegvideo.c`,
//! `mpegvideo_dec.c`, `mpegvideo_motion.c`, `hpeldsp.c`,
//! `simple_idct_template.c`, `simple_idct.c`, `vlc.c`, `rl.c`,
//! `get_bits.h`, `vc1dec.c`, `vc1.c`, `vc1_block.c`, `vc1_loopfilter.c`,
//! `vc1_mc.c`, `vc1_pred.c`, `vc1dsp.c`, `vc1data.c`, `vc1acdata.h`,
//! `vc1_parser.c`; (libavformat) `vc1dec.c`, `vc1test.c`.
//! LGPL-2.1-or-later; see LICENSE.
//!
//! Every byte comes from untrusted peers: `#![forbid(unsafe_code)]`, checked
//! arithmetic at bitstream-derived indices, `Error::InvalidData` instead of
//! panics, and dimension caps of 16384 per side / 256 MiB per frame.

#![forbid(unsafe_code)]

pub mod tables;

mod bits;
mod idct;
mod mpv;
mod msmpeg4;
mod vc1;
mod vc1_tables;
mod vlc;
mod wmv2;
mod x8;

pub mod demuxers;

pub use demuxers::{CODEC_ID_VC1, CODEC_ID_WMV3};
pub use msmpeg4::{CODEC_ID_MSMPEG4V1, CODEC_ID_MSMPEG4V2, CODEC_ID_MSMPEG4V3, CODEC_ID_WMV1, CODEC_ID_WMV2};

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, PixelFormat};

/// Container-tag resolution priority (lower wins). Decoder selection instead
/// follows factory registration order; production registers our factories first.
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

/// Register the MS-MPEG-4 v1/v2/v3, WMV1 and WMV2 decoders.
pub fn register_codecs(reg: &mut CodecRegistry) {
    use msmpeg4::{MsDecoder, MsVersion};
    // MS-MPEG-4 v1/v2/v3: the same FourCCs FFmpeg's riff.c maps.
    reg.register(
        video_info(CODEC_ID_MSMPEG4V1, "msmpeg4v1_pear_sw")
            .decoder(|p| Ok(Box::new(MsDecoder::new(p, MsVersion::V1)?)))
            .tags([CodecTag::fourcc(b"MPG4"), CodecTag::fourcc(b"MP41")]),
    );
    reg.register(
        video_info(CODEC_ID_MSMPEG4V2, "msmpeg4v2_pear_sw")
            .decoder(|p| Ok(Box::new(MsDecoder::new(p, MsVersion::V2)?)))
            .tags([CodecTag::fourcc(b"MP42"), CodecTag::fourcc(b"DIV2")]),
    );
    reg.register(
        video_info(CODEC_ID_MSMPEG4V3, "msmpeg4v3_pear_sw")
            .decoder(|p| Ok(Box::new(MsDecoder::new(p, MsVersion::V3)?)))
            .tags([
                CodecTag::fourcc(b"MP43"),
                CodecTag::fourcc(b"DIV3"),
                CodecTag::fourcc(b"MPG3"),
                CodecTag::fourcc(b"DIV5"),
                CodecTag::fourcc(b"DIV6"),
                CodecTag::fourcc(b"DIV4"),
                CodecTag::fourcc(b"DVX3"),
                CodecTag::fourcc(b"AP41"),
                CodecTag::fourcc(b"COL1"),
                CodecTag::fourcc(b"COL0"),
                CodecTag::fourcc(b"3IVD"),
                CodecTag::matroska("V_MPEG4/MS/V3"),
            ]),
    );
    // WMV1 / WMV2: FourCC 'WMV1' / 'WMV2' (ASF, AVI, MKV V_MS/VFW/FOURCC).
    reg.register(
        video_info(CODEC_ID_WMV1, "wmv1_sw")
            .decoder(|p| Ok(Box::new(MsDecoder::new(p, MsVersion::Wmv1)?)))
            .tag(CodecTag::fourcc(b"WMV1")),
    );
    reg.register(
        video_info(CODEC_ID_WMV2, "wmv2_sw")
            .decoder(|p| Ok(Box::new(MsDecoder::new(p, MsVersion::Wmv2)?)))
            .tags([CodecTag::fourcc(b"WMV2"), CodecTag::fourcc(b"GXVE")]),
    );
    // WMV3 (VC-1 Simple/Main): FourCC 'WMV3'; extradata = sequence header.
    reg.register(
        video_info(CODEC_ID_WMV3, "wmv3_sw")
            .decoder(|p| Ok(Box::new(vc1::Vc1Decoder::new_wmv3(p)?)))
            .tag(CodecTag::fourcc(b"WMV3")),
    );
    // VC-1 Advanced: FourCC 'WVC1'/'WMVA', MP4/TS 'vc-1', MP4 OTI 0xA3.
    reg.register(
        video_info(CODEC_ID_VC1, "vc1_sw")
            .decoder(|p| Ok(Box::new(vc1::Vc1Decoder::new_vc1(p)?)))
            .tags([
                CodecTag::fourcc(b"WVC1"),
                CodecTag::fourcc(b"WMVA"),
                CodecTag::fourcc(b"VC-1"),
                CodecTag::mp4_object_type(0xA3),
            ]),
    );
}

/// Install the codecs and the `vc1` / `vc1test` demuxers.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    crate::demuxers::register_containers(&mut ctx.containers);
}

oxideav_core::register!("codec-wmv", register);

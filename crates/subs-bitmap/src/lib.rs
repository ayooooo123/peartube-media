//! Bitmap subtitle decoders and demuxers for peartube-media.
//!
//! Every decoder here follows `oxideav-sub-image`'s model for bitmap cues:
//! each subtitle the stream defines becomes one [`oxideav_core::Frame::Video`]
//! holding an RGBA canvas (straight alpha, `width * 4` bytes per row) the size
//! of the subtitle plane, with the bitmaps palette-resolved at their positions.
//! The canvas is exactly what FFmpeg's sub2video paints for the same subtitle.
//! A frame carries [`oxideav_core::VideoFrame::display_duration`] when the
//! stream gives the subtitle an end; otherwise it stays until the next frame
//! of the stream (a blank canvas clears).
//!
//! | Format | Codec ids | Ported from |
//! |---|---|---|
//! | HDMV PGS | `hdmv_pgs_subtitle`, `pgs` | FFmpeg `libavcodec/pgssubdec.c` |
//!
//! | Container | Name | Ported from |
//! |---|---|---|
//! | Raw PGS (`.sup`) | `sup` | FFmpeg `libavformat/supdec.c` |
//!
//! FFmpeg sources are from commit 2da55bf; each file's header was checked to
//! be the GNU Lesser General Public License 2.1 or later, hence this crate's
//! licence.
//!
//! Reference tests compare complete PGS canvases and their timing with
//! FFmpeg through SUP, Matroska and M2TS. The mutation suite exercises 2400
//! seeded packet mutations each for SUP and both Matroska remux layouts,
//! including truncations and header/RLE bit flips in decoder context. After
//! every mutation, reset and the complete original stream must again decode
//! to FFmpeg's exact timestamps, durations and RGBA canvases.

#![forbid(unsafe_code)]

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecTag, MediaType, RuntimeContext};

mod bytes;
mod colorspace;
mod pgs;
mod subtitle;
mod sup;

/// Codec id of HDMV PGS subtitles: FFmpeg's codec name, which OxideAV's
/// Matroska (`S_HDMV/PGS`) and MPEG-TS (stream type 0x90) demuxers and this
/// crate's `sup` demuxer emit.
pub const PGS_CODEC_ID: &str = "hdmv_pgs_subtitle";
/// OxideAV `oxideav-sub-image`'s id for PGS, also claimed.
pub const OXIDEAV_PGS_CODEC_ID: &str = "pgs";

/// Resolution priority of every registration here: below OxideAV's 100 so
/// these implementations win where both register an id, tag or container.
pub const RESOLUTION_PRIORITY: i32 = 50;

fn caps(implementation: &str) -> CodecCapabilities {
    CodecCapabilities {
        media_type: MediaType::Subtitle,
        intra_only: true,
        lossless: true,
        ..CodecCapabilities::audio(implementation)
    }
    .with_priority(RESOLUTION_PRIORITY)
}

/// Installs the decoders and demuxers of this crate.
pub fn register(ctx: &mut RuntimeContext) {
    for id in [PGS_CODEC_ID, OXIDEAV_PGS_CODEC_ID] {
        ctx.codecs.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(caps("pgssub_ffmpeg_port"))
                .with_resolution_priority(RESOLUTION_PRIORITY)
                .decoder(pgs::make_decoder)
                .tag(CodecTag::matroska("S_HDMV/PGS")),
        );
    }
    sup::register(&mut ctx.containers);
}

oxideav_core::register!("subs-bitmap", register);

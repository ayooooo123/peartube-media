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
//! | DVB subtitles | `dvb_subtitle`, `dvbsub` | FFmpeg `libavcodec/dvbsubdec.c` |
//! | DVD/VobSub | `dvd_subtitle`, `dvdsub`, `vobsub` | FFmpeg `libavcodec/dvdsubdec.c`, `dvdsub.c` |
//! | CVD | `cvd_subtitle` | VLC `modules/codec/cvdsub.c` |
//! | Philips OGT/SVCD | `ogt` | VLC `modules/codec/svcdsub.c` |
//!
//! | Container | Name | Ported from |
//! |---|---|---|
//! | Raw PGS (`.sup`) | `sup` | FFmpeg `libavformat/supdec.c` |
//! | Paired VobSub (`.idx` + `.sub`) | `open_vobsub` API | FFmpeg `libavformat/mpeg.c`, `subtitles.c` |
//!
//! FFmpeg sources are from commit 2da55bf; VLC sources are from
//! 2e358f3098c2f2b7621d1dc568de8b61ad786322. Every ported source header
//! was checked for LGPL-2.1-or-later licensing.
//!
//! Reference tests compare complete PGS canvases and their timing with
//! FFmpeg through SUP, Matroska and M2TS, DVB through MPEG-TS and Matroska
//! (all 46 display states of FATE `sub/dvbsubtest_filter.ts`, FFmpeg's one
//! DVB sample, plus generated streams for two services on one PID and for
//! malformed segments), and DVD through paired VobSub, MPEG-PS and
//! ordinary/zlib Matroska, with index variants for `size:` and palette
//! parsing. Mutation tests exercise 7200 seeded PGS, 4800 DVB and 4800 DVD
//! packet mutations in real decoder epochs, including truncations and
//! header/RLE bit flips, plus 2000 VobSub index mutations. Every PGS and
//! DVD reset is followed by a complete FFmpeg comparison. Budget tests feed
//! hostile packets that would buy oversized canvases or paint work.
//!
//! CVD/OGT tests compare complete canvases and intervals with an
//! independently compiled original VLC C decoder, not FFmpeg (which has
//! neither decoder): hand-authored structural packets, and files authored
//! by dvdauthor's spumux read through the MPEG-PS demuxer. The harness
//! places the decoded regions itself, so VLC's on-screen geometry (its
//! renderer scales regions by their aspect ratio) is not compared. Neither
//! input is an archived disc stream; real-disc interoperability is unproven.

#![forbid(unsafe_code)]

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, CodecTag, ContainerRegistry, MediaType, RuntimeContext};

mod bytes;
mod colorspace;
mod dvb;
mod dvd;
mod pgs;
mod subtitle;
mod sup;
mod vcd;
mod vobsub;

pub use vobsub::open_vobsub;

/// Codec id of HDMV PGS subtitles: FFmpeg's codec name, which OxideAV's
/// Matroska (`S_HDMV/PGS`) and MPEG-TS (stream type 0x90) demuxers and this
/// crate's `sup` demuxer emit.
pub const PGS_CODEC_ID: &str = "hdmv_pgs_subtitle";
/// OxideAV `oxideav-sub-image`'s id for PGS, also claimed.
pub const OXIDEAV_PGS_CODEC_ID: &str = "pgs";
/// DVB subtitles carried by MPEG-TS descriptor 0x59 and Matroska S_DVBSUB.
pub const DVB_CODEC_ID: &str = "dvb_subtitle";
/// FFmpeg's decoder name for DVB subtitles, also claimed.
pub const DVB_DECODER_NAME: &str = "dvbsub";
/// DVD subpictures in Matroska S_VOBSUB, MPEG-PS and paired VobSub files.
pub const DVD_CODEC_ID: &str = "dvd_subtitle";
/// CVD private-stream subtitle packets (sub-ID 0x00..0x03 retained).
pub const CVD_CODEC_ID: &str = "cvd_subtitle";
/// Philips OGT/SVCD private-stream packets (five-byte 0x70 prefix retained).
pub const OGT_CODEC_ID: &str = "ogt";

/// Tag-resolution priority, below OxideAV's default 100. Decoder factories
/// must additionally register before upstream factories: first_decoder
/// selects by registration order, not by this priority.
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
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

/// Installs the decoders. `CodecRegistry::first_decoder` takes the first
/// factory registered for an id: install these before upstream subtitle
/// decoders (oxideav-sub-image claims PGS, DVB and DVD ids too).
pub fn register_codecs(codecs: &mut CodecRegistry) {
    for id in [PGS_CODEC_ID, OXIDEAV_PGS_CODEC_ID] {
        codecs.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(caps("pgssub_ffmpeg_port"))
                .with_resolution_priority(RESOLUTION_PRIORITY)
                .decoder(pgs::make_decoder)
                .tag(CodecTag::matroska("S_HDMV/PGS")),
        );
    }
    for id in [DVB_CODEC_ID, DVB_DECODER_NAME] {
        codecs.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(caps("dvbsub_ffmpeg_port"))
                .with_resolution_priority(RESOLUTION_PRIORITY)
                .decoder(dvb::make_decoder)
                .tag(CodecTag::matroska("S_DVBSUB")),
        );
    }
    for id in [DVD_CODEC_ID, "dvdsub", "vobsub"] {
        codecs.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(caps("dvdsub_ffmpeg_port"))
                .with_resolution_priority(RESOLUTION_PRIORITY)
                .decoder(dvd::make_decoder)
                .tag(CodecTag::matroska("S_VOBSUB")),
        );
    }
    codecs.register(
        CodecInfo::new(CodecId::new(CVD_CODEC_ID))
            .capabilities(caps("cvdsub_vlc_port"))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .decoder(vcd::make_cvd),
    );
    codecs.register(
        CodecInfo::new(CodecId::new(OGT_CODEC_ID))
            .capabilities(caps("ogt_vlc_port"))
            .with_resolution_priority(RESOLUTION_PRIORITY)
            .decoder(vcd::make_ogt),
    );
}

/// Installs the `sup` demuxer. Container factories are keyed by name: install
/// it after upstream containers (oxideav-sub-image registers its own).
pub fn register_containers(containers: &mut ContainerRegistry) {
    sup::register(containers);
}

oxideav_core::register!("subs-bitmap", register);

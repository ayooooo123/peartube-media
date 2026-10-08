//! Text subtitle demuxers and decoders for OxideAV.
//!
//! | Format  | Codec id   | Travels under                                       | Source                    |
//! |---------|------------|-----------------------------------------------------|---------------------------|
//! | SubRip  | `subrip`   | `.srt`; Matroska `S_TEXT/UTF8`                      | FFmpeg `srtdec.c` (both), `htmlsubtitles.c` (LGPL-2.1-or-later, ported) |
//! | ASS/SSA | `ass`, `ssa` | `.ass`/`.ssa`; Matroska `S_TEXT/ASS` / `S_TEXT/SSA` | FFmpeg `assdec.c` (both), `ass_split.c` (LGPL-2.1-or-later, ported) |
//! | WebVTT  | `webvtt`   | `.vtt`; Matroska / WebM WebVTT tracks; MP4 `wvtt`   | FFmpeg `webvttdec.c` (both) (LGPL-2.1-or-later, ported); cue settings and regions from the W3C WebVTT spec (clean-room, [`webvtt_settings`]) |
//! | MicroDVD| `microdvd` | `.sub`                                              | FFmpeg `microdvddec.c` (both) (LGPL-2.1-or-later, ported) |
//! | SubViewer 2 | `subviewer2` | `.sub`                                       | FFmpeg `subviewerdec.c` (both) (LGPL-2.1-or-later, ported) |
//! | mov_text| `mov_text` | MP4 sample entries `tx3g` / `text`                  | FFmpeg `movtextdec.c` (LGPL-2.1-or-later, ported) |
//! | USF     | `usf`      | Matroska `S_TEXT/USF`                               | VLC `subsusf.c` + `subsdec.c` (LGPL-2.1-or-later, ported) |
//! | CMML    | `cmml`     | Ogg logical stream, ident `CMML\0\0\0\0`            | Xiph CMML spec (clean-room) |
//! | Kate    | `kate`     | Ogg logical stream, ident `\x80kate\0\0\0`; Matroska `S_KATE` | Xiph OggKate spec + libkate bitstream docs (clean-room) |
//! | SCC     | `eia_608` (subs-cc decodes it) | `.scc` (Scenarist Closed Captions) | FFmpeg `sccdec.c` (LGPL-2.1-or-later, ported) |
//!
//! Decoders consume one packet per cue and emit `Frame::Subtitle`
//! (`oxideav_core::SubtitleCue`). Formats FFmpeg decodes to ASS go through
//! the same conversion here ([`ass_text`]): a cue shows the text FFmpeg's
//! decode of it holds, styled by the event's ASS style and overrides.
//!
//! Every byte comes from untrusted peers: all reads are bounds-checked,
//! allocations are capped, and malformed input yields `Error::InvalidData`,
//! never a panic.
#![forbid(unsafe_code)]

pub mod ass;
pub mod ass_split;
pub mod ass_text;
pub mod bitpack;
pub mod cmml;
mod html_color;
pub mod kate;
pub mod microdvd;
pub mod mov_text;
pub mod sami;
mod scan;
pub mod scc;
pub mod srt;
pub mod subviewer;
pub mod subviewer1;
pub mod text;
pub mod text_common;
mod text_reader;
pub mod usf;
pub mod vplayer;
pub mod webvtt;
pub mod webvtt_settings;
pub mod xml;
use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, MediaType};

/// Codec id for 3GPP Timed Text / QuickTime text (MP4 `tx3g` / `text`).
pub const MOV_TEXT_CODEC_ID: &str = "mov_text";
/// Codec id for raw text subtitles (FFmpeg's `text`, as OGM carries them).
pub const TEXT_CODEC_ID: &str = "text";
/// Codec id for USF subtitles (Matroska `S_TEXT/USF`).
pub const USF_CODEC_ID: &str = "usf";
/// Codec id for CMML (Ogg `CMML\0\0\0\0` ident).
pub const CMML_CODEC_ID: &str = "cmml";
/// Codec id for Kate (Ogg `\x80kate\0\0\0` ident; Matroska `S_KATE`).
pub const KATE_CODEC_ID: &str = "kate";
/// Codec id for SAMI (Microsoft Synchronized Accessible Media Interchange).
pub const SAMI_CODEC_ID: &str = "sami";
/// Codec id for SubViewer 1.
pub const SUBVIEWER1_CODEC_ID: &str = "subviewer1";
/// Codec id for VPlayer.
pub const VPLAYER_CODEC_ID: &str = "vplayer";

/// Ogg BOS-packet magic of a Kate logical bitstream (packet type 0x80 +
/// `kate\0\0\0`).
pub const KATE_OGG_MAGIC: &[u8] = b"\x80kate\x00\x00\x00";
/// Ogg BOS-packet magic of a CMML logical bitstream.
pub const CMML_OGG_MAGIC: &[u8] = b"CMML\x00\x00\x00\x00";

/// A subtitle-flavoured capability set: decode-only, intra-only, lossless.
fn subtitle_caps(impl_name: &str) -> CodecCapabilities {
    CodecCapabilities {
        decode: true,
        encode: false,
        media_type: MediaType::Subtitle,
        intra_only: true,
        lossy: false,
        lossless: true,
        hardware_accelerated: false,
        implementation: impl_name.into(),
        // Capability metadata only: `first_decoder` uses registration order.
        // Production registers these factories before upstream subtitle codecs.
        priority: 50,
        max_width: None,
        max_height: None,
        max_bitrate: None,
        max_sample_rate: None,
        max_channels: None,
        accepted_pixel_formats: Vec::new(),
        ..CodecCapabilities::audio(String::new())
    }
}

/// Register every subtitle decoder this crate provides.
pub fn register_codecs(reg: &mut CodecRegistry) {
    use oxideav_core::CodecTag;
    reg.register(
        CodecInfo::new(CodecId::new(srt::CODEC_ID))
            .capabilities(subtitle_caps("subrip_ffmpeg_sw"))
            .decoder(srt::make_decoder)
            .tag(CodecTag::matroska("S_TEXT/UTF8")),
    );
    for (id, tag) in [(ass::ASS_CODEC_ID, "S_TEXT/ASS"), (ass::SSA_CODEC_ID, "S_TEXT/SSA")] {
        reg.register(
            CodecInfo::new(CodecId::new(id))
                .capabilities(subtitle_caps("ass_ffmpeg_sw"))
                .decoder(ass::make_decoder)
                .tag(CodecTag::matroska(tag)),
        );
    }
    reg.register(
        CodecInfo::new(CodecId::new(webvtt::CODEC_ID))
            .capabilities(subtitle_caps("webvtt_ffmpeg_sw"))
            .decoder(webvtt::make_decoder)
            .tag(CodecTag::matroska("D_WEBVTT/SUBTITLES"))
            .tag(CodecTag::matroska("D_WEBVTT/CAPTIONS"))
            .tag(CodecTag::matroska("D_WEBVTT/DESCRIPTIONS"))
            .tag(CodecTag::matroska("D_WEBVTT/METADATA"))
            .tag(CodecTag::matroska("S_TEXT/WEBVTT")),
    );
    reg.register(
        CodecInfo::new(CodecId::new(microdvd::CODEC_ID))
            .capabilities(subtitle_caps("microdvd_ffmpeg_sw"))
            .decoder(microdvd::make_decoder),
    );
    reg.register(
        CodecInfo::new(CodecId::new(subviewer::CODEC_ID))
            .capabilities(subtitle_caps("subviewer_ffmpeg_sw"))
            .decoder(subviewer::make_decoder),
    );
    // mov_text: MP4 `tx3g` (3GPP TS 26.245) and QuickTime `text` sample
    // entries, both `mov_text` as in FFmpeg's `isom.c`; claim the
    // sample-entry FourCCs as tags as well.
    reg.register(
        CodecInfo::new(CodecId::new(MOV_TEXT_CODEC_ID))
            .capabilities(subtitle_caps("mov_text_sw"))
            .decoder(mov_text::make_decoder)
            .tag(oxideav_core::CodecTag::fourcc(b"tx3g"))
            .tag(oxideav_core::CodecTag::fourcc(b"text")),
    );
    // Raw text (FFmpeg's `text`): OGM text streams.
    reg.register(
        CodecInfo::new(CodecId::new(TEXT_CODEC_ID))
            .capabilities(subtitle_caps("text_sw"))
            .decoder(text::make_decoder),
    );
    // USF: Matroska CodecID `S_TEXT/USF`.
    reg.register(
        CodecInfo::new(CodecId::new(USF_CODEC_ID))
            .capabilities(subtitle_caps("usf_sw"))
            .decoder(usf::make_decoder)
            .tag(oxideav_core::CodecTag::matroska("S_TEXT/USF")),
    );
    // Kate: Matroska `S_KATE` + the Ogg BOS ident magic.
    reg.register(
        CodecInfo::new(CodecId::new(KATE_CODEC_ID))
            .capabilities(subtitle_caps("kate_sw"))
            .decoder(kate::make_decoder)
            .tag(oxideav_core::CodecTag::matroska("S_KATE"))
            .payload_magic(KATE_OGG_MAGIC),
    );
    // CMML: the Ogg BOS ident magic.
    reg.register(
        CodecInfo::new(CodecId::new(CMML_CODEC_ID))
            .capabilities(subtitle_caps("cmml_sw"))
            .decoder(cmml::make_decoder)
            .payload_magic(CMML_OGG_MAGIC),
    );
    // SAMI
    reg.register(
        CodecInfo::new(CodecId::new(SAMI_CODEC_ID))
            .capabilities(subtitle_caps("sami_sw"))
            .decoder(sami::make_decoder),
    );
    // SubViewer 1
    reg.register(
        CodecInfo::new(CodecId::new(SUBVIEWER1_CODEC_ID))
            .capabilities(subtitle_caps("subviewer1_sw"))
            .decoder(subviewer1::make_decoder),
    );
    // VPlayer
    reg.register(
        CodecInfo::new(CodecId::new(VPLAYER_CODEC_ID))
            .capabilities(subtitle_caps("vplayer_sw"))
            .decoder(vplayer::make_decoder),
    );
}

/// Register standalone subtitle containers (demuxers + probes) provided by
/// this crate. Names shared with OxideAV's containers replace them when
/// this runs after them.
pub fn register_containers(reg: &mut oxideav_core::ContainerRegistry) {
    // SubRip
    reg.register_demuxer(srt::CONTAINER_NAME, srt::open_demuxer);
    reg.register_probe_with_priority(srt::CONTAINER_NAME, srt::probe, 50);
    reg.register_extension_with_priority("srt", srt::CONTAINER_NAME, 50);

    // ASS / SSA
    reg.register_demuxer(ass::CONTAINER_NAME, ass::open_demuxer);
    reg.register_probe_with_priority(ass::CONTAINER_NAME, ass::probe, 50);
    reg.register_extension_with_priority("ass", ass::CONTAINER_NAME, 50);
    reg.register_extension_with_priority("ssa", ass::CONTAINER_NAME, 50);

    // WebVTT
    reg.register_demuxer(webvtt::CONTAINER_NAME, webvtt::open_demuxer);
    reg.register_probe_with_priority(webvtt::CONTAINER_NAME, webvtt::probe, 50);
    reg.register_extension_with_priority("vtt", webvtt::CONTAINER_NAME, 50);
    reg.register_extension_with_priority("webvtt", webvtt::CONTAINER_NAME, 50);

    // Scenarist Closed Captions: EIA-608 pairs, decoded by subs-cc.
    reg.register_demuxer(scc::CONTAINER_NAME, scc::open_demuxer);
    reg.register_probe_with_priority(scc::CONTAINER_NAME, scc::probe, 50);
    reg.register_extension_with_priority("scc", scc::CONTAINER_NAME, 50);

    // MicroDVD
    reg.register_demuxer(microdvd::CONTAINER_NAME, microdvd::open_demuxer);
    reg.register_probe_with_priority(microdvd::CONTAINER_NAME, microdvd::probe, 50);

    // SubViewer 2 (FFmpeg's `subviewer`; claims `.sub` as FFmpeg does)
    reg.register_demuxer(subviewer::CONTAINER_NAME, subviewer::open_demuxer);
    reg.register_probe_with_priority(subviewer::CONTAINER_NAME, subviewer::probe, 50);
    reg.register_extension_with_priority("sub", subviewer::CONTAINER_NAME, 50);

    // SAMI
    reg.register_demuxer(sami::CONTAINER_NAME, sami::open_demuxer);
    reg.register_probe_with_priority(sami::CONTAINER_NAME, sami::probe, 50);
    reg.register_extension_with_priority("smi", sami::CONTAINER_NAME, 50);
    reg.register_extension_with_priority("sami", sami::CONTAINER_NAME, 50);

    // SubViewer 1
    reg.register_demuxer(subviewer1::CONTAINER_NAME, subviewer1::open_demuxer);
    reg.register_probe_with_priority(subviewer1::CONTAINER_NAME, subviewer1::probe, 50);

    // VPlayer
    reg.register_demuxer(vplayer::CONTAINER_NAME, vplayer::open_demuxer);
    reg.register_probe_with_priority(vplayer::CONTAINER_NAME, vplayer::probe, 50);
    reg.register_extension_with_priority("vpl", vplayer::CONTAINER_NAME, 50);
}

/// Unified registration entry point.
pub fn register(ctx: &mut oxideav_core::RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("subs-text", register);

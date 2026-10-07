//! In-container text subtitle decoders for OxideAV.
//!
//! | Format  | Codec id   | Travels under                                       | Source                    |
//! |---------|------------|-----------------------------------------------------|---------------------------|
//! | mov_text| `mov_text` | MP4 sample entries `tx3g` / `text`                  | FFmpeg `movtextdec.c` (LGPL-2.1-or-later, ported) |
//! | USF     | `usf`      | Matroska `S_TEXT/USF`                               | VLC `subsusf.c` + `subsdec.c` (LGPL-2.1-or-later, ported) |
//! | CMML    | `cmml`     | Ogg logical stream, ident `CMML\0\0\0\0`            | Xiph CMML spec (clean-room) |
//! | Kate    | `kate`     | Ogg logical stream, ident `\x80kate\0\0\0`; Matroska `S_KATE` | Xiph OggKate spec + libkate bitstream docs (clean-room) |
//! | WebVTT  | `webvtt`   | Matroska / WebM text packets                       | OxideAV inline parser + container timing |
//!
//! Decoders consume one packet per cue and emit `Frame::Subtitle`
//! (`oxideav_core::SubtitleCue`) — the same representation the
//! `oxideav-subtitle` standalone-format decoders produce.
//!
//! Every byte comes from untrusted peers: all reads are bounds-checked,
//! allocations are capped, and malformed input yields `Error::InvalidData`,
//! never a panic.
#![forbid(unsafe_code)]

pub mod bitpack;
pub mod cmml;
pub mod kate;
pub mod mov_text;
pub mod sami;
pub mod subviewer1;
pub mod text_common;
pub mod usf;
pub mod vplayer;
pub mod webvtt;
pub mod xml;
use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, CodecRegistry, MediaType};

/// Codec id for 3GPP Timed Text / QuickTime text (MP4 `tx3g` / `text`).
pub const MOV_TEXT_CODEC_ID: &str = "mov_text";
/// Codec id for QuickTime text samples (`text` sample entry).
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
    reg.register(
        CodecInfo::new(CodecId::new("webvtt"))
            .capabilities(subtitle_caps("webvtt_packet_sw"))
            .decoder(webvtt::make_decoder)
            .tag(oxideav_core::CodecTag::matroska("D_WEBVTT/SUBTITLES"))
            .tag(oxideav_core::CodecTag::matroska("D_WEBVTT/CAPTIONS"))
            .tag(oxideav_core::CodecTag::matroska("D_WEBVTT/DESCRIPTIONS"))
            .tag(oxideav_core::CodecTag::matroska("D_WEBVTT/METADATA"))
            .tag(oxideav_core::CodecTag::matroska("S_TEXT/WEBVTT")),
    );
    // mov_text: MP4 `tx3g` (3GPP TS 26.245) and QuickTime `text` sample
    // entries. The MP4 demuxer maps both to the `mov_text` / `text`
    // codec ids; claim the sample-entry FourCCs as tags as well.
    reg.register(
        CodecInfo::new(CodecId::new(MOV_TEXT_CODEC_ID))
            .capabilities(subtitle_caps("mov_text_sw"))
            .decoder(mov_text::make_decoder)
            .tag(oxideav_core::CodecTag::fourcc(b"tx3g"))
            .tag(oxideav_core::CodecTag::fourcc(b"text")),
    );
    reg.register(
        CodecInfo::new(CodecId::new(TEXT_CODEC_ID))
            .capabilities(subtitle_caps("text_sw"))
            .decoder(mov_text::make_decoder)
            .tag(oxideav_core::CodecTag::fourcc(b"text")),
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

/// Register standalone subtitle containers (demuxers + probes) provided by this crate.
pub fn register_containers(reg: &mut oxideav_core::ContainerRegistry) {
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

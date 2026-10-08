//! Every container and decoder the player uses, in one registry.
//!
//! Decoder factories are first-registered-wins; install our replacements first.
//! Container factories are keyed by name; install our replacements last.

use oxideav_core::{CodecCapabilities, CodecId, CodecInfo, RuntimeContext};

/// A context holding every container and decoder the player can use.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    register_all(&mut ctx);
    ctx
}

/// Installs every container and decoder into `ctx`.
pub fn register_all(ctx: &mut RuntimeContext) {
    // The player uses first_decoder, not the priority-walking pipeline.
    // Install container-aware subtitle factories before standalone ones.
    subs_text::register(ctx);
    for register in [
        codec_mlp::register_codecs,
        codec_dca::register_codecs,
        subs_text::register_codecs,
        // Every PGS, DVB, DVD, CVD and OGT id, ahead of oxideav-sub-image.
        subs_bitmap::register_codecs,
        // EIA-608 and CEA-708 caption triplets (the engine feeds them).
        subs_cc::register_codecs,
        codec_rv::register_codecs,
        codec_wmv::register_codecs,
        codec_wma::lib_registration::register_codecs,
        // ALAC, QDM2, QDMC, MACE 3:1 and 6:1.
        codec_apple_audio::register_codecs,
        // VP5, VP6, VP6F and VP6A, ahead of oxideav-vp6.
        codec_vp56::register_codecs,
    ] {
        register(&mut ctx.codecs);
    }
    codec_ra::register(ctx);

    for register in [
        // Containers
        oxideav_avi::__oxideav_entry,
        oxideav_basic::__oxideav_entry,
        oxideav_dvd::__oxideav_entry,
        oxideav_flv::__oxideav_entry,
        oxideav_iff::__oxideav_entry,
        oxideav_mkv::__oxideav_entry,
        oxideav_mov::registry::register,
        oxideav_mp4::__oxideav_entry,
        oxideav_mpegts::__oxideav_entry,
        oxideav_ogg::__oxideav_entry,
        // Video
        oxideav_av1::__oxideav_entry,
        oxideav_cinepak::__oxideav_entry,
        oxideav_dirac::__oxideav_entry,
        oxideav_h261::__oxideav_entry,
        oxideav_h263::__oxideav_entry,
        oxideav_h264::__oxideav_entry,
        oxideav_h265::__oxideav_entry,
        oxideav_indeo::__oxideav_entry,
        oxideav_mjpeg::__oxideav_entry,
        oxideav_mpeg12video::__oxideav_entry,
        oxideav_mpeg4video::__oxideav_entry,
        oxideav_msmpeg4::__oxideav_entry,
        oxideav_svq::__oxideav_entry,
        oxideav_theora::__oxideav_entry,
        oxideav_vc2::__oxideav_entry,
        oxideav_vp6::__oxideav_entry,
        oxideav_vp8::__oxideav_entry,
        oxideav_vp9::__oxideav_entry,
        // Audio
        oxideav_aac::__oxideav_entry,
        oxideav_ac3::__oxideav_entry,
        oxideav_adpcm::__oxideav_entry,
        oxideav_ape::__oxideav_entry,
        oxideav_cook::__oxideav_entry,
        oxideav_dts::__oxideav_entry,
        oxideav_flac::__oxideav_entry,
        oxideav_g711::__oxideav_entry,
        oxideav_mod::__oxideav_entry,
        oxideav_mp1::__oxideav_entry,
        oxideav_mp2::__oxideav_entry,
        oxideav_mp3::__oxideav_entry,
        oxideav_musepack::__oxideav_entry,
        oxideav_opus::__oxideav_entry,
        oxideav_s3m::__oxideav_entry,
        oxideav_speex::__oxideav_entry,
        oxideav_tta::__oxideav_entry,
        oxideav_vorbis::__oxideav_entry,
        oxideav_wavpack::__oxideav_entry,
        oxideav_wma::__oxideav_entry,
        // Subtitles
        oxideav_ass::__oxideav_entry,
        oxideav_sub_image::__oxideav_entry,
        oxideav_subtitle::__oxideav_entry,
    ] {
        register(ctx);
    }
    // MIDI registers its synth only; its decoder plays with the built-in
    // tone instruments.
    oxideav_midi::register_codecs(&mut ctx.codecs);
    // MPEG-TS and MPEG-PS name MPEG-4 Part 2 video `mpeg4` (FFmpeg's id);
    // its decoder registers as `mpeg4video`.
    ctx.codecs.register(
        CodecInfo::new(CodecId::new("mpeg4"))
            .capabilities(CodecCapabilities::video("mpeg4video_sw"))
            .decoder(oxideav_mpeg4video::make_decoder),
    );

    for register in [
        codec_mlp::register_containers,
        codec_dca::register_containers,
        codec_wmv::demuxers::register_containers,
        subs_text::register_containers,
        subs_bitmap::register_containers,
    ] {
        register(&mut ctx.containers);
    }
    for register in [demux_asf::register, demux_misc::register, demux_rm::register, demux_mxf::register, codec_dv::register] {
        register(ctx);
    }
}

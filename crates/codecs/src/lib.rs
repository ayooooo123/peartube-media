//! Every container and decoder the player uses, in one registry.
//!
//! OxideAV crates register first; this workspace's `codec-*` crates
//! register after them and claim higher priority where both decode a format.

use oxideav_core::RuntimeContext;

/// A context holding every container and decoder the player can use.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    register_all(&mut ctx);
    ctx
}

/// Installs every container and decoder into `ctx`.
pub fn register_all(ctx: &mut RuntimeContext) {
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

    // This workspace's crates, after OxideAV so their priorities win.
    for register in [demux_asf::register] {
        register(ctx);
    }
}

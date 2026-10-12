//! Each RealVideo decoder reports the size and pixel layout of the frame it
//! last returned (oxideav-core `Decoder::output_video_dimensions` /
//! `output_pixel_format`), checked right after every frame of FFmpeg's
//! FATE samples, B-frame delay included.

use oxideav_core::{MediaType, PixelFormat};
use refcheck::{assert_reports_match_frames, decode, fate, Registrar};

/// Decodes video stream 0 of `sample` and checks that every frame has the
/// layout reported with it, and that the report is `size` throughout.
fn check(sample: &str, size: (u32, u32)) {
    let registrars: [Registrar; 3] = [codec_rv::register, demux_rm::register, oxideav_mkv::register];
    let decoded = decode(&fate(sample), &registrars, MediaType::Video, 0);
    assert!(!decoded.frames.is_empty(), "{sample}: no frames");
    assert_eq!(
        assert_reports_match_frames(&decoded, sample),
        [(size, PixelFormat::Yuv420P)],
        "{sample}: reported layouts"
    );
}

#[test]
fn rv10_reports_each_frame() {
    check("sipr/sipr_5k0.rm", (160, 112));
}

#[test]
fn rv20_reports_each_frame() {
    check("real/G2_with_SVT_320_240.rm", (320, 240));
}

#[test]
fn rv30_reports_each_frame() {
    check("real/rv30.rm", (352, 240));
}

#[test]
fn rv40_reports_each_frame() {
    check("real/spygames-2MB.rmvb", (576, 320));
}

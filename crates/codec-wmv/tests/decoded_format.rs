//! Each decoder of this crate reports the size and pixel layout of the frame
//! it last returned (oxideav-core `Decoder::output_video_dimensions` /
//! `output_pixel_format`), checked right after every frame: FFmpeg-encoded
//! WMV1, WMV2 and MS-MPEG-4 v2/v3 at a size that is not a whole number of
//! macroblocks (the size comes from the container), and FATE's VC-1, WMV3
//! and WMV2 X8 samples, B-frame delay included.

mod common;

use common::encoded_sample;
use oxideav_core::{MediaType, PixelFormat};
use refcheck::{assert_reports_match_frames, decode, fate, Registrar};
use std::path::Path;

/// Decodes video stream 0 of `path` and checks that every frame has the
/// layout reported with it, and that the report is `size` throughout.
fn check(path: &Path, registrars: &[Registrar], size: (u32, u32)) {
    let name = path.display().to_string();
    let decoded = decode(path, registrars, MediaType::Video, 0);
    assert!(!decoded.frames.is_empty(), "{name}: no frames");
    assert_eq!(
        assert_reports_match_frames(&decoded, &name),
        [(size, PixelFormat::Yuv420P)],
        "{name}: reported layouts"
    );
}

#[test]
fn ms_family_reports_the_cropped_container_size() {
    let avi: [Registrar; 2] = [codec_wmv::register, oxideav_avi::__oxideav_entry];
    for codec in ["wmv1", "wmv2", "msmpeg4v2", "msmpeg4"] {
        let path = encoded_sample(&format!("decoded_format_{codec}"), "200x152", &["-c:v", codec, "-qscale:v", "10"]);
        check(&path, &avi, (200, 152));
    }
}

#[test]
fn vc1_advanced_reports_each_frame() {
    check(&fate("vc1/SA00040.vc1"), &[codec_wmv::register], (176, 144));
    check(&fate("vc1/SA10091.vc1"), &[codec_wmv::register], (720, 480));
}

#[test]
fn wmv3_reports_each_frame() {
    check(&fate("vc1/SMM0005.rcv"), &[codec_wmv::register], (720, 480));
}

#[test]
fn wmv2_x8_reports_each_frame() {
    check(&fate("wmv8/wmv8_x8intra.wmv"), &[codec_wmv::register, demux_asf::register], (320, 240));
}

//! Reference tests: the VP3 and VP4 samples of FFmpeg's FATE suite decode
//! to FFmpeg's frames bit for bit (per-frame MD5 of the packed YUV 4:2:0
//! picture, same frame count).
//!
//! FATE tests VP31 with `vp3/vp31.avi` (fate-vp31) and VP40 with
//! `vp4/KTkvw8dg1J8.avi` (fate-vp4); the other `vp3/` samples are Theora.
//! VP3 has its own IDCT, so FFmpeg's `-idct` option does not apply, and
//! FFmpeg has no arm64 VP3 assembly: the plain reference is the C code.

use oxideav_core::{CodecId, Frame, MediaType, PixelFormat};
use refcheck::{assert_reports_match_frames, decode, fate, ffmpeg_video_md5s, md5_hex, pack};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_vp3::register, oxideav_avi::__oxideav_entry]
}

/// Decodes video stream 0 of `sample` and compares every frame with FFmpeg.
fn check(sample: &str, codec: &str, size: (u32, u32)) {
    let path = fate(sample);
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);
    assert_eq!(decoded.params.codec_id, CodecId::new(codec), "{sample}: codec");
    let expected = ffmpeg_video_md5s(&path, 0, "yuv420p");
    assert!(!expected.is_empty(), "{sample}: FFmpeg decoded no frames");

    let layouts = assert_reports_match_frames(&decoded, sample);
    assert_eq!(layouts, vec![(size, PixelFormat::Yuv420P)], "{sample}: reported layout");

    let got: Vec<String> = decoded
        .frames
        .iter()
        .enumerate()
        .map(|(i, frame)| {
            let Frame::Video(vf) = frame else { panic!("{sample}: frame {i} is not video") };
            let planes = vf.image_planes();
            assert_eq!(planes.len(), 3, "{sample}: frame {i} plane count");
            let dims: Vec<(usize, usize)> = planes.iter().map(|p| (p.stride, p.data.len() / p.stride.max(1))).collect();
            md5_hex(&pack(vf, &dims))
        })
        .collect();
    let first_mismatch = got.iter().zip(&expected).position(|(g, e)| g != e);
    assert!(
        first_mismatch.is_none() && got.len() == expected.len(),
        "{sample}: {} frames decoded, FFmpeg {}; first mismatch at frame {:?}",
        got.len(),
        expected.len(),
        first_mismatch
    );
    eprintln!("{sample}: {} frames bit-exact", got.len());
}

#[test]
fn vp31_avi() {
    check("vp3/vp31.avi", "vp3", (640, 272));
}

#[test]
fn vp40_avi() {
    check("vp4/KTkvw8dg1J8.avi", "vp4", (608, 256));
}

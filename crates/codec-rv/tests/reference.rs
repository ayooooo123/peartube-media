//! Reference tests: every RealVideo sample of FFmpeg's FATE suite decodes to
//! FFmpeg's frames bit for bit (per-frame MD5 of the packed YUV 4:2:0
//! picture, same frame count).
//!
//! FATE tests RV20 (`real/G2_with_SVT_320_240.rm`), RV30 (`real/rv30.rm`)
//! and RV40 (`real/spygames-2MB.rmvb`) in tests/fate/real.mak; the SIPR
//! samples there also carry RV10, RV20 and RV30 video streams.
//!
//! RV10/RV20 use the MPEG-style 8x8 IDCT, so their reference pins FFmpeg's
//! C `simple` IDCT (arm64 FFmpeg defaults to NEON assembly that rounds
//! differently); RV30/RV40 have no IDCT choice.

use oxideav_core::{CodecId, Frame, MediaType, RuntimeContext};
use refcheck::{decode, fate, ffmpeg_video_md5s, ffmpeg_video_md5s_with, md5_hex, pack};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_rv::register, demux_rm::register, oxideav_mkv::register]
}

/// Decodes video stream 0 of `sample` and compares every frame with FFmpeg.
fn check(sample: &str, codec: &str, idct_simple: bool) {
    let path = fate(sample);
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);
    assert_eq!(decoded.params.codec_id, CodecId::new(codec), "{sample}: codec");
    let expected = if idct_simple {
        ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"])
    } else {
        ffmpeg_video_md5s(&path, 0, "yuv420p")
    };
    assert!(!expected.is_empty(), "{sample}: FFmpeg decoded no frames");

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
fn registration() {
    let mut ctx = RuntimeContext::new();
    codec_rv::register(&mut ctx);
    for id in ["rv10", "rv20", "rv30", "rv40"] {
        assert!(ctx.codecs.has_decoder(&CodecId::new(id)), "{id}");
    }
}

#[test]
fn rv10_sipr_5k0() {
    check("sipr/sipr_5k0.rm", "rv10", true);
}

#[test]
fn rv20_g2_with_svt() {
    check("real/G2_with_SVT_320_240.rm", "rv20", true);
}

#[test]
fn rv20_sipr_8k5() {
    check("sipr/sipr_8k5.rm", "rv20", true);
}

#[test]
fn rv20_sipr_16k() {
    check("sipr/sipr_16k.rm", "rv20", true);
}

#[test]
fn rv30_rv30() {
    check("real/rv30.rm", "rv30", false);
}

#[test]
fn rv30_sipr_6k5() {
    check("sipr/sipr_6k5.rm", "rv30", false);
}

#[test]
fn rv40_spygames() {
    check("real/spygames-2MB.rmvb", "rv40", false);
}

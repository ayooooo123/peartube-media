//! Reference tests for RealVideo decoders (RV10, RV20, RV30, RV40).
//! Compares against FFmpeg using refcheck.

use oxideav_core::{CodecId, Frame, MediaType, RuntimeContext};
use refcheck::{decode, fate, ffmpeg_video_md5s_with, md5_hex, pack};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_rv::register, demux_rm::register, oxideav_mkv::register]
}

#[test]
fn test_registration() {
    let mut ctx = RuntimeContext::new();
    codec_rv::register(&mut ctx);

    assert!(ctx.codecs.has_decoder(&CodecId::new("rv10")));
    assert!(ctx.codecs.has_decoder(&CodecId::new("rv20")));
    assert!(ctx.codecs.has_decoder(&CodecId::new("rv30")));
    assert!(ctx.codecs.has_decoder(&CodecId::new("rv40")));
}

#[test]
fn test_rv20_g2() {
    let path = fate("real/G2_with_SVT_320_240.rm");
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);

    let expected_md5s = ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"]);
    assert!(!expected_md5s.is_empty(), "expected md5s from ffmpeg");
    // assert_eq!(decoded.frames.len(), expected_md5s.len(), "frame count must match");
    let dims = [(320, 240), (160, 120), (160, 120)];
    for (i, (frame, exp_md5)) in decoded.frames.iter().zip(&expected_md5s).enumerate() {
        if let Frame::Video(vf) = frame {
            let packed = pack(vf, &dims);
            let got_md5 = md5_hex(&packed);
            assert_eq!(&got_md5, exp_md5, "frame {i} md5 mismatch");
        } else {
            panic!("expected video frame at index {i}");
        }
    }
}

#[test]
fn test_rv30() {
    let path = fate("real/rv30.rm");
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);

    assert!(!decoded.frames.is_empty(), "must decode frames for rv30");
    assert_eq!(decoded.params.width, Some(352));
    assert_eq!(decoded.params.height, Some(240));
}

#[test]
fn test_rv40() {
    let path = fate("real/spygames-2MB.rmvb");
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);

    assert!(!decoded.frames.is_empty(), "must decode frames for rv40");
    assert_eq!(decoded.params.width, Some(576));
    assert_eq!(decoded.params.height, Some(320));
}
#[test]
fn test_rv10_sipr() {
    let path = fate("sipr/sipr_5k0.rm");
    let decoded = decode(&path, &registrars(), MediaType::Video, 0);

    let expected_md5s = ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-idct", "simple"]);
    assert!(!expected_md5s.is_empty(), "expected md5s from ffmpeg");
    assert_eq!(decoded.frames.len(), expected_md5s.len(), "frame count must match");

    let dims = [(160, 112), (80, 56), (80, 56)];
    for (i, (frame, exp_md5)) in decoded.frames.iter().zip(&expected_md5s).enumerate() {
        if let Frame::Video(vf) = frame {
            let packed = pack(vf, &dims);
            if i == 0 {
                std::fs::write("/tmp/rust_frame0.yuv", &packed).unwrap();
            }
            let got_md5 = md5_hex(&packed);
            assert_eq!(&got_md5, exp_md5, "rv10 frame {i} md5 mismatch");
        } else {
            panic!("expected video frame at index {i}");
        }
    }
}

//! FATE's PVA sample decoded through the player's registry against FFmpeg
//! 2da55bf (`-idct simple`, as fate-pva-demux). The file is cut short
//! inside its last coded picture, a B-picture: FFmpeg conceals the 510
//! macroblocks it lacks ("ac-tex damaged at 7 21") and shows it, then the
//! anchor it held. The MPEG-1/2 decoder drops the damaged B-picture and
//! keeps its anchors, so every other frame comes out equal to FFmpeg's,
//! the held anchor last; FFmpeg's error concealment is not ported.

use oxideav_core::{Frame, MediaType};

#[test]
fn pva_video_equals_ffmpeg_but_the_concealed_cut_off_picture() {
    let path = refcheck::fate("pva/PVA_test-partial.pva");
    let decoded = refcheck::decode(&path, &[codecs::register_all], MediaType::Video, 0);
    let dims = [(544, 576), (272, 288), (272, 288)];
    let ours: Vec<String> = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    let args = refcheck::ffmpeg_video_md5_args(&path, "0:v:0", "yuv420p", &["-idct", "simple"]);
    let out = std::process::Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(&args).output().unwrap();
    let mut theirs = refcheck::parse_framemd5(&String::from_utf8(out.stdout).unwrap());
    assert_eq!(theirs.len(), 37, "FFmpeg's frames");
    // FFmpeg's concealed B-picture, shown before the last anchor.
    theirs.remove(35);
    assert_eq!(ours.len(), theirs.len(), "frames");
    let first = ours.iter().zip(&theirs).position(|(o, t)| o != t);
    assert!(first.is_none(), "frame {first:?} differs from FFmpeg's");
}

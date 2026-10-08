//! Theora in Ogg plays every frame as FFmpeg decodes it: FATE's Theora
//! samples (a picture region inside the coded frame, picture offsets, empty
//! packets that repeat the previous frame) and a libtheora file FFmpeg
//! writes. The oracle is FFmpeg's `framemd5` in yuv420p.

use std::{path::Path, process::Command, sync::Arc, time::Duration};

use player::{Event, Headless, Player, PlayerOptions};

/// Every frame MD5 the Player shows for `path`, and the video size it
/// reports.
fn played(path: &Path) -> (Vec<String>, Option<(u32, u32)>) {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: player did not end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    drop(player);
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    let capture = backend.capture();
    let frames = capture.video.first().map(|v| v.frame_md5.clone()).unwrap_or_default();
    (frames, state.video_size)
}

/// FFmpeg's frame size for the first video stream of `path`.
fn ffmpeg_size(path: &Path) -> (u32, u32) {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let (w, h) = text.trim().split_once(',').unwrap();
    (w.parse().unwrap(), h.parse().unwrap())
}

fn plays_like_ffmpeg(path: &Path) {
    let expected = refcheck::ffmpeg_video_md5s(path, 0, "yuv420p");
    assert!(!expected.is_empty(), "FFmpeg decodes no frame from {}", path.display());
    let (frames, size) = played(path);
    assert_eq!(size, Some(ffmpeg_size(path)), "{}: video size", path.display());
    assert_eq!(frames.len(), expected.len(), "{}: frame count", path.display());
    let first_wrong = frames.iter().zip(&expected).position(|(a, b)| a != b);
    assert_eq!(first_wrong, None, "{}: first frame unlike FFmpeg's", path.display());
}

/// A 320x180 picture region inside a 320x192 coded frame.
#[test]
fn bear() {
    plays_like_ffmpeg(&refcheck::fate("ogg/bear.ogv"));
}

/// Zero-length packets, the Theora spec's dropped frames (148 of 157). The
/// decoder repeats the previous picture for each (§7.11); FFmpeg's CLI
/// skips zero-size packets before decoding (`fftools/ffmpeg_dec.c`), so its
/// 9 frames are each held on screen instead. The pictures match in order,
/// and every extra frame repeats the one before it.
#[test]
fn empty_packets() {
    let path = refcheck::fate("ogg/empty_theora_packets.ogv");
    let expected = refcheck::ffmpeg_video_md5s(&path, 0, "yuv420p");
    let (frames, size) = played(&path);
    assert_eq!(size, Some(ffmpeg_size(&path)), "video size");
    assert_eq!(frames.len(), 157, "one frame per packet");
    let mut pictures = frames.clone();
    pictures.dedup();
    let mut ffmpeg_pictures = expected.clone();
    ffmpeg_pictures.dedup();
    assert_eq!(expected.len(), 9);
    assert_eq!(pictures, ffmpeg_pictures, "the pictures, in order");
}

/// A 512x512 picture at an offset inside a 768x784 coded frame.
#[test]
fn picture_offset() {
    plays_like_ffmpeg(&refcheck::fate("vp3/offset_test.ogv"));
}

/// What FFmpeg's libtheora encoder writes.
#[test]
fn libtheora() {
    let path = std::env::temp_dir().join(format!("ogg-theora-{}.ogv", std::process::id()));
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25", "-t", "2",
            "-c:v", "libtheora", "-q:v", "6"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    plays_like_ffmpeg(&path);
    let _ = std::fs::remove_file(&path);
}

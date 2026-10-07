//! Transport streams played through the PearTube engine (headless sink,
//! not realtime) against FFmpeg: every H.264 frame's digest in order, and
//! the AC-3 sample count of a stream whose PES carry several syncframes.
//!
//! Needs `ffmpeg`, the generated corpus (`PEARTUBE_CORPUS_DIR`) and the
//! FATE suite (`FATE_SUITE`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use player::{Capture, Event, Headless, Player, PlayerOptions};

fn play(path: &Path) -> Capture {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let options = PlayerOptions { realtime: false, ..PlayerOptions::default() };
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()), options, move |event| {
        let _ = tx.send(event);
    });
    loop {
        match rx.recv_timeout(Duration::from_secs(600)) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(e)) => panic!("{}: {e}", path.display()),
            Ok(Event::Changed) => {}
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
    assert_eq!(player.state().error, None, "{}", path.display());
    drop(player);
    backend.capture()
}

#[test]
fn h264_frames_equal_ffmpeg() {
    for path in [check_mpegts::corpus("h264_aac.ts"), refcheck::fate("mpegts/h264small.ts")] {
        let capture = play(&path);
        let [video] = capture.video.as_slice() else {
            panic!("{}: {} video streams captured", path.display(), capture.video.len());
        };
        let theirs = refcheck::ffmpeg_video_md5s(&path, 0, "yuv420p");
        assert!(!theirs.is_empty());
        assert_eq!(video.frame_md5, theirs, "{}: {}x{} frames", path.display(), video.width, video.height);
    }
}

#[test]
fn ac3_sample_count_equals_ffmpeg() {
    // gen:h264_ac3.ts: 188 mono 1536-sample syncframes, up to seven per PES.
    let path = check_mpegts::corpus("h264_ac3.ts");
    let capture = play(&path);
    let [audio] = capture.audio.as_slice() else {
        panic!("{} audio streams captured", capture.audio.len());
    };
    assert_eq!(audio.channels, 1);
    assert_eq!(audio.pcm.len(), 288_768);
}

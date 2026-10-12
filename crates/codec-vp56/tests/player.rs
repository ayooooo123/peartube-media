//! The Player plays VP6 in FLV through the registry (`codecs::context()`):
//! every frame equals FFmpeg's, at FFmpeg's size, including VP6A, whose
//! adjustment byte crops the coded 304x192 to 300x180.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{Event, Headless, Player, PlayerOptions};
use refcheck::fate;

fn play(path: &Path) -> player::Capture {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| {
            let _ = tx.send(event);
        },
    );
    player.play();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{}: {error}", path.display()),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{}: playback did not end: {error}: {:?}", path.display(), player.state()),
        }
    }
    let state = player.state();
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    drop(player);
    backend.capture()
}

fn check(sample: &str, pix_fmt: &str, size: (u32, u32)) {
    let path = fate(sample);
    let capture = play(&path);
    let video = capture.video.first().expect("a video stream played");
    assert_eq!(refcheck::ffmpeg_pix_fmt(video.pixel_format), pix_fmt, "{sample}: pixel format");
    assert_eq!((video.width, video.height), size, "{sample}: the size shown");
    let want = refcheck::ffmpeg_video_md5s(&path, 0, pix_fmt);
    assert!(!want.is_empty(), "{sample}: FFmpeg's frames");
    assert_eq!(video.frame_md5, want, "{sample}: the frames equal FFmpeg's");
}

#[test]
fn vp6f_in_flv_plays_as_ffmpeg() {
    check("flash-vp6/clip1024.flv", "yuv420p", (112, 80));
}

#[test]
fn vp6a_in_flv_plays_cropped_with_its_alpha_as_ffmpeg() {
    check("flash-vp6/300x180-Scr-f8-056alpha.flv", "yuva420p", (300, 180));
}

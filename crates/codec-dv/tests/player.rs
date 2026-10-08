//! The Player plays DV through the registry (`codecs::context()`): raw DV
//! with its audio and DV in MXF give every frame and every audio sample
//! FFmpeg decodes (`-idct simple`).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxideav_core::PixelFormat;
use player::{Event, Headless, Player, PlayerOptions};
use refcheck::fate;

/// Plays `path` to its end with the default tracks; the capture.
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

fn check(path: &Path) {
    let name = path.display().to_string();
    let capture = play(path);
    let video = capture.video.first().expect("a video stream played");
    let pix_fmt = match video.pixel_format {
        PixelFormat::Yuv420P => "yuv420p",
        PixelFormat::Yuv411P => "yuv411p",
        PixelFormat::Yuv422P => "yuv422p",
        other => panic!("{name}: {other:?}"),
    };
    let want = refcheck::ffmpeg_video_md5s_with(path, 0, pix_fmt, &["-idct", "simple"]);
    assert!(!want.is_empty(), "{name}: FFmpeg's frames");
    assert_eq!(video.frame_md5, want, "{name}: the frames equal FFmpeg's");
    let audio = capture.audio.first().expect("an audio stream played");
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    assert_eq!(audio.pcm.len(), reference.len(), "{name}: audio samples");
    let snr = refcheck::snr_db(&reference, &audio.pcm, 0);
    assert!(snr.is_infinite(), "{name}: the audio equals FFmpeg's ({snr} dB)");
}

fn made(name: &str, args: &[&str]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-player-{}-{name}", std::process::id()));
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("the fixture FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

#[test]
fn raw_dv_with_audio_plays_as_ffmpeg() {
    let path = made(
        "ntsc.dv",
        &[
            "-f", "lavfi", "-i", "testsrc=size=720x480:rate=30000/1001:duration=2", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "dvvideo", "-pix_fmt", "yuv411p", "-c:a", "pcm_s16le",
            "-ac", "2", "-f", "dv",
        ],
    );
    check(&path);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn dv_in_mxf_plays_as_ffmpeg() {
    check(&fate("mxf/Avid-00005.mxf"));
}

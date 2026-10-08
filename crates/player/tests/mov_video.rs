//! H.264 and HEVC in QuickTime movies play as FFmpeg decodes them, in the
//! layout phones write: B-frame video, AAC sound, the edit lists FFmpeg's
//! `mov` muxer writes for both (the video's composition delay, the AAC
//! priming), and pictures stored unrotated with a 90° display matrix. The
//! files are FFmpeg's (`-f mov`; libx264, libx265 tagged `hvc1`).
//!
//! The Player shows pictures as coded: it does not apply the display
//! matrix, so the video oracle is FFmpeg's decode without autorotation.
//! Sound must be FFmpeg 2da55bf's: the same sample count (the edit list's
//! priming and end trimmed) and the same samples to 90 dB.

use std::{path::Path, process::Command, sync::Arc, time::Duration};

use player::{Event, Headless, Player, PlayerOptions};

struct Played {
    frames: Vec<String>,
    size: Option<(u32, u32)>,
    pcm: Vec<f32>,
    channels: u16,
}

fn played(path: &Path) -> Played {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
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
    let (pcm, channels) = capture.audio.first().map(|a| (a.pcm.clone(), a.channels)).unwrap_or_default();
    Played {
        frames: capture.video.first().map(|v| v.frame_md5.clone()).unwrap_or_default(),
        size: state.video_size,
        pcm,
        channels,
    }
}

/// `-f mov` from FFmpeg: 2 s of 320x240 30 fps video encoded with `video`
/// (arguments), stored unrotated with a 90° display matrix, and 48 kHz
/// stereo AAC.
fn phone_movie(name: &str, video: &[&str]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("mov-video-{}-{name}.mov", std::process::id()));
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-noautorotate", "-display_rotation", "90", "-f", "lavfi", "-i",
            "testsrc2=size=320x240:rate=30", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-t", "2",
            "-map", "0:v", "-map", "1:a", "-pix_fmt", "yuv420p"])
        .args(video)
        .args(["-c:a", "aac", "-ac", "2", "-f", "mov"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not write {name}");
    path
}

fn plays_like_ffmpeg(path: &Path) {
    let name = path.display();
    let expected = refcheck::ffmpeg_video_md5s_with(path, 0, "yuv420p", &["-noautorotate"]);
    assert!(!expected.is_empty(), "FFmpeg decodes no frame from {name}");
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    let played = played(path);
    assert_eq!(played.size, Some((320, 240)), "{name}: video size");
    assert_eq!(played.frames.len(), expected.len(), "{name}: frame count");
    let first_wrong = played.frames.iter().zip(&expected).position(|(a, b)| a != b);
    assert_eq!(first_wrong, None, "{name}: first frame unlike FFmpeg's");
    assert_eq!(played.channels, 2, "{name}: channels");
    assert_eq!(played.pcm.len(), reference.len(), "{name}: samples (interleaved)");
    let snr = refcheck::snr_db(&reference, &played.pcm, 0);
    assert!(snr >= 90.0, "{name}: sound at {snr:.1} dB");
    let _ = std::fs::remove_file(path);
}

#[test]
fn h264_aac_phone_movie() {
    plays_like_ffmpeg(&phone_movie("h264", &["-c:v", "libx264"]));
}

#[test]
fn hevc_aac_phone_movie() {
    plays_like_ffmpeg(&phone_movie("hevc", &["-c:v", "libx265", "-tag:v", "hvc1", "-x265-params", "log-level=error"]));
}

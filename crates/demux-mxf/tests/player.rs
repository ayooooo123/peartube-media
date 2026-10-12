//! The Player plays MXF files through the registry. An OP1a file of
//! long-GOP MPEG-2 (B-frames) and PCM made by FFmpeg's muxer: every video
//! frame and the audio equal FFmpeg's decode. On FATE samples whose video
//! decoders do not match FFmpeg yet (MPEG-2 4:2:2, one MPEG-4 frame), the
//! audio the demuxer delivers equals FFmpeg's: D-10 AES3 unpacked to
//! 8-channel PCM, and A-law next to MPEG-4 cut by its parser.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
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

/// The first audio stream's samples equal FFmpeg's, bit for bit.
fn audio_as_ffmpeg(name: &str, path: &Path, capture: &player::Capture) {
    let audio = capture.audio.first().expect("an audio stream played");
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    assert_eq!(audio.pcm.len(), reference.len(), "{name}: audio samples");
    let snr = refcheck::snr_db(&reference, &audio.pcm, 0);
    assert!(snr.is_infinite(), "{name}: audio equals FFmpeg's ({snr} dB)");
}

fn generated(name: &str, args: &[&str]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-mxf-{}-{name}", std::process::id()));
    let out = std::process::Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("the fixture FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

#[test]
fn mpeg2_with_b_frames_and_pcm_play_as_ffmpeg() {
    let path = generated(
        "op1a.mxf",
        &[
            "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=2", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "mpeg2video", "-pix_fmt", "yuv420p", "-g", "12",
            "-bf", "2", "-c:a", "pcm_s16le", "-f", "mxf",
        ],
    );
    let capture = play(&path);
    let video = capture.video.first().expect("a video stream played");
    let want = refcheck::ffmpeg_video_md5s_with(&path, 0, refcheck::ffmpeg_pix_fmt(video.pixel_format), &["-idct", "simple"]);
    assert_eq!(want.len(), 50, "FFmpeg's 50 frames");
    assert_eq!(video.frame_md5, want, "video frames equal FFmpeg's");
    audio_as_ffmpeg("generated OP1a", &path, &capture);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn d10_aes3_audio_plays_as_ffmpeg() {
    let path = fate("mxf/Sony-00001.mxf");
    audio_as_ffmpeg("Sony-00001", &path, &play(&path));
}

#[test]
fn a_law_next_to_parsed_mpeg4_plays_as_ffmpeg() {
    let path = fate("mxf/C0023S01.mxf");
    audio_as_ffmpeg("C0023S01", &path, &play(&path));
}

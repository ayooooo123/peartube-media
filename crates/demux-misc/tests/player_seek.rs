//! Playback after a seek through demuxers ported here: the player
//! (Headless backend, every codec of the app) seeks where FFmpeg 2da55bf
//! seeks, then presents exactly the video frames and audio samples that
//! `ffmpeg -ss TARGET` of the port's ffmpeg (FFMPEG_SRC) decodes from the
//! target on. The seek goes back after the whole file played, so the
//! demuxer must move: right after opening, the engine would reach the
//! target by decoding and dropping everything before it. An IVF VP8 seek
//! inside a GOP resumes decoding at the key frame before the target; a VOC
//! PCM seek inside a block trims the block's head.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use player::{Capture, Event, Headless, Player, PlayerOptions};

/// The port's ffmpeg, checked to be revision 2da55bf.
fn port_ffmpeg() -> PathBuf {
    let src = std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"));
    let bin = src.join("ffmpeg");
    let out = Command::new(&bin).arg("-version").output().expect("build ffmpeg in FFMPEG_SRC: make ffmpeg");
    assert!(String::from_utf8_lossy(&out.stdout).contains("2da55bf"), "seek oracle must be FFmpeg 2da55bf");
    bin
}

/// What the port's ffmpeg writes to stdout for `args`.
fn port_ffmpeg_output(args: &[String]) -> Vec<u8> {
    let out = Command::new(port_ffmpeg()).args(["-v", "error", "-nostdin"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// `name` in the scratch directory Cargo gives integration tests, made by
/// the `ffmpeg` on PATH (it has the encoders) from `args`.
fn generated(name: &str, args: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-misc-player-seek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args)
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    path
}

/// Open `path` and play it until `played(capture)` says playback passed
/// the target; pause there, seek back to `target`, and play to the end.
/// Video plays as fast as the pipeline goes. PCM would finish before the
/// test could pause it, so audio plays on a simulated device `speed` times
/// as fast as its sample rate.
fn play_after_seek(path: &Path, target: Duration, speed: Option<f64>, played: impl Fn(&Capture) -> bool) -> Capture {
    let backend = Headless::new();
    if let Some(speed) = speed {
        backend.set_audio_speed(speed);
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(
        path.to_str().unwrap(),
        backend.clone(),
        Arc::new(codecs::context()),
        PlayerOptions { realtime: speed.is_some(), ..PlayerOptions::default() },
        move |event| {
            let _ = tx.send(event);
        },
    );
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut seeked = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(2)) {
            Ok(Event::Ended) if seeked => break,
            Ok(Event::Ended) => panic!("playback ended before it passed the target"),
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("{error}"),
        }
        assert!(Instant::now() < deadline, "player failed to end: {:?}", player.state());
        if !seeked && played(&backend.capture()) {
            player.pause();
            player.seek(target);
            player.play();
            seeked = true;
        }
    }
    drop(player);
    backend.capture()
}

/// Key frames every 0.4 s (25 fps, GOP 10): a seek to the frame at 1.56 s
/// lands on the key frame at 1.2 s and decodes 1.24 s to 1.52 s unseen.
/// (The target is a frame time: for one between frames `ffmpeg -ss`
/// also keeps the frame its trim rounds the target to, trim.c av_rescale_q,
/// while the player starts at the first frame at or after the target.)
#[test]
fn player_resumes_ivf_vp8_frame_exactly_after_a_seek_inside_a_gop() {
    let path = generated(
        "vp8.ivf",
        &["-f", "lavfi", "-i", "testsrc=duration=20:size=96x64:rate=25", "-g", "10", "-keyint_min", "10",
            "-c:v", "libvpx", "-b:v", "200k", "-f", "ivf"],
    );
    let target = Duration::from_millis(1560);
    let capture = play_after_seek(&path, target, None, |c| {
        c.video.first().and_then(|v| v.pts.last()).is_some_and(|pts| *pts >= Duration::from_secs(3))
    });
    let video = capture.video.first().expect("video sink opened");
    let start = video.flushes.last().copied().unwrap_or(0);
    assert_eq!(video.pts.get(start), Some(&target), "the first frame shown after the seek is the target's");
    let resumed: Vec<&String> = video.frame_md5[start..].iter().collect();
    let args = refcheck::ffmpeg_video_md5_args(&path, "0:v:0", "yuv420p", &["-ss", "1.56"]);
    let expected = refcheck::parse_framemd5(&String::from_utf8(port_ffmpeg_output(&args)).unwrap());
    assert_eq!(expected.len(), 461, "FFmpeg decodes the frames at 1.56 s to 19.96 s");
    assert_eq!(resumed, expected.iter().collect::<Vec<_>>(), "every frame after the seek, as FFmpeg decodes it");
}

/// The VOC demuxer returns 2048-byte packets (512 stereo s16 frames);
/// 1.25 s (sample 55125) lies 341 samples into the packet at 54784.
#[test]
fn player_resumes_voc_pcm_sample_exactly_after_a_seek_inside_a_block() {
    let path = generated(
        "s16.voc",
        &["-f", "lavfi", "-i", "sine=frequency=500:duration=20:sample_rate=44100", "-ac", "2", "-c:a", "pcm_s16le"],
    );
    let capture = play_after_seek(&path, Duration::from_millis(1250), Some(25.0), |c| {
        c.audio.first().and_then(|a| a.writes.last()).is_some_and(|w| w.0 >= Duration::from_secs(3))
    });
    let audio = capture.audio.first().expect("audio sink opened");
    let first_write = audio.flushes.last().copied().unwrap_or(0);
    let (pts, from) = audio.writes[first_write];
    assert_eq!(pts, Duration::from_millis(1250), "the first samples after the seek are the target's");
    let args: Vec<String> = ["-ss", "1.25", "-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"]
        .into_iter().map(String::from).collect();
    let expected: Vec<f32> = port_ffmpeg_output(&args)
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();
    assert_eq!(expected.len(), (20 * 44100 - 55125) * 2, "FFmpeg decodes from sample 55125 on");
    assert!(audio.pcm[from..] == expected[..], "every sample after the seek, as FFmpeg decodes it");
}

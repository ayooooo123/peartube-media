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

use std::{path::{Path, PathBuf}, process::Command, sync::Arc, time::Duration};

use player::{Event, Headless, Player, PlayerOptions};

struct Played {
    frames: Vec<String>,
    pts: Vec<Duration>,
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
    let video = capture.video.first();
    Played {
        frames: video.map(|v| v.frame_md5.clone()).unwrap_or_default(),
        pts: video.map(|v| v.pts.clone()).unwrap_or_default(),
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

struct TempFileGuard(PathBuf);

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn assert_fixture_stamps(path_a: &Path, path_b: &Path) {
    let ctx = codecs::context();
    let video_packets = |p: &Path| -> Vec<oxideav_core::Packet> {
        let format = refcheck::probe_container(&ctx, p).unwrap();
        let mut demux = ctx.containers.open_demuxer(&format,
            Box::new(std::fs::File::open(p).unwrap()), &ctx.codecs).unwrap();
        let stream = demux.streams().iter().find(|s| s.params.media_type == oxideav_core::MediaType::Video).unwrap().clone();
        let mut packets = Vec::new();
        loop {
            match demux.next_packet() {
                Ok(packet) if packet.stream_index == stream.index => packets.push(packet),
                Ok(_) => {}
                Err(oxideav_core::Error::Eof) => break,
                Err(error) => panic!("{error}"),
            }
        }
        packets
    };
    let a_pkts = video_packets(path_a);
    let b_pkts = video_packets(path_b);
    assert!(!a_pkts.is_empty(), "Twin A has no video packets");
    assert!(!b_pkts.is_empty(), "Twin B has no video packets");
    assert!(a_pkts.iter().any(|p| p.pts != p.dts), "Twin A must have packets where pts != dts");
    assert!(b_pkts.iter().all(|p| p.pts == p.dts), "Twin B's video packets must all have pts == dts");
}

fn make_twins(name: &str, video_args: &[&str]) -> (PathBuf, PathBuf) {
    let tmp = std::env::temp_dir();
    let path_a = tmp.join(format!("mov-video-{}-{name}-a.mp4", std::process::id()));
    let path_b = tmp.join(format!("mov-video-{}-{name}-b.mp4", std::process::id()));

    let status_a = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i",
            "testsrc2=size=320x240:rate=24000/1001", "-f", "lavfi", "-i",
            "sine=frequency=440:sample_rate=48000", "-t", "3", "-map", "0:v", "-map", "1:a",
            "-pix_fmt", "yuv420p"])
        .args(video_args)
        .args(["-c:a", "aac", "-ac", "2", "-f", "mp4"])
        .arg(&path_a)
        .status()
        .unwrap();
    assert!(status_a.success(), "ffmpeg could not write twin A for {name}");

    let status_b = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-i"])
        .arg(&path_a)
        .args(["-map", "0", "-c", "copy", "-bsf:v", "setts=pts=DTS", "-f", "mp4"])
        .arg(&path_b)
        .status()
        .unwrap();
    assert!(status_b.success(), "ffmpeg could not write twin B for {name}");

    assert_fixture_stamps(&path_a, &path_b);

    (path_a, path_b)
}

fn shown_after_seek(path: &Path, target: Duration) -> Vec<(Duration, String)> {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    player.pause();
    player.seek(target);
    player.play();
    let deadline = std::time::Instant::now() + Duration::from_secs(240);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("player failed to end: {error}: {:?}", player.state()),
        }
    }
    drop(player);
    let capture = backend.capture();
    let video = capture.video.first().expect("video sink opened");
    let after_seek = video.flushes.last().copied().unwrap_or(0);
    video.pts.iter().zip(&video.frame_md5).skip(after_seek)
        .filter(|(pts, _)| **pts >= target)
        .map(|(pts, md5)| (*pts, md5.clone()))
        .collect()
}

#[test]
fn hevc_mp4_stamped_in_decode_order_plays_at_presentation_times() {
    let (path_a, path_b) = make_twins("hevc", &["-c:v", "libx265", "-x265-params", "keyint=24:min-keyint=24:log-level=error"]);
    let _ga = TempFileGuard(path_a.clone());
    let _gb = TempFileGuard(path_b.clone());

    let expected = refcheck::ffmpeg_video_md5s_with(&path_a, 0, "yuv420p", &[]);
    assert!(!expected.is_empty(), "FFmpeg decodes no frame from A");

    let played_a = played(&path_a);
    let played_b = played(&path_b);

    assert_eq!(played_a.frames, expected, "A's frame MD5s equal FFmpeg's decode of A");
    assert!(played_a.pts.windows(2).all(|w| w[0] < w[1]), "A's pts strictly increase");
    assert_eq!(played_b.frames, played_a.frames, "B's MD5 list == A's MD5 list");
    assert_eq!(played_b.pts, played_a.pts, "B's pts list == A's pts list");
}

#[test]
fn h264_mp4_stamped_in_decode_order_plays_at_presentation_times() {
    let (path_a, path_b) = make_twins("h264", &["-c:v", "libx264", "-x264-params", "keyint=24:min-keyint=24"]);
    let _ga = TempFileGuard(path_a.clone());
    let _gb = TempFileGuard(path_b.clone());

    let expected = refcheck::ffmpeg_video_md5s_with(&path_a, 0, "yuv420p", &[]);
    assert!(!expected.is_empty(), "FFmpeg decodes no frame from A");

    let played_a = played(&path_a);
    let played_b = played(&path_b);

    assert_eq!(played_a.frames, expected, "A's frame MD5s equal FFmpeg's decode of A");
    assert!(played_a.pts.windows(2).all(|w| w[0] < w[1]), "A's pts strictly increase");
    assert_eq!(played_b.frames, played_a.frames, "B's MD5 list == A's MD5 list");
    assert_eq!(played_b.pts, played_a.pts, "B's pts list == A's pts list");
}

#[test]
fn hevc_mp4_stamped_in_decode_order_seeks_like_its_twin() {
    let (path_a, path_b) = make_twins("hevc-seek", &["-c:v", "libx265", "-x265-params", "keyint=24:min-keyint=24:open-gop=0:log-level=error"]);
    let _ga = TempFileGuard(path_a.clone());
    let _gb = TempFileGuard(path_b.clone());

    let target = Duration::from_millis(1500);
    let shown_a = shown_after_seek(&path_a, target);
    let shown_b = shown_after_seek(&path_b, target);

    assert!(!shown_a.is_empty(), "A's shown list after seek is non-empty");
    assert_eq!(shown_b, shown_a, "B's list == A's list");
}

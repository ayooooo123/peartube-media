//! Anamorphic video reports its pixel's shape (`State::video_pixel_aspect`),
//! so the apps can show it at its display size instead of squeezed: a DVD's
//! 720x480 is 16:9 on screen, a 1440x1080 broadcast is 1920 wide. FFmpeg
//! writes each file and `ffprobe`'s sample aspect ratio is the oracle. The
//! shape lives in the container's setup bytes (MKV, MP4) or only inline in
//! the stream (MPEG-TS, AVI), and both must be found.

use std::{path::{Path, PathBuf}, process::Command, sync::Arc, time::Duration};

use player::{Event, Headless, Player, PlayerOptions};

struct Clip(PathBuf);

impl Drop for Clip {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// One second of 24 fps test picture at `size`, stored with `video` (FFmpeg arguments).
fn clip(name: &str, size: &str, video: &[&str]) -> Clip {
    let path = std::env::temp_dir().join(format!("pixel-aspect-{}-{name}", std::process::id()));
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc2=size={size}:rate=24"))
        .args(["-t", "1", "-pix_fmt", "yuv420p"])
        .args(video)
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not write {name}");
    Clip(path)
}

/// FFmpeg's sample aspect ratio for the file's video, reduced; None when square or unset.
fn ffprobe_sar(path: &Path) -> Option<(u32, u32)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=sample_aspect_ratio", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    // One line per stream entry (MPEG-TS lists the stream under its program too), maybe comma-ended.
    let text = String::from_utf8(out.stdout).unwrap();
    let field = text.lines().map(|l| l.trim().trim_end_matches(',')).find(|l| !l.is_empty())?;
    let (h, v) = field.split_once(':')?;
    let (h, v): (u32, u32) = (h.parse().ok()?, v.parse().ok()?);
    (h > 0 && v > 0 && h != v).then_some((h, v))
}

fn played_aspect(path: &Path) -> Option<(u32, u32)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), Headless::new(), Arc::new(codecs::context()),
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
    assert!(state.error.is_none(), "{}: {:?}", path.display(), state.error);
    state.video_pixel_aspect
}

fn reports_ffmpegs_shape(clip: Clip) {
    let expected = ffprobe_sar(&clip.0);
    assert_eq!(played_aspect(&clip.0), expected, "{}", clip.0.display());
}

fn anamorphic(clip: Clip) {
    assert!(ffprobe_sar(&clip.0).is_some(), "{}: fixture is square", clip.0.display());
    reports_ffmpegs_shape(clip);
}

#[test]
fn h264_shape_from_mkv_setup_bytes() {
    anamorphic(clip("h264.mkv", "720x480", &["-vf", "setsar=32/27", "-c:v", "libx264"]));
}

#[test]
fn h264_shape_inline_in_mpegts() {
    anamorphic(clip("h264.ts", "1440x1080", &["-vf", "setsar=4/3", "-c:v", "libx264", "-f", "mpegts"]));
}

#[test]
fn hevc_shape_from_mp4_setup_bytes() {
    anamorphic(clip("hevc.mp4", "1440x1080", &["-vf", "setsar=4/3", "-c:v", "libx265", "-tag:v", "hvc1", "-x265-params", "log-level=error"]));
}

#[test]
fn mpeg2_display_shape_from_mkv_setup_bytes() {
    anamorphic(clip("mpeg2.mkv", "720x480", &["-c:v", "mpeg2video", "-aspect", "16:9"]));
}

#[test]
fn mpeg2_display_shape_inline_in_mpegts() {
    anamorphic(clip("mpeg2.ts", "720x576", &["-c:v", "mpeg2video", "-aspect", "4:3", "-f", "mpegts"]));
}

#[test]
fn mpeg4_part2_shape_in_avi() {
    anamorphic(clip("mpeg4.avi", "720x480", &["-vf", "setsar=40/33", "-c:v", "mpeg4", "-vtag", "XVID"]));
}

#[test]
fn square_pixels_report_none() {
    let clip = clip("square.mp4", "640x360", &["-c:v", "libx264"]);
    assert_eq!(ffprobe_sar(&clip.0), None);
    reports_ffmpegs_shape(clip);
}

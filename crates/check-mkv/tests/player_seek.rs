//! Seeking to a Matroska random-access point that FFmpeg's H.264 parser
//! does not flag as a keyframe (non-IDR I frame, several reference frames,
//! no recovery-point SEI). The container marks it with the SimpleBlock
//! keyframe bit and Cues; FFmpeg seeks there and decodes every following
//! frame exactly. The player must resume video after the same seek.
//!
//! Packet keyframe flags stay FFmpeg-exact. Resuming needs the Block's own
//! random-access signal (`container_keyframe`, first lace only) through the
//! shared packet metadata, consumed by the engine and platform sink gates.
//! Until that integration lands this test fails, documenting the freeze.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use player::{Event, Headless, Player, PlayerOptions};

fn reproducer() -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join("opengop_nosei.mkv");
    let status = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=8",
            "-c:v", "libx264", "-preset", "medium",
            "-x264-params", "keyint=50:min-keyint=50:scenecut=0:open-gop=1:ref=3:bframes=2",
            "-bsf:v", "filter_units=remove_types=6", "-f", "matroska"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

#[test]
fn player_resumes_video_after_seeking_to_a_container_random_access_point() {
    let path = reproducer();
    let target = Duration::from_secs(4);
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    player.seek(target);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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
    let after_seek = &video.pts[video.flushes.last().copied().unwrap_or(0)..];
    let resumed = after_seek.iter().filter(|&&t| t >= target).count();
    // FFmpeg (`-ss 4`) lands on the Cues entry at 4 s and outputs frames
    // 100.. identical to a full decode: 100 frames remain at 25 fps.
    assert!(resumed >= 90, "{resumed} video frames after seeking to {target:?} (first: {:?})", after_seek.first());
}

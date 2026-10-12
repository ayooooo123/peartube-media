//! Seek inside a gradual refresh cycle: recover before the target, not
//! after it. Starting without earlier input must hide partial pictures.
#[path = "support/seek_preroll.rs"]
mod fixture;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use player::{Event, Headless, Player, PlayerOptions};

fn play(path: &Path, seek: Option<Duration>) -> (Vec<Duration>, Vec<String>) {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    if let Some(target) = seek {
        player.pause();
        player.seek(target);
        player.play();
    }
    let deadline = Instant::now() + Duration::from_secs(90);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Event::Ended) => break,
            Ok(Event::Error(error)) => panic!("{error}"),
            Ok(Event::Changed) => {}
            Err(error) => panic!("{error}: {:?}", player.state()),
        }
    }
    drop(player);
    let capture = backend.capture();
    let video = &capture.video[0];
    let start = video.flushes.last().copied().unwrap_or(0);
    (video.pts[start..].to_vec(), video.frame_md5[start..].to_vec())
}

#[test]
fn seeks_inside_intra_refresh_windows_preserve_every_target_picture() {
    let (ts, _) = fixture::intra_refresh("seek_intra_refresh");
    for (format, ext) in [("matroska", "mkv"), ("mp4", "mp4"), ("mpegts", "ts")] {
        let path = if ext == "ts" { ts.clone() } else {
            let path = fixture::tmp(&format!("seek_intra_refresh.{ext}"));
            fixture::encode(&["-i", ts.to_str().unwrap(), "-c", "copy", "-f", format], &path);
            path
        };
        let (stream, packets) = fixture::video_stream(&path);
        let start = Duration::from_secs_f64(stream.time_base.seconds_of(packets[0].pts.unwrap()));
        let reference = refcheck::ffmpeg_video_md5s(&path, 0, "yuv420p");
        assert_eq!(reference.len(), 200);
        // 4.2 s falls inside the current refresh cycle; 5 s can start at
        // the preceding recovery point without walking to the IDR.
        for (offset, first) in [(4200, 105), (5000, 125)] {
            let target = start + Duration::from_millis(offset);
            let (pts, shown) = play(&path, Some(target));
            assert!(pts.iter().all(|&pts| pts >= target), "{ext}: pre-target frame shown");
            assert_eq!(shown, reference[first..], "{ext}: seek to {offset} ms");
        }
    }
}

#[test]
fn opening_at_a_gradual_recovery_point_hides_partial_pictures_like_ffmpeg() {
    let (_, cut) = fixture::intra_refresh("start_intra_refresh");
    let expected = refcheck::ffmpeg_video_md5s(&cut, 0, "yuv420p");
    let reference_pts = fixture::reference_pts(&cut);
    let (stream, packets) = fixture::video_stream(&cut);
    let first_packet = Duration::from_secs_f64(stream.time_base.seconds_of(packets[0].pts.unwrap()));
    assert!(reference_pts[0] > first_packet + Duration::from_millis(500), "fixture needs a nonzero recovery window");
    let (pts, shown) = play(&cut, None);
    assert_eq!(shown, expected, "partial pictures must not escape the decoder");
    assert_eq!(pts.len(), reference_pts.len());
    for (ours, expected) in pts.iter().zip(reference_pts) {
        assert!(ours.abs_diff(expected) < Duration::from_micros(2), "{ours:?} != {expected:?}");
    }
}

//! Seeking to a Matroska random-access point that FFmpeg's H.264 parser
//! does not flag as a keyframe (non-IDR I frame, several reference frames,
//! no recovery-point SEI). The container marks it with the SimpleBlock
//! keyframe bit and Cues; FFmpeg seeks there and decodes every following
//! frame exactly. The player must resume video after the same seek.
//!
//! Packet keyframe flags stay FFmpeg-exact. Resuming needs the Block's own
//! random-access signal (`container_keyframe`, first lace only) through the
//! shared packet metadata, consumed by the engine and platform sink gates.
//! The demuxer exposes it (checked below), and the complete post-seek frame
//! sequence must match the independent decoder.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use oxideav_core::Error;
use player::{Event, Headless, Player, PlayerOptions};

/// Made once for both tests, which run in parallel.
static REPRODUCER: LazyLock<PathBuf> = LazyLock::new(|| {
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
});

fn reproducer() -> &'static Path {
    &REPRODUCER
}

#[test]
fn demuxer_marks_the_container_random_access_points_ffmpeg_does_not_flag() {
    let path = reproducer();
    let mut d = check_mkv::open(Box::new(std::fs::File::open(path).unwrap())).unwrap();
    let mut ours = Vec::new();
    let mut container = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => {
                if d.packet_metadata().container_keyframe {
                    container.push(p.pts.unwrap());
                }
                ours.push(check_mkv::Pkt::of(&p));
            }
            Err(Error::Eof) => break,
            Err(e) => panic!("{e}"),
        }
    }
    // Packet flags, timestamps and data stay FFmpeg's.
    let theirs = check_mkv::ffprobe_packets(path, &[]);
    assert_eq!(check_mkv::differences(&ours, &theirs, &None), "");
    let ffmpeg_keys: Vec<i64> = theirs.iter().filter(|p| p.keyframe).filter_map(|p| p.pts).collect();
    assert_eq!(ffmpeg_keys, [0]);
    // The container's own indication is on exactly the Blocks its Cues index.
    let typed = oxideav_mkv::demux::open_typed(
        Box::new(std::fs::File::open(path).unwrap()),
        &oxideav_core::NullCodecResolver,
    )
    .unwrap();
    let cues: Vec<i64> = typed.cue_points().iter().map(|c| c.time as i64).collect();
    assert_eq!(cues, [0, 2000, 4000, 6000]);
    assert_eq!(container, cues);
}

#[test]
fn player_resumes_video_after_seeking_to_a_container_random_access_point() {
    let path = reproducer();
    let target = Duration::from_secs(4);
    // Parser flags must remain false at the container's 4 s seek point.
    let mut demux = check_mkv::open(Box::new(std::fs::File::open(path).unwrap())).unwrap();
    let video_stream = demux.streams().iter()
        .find(|stream| stream.params.media_type == oxideav_core::MediaType::Video)
        .unwrap().clone();
    let tb = video_stream.time_base.as_rational();
    let seek_pts = 4 * tb.den / tb.num;
    demux.seek_to(video_stream.index, seek_pts).unwrap();
    let packet = loop {
        let packet = demux.next_packet().unwrap();
        if packet.stream_index == video_stream.index { break packet; }
    };
    assert_eq!(packet.pts, Some(seek_pts));
    assert!(!packet.flags.keyframe, "this seek must require container random access");
    assert!(demux.packet_metadata().container_keyframe);
    drop(demux);
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
    let start = video.flushes.last().copied().unwrap_or(0);
    let resumed: Vec<_> = video.pts.iter().zip(&video.frame_md5).skip(start)
        .filter(|(pts, _)| **pts >= target).map(|(_, md5)| md5.clone()).collect();
    let expected = refcheck::ffmpeg_video_md5s_with(path, 0, "yuv420p", &["-ss", "4"]);
    assert_eq!(expected.len(), 100, "the oracle must retain all frames after 4 s");
    assert_eq!(resumed, expected, "seek must resume every reference frame exactly");
}

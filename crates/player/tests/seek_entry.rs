//! A seek that lands on a random-access point the decoder cannot start
//! from: x264 open-GOP I frames whose recovery-point SEIs were removed
//! (non-IDR I frames, three reference frames). Matroska marks them with the
//! keyframe bit and Cues, MP4 lists them as sync samples, MPEG-TS sets the
//! random-access indicator. The pictures after each predict from pictures
//! before it, so decoding from there loses them. After a seek to 4 s the
//! player must show every picture from 4 s on as the uninterrupted decode
//! has it: it starts from the IDR picture before the target and drops what
//! precedes the target.
//!
//! The reference is FFmpeg's uninterrupted decode from 4 s on. FFmpeg's own
//! `-ss 4` equals it on the Matroska copy (fftools seeks 3/23 s early and
//! lands on the 2 s Cue); on the MP4 and MPEG-TS copies it lands on the 4 s
//! I frame and its decoder shows no picture at all.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use oxideav_core::{Demuxer, MediaType};
use player::{Event, Headless, Player, PlayerOptions};

/// The open-GOP stream without its recovery points, 8 s at 25 fps, IDR at
/// the start only, open-GOP I frames every 2 s.
fn make(format: &str, ext: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("seek_entry_opengop_nosei.{ext}"));
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=8",
            "-c:v", "libx264", "-preset", "medium",
            "-x264-params", "keyint=50:min-keyint=50:scenecut=0:open-gop=1:ref=3:bframes=2",
            "-bsf:v", "filter_units=remove_types=6", "-f", format])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    path
}

fn open(path: &Path) -> Box<dyn Demuxer> {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap()
}

/// The NAL unit types of an H.264 packet: length-prefixed when the stream
/// has avcC extradata, else Annex B.
fn nal_types(data: &[u8], avcc: bool) -> Vec<u8> {
    let mut types = Vec::new();
    let mut at = 0;
    if avcc {
        while at + 4 < data.len() {
            let len = u32::from_be_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
            types.push(data[at + 4] & 0x1F);
            at += 4 + len;
        }
    } else {
        while at + 3 < data.len() {
            if data[at..at + 3] == [0, 0, 1] {
                types.push(data[at + 3] & 0x1F);
                at += 3;
            } else {
                at += 1;
            }
        }
    }
    types
}

/// The video stream's first presentation time, and where a demuxer seek to
/// 4 s past it lands: a non-IDR I frame the container marks for random
/// access. Without that the test would not exercise the case.
fn start_and_check_landing(path: &Path) -> Duration {
    let mut d = open(path);
    let video = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let avcc = video.params.extradata.first() == Some(&1);
    let first = loop {
        let p = d.next_packet().unwrap();
        if p.stream_index == video.index {
            break p;
        }
    };
    let start = video.time_base.seconds_of(first.pts.unwrap());
    d.seek_to(video.index, video.time_base.ticks_of(start + 4.0)).unwrap();
    let landed = loop {
        let p = d.next_packet().unwrap();
        if p.stream_index == video.index {
            break p;
        }
    };
    let random_access = landed.flags.keyframe || d.packet_metadata().container_keyframe;
    let types = nal_types(&landed.data, avcc);
    assert!(random_access, "{}: the seek must land on a random-access point", path.display());
    assert!(types.contains(&1) && !types.contains(&5), "{}: landed on NAL types {types:?}", path.display());
    let at = video.time_base.seconds_of(landed.pts.unwrap());
    assert!((at - (start + 4.0)).abs() < 0.001, "{}: landed at {at} s", path.display());
    Duration::from_secs_f64(start)
}

/// The frames the player shows after a seek to `target`, from the target on.
fn shown_after_seek(path: &Path, target: Duration) -> Vec<String> {
    let backend = Headless::new();
    let (tx, rx) = std::sync::mpsc::channel();
    let player = Player::open(path.to_str().unwrap(), backend.clone(), Arc::new(codecs::context()),
        PlayerOptions { realtime: false, ..PlayerOptions::default() },
        move |event| { let _ = tx.send(event); });
    // Paused, the pipelines wait while the demuxer applies the seek (else
    // the whole file can play first).
    player.pause();
    player.seek(target);
    player.play();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
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
        .filter(|(pts, _)| **pts >= target).map(|(_, md5)| md5.clone()).collect()
}

fn check(format: &str, ext: &str) -> (PathBuf, Vec<String>) {
    let path = make(format, ext);
    let start = start_and_check_landing(&path);
    let full = refcheck::ffmpeg_video_md5s(&path, 0, "yuv420p");
    assert_eq!(full.len(), 200, "{ext}: FFmpeg's uninterrupted decode");
    let shown = shown_after_seek(&path, start + Duration::from_secs(4));
    assert_eq!(shown, full[100..], "{ext}: every picture from 4 s on, as decoded without the seek");
    (path, shown)
}

#[test]
fn matroska_cue_on_a_non_idr_i_frame() {
    let (path, shown) = check("matroska", "mkv");
    assert_eq!(shown, refcheck::ffmpeg_video_md5s_with(&path, 0, "yuv420p", &["-ss", "4"]), "FFmpeg's -ss 4");
}

#[test]
fn mp4_sync_sample_on_a_non_idr_i_frame() {
    check("mp4", "mp4");
}

#[test]
fn mpegts_random_access_indicator_on_a_non_idr_i_frame() {
    check("mpegts", "ts");
}

//! Full-output HEVC references through the unchanged production registry.
//! Standard inputs: corpus/perf-inputs.sh from the performance audit; no caps.

use oxideav_core::{Decoder, Error, ExecutionContext, Frame, MediaType};
use std::{fs, path::{Path, PathBuf}, process::Command, time::Instant};

fn evidence_dir() -> PathBuf {
    let root = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target"));
    let dir = root.join("hevc-production-oracles");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn complete_reference(path: &Path, width: usize, height: usize, wide: bool, frames: usize) {
    let ctx = codecs::context();
    let container = refcheck::probe_container(&ctx, path).expect("production container selection");
    let mut demux = ctx.containers.open_demuxer(
        &container, Box::new(fs::File::open(path).expect("required HEVC fixture")), &ctx.codecs,
    ).expect("production demuxer");
    let stream = demux.streams().iter().find(|s| s.params.media_type == MediaType::Video)
        .expect("video stream").clone();
    assert!(matches!(stream.params.codec_id.as_str(), "h265" | "hevc"));
    assert_eq!(stream.params.width, Some(width as u32));
    assert_eq!(stream.params.height, Some(height as u32));
    let _ = demux.set_active_streams(&[stream.index]);
    let mut decoder = ctx.codecs.first_decoder(&stream.params).expect("production HEVC decoder");
    decoder.set_execution_context(&ExecutionContext::serial());
    let bytes = if wide { 2 } else { 1 };
    let dims = [(width * bytes, height), (width.div_ceil(2) * bytes, height.div_ceil(2)),
        (width.div_ceil(2) * bytes, height.div_ceil(2))];
    let mut ours = Vec::new();
    let mut drain = |decoder: &mut dyn Decoder| loop {
        match decoder.receive_frame() {
            Ok(Frame::Video(frame)) => ours.push(refcheck::md5_hex(&refcheck::pack(&frame, &dims))),
            Ok(_) => panic!("HEVC emitted a non-video frame"),
            Err(Error::NeedMore | Error::Eof) => break,
            Err(error) => panic!("HEVC receive: {error}"),
        }
    };
    let started = Instant::now();
    loop {
        match demux.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                decoder.send_packet(&packet).expect("complete HEVC packet decode");
                drain(decoder.as_mut());
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(error) => panic!("HEVC demux: {error}"),
        }
    }
    decoder.flush().expect("complete HEVC flush");
    drain(decoder.as_mut());
    let elapsed = started.elapsed();
    let pix_fmt = if wide { "yuv420p10le" } else { "yuv420p" };
    let expected = refcheck::ffmpeg_video_md5s(path, 0, pix_fmt);
    let name = path.file_stem().unwrap().to_str().unwrap();
    let dir = evidence_dir();
    fs::write(dir.join(format!("{name}.ours.md5")), ours.join("\n")).unwrap();
    fs::write(dir.join(format!("{name}.ffmpeg.md5")), expected.join("\n")).unwrap();
    assert_eq!(expected.len(), frames, "complete fixture frame count");
    assert_eq!(ours, expected, "every complete HEVC frame, {}", path.display());
    eprintln!("{}: {frames}/{frames} complete {width}x{height} {pix_fmt} MD5s exact, production {container}, decode+hash {elapsed:?}", path.display());
}

#[test]
fn main_and_main10_complete_production_output() {
    let corpus = std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/peartube-media-corpus"));
    for (name, wide) in [("hevc_1080p30_main.mkv", false), ("hevc_1080p30_main10.mkv", true)] {
        complete_reference(&corpus.join("perf").join(name), 1920, 1080, wide, 900);
    }
}

#[test]
fn larger_picture_complete_production_output() {
    let path = evidence_dir().join("hevc_2160p30_main10.mkv");
    let output = Command::new("ffmpeg").args([
        "-v", "error", "-nostdin", "-y", "-filter_threads", "1", "-f", "lavfi", "-i",
        "testsrc2=size=3840x2160:rate=30:duration=2", "-an", "-c:v", "libx265", "-preset", "medium",
        "-pix_fmt", "yuv420p10le", "-x265-params",
        "pools=2:frame-threads=1:log-level=error:keyint=30:min-keyint=30:scenecut=0",
    ]).arg(&path).output().expect("FFmpeg with libx265 required");
    assert!(output.status.success(), "4K fixture: {}", String::from_utf8_lossy(&output.stderr));
    complete_reference(&path, 3840, 2160, true, 60);
}

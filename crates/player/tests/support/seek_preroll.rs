use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(name)
}

pub fn encode(args: &[&str], path: &Path) {
    let status = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y"])
        .args(args).arg(path).status().unwrap();
    assert!(status.success(), "encoding {}", path.display());
}

/// Eight seconds, recovery SEIs with positive counts, including reference
/// B pictures. The cut begins at a gradual recovery point, without an IDR.
pub fn intra_refresh(name: &str) -> (PathBuf, PathBuf) {
    let full = tmp(&format!("{name}.ts"));
    encode(&["-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=8",
        "-c:v", "libx264", "-preset", "medium", "-x264-params", "keyint=50:intra-refresh=1",
        "-f", "mpegts"], &full);
    let cut = tmp(&format!("{name}_cut.ts"));
    encode(&["-ss", "2.04", "-i", full.to_str().unwrap(), "-c", "copy", "-f", "mpegts"], &cut);
    (full, cut)
}

pub fn video_stream(path: &Path) -> (oxideav_core::StreamInfo, Vec<oxideav_core::Packet>) {
    let ctx = codecs::context();
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let mut demux = ctx.containers.open_demuxer(&format,
        Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
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
    (stream, packets)
}

/// FFmpeg's output timestamps, preserving the container's origin. This
/// observes decoder output, not the container's packet/keyframe flags.
pub fn reference_pts(path: &Path) -> Vec<Duration> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-nostdin", "-v", "error", "-copyts", "-i"]).arg(path)
        .args(["-map", "0:v:0", "-an", "-fps_mode", "passthrough", "-f", "framemd5", "-"])
        .output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let tb = text.lines().find_map(|line| line.strip_prefix("#tb 0: ")).unwrap();
    let (num, den) = tb.split_once('/').unwrap();
    let tick = num.parse::<f64>().unwrap() / den.parse::<f64>().unwrap();
    text.lines().filter(|line| !line.starts_with('#') && !line.is_empty()).map(|line| {
        let pts = line.split(',').nth(2).unwrap().trim().parse::<i64>().unwrap();
        Duration::from_secs_f64((pts as f64 * tick).max(0.0))
    }).collect()
}

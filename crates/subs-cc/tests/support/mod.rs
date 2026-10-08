//! Shared helpers: FFmpeg's caption data and cues as references, the
//! caption-carrying inputs, and our side of each comparison.

#![allow(dead_code)]

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use oxideav_core::{Error, MediaType, Packet, RuntimeContext, StreamInfo};
use subs_cc::CcExtractor;

/// A stream's time base as (num, den).
pub type TimeBase = (i64, i64);

/// The caption data of a video stream, picture by picture in presentation
/// order: each picture's best-effort timestamp (`None`: FFmpeg has none)
/// and its triplets.
#[derive(Debug)]
pub struct Captions {
    pub time_base: TimeBase,
    pub pictures: Vec<(Option<i64>, Vec<[u8; 3]>)>,
}

/// The rollup FATE sample every generated input is made from: MPEG-2 with
/// A/53 Part 4 user data carrying EIA-608 roll-up captions and CEA-708.
pub fn rollup() -> PathBuf {
    refcheck::fate("sub/Closedcaption_rollup.m2v")
}

/// The SCTE-20 FATE sample: MPEG-2 in TS.
pub fn scte20() -> PathBuf {
    refcheck::fate("sub/scte20.ts")
}

/// The scratch directory under the cargo target dir that keeps generated
/// inputs from run to run.
pub fn scratch() -> &'static Path {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("subs-cc");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    });
    &DIR
}

/// A directory of this test process's own, for files no other run reads.
pub fn process_dir() -> &'static Path {
    static DIR: LazyLock<PathBuf> = LazyLock::new(|| {
        let dir = scratch().join(format!("run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    });
    &DIR
}

/// Runs FFmpeg; panics with its error output when it fails.
pub fn ffmpeg(args: &[&str]) {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y"])
        .args(args)
        .output()
        .expect("run ffmpeg");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// The rollup sample re-encoded to `name` (`h264.mkv`, `h264.ts`,
/// `hevc.mkv`, `hevc.ts`): its A/53 data carried in SEI by libx264
/// (B-frames on) or libx265. Made once and kept: written under another
/// name, then renamed, so a run never reads a half-written file.
pub fn generated(name: &str) -> PathBuf {
    let path = scratch().join(name);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().unwrap();
    if path.is_file() {
        return path;
    }
    let source = rollup();
    let encoder: &[&str] = if name.starts_with("h264") {
        &["-c:v", "libx264", "-preset", "veryfast", "-bf", "3", "-a53cc", "1"]
    } else {
        &["-c:v", "libx265", "-preset", "ultrafast", "-a53cc", "1", "-x265-params", "log-level=error:bframes=3"]
    };
    let partial = process_dir().join(name);
    let mut args = vec!["-i", source.to_str().unwrap(), "-an"];
    args.extend_from_slice(encoder);
    args.push(partial.to_str().unwrap());
    ffmpeg(&args);
    std::fs::rename(&partial, &path).unwrap();
    path
}

/// FFmpeg's caption data for `path`: the `subcc` output of its lavfi movie
/// source (`ffprobe -f lavfi -i movie=…[out0+subcc]`), one packet per
/// picture with A/53 side data, in presentation order.
pub fn ffmpeg_captions(path: &Path) -> Captions {
    let name = path.to_str().unwrap();
    assert!(!name.contains([':', ',', ';', '[', ']', '\'', '\\', '=']), "lavfi needs escaping for {name}");
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-f", "lavfi", "-i", &format!("movie={name}[out0+subcc]")])
        .args(["-select_streams", "s", "-show_streams", "-show_packets", "-show_data", "-of", "json"])
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe {name}: {}", String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let time_base = json["streams"][0]["time_base"].as_str().map(parse_time_base).expect("subcc stream");
    let pictures = json["packets"]
        .as_array()
        .map(|packets| {
            packets
                .iter()
                .map(|p| {
                    let pts = p["pts"].as_i64();
                    let bytes = parse_hex_dump(p["data"].as_str().unwrap_or(""));
                    assert_eq!(bytes.len() % 3, 0, "{name}: subcc packet of {} bytes", bytes.len());
                    (pts, bytes.chunks_exact(3).map(|t| [t[0], t[1], t[2]]).collect())
                })
                .collect()
        })
        .unwrap_or_default();
    Captions { time_base, pictures }
}

fn parse_time_base(s: &str) -> TimeBase {
    let (num, den) = s.split_once('/').unwrap();
    (num.parse().unwrap(), den.parse().unwrap())
}

/// The bytes of an ffprobe `-show_data` hex dump.
pub fn parse_hex_dump(dump: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for line in dump.lines() {
        let Some((_, rest)) = line.split_once(": ") else { continue };
        let hex = rest.split("  ").next().unwrap_or("");
        let digits: String = hex.chars().filter(|c| !c.is_whitespace()).collect();
        for pair in digits.as_bytes().chunks(2) {
            bytes.push(u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap());
        }
    }
    bytes
}

/// The registry our side demuxes with.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    demux_misc::register(&mut ctx);
    oxideav_mkv::__oxideav_entry(&mut ctx);
    oxideav_mpegts::__oxideav_entry(&mut ctx);
    ctx
}

/// The first video stream of `path` and all its packets, in decode order.
pub fn video_packets(path: &Path) -> (StreamInfo, Vec<Packet>) {
    stream_packets(path, MediaType::Video)
}

/// The first stream of `kind` in `path` and all its packets.
pub fn stream_packets(path: &Path, kind: MediaType) -> (StreamInfo, Vec<Packet>) {
    let ctx = context();
    let format = refcheck::probe_container(&ctx, path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let file = File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap();
    let stream = demuxer
        .streams()
        .iter()
        .find(|s| s.params.media_type == kind)
        .unwrap_or_else(|| panic!("{}: no {kind:?} stream", path.display()))
        .clone();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => packets.push(packet),
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
    (stream, packets)
}

/// Our caption data for `path`: each video packet through a
/// [`CcExtractor`] and a [`subs_cc::CaptionTimeline`], as the player
/// streams them. Data no picture took by the end is dropped, as FFmpeg
/// drops it.
pub fn our_captions(path: &Path) -> Captions {
    let (stream, packets) = video_packets(path);
    let mut extractor = CcExtractor::new(stream.params.codec_id.as_str(), &stream.params.extradata)
        .unwrap_or_else(|| panic!("{}: {} carries no A/53 captions", path.display(), stream.params.codec_id.as_str()));
    let mut timeline = subs_cc::CaptionTimeline::new();
    let mut pictures = Vec::new();
    for packet in &packets {
        let triplets = extractor.extract(&packet.data);
        pictures.extend(timeline.push(packet.pts, packet.dts, triplets));
    }
    pictures.extend(timeline.finish());
    Captions { time_base: (stream.time_base.0.num, stream.time_base.0.den), pictures }
}

/// `a` in time base `tb_a` equals `b` in `tb_b` (two missing times are
/// equal).
pub fn same_time(a: Option<i64>, tb_a: TimeBase, b: Option<i64>, tb_b: TimeBase) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            i128::from(a) * i128::from(tb_a.0) * i128::from(tb_b.1) == i128::from(b) * i128::from(tb_b.0) * i128::from(tb_a.1)
        }
        (None, None) => true,
        _ => false,
    }
}

/// The packets FFmpeg demuxes from the first stream of `path` (e.g. its
/// SCC demuxer's `eia_608` triplets): time base, and each packet's pts
/// and bytes.
pub fn ffprobe_packets(path: &Path) -> (TimeBase, Vec<(Option<i64>, Vec<u8>)>) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "0", "-show_streams", "-show_packets", "-show_data", "-of", "json"])
        .arg(path)
        .output()
        .expect("run ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let time_base = json["streams"][0]["time_base"].as_str().map(parse_time_base).expect("stream");
    let packets = json["packets"]
        .as_array()
        .map(|packets| {
            packets.iter().map(|p| (p["pts"].as_i64(), parse_hex_dump(p["data"].as_str().unwrap_or("")))).collect()
        })
        .unwrap_or_default();
    (time_base, packets)
}

/// The triplets of a picture as hex, for failure messages.
pub fn hex(triplets: &[[u8; 3]]) -> String {
    triplets.iter().map(|t| format!("{:02x}{:02x}{:02x}", t[0], t[1], t[2])).collect::<Vec<_>>().join(" ")
}

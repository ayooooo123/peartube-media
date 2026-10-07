//! The `vc1` and `vc1test` demuxers seek where FFmpeg 2da55bf does. Both
//! are AVFMT_GENERIC_INDEX (vc1dec.c, vc1test.c): seek.c
//! seek_frame_generic lands on the last key packet at or before the
//! target. Every FATE VC-1 sample has a single key frame, so each test file
//! is a FATE sample twice over (a second key frame mid-file). After each
//! seek the first packets equal `ffprobe -read_intervals TARGET%+#N` of
//! the port's ffprobe (FFMPEG_SRC): size, payload, key flag, dts, and pts
//! where FFmpeg sets one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Error, RuntimeContext, TimeBase};

fn port_ffprobe() -> PathBuf {
    let src = std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"));
    let bin = src.join("ffprobe");
    let out = Command::new(&bin).arg("-version").output().expect("build ffprobe in FFMPEG_SRC");
    assert!(String::from_utf8_lossy(&out.stdout).contains("2da55bf"), "seek oracle must be FFmpeg 2da55bf");
    bin
}

#[derive(Debug, PartialEq)]
struct Pkt {
    size: usize,
    md5: String,
    key: bool,
    pts: Option<i64>,
    dts: Option<i64>,
}

/// FFmpeg's first `n` packets after seeking `path` to `target` seconds.
fn ffprobe_after(path: &Path, format: &str, target: &str, n: usize) -> Vec<Pkt> {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error", "-f", format, "-read_intervals", &format!("{target}%+#{n}")])
        .args(["-show_data_hash", "md5", "-show_entries", "packet=pts,dts,size,flags,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("packet|"))
        .map(|line| {
            let kv: HashMap<&str, &str> = line.split('|').filter_map(|f| f.split_once('=')).collect();
            Pkt {
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                key: kv["flags"].starts_with('K'),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            }
        })
        .collect()
}

/// av_rescale(us, tb.den, 1000000 * tb.num), to nearest.
fn ticks(target: &str, tb: TimeBase) -> i64 {
    let us = (target.parse::<f64>().unwrap() * 1e6).round() as i128;
    let (num, den) = (i128::from(tb.0.num) * 1_000_000, i128::from(tb.0.den));
    ((us * den + num / 2) / num) as i64
}

fn check(path: &Path, name: &str, format: &str, targets: &[&str], n: usize) {
    let mut failures = Vec::new();
    for target in targets {
        let want = ffprobe_after(path, format, target, n);
        let mut ctx = RuntimeContext::new();
        codec_wmv::register(&mut ctx);
        let mut demuxer = ctx.containers.open_demuxer(format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
        let tb = demuxer.streams()[0].time_base;
        let landed = match demuxer.seek_to(0, ticks(target, tb)) {
            Ok(landed) => landed,
            Err(e) => {
                failures.push(format!("{name} @ {target}: {e}"));
                continue;
            }
        };
        let mut got = Vec::new();
        while got.len() < want.len() {
            match demuxer.next_packet() {
                Ok(p) => got.push(Pkt { size: p.data.len(), md5: refcheck::md5_hex(&p.data), key: p.flags.keyframe, pts: p.pts, dts: p.dts }),
                Err(Error::Eof) => break,
                Err(e) => panic!("{name} @ {target}: {e}"),
            }
        }
        // FFmpeg leaves some raw VC-1 pts unset; this demuxer's equal the dts.
        for (g, w) in got.iter_mut().zip(&want) {
            if w.pts.is_none() {
                g.pts = None;
            }
        }
        if got != want {
            failures.push(format!("{name} @ {target}: landed {landed}\n  ours   {got:?}\n  ffmpeg {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{} seeks differ from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}

fn scratch(name: &str, bytes: &[u8]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-wmv-seek-{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// Raw VC-1 Advanced Profile: SA10091.vc1 twice, key frames at 0 and
/// 1.2 s; the targets fall in each GOP and between them.
#[test]
fn vc1_lands_on_ffmpegs_key_frame() {
    let one = std::fs::read(refcheck::fate("vc1/SA10091.vc1")).unwrap();
    let path = scratch("twice.vc1", &[one.clone(), one].concat());
    check(&path, "SA10091 twice", "vc1", &["0.5", "1.2", "1.3", "2.1"], 4);
    let _ = std::fs::remove_file(&path);
}

/// VC-1 test format (WMV3 Main in RCV): SMM0015.rcv with its frames twice
/// (the frame count doubled), key frames at frame 0 and 25 (1 s).
#[test]
fn vc1test_lands_on_ffmpegs_key_frame() {
    let rcv = std::fs::read(refcheck::fate("vc1/SMM0015.rcv")).unwrap();
    let header = 8 + u32::from_le_bytes(rcv[4..8].try_into().unwrap()) as usize + 24;
    let frames = u32::from_le_bytes([rcv[0], rcv[1], rcv[2], 0]) * 2;
    let mut twice = frames.to_le_bytes()[..3].to_vec();
    twice.extend_from_slice(&rcv[3..header]);
    twice.extend_from_slice(&rcv[header..]);
    twice.extend_from_slice(&rcv[header..]);
    let path = scratch("twice.rcv", &twice);
    check(&path, "SMM0015 twice", "vc1test", &["0.5", "1.1", "1.5", "1.9"], 4);
    let _ = std::fs::remove_file(&path);
}

//! Seeking raw DV as `ffprobe -read_intervals T%+#N` does (avformat_seek_file
//! of the video stream, dv_read_seek): the next packets equal FFmpeg's,
//! for a target between frames (FFmpeg takes the nearest), an NTSC rate
//! and a target past the end (the last frame).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Error, RuntimeContext};

type Pkt = (u32, Option<i64>, Option<i64>, usize, String);

fn made(name: &str, size: &str, rate: &str, pix_fmt: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-seek-{}-{name}", std::process::id()));
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=size={size}:rate={rate}:duration=2"))
        .args(["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=2"])
        .args(["-c:v", "dvvideo", "-pix_fmt", pix_fmt, "-c:a", "pcm_s16le", "-ac", "2", "-f", "dv"])
        .arg(&path)
        .output()
        .expect("ffmpeg on PATH");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    path
}

fn ffprobe_after_seek(path: &Path, seconds: &str, n: usize) -> Vec<Pkt> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-read_intervals", &format!("{seconds}%+#{n}"), "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,dts,size,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("packet|"))
        .map(|l| {
            let kv: HashMap<&str, &str> = l.split('|').filter_map(|f| f.split_once('=')).collect();
            (
                kv["stream_index"].parse().unwrap(),
                num(kv.get("pts")),
                num(kv.get("dts")),
                kv["size"].parse().unwrap(),
                kv["data_hash"].trim_start_matches("MD5:").to_string(),
            )
        })
        .collect()
}

/// After `read` packets, a seek of the video stream to `seconds` gives
/// FFmpeg's next packets (up to `n`).
fn check(path: &Path, read: usize, seconds: &str, n: usize) {
    let mut ctx = RuntimeContext::new();
    codec_dv::register(&mut ctx);
    let mut d = ctx.containers.open_demuxer("dv", Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    for _ in 0..read {
        d.next_packet().unwrap();
    }
    // av_seek_frame: AV_TIME_BASE to the video time base (1/60000), rounded.
    let micros = (seconds.parse::<f64>().unwrap() * 1e6).round() as i64;
    let target = (micros * 60_000 + 500_000) / 1_000_000;
    d.seek_to(0, target).unwrap();
    let mut got = Vec::new();
    while got.len() < n {
        match d.next_packet() {
            Ok(p) => got.push((p.stream_index, p.pts, p.dts, p.data.len(), refcheck::md5_hex(&p.data))),
            Err(Error::Eof) => break,
            Err(e) => panic!("{}: {e}", path.display()),
        }
    }
    let want = ffprobe_after_seek(path, seconds, n);
    assert!(!want.is_empty(), "{}: FFmpeg's packets after {seconds}", path.display());
    assert_eq!(got, want, "{}: the packets after the seek to {seconds}", path.display());
}

#[test]
fn seeks_land_on_the_frame_ffmpeg_lands_on() {
    let pal = made("pal.dv", "720x576", "25", "yuv420p");
    check(&pal, 5, "1.0", 6);
    check(&pal, 0, "0.53", 4);
    let ntsc = made("ntsc.dv", "720x480", "30000/1001", "yuv411p");
    check(&ntsc, 9, "0.75", 6);
}

#[test]
fn a_seek_past_the_end_lands_on_the_last_frame() {
    let pal = made("pal_end.dv", "720x576", "25", "yuv420p");
    check(&pal, 3, "30", 4);
}

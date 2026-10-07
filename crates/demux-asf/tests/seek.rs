//! The `asf` demuxer seeks where FFmpeg 2da55bf does: asfdec_f.c
//! asf_read_seek seeks to the data start for 0, by the Simple Index
//! Object where the file has one, else by ff_seek_frame_binary over
//! asf_read_pts (the dts of the next key packet of the seek stream), then
//! skips video to a key frame; argo_asf.c seeks to the block holding the
//! target. After each seek the first packets equal `ffprobe
//! -read_intervals TARGET%+#N` of the port's ffprobe (FFMPEG_SRC):
//! stream, size, payload, key flag, dts and, where FFmpeg knows it, pts.
//! Then mutated copies of indexed and unindexed samples are seeked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Error, MediaType, RuntimeContext, TimeBase};
use refcheck::fate;

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
    stream: u32,
    size: usize,
    md5: String,
    key: bool,
    pts: Option<i64>,
    dts: Option<i64>,
}

/// FFmpeg's stream time bases and first `n` packets after seeking `path`
/// to `target` seconds.
fn ffprobe_after(path: &Path, target: &str, n: usize) -> (Vec<TimeBase>, Vec<Pkt>) {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error", "-read_intervals", &format!("{target}%+#{n}"), "-show_data_hash", "md5"])
        .args(["-show_entries", "stream=time_base:packet=stream_index,pts,dts,size,flags,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    let (mut tbs, mut packets) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        match section {
            "packet" => packets.push(Pkt {
                stream: kv["stream_index"].parse().unwrap(),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                key: kv["flags"].starts_with('K'),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            }),
            "stream" => {
                let (n, d) = kv["time_base"].split_once('/').unwrap();
                tbs.push(TimeBase::new(n.parse().unwrap(), d.parse().unwrap()));
            }
            _ => {}
        }
    }
    (tbs, packets)
}

/// av_rescale_q, to nearest with ties away from zero.
fn rescale(ts: i64, from: TimeBase, to: TimeBase) -> i64 {
    let num = i128::from(ts) * i128::from(from.num()) * i128::from(to.den());
    let den = i128::from(from.den()) * i128::from(to.num());
    let q = (num.abs() + den / 2) / den;
    (if num < 0 { -q } else { q }) as i64
}

fn open(data: Vec<u8>) -> oxideav_core::Result<Box<dyn oxideav_core::Demuxer>> {
    let mut ctx = RuntimeContext::new();
    demux_asf::register(&mut ctx);
    ctx.containers.open_demuxer("asf", Box::new(std::io::Cursor::new(data)), &ctx.codecs)
}

/// av_find_default_stream_index for these samples: the first video
/// stream, else the first audio stream.
fn default_stream(demuxer: &dyn oxideav_core::Demuxer) -> u32 {
    let first = |t: MediaType| demuxer.streams().iter().position(|s| s.params.media_type == t);
    first(MediaType::Video).or_else(|| first(MediaType::Audio)).unwrap_or(0) as u32
}

fn check(rel: &str, targets: &[&str], n: usize) {
    let path = fate(rel);
    let mut failures = Vec::new();
    for target in targets {
        let (ff_tbs, want) = ffprobe_after(&path, target, n);
        let mut demuxer = open(std::fs::read(&path).unwrap()).unwrap();
        let stream = default_stream(&*demuxer);
        let tb = demuxer.streams()[stream as usize].time_base;
        let us = (target.parse::<f64>().unwrap() * 1e6).round() as i64;
        let landed = match demuxer.seek_to(stream, rescale(us, TimeBase::new(1, 1_000_000), tb)) {
            Ok(landed) => landed,
            Err(e) => {
                failures.push(format!("{rel} @ {target}: {e}"));
                continue;
            }
        };
        let mut got = Vec::new();
        while got.len() < want.len() {
            match demuxer.next_packet() {
                Ok(p) => {
                    let ff_tb = ff_tbs[p.stream_index as usize];
                    let ff_pts = want.get(got.len()).and_then(|w: &Pkt| w.pts);
                    got.push(Pkt {
                        stream: p.stream_index,
                        size: p.data.len(),
                        md5: refcheck::md5_hex(&p.data),
                        key: p.flags.keyframe,
                        // ffprobe has no pts for frames of delayed video.
                        pts: ff_pts.and(p.pts.map(|t| rescale(t, p.time_base, ff_tb))),
                        dts: p.dts.map(|t| rescale(t, p.time_base, ff_tb)),
                    });
                }
                Err(Error::Eof) => break,
                Err(e) => panic!("{rel} @ {target}: {e}"),
            }
        }
        if got != want {
            failures.push(format!("{rel} @ {target}: landed {landed}\n  ours   {got:?}\n  ffmpeg {want:?}"));
        }
    }
    assert!(failures.is_empty(), "{} seeks differ from FFmpeg:\n{}", failures.len(), failures.join("\n"));
}

/// Files with a Simple Index Object land on its entries; 0 rewinds to
/// the first data packet without waiting for a key frame.
#[test]
fn indexed_files_land_on_ffmpegs_index_entry() {
    check("wmv8/wmv8_x8intra.wmv", &["0", "10.5", "33.3"], 6);
    check("g2m/g2m2.asf", &["5.5"], 6);
}

/// Files without one bisect the data packets for the key packets of the
/// seek stream (the video stream where there is one).
#[test]
fn unindexed_files_land_where_ffmpegs_bisection_does() {
    check("lossless-audio/luckynight-partial.wma", &["4.4", "7.7"], 4);
    check("wmavoice/streaming_CBR-7K.wma", &["8.0"], 4);
    check("mss1/screen_codec.wmv", &["3.0"], 6);
    check("g2m/g2m4.asf", &["2.2"], 4);
    check("wmv8/wmv_drm.wmv", &["2.5"], 4);
    check("tdsc/tdsc.asf", &["1.2"], 4);
}

/// Argo ASF seeks to the 32-sample block holding the target.
#[test]
fn argo_lands_on_ffmpegs_block() {
    check("argo-asf/PWIN22M.ASF", &["1.0", "2.5"], 2);
    check("argo-asf/CBK2_cut.asf", &["0.3"], 2);
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*, fixed seed
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// 2000 truncated or bit-flipped copies of an indexed and an unindexed
/// sample seek to 1 s, an hour, 0 and i64::MAX, reading after each,
/// within a deadline: a seek reads at most a bounded number of times
/// through the input. The intact samples seek.
#[test]
fn seeking_mutated_files_never_panics_or_hangs() {
    let samples = ["g2m/g2m2.asf", "mss1/screen_codec.wmv"];
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for rel in samples {
            let data = std::fs::read(fate(rel)).unwrap();
            let mut rng = Rng(0x5EED_A5F0_0000_0001);
            for step in 0..=2000 {
                let mut mutated = data.clone();
                if step > 0 && rng.next() % 4 == 0 {
                    mutated.truncate((rng.next() as usize) % (data.len() + 1));
                } else if step > 0 {
                    for _ in 0..1 + rng.next() % 8 {
                        let pos = (rng.next() as usize) % mutated.len();
                        mutated[pos] ^= (rng.next() & 0xFF) as u8 | 1;
                    }
                }
                let Ok(mut demuxer) = open(mutated) else {
                    assert!(step > 0, "{rel}: the intact sample does not open");
                    continue;
                };
                let stream = default_stream(&*demuxer);
                for (n, target) in [1000, 3_600_000, 0, i64::MAX].into_iter().enumerate() {
                    let seeked = demuxer.seek_to(stream, target);
                    assert!(step > 0 || n > 0 || seeked.is_ok(), "{rel}: the intact sample does not seek: {seeked:?}");
                    for _ in 0..4 {
                        if demuxer.next_packet().is_err() {
                            break;
                        }
                    }
                }
            }
        }
        let _ = done.send(());
    });
    match finished.recv_timeout(std::time::Duration::from_secs(600)) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("seeking mutated files missed the deadline"),
        // The worker dropped its sender without finishing: it panicked.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
    }
    worker.join().expect("a seek panicked");
}

//! The `rm` demuxer seeks where FFmpeg 2da55bf does: rmdec.c rm_read_seek
//! is ff_seek_frame_binary over rm_read_dts (the header timestamp of the
//! next packet of the seek stream flagged key with slice sequence 1),
//! bounded by the INDX entries, landing on the last such packet at or
//! before the target. After each seek the first packets of the seek
//! stream equal `ffprobe -read_intervals TARGET%+#N` of the port's ffprobe
//! (FFMPEG_SRC): size, payload, key flag and pts. The other streams are
//! not compared: FFmpeg keeps the audio deinterleaver across a seek, so
//! their first frames depend on what was read before it. RealAudio (.ra)
//! files cannot seek in FFmpeg either. Then mutated copies are seeked.

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
    size: usize,
    md5: String,
    key: bool,
    pts: Option<i64>,
}

/// The `ffprobe -read_intervals` run: its exit status and, per stream,
/// the packets it printed after seeking `path` to `target` seconds.
fn ffprobe_after(path: &Path, target: &str, n: usize) -> (bool, HashMap<u32, Vec<Pkt>>) {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error", "-read_intervals", &format!("{target}%+#{n}"), "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,size,flags,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    let mut packets: HashMap<u32, Vec<Pkt>> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        if fields.next() != Some("packet") {
            continue;
        }
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        packets.entry(kv["stream_index"].parse().unwrap()).or_default().push(Pkt {
            size: kv["size"].parse().unwrap(),
            md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
            key: kv["flags"].starts_with('K'),
            pts: kv.get("pts").and_then(|v| v.parse().ok()),
        });
    }
    (out.status.success(), packets)
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
    demux_rm::register(&mut ctx);
    ctx.containers.open_demuxer("rm", Box::new(std::io::Cursor::new(data)), &ctx.codecs)
}

/// av_find_default_stream_index for these samples: the first video
/// stream, else the first audio stream.
fn default_stream(demuxer: &dyn oxideav_core::Demuxer) -> u32 {
    let first = |t: MediaType| demuxer.streams().iter().position(|s| s.params.media_type == t);
    first(MediaType::Video).or_else(|| first(MediaType::Audio)).unwrap_or(0) as u32
}

/// Seek `rel` to each target; compare the first `n` packets of the seek
/// stream (all streams print until ffprobe has `n` packets in total).
fn check(rel: &str, targets: &[&str], n: usize) {
    let path = fate(rel);
    let mut failures = Vec::new();
    for target in targets {
        let (ok, ff) = ffprobe_after(&path, target, n);
        assert!(ok, "{rel} @ {target}: ffprobe cannot seek");
        let mut demuxer = open(std::fs::read(&path).unwrap()).unwrap();
        let stream = default_stream(&*demuxer);
        let want = ff.get(&stream).map(Vec::as_slice).unwrap_or_default();
        assert!(!want.is_empty(), "{rel} @ {target}: ffprobe printed no packet of the seek stream");
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
                Ok(p) if p.stream_index == stream => got.push(Pkt {
                    size: p.data.len(),
                    md5: refcheck::md5_hex(&p.data),
                    key: p.flags.keyframe,
                    pts: p.pts,
                }),
                Ok(_) => {}
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

/// Video streams land on the last key frame at or before the target,
/// with and without an INDX chunk. (FFmpeg cannot seek rv30.rm to 1.1 s
/// to 1.3 s: its bisection syncs on a false packet header there.)
#[test]
fn video_lands_on_ffmpegs_key_frame() {
    check("real/G2_with_SVT_320_240.rm", &["2.0", "4.5"], 6);
    check("real/rv30.rm", &["1.0", "2.0"], 8);
    check("real/spygames-2MB.rmvb", &["15.0", "21.0"], 8);
    check("sipr/sipr_5k0.rm", &["10.0", "13.0"], 8);
    check("sipr/sipr_16k.rm", &["20.0"], 12);
}

/// Audio-only files land on the first packet of a deinterleaving block.
#[test]
fn audio_lands_on_ffmpegs_block() {
    check("real/ra_cook.rm", &["2.5", "4.0"], 6);
    check("real/ra3_in_rm_file.rm", &["3.0", "8.0"], 6);
    check("lossless-audio/luckynight-partial.rmvb", &["5.0"], 3);
}

/// Where FFmpeg's bisection fails ("read_timestamp() failed in the
/// middle": a scan from mid-file syncs on a false packet header and runs
/// off the end), so does this one, and reading resumes where it was.
#[test]
fn seeks_fail_where_ffmpegs_bisection_does() {
    for (rel, target) in [("real/ra_288.rm", "5.0"), ("real/ra_288.rm", "30.0"), ("real/ra3_in_rm_file.rm", "1.0"), ("real/rv30.rm", "1.25")] {
        let path = fate(rel);
        assert!(!ffprobe_after(&path, target, 2).0, "{rel} @ {target}: FFmpeg seeks");
        let data = std::fs::read(&path).unwrap();
        let mut demuxer = open(data.clone()).unwrap();
        let first = demuxer.next_packet().unwrap();
        let stream = default_stream(&*demuxer);
        let ms = (target.parse::<f64>().unwrap() * 1000.0) as i64;
        assert!(demuxer.seek_to(stream, ms).is_err(), "{rel} @ {target}: seeks");
        let mut fresh = open(data).unwrap();
        fresh.next_packet().unwrap();
        let (after, want) = (demuxer.next_packet().unwrap(), fresh.next_packet().unwrap());
        assert_eq!((after.stream_index, after.pts, &after.data), (want.stream_index, want.pts, &want.data), "{rel} @ {target}: reading resumes elsewhere after {first:?}");
    }
}

/// rm_read_dts has no timestamps for RealAudio files: FFmpeg's seek fails
/// ("Operation not permitted") and so does this one.
#[test]
fn realaudio_cannot_seek_like_ffmpeg() {
    let path = fate("realaudio/ra4_288.ra");
    assert!(!ffprobe_after(&path, "0.5", 2).0, "FFmpeg seeks a .ra file");
    let mut demuxer = open(std::fs::read(&path).unwrap()).unwrap();
    let ts = rescale(500_000, TimeBase::new(1, 1_000_000), demuxer.streams()[0].time_base);
    assert!(matches!(demuxer.seek_to(0, ts), Err(Error::Unsupported(_))));
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
    let samples = ["sipr/sipr_5k0.rm", "real/rv30.rm"];
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for rel in samples {
            let data = std::fs::read(fate(rel)).unwrap();
            let mut rng = Rng(0x5EED_4D00_0000_0001);
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

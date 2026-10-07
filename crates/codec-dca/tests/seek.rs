//! The `dts` and `dtshd` demuxers seek where FFmpeg 2da55bf does: both
//! are AVFMT_GENERIC_INDEX (dtsdec.c, dtshddec.c), so seek.c
//! seek_frame_generic lands on the last frame at or before the target,
//! the parser restarting at it. After each seek the first packets equal
//! `ffprobe -read_intervals TARGET%+#N` of the port's ffprobe
//! (FFMPEG_SRC): size, payload, pts and dts in FFmpeg's time base. Then
//! 2000 mutated copies of each sample are seeked.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Error, RuntimeContext, TimeBase};
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
    pts: Option<i64>,
    dts: Option<i64>,
}

/// FFmpeg's stream time base and first `n` packets after seeking `path`
/// to `target` seconds.
fn ffprobe_after(path: &Path, format: &str, target: &str, n: usize) -> (TimeBase, Vec<Pkt>) {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error", "-f", format, "-read_intervals", &format!("{target}%+#{n}")])
        .args(["-show_data_hash", "md5", "-show_entries", "stream=time_base:packet=pts,dts,size,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    let (mut tb, mut packets) = (TimeBase::new(1, 1), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        match section {
            "packet" => packets.push(Pkt {
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            }),
            "stream" => {
                let (n, d) = kv["time_base"].split_once('/').unwrap();
                tb = TimeBase::new(n.parse().unwrap(), d.parse().unwrap());
            }
            _ => {}
        }
    }
    assert!(!packets.is_empty(), "ffprobe {} @ {target}: no packets", path.display());
    (tb, packets)
}

/// av_rescale_q, to nearest with ties away from zero.
fn rescale(ts: i64, from: TimeBase, to: TimeBase) -> i64 {
    let num = i128::from(ts) * i128::from(from.num()) * i128::from(to.den());
    let den = i128::from(from.den()) * i128::from(to.num());
    let q = (num.abs() + den / 2) / den;
    (if num < 0 { -q } else { q }) as i64
}

fn open(format: &str, data: Vec<u8>) -> oxideav_core::Result<Box<dyn oxideav_core::Demuxer>> {
    let mut ctx = RuntimeContext::new();
    codec_dca::register(&mut ctx);
    ctx.containers.open_demuxer(format, Box::new(std::io::Cursor::new(data)), &ctx.codecs)
}

fn check(rel: &str, format: &str, targets: &[&str], n: usize) {
    let path = fate(rel);
    let mut failures = Vec::new();
    for target in targets {
        let (ff_tb, want) = ffprobe_after(&path, format, target, n);
        let mut demuxer = open(format, std::fs::read(&path).unwrap()).unwrap();
        let tb = demuxer.streams()[0].time_base;
        let us = (target.parse::<f64>().unwrap() * 1e6).round() as i64;
        let landed = match demuxer.seek_to(0, rescale(us, TimeBase::new(1, 1_000_000), tb)) {
            Ok(landed) => landed,
            Err(e) => {
                failures.push(format!("{rel} @ {target}: {e}"));
                continue;
            }
        };
        let mut got = Vec::new();
        while got.len() < want.len() {
            match demuxer.next_packet() {
                Ok(p) => got.push(Pkt {
                    size: p.data.len(),
                    md5: refcheck::md5_hex(&p.data),
                    pts: p.pts.map(|t| rescale(t, tb, ff_tb)),
                    dts: p.dts.map(|t| rescale(t, tb, ff_tb)),
                }),
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

#[test]
fn dts_lands_on_ffmpegs_frame() {
    check("dts/dts_es.dts", "dts", &["0.5", "1.234", "2.0"], 3);
    check("dts/master_audio_7.1_24bit.dts", "dts", &["0.333", "3.0"], 3);
}

/// The DTS-HD samples are 6 frames long: a target between frames, and
/// one past the last frame (which lands on it).
#[test]
fn dtshd_lands_on_ffmpegs_frame() {
    check("dts/dcadec-suite/xll_51_24_48_768.dtshd", "dtshd", &["0.015", "0.03", "0.25"], 2);
    check("dts/dcadec-suite/core_51_24_48_768_0.dtshd", "dtshd", &["0.025"], 2);
}

/// A reader of `unit` repeated up to `len` bytes, made as it is read.
struct Repeat {
    unit: Vec<u8>,
    len: u64,
    pos: u64,
}

impl std::io::Read for Repeat {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let unit = self.unit.len() as u64;
        let n = (buf.len() as u64).min(self.len.saturating_sub(self.pos)) as usize;
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = self.unit[((self.pos + i as u64) % unit) as usize];
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl std::io::Seek for Repeat {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.pos = match to {
            std::io::SeekFrom::Start(p) => p,
            std::io::SeekFrom::End(d) => self.len.saturating_add_signed(d),
            std::io::SeekFrom::Current(d) => self.pos.saturating_add_signed(d),
        };
        Ok(self.pos)
    }
}

/// dts_es.dts's first frame 140,000 times (281 MB of DTS): seeking past
/// the end reads frame after frame for one after the target, and stops
/// on the seek's allowance (1 M packets, 256 MiB) with
/// ResourceExhausted, reading resuming where it was.
#[test]
fn a_long_dts_stream_exhausts_the_seek_allowance() {
    let sample = std::fs::read(fate("dts/dts_es.dts")).unwrap();
    let unit = sample[..2012].to_vec();
    let input = move || -> Box<dyn oxideav_core::ReadSeek> {
        Box::new(Repeat { unit: unit.clone(), len: 2012 * 140_000, pos: 0 })
    };
    let open = |input: Box<dyn oxideav_core::ReadSeek>| {
        let mut ctx = RuntimeContext::new();
        codec_dca::register(&mut ctx);
        ctx.containers.open_demuxer("dts", input, &ctx.codecs).unwrap()
    };
    let first = open(input()).next_packet().map(|p| (p.pts, refcheck::md5_hex(&p.data))).ok();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut demuxer = open(input());
        let result = demuxer.seek_to(0, i64::MAX / 2);
        let _ = tx.send((result, demuxer.next_packet().map(|p| (p.pts, refcheck::md5_hex(&p.data))).ok()));
    });
    let (result, after) = rx.recv_timeout(std::time::Duration::from_secs(300)).expect("the seek ends");
    assert!(matches!(result, Err(oxideav_core::Error::ResourceExhausted(_))), "the seek ends on its allowance: {result:?}");
    assert_eq!(after, first, "reading resumes where it was");
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

/// 2000 truncated or bit-flipped copies of each sample seek to 1 s, an
/// hour, 0 and i64::MAX, reading after each, within a deadline: a seek
/// reads at most to the input's end. The intact samples must seek.
#[test]
fn seeking_mutated_files_never_panics_or_hangs() {
    let cases = [("dts/dts_es.dts", "dts"), ("dts/dcadec-suite/xll_51_24_48_768.dtshd", "dtshd")];
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        for (rel, format) in cases {
            let data = std::fs::read(fate(rel)).unwrap();
            let mut rng = Rng(0x5EED_DCA0_0000_0001);
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
                let Ok(mut demuxer) = open(format, mutated) else {
                    assert!(step > 0, "{rel}: the intact sample does not open");
                    continue;
                };
                let tb = demuxer.streams()[0].time_base.as_rational();
                let second = tb.den / tb.num.max(1);
                for (n, target) in [second, 3600 * second, 0, i64::MAX].into_iter().enumerate() {
                    let seeked = demuxer.seek_to(0, target);
                    assert!(step > 0 || n > 0 || seeked.is_ok(), "{rel}: the intact sample does not seek: {seeked:?}");
                    for _ in 0..4 {
                        if demuxer.next_packet().is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = done.send(rel);
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(600);
    let mut seen = 0;
    while seen < 2 {
        match finished.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())) {
            Ok(_) => seen += 1,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("seeking mutated files missed the deadline"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    worker.join().expect("a seek panicked");
    assert_eq!(seen, 2, "every sample seeked");
}

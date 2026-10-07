//! The raw `mlp` and `truehd` demuxers seek where FFmpeg 2da55bf does.
//! mlpdec.c is AVFMT_GENERIC_INDEX: seek.c seek_frame_generic lands on
//! the last key packet at or before the target, and FFmpeg's MLP parser
//! flags as key exactly the access units with a major sync
//! (mlp_parser.c). After each seek the first packets equal `ffprobe
//! -read_intervals TARGET%+#N` of the port's ffprobe (FFMPEG_SRC): size,
//! payload, key flag, pts and dts in FFmpeg's time base. Then 2000
//! mutated copies of a sample are seeked.

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
    key: bool,
    pts: Option<i64>,
    dts: Option<i64>,
}

/// FFmpeg's stream time base and first `n` packets after seeking `path`
/// to `target` seconds.
fn ffprobe_after(path: &Path, format: &str, target: &str, n: usize) -> (TimeBase, Vec<Pkt>) {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error", "-f", format, "-read_intervals", &format!("{target}%+#{n}")])
        .args(["-show_data_hash", "md5", "-show_entries", "stream=time_base:packet=pts,dts,size,flags,data_hash", "-of", "compact"])
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
                key: kv["flags"].starts_with('K'),
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
    codec_mlp::register(&mut ctx);
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
                    key: p.flags.keyframe,
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

/// TrueHD major syncs every 128 access units: targets between them.
#[test]
fn truehd_lands_on_ffmpegs_major_sync() {
    check("truehd/ticket-1726-monocut.thd", "truehd", &["0.2", "0.4", "0.6"], 3);
    check("truehd/spdifenc-branch-padding.thd", "truehd", &["0.02", "0.03"], 3);
}

/// MLP major syncs every 8 access units.
#[test]
fn mlp_lands_on_ffmpegs_major_sync() {
    check("lossless-audio/luckynight-partial.mlp", "mlp", &["1.0", "3.333", "7.5"], 3);
}

/// The valid access units at the head of `data`: the length chain from
/// offset 0 cut after its last whole unit.
fn whole_units(data: &[u8]) -> &[u8] {
    let mut end = 0;
    while end + 2 <= data.len() {
        let len = usize::from(u16::from_be_bytes([data[end], data[end + 1]]) & 0xfff) * 2;
        if len < 4 || end + len > data.len() {
            break;
        }
        end += len;
    }
    &data[..end]
}

/// A valid TrueHD head, then 200,000 false headers: a zero length field
/// four bytes before a major-sync word, every 8 bytes. Each one loses
/// sync and the scan finds the next. Seeking past the end and reading on
/// cross the whole run with bounded work and memory, not one nested
/// resync per header.
#[test]
fn a_run_of_false_headers_is_crossed_without_nesting() {
    let sample = std::fs::read(fate("truehd/ticket-1726-monocut.thd")).unwrap();
    let mut data = whole_units(&sample).to_vec();
    let units = data.len();
    assert!(units > 8000, "the head keeps most of the sample");
    for _ in 0..200_000 {
        data.extend_from_slice(&[0, 0, 0, 0, 0xF8, 0x72, 0x6F, 0xBA]);
    }
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut demuxer = open("truehd", data).unwrap();
        let landed = demuxer.seek_to(0, i64::MAX).unwrap();
        assert_eq!(landed, 30_720, "the last major sync of the head");
        let mut read = 0;
        while demuxer.next_packet().is_ok() {
            read += 1;
        }
        let _ = done.send(read);
    });
    let read = finished.recv_timeout(std::time::Duration::from_secs(60));
    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = read {
        panic!("crossing the run missed the deadline");
    }
    worker.join().expect("crossing the run panicked");
    assert_eq!(read, Ok(37), "the units from the landing to the end of the head ({units} bytes)");
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

/// 2000 truncated or bit-flipped copies seek to 0.3 s, an hour, 0 and
/// i64::MAX, reading after each, within a deadline: a seek reads at most
/// to the input's end. The intact sample seeks to an access unit with a
/// major sync, the one a decoder can start from.
#[test]
fn seeking_mutated_files_never_panics_or_hangs() {
    let (done, finished) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let data = std::fs::read(fate("truehd/ticket-1726-monocut.thd")).unwrap();
        let mut rng = Rng(0x5EED_3170_0000_0001);
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
            let Ok(mut demuxer) = open("truehd", mutated) else {
                assert!(step > 0, "the intact sample does not open");
                continue;
            };
            for (n, target) in [14_400, 3600 * 48_000, 0, i64::MAX].into_iter().enumerate() {
                let seeked = demuxer.seek_to(0, target);
                for read in 0..4 {
                    let next = demuxer.next_packet();
                    if step == 0 && n == 0 && read == 0 {
                        let au = next.as_ref().map(|p| p.data.clone()).unwrap_or_default();
                        let sync = au.get(4..8).map(|b| u32::from_be_bytes(b.try_into().unwrap()) & 0xFFFF_FFFE);
                        assert!(seeked.is_ok() && sync == Some(0xF872_6FBA), "the intact sample seeks to {seeked:?}, no major sync");
                    }
                    if next.is_err() {
                        break;
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

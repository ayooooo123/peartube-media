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
    assert!(!packets.is_empty(), "ffprobe {} @ {target}: no packets", path.display());
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

/// Run `f` on a worker thread: its result, or a panic when it does not
/// end within two minutes (a loop) or panics.
fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(120)) {
        Ok(value) => {
            worker.join().unwrap();
            value
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!("{what} does not end"),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            let _ = worker.join();
            panic!("{what} panicked")
        }
    }
}

/// wmv8_x8intra.wmv with its Simple Index Object replaced by one of
/// `entries` entries, entry i at packet i % 356: one second apart, each
/// entry a new position, so av_add_index_entry keeps them all.
fn with_index(entries: u32) -> Vec<u8> {
    let data = std::fs::read(fate("wmv8/wmv8_x8intra.wmv")).unwrap();
    let at = 449_206; // the Simple Index Object
    let mut out = data[..at].to_vec();
    out.extend_from_slice(&data[at..at + 16]);
    out.extend_from_slice(&(56 + 6 * u64::from(entries)).to_le_bytes());
    out.extend_from_slice(&data[at + 24..at + 40]);
    out.extend_from_slice(&10_000_000u64.to_le_bytes());
    out.extend_from_slice(&1u32.to_le_bytes());
    out.extend_from_slice(&entries.to_le_bytes());
    for i in 0..entries {
        out.extend_from_slice(&(i % 356).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
    }
    out
}

/// 50,000 Simple Index entries, more than avformat's generic cap
/// (43,690), which asf_build_simple_index's av_add_index_entry never
/// applies: a seek to an odd entry dropped by halving lands on it, as
/// FFmpeg's does.
#[test]
fn index_landings_past_the_index_cap_stay_ffmpegs() {
    let data = with_index(50_000);
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("demux-asf-seek");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("long-index-{}.wmv", std::process::id()));
    std::fs::write(&path, &data).unwrap();
    for target in ["43689.5", "45001.2"] {
        let (ff_tbs, want) = ffprobe_after(&path, target, 4);
        let mut demuxer = open(data.clone()).unwrap();
        let ms = (target.parse::<f64>().unwrap() * 1000.0).round() as i64;
        demuxer.seek_to(1, ms).unwrap();
        let got: Vec<Pkt> = (0..want.len())
            .map(|n| {
                let p = demuxer.next_packet().unwrap();
                let ff_tb = ff_tbs[p.stream_index as usize];
                Pkt {
                    stream: p.stream_index,
                    size: p.data.len(),
                    md5: refcheck::md5_hex(&p.data),
                    key: p.flags.keyframe,
                    pts: want[n].pts.and(p.pts.map(|t| rescale(t, p.time_base, ff_tb))),
                    dts: p.dts.map(|t| rescale(t, p.time_base, ff_tb)),
                }
            })
            .collect();
        assert_eq!(got, want, "seek to {target} s");
    }
    std::fs::remove_file(&path).unwrap();
}

/// One 32-byte single-payload ASF data packet: stream `stream` (key when
/// `key`), presentation time `ms` with the 5 s preroll, a 5-byte object.
fn data_packet(stream: u8, key: bool, ms: u32, object: u8) -> [u8; 32] {
    let mut p = [0u8; 32];
    // Error correction 0x82 0 0; length flags: a padding-length byte, the
    // packet length absent (the file's packet size); property flags 0x5D:
    // byte stream number, byte object number, dword offset, byte
    // replicated-data length.
    p[..5].copy_from_slice(&[0x82, 0, 0, 0x08, 0x5D]);
    p[5] = 0; // padding
    p[6..10].copy_from_slice(&(5000 + ms).to_le_bytes()); // send time
    p[10..12].copy_from_slice(&40u16.to_le_bytes()); // duration
    p[12] = stream | if key { 0x80 } else { 0 };
    p[13] = object;
    p[14..18].copy_from_slice(&0u32.to_le_bytes()); // offset in the object
    p[18] = 8; // replicated data: object size, presentation time
    p[19..23].copy_from_slice(&5u32.to_le_bytes());
    p[23..27].copy_from_slice(&(5000 + ms).to_le_bytes());
    p[27..].copy_from_slice(&[object; 5]);
    p
}

/// asf_read_pts reads on to the next key packet of the seek stream. The
/// header of wmv8_x8intra.wmv (audio stream 1, video stream 2) over
/// 32-byte data packets: one video key frame, then 1.1 M audio packets
/// and no index. The bisection's reads stop on the seek's allowance (1 M
/// packets, 256 MiB), the seek fails with ResourceExhausted, and reading
/// resumes where it was.
#[test]
fn a_long_run_of_another_streams_packets_exhausts_the_seek_allowance() {
    let sample = std::fs::read(fate("wmv8/wmv8_x8intra.wmv")).unwrap();
    let file_properties: [u8; 16] = [
        0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
    ];
    let fp = sample.windows(16).position(|w| w == file_properties).unwrap();
    let data_object = 2816;
    let mut data = sample[..data_object + 50].to_vec();
    data[fp + 92..fp + 100].copy_from_slice(&[32, 0, 0, 0, 32, 0, 0, 0]); // packet size
    let packets: u64 = 1_100_001;
    data[data_object + 16..data_object + 24].copy_from_slice(&(50 + packets * 32).to_le_bytes());
    data.extend_from_slice(&data_packet(2, true, 0, 0));
    for n in 1..packets as u32 {
        data.extend_from_slice(&data_packet(1, false, n * 40, n as u8));
    }
    let first = open(data.clone()).unwrap().next_packet().map(|p| (p.stream_index, p.pts, p.data)).ok();
    assert!(first.as_ref().is_some_and(|p| p.0 == 1), "the video key frame comes first: {first:?}");
    let (result, after) = bounded("seeking", move || {
        let mut demuxer = open(data).unwrap();
        let result = demuxer.seek_to(1, 30_000_000);
        (result, demuxer.next_packet().map(|p| (p.stream_index, p.pts, p.data)).ok())
    });
    assert!(matches!(result, Err(Error::ResourceExhausted(_))), "the seek ends on its allowance: {result:?}");
    assert_eq!(after, first, "reading resumes where it was");
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

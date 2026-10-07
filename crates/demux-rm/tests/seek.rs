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

/// Every packet `ffprobe -read_intervals INTERVALS` printed, in order:
/// stream, size, payload, key flag and pts.
fn ffprobe_packets(path: &Path, intervals: &str) -> Vec<(u32, Pkt)> {
    let mut args = vec!["-v", "error"];
    if !intervals.is_empty() {
        args.extend(["-read_intervals", intervals]);
    }
    let out = Command::new(port_ffprobe())
        .args(args)
        .args(["-show_data_hash", "md5", "-show_entries", "packet=stream_index,pts,size,flags,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("port ffprobe");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("packet|"))
        .map(|line| {
            let kv: HashMap<&str, &str> = line.split('|').filter_map(|f| f.split_once('=')).collect();
            let pkt = Pkt {
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                key: kv["flags"].starts_with('K'),
                pts: kv.get("pts").and_then(|v| v.parse().ok()),
            };
            (kv["stream_index"].parse().unwrap(), pkt)
        })
        .collect()
}

fn as_pkt(p: &oxideav_core::Packet) -> (u32, Pkt) {
    let pkt = Pkt { size: p.data.len(), md5: refcheck::md5_hex(&p.data), key: p.flags.keyframe, pts: p.pts };
    (p.stream_index, pkt)
}

/// `data` as a file in the scratch directory Cargo gives integration tests.
fn scratch(name: &str, data: &[u8]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-rm-seek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

/// Where the header chunk `tag` starts.
fn chunk(data: &[u8], tag: &[u8; 4]) -> usize {
    let mut at = 0;
    loop {
        if &data[at..at + 4] == tag {
            return at;
        }
        assert!(&data[at..at + 4] != b"DATA", "no {} chunk", String::from_utf8_lossy(tag));
        at += u32::from_be_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
    }
}

/// Run `f` on a worker thread; its result, or a panic when it does not
/// end within a minute (a loop) or panics itself.
fn bounded<T: Send + 'static>(what: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
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

/// A DATA chunk whose next-data pointer names itself, with a literal
/// DATA tag where its first packet starts: rm_sync logs the tag and
/// scans on (rmdec.c:745-751), it never follows the pointer. Reading and
/// seeking end, and the packets (stream, size, payload, key flag) are
/// FFmpeg's. Their timestamps are not compared: the lost first chunk
/// leaves the second block first, whose first packet FFmpeg's new cook
/// parser gives no timestamp (a cached packet's pos is -1), so its
/// block is timed from the stream's relative origin, 0, where this port
/// keeps the chunk's 1856.
#[test]
fn a_data_pointer_back_to_its_own_chunk_does_not_loop() {
    let mut data = std::fs::read(fate("real/ra_cook.rm")).unwrap();
    let at = chunk(&data, b"DATA");
    data[at + 14..at + 18].copy_from_slice(&(at as u32).to_be_bytes());
    data[at + 18..at + 22].copy_from_slice(b"DATA");
    let path = scratch("data-loop.rm", &data);
    let untimed = |(stream, p): (u32, Pkt)| (stream, Pkt { pts: None, ..p });
    let want: Vec<(u32, Pkt)> = ffprobe_packets(&path, "").into_iter().map(untimed).collect();
    assert!(!want.is_empty(), "FFmpeg reads the file");
    let got = bounded("reading", {
        let data = data.clone();
        move || {
            let mut demuxer = open(data).unwrap();
            std::iter::from_fn(|| demuxer.next_packet().ok()).map(|p| untimed(as_pkt(&p))).collect::<Vec<_>>()
        }
    });
    assert_eq!(got, want, "packets of the file");
    let landed = bounded("seeking", move || open(data).unwrap().seek_to(0, 2000).map_err(|e| e.to_string()));
    assert!(matches!(landed, Ok(ts) if ts <= 2000), "the seek lands at or before 2 s: {landed:?}");
}

/// A version-2 INDX whose first entry puts a key frame at i64::MIN: the
/// position is outside the file, so the entry is not indexed, and a
/// seek between it and the next entry neither overflows nor loops.
#[test]
fn a_v2_index_position_outside_the_file_is_not_indexed() {
    let mut data = std::fs::read(fate("real/ra_cook.rm")).unwrap();
    let prop = chunk(&data, b"PROP");
    let stream = u16::from_be_bytes(data[chunk(&data, b"MDPR") + 10..][..2].try_into().unwrap());
    let indx = data.len() as u32;
    data[prop + 38..prop + 42].copy_from_slice(&indx.to_be_bytes());
    data.extend_from_slice(b"INDX");
    data.extend_from_slice(&(24u32 + 2 * 18).to_be_bytes());
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&2u32.to_be_bytes());
    data.extend_from_slice(&stream.to_be_bytes());
    data.extend_from_slice(&[0; 8]); // next index, then the version-2 skip
    for (pts, pos) in [(0u32, i64::MIN), (1000, 100)] {
        data.extend_from_slice(&[0, 0]);
        data.extend_from_slice(&pts.to_be_bytes());
        data.extend_from_slice(&pos.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
    }
    let landed = bounded("seeking", move || open(data).unwrap().seek_to(0, 500).map_err(|e| e.to_string()));
    assert!(matches!(landed, Ok(ts) if ts <= 500), "the seek lands at or before 500 ms: {landed:?}");
}

/// A seek's allowance (1 MiB packets, 256 MiB): one video key frame,
/// then 1.1 M one-byte audio packets. Searching the video stream reads
/// through them; the seek stops when its allowance runs out, whatever
/// the file's length, fails with ResourceExhausted, and reading resumes
/// where it was.
#[test]
fn a_long_run_of_another_streams_packets_exhausts_the_seek_allowance() {
    let sample = std::fs::read(fate("sipr/sipr_5k0.rm")).unwrap();
    let data_start = chunk(&sample, b"DATA") + 18;
    let mut data = sample[..data_start].to_vec();
    // The first video packet (stream 1, key) as the file has it.
    let mut at = data_start;
    loop {
        let len = usize::from(u16::from_be_bytes([sample[at + 2], sample[at + 3]]));
        if u16::from_be_bytes([sample[at + 4], sample[at + 5]]) == 1 && sample[at + 11] & 2 != 0 {
            data.extend_from_slice(&sample[at..at + len]);
            break;
        }
        at += len;
    }
    for n in 0..1_100_000u32 {
        // version 0, length 13, stream 0, timestamp, group 0, flags 0, one byte
        data.extend_from_slice(&[0, 0, 0, 13, 0, 0]);
        data.extend_from_slice(&(n / 8).to_be_bytes());
        data.extend_from_slice(&[0, 0, 0]);
    }
    let first = open(data.clone()).unwrap().next_packet().map(|p| as_pkt(&p)).ok();
    let (counted, read) = counting(data);
    let (result, during, first_after) = bounded("seeking", move || {
        let mut ctx = RuntimeContext::new();
        demux_rm::register(&mut ctx);
        let mut demuxer = ctx.containers.open_demuxer("rm", counted, &ctx.codecs).unwrap();
        let before = read.load(std::sync::atomic::Ordering::Relaxed);
        let result = demuxer.seek_to(1, 10_000);
        let during = read.load(std::sync::atomic::Ordering::Relaxed) - before;
        (result, during, demuxer.next_packet().map(|p| as_pkt(&p)).ok())
    });
    assert!(matches!(result, Err(Error::ResourceExhausted(_))), "the seek ends on its allowance: {result:?}");
    assert!(during <= (1 << 20) * 13 + (1 << 20), "the seek read {during} bytes");
    assert_eq!(first_after, first, "reading resumes where it was");
}

/// A reader over `data` that counts the bytes read.
fn counting(data: Vec<u8>) -> (Box<dyn oxideav_core::ReadSeek>, std::sync::Arc<std::sync::atomic::AtomicU64>) {
    struct Counting(std::io::Cursor<Vec<u8>>, std::sync::Arc<std::sync::atomic::AtomicU64>);
    impl std::io::Read for Counting {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.read(buf)?;
            self.1.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            Ok(n)
        }
    }
    impl std::io::Seek for Counting {
        fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
            self.0.seek(to)
        }
    }
    let read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    (Box::new(Counting(std::io::Cursor::new(data), read.clone())), read)
}

/// SIPR in RM: from the start FFmpeg times the first block early by the
/// decoder's priming, after a seek it does not. The audio after a seek is
/// timed as FFmpeg times it, and is the same whether or not packets were
/// read before the seek.
#[test]
fn sipr_audio_after_a_seek_is_timed_like_ffmpegs_and_independent_of_history() {
    let path = fate("sipr/sipr_5k0.rm");
    let audio_after = |read_first: usize, target_ms: i64| {
        let mut demuxer = open(std::fs::read(&path).unwrap()).unwrap();
        for _ in 0..read_first {
            demuxer.next_packet().unwrap();
        }
        demuxer.seek_to(1, target_ms).unwrap();
        std::iter::from_fn(|| demuxer.next_packet().ok())
            .filter(|p| p.stream_index == 0)
            .take(6)
            .map(|p| (p.pts, refcheck::md5_hex(&p.data)))
            .collect::<Vec<_>>()
    };
    for (target, ms) in [("10.0", 10_000), ("13.0", 13_000)] {
        let want: Vec<Option<i64>> = ffprobe_packets(&path, &format!("{target}%+#24"))
            .into_iter()
            .filter(|(stream, _)| *stream == 0)
            .take(6)
            .map(|(_, p)| p.pts)
            .collect();
        let fresh = audio_after(0, ms);
        let pts: Vec<Option<i64>> = fresh.iter().map(|p| p.0).collect();
        assert_eq!(pts, want, "audio pts after a seek to {target}");
        assert_eq!(audio_after(60, ms), fresh, "audio after reading 60 packets, then seeking to {target}");
    }
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

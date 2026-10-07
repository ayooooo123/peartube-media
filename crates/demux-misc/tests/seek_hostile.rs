//! Seeking under the review's findings (attempt 2): a seek reads within
//! one allowance (1,048,576 packets, 256 MiB) and fails with
//! ResourceExhausted past it, reading resuming where it was; a failed
//! search restores the parser state it touched; landings FFmpeg keeps
//! past avformat's index cap stay FFmpeg's; NUT reads an index over 4096
//! bytes (its header checksum covers the size field); an AV1 unit CBS
//! rejects is never key. The oracle is the port's ffprobe (FFMPEG_SRC,
//! 2da55bf).

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use oxideav_core::{Demuxer, Error, Packet, ReadSeek};

fn port_ffprobe() -> PathBuf {
    let src = std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"));
    let bin = src.join("ffprobe");
    let out = Command::new(&bin).arg("-version").output().expect("build ffprobe in FFMPEG_SRC");
    assert!(String::from_utf8_lossy(&out.stdout).contains("2da55bf"), "seek oracle must be FFmpeg 2da55bf");
    bin
}

/// The scratch directory Cargo gives integration tests.
fn scratch_dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("demux-misc-seek-hostile");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// `data` as `name` in the scratch directory.
fn scratch(name: &str, data: &[u8]) -> PathBuf {
    let path = scratch_dir().join(name);
    let tmp = scratch_dir().join(format!("{name}.{}", std::process::id()));
    std::fs::write(&tmp, data).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    path
}

/// `name` made by the `ffmpeg` on PATH from `args`, once.
fn generated(name: &str, args: &[&str]) -> PathBuf {
    let path = scratch_dir().join(name);
    if !path.exists() {
        let tmp = scratch_dir().join(format!("{}.{}.tmp", std::process::id(), name));
        let out = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-y"])
            .args(args)
            .arg(&tmp)
            .output()
            .expect("ffmpeg must be on PATH");
        assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        std::fs::rename(&tmp, &path).unwrap();
    }
    path
}

#[derive(Clone, Debug, PartialEq)]
struct Pkt {
    stream: u32,
    size: usize,
    md5: String,
    key: bool,
    pts: Option<i64>,
    dts: Option<i64>,
}

fn pkt(p: &Packet) -> Pkt {
    Pkt {
        stream: p.stream_index,
        size: p.data.len(),
        md5: refcheck::md5_hex(&p.data),
        key: p.flags.keyframe,
        pts: p.pts,
        dts: p.dts,
    }
}

/// Every packet the port's ffprobe prints for `args` (before the path).
fn ffprobe_packets(path: &Path, args: &[&str]) -> Vec<Pkt> {
    let out = Command::new(port_ffprobe())
        .args(["-v", "error"])
        .args(args)
        .args(["-show_data_hash", "md5", "-show_entries", "packet=stream_index,pts,dts,size,flags,data_hash", "-of", "compact"])
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
                stream: kv["stream_index"].parse().unwrap(),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                key: kv["flags"].starts_with('K'),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            }
        })
        .collect()
}

fn open(format: &str, input: Box<dyn ReadSeek>) -> Box<dyn Demuxer> {
    let ctx = codecs::context();
    ctx.containers.open_demuxer(format, input, &ctx.codecs).unwrap()
}

fn open_bytes(format: &str, data: Vec<u8>) -> Box<dyn Demuxer> {
    open(format, Box::new(Cursor::new(data)))
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

/// Reads over `data` that count the bytes read and fail inside `bad`.
struct Input {
    data: Cursor<Vec<u8>>,
    read: Arc<AtomicU64>,
    bad: Range<u64>,
}

impl Input {
    fn new(data: Vec<u8>, bad: Range<u64>) -> (Box<dyn ReadSeek>, Arc<AtomicU64>) {
        let read = Arc::new(AtomicU64::new(0));
        (Box::new(Input { data: Cursor::new(data), read: read.clone(), bad }), read)
    }
}

impl Read for Input {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let at = self.data.position();
        if at < self.bad.end && at + buf.len() as u64 > self.bad.start {
            return Err(std::io::Error::other("unreadable range"));
        }
        let n = self.data.read(buf)?;
        self.read.fetch_add(n as u64, Ordering::Relaxed);
        Ok(n)
    }
}

impl Seek for Input {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.data.seek(to)
    }
}

/// Seek a demuxer over `data` from its start, as reading it would stand
/// right after opening: the seek ends on its allowance, read at most
/// `max_bytes`, and reading resumes at the first packet.
fn exhausts_allowance(format: &str, data: Vec<u8>, stream: u32, ts: i64, max_bytes: u64) {
    let first = pkt(&open_bytes(format, data.clone()).next_packet().unwrap());
    let format = format.to_string();
    let (result, during, after) = bounded("seeking", move || {
        let (input, read) = Input::new(data, u64::MAX..u64::MAX);
        let mut demuxer = open(&format, input);
        let before = read.load(Ordering::Relaxed);
        let result = demuxer.seek_to(stream, ts);
        let during = read.load(Ordering::Relaxed) - before;
        (result, during, demuxer.next_packet().map(|p| pkt(&p)).ok())
    });
    assert!(matches!(result, Err(Error::ResourceExhausted(_))), "the seek ends on its allowance: {result:?}");
    assert!(during <= max_bytes, "the seek read {during} bytes");
    assert_eq!(after, Some(first), "reading resumes where it was");
}

/// IVF frames all at pts 0 (generic index): a seek to pts 1 reads on
/// looking for a frame after the target, 1.1 M one-byte frames of them.
#[test]
fn ivf_frames_never_passing_the_target_exhaust_the_seek_allowance() {
    let mut data = Vec::with_capacity(32 + 1_100_000 * 13);
    data.extend_from_slice(b"DKIF\0\0\x20\0VP80");
    data.extend_from_slice(&[16, 0, 16, 0, 25, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    for _ in 0..1_100_000 {
        data.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }
    exhausts_allowance("ivf", data, 0, 1, (1 << 20) * 13 + (1 << 20));
}

/// An MPEG-PS whose video has timestamps only near its start, then 1.1 M
/// ten-byte padding packets: the bisection's timestamp reads walk through
/// their start codes (MAX_SYNC_SIZE bounds the bytes between two, not
/// how many), within one allowance for the whole seek.
#[test]
fn mpegps_runs_of_start_codes_exhaust_the_seek_allowance() {
    let head = std::fs::read(refcheck::fate("mpeg2/matrixbench_mpeg2.lq1.mpg")).unwrap();
    let mut data = head[..96 * 1024].to_vec();
    for _ in 0..1_100_000 {
        data.extend_from_slice(&[0, 0, 1, 0xBE, 0, 4, 0xFF, 0xFF, 0xFF, 0xFF]);
    }
    exhausts_allowance("mpeg", data, 0, 90_000 * 3600, (1 << 20) * 10 + (2 << 20));
}

/// A VOC of 50,001 one-sample blocks indexes 50,002 packet starts, more
/// than avformat's generic cap (43,690), which ff_voc_get_packet's
/// av_add_index_entry never applies. After reading them all, a seek to
/// an odd one (dropped by halving) lands on it, as FFmpeg's does.
#[test]
fn voc_landings_past_the_index_cap_stay_ffmpegs() {
    let blocks = 50_000;
    let mut data = b"Creative Voice File\x1a".to_vec();
    data.extend_from_slice(&[26, 0, 0x0A, 0x01]);
    data.extend_from_slice(&(!0x010Au16).wrapping_add(0x1234).to_le_bytes());
    // 8000 Hz unsigned 8-bit, one sample per block
    data.extend_from_slice(&[1, 3, 0, 0, 131, 0, 0x80]);
    for n in 0..blocks {
        data.extend_from_slice(&[2, 1, 0, 0, (n % 251) as u8]);
    }
    data.push(0);
    let path = scratch("one-sample-blocks.voc", &data);
    let all = blocks + 1;
    for ts in [43_689i64, 43_687] {
        let target = format!("{}", ts as f64 / 8000.0);
        let want: Vec<Pkt> =
            ffprobe_packets(&path, &["-read_intervals", &format!("%+#{all},{target}%+#3")]).split_off(all);
        let mut demuxer = open_bytes("voc", data.clone());
        for _ in 0..all {
            demuxer.next_packet().unwrap();
        }
        demuxer.seek_to(0, ts).unwrap();
        let got: Vec<Pkt> = (0..want.len()).map(|_| pkt(&demuxer.next_packet().unwrap())).collect();
        assert_eq!(got, want, "after reading every block, a seek to {ts}");
    }
}

/// A NUT index of more than 4096 bytes carries a checksum, which FFmpeg
/// runs over the start code, the size field and itself
/// (nutdec.c:97-107). Read right, the index gives FFmpeg's landings; read
/// wrong, it is discarded and the syncpoint search lands elsewhere.
#[test]
fn nut_index_over_4096_bytes_gives_ffmpegs_landings() {
    let path = generated(
        "large-index.nut",
        &[
            "-f", "lavfi", "-i", "sine=frequency=1000:duration=60", "-f", "lavfi", "-i",
            "testsrc=size=320x240:rate=25:duration=60", "-c:a", "mp2", "-c:v", "mpeg2video", "-g", "1", "-q:v", "1",
            "-bf", "0", "-shortest", "-bitexact", "-f", "nut",
        ],
    );
    let data = std::fs::read(&path).unwrap();
    for target in ["3.33", "10.01", "33.3"] {
        let want = ffprobe_packets(&path, &["-fflags", "+noparse+nofillin", "-read_intervals", &format!("{target}%+#6")]);
        let mut demuxer = open_bytes("nut", data.clone());
        // ffprobe seeks the default stream, the video.
        let video = demuxer.streams().iter().position(|s| s.params.media_type == oxideav_core::MediaType::Video).unwrap();
        let tb = demuxer.streams()[video].time_base.as_rational();
        let ticks = (target.parse::<f64>().unwrap() * tb.den as f64 / tb.num as f64).round() as i64;
        demuxer.seek_to(video as u32, ticks).unwrap();
        let got: Vec<Pkt> = (0..want.len()).map(|_| pkt(&demuxer.next_packet().unwrap())).collect();
        let got: Vec<(u32, Option<i64>)> = got.iter().map(|p| (p.stream, p.pts)).collect();
        let want: Vec<(u32, Option<i64>)> = want.iter().map(|p| (p.stream, p.pts)).collect();
        assert_eq!(got, want, "seek to {target}");
    }
}

/// av1_parser.c flags key only what CBS reads in full
/// (cbs_av1_syntax_template.c): not a sequence header of profile 7 with
/// the reduced-still-picture bit before an empty frame header, nor a key
/// frame whose header is cut short. The units' key flags equal FFmpeg's.
#[test]
fn av1_units_cbs_rejects_are_not_key() {
    let sample = std::fs::read(refcheck::fate("av1/seq_hdr_op_param_info.ivf")).unwrap();
    let first_len = u32::from_le_bytes(sample[32..36].try_into().unwrap()) as usize;
    let first = sample[44..44 + first_len].to_vec();
    // The first unit's frame OBU, its payload cut to three bytes.
    let mut cut = Vec::new();
    let mut p = 0;
    while p < first.len() {
        let header = first[p];
        let mut q = p + 1 + usize::from(header & 4 != 0);
        let (mut size, mut shift) = (0usize, 0);
        loop {
            let b = first[q];
            q += 1;
            size |= usize::from(b & 0x7F) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                break;
            }
        }
        if (header >> 3) & 15 == 6 {
            cut.extend_from_slice(&first[p..p + 1 + usize::from(header & 4 != 0)]);
            cut.push(3);
            cut.extend_from_slice(&first[q..q + 3]);
        } else {
            cut.extend_from_slice(&first[p..q + size]);
        }
        p = q + size;
    }
    let units: [&[u8]; 4] = [&first, &[0x0A, 0x01, 0xE8, 0x1A, 0x00], &cut, &first];
    let mut data = sample[..32].to_vec();
    for (n, unit) in units.iter().enumerate() {
        data.extend_from_slice(&(unit.len() as u32).to_le_bytes());
        data.extend_from_slice(&(n as u64).to_le_bytes());
        data.extend_from_slice(unit);
    }
    let path = scratch("av1-rejected-units.ivf", &data);
    let want: Vec<bool> = ffprobe_packets(&path, &[]).iter().map(|p| p.key).collect();
    let mut demuxer = open_bytes("ivf", data);
    let got: Vec<bool> = std::iter::from_fn(|| demuxer.next_packet().ok()).map(|p| p.flags.keyframe).collect();
    assert_eq!(got, want, "key flags of: a valid key unit, a profile-7 header, a cut key frame, the valid unit");
}

/// After `read` packets a seek fails (an unreadable `bad` range or no
/// timestamp to seek by), after its search changed the parser state.
/// Reading then goes on exactly as without the seek, `n` packets checked.
fn failed_seek_leaves_reading_as_it_was(format: &str, data: Vec<u8>, read: usize, bad: Range<u64>, seek: (u32, i64), n: usize) {
    let mut plain = open_bytes(format, data.clone());
    for _ in 0..read {
        plain.next_packet().unwrap();
    }
    let want: Vec<Pkt> = (0..n).map(|_| pkt(&plain.next_packet().unwrap())).collect();
    let (input, _) = Input::new(data, bad);
    let mut demuxer = open(format, input);
    for _ in 0..read {
        demuxer.next_packet().unwrap();
    }
    assert!(demuxer.seek_to(seek.0, seek.1).is_err(), "the seek fails");
    let got: Vec<Pkt> = (0..n).map(|_| pkt(&demuxer.next_packet().unwrap())).collect();
    assert_eq!(got, want, "reading after the failed seek");
}

/// NUT without an index, a video key frame at 0 and 10 s: frames take
/// their pts from the last one plus their frame code's delta. A seek to
/// 2 s reads back from the end, decoding the syncpoints there (which set
/// the stream's last pts to about 10 s), then probes the middle, which
/// cannot be read. Reading resumes with the pts it had.
#[test]
fn a_failed_nut_search_restores_the_stream_timestamps() {
    let path = generated(
        "sparse-syncpoints.nut",
        &[
            "-f", "lavfi", "-i", "testsrc=duration=12:size=64x64:rate=10", "-c:v", "mpeg2video", "-g", "100", "-bf",
            "0", "-write_index", "0", "-bitexact", "-f", "nut",
        ],
    );
    let data = std::fs::read(&path).unwrap();
    let len = data.len() as i64;
    let syncpoint = 0x4E4B_E4AD_EECA_4569u64.to_be_bytes();
    let last = data.windows(8).rposition(|w| w == syncpoint).unwrap() as i64;
    // ff_find_last_ts reads from len - 1 - 1024 * 2^k back: the first of
    // those before the last syncpoint finds a syncpoint near the end.
    let back = (0..).map(|k| len - 1 - (1024 << k)).find(|&p| p < last).unwrap();
    assert!(back > 12_000, "the file has the layout the test needs");
    let tb = open_bytes("nut", data.clone()).streams()[0].time_base.as_rational();
    failed_seek_leaves_reading_as_it_was("nut", data, 5, 12_000..back as u64, (0, 2 * tb.den / tb.num), 12);
}

/// A PVA packet: stream 1 (video, `pts` when given) or 2 (audio).
fn pva_packet(stream: u8, pts: Option<u32>, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x41, 0x56, stream, 0, 0x55];
    p.push(if pts.is_some() { 0x10 } else { 0 });
    let len = payload.len() + if pts.is_some() { 4 } else { 0 };
    p.extend_from_slice(&(len as u16).to_be_bytes());
    if let Some(pts) = pts {
        p.extend_from_slice(&pts.to_be_bytes());
    }
    p.extend_from_slice(payload);
    p
}

/// PVA whose audio PES each span three PVA packets, without a pts: a
/// seek on the audio stream finds no timestamp and fails, after its
/// timestamp reads ended the PES in progress (continue_pes). Reading
/// resumes in the middle of that PES as it was.
#[test]
fn a_failed_pva_search_restores_the_audio_pes_in_progress() {
    let mut data = Vec::new();
    for n in 0..40u32 {
        data.extend(pva_packet(1, Some(n * 3600), &[0, 0, 1, 0, n as u8, 0x10, 0, 0]));
        // A PES header (no pts) and 20 payload bytes, then two packets of
        // 30 more: the PES claims 3 + 1 + 80 bytes.
        let mut pes = vec![0, 0, 1, 0xC0, 0, 84, 0x80, 0, 1, 0xFF];
        pes.extend(std::iter::repeat_n(n as u8, 20));
        data.extend(pva_packet(2, None, &pes));
        data.extend(pva_packet(2, None, &[n as u8 ^ 0x5A; 30]));
        data.extend(pva_packet(2, None, &[n as u8 ^ 0xA5; 30]));
    }
    failed_seek_leaves_reading_as_it_was("pva", data, 3, u64::MAX..u64::MAX, (1, 90_000), 8);
}

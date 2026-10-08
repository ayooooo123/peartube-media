//! Test-only helpers for the `check-mkv` acceptance tests of the PearTube
//! `oxideav-mkv` fork, which demuxes incrementally: packets leave the
//! demuxer as their Blocks are read (a Cluster's CRC-32 is checked while it
//! streams), the open follows the SeekHead instead of walking the Cluster
//! run, and a Cues-less file seeks by scanning its Clusters.
//!
//! The acceptance surface is in `tests/`: packet equality with `ffprobe`
//! over every FATE Matroska / WebM sample and the generated corpus
//! (`packets.rs`) and over laced audio without DefaultDuration
//! (`lace_timing.rs`), the bytes read to open a 50 MB file and return its
//! first packets (`incremental.rs`), seeks against `ffprobe
//! -read_intervals` (`seek.rs`), container mutations (`mutations.rs`) and
//! Player seeks to container random-access points (`player_seek.rs`).

#![forbid(unsafe_code)]

use std::collections::BTreeSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use parking_lot::Mutex;

use oxideav_core::{Demuxer, Error, Packet, ReadSeek, RuntimeContext};

/// Every FATE sample in a Matroska / WebM container: the files FFmpeg's
/// `tests/fate/*.mak` name, plus the ones the suite carries for other
/// tests (Opus vectors, filters, H.264).
pub const FATE_SAMPLES: &[&str] = &[
    "audiomatch/tones_opus_48000_stereo.mka",
    "filter/242_4.mkv",
    "filter/anim.mkv",
    "h264-high-depth/high-qp.mkv",
    "h264/H264_might_overflow.mkv",
    "h264/direct-bff.mkv",
    "h264/dts_5frames.mkv",
    "lcevc/L_AV1_854x480p_8bit8bit_2D_dd.mkv",
    "mkv/1242-small.mkv",
    "mkv/codec_delay_opus.mkv",
    "mkv/dovi-p7-hvce.mkv",
    "mkv/flac_channel_layouts.mka",
    "mkv/h264_tta_undecodable.mkv",
    "mkv/hdr10_plus_vp9_sample.webm",
    "mkv/hdr10tags-both.mkv",
    "mkv/lzo.mka",
    "mkv/prores_bz2.mkv",
    "mkv/prores_zlib.mkv",
    "mkv/spherical.mkv",
    "mkv/subtitle_zlib.mks",
    "mkv/test7_cut.mkv",
    "mkv/tts10.mkv",
    "mkv/wavpack_missing_codecprivate.mka",
    "mkv/xiph_lacing.mka",
    "mkv/zero_length_block.mks",
    "opus/silk-lbrr-mono.mka",
    "opus/silk-lbrr.mka",
    "opus/testvector01.mka",
    "opus/testvector02.mka",
    "opus/testvector03.mka",
    "opus/testvector04.mka",
    "opus/testvector05.mka",
    "opus/testvector06.mka",
    "opus/testvector07.mka",
    "opus/testvector08.mka",
    "opus/testvector09.mka",
    "opus/testvector10.mka",
    "opus/testvector11.mka",
    "opus/testvector12.mka",
    "opus/tron.6ch.tinypkts.mka",
    "vp3/coeff_level64.mkv",
    "vp8/RRSF49-short.webm",
    "vp8/dash_audio1.webm",
    "vp8/dash_audio2.webm",
    "vp8/dash_audio3.webm",
    "vp8/dash_video1.webm",
    "vp8/dash_video2.webm",
    "vp8/dash_video3.webm",
    "vp8/dash_video4.webm",
    "vp8/frame_size_change.webm",
    "vp8_alpha/vp8_video_with_alpha.webm",
    "vp9-test-vectors/vp90-2-2pass-akiyo.webm",
    "vp9-test-vectors/vp90-2-segmentation-aq-akiyo.webm",
    "vp9-test-vectors/vp90-2-segmentation-sf-akiyo.webm",
    "vp9-test-vectors/vp93-2-20-12bit-yuv422.webm",
    "wavpack/special/matroska_mode.mka",
];

/// The generated corpus (`PEARTUBE_CORPUS_DIR`, default
/// `~/projects/peartube-media-corpus`), as the e2e runner resolves it.
pub fn corpus_dir() -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/peartube-media-corpus"))
}

/// Every Matroska / WebM file of the generated corpus, sorted.
pub fn corpus_samples() -> Vec<PathBuf> {
    let dir = corpus_dir();
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("mkv" | "webm")))
        .collect();
    out.sort();
    assert!(!out.is_empty(), "no .mkv / .webm in {}", dir.display());
    out
}

/// One packet, as the fields `ffprobe -show_packets -show_data_hash md5`
/// reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkt {
    pub stream: u32,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub size: usize,
    pub keyframe: bool,
    pub md5: String,
}

impl Pkt {
    pub fn of(p: &Packet) -> Self {
        Pkt {
            stream: p.stream_index,
            pts: p.pts,
            dts: p.dts,
            size: p.data.len(),
            keyframe: p.flags.keyframe,
            md5: refcheck::md5_hex(&p.data),
        }
    }
}

/// The pinned `ffprobe`'s packets for `path`; `args` go before the input
/// (`-select_streams`, `-read_intervals`).
pub fn ffprobe_packets(path: &Path, args: &[&str]) -> Vec<Pkt> {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,dts,size,flags,data_hash"])
        .args(["-of", "compact=p=0"])
        .args(args)
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run ffprobe: {e}"));
    assert!(
        out.status.success(),
        "ffprobe {}: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("stream_index="))
        .map(|line| {
            let mut p = Pkt { stream: 0, pts: None, dts: None, size: 0, keyframe: false, md5: String::new() };
            for (key, value) in line.split('|').filter_map(|f| f.split_once('=')) {
                match key {
                    "stream_index" => p.stream = value.parse().expect("stream_index"),
                    "pts" => p.pts = value.parse().ok(),
                    "dts" => p.dts = value.parse().ok(),
                    "size" => p.size = value.parse().expect("size"),
                    "flags" => p.keyframe = value.starts_with('K'),
                    "data_hash" => p.md5 = value.trim_start_matches("MD5:").to_string(),
                    _ => {}
                }
            }
            p
        })
        .collect()
}

/// The runtime the player installs for Matroska: the fork's registration.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_mkv::register(&mut ctx);
    ctx
}

/// Opens `input` through the registry's `matroska` demuxer, as the player
/// does.
pub fn open(input: Box<dyn ReadSeek>) -> oxideav_core::Result<Box<dyn Demuxer>> {
    let ctx = context();
    ctx.containers.open_demuxer("matroska", input, &ctx.codecs)
}

/// Every packet our demuxer returned for a file, and the error that ended
/// the demux early, if any.
pub struct Demuxed {
    pub packets: Vec<Pkt>,
    pub error: Option<String>,
}

pub fn our_packets(path: &Path) -> Demuxed {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut dmx = match open(Box::new(file)) {
        Ok(d) => d,
        Err(e) => return Demuxed { packets: Vec::new(), error: Some(format!("open: {e}")) },
    };
    let mut packets = Vec::new();
    loop {
        match dmx.next_packet() {
            Ok(p) => packets.push(Pkt::of(&p)),
            Err(Error::Eof) => return Demuxed { packets, error: None },
            Err(e) => return Demuxed { packets, error: Some(e.to_string()) },
        }
    }
}

/// Every contracted field compared directly with the oracle, per stream:
/// empty when `ours` equals `theirs`. All failures are kept, so a
/// timestamp difference cannot hide a payload or count regression.
pub fn differences(ours: &[Pkt], theirs: &[Pkt], error: &Option<String>) -> String {
    let mut parts = Vec::new();
    if let Some(e) = error {
        parts.push(format!("error: {e}"));
    }
    let streams: BTreeSet<u32> = ours.iter().chain(theirs).map(|p| p.stream).collect();
    for s in streams {
        let a: Vec<&Pkt> = ours.iter().filter(|p| p.stream == s).collect();
        let b: Vec<&Pkt> = theirs.iter().filter(|p| p.stream == s).collect();
        let mut diffs = Vec::new();
        if a.len() != b.len() {
            diffs.push(format!("count {}/{}", a.len(), b.len()));
        }
        let n = a.len().min(b.len());
        let fields: [(&str, fn(&Pkt, &Pkt) -> bool); 5] = [
            ("pts", |x, y| x.pts == y.pts),
            ("dts", |x, y| x.dts == y.dts),
            ("size", |x, y| x.size == y.size),
            ("key", |x, y| x.keyframe == y.keyframe),
            ("md5", |x, y| x.md5 == y.md5),
        ];
        for (name, same) in fields {
            let k = (0..n).filter(|&i| !same(a[i], b[i])).count();
            if k > 0 {
                diffs.push(format!("{name} {k}/{n}"));
            }
        }
        if !diffs.is_empty() {
            parts.push(format!("s{s}: {}", diffs.join(", ")));
        }
    }
    if ours.iter().map(|p| p.stream).ne(theirs.iter().map(|p| p.stream)) {
        parts.push("order".into());
    }
    parts.join("; ")
}

/// The reads a [`Counting`] reader served, as `(offset, length)`.
#[derive(Clone, Default)]
pub struct ReadLog(Arc<Mutex<Vec<(u64, u64)>>>);

impl ReadLog {
    pub fn clear(&self) {
        self.0.lock().clear();
    }

    /// Bytes read, counting a re-read byte again.
    pub fn total(&self) -> u64 {
        self.0.lock().iter().map(|r| r.1).sum()
    }

    /// The byte ranges read, merged, as `(start, end)`.
    pub fn ranges(&self) -> Vec<(u64, u64)> {
        let mut reads: Vec<(u64, u64)> = self.0.lock().iter().map(|&(at, n)| (at, at + n)).collect();
        reads.sort();
        let mut out: Vec<(u64, u64)> = Vec::new();
        for (start, end) in reads {
            match out.last_mut() {
                Some(last) if start <= last.1 => last.1 = last.1.max(end),
                _ => out.push((start, end)),
            }
        }
        out
    }
}

/// A `Read + Seek` wrapper logging the offset and length of every read.
pub struct Counting<R> {
    inner: R,
    pos: u64,
    log: ReadLog,
}

impl<R: Seek> Counting<R> {
    pub fn new(mut inner: R, log: ReadLog) -> Self {
        let pos = inner.stream_position().expect("stream position");
        Counting { inner, pos, log }
    }
}

impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.log.0.lock().push((self.pos, n as u64));
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for Counting<R> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(to)?;
        Ok(self.pos)
    }
}

/// The files of the incremental and seek checks, made with `ffmpeg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generated {
    /// 60 s of 640x360 H.264 + AAC at ~6.8 Mbit/s (over 50 MB) in FFmpeg's
    /// default Matroska muxing: SeekHead, a CRC-32 in every Cluster, Cues
    /// after the last Cluster.
    CuesAtEnd,
    /// [`Generated::CuesAtEnd`] remuxed to a pipe: unknown-size Segment,
    /// no Cues.
    NoCues,
    /// The corpus `h264_aac.mkv` remuxed to a pipe: a small Cues-less file.
    SmallNoCues,
}

/// Path of `which` under `dir`, generated on first use.
pub fn generated(dir: &Path, which: Generated) -> PathBuf {
    static LOCK: Mutex<()> = Mutex::new(());
    let _guard = LOCK.lock();
    std::fs::create_dir_all(dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    let make = |name: &str, args: &[&str], to_pipe: bool| -> PathBuf {
        let path = dir.join(name);
        if path.is_file() {
            return path;
        }
        let tmp = dir.join(format!("{name}.{}.tmp", std::process::id()));
        let mut cmd = Command::new(refcheck::system_ffmpeg());
        cmd.args(["-hide_banner", "-loglevel", "error", "-y"]).args(args);
        if to_pipe {
            cmd.arg("pipe:1").stdout(File::create(&tmp).expect("create output"));
        } else {
            cmd.arg(&tmp);
        }
        let status = cmd.status().unwrap_or_else(|e| panic!("run ffmpeg: {e}"));
        assert!(status.success(), "ffmpeg failed making {name}");
        std::fs::rename(&tmp, &path).expect("rename generated file");
        path
    };
    let cues = || {
        make(
            "h264_aac_60s_cues.mkv",
            &[
                "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=25,noise=alls=40:allf=t",
                "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
                "-t", "60",
                "-c:v", "libx264", "-preset", "veryfast",
                "-b:v", "6700k", "-minrate", "6700k", "-maxrate", "6700k", "-bufsize", "2M",
                "-x264-params", "nal-hrd=cbr",
                "-c:a", "aac", "-b:a", "128k",
                "-f", "matroska",
            ],
            false,
        )
    };
    match which {
        Generated::CuesAtEnd => cues(),
        Generated::NoCues => {
            let src = cues();
            let src = src.to_str().expect("utf-8 path");
            make("h264_aac_60s_nocues.mkv", &["-i", src, "-c", "copy", "-f", "matroska"], true)
        }
        Generated::SmallNoCues => {
            let src = corpus_dir().join("h264_aac.mkv");
            let src = src.to_str().expect("utf-8 path");
            make("h264_aac_nocues.mkv", &["-i", src, "-c", "copy", "-f", "matroska"], true)
        }
    }
}

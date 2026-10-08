//! Test-only helpers for the `check-mpegts` acceptance tests of the
//! PearTube `oxideav-mpegts` fork: packet tables compared field by field
//! with `ffprobe -show_packets -show_data_hash md5` (FFmpeg's demuxer plus
//! the codec parser it runs on every TS stream).

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Demuxer, Error, Packet, ReadSeek, RuntimeContext};

/// The generated corpus (`PEARTUBE_CORPUS_DIR`, default
/// `~/projects/peartube-media-corpus`), as the e2e runner resolves it.
pub fn corpus_dir() -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var("HOME").expect("HOME")).join("projects/peartube-media-corpus")
    })
}

/// A generated corpus file by name.
pub fn corpus(name: &str) -> PathBuf {
    corpus_dir().join(name)
}

/// One packet, as the fields `ffprobe -show_packets -show_data_hash md5`
/// reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pkt {
    pub stream: u32,
    pub pts: Option<i64>,
    pub dts: Option<i64>,
    pub duration: Option<i64>,
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
            duration: p.duration,
            size: p.data.len(),
            keyframe: p.flags.keyframe,
            md5: refcheck::md5_hex(&p.data),
        }
    }
}

impl std::fmt::Display for Pkt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let t = |v: Option<i64>| v.map_or("N/A".to_string(), |v| v.to_string());
        write!(
            f,
            "s{} pts={} dts={} dur={} size={} key={} md5={}",
            self.stream,
            t(self.pts),
            t(self.dts),
            t(self.duration),
            self.size,
            u8::from(self.keyframe),
            self.md5
        )
    }
}

/// `ffprobe`'s packets for `path`; `args` go before the input.
pub fn ffprobe_packets(path: &Path, args: &[&str]) -> Vec<Pkt> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,dts,duration,size,flags,data_hash"])
        .args(["-of", "compact=p=0"])
        .args(args)
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run ffprobe: {e}"));
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("stream_index="))
        .map(|line| {
            let mut p = Pkt {
                stream: 0,
                pts: None,
                dts: None,
                duration: None,
                size: 0,
                keyframe: false,
                md5: String::new(),
            };
            for (key, value) in line.split('|').filter_map(|f| f.split_once('=')) {
                match key {
                    "stream_index" => p.stream = value.parse().expect("stream_index"),
                    "pts" => p.pts = value.parse().ok(),
                    "dts" => p.dts = value.parse().ok(),
                    "duration" => p.duration = value.parse().ok(),
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

/// `ffprobe`'s streams for `path`: each stream's `key=value` fields from
/// `-show_entries stream=<entries>`.
pub fn ffprobe_streams(path: &Path, entries: &str) -> Vec<BTreeMap<String, String>> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", &format!("stream={entries}"), "-of", "compact=p=0"])
        .arg(path)
        .output()
        .unwrap_or_else(|e| panic!("run ffprobe: {e}"));
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("index="))
        .map(|line| {
            line.split('|')
                .filter_map(|f| f.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .collect()
}

/// The runtime the player installs for MPEG-TS: the fork's registration.
pub fn context() -> RuntimeContext {
    let mut ctx = RuntimeContext::new();
    oxideav_mpegts::register(&mut ctx);
    ctx
}

/// Opens `input` through the registry's `mpegts` demuxer, as the player
/// does.
pub fn open(input: Box<dyn ReadSeek>) -> oxideav_core::Result<Box<dyn Demuxer>> {
    let ctx = context();
    ctx.containers.open_demuxer("mpegts", input, &ctx.codecs)
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

/// Every contracted field compared directly with the oracle, per stream,
/// then the interleaving across streams: empty when `ours` equals
/// `theirs`. All failures are kept, so a timestamp difference cannot hide
/// a payload or count regression; the first differing packet of each
/// stream is shown with FFmpeg's.
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
        let fields: [(&str, fn(&Pkt, &Pkt) -> bool); 6] = [
            ("pts", |x, y| x.pts == y.pts),
            ("dts", |x, y| x.dts == y.dts),
            ("duration", |x, y| x.duration == y.duration),
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
        if let Some(i) = (0..n).find(|&i| a[i] != b[i]) {
            diffs.push(format!("first #{i}: ours {} | ffmpeg {}", a[i], b[i]));
        } else if a.len() != b.len() {
            let extra = a.get(n).or(b.get(n)).expect("longer side");
            diffs.push(format!("first #{n}: only in {} {extra}", if a.len() > n { "ours" } else { "ffmpeg" }));
        }
        if !diffs.is_empty() {
            parts.push(format!("s{s}: {}", diffs.join(", ")));
        }
    }
    if parts.is_empty() {
        if let Some(i) = (0..ours.len()).find(|&i| ours[i] != theirs[i]) {
            parts.push(format!("order: packet {i} is ours {} | ffmpeg {}", ours[i], theirs[i]));
        }
    }
    parts.join("; ")
}

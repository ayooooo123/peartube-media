//! Independent native VLC oracle, not FFmpeg parity. The adapter includes
//! original C decoder and converter source files without rewriting their
//! parsing/rendering logic. Encoded input, native output and exact commands
//! remain under CARGO_TARGET_TMPDIR/subs-bitmap-vlc-<pid> for replay.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;
use std::time::Duration;
use oxideav_core::{Frame, Packet};

const REVISION: &str = "2e358f3098c2f2b7621d1dc568de8b61ad786322";
static BINARIES: LazyLock<[PathBuf; 2]> = LazyLock::new(build);
fn directory() -> PathBuf {
    let root = option_env!("CARGO_TARGET_TMPDIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!("subs-bitmap-vlc-{}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap(); path
}
fn build() -> [PathBuf; 2] {
    let source = std::env::var_os("VLC_SRC").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap()).join("projects/vlc-src"));
    let revision = Command::new("git").args(["-C", source.to_str().unwrap(), "rev-parse", "HEAD"]).output().expect("original VLC source checkout");
    assert!(revision.status.success());
    assert_eq!(String::from_utf8(revision.stdout).unwrap().trim(), REVISION, "VLC oracle source must be the audited revision");
    let adapter = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vlc_reference");
    ["cvd", "ogt"].map(|kind| {
        let binary = directory().join(kind);
        let decoder = if kind == "cvd" { "cvdsub.c" } else { "svcdsub.c" };
        let mut command = Command::new("cc");
        command.args(["-O2", "-std=c11", "-I"]).arg(&adapter).arg("-I").arg(source.join("include"));
        command.arg(format!("-DVLC_DECODER_SOURCE=\"{}\"", source.join("modules/codec").join(decoder).display()));
        command.arg(format!("-DVLC_YUVP_SOURCE=\"{}\"", source.join("modules/video_chroma/yuvp.c").display()));
        if kind == "cvd" { command.arg("-DREFERENCE_CVD"); }
        command.arg(adapter.join("decode.c")).arg(adapter.join("rgba.c")).arg("-o").arg(&binary);
        eprintln!("VLC {REVISION} native oracle build: {command:?}");
        assert!(command.status().expect("C compiler for original VLC oracle").success());
        binary
    })
}
pub struct Cue { pub start_us: i64, pub duration: Option<Duration>, pub canvas: Vec<u8> }
pub fn reference(cvd: bool, label: &str, packets: &[Packet], width: usize, height: usize) -> Vec<Cue> {
    let binary = &BINARIES[if cvd { 0 } else { 1 }];
    let input = directory().join(format!("{label}.packets"));
    let output = directory().join(format!("{label}.rgba"));
    let mut bytes = b"VLCSUB01".to_vec();
    bytes.extend_from_slice(&(width as u32).to_le_bytes()); bytes.extend_from_slice(&(height as u32).to_le_bytes());
    for packet in packets {
        let pts = packet.pts.map_or(i64::MIN, |pts| super::support::to_us(pts, packet.time_base));
        bytes.extend_from_slice(&pts.to_le_bytes()); bytes.extend_from_slice(&(packet.data.len() as u32).to_le_bytes()); bytes.extend_from_slice(&packet.data);
    }
    std::fs::write(&input, bytes).unwrap();
    eprintln!("VLC {REVISION} replay: {} < {} > {}", binary.display(), input.display(), output.display());
    let status = Command::new(binary).stdin(std::fs::File::open(&input).unwrap()).stdout(Stdio::from(std::fs::File::create(&output).unwrap())).status().unwrap();
    assert!(status.success(), "original VLC decoder/converter failed");
    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[..8], b"VLCRGBA1");
    let size = width * height * 4 + 32;
    assert_eq!((bytes.len() - 8) % size, 0);
    bytes[8..].chunks_exact(size).map(|record| {
        let start = i64::from_le_bytes(record[..8].try_into().unwrap());
        let stop = i64::from_le_bytes(record[8..16].try_into().unwrap());
        let rect: Vec<u32> = record[16..32].chunks_exact(4).map(|v| u32::from_le_bytes(v.try_into().unwrap())).collect();
        eprintln!("{label}: original VLC interval={start}..{stop} rect={rect:?} canvas={}x{} md5={}", width, height, refcheck::md5_hex(&record[32..]));
        Cue { start_us: start, duration: (stop > start).then(|| Duration::from_micros((stop - start) as u64)), canvas: record[32..].to_vec() }
    }).collect()
}
pub fn assert_frame(frame: Frame, expected: &Cue, packet: &Packet, width: usize) {
    let Frame::Video(frame) = frame else { panic!("non-bitmap VCD output") };
    assert_eq!(super::support::to_us(frame.pts.unwrap(), packet.time_base), expected.start_us);
    assert_eq!(frame.display_duration(), expected.duration);
    let planes = frame.image_planes(); assert_eq!(planes.len(), 1);
    assert_eq!(planes[0].stride, width * 4);
    assert_eq!(planes[0].data, expected.canvas, "complete RGBA canvas differs from original VLC C");
}

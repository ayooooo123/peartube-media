//! Laced fixed-frame audio without DefaultDuration: Matroska stores only
//! each Block's first timestamp, and FFmpeg times the later laces from
//! frame durations. Every packet field must equal the pinned FFmpeg's
//! (2da55bf) `ffprobe -show_packets -show_data_hash md5`.
//!
//! Inputs are made here with ffmpeg, laced by mkvmerge (8 frames a Block),
//! then stripped of DefaultDuration with mkvpropedit. Sources: a 2 s 48 kHz
//! stereo sine through FFmpeg's encoders, and FATE's HE-AAC (SBR)
//! conformance stream. The `_ms` variants use 1 ms Segment ticks, where a
//! frame is not a whole number of ticks.

use std::path::{Path, PathBuf};
use std::process::Command;

use check_mkv::{differences, ffprobe_packets, our_packets};

enum Input {
    /// `ffmpeg` encoder arguments for the sine at a sample rate, and the
    /// intermediate file's extension.
    Sine(u32, &'static [&'static str], &'static str),
    /// A FATE sample, relative to the suite.
    Fate(&'static str),
}

const CASES: &[(&str, Input, &[&str])] = &[
    ("aac", Input::Sine(48000, &["-c:a", "aac", "-b:a", "128k"], "m4a"), &[]),
    ("aac_ms", Input::Sine(48000, &["-c:a", "aac", "-b:a", "128k"], "m4a"), &["--timestamp-scale", "1000000"]),
    ("heaac", Input::Fate("aac/al_sbr_cm_48_2.mp4"), &[]),
    ("mp3", Input::Sine(48000, &["-c:a", "libmp3lame", "-b:a", "128k"], "mp3"), &[]),
    ("mp3_24k", Input::Sine(24000, &["-c:a", "libmp3lame", "-b:a", "64k"], "mp3"), &[]),
    ("ac3", Input::Sine(48000, &["-c:a", "ac3", "-b:a", "192k"], "ac3"), &[]),
    ("eac3", Input::Sine(48000, &["-c:a", "eac3", "-b:a", "192k"], "eac3"), &[]),
    ("dts", Input::Sine(48000, &["-strict", "-2", "-c:a", "dca"], "dts"), &[]),
    ("dts_ms", Input::Sine(48000, &["-strict", "-2", "-c:a", "dca"], "dts"), &["--timestamp-scale", "1000000"]),
];

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap_or_else(|e| panic!("run {cmd:?}: {e}"));
    // mkvmerge exits 1 for warnings only.
    let warnings_only = cmd.get_program() == "mkvmerge" && out.status.code() == Some(1);
    assert!(out.status.success() || warnings_only, "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn fixture(dir: &Path, name: &str, input: &Input, mkvmerge: &[&str]) -> PathBuf {
    let path = dir.join(format!("laced_{name}.mka"));
    if path.is_file() {
        return path;
    }
    let source = match input {
        Input::Sine(rate, encoder, ext) => {
            let source = dir.join(format!("{name}.{ext}"));
            run(Command::new("ffmpeg")
                .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi"])
                .arg("-i").arg(format!("sine=frequency=440:sample_rate={rate}:duration=2"))
                .args(["-ac", "2"]).args(*encoder).arg(&source));
            source
        }
        Input::Fate(rel) => refcheck::fate(rel),
    };
    let tmp = dir.join(format!("laced_{name}.tmp.mka"));
    run(Command::new("mkvmerge").arg("-q").arg("-o").arg(&tmp).args(mkvmerge).arg(&source));
    run(Command::new("mkvpropedit").arg("-q").arg(&tmp).args(["--edit", "track:1", "--delete", "default-duration"]));
    std::fs::rename(&tmp, &path).unwrap();
    path
}

/// Whether FFmpeg reads laced Blocks from `path`: consecutive packets
/// from one Block share its position.
fn laced(path: &Path) -> bool {
    let out = Command::new(refcheck::pinned_ffmpeg().with_file_name("ffprobe"))
        .args(["-v", "error", "-show_entries", "packet=pos", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let pos: Vec<String> = String::from_utf8_lossy(&out.stdout).lines().map(str::to_owned).collect();
    pos.windows(2).any(|w| w[0] == w[1])
}

#[test]
fn laced_audio_without_default_duration_equals_ffprobe() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lace_timing");
    std::fs::create_dir_all(&dir).unwrap();
    let mut failures = Vec::new();
    for (name, input, mkvmerge) in CASES {
        let path = fixture(&dir, name, input, mkvmerge);
        let typed = oxideav_mkv::demux::open_typed(
            Box::new(std::fs::File::open(&path).unwrap()),
            &oxideav_core::NullCodecResolver,
        )
        .unwrap();
        assert_eq!(typed.track_timing(0).and_then(|t| t.default_duration()), None, "{name}: DefaultDuration kept");
        assert!(laced(&path), "{name}: no laced Blocks");
        let ours = our_packets(&path);
        let theirs = ffprobe_packets(&path, &[]);
        let diff = differences(&ours.packets, &theirs, &ours.error);
        println!("{name}: {} packets, {}", theirs.len(), if diff.is_empty() { "EXACT" } else { &diff });
        if !diff.is_empty() {
            let first = ours.packets.iter().zip(&theirs).position(|(a, b)| a != b);
            failures.push(format!("{name}: {diff}; first difference at packet {first:?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

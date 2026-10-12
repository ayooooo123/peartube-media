//! codec-mp1 against FFmpeg 2da55bf (`-cpuflags 0`, its default fixed-point
//! mp1 decoder). FATE has no Layer I file and no encoder here writes joint
//! stereo, so each test writes a stream of random valid Layer I frames
//! (tests/common) and plays it through the MPEG audio demuxer the player
//! opens `.mpa` files with: FFmpeg's samples byte for byte and its sample
//! count.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Spec, Version, stream};
use oxideav_core::{Frame, MediaType, SampleFormat};
use refcheck::{Registrar, decode, pinned_ffmpeg};

const REGISTRARS: &[Registrar] = &[codec_mp1::register, oxideav_mp3::__oxideav_entry];

fn ffmpeg_s16(path: &Path) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "s16le", "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Our S16P frames interleaved, as FFmpeg's s16le output is.
fn interleaved(frames: &[Frame]) -> Vec<u8> {
    let mut out = Vec::new();
    for f in frames {
        if let Frame::Audio(a) = f {
            for i in 0..a.samples as usize {
                for plane in &a.data {
                    out.extend_from_slice(&plane[2 * i..2 * i + 2]);
                }
            }
        }
    }
    out
}

fn check(name: &str, seed: u64, s: Spec) {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-mp1-{name}.mpa"));
    std::fs::write(&path, stream(seed, &s)).expect("write stream");
    let decoded = decode(&path, REGISTRARS, MediaType::Audio, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "mp1", "{name}: resolved codec");
    let format = decoded.audio_format.expect("output format");
    assert_eq!(format.sample_format, SampleFormat::S16P);
    let ours = interleaved(&decoded.frames);
    let theirs = ffmpeg_s16(&path);
    std::fs::remove_file(&path).ok();
    let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "{name}: first differing byte");
    assert_eq!(ours.len(), theirs.len(), "{name}: bytes");
    eprintln!(
        "{name}: {} samples/channel, {} ch at {} Hz, bit-exact",
        ours.len() / 2 / usize::from(format.channels),
        format.channels,
        format.sample_rate
    );
}

const BASE: Spec = Spec { version: Version::Mpeg1, rate_index: 1, bitrate_index: 14, mode: 0, crc: false, frames: 60, cut: 0 };

#[test]
fn mpeg1_stereo() {
    check("stereo", 1, BASE);
}

#[test]
fn mpeg1_joint_stereo_every_mode_extension_with_crc() {
    check("joint", 2, Spec { mode: 1, crc: true, rate_index: 0, bitrate_index: 12, ..BASE });
}

#[test]
fn mpeg1_dual_channel() {
    check("dual", 3, Spec { mode: 2, rate_index: 2, bitrate_index: 9, ..BASE });
}

#[test]
fn mpeg1_mono_low_bitrate_with_crc() {
    check("mono", 4, Spec { mode: 3, crc: true, rate_index: 2, bitrate_index: 1, ..BASE });
}

#[test]
fn mpeg2_lsf_rates() {
    for (rate_index, mode) in [(0, 1), (1, 0), (2, 3)] {
        let s = Spec { version: Version::Mpeg2, rate_index, bitrate_index: 7 + rate_index * 3, mode, ..BASE };
        check(&format!("mpeg2-{rate_index}"), 5 + u64::from(rate_index), s);
    }
}

#[test]
fn mpeg25_rates() {
    for rate_index in 0..3 {
        let s = Spec { version: Version::Mpeg25, rate_index, bitrate_index: 4 + rate_index, mode: 1, crc: true, ..BASE };
        check(&format!("mpeg25-{rate_index}"), 8 + u64::from(rate_index), s);
    }
}

/// A stream cut inside its last frame: FFmpeg decodes that frame over
/// zeros, a full 384 samples.
#[test]
fn cut_last_frame() {
    check("cut", 11, Spec { mode: 1, cut: 300, ..BASE });
}

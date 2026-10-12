//! codec-als against FFmpeg 2da55bf (`-cpuflags 0`): the MPEG-4 ALS
//! conformance files of tests/fate/als.mak (fate-mpeg4-als-conformance-00
//! to -05 and -09) and FATE's 32-bit float file, through the MP4 demuxer
//! the player opens them with: FFmpeg's samples byte for byte and its
//! sample count. als_09 has 512 channels, above the 64 the decoder takes:
//! it is refused.

use std::path::Path;
use std::process::Command;

use oxideav_core::{Frame, MediaType, SampleFormat};
use refcheck::{Registrar, decode, fate, pinned_ffmpeg};

const REGISTRARS: &[Registrar] = &[codec_als::register, oxideav_mp4::__oxideav_entry];

fn ffmpeg_pcm(path: &Path, f: &str) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", f, "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn pcm(frames: &[Frame]) -> Vec<u8> {
    frames
        .iter()
        .filter_map(|f| match f {
            Frame::Audio(a) => Some(a.data[0].as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect()
}

fn check(rel: &str) {
    let path = fate(rel);
    let decoded = decode(&path, REGISTRARS, MediaType::Audio, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "mp4als", "{rel}: resolved codec");
    let format = decoded.audio_format.expect("output format");
    let (f, bytes) = match format.sample_format {
        SampleFormat::S16 => ("s16le", 2),
        SampleFormat::S32 => ("s32le", 4),
        SampleFormat::F32 => ("f32le", 4),
        other => panic!("{rel}: unexpected format {other:?}"),
    };
    let ours = pcm(&decoded.frames);
    let theirs = ffmpeg_pcm(&path, f);
    let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "{rel}: first differing byte");
    assert_eq!(ours.len(), theirs.len(), "{rel}: bytes");
    eprintln!(
        "{rel}: {} samples/channel, {} ch {:?}, bit-exact",
        ours.len() / bytes / usize::from(format.channels),
        format.channels,
        format.sample_format
    );
}

#[test]
fn conformance_00() {
    check("lossless-audio/als_00_2ch48k16b.mp4");
}

#[test]
fn conformance_01() {
    check("lossless-audio/als_01_2ch48k16b.mp4");
}

#[test]
fn conformance_02() {
    check("lossless-audio/als_02_2ch48k16b.mp4");
}

#[test]
fn conformance_03() {
    check("lossless-audio/als_03_2ch48k16b.mp4");
}

#[test]
fn conformance_04() {
    check("lossless-audio/als_04_2ch48k16b.mp4");
}

#[test]
fn conformance_05() {
    check("lossless-audio/als_05_2ch48k16b.mp4");
}

#[test]
fn float_32bit_192k() {
    check("lossless-audio/als_07_2ch192k32bF.mp4");
}

/// 512 channels: above the 64 the decoder takes (FFmpeg takes up to 512).
#[test]
fn conformance_09_is_refused_for_its_512_channels() {
    let path = fate("lossless-audio/als_09_512ch2k16b.mp4");
    let mut ctx = oxideav_core::RuntimeContext::new();
    for r in REGISTRARS {
        r(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, &path).expect("probe");
    let file = std::fs::File::open(&path).expect("open");
    let demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).expect("open demuxer");
    let params = demuxer.streams()[0].params.clone();
    assert_eq!(params.codec_id.as_str(), "mp4als");
    let err = codec_als::AlsDecoder::new(&params).err().expect("refused");
    assert!(err.to_string().contains("512 channels"), "{err}");
}

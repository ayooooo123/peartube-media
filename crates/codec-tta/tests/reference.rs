//! codec-tta against FFmpeg 2da55bf (`-cpuflags 0`). The FATE samples of
//! tests/fate/lossless-audio.mak (fate-lossless-tta: inside.tta,
//! fate-lossless-tta-encrypted: encrypted.tta with password "ffmpeg"),
//! luckynight-partial.tta, and files FFmpeg's TTA encoder makes in each
//! sample format and several layouts: FFmpeg's samples byte for byte, its
//! sample count, and the demuxer's packets and seeks equal ffprobe's.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Demuxer, Frame, MediaType, RuntimeContext, SampleFormat};
use refcheck::{decode, fate, pinned_ffmpeg};

fn run(program: PathBuf, args: &[&str]) -> Vec<u8> {
    let out = Command::new(&program).args(["-v", "error"]).args(args).output().expect("pinned FFmpeg runs");
    assert!(out.status.success(), "{} {args:?}: {}", program.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    run(pinned_ffmpeg(), &[&["-nostdin"], args].concat())
}

fn ffprobe_csv(args: &[&str]) -> Vec<Vec<String>> {
    String::from_utf8(run(pinned_ffmpeg().with_file_name("ffprobe"), args))
        .expect("utf-8")
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| l.split(',').map(str::to_string).collect())
        .collect()
}

/// FFmpeg's decode in the decoder's own sample format.
fn ffmpeg_pcm(path: &Path, format: SampleFormat, input_args: &[&str]) -> Vec<u8> {
    let f = match format {
        SampleFormat::U8 => "u8",
        SampleFormat::S16 => "s16le",
        SampleFormat::S32 => "s32le",
        other => panic!("unexpected format {other:?}"),
    };
    let path = path.to_str().unwrap();
    let args = [&["-cpuflags", "0"], input_args, &["-i", path, "-map", "0:a:0", "-f", f, "-"]].concat();
    ffmpeg(&args)
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

/// Our samples against FFmpeg's: the bytes both have, then the count.
fn assert_same(name: &str, ours: &[u8], theirs: &[u8], frame_bytes: usize) -> usize {
    let first = ours.iter().zip(theirs).position(|(a, b)| a != b);
    assert_eq!(first, None, "{name}: first differing byte");
    assert_eq!(ours.len(), theirs.len(), "{name}: bytes");
    ours.len() / frame_bytes
}

/// `path` through the `tta` demuxer and decoder equals FFmpeg.
fn check(path: &Path) -> (usize, SampleFormat, u16) {
    let decoded = decode(path, &[codec_tta::register], MediaType::Audio, 0);
    let format = decoded.audio_format.expect("output format");
    let theirs = ffmpeg_pcm(path, format.sample_format, &[]);
    let n = assert_same(
        &path.display().to_string(),
        &pcm(&decoded.frames),
        &theirs,
        format.sample_format.bytes_per_sample() * usize::from(format.channels),
    );
    (n, format.sample_format, format.channels)
}

#[test]
fn inside_tta_matches_ffmpeg() {
    let (n, f, c) = check(&fate("lossless-audio/inside.tta"));
    eprintln!("inside.tta: {n} samples/channel, {c} ch {f:?}, bit-exact");
}

/// luckynight-partial.tta is cut inside its last frame: FFmpeg fails that
/// frame ("Decoding error") and keeps the nine before it, and so does
/// codec-tta.
#[test]
fn luckynight_tta_drops_its_cut_frame_as_ffmpeg_does() {
    let path = fate("lossless-audio/luckynight-partial.tta");
    let mut demuxer = open(&path);
    let params = demuxer.streams()[0].params.clone();
    let mut decoder = codec_tta::TtaDecoder::new(&params).expect("decoder");
    let (mut frames, mut failed) = (Vec::new(), Vec::new());
    let mut index = 0;
    while let Ok(p) = demuxer.next_packet() {
        if oxideav_core::Decoder::send_packet(&mut decoder, &p).is_err() {
            failed.push(index);
        }
        while let Ok(f) = oxideav_core::Decoder::receive_frame(&mut decoder) {
            frames.push(f);
        }
        index += 1;
    }
    assert_eq!(failed, [9], "the cut frame, the last of 10");
    let theirs = ffmpeg_pcm(&path, SampleFormat::S16, &[]);
    let n = assert_same("luckynight-partial.tta", &pcm(&frames), &theirs, 4);
    eprintln!("luckynight-partial.tta: {n} samples/channel, bit-exact; the cut last frame fails, as in FFmpeg");
}

/// encrypted.tta with its password, as fate-lossless-tta-encrypted plays
/// it; without one the decoder refuses the stream, as FFmpeg does.
#[test]
fn encrypted_tta_matches_ffmpeg_with_its_password() {
    let path = fate("lossless-audio/encrypted.tta");
    let mut ctx = RuntimeContext::new();
    codec_tta::register(&mut ctx);
    let file = std::fs::File::open(&path).expect("open");
    let mut demuxer = ctx.containers.open_demuxer("tta", Box::new(file), &ctx.codecs).expect("open demuxer");
    let mut params = demuxer.streams()[0].params.clone();
    assert!(codec_tta::TtaDecoder::new(&params).is_err(), "no password");
    params.options.insert("password", "ffmpeg".to_string());
    let mut decoder = codec_tta::TtaDecoder::new(&params).expect("decoder");
    let mut frames = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        oxideav_core::Decoder::send_packet(&mut decoder, &p).expect("send");
        while let Ok(f) = oxideav_core::Decoder::receive_frame(&mut decoder) {
            frames.push(f);
        }
    }
    let theirs = ffmpeg_pcm(&path, SampleFormat::S16, &["-password", "ffmpeg"]);
    let n = assert_same("encrypted.tta", &pcm(&frames), &theirs, 4);
    eprintln!("encrypted.tta: {n} samples/channel, bit-exact");
}

/// A file FFmpeg's TTA encoder makes from `channels` noise channels at
/// `rate` Hz in `sample_fmt`, `seconds` long.
fn encoded(name: &str, sample_fmt: &str, channels: u32, rate: u32, seconds: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-tta-{name}.tta"));
    let source = format!("anoisesrc=r={rate}:a=0.5:c=pink:seed=7:d={seconds}");
    let layout = format!("aformat=channel_layouts={channels}c");
    ffmpeg(&[
        "-y",
        "-f",
        "lavfi",
        "-i",
        &source,
        "-af",
        &format!("{layout},volume=0.9"),
        "-sample_fmt",
        sample_fmt,
        "-c:a",
        "tta",
        path.to_str().unwrap(),
    ]);
    path
}

/// 8-bit, 16-bit and 24-bit, mono to 8 channels, rates whose frames are
/// not whole numbers of milliseconds, and lengths that end mid-frame.
#[test]
fn encoder_made_files_match_ffmpeg() {
    for (name, fmt, channels, rate, seconds, want) in [
        ("u8-mono", "u8", 1, 22050, "3.3", SampleFormat::U8),
        ("s16-stereo", "s16", 2, 44100, "4.01", SampleFormat::S16),
        ("s32-24bit-stereo", "s32", 2, 96000, "2.7", SampleFormat::S32),
        ("s16-6ch", "s16", 6, 48000, "2.2", SampleFormat::S16),
        ("s32-24bit-8ch", "s32", 8, 32000, "1.9", SampleFormat::S32),
        ("s16-3ch-odd-rate", "s16", 3, 11025, "5.55", SampleFormat::S16),
    ] {
        let path = encoded(name, fmt, channels, rate, seconds);
        let (n, f, c) = check(&path);
        assert_eq!((f, u32::from(c)), (want, channels), "{name}");
        eprintln!("{name}: {n} samples/channel, bit-exact");
    }
}

fn open(path: &Path) -> Box<dyn Demuxer> {
    let mut ctx = RuntimeContext::new();
    codec_tta::register(&mut ctx);
    assert_eq!(refcheck::probe_container(&ctx, path).as_deref(), Ok("tta"));
    let file = std::fs::File::open(path).expect("open");
    ctx.containers.open_demuxer("tta", Box::new(file), &ctx.codecs).expect("open tta demuxer")
}

#[test]
fn packets_equal_ffprobe() {
    for rel in ["lossless-audio/inside.tta", "lossless-audio/luckynight-partial.tta"] {
        let path = fate(rel);
        let expected =
            ffprobe_csv(&["-select_streams", "a:0", "-show_entries", "packet=pts,duration,size", "-of", "csv=p=0", path.to_str().unwrap()]);
        let mut demuxer = open(&path);
        let mut ours = Vec::new();
        while let Ok(p) = demuxer.next_packet() {
            ours.push(vec![p.pts.unwrap().to_string(), p.duration.unwrap().to_string(), p.data.len().to_string()]);
        }
        assert_eq!(ours, expected, "{rel}");
    }
}

#[test]
fn seeks_land_where_ffprobe_lands() {
    let path = fate("lossless-audio/inside.tta");
    let rate = 44100.0;
    for seconds in ["0", "1.5", "7.3", "100000"] {
        let expected = ffprobe_csv(&[
            "-select_streams",
            "a:0",
            "-read_intervals",
            &format!("{seconds}%+#1"),
            "-show_entries",
            "packet=pts,size",
            "-of",
            "csv=p=0",
            path.to_str().unwrap(),
        ]);
        let mut demuxer = open(&path);
        while demuxer.next_packet().is_ok() {}
        let target = (seconds.parse::<f64>().unwrap() * rate) as i64;
        let landed = demuxer.seek_to(0, target).expect("seek");
        let p = demuxer.next_packet().expect("packet after seek");
        assert_eq!(
            vec![landed.to_string(), p.data.len().to_string()],
            vec![expected[0][0].clone(), expected[0][1].clone()],
            "seek to {seconds} s"
        );
    }
}

/// An ID3v2 tag before the header, as FFmpeg skips it (version 4 with a
/// footer, then version 3).
#[test]
fn id3v2_tags_before_the_header_are_skipped() {
    let mut file = Vec::new();
    for (version, flags, body) in [(4u8, 0x10u8, 300usize), (3, 0, 40)] {
        file.extend_from_slice(b"ID3");
        file.extend_from_slice(&[version, 0, flags]);
        let size = body as u32;
        file.extend_from_slice(&[(size >> 21 & 0x7F) as u8, (size >> 14 & 0x7F) as u8, (size >> 7 & 0x7F) as u8, (size & 0x7F) as u8]);
        file.extend(std::iter::repeat_n(0u8, body));
        if flags & 0x10 != 0 {
            file.extend_from_slice(b"3DI");
            file.extend_from_slice(&[version, 0, flags, 0, 0, 2, 44]);
        }
    }
    file.extend_from_slice(&std::fs::read(fate("lossless-audio/inside.tta")).expect("read"));
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("codec-tta-id3v2.tta");
    std::fs::write(&path, &file).expect("write");
    let (n, _, _) = check(&path);
    eprintln!("ID3v2-tagged inside.tta: {n} samples/channel, bit-exact");
}

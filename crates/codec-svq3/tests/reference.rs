//! svq3 against FFmpeg 2da55bf on the samples tests/fate/qt.mak decodes
//! (all in QuickTime): every frame's MD5 equals FFmpeg's, frame for frame,
//! and the decoder reports FFmpeg's pixel format (yuvj420p) and size.

use std::path::Path;
use std::process::Command;

use oxideav_core::{Frame, MediaType, PixelFormat, RuntimeContext};
use refcheck::{fate, Registrar};

fn mov(ctx: &mut RuntimeContext) {
    oxideav_mov::registry::register(ctx);
}

const REGISTRARS: [Registrar; 2] = [codec_svq3::register, mov];

/// The pinned FFmpeg's frame MD5s, `input_args` before its `-i`.
fn ffmpeg_md5s(path: &Path, input_args: &[&str]) -> Vec<String> {
    let args = refcheck::ffmpeg_video_md5_args(path, "0:v:0", "yuvj420p", input_args);
    let out = Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(&args).output().expect("the pinned FFmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    refcheck::parse_framemd5(&String::from_utf8(out.stdout).unwrap())
}

/// The decoder's frames as MD5s of their (cropped) planes, and what it
/// reports after its first frame.
fn ours(path: &Path) -> (Vec<String>, Option<PixelFormat>, Option<(u32, u32)>) {
    let decoded = refcheck::decode(path, &REGISTRARS, MediaType::Video, 0);
    let mut ctx = RuntimeContext::new();
    for r in REGISTRARS {
        r(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let d = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    let (w, h) = decoder.output_video_dimensions().expect("the size from the SEQH header");
    let (w, h) = (w as usize, h as usize);
    let dims = [(w, h), (w.div_ceil(2), h.div_ceil(2)), (w.div_ceil(2), h.div_ceil(2))];
    let md5s = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    (md5s, decoder.output_pixel_format(), decoder.output_video_dimensions())
}

/// Every frame equals FFmpeg's from `from` on.
fn check(sample: &str, size: (u32, u32), input_args: &[&str], from: usize) {
    let path = fate(sample);
    let (got, format, dims) = ours(&path);
    assert_eq!((format, dims), (Some(PixelFormat::YuvJ420P), Some(size)), "{sample}: what the decoder reports");
    let want = ffmpeg_md5s(&path, input_args);
    assert!(want.len() > from, "{sample}: FFmpeg's frames");
    assert_eq!(got.len(), want.len(), "{sample}: frames");
    let first = got.iter().zip(&want).skip(from).position(|(g, w)| g != w);
    assert!(first.is_none(), "{sample}: frame {:?} of {} differs from FFmpeg's", first.map(|f| f + from), got.len());
}

/// fate-svq3-1: half-pel and third-pel motion, B pictures.
#[test]
fn vertical_400kbit() {
    check("svq3/Vertical400kbit.sorenson3.mov", (320, 240), &[], 0);
}

/// fate-svq3-watermark: the slice headers are XORed with the key from the
/// zlib-compressed logo in the SEQH header.
#[test]
fn watermark() {
    check("svq3/svq3_watermark.mov", (284, 240), &["-flags", "+bitexact"], 0);
}

/// fate-svq3-2 (disabled in FATE: its first frame is a dummy reference
/// whose last chroma row FFmpeg leaves uninitialised, 257 rows high): from
/// the second frame on. Its edit list leaves no frame in FFmpeg, as in
/// FATE's command line it is ignored.
#[test]
fn decoding_regression() {
    check("svq3/svq3_decoding_regression.mov", (480, 257), &["-flags", "+bitexact", "-ignore_editlist", "1"], 1);
}

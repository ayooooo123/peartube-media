//! svq1 against FFmpeg 2da55bf on the samples tests/fate/qt.mak decodes:
//! every frame's MD5 equals FFmpeg's, frame for frame, in FFmpeg's yuv410p,
//! and the decoder reports that format and the picture's size.

use std::path::Path;
use std::process::Command;

use oxideav_core::{Frame, MediaType, PixelFormat, RuntimeContext};
use refcheck::{fate, Registrar};

fn mov(ctx: &mut RuntimeContext) {
    oxideav_mov::registry::register(ctx);
}

const REGISTRARS: [Registrar; 2] = [codec_svq1::register, mov];

/// The pinned FFmpeg's frame MD5s.
fn ffmpeg_md5s(path: &Path) -> Vec<String> {
    let args = refcheck::ffmpeg_video_md5_args(path, "0:v:0", "yuv410p", &[]);
    let out = Command::new(refcheck::pinned_ffmpeg()).args(["-v", "error", "-nostdin"]).args(&args).output().expect("the pinned FFmpeg");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    refcheck::parse_framemd5(&String::from_utf8(out.stdout).unwrap())
}

/// Every frame equals FFmpeg's; the decoder reports yuv410p at `size`.
fn check(sample: &str, size: (u32, u32)) {
    let path = fate(sample);
    let decoded = refcheck::decode(&path, &REGISTRARS, MediaType::Video, 0);
    let (w, h) = (size.0 as usize, size.1 as usize);
    let dims = [(w, h), (w.div_ceil(4), h.div_ceil(4)), (w.div_ceil(4), h.div_ceil(4))];
    let got: Vec<String> = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("{sample}: not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    let want = ffmpeg_md5s(&path);
    assert!(!want.is_empty(), "{sample}: FFmpeg's frames");
    let first = got.iter().zip(&want).position(|(g, w)| g != w);
    assert!(first.is_none(), "{sample}: frame {first:?} of {} differs from FFmpeg's", got.len());
    assert_eq!(got.len(), want.len(), "{sample}: frames");

    let mut ctx = RuntimeContext::new();
    for r in REGISTRARS {
        r(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, &path).unwrap();
    let mut d = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs).unwrap();
    let stream = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    while let Ok(p) = d.next_packet() {
        if p.stream_index == stream.index {
            decoder.send_packet(&p).unwrap();
            break;
        }
    }
    assert_eq!(
        (decoder.output_pixel_format(), decoder.output_video_dimensions()),
        (Some(PixelFormat::Yuv410P), Some(size)),
        "{sample}: what the decoder reports"
    );
}

/// fate-svq1 (FATE compares 10 s; every frame here): 160x120, the
/// standard size from the frame header's table, inter and 4-vector
/// blocks.
#[test]
fn marymary_shackles() {
    check("svq1/marymary-shackles.mov", (160, 120));
}

/// fate-svq1-headerswap (FATE compares 4 frames; all 20 here): frame code
/// 0x60, so the header bytes are swapped and a packet checksum and the
/// encoder's embedded message precede a 12-bit size, 293x178, whose
/// chroma is 74x45.
#[test]
fn ct_ending_cut_headerswap() {
    check("svq1/ct_ending_cut.mov", (293, 178));
}

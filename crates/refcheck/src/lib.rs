//! Test helper for the codec crates: decodes a sample through OxideAV's
//! registries and FFmpeg's command line, and compares the two.
//!
//! Samples come from FFmpeg's FATE suite. Set `FATE_SUITE` to its directory
//! (default `~/projects/fate-suite`); fetch it with
//! `rsync -rlt rsync://fate-suite.ffmpeg.org/fate-suite/ ~/projects/fate-suite/`.
//! A missing sample fails the test: a reference test that skips proves nothing.

use oxideav_core::{
    CodecParameters, Error, Frame, MediaType, PROBE_SCORE_EXTENSION, PixelFormat, ProbeData, RuntimeContext,
    SampleFormat, VideoFrame,
};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Registers codecs and containers into a context, e.g. `oxideav_mkv::register`.
pub type Registrar = fn(&mut RuntimeContext);

/// Absolute path of a FATE sample, e.g. `fate("truehd/atmos.thd")`. Panics
/// with instructions when the suite or the file is missing.
pub fn fate(relative: &str) -> PathBuf {
    let root = std::env::var_os("FATE_SUITE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/fate-suite"));
    let path = root.join(relative);
    assert!(
        path.is_file(),
        "missing FATE sample {} (set FATE_SUITE or rsync rsync://fate-suite.ffmpeg.org/fate-suite/ into {})",
        path.display(),
        root.display()
    );
    path
}

/// One decoded stream: its parameters and every frame, in output order.
pub struct Decoded {
    pub params: CodecParameters,
    pub frames: Vec<Frame>,
}

/// Opens `path` with the containers and codecs that `registrars` install,
/// picks the `nth` stream of `kind`, and decodes all of it.
pub fn decode(path: &Path, registrars: &[Registrar], kind: MediaType, nth: usize) -> Decoded {
    let mut ctx = RuntimeContext::new();
    for register in registrars {
        register(&mut ctx);
    }
    let mut head = vec![0; 256 * 1024];
    let n = File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let probe = ProbeData { buf: &head[..n], ext: ext.as_deref() };
    let candidates = ctx.containers.probe_candidates(&probe);
    // Below an extension match a probe is guessing (EBU STL scores 5 on any
    // input); the player rejects those too.
    let by_extension = ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e));
    let format = match (candidates.first(), by_extension) {
        (Some(c), _) if c.score >= PROBE_SCORE_EXTENSION => c.name.to_string(),
        (_, Some(name)) => name.to_string(),
        _ => panic!(
            "{}: no container claims it (candidates: {:?})",
            path.display(),
            candidates.iter().map(|c| (c.name, c.score)).collect::<Vec<_>>()
        ),
    };
    let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut demuxer = ctx
        .containers
        .open_demuxer(&format, Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("open {format} demuxer: {e}"));
    let stream = demuxer
        .streams()
        .iter()
        .filter(|s| s.params.media_type == kind)
        .nth(nth)
        .unwrap_or_else(|| panic!("{}: no {kind:?} stream #{nth}", path.display()))
        .clone();
    let mut decoder = ctx
        .codecs
        .first_decoder(&stream.params)
        .unwrap_or_else(|e| panic!("no decoder for {:?}: {e}", stream.params.codec_id));
    let mut frames = Vec::new();
    let drain = |decoder: &mut Box<dyn oxideav_core::Decoder>, frames: &mut Vec<Frame>| loop {
        match decoder.receive_frame() {
            Ok(frame) => frames.push(frame),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("decode: {e}"),
        }
    };
    loop {
        match demuxer.next_packet() {
            Ok(packet) if packet.stream_index == stream.index => {
                decoder.send_packet(&packet).unwrap_or_else(|e| panic!("send_packet: {e}"));
                drain(&mut decoder, &mut frames);
            }
            Ok(_) => {}
            Err(Error::Eof) => break,
            Err(e) => panic!("demux: {e}"),
        }
    }
    decoder.flush().unwrap_or_else(|e| panic!("flush: {e}"));
    drain(&mut decoder, &mut frames);
    Decoded { params: stream.params, frames }
}

/// FFmpeg's name for a pixel format, for `-pix_fmt`.
pub fn ffmpeg_pix_fmt(format: PixelFormat) -> &'static str {
    match format {
        PixelFormat::Yuv420P => "yuv420p",
        PixelFormat::Yuv422P => "yuv422p",
        PixelFormat::Yuv444P => "yuv444p",
        PixelFormat::Yuv440P => "yuv440p",
        PixelFormat::Yuv411P => "yuv411p",
        PixelFormat::Yuv420P10Le => "yuv420p10le",
        PixelFormat::Yuv422P10Le => "yuv422p10le",
        PixelFormat::Yuv444P10Le => "yuv444p10le",
        PixelFormat::Gray8 => "gray",
        PixelFormat::Rgb24 => "rgb24",
        PixelFormat::Bgr24 => "bgr24",
        PixelFormat::Rgba => "rgba",
        PixelFormat::Pal8 => "pal8",
        PixelFormat::Nv12 => "nv12",
        other => panic!("refcheck: add FFmpeg's name for {other:?}"),
    }
}

/// The frame's image planes packed row by row without stride padding, the
/// layout FFmpeg's rawvideo and framemd5 use. `plane_dims` gives each
/// plane's (bytes per row, rows).
pub fn pack(frame: &VideoFrame, plane_dims: &[(usize, usize)]) -> Vec<u8> {
    let planes = frame.image_planes();
    assert_eq!(planes.len(), plane_dims.len(), "plane count");
    let mut out = Vec::new();
    for (plane, &(row_bytes, rows)) in planes.iter().zip(plane_dims) {
        for row in 0..rows {
            let start = row * plane.stride;
            out.extend_from_slice(&plane.data[start..start + row_bytes]);
        }
    }
    out
}

/// MD5 of every video frame FFmpeg decodes from stream `0:v:nth`, in the
/// given pixel format, in output order. Cropping the bitstream signals (SPS
/// cropping) applies, as decoders output it; container cropping (MOV `clap`)
/// does not, because the player applies that at presentation. `-fps_mode
/// passthrough` keeps FFmpeg from duplicating or dropping frames.
pub fn ffmpeg_video_md5s(path: &Path, nth: usize, pix_fmt: &str) -> Vec<String> {
    ffmpeg_video_md5s_with(path, nth, pix_fmt, &[])
}

/// [`ffmpeg_video_md5s`] with extra decoder options placed before `-i`, e.g.
/// `&["-idct", "simple"]` to pin FFmpeg's C IDCT: on arm64 its default picks
/// NEON assembly whose rounding differs from the C reference.
pub fn ffmpeg_video_md5s_with(path: &Path, nth: usize, pix_fmt: &str, input_args: &[&str]) -> Vec<String> {
    let map = format!("0:v:{nth}");
    let mut args = vec!["-apply_cropping", "codec"];
    args.extend_from_slice(input_args);
    args.extend_from_slice(&[
        "-i", path.to_str().unwrap(), "-map", &map, "-fps_mode", "passthrough", "-pix_fmt", pix_fmt, "-f",
        "framemd5", "-",
    ]);
    String::from_utf8(ffmpeg(&args))
        .unwrap()
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
        .collect()
}

/// Hex MD5 of `bytes`.
pub fn md5_hex(bytes: &[u8]) -> String {
    format!("{:x}", md5::compute(bytes))
}

/// FFmpeg's decode of stream `0:a:nth` as interleaved f32 at the source rate
/// and channel count.
pub fn ffmpeg_audio_f32(path: &Path, nth: usize) -> Vec<f32> {
    let out = ffmpeg(&[
        "-i", path.to_str().unwrap(), "-map", &format!("0:a:{nth}"), "-f", "f32le", "-c:a", "pcm_f32le", "-",
    ]);
    out.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// Every audio frame converted to interleaved f32 in [-1, 1].
pub fn interleaved_f32(decoded: &Decoded) -> Vec<f32> {
    let channels = decoded.params.channels.unwrap_or(1) as usize;
    let format = decoded.params.sample_format.expect("audio stream without sample_format");
    let mut out = Vec::new();
    for frame in &decoded.frames {
        let Frame::Audio(a) = frame else { continue };
        let n = a.samples as usize;
        for i in 0..n {
            for c in 0..channels {
                out.push(sample_f32(format, &a.data, channels, c, i));
            }
        }
    }
    out
}

fn sample_f32(format: SampleFormat, data: &[Vec<u8>], channels: usize, c: usize, i: usize) -> f32 {
    let (plane, index) = if format.is_planar() { (&data[c], i) } else { (&data[0], i * channels + c) };
    let w = format.bytes_per_sample();
    let b = &plane[index * w..index * w + w];
    match format {
        SampleFormat::U8 | SampleFormat::U8P => (b[0] as f32 - 128.0) / 128.0,
        SampleFormat::S16 | SampleFormat::S16P => i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0,
        SampleFormat::S32 | SampleFormat::S32P => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2147483648.0,
        SampleFormat::F32 | SampleFormat::F32P => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        SampleFormat::S24 => (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8) as f32 / 8388608.0,
        SampleFormat::F64 | SampleFormat::F64P => f64::from_le_bytes(b.try_into().unwrap()) as f32,
        other => panic!("refcheck: add conversion for {other:?}"),
    }
}

/// Signal-to-noise ratio of `test` against `reference`, in dB, over the
/// common length. Lengths must agree within `slack` samples.
pub fn snr_db(reference: &[f32], test: &[f32], slack: usize) -> f64 {
    assert!(
        reference.len().abs_diff(test.len()) <= slack,
        "length {} vs FFmpeg {} (slack {slack})",
        test.len(),
        reference.len()
    );
    let n = reference.len().min(test.len());
    let (mut signal, mut noise) = (0f64, 0f64);
    for i in 0..n {
        let r = reference[i] as f64;
        signal += r * r;
        noise += (r - test[i] as f64).powi(2);
    }
    if noise == 0.0 { f64::INFINITY } else { 10.0 * (signal / noise).log10() }
}

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin"])
        .args(args)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

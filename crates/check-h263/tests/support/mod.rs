//! What the reference and damage tests share: the inputs (FATE's H.263
//! encode/decode streams, regenerated as FATE makes them, and files from
//! FFmpeg's sample archive), the player's registry, and FFmpeg 2da55bf as
//! the oracle.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use oxideav_core::{CodecId, CodecParameters, Decoder, Error, Frame, MediaType, Packet, TimeBase};
use refcheck::{Decoded, Registrar, VideoLayout};

/// The player's registry: every codec and container it installs, the
/// forked `oxideav-h263` among them.
pub const REGISTRARS: [Registrar; 1] = [codecs::register_all];

pub fn ffmpeg_src() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
}

/// A file from FFmpeg's sample archive (samples.ffmpeg.org), hash-pinned
/// in `tests/data/ffmpeg-samples`: `$FFMPEG_SAMPLES/<archive path>`,
/// default ~/projects/oracles/ffmpeg-samples.
pub fn archive(relative: &str) -> PathBuf {
    let root = std::env::var_os("FFMPEG_SAMPLES")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/oracles/ffmpeg-samples"));
    let path = root.join(relative);
    assert!(path.is_file(), "missing {}: fetch it as tests/data/ffmpeg-samples/README.md says", path.display());
    path
}

fn run(binary: &Path, args: &[&str]) -> Vec<u8> {
    let out = Command::new(binary).args(args).output().unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(out.status.success(), "{} {args:?}: {}", binary.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// Generation is serialised: tests run on threads of one process, and
/// two of them may want the same file.
static GENERATING: Mutex<()> = Mutex::new(());

/// `name` in the persistent scratch directory, made by `make` (given the
/// path to write) on first use and published by rename.
fn generated(name: &str, make: impl FnOnce(&Path)) -> PathBuf {
    let _guard = GENERATING.lock().unwrap_or_else(|e| e.into_inner());
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("check-h263");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.is_file() {
        let partial = dir.join(format!("{}.{name}", std::process::id()));
        make(&partial);
        std::fs::rename(&partial, &path).unwrap();
    }
    path
}

/// FFmpeg's FATE source generator `tests/<name>.c`, built from
/// `$FFMPEG_SRC` by the C compiler on PATH.
fn fate_tool(name: &str) -> PathBuf {
    generated(name, |out| {
        let source = ffmpeg_src().join("tests").join(format!("{name}.c"));
        let status = Command::new("cc").arg("-O2").arg("-o").arg(out).arg(&source).status().expect("cc");
        assert!(status.success(), "cc {}", source.display());
    })
}

/// FATE's 352x288 YUV 4:2:0 source `tests/data/<source>.yuv`, made as
/// `tests/Makefile` makes it: `vsynth1` by videogen, `vsynth2` by rotozoom
/// from `tests/reference.pnm`, `vsynth_lena` by rotozoom from FATE's
/// `lena.pnm`.
fn vsynth_yuv(source: &str) -> PathBuf {
    let (tool, input) = match source {
        "vsynth1" => (fate_tool("videogen"), None),
        "vsynth2" => (fate_tool("rotozoom"), Some(ffmpeg_src().join("tests/reference.pnm"))),
        "vsynth_lena" => (fate_tool("rotozoom"), Some(refcheck::fate("lena.pnm"))),
        _ => panic!("no FATE source {source}"),
    };
    generated(&format!("{source}.yuv"), |out| {
        let mut args = Vec::new();
        if let Some(input) = &input {
            args.push(input.to_str().unwrap());
        }
        args.push(out.to_str().unwrap());
        run(&tool, &args);
    })
}

/// One of FATE's H.263 encode/decode tests, `fate-<source>-<test>`
/// (vcodec.mak: `h263` at `-qscale 10`, `h263-obmc` adding `-obmc 1`,
/// `h263p` at `-qscale 2 -flags +aic -umv 1 -aiv 1 -ps 300`): the AVI
/// file its encode step writes, made by FFmpeg 2da55bf with
/// `fate-run.sh`'s exact command and checked against the stream MD5 in
/// FATE's reference, and the MD5 FATE records for its decode (every
/// frame as raw YUV 4:2:0).
pub fn fate_vsynth(source: &str, test: &str) -> (PathBuf, String) {
    let name = format!("{source}-{test}");
    let reference = std::fs::read_to_string(ffmpeg_src().join("tests/ref/vsynth").join(&name)).unwrap();
    // "<md5> *<avi>", "<size> <avi>", "<md5> *<out.rawvideo>", "stddev: ..."
    let tokens: Vec<&str> = reference.split_whitespace().collect();
    let (stream_md5, decoded_md5) = (tokens[0], tokens[4]);
    let encoder: &[&str] = match test {
        "h263" => &["-c", "h263", "-qscale", "10"],
        "h263-obmc" => &["-c", "h263", "-qscale", "10", "-obmc", "1"],
        "h263p" => &["-c", "h263p", "-qscale", "2", "-flags", "+aic", "-umv", "1", "-aiv", "1", "-ps", "300"],
        _ => panic!("no FATE test {test}"),
    };
    let yuv = vsynth_yuv(source);
    let path = generated(&format!("{name}.avi"), |out| {
        let flags = ["-flags", "+bitexact", "-sws_flags", "+accurate_rnd+bitexact", "-fflags", "+bitexact"];
        let mut args = vec!["-v", "error", "-nostdin", "-nostats", "-noauto_conversion_filters", "-cpuflags", "all"];
        args.extend(["-auto_conversion_filters", "-f", "rawvideo", "-s", "352x288", "-color_range", "mpeg"]);
        args.extend(["-pix_fmt", "yuv420p", "-chroma_sample_location", "center"]);
        // DEC_OPTS, then the decoder options fate-run.sh's ffmpeg() puts
        // before every -i.
        args.extend(["-threads", "1", "-thread_type", "frame+slice", "-idct", "simple"]);
        args.extend(flags);
        args.extend(["-hwaccel", "none", "-threads", "1", "-thread_type", "frame+slice"]);
        args.extend(["-i", yuv.to_str().unwrap()]);
        // ENC_OPTS, the test's encoder options, FLAGS.
        args.extend(["-threads", "1", "-idct", "simple", "-dct", "fastint"]);
        args.extend(encoder);
        args.extend(flags);
        args.extend(["-f", "avi", "-y", out.to_str().unwrap()]);
        run(&refcheck::pinned_ffmpeg(), &args);
    });
    let made = refcheck::md5_hex(&std::fs::read(&path).unwrap());
    assert_eq!(made, stream_md5, "{name}: the regenerated stream is not FATE's");
    (path, decoded_md5.to_string())
}

/// `source` remuxed by FFmpeg 2da55bf (`-c copy`, so every packet as
/// it is) with `output_args`, on first use.
pub fn remux(name: &str, source: &Path, output_args: &[&str]) -> PathBuf {
    generated(name, |out| {
        let mut args = vec!["-v", "error", "-nostdin", "-i", source.to_str().unwrap()];
        args.extend_from_slice(output_args);
        args.push(out.to_str().unwrap());
        run(&refcheck::pinned_ffmpeg(), &args);
    })
}

/// FFmpeg 2da55bf's framemd5 of `0:v:0` in YUV 4:2:0, through its C IDCT
/// (`-idct simple`; its arm64 default is NEON assembly that rounds
/// differently), `input_args` before `-i`.
pub fn ffmpeg_md5s(path: &Path, input_args: &[&str]) -> Vec<String> {
    let mut input = vec!["-idct", "simple"];
    input.extend_from_slice(input_args);
    let oracle = refcheck::ffmpeg_video_md5_args(path, "0:v:0", "yuv420p", &input);
    let mut args = vec!["-v", "error", "-nostdin"];
    args.extend(oracle.iter().map(String::as_str));
    refcheck::parse_framemd5(&String::from_utf8(run(&refcheck::pinned_ffmpeg(), &args)).unwrap())
}

/// One frame packed as framemd5 packs it, at the size and in the format
/// its decoder reported.
fn packed(frame: &Frame, layout: &VideoLayout, at: &str) -> Vec<u8> {
    let Frame::Video(vf) = frame else { panic!("{at}: not a video frame") };
    let (Some((w, h)), Some(format)) = *layout else { panic!("{at}: decoder reported {layout:?}") };
    let dims: Vec<(usize, usize)> = (0..format.plane_count())
        .map(|p| {
            let row = format.plane_row_bytes(p, w).expect("plane row bytes");
            let (_, rows) = format.plane_dimensions(p, w, h).expect("plane geometry");
            (row, rows as usize)
        })
        .collect();
    refcheck::pack(vf, &dims)
}

/// Every frame packed, after checking each against its layout report.
pub fn packed_frames(decoded: &Decoded, name: &str) -> Vec<Vec<u8>> {
    refcheck::assert_reports_match_frames(decoded, name);
    decoded
        .frames
        .iter()
        .zip(&decoded.frame_video_layouts)
        .enumerate()
        .map(|(i, (frame, layout))| packed(frame, layout, &format!("{name}: frame {i}")))
        .collect()
}

/// Every frame's MD5, after checking each against its layout report.
pub fn frame_md5s(decoded: &Decoded, name: &str) -> Vec<String> {
    packed_frames(decoded, name).iter().map(|frame| refcheck::md5_hex(frame)).collect()
}

/// `path`'s first video stream decoded through the player's registry.
pub fn decode(path: &Path) -> Decoded {
    refcheck::decode(path, &REGISTRARS, MediaType::Video, 0)
}

/// A raw elementary stream of codec `codec`, which no player container
/// reads, fed to the decoder `chunk` bytes at a time: every frame's MD5
/// and reported size.
pub fn decode_raw(path: &Path, codec: &str, chunk: usize) -> Vec<(String, (u32, u32))> {
    let ctx = codecs::context();
    let mut decoder = ctx.codecs.first_decoder(&CodecParameters::video(CodecId::new(codec))).unwrap();
    let data = std::fs::read(path).unwrap();
    let mut out = Vec::new();
    let drain = |decoder: &mut Box<dyn Decoder>, out: &mut Vec<(String, (u32, u32))>| loop {
        match decoder.receive_frame() {
            Ok(frame) => {
                let layout = (decoder.output_video_dimensions(), decoder.output_pixel_format());
                let at = format!("{}: frame {}", path.display(), out.len());
                out.push((refcheck::md5_hex(&packed(&frame, &layout, &at)), layout.0.unwrap()));
            }
            Err(Error::NeedMore | Error::Eof) => return,
            Err(e) => panic!("receive_frame: {e}"),
        }
    };
    for piece in data.chunks(chunk) {
        decoder.send_packet(&Packet::new(0, TimeBase::MICROS, piece.to_vec())).unwrap();
        drain(&mut decoder, &mut out);
    }
    decoder.flush().unwrap();
    drain(&mut decoder, &mut out);
    out
}

/// `ours` equals `theirs` frame for frame; otherwise the counts and the
/// differing frames.
pub fn assert_frames_equal(name: &str, ours: &[String], theirs: &[String]) {
    let differing: Vec<usize> = ours.iter().zip(theirs).enumerate().filter(|(_, (a, b))| a != b).map(|(i, _)| i).collect();
    assert!(
        ours.len() == theirs.len() && differing.is_empty(),
        "{name}: {} frames, FFmpeg {}; {} differ, the first {:?}",
        ours.len(),
        theirs.len(),
        differing.len(),
        &differing[..differing.len().min(8)]
    );
    eprintln!("{name}: {} frames, every one equal to FFmpeg's", ours.len());
}

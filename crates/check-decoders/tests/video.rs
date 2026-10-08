//! Video decoders against FFmpeg 2da55bf on its C code paths (`-idct
//! simple -cpuflags 0`), through the registry the player uses
//! (`codecs::register_all`): the decoder must report FFmpeg's pixel format
//! and size, and every frame, packed as FFmpeg's framemd5 hashes it, must
//! equal FFmpeg's, frame for frame.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{Frame, MediaType};

fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin"])
        .args(args)
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// FFmpeg's decoded pixel format and size of the first video stream.
fn ffmpeg_format(path: &Path) -> (String, u32, u32) {
    let out = Command::new(refcheck::pinned_ffmpeg().with_file_name("ffprobe"))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt,width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("pinned ffprobe runs");
    let text = String::from_utf8(out.stdout).unwrap();
    let f: Vec<&str> = text.trim().split(',').collect();
    (f[2].to_string(), f[0].parse().unwrap(), f[1].parse().unwrap())
}

/// Frame-exact check of `path`'s first video stream: `Err` names the first
/// difference.
fn check(path: &Path) -> Result<String, String> {
    // refcheck's decode loop panics on a decoder error; report it as this
    // case's failure so the other cases still run.
    let decoded = std::panic::catch_unwind(|| refcheck::decode(path, &[codecs::register_all], MediaType::Video, 0))
        .map_err(|e| format!("decode failed: {}", e.downcast_ref::<String>().cloned().unwrap_or_default()))?;
    let mut ours = Vec::new();
    let mut layout = None;
    for (frame, &(size, format)) in decoded.frames.iter().zip(&decoded.frame_video_layouts) {
        let Frame::Video(vf) = frame else { return Err("not a video frame".into()) };
        let (w, h) = size.or(decoded.params.width.zip(decoded.params.height)).ok_or("no size")?;
        let format = format.or(decoded.params.pixel_format).ok_or("no pixel format")?;
        layout.get_or_insert((format, w, h));
        ours.push(refcheck::md5_hex(&player::headless::pack_frame(vf, format, w, h)));
    }
    let (format, w, h) = layout.ok_or("no frame decoded")?;
    let name = refcheck::ffmpeg_pix_fmt_name(format).ok_or(format!("{format:?} has no FFmpeg name"))?;
    let (theirs_fmt, tw, th) = ffmpeg_format(path);
    if (name, w, h) != (theirs_fmt.as_str(), tw, th) {
        return Err(format!("{name} {w}x{h}, FFmpeg {theirs_fmt} {tw}x{th}"));
    }
    let args = refcheck::ffmpeg_video_md5_args(path, "0:v:0", name, &["-idct", "simple", "-cpuflags", "0"]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let theirs = refcheck::parse_framemd5(&String::from_utf8(ffmpeg(&args)).unwrap());
    let first = ours.iter().zip(&theirs).position(|(a, b)| a != b);
    let matched = ours.iter().filter(|m| theirs.contains(m)).count();
    let summary = format!("{name} {w}x{h}: {} frames vs FFmpeg {}, {matched} match", ours.len(), theirs.len());
    if ours.len() != theirs.len() || first.is_some() {
        return Err(format!("{summary}, first difference at frame {first:?}"));
    }
    Ok(summary)
}

fn corpus(name: &str) -> PathBuf {
    std::env::var_os("PEARTUBE_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/peartube-media-corpus"))
        .join(name)
}

/// A one-second test clip made by FFmpeg's encoder (`args` choose the
/// codec and format), in this test's scratch directory.
fn made(name: &str, args: &[&str]) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{}-{name}", std::process::id()));
    let mut all = vec!["-y", "-f", "lavfi", "-i", "testsrc2=s=320x240:r=25:d=1"];
    all.extend_from_slice(args);
    all.push(path.to_str().unwrap());
    ffmpeg(&all);
    path
}

fn run(cases: Vec<(String, PathBuf)>) {
    let mut failures = Vec::new();
    for (label, path) in cases {
        match check(&path) {
            Ok(summary) => eprintln!("{label}: {summary}"),
            Err(why) => failures.push(format!("{label}: {why}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn mjpeg_frames_equal_ffmpegs() {
    run(vec![
        ("gen:video_mjpeg.avi".into(), corpus("video_mjpeg.avi")),
        ("mjpeg/mjpeg_field_order.avi".into(), refcheck::fate("mjpeg/mjpeg_field_order.avi")),
        ("4:2:2 in AVI".into(), made("mjpeg422.avi", &["-c:v", "mjpeg", "-pix_fmt", "yuvj422p", "-q:v", "3"])),
        ("4:2:0 in MOV".into(), made("mjpeg420.mov", &["-c:v", "mjpeg", "-pix_fmt", "yuvj420p", "-q:v", "3"])),
        ("4:4:4 in MOV".into(), made("mjpeg444.mov", &["-c:v", "mjpeg", "-pix_fmt", "yuvj444p", "-q:v", "3"])),
    ]);
}

/// FATE's 24-bit Cinepak AVIs (cvid/; the third, a palettized `.mov`,
/// needs FFmpeg's palette mode, which the stream parameters cannot
/// select) and a clip from FFmpeg's encoder.
#[test]
fn cinepak_frames_equal_ffmpegs() {
    run(vec![
        ("cvid/laracroft-cinepak-partial.avi".into(), refcheck::fate("cvid/laracroft-cinepak-partial.avi")),
        ("cvid/pcitva15.avi".into(), refcheck::fate("cvid/pcitva15.avi")),
        ("FFmpeg cinepak in AVI".into(), made("cinepak.avi", &["-c:v", "cinepak", "-pix_fmt", "rgb24"])),
    ]);
}

/// The corpus clip and clips from FFmpeg's encoder (FATE has no H.261
/// sample; it encodes its own, as here), QCIF and CIF.
#[test]
fn h261_frames_equal_ffmpegs() {
    let cif = |name: &str, extra: &[&str]| {
        let mut args = vec!["-s", "352x288", "-c:v", "h261"];
        args.extend_from_slice(extra);
        made(name, &args)
    };
    run(vec![
        ("gen:video_h261.avi".into(), corpus("video_h261.avi")),
        ("QCIF".into(), made("h261-qcif.avi", &["-s", "176x144", "-c:v", "h261", "-q:v", "6"])),
        ("CIF, high quality".into(), cif("h261-cif-q2.avi", &["-q:v", "2"])),
        ("CIF, low quality".into(), cif("h261-cif-q20.avi", &["-q:v", "20"])),
    ]);
}

/// FATE's Indeo 3 samples (iv32/), in FFmpeg's native 4:1:0.
#[test]
fn indeo3_frames_equal_ffmpegs() {
    run(vec![
        ("iv32/OPENINGH.avi".into(), refcheck::fate("iv32/OPENINGH.avi")),
        ("iv32/cubes.mov".into(), refcheck::fate("iv32/cubes.mov")),
    ]);
}

/// FATE's raw Dirac samples (dirac/), opened as the player opens them: the
/// main profile with inter pictures in FFmpeg's output order, and VC-2 low
/// delay.
#[test]
fn dirac_frames_equal_ffmpegs() {
    run(vec![
        ("dirac/vts.profile-main.drc".into(), refcheck::fate("dirac/vts.profile-main.drc")),
        ("dirac/vts.profile-vc2-low-delay.drc".into(), refcheck::fate("dirac/vts.profile-vc2-low-delay.drc")),
    ]);
}

//! DVD-Video and Blu-ray LPCM through the Player's registry
//! (`codecs::register_all`: the MPEG-PS and MPEG-TS readers, these
//! decoders) against FFmpeg 2da55bf (`$FFMPEG_SRC/ffmpeg -cpuflags 0`): the
//! layout FFmpeg outputs, the same number of samples, every sample equal.
//!
//! Inputs: FATE's DVD LPCM samples (`mpegps/pcm_aud.mpg`, pcm.mak's
//! `pcm-dvd/coolitnow-partial.vob`), and FFmpeg encodes as FATE's pcm.mak
//! makes them: `pcm_dvd` in VOB at 16 and 24 bits, mono to 7.1, 48 and
//! 96 kHz; `pcm_bluray` in M2TS (`-mpegts_m2ts_mode 1`, the HDMV
//! registration) at 16 and 24 bits, with the layouts that carry an empty
//! channel (mono, 3.0) or are reordered (5.1, 7.1).

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::{MediaType, SampleFormat};

fn pinned_ffmpeg() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
        .join("ffmpeg")
}

/// FFmpeg's decode of the first audio stream as f32, and its layout
/// (sample format, rate, channels) from `ffprobe`.
fn ffmpeg(path: &Path) -> (Vec<f32>, (String, u32, u16)) {
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let samples = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect();
    let probe = Command::new(pinned_ffmpeg().with_file_name("ffprobe"))
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=sample_fmt,sample_rate,channels"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8(probe.stdout).unwrap();
    let fields: Vec<&str> = text.trim().split(',').collect();
    (samples, (fields[0].to_string(), fields[1].parse().unwrap(), fields[2].parse().unwrap()))
}

/// `name` in the test scratch directory: 1 s of a different tone per
/// channel at `rate` Hz, encoded by the `ffmpeg` on PATH with `args`.
fn encoded(name: &str, rate: u32, channels: u32, args: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("lpcm");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.is_file() {
        let tones: Vec<String> =
            (0..channels).map(|c| format!("0.7*sin(2*PI*{}*t)+0.05*random({c})", 220 * (c + 1))).collect();
        let source = format!("aevalsrc={}:s={rate}:d=1", tones.join("|"));
        let partial = dir.join(format!("{}.{name}", std::process::id()));
        let out = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-y", "-f", "lavfi", "-i", &source])
            .args(args)
            .arg(&partial)
            .output()
            .unwrap();
        assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
        std::fs::rename(&partial, &path).unwrap();
    }
    path
}

fn sample_fmt(format: SampleFormat) -> &'static str {
    match format {
        SampleFormat::S16 => "s16",
        SampleFormat::S32 => "s32",
        _ => "other",
    }
}

/// Every input decodes to FFmpeg's layout and samples.
fn all_like_ffmpeg(inputs: &[PathBuf]) {
    let mut failed = Vec::new();
    for path in inputs {
        let name = path.file_name().unwrap().to_string_lossy();
        let (theirs, layout) = ffmpeg(path);
        let decoded = refcheck::decode(path, &[codecs::register_all], MediaType::Audio, 0);
        let format = decoded.audio_format.expect("the decoder reports its layout");
        let ours = refcheck::interleaved_f32(&decoded);
        let first = ours.iter().zip(&theirs).position(|(a, b)| a.to_bits() != b.to_bits());
        let line = format!(
            "{name}: {} {}x{} Hz {}, {} samples; FFmpeg {}x{} Hz {}, {} samples; first difference {first:?}",
            decoded.params.codec_id.as_str(),
            format.channels,
            format.sample_rate,
            sample_fmt(format.sample_format),
            ours.len(),
            layout.2,
            layout.1,
            layout.0,
            theirs.len()
        );
        println!("{line}");
        let ours_layout = (sample_fmt(format.sample_format).to_string(), format.sample_rate, format.channels);
        if ours_layout != layout || ours.len() != theirs.len() || first.is_some() || ours.is_empty() {
            failed.push(line);
        }
    }
    assert!(failed.is_empty(), "{} of {} unlike FFmpeg:\n{}", failed.len(), inputs.len(), failed.join("\n"));
}

#[test]
fn pcm_dvd() {
    let vob = |name: &str, rate, channels, extra: &[&str]| {
        let mut args = vec!["-c:a", "pcm_dvd"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-f", "vob"]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        refcheck::fate("mpegps/pcm_aud.mpg"),
        refcheck::fate("pcm-dvd/coolitnow-partial.vob"),
        vob("dvd-16-1-96000.vob", 96_000, 1, &["-sample_fmt", "s16"]),
        vob("dvd-16-2-48000.vob", 48_000, 2, &["-sample_fmt", "s16"]),
        vob("dvd-16-6-48000.vob", 48_000, 6, &["-sample_fmt", "s16"]),
        vob("dvd-24-2-48000.vob", 48_000, 2, &["-sample_fmt", "s32"]),
        vob("dvd-24-6-48000.vob", 48_000, 6, &["-sample_fmt", "s32"]),
        vob("dvd-24-8-48000.vob", 48_000, 8, &["-sample_fmt", "s32"]),
        vob("dvd-24-1-96000.vob", 96_000, 1, &["-sample_fmt", "s32"]),
    ]);
}

#[test]
fn pcm_bluray() {
    let m2ts = |name: &str, rate, channels, extra: &[&str]| {
        let mut args = vec!["-c:a", "pcm_bluray"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-mpegts_m2ts_mode", "1", "-f", "mpegts"]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        m2ts("bd-16-mono.m2ts", 48_000, 1, &["-sample_fmt", "s16"]),
        m2ts("bd-16-stereo.m2ts", 48_000, 2, &["-sample_fmt", "s16"]),
        m2ts("bd-16-3.0.m2ts", 48_000, 3, &["-sample_fmt", "s16", "-af", "pan=3.0|FL=c0|FR=c1|FC=c2"]),
        m2ts("bd-16-5.1.m2ts", 48_000, 6, &["-sample_fmt", "s16"]),
        m2ts("bd-16-7.1.m2ts", 48_000, 8, &["-sample_fmt", "s16"]),
        m2ts("bd-24-stereo-96000.m2ts", 96_000, 2, &["-sample_fmt", "s32"]),
        m2ts("bd-24-5.1-48000.m2ts", 48_000, 6, &["-sample_fmt", "s32"]),
    ]);
}

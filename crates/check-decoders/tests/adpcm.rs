//! Every ADPCM variant OxideAV registers that FFmpeg also decodes, played
//! through the Player's registry (`codecs::register_all`, with its
//! containers), against FFmpeg 2da55bf's decoders (`$FFMPEG_SRC/ffmpeg
//! -cpuflags 0`): the same number of samples, and every sample equal. All
//! are integer decoders.
//!
//! Inputs: FATE's ADPCM samples (adpcm.mak, qt.mak: IMA4 mono and stereo,
//! MS ADPCM and IMA WAV in QuickTime, OKI in WAV, the Sound Blaster Pro
//! 2-, 2.6- and 4-bit VOC files and Creative ADPCM in WAV), and FFmpeg
//! encodes of the kinds FATE's acodec tests make (IMA QT in AIFF; IMA WAV,
//! MS and Yamaha in WAV; each also with `-trellis 5`), mono and stereo, at
//! more block sizes and in more containers, plus G.726 at each bit rate
//! (FATE has no G.726 test). `adpcm_yamaha_a` has no FFmpeg decoder, so it
//! is not here.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::MediaType;

/// FFmpeg 2da55bf: `$FFMPEG_SRC/ffmpeg`, default ~/projects/ffmpeg-src.
fn pinned_ffmpeg() -> PathBuf {
    std::env::var_os("FFMPEG_SRC")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"))
        .join("ffmpeg")
}

/// FFmpeg's decode of the first audio stream through its C code, as f32
/// (its s16 output over 32768, as refcheck reads ours).
fn ffmpeg_f32(path: &Path) -> Vec<f32> {
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes(b.try_into().unwrap())).collect()
}

/// `name` in the test scratch directory: 2 s of a sweep with noise at
/// `rate` Hz and `channels` channels, encoded by the `ffmpeg` on PATH with
/// `args` (codec, options, format).
fn encoded(name: &str, rate: u32, channels: u32, args: &[&str]) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("adpcm");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    if !path.is_file() {
        let voice = "0.6*sin(2*PI*(150+1800*t)*t)+0.2*random(0)-0.1";
        let source = format!("aevalsrc={}:s={rate}:d=2", vec![voice; channels as usize].join("|"));
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

/// `path` decodes to FFmpeg's samples; the outcome either way.
fn compare(path: &Path) -> Result<String, String> {
    let name = path.file_name().unwrap().to_string_lossy().to_string();
    let theirs = ffmpeg_f32(path);
    let decoded = std::panic::catch_unwind(|| refcheck::decode(path, &[codecs::register_all], MediaType::Audio, 0))
        .map_err(|e| {
            let why = e.downcast_ref::<String>().cloned().or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()));
            format!("{name}: {}", why.unwrap_or_default())
        })?;
    let codec = decoded.params.codec_id.as_str().to_string();
    let ours = refcheck::interleaved_f32(&decoded);
    let channels = usize::from(decoded.params.channels.unwrap_or(1)).max(1);
    let first = ours.iter().zip(&theirs).position(|(a, b)| a.to_bits() != b.to_bits());
    if ours.len() == theirs.len() && first.is_none() && !ours.is_empty() {
        return Ok(format!("{name}: {codec}, {} samples per channel equal", ours.len() / channels));
    }
    let snr = refcheck::try_snr_db(&theirs, &ours, 0).map_or_else(|e| e, |s| format!("{s:.1} dB"));
    Err(format!(
        "{name}: {codec}, {} samples per channel, FFmpeg {}; first difference at {first:?}; SNR {snr}",
        ours.len() / channels,
        theirs.len() / channels
    ))
}

/// Every input decodes to FFmpeg's samples.
fn all_like_ffmpeg(inputs: &[PathBuf]) {
    let results: Vec<_> = inputs.iter().map(|p| compare(p)).collect();
    for r in &results {
        match r {
            Ok(line) => println!("ok   {line}"),
            Err(line) => println!("FAIL {line}"),
        }
    }
    let failed: Vec<_> = results.iter().filter_map(|r| r.as_ref().err()).collect();
    assert!(failed.is_empty(), "{} of {} unlike FFmpeg:\n{}", failed.len(), results.len(), failed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n"));
}

fn fate(relative: &str) -> PathBuf {
    refcheck::fate(relative)
}

#[test]
fn ima_qt() {
    let qt = |name: &str, rate, channels, extra: &[&str], format: &str| {
        let mut args = vec!["-c:a", "adpcm_ima_qt"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-f", format]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        fate("qt-surge-suite/surge-1-16-B-ima4.mov"),
        fate("qt-surge-suite/surge-2-16-B-ima4.mov"),
        qt("ima_qt-1.aiff", 44100, 1, &[], "aiff"),
        qt("ima_qt-2.aiff", 44100, 2, &[], "aiff"),
        qt("ima_qt-trellis-2.aiff", 44100, 2, &["-trellis", "5"], "aiff"),
        qt("ima_qt-2.mov", 48000, 2, &[], "mov"),
        qt("ima_qt-1.caf", 22050, 1, &[], "caf"),
    ]);
}

#[test]
fn ima_wav() {
    let wav = |name: &str, rate, channels, extra: &[&str], format: &str| {
        let mut args = vec!["-c:a", "adpcm_ima_wav"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-f", format]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        fate("qt-surge-suite/surge-2-16-L-ms11.mov"),
        wav("ima_wav-1.wav", 44100, 1, &[], "wav"),
        wav("ima_wav-2.wav", 44100, 2, &[], "wav"),
        wav("ima_wav-trellis-2.wav", 44100, 2, &["-trellis", "5"], "wav"),
        wav("ima_wav-2-256.wav", 22050, 2, &["-block_size", "256"], "wav"),
        wav("ima_wav-1-4096.wav", 48000, 1, &["-block_size", "4096"], "wav"),
        wav("ima_wav-2.avi", 44100, 2, &[], "avi"),
    ]);
}

#[test]
fn ms() {
    let ms = |name: &str, rate, channels, extra: &[&str], format: &str| {
        let mut args = vec!["-c:a", "adpcm_ms"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-f", format]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        fate("qt-surge-suite/surge-2-16-L-ms02.mov"),
        ms("ms-1.wav", 44100, 1, &[], "wav"),
        ms("ms-2.wav", 44100, 2, &[], "wav"),
        ms("ms-trellis-2.wav", 44100, 2, &["-trellis", "5"], "wav"),
        ms("ms-2-256.wav", 22050, 2, &["-block_size", "256"], "wav"),
        ms("ms-1-4096.wav", 48000, 1, &["-block_size", "4096"], "wav"),
        ms("ms-2.avi", 44100, 2, &[], "avi"),
    ]);
}

#[test]
fn yamaha() {
    let yamaha = |name: &str, rate, channels, extra: &[&str]| {
        let mut args = vec!["-c:a", "adpcm_yamaha"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["-f", "wav"]);
        encoded(name, rate, channels, &args)
    };
    all_like_ffmpeg(&[
        yamaha("yamaha-1.wav", 44100, 1, &[]),
        yamaha("yamaha-2.wav", 44100, 2, &[]),
        yamaha("yamaha-trellis-2.wav", 44100, 2, &["-trellis", "5"]),
        yamaha("yamaha-2-256.wav", 22050, 2, &["-block_size", "256"]),
    ]);
}

/// FFmpeg's `adpcm_ima_oki` (WAV tag 0x0010): OxideAV's `adpcm_dialogic`.
#[test]
fn oki() {
    all_like_ffmpeg(&[fate("oki/test.wav")]);
}

/// FFmpeg's `adpcm_sbpro_2`/`_3`/`_4` (VOC codecs 3, 2, 1) and `adpcm_ct`
/// (WAV tag 0x0200), FATE's adpcm-creative tests.
#[test]
fn creative() {
    all_like_ffmpeg(&[
        fate("creative/BBC_2BIT.VOC"),
        fate("creative/BBC_3BIT.VOC"),
        fate("creative/BBC_4BIT.VOC"),
        fate("creative/intro-partial.wav"),
    ]);
}

#[test]
fn g726() {
    let inputs: Vec<PathBuf> = ["16k", "24k", "32k", "40k"]
        .iter()
        .map(|rate| encoded(&format!("g726-{rate}.wav"), 8000, 1, &["-c:a", "g726", "-b:a", rate, "-f", "wav"]))
        .collect();
    all_like_ffmpeg(&inputs);
}

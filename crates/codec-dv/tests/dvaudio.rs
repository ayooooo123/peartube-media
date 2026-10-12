//! dvaudio against FFmpeg 2da55bf's: WAV files of Ulead DV audio (WAVE
//! tag 0x0216 for 625/50, 0x0215 for 525/60; each block the audio DIF
//! blocks of one DV frame end to end), made from DV that FFmpeg encodes,
//! decode through the WAV demuxer to FFmpeg's samples, bit for bit.

use std::path::{Path, PathBuf};
use std::process::Command;

use oxideav_core::MediaType;

fn tmp(name: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-dvaudio-{}-{name}", std::process::id()))
}

/// FFmpeg-made raw DV with a 1 kHz tone, 48 kHz stereo.
fn raw_dv(name: &str, size: &str, rate: &str, pix_fmt: &str) -> Vec<u8> {
    let path = tmp(name);
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(format!("testsrc=size={size}:rate={rate}:duration=1"))
        .args(["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=1"])
        .args(["-c:v", "dvvideo", "-pix_fmt", pix_fmt, "-c:a", "pcm_s16le", "-ac", "2", "-f", "dv"])
        .arg(&path)
        .output()
        .expect("the fixture FFmpeg runs");
    assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    let data = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    data
}

/// A Ulead DV audio WAV: per DV frame, the nine audio DIF blocks of each
/// DIF sequence (blocks 6, 22, …, 134 of its 150).
fn ulead_wav(name: &str, dv: &[u8], frame_size: usize, sequences: usize, tag: u16, fps: f64) -> PathBuf {
    let mut data = Vec::new();
    for frame in dv.chunks_exact(frame_size) {
        for seq in 0..sequences {
            for blk in 0..9 {
                let at = seq * 150 * 80 + (6 + 16 * blk) * 80;
                data.extend_from_slice(&frame[at..at + 80]);
            }
        }
    }
    let block_align = (sequences * 9 * 80) as u16;
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(4 + 8 + 16 + 8 + data.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&tag.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&48_000u32.to_le_bytes());
    wav.extend_from_slice(&((f64::from(block_align) * fps) as u32).to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data.len() as u32).to_le_bytes());
    wav.extend_from_slice(&data);
    let path = tmp(name);
    std::fs::write(&path, wav).unwrap();
    path
}

fn check(path: &Path) {
    let name = path.display().to_string();
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "dvaudio", "{name}: FFmpeg decodes it as dvaudio");
    let decoded = refcheck::decode(path, &[codec_dv::register, oxideav_basic::__oxideav_entry], MediaType::Audio, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "dvaudio", "{name}: resolved to dvaudio");
    let ours = refcheck::interleaved_f32(&decoded);
    let reference = refcheck::ffmpeg_audio_f32(path, 0);
    assert!(!reference.is_empty(), "{name}: FFmpeg's samples");
    assert_eq!(ours.len(), reference.len(), "{name}: samples");
    let snr = refcheck::snr_db(&reference, &ours, 0);
    assert!(snr.is_infinite(), "{name}: equals FFmpeg's ({snr} dB)");
}

#[test]
fn pal_ulead_dv_audio_decodes_as_ffmpeg() {
    let dv = raw_dv("pal.dv", "720x576", "25", "yuv420p");
    check(&ulead_wav("pal.wav", &dv, 144_000, 12, 0x0216, 25.0));
}

#[test]
fn ntsc_ulead_dv_audio_decodes_as_ffmpeg() {
    let dv = raw_dv("ntsc.dv", "720x480", "30000/1001", "yuv411p");
    check(&ulead_wav("ntsc.wav", &dv, 120_000, 10, 0x0215, 30000.0 / 1001.0));
}

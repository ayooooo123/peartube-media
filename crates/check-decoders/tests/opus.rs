//! oxideav-opus against FFmpeg 2da55bf on surround Opus, read through the
//! containers the player reads it from (`refcheck::decode`: the registered
//! decoder, the container's trims). The fork registers FFmpeg's Opus
//! decoder, ported: planar float, FFmpeg's channel order, FFmpeg's samples.
//! Before it, mapping-family-1 streams came out in the `OpusHead`'s Vorbis
//! channel order as 16-bit PCM: -2.3 dB against FFmpeg on 7.1.

use oxideav_core::{AudioFormat, MediaType, SampleFormat};
use refcheck::Registrar;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Decodes the first audio stream of `path` and compares it with FFmpeg's
/// decode: the decoder's reported layout, the exact sample count, and the
/// SNR (contract floor for float decoders: 90 dB).
fn assert_matches_ffmpeg(path: &Path, registrars: &[Registrar], channels: u16) {
    let name = path.display();
    let decoded = refcheck::decode(path, registrars, MediaType::Audio, 0);
    assert_eq!(
        decoded.audio_format,
        Some(AudioFormat { sample_format: SampleFormat::F32P, sample_rate: 48_000, channels }),
        "{name}: reported layout"
    );
    assert!(decoded.trim_fallbacks.is_empty(), "{name}: trims not applied: {:?}", decoded.trim_fallbacks);
    let ours = refcheck::interleaved_f32(&decoded);
    let theirs = refcheck::ffmpeg_src_audio_f32(path, 0);
    assert_eq!(ours.len(), theirs.len(), "{name}: interleaved samples vs FFmpeg's");
    let snr = refcheck::snr_db(&theirs, &ours, 0);
    assert!(snr >= 90.0, "{name}: {snr:.1} dB against FFmpeg");
}

/// A 7.1 Opus file made by FFmpeg's libopus encoder (mapping family 1:
/// 5 streams, 3 coupled; CELT), eight tones, 2 s, in the container `ext`
/// names. Made once per target directory with the `ffmpeg` on PATH; the
/// comparison is with FFmpeg 2da55bf's decode of the same file.
fn ffmpeg_made_7_1(ext: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("opus-7.1-libopus.{ext}"));
    if path.is_file() {
        return path;
    }
    let tones: Vec<String> = [220, 330, 440, 110, 550, 660, 770, 880]
        .iter()
        .enumerate()
        .map(|(i, f)| format!("sine=f={f}:r=48000:d=2[a{i}]"))
        .collect();
    let graph = format!(
        "{};[a0][a1][a2][a3][a4][a5][a6][a7]amerge=inputs=8,aformat=channel_layouts=7.1[out]",
        tones.join(";")
    );
    let partial = path.with_extension(format!("{ext}.part.{}", std::process::id()));
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-filter_complex", &graph, "-map", "[out]"])
        .args(["-c:a", "libopus", "-b:a", "448k", "-f"])
        .arg(match ext {
            "ogg" => "ogg",
            "mkv" => "matroska",
            _ => "mpegts",
        })
        .arg(&partial)
        .output()
        .expect("ffmpeg with libopus on PATH");
    assert!(out.status.success(), "ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    std::fs::rename(&partial, &path).expect("rename");
    path
}

/// FATE's 7.1 Opus in MPEG-TS (`fate-ts-opus-demux`): its first stream is
/// hybrid (SILK + CELT), the other four CELT.
#[test]
fn fate_7_1_in_mpegts_matches_ffmpeg() {
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_mpegts::__oxideav_entry];
    assert_matches_ffmpeg(&refcheck::fate("opus/test-8-7.1.opus-small.ts"), registrars, 8);
}

/// FATE's 5.1 Opus (`fate-opus-tron.6ch.tinypkts`), 2.5 ms CELT packets.
#[test]
fn fate_5_1_in_matroska_matches_ffmpeg() {
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_mkv::__oxideav_entry];
    assert_matches_ffmpeg(&refcheck::fate("opus/tron.6ch.tinypkts.mka"), registrars, 6);
}

#[test]
fn ffmpeg_made_7_1_in_ogg_matches_ffmpeg() {
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_ogg::__oxideav_entry];
    assert_matches_ffmpeg(&ffmpeg_made_7_1("ogg"), registrars, 8);
}

#[test]
fn ffmpeg_made_7_1_in_matroska_matches_ffmpeg() {
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_mkv::__oxideav_entry];
    assert_matches_ffmpeg(&ffmpeg_made_7_1("mkv"), registrars, 8);
}

#[test]
fn ffmpeg_made_7_1_in_mpegts_matches_ffmpeg() {
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_mpegts::__oxideav_entry];
    assert_matches_ffmpeg(&ffmpeg_made_7_1("ts"), registrars, 8);
}

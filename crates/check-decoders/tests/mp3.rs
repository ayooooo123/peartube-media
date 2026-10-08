//! oxideav-mp3 against FFmpeg's default MP3 decoder, `mp3float` (FFmpeg
//! 2da55bf, C code paths: `-cpuflags 0`), fed the packets FFmpeg's demuxer
//! and mpegaudio parser give it: every FATE mp3-conformance stream within
//! 90 dB of FFmpeg, with FFmpeg's sample count, in planar float as the
//! decoder reports.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use check_decoders::{decode_packets, ffmpeg_packets};
use oxideav_core::{CodecId, CodecParameters, Frame, SampleFormat};

/// FATE's mp3-conformance streams. `he_free.bit` is left out: it is
/// free format, which FFmpeg 2da55bf cannot open ("Failed to find two
/// consecutive MPEG audio frames"), so there is no reference.
const STREAMS: &[&str] = &[
    "compl", "he_32khz", "he_44khz", "he_48khz", "he_mode", "hecommon", "si", "si_block", "si_huff", "sin1k0db",
];

/// FFmpeg's decode with every frame given `channels` channels,
/// interleaved f32. A frame already in that layout passes unchanged, so
/// each frame can be compared in its own layout although `he_mode.bit`
/// switches between mono and stereo (the ffmpeg tool otherwise remixes
/// every frame to the first one's layout).
fn ffmpeg_f32(path: &Path, channels: u16) -> Vec<f32> {
    let out = Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ac", &channels.to_string(), "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(out.status.success(), "{}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

#[test]
fn conformance_streams_decode_within_90_db_of_ffmpeg() {
    let mut failures = Vec::new();
    for name in STREAMS {
        let path = refcheck::fate(&format!("mp3-conformance/{name}.bit"));
        let packets = ffmpeg_packets(&path, "a:0", None);
        let mut params = CodecParameters::audio(CodecId::new("mp3"));
        // A hint: every frame header gives the real rate and channel count.
        params.channels = Some(2);
        let (decoded, refused) = decode_packets(&[oxideav_mp3::register], &params, &packets);

        // Each frame next to FFmpeg's same frame, in the frame's layout.
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        let mut references: HashMap<u16, Vec<f32>> = HashMap::new();
        let mut at = 0;
        for (frame, format) in decoded.frames.iter().zip(&decoded.frame_formats) {
            let Frame::Audio(a) = frame else { continue };
            let Some(format) = format.filter(|f| f.sample_format == SampleFormat::F32P) else {
                failures.push(format!("{name}: reported {format:?}, not planar float"));
                break;
            };
            let channels = usize::from(format.channels);
            let planes: Vec<Vec<f32>> = a
                .data
                .iter()
                .map(|p| p.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
                .collect();
            for i in 0..a.samples as usize {
                ours.extend(planes.iter().map(|p| p[i]));
            }
            let reference = references.entry(format.channels).or_insert_with(|| ffmpeg_f32(&path, format.channels));
            let span = at * channels..(at + a.samples as usize) * channels;
            theirs.extend_from_slice(reference.get(span).unwrap_or(&[]));
            at += a.samples as usize;
        }
        let lengths: Vec<usize> = references.iter().map(|(&c, r)| r.len() / usize::from(c)).collect();
        let snr = refcheck::try_snr_db(&theirs, &ours, 0);
        eprintln!("{name}: {at} samples per channel vs FFmpeg {lengths:?}, {} refused, SNR {snr:?}", refused.len());
        match snr {
            Ok(db) if db >= 90.0 && refused.is_empty() && lengths.iter().all(|&n| n == at) => {}
            other => failures.push(format!("{name}: {other:?}, FFmpeg lengths {lengths:?} vs {at}, refused {refused:?}")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

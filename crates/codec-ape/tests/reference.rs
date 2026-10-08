//! Reference tests for codec-ape: Monkey's Audio demuxer and decoder
//! compared with pinned FFmpeg 2da55bf (`-cpuflags 0`).

use std::path::Path;
use std::process::Command;

use oxideav_core::{Frame, MediaType, RuntimeContext, SampleFormat};
use refcheck::{decode, fate, pinned_ffmpeg};

fn run_pinned_ffmpeg(path: &Path, format: &str, codec: &str) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg())
        .args([
            "-v", "error", "-nostdin", "-cpuflags", "0",
            "-i", path.to_str().unwrap(),
            "-map", "0:a:0",
            "-f", format,
            "-c:a", codec,
            "-",
        ])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(
        out.status.success() || !out.stdout.is_empty(),
        "ffmpeg error: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn interleaved_bytes(decoded: &refcheck::Decoded) -> Vec<u8> {
    let format = decoded.audio_format.expect("audio format reported");
    let channels = format.channels as usize;
    let mut out = Vec::new();

    for frame in &decoded.frames {
        let Frame::Audio(audio) = frame else { continue };
        let samples = audio.samples as usize;
        match format.sample_format {
            SampleFormat::U8P => {
                for i in 0..samples {
                    for ch in 0..channels {
                        out.push(audio.data[ch][i]);
                    }
                }
            }
            SampleFormat::S16P => {
                for i in 0..samples {
                    for ch in 0..channels {
                        out.extend_from_slice(&audio.data[ch][i * 2..i * 2 + 2]);
                    }
                }
            }
            SampleFormat::S32P => {
                for i in 0..samples {
                    for ch in 0..channels {
                        out.extend_from_slice(&audio.data[ch][i * 4..i * 4 + 4]);
                    }
                }
            }
            _ => panic!("unsupported sample format {:?}", format.sample_format),
        }
    }

    out
}

fn check_reference(rel: &str) {
    let path = fate(rel);
    let decoded = decode(&path, &[codec_ape::register], MediaType::Audio, 0);
    let format = decoded.audio_format.expect("audio format reported");

    let (fmt_str, codec_str) = match format.sample_format {
        SampleFormat::U8P => ("u8", "pcm_u8"),
        SampleFormat::S16P => ("s16le", "pcm_s16le"),
        SampleFormat::S32P => ("s32le", "pcm_s32le"),
        other => panic!("unexpected format {other:?}"),
    };

    let expected = run_pinned_ffmpeg(&path, fmt_str, codec_str);
    let ours = interleaved_bytes(&decoded);

    let bytes_per_sample = format.sample_format.bytes_per_sample() * format.channels as usize;
    let expected_samples = expected.len() / bytes_per_sample;
    let our_samples = ours.len() / bytes_per_sample;

    assert_eq!(
        our_samples, expected_samples,
        "{rel}: sample count mismatch: ours {our_samples} vs ffmpeg {expected_samples}"
    );

    assert_eq!(
        ours, expected,
        "{rel}: byte-exact PCM mismatch (len ours {} vs ffmpeg {})",
        ours.len(),
        expected.len()
    );
}

#[test]
fn fate_luckynight_mac380_c2000() {
    check_reference("lossless-audio/luckynight-mac380-c2000.ape");
}

#[test]
fn fate_luckynight_mac380_c4000() {
    check_reference("lossless-audio/luckynight-mac380-c4000.ape");
}

#[test]
fn fate_luckynight_mac388_c2000() {
    check_reference("lossless-audio/luckynight-mac388-c2000.ape");
}

#[test]
fn fate_luckynight_mac388_c4000() {
    check_reference("lossless-audio/luckynight-mac388-c4000.ape");
}

#[test]
fn fate_luckynight_mac389b1_c2000() {
    check_reference("lossless-audio/luckynight-mac389b1-c2000.ape");
}

#[test]
fn fate_luckynight_mac389b1_c4000() {
    check_reference("lossless-audio/luckynight-mac389b1-c4000.ape");
}

#[test]
fn fate_luckynight_mac391b1_c2000() {
    check_reference("lossless-audio/luckynight-mac391b1-c2000.ape");
}

#[test]
fn fate_luckynight_mac391b1_c4000() {
    check_reference("lossless-audio/luckynight-mac391b1-c4000.ape");
}

#[test]
fn fate_luckynight_mac392b2_c2000() {
    check_reference("lossless-audio/luckynight-mac392b2-c2000.ape");
}

#[test]
fn fate_luckynight_mac392b2_c4000() {
    check_reference("lossless-audio/luckynight-mac392b2-c4000.ape");
}

#[test]
fn fate_luckynight_mac394b1_c2000() {
    check_reference("lossless-audio/luckynight-mac394b1-c2000.ape");
}

#[test]
fn fate_luckynight_mac394b1_c4000() {
    check_reference("lossless-audio/luckynight-mac394b1-c4000.ape");
}

#[test]
fn fate_luckynight_partial() {
    check_reference("lossless-audio/luckynight-partial.ape");
}

#[test]
fn fate_nolegacy_cut() {
    check_reference("lossless-audio/NoLegacy-cut.ape");
}

#[test]
fn demuxer_packets_luckynight_partial() {
    check_demuxer_packets("lossless-audio/luckynight-partial.ape");
}

#[test]
fn demuxer_packets_nolegacy_cut() {
    check_demuxer_packets("lossless-audio/NoLegacy-cut.ape");
}

fn check_demuxer_packets(rel: &str) {
    let path = fate(rel);
    let ffprobe = refcheck::pinned_ffprobe();
    let out = Command::new(&ffprobe)
        .args([
            "-v", "error", "-cpuflags", "0",
            "-show_entries", "packet=pts,size",
            "-of", "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe runs");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let expected: Vec<(i64, usize)> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut parts = l.split(',');
            let pts: i64 = parts.next().unwrap().trim().parse().unwrap();
            let size: usize = parts.next().unwrap().trim().parse().unwrap();
            (pts, size)
        })
        .collect();

    let file = std::fs::File::open(&path).expect("open file");
    let ctx = RuntimeContext::new();
    let mut demuxer = codec_ape::open_ape(Box::new(file), &ctx.codecs).expect("open demuxer");

    let mut actual = Vec::new();
    while let Ok(pkt) = demuxer.next_packet() {
        actual.push((pkt.pts.expect("pts"), pkt.data.len()));
    }

    assert_eq!(actual.len(), expected.len(), "{rel}: packet count mismatch");
    for (i, (act, exp)) in actual.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            act, exp,
            "{rel}: packet {i} mismatch: got (pts={}, size={}), expected (pts={}, size={})",
            act.0, act.1, exp.0, exp.1
        );
    }
}

#[test]
fn demuxer_seek_luckynight_partial() {
    let path = fate("lossless-audio/luckynight-partial.ape");
    let ffprobe = refcheck::pinned_ffprobe();
    let out = Command::new(&ffprobe)
        .args([
            "-v", "error", "-cpuflags", "0",
            "-read_intervals", "1.0%+#3",
            "-select_streams", "a:0",
            "-show_entries", "packet=pts",
            "-of", "csv=p=0",
            path.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe runs");
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let expected_pts: Vec<i64> = stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().parse().unwrap())
        .collect();

    assert_eq!(expected_pts.len(), 3, "expected 3 packets from ffprobe");

    let file = std::fs::File::open(&path).expect("open file");
    let ctx = RuntimeContext::new();
    let mut demuxer = codec_ape::open_ape(Box::new(file), &ctx.codecs).expect("open demuxer");

    // Seek to 1.0 s in stream's timebase (44100 units/sec)
    let sample_rate = demuxer.streams()[0].params.sample_rate.unwrap() as i64;
    let seek_pts = 1 * sample_rate; // 44100
    demuxer.seek_to(0, seek_pts).expect("seek succeeds");

    let mut actual_pts = Vec::new();
    for _ in 0..3 {
        let pkt = demuxer.next_packet().expect("next packet after seek");
        actual_pts.push(pkt.pts.expect("pts"));
    }

    assert_eq!(actual_pts, expected_pts, "seek packets pts mismatch");
}

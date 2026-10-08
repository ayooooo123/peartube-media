// Ported from FFmpeg tests/fate/wavpack.mak (commit 2da55bf)
// License: LGPL-2.1-or-later

#![forbid(unsafe_code)]

use std::fs::File;
use std::path::Path;
use std::process::Command;

use oxideav_core::{CodecId, Demuxer, Frame, MediaType, SampleFormat};
use refcheck::{decode, fate, pinned_ffmpeg, pinned_ffprobe, Decoded, Registrar};

fn registrars() -> Vec<Registrar> {
    vec![codec_wavpack::register]
}

fn matroska_registrars() -> Vec<Registrar> {
    vec![codec_wavpack::register, oxideav_mkv::__oxideav_entry]
}

fn ffmpeg_pcm(path: &Path, fmt: &str, codec: &str) -> Vec<u8> {
    let out = Command::new(pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0"])
        .args(["-i", path.to_str().unwrap()])
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", codec, "-"])
        .output()
        .expect("pinned ffmpeg runs");
    assert!(
        out.status.success(),
        "ffmpeg {path:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn interleave_pcm(decoded: &Decoded) -> (Vec<u8>, usize) {
    let mut out = Vec::new();
    let mut total_samples = 0usize;

    for frame in &decoded.frames {
        let Frame::Audio(audio) = frame else {
            continue;
        };
        let samples = audio.samples as usize;
        total_samples += samples;
        let channels = audio.data.len();
        if channels == 0 || samples == 0 {
            continue;
        }

        let format = decoded.audio_format.unwrap().sample_format;
        let bpp = format.bytes_per_sample();

        for i in 0..samples {
            for c in 0..channels {
                let off = i * bpp;
                out.extend_from_slice(&audio.data[c][off..off + bpp]);
            }
        }
    }

    (out, total_samples)
}

fn check_reference(rel_path: &str, use_matroska: bool) {
    let path = fate(rel_path);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();

    let regs = if use_matroska {
        matroska_registrars()
    } else {
        registrars()
    };

    let decoded = decode(&path, &regs, MediaType::Audio, 0);
    assert_eq!(
        decoded.params.codec_id,
        CodecId::new("wavpack"),
        "{name}: codec id"
    );

    let format = decoded
        .audio_format
        .expect("decoder must report its output audio format");

    let (fmt_str, codec_str) = match format.sample_format {
        SampleFormat::S16P => ("s16le", "pcm_s16le"),
        SampleFormat::S32P => ("s32le", "pcm_s32le"),
        SampleFormat::F32P => ("f32le", "pcm_f32le"),
        SampleFormat::U8P => ("u8", "pcm_u8"),
        other => panic!("{name}: unexpected sample format {other:?}"),
    };

    let (ours, our_samples) = interleave_pcm(&decoded);
    let theirs = ffmpeg_pcm(&path, fmt_str, codec_str);

    let bytes_per_sample = format.sample_format.bytes_per_sample();
    let their_samples = theirs.len() / (format.channels as usize * bytes_per_sample);

    assert_eq!(
        our_samples, their_samples,
        "{name}: sample count mismatch (ours: {our_samples}, theirs: {their_samples})"
    );
    assert_eq!(
        ours.len(),
        theirs.len(),
        "{name}: byte length mismatch (ours: {}, theirs: {})",
        ours.len(),
        theirs.len()
    );

    if ours != theirs {
        let mut diffs = 0;
        for (i, (a, b)) in ours.chunks_exact(4).zip(theirs.chunks_exact(4)).enumerate() {
            if a != b {
                let fa = f32::from_le_bytes(a.try_into().unwrap());
                let fb = f32::from_le_bytes(b.try_into().unwrap());
                if diffs < 10 {
                    eprintln!("diff at float {i}: ours={fa} (bits {:08x}) theirs={fb} (bits {:08x})", fa.to_bits(), fb.to_bits());
                }
                diffs += 1;
            }
        }
        panic!("{name}: decoded PCM bytes differ from pinned FFmpeg ({diffs} floats differ)");
    }

    eprintln!(
        "{name}: bit-exact match against pinned FFmpeg ({} samples/ch, {} bytes)",
        our_samples,
        ours.len()
    );
}

// ───────────────────────── Lossless ─────────────────────────

#[test]
fn lossless_8bit() {
    check_reference("wavpack/lossless/8bit-partial.wv", false);
}

#[test]
fn lossless_12bit() {
    check_reference("wavpack/lossless/12bit-partial.wv", false);
}

#[test]
fn lossless_16bit() {
    check_reference("wavpack/lossless/16bit-partial.wv", false);
}

#[test]
fn lossless_24bit() {
    check_reference("wavpack/lossless/24bit-partial.wv", false);
}

#[test]
fn lossless_32bit_int() {
    check_reference("wavpack/lossless/32bit_int-partial.wv", false);
}

#[test]
fn lossless_32bit_float() {
    check_reference("wavpack/lossless/32bit_float-partial.wv", false);
}

#[test]
fn lossless_dsd() {
    check_reference("wavpack/lossless/dsd.wv", false);
}

// ───────────────────────── Lossy ─────────────────────────

#[test]
fn lossy_8bit() {
    check_reference("wavpack/lossy/4.0_8-bit.wv", false);
}

#[test]
fn lossy_16bit() {
    check_reference("wavpack/lossy/4.0_16-bit.wv", false);
}

#[test]
fn lossy_24bit() {
    check_reference("wavpack/lossy/4.0_24-bit.wv", false);
}

#[test]
fn lossy_32bit_int() {
    check_reference("wavpack/lossy/4.0_32-bit_int.wv", false);
}

#[test]
fn lossy_float() {
    check_reference("wavpack/lossy/2.0_32-bit_float.wv", false);
}

// ───────────────────────── Num Channels ─────────────────────────

#[test]
fn channels_mono_float() {
    check_reference("wavpack/num_channels/mono_float-partial.wv", false);
}

#[test]
fn channels_mono_int() {
    check_reference("wavpack/num_channels/mono_16bit_int.wv", false);
}

#[test]
fn channels_4_0() {
    check_reference("wavpack/num_channels/edward_4.0_16bit-partial.wv", false);
}

#[test]
fn channels_5_1() {
    check_reference(
        "wavpack/num_channels/panslab_sample_5.1_16bit-partial.wv",
        false,
    );
}

#[test]
fn channels_6_1() {
    check_reference("wavpack/num_channels/eva_2.22_6.1_16bit-partial.wv", false);
}

#[test]
fn channels_7_1() {
    check_reference(
        "wavpack/num_channels/panslab_sample_7.1_16bit-partial.wv",
        false,
    );
}

// ───────────────────────── Speed Modes ─────────────────────────

#[test]
fn speed_default() {
    check_reference("wavpack/speed_modes/default-partial.wv", false);
}

#[test]
fn speed_fast() {
    check_reference("wavpack/speed_modes/fast-partial.wv", false);
}

#[test]
fn speed_high() {
    check_reference("wavpack/speed_modes/high-partial.wv", false);
}

#[test]
fn speed_vhigh() {
    check_reference("wavpack/speed_modes/vhigh-partial.wv", false);
}

// ───────────────────────── Special ─────────────────────────

#[test]
fn special_clipping() {
    check_reference("wavpack/special/clipping.wv", false);
}

#[test]
fn special_cuesheet() {
    check_reference("wavpack/special/cue_sheet.wv", false);
}

#[test]
fn special_false_stereo() {
    check_reference("wavpack/special/false_stereo.wv", false);
}

#[test]
fn special_zero_lsbs() {
    check_reference("wavpack/special/zero_lsbs.wv", false);
}

// ───────────────────────── Matroska ─────────────────────────

#[test]
fn matroska_mode() {
    check_reference("wavpack/special/matroska_mode.mka", true);
}

// ───────────────────────── Demuxer Packet Check ─────────────────────────

struct ExpectedPacket {
    pts: i64,
    size: usize,
}

fn ffprobe_packets(path: &Path) -> Vec<ExpectedPacket> {
    let out = Command::new(pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "a:0", "-show_packets"])
        .arg(path.to_str().unwrap())
        .output()
        .expect("pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe: {}", String::from_utf8_lossy(&out.stderr));

    let text = String::from_utf8_lossy(&out.stdout);
    let mut list = Vec::new();
    let mut current_pts: Option<i64> = None;
    let mut current_size: Option<usize> = None;

    for line in text.lines() {
        if line == "[PACKET]" {
            current_pts = None;
            current_size = None;
        } else if let Some(val) = line.strip_prefix("pts=") {
            current_pts = val.parse().ok();
        } else if let Some(val) = line.strip_prefix("size=") {
            current_size = val.parse().ok();
        } else if line == "[/PACKET]" {
            if let (Some(pts), Some(size)) = (current_pts, current_size) {
                list.push(ExpectedPacket { pts, size });
            }
        }
    }
    list
}

fn check_demuxer_packets(rel_path: &str) {
    let path = fate(rel_path);
    let expected = ffprobe_packets(&path);
    assert!(!expected.is_empty(), "expected packets from ffprobe");

    let file = File::open(&path).expect("open file");
    let ctx = oxideav_core::RuntimeContext::new();
    let mut demuxer =
        codec_wavpack::demuxer::RawWvDemuxer::open(Box::new(file), &ctx.codecs).expect("open demuxer");

    let mut actual_pts = Vec::new();
    let mut actual_sizes = Vec::new();

    loop {
        match demuxer.next_packet() {
            Ok(pkt) => {
                actual_pts.push(pkt.pts.expect("packet pts"));
                actual_sizes.push(pkt.data.len());
            }
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("demux error on {rel_path}: {e}"),
        }
    }

    assert_eq!(
        actual_pts.len(),
        expected.len(),
        "{rel_path}: packet count mismatch"
    );
    for (i, (act_p, exp)) in actual_pts.iter().zip(&expected).enumerate() {
        assert_eq!(*act_p, exp.pts, "{rel_path}: packet {i} pts mismatch");
        assert_eq!(
            actual_sizes[i], exp.size,
            "{rel_path}: packet {i} size mismatch"
        );
    }
    eprintln!(
        "{rel_path}: all {} packets match ffprobe (pts & size)",
        expected.len()
    );
}

#[test]
fn demuxer_lossless_16bit() {
    check_demuxer_packets("wavpack/lossless/16bit-partial.wv");
}

#[test]
fn demuxer_num_channels_5_1() {
    check_demuxer_packets("wavpack/num_channels/panslab_sample_5.1_16bit-partial.wv");
}

#[test]
fn demuxer_lossless_dsd() {
    check_demuxer_packets("wavpack/lossless/dsd.wv");
}

// ───────────────────────── Seek Test ─────────────────────────

#[test]
fn demuxer_seek_1s() {
    let path = fate("wavpack/num_channels/panslab_sample_5.1_16bit-partial.wv");
    let file = File::open(&path).expect("open file");
    let ctx = oxideav_core::RuntimeContext::new();
    let mut demuxer =
        codec_wavpack::demuxer::RawWvDemuxer::open(Box::new(file), &ctx.codecs).expect("open demuxer");

    // Query pinned ffprobe for the expected 3 packets starting at 1.0s
    let ffprobe_out = Command::new(pinned_ffprobe())
        .args([
            "-v",
            "error",
            "-read_intervals",
            "1.0%+#3",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts",
            "-of",
            "csv=p=0",
        ])
        .arg(path.to_str().unwrap())
        .output()
        .expect("pinned ffprobe runs");
    assert!(
        ffprobe_out.status.success(),
        "ffprobe: {}",
        String::from_utf8_lossy(&ffprobe_out.stderr)
    );
    let expected_pts: Vec<i64> = String::from_utf8_lossy(&ffprobe_out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect();
    assert_eq!(expected_pts.len(), 3, "expected 3 packet pts from ffprobe");

    let landed = demuxer.seek_to(0, expected_pts[0]).expect("seek succeeds");
    assert_eq!(landed, expected_pts[0], "landed pts");

    let p1 = demuxer.next_packet().expect("packet 1");
    let p2 = demuxer.next_packet().expect("packet 2");
    let p3 = demuxer.next_packet().expect("packet 3");

    assert_eq!(p1.pts, Some(expected_pts[0]), "packet 1 pts");
    assert_eq!(p2.pts, Some(expected_pts[1]), "packet 2 pts");
    assert_eq!(p3.pts, Some(expected_pts[2]), "packet 3 pts");

    eprintln!(
        "seek_to 1.0s: successfully landed at {}, subsequent pts {:?}",
        expected_pts[0], expected_pts
    );
}

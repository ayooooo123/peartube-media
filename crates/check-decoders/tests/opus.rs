//! oxideav-opus against FFmpeg 2da55bf, read through the containers the
//! player reads Opus from (`refcheck::decode`: the registered decoder, the
//! container's trims). The fork registers FFmpeg's Opus decoder, ported:
//! planar float, FFmpeg's channel order, and FFmpeg's samples bit for bit
//! against its C code (`ffmpeg_c_path`). Before it, mapping-family-1
//! streams came out in the `OpusHead`'s Vorbis channel order (7.1: -2.3 dB
//! against FFmpeg), and every stream as the RFC 6716 decoder's 16-bit PCM:
//! CELT files at the 16-bit limit of FFmpeg's own output (testvector01:
//! 79.8 dB), SILK and hybrid files at 9.6 to 36 dB (FFmpeg decodes SILK in
//! float and resamples it with libswresample).

use oxideav_core::{AudioFormat, CodecId, CodecParameters, MediaType, SampleFormat};
use refcheck::Registrar;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Decodes the first audio stream of `path` and compares it with FFmpeg's
/// decode through its C code (`ffmpeg_c_path`): the decoder's reported
/// layout, the exact sample count, and the samples bit for bit.
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
    let theirs = ffmpeg_c_path(path, false);
    assert_eq!(ours.len(), theirs.len(), "{name}: interleaved samples vs FFmpeg's");
    let differ = differing(&theirs, &ours);
    eprintln!("{name}: {} samples/channel, {differ} differ from FFmpeg's", ours.len() / usize::from(channels));
    assert_eq!(differ, 0, "{name}: {differ} samples differ ({:.1} dB)", refcheck::snr_db(&theirs, &ours, 0));
}

/// How many samples differ in their bits.
fn differing(theirs: &[f32], ours: &[f32]) -> usize {
    theirs.iter().zip(ours).filter(|(t, o)| t.to_bits() != o.to_bits()).count()
}

/// Every FATE file with Opus audio, mono to 7.1, SILK, hybrid and CELT.
fn fate_opus_files() -> Vec<String> {
    let mut files: Vec<String> = (1..=12).map(|n| format!("opus/testvector{n:02}.mka")).collect();
    files.extend(
        [
            "opus/silk-lbrr.mka",
            "opus/silk-lbrr-mono.mka",
            "opus/tron.6ch.tinypkts.mka",
            "opus/test-8-7.1.opus-small.ts",
            "audiomatch/tones_opus_48000_stereo.mka",
            "audiomatch/tones_opus_48000_stereo.opus",
            "ogg/intro-partial.opus",
            "cover_art/ogg_vorbiscomment_cover.opus",
            "mkv/codec_delay_opus.mkv",
            "caf/opus.caf",
        ]
        .map(String::from),
    );
    files
}

/// FFmpeg 2da55bf's `OpusHead` for stream `0:a:0` (`ffprobe -show_data`).
fn ffmpeg_extradata(path: &Path) -> Vec<u8> {
    let out = check_decoders::tool(
        refcheck::pinned_ffprobe(),
        &["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=extradata", "-show_data", "-of", "default=nw=1", path.to_str().unwrap()],
    );
    let text = String::from_utf8(out).expect("UTF-8");
    let mut bytes = Vec::new();
    // Hex dump lines: "00000000: 4f70 7573 4865 6164 0101 3801 401f 0000  OpusHead..."
    for line in text.lines().filter(|l| l.len() > 10 && l.as_bytes()[8] == b':') {
        let hex: String = line[10..].split("  ").next().unwrap().split_whitespace().collect();
        bytes.extend((0..hex.len()).step_by(2).map(|k| u8::from_str_radix(&hex[k..k + 2], 16).expect("hex")));
    }
    bytes
}

/// FFmpeg 2da55bf's decode of stream `0:a:0`, interleaved f32, through its
/// C code paths (`-cpuflags 0`: contract.md's reference for float decoders
/// whose C code calls SIMD-selected DSP, here av_tx and swresample), with
/// the container's trims or, `untrimmed`, nothing trimmed
/// (`-flags2 +skip_manual`).
fn ffmpeg_c_path(path: &Path, untrimmed: bool) -> Vec<f32> {
    let mut args = vec!["-v", "error", "-nostdin", "-cpuflags", "0"];
    if untrimmed {
        args.extend(["-flags2", "+skip_manual"]);
    }
    args.extend(["-i", path.to_str().unwrap(), "-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"]);
    let out = check_decoders::tool(refcheck::pinned_ffmpeg().to_str().unwrap(), &args);
    out.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// The decoder on its own: FFmpeg's packets and `OpusHead` of every FATE
/// Opus file (its pre-skip cleared, so the decoder trims nothing) give
/// FFmpeg's untrimmed output bit for bit, the samples the resampler holds
/// back included (drained at the end). No container or trimmer is involved.
#[test]
fn every_fate_opus_file_decodes_to_ffmpegs_samples() {
    let mut failures = Vec::new();
    let files = fate_opus_files();
    for rel in &files {
        let path = refcheck::fate(rel);
        let packets = check_decoders::ffmpeg_packets(&path, "a:0", None);
        let mut params = CodecParameters::audio(CodecId::new("opus"));
        params.extradata = ffmpeg_extradata(&path);
        assert!(params.extradata.len() >= 19, "{rel}: no OpusHead from ffprobe");
        params.extradata[10..12].fill(0);
        let (decoded, errors) = check_decoders::decode_packets(&[oxideav_opus::register], &params, &packets);
        let ours = refcheck::interleaved_f32(&decoded);
        let theirs = ffmpeg_c_path(&path, true);
        let channels = decoded.audio_format.map_or(1, |f| usize::from(f.channels));
        let differ = differing(&theirs, &ours);
        eprintln!("{rel}: {} vs FFmpeg {} samples/channel, {differ} differ", ours.len() / channels, theirs.len() / channels);
        if ours.len() != theirs.len() || differ != 0 || !errors.is_empty() {
            failures.push(format!("{rel}: {} vs {} samples, {differ} differ, errors {errors:?}", ours.len(), theirs.len()));
        }
    }
    assert!(failures.is_empty(), "{} of {} files:\n{}", failures.len(), files.len(), failures.join("\n"));
}

/// The player's path: every FATE Opus file through the demuxer the player
/// opens it with and the container's trims gives FFmpeg's decode bit for
/// bit. `opus/silk-lbrr-mono.mka` ends with a packet that discards 570
/// samples of padding, after which the decoder drains 24 delayed SILK
/// samples: FFmpeg cuts the padding from that packet's frame and keeps the
/// drained ones (`libavcodec/decode.c` trims each frame by the last
/// packet's side data, the padding only where it fits in the frame).
/// `caf/opus.caf` plays through the `OpusHead` the CAF reader builds
/// (`caf_opus_header_is_ffmpegs`).
#[test]
fn every_fate_opus_file_plays_ffmpegs_samples() {
    let registrars: &[Registrar] = &[
        oxideav_opus::__oxideav_entry,
        oxideav_mkv::__oxideav_entry,
        oxideav_ogg::__oxideav_entry,
        oxideav_mpegts::__oxideav_entry,
        demux_misc::register,
    ];
    let files = fate_opus_files();
    let mut failures = Vec::new();
    for rel in &files {
        let path = refcheck::fate(rel);
        let decoded = refcheck::decode(&path, registrars, MediaType::Audio, 0);
        let ours = refcheck::interleaved_f32(&decoded);
        let theirs = ffmpeg_c_path(&path, false);
        let channels = decoded.audio_format.map_or(1, |f| usize::from(f.channels));
        let differ = differing(&theirs, &ours);
        eprintln!("{rel}: {channels} ch, {} vs FFmpeg {} samples/channel, {differ} differ", ours.len() / channels, theirs.len() / channels);
        if ours.len() != theirs.len() || differ != 0 || !decoded.trim_fallbacks.is_empty() {
            failures.push(format!("{rel}: {} vs {} samples, {differ} differ, trims {:?}", ours.len(), theirs.len(), decoded.trim_fallbacks));
        }
    }
    assert!(failures.is_empty(), "{} of {} files:\n{}", failures.len(), files.len(), failures.join("\n"));
}

/// FFmpeg's CAF demuxer does not hand an Opus decoder the `kuki` bytes: it
/// builds a 19-byte `OpusHead` from the stream description, with the packet
/// table's priming frames as the pre-skip (`libavformat/cafdec.c:231-250`,
/// `286-287`). The CAF reader hands the decoder the same header.
#[test]
fn caf_opus_header_is_ffmpegs() {
    let path = refcheck::fate("caf/opus.caf");
    let mut ctx = oxideav_core::RuntimeContext::new();
    demux_misc::register(&mut ctx);
    let format = refcheck::probe_container(&ctx, &path).expect("CAF probe");
    let file = std::fs::File::open(&path).expect("open caf/opus.caf");
    let demuxer = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).expect("open the CAF reader");
    assert_eq!(demuxer.streams()[0].params.extradata, ffmpeg_extradata(&path));
}

/// FATE's chained Ogg Opus: two mono links, 4800 samples each after the
/// pre-skip. Both play, one after the other as FFmpeg plays them, and are
/// FFmpeg's samples bit for bit.
#[test]
fn chained_ogg_matches_ffmpeg() {
    let path = refcheck::fate("ogg-opus/chained-meta.ogg");
    let registrars: &[Registrar] = &[oxideav_opus::__oxideav_entry, oxideav_ogg::__oxideav_entry];
    let decoded = refcheck::decode(&path, registrars, MediaType::Audio, 0);
    let ours = refcheck::interleaved_f32(&decoded);
    let theirs = ffmpeg_c_path(&path, false);
    assert_eq!(ours.len(), theirs.len(), "chained-meta.ogg samples");
    assert_eq!(ours.len(), 9600, "two links of 4800");
    assert_eq!(differing(&theirs, &ours), 0, "chained-meta.ogg differs from FFmpeg's samples");
}

/// A 7.1 Opus file made by FFmpeg's libopus encoder (mapping family 1:
/// 5 streams, 3 coupled; CELT), eight tones, 2 s, in the container `ext`
/// names. Made once per target directory by `refcheck::system_ffmpeg`
/// (libopus); the comparison is with FFmpeg 2da55bf's decode of the same file.
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
    let out = Command::new(refcheck::system_ffmpeg())
        .args(["-v", "error", "-nostdin", "-y", "-filter_complex", &graph, "-map", "[out]"])
        .args(["-c:a", "libopus", "-b:a", "448k", "-f"])
        .arg(match ext {
            "ogg" => "ogg",
            "mkv" => "matroska",
            _ => "mpegts",
        })
        .arg(&partial)
        .output()
        .expect("the system FFmpeg runs");
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

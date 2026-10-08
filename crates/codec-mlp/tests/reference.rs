//! Reference tests: decode every FATE TrueHD / MLP sample through this
//! crate's decoders and the raw `truehd` / `mlp` demuxers, and the generated
//! corpus' TrueHD through the MPEG-TS, Matroska and MP4 demuxers, and compare
//! with FFmpeg 2da55bf (`refcheck::pinned_ffmpeg`).
//! The decoders are integer ports of FFmpeg's, so the comparison is
//! byte-exact: the interleaved PCM must equal FFmpeg's `-f s32le` /
//! `-f s16le` output (what FFmpeg's own FATE tests hash for these samples).
//! The decoder must also report its output layout, and every sample read in
//! the layout a consumer reads it in must be finite.

use oxideav_core::{AudioFormat, Frame, MediaType, SampleFormat};
use refcheck::{Registrar, decode, fate};
use std::path::{Path, PathBuf};

/// The generated corpus file `name` (`PEARTUBE_CORPUS_DIR`, default
/// `~/projects/peartube-media-corpus`, made by `corpus/generate.sh`).
fn corpus(name: &str) -> PathBuf {
    let root = std::env::var_os("PEARTUBE_CORPUS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join("projects/peartube-media-corpus"));
    let path = root.join(name);
    assert!(path.is_file(), "missing corpus file {} (run corpus/generate.sh)", path.display());
    path
}

/// Interleaved PCM bytes of every decoded audio frame (one `data[0]` plane;
/// our decoder always outputs interleaved).
fn pcm_bytes(decoded: &refcheck::Decoded) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in &decoded.frames {
        if let Frame::Audio(a) = frame {
            out.extend_from_slice(&a.data[0]);
        }
    }
    out
}

/// FFmpeg 2da55bf's decode of stream `0:a:0` as interleaved little-endian
/// PCM in `format` (`-f s32le` or `-f s16le`).
fn ffmpeg_pcm(path: &Path, format: SampleFormat) -> Vec<u8> {
    let (muxer, codec) = match format {
        SampleFormat::S32 => ("s32le", "pcm_s32le"),
        SampleFormat::S16 => ("s16le", "pcm_s16le"),
        other => panic!("no FFmpeg PCM muxer for {other:?}"),
    };
    let binary = refcheck::pinned_ffmpeg();
    let out = std::process::Command::new(&binary)
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", muxer, "-c:a", codec, "-"])
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}", binary.display()));
    assert!(
        out.status.success(),
        "{} on {}: {}",
        binary.display(),
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

fn layout(channels: u16, sample_rate: u32, sample_format: SampleFormat) -> AudioFormat {
    AudioFormat { sample_format, sample_rate, channels }
}

/// Decodes the first audio stream of `path` with `registrars` installed and
/// checks, in order: every sample, read in the layout refcheck and the
/// player read it in (the decoder's report, else the container's
/// declaration), is finite; the decoder reports `expected` for every frame;
/// a container that declares channels or a rate declares `expected`'s; and
/// the PCM equals FFmpeg's byte for byte.
fn assert_matches_ffmpeg(path: &Path, registrars: &[Registrar], expected: AudioFormat) {
    let name = path.display();
    let decoded = decode(path, registrars, MediaType::Audio, 0);
    let samples = refcheck::interleaved_f32(&decoded);
    let non_finite = samples.iter().filter(|x| !x.is_finite()).count();
    assert_eq!(non_finite, 0, "{name}: {non_finite} of {} samples read as NaN or infinity", samples.len());
    assert_eq!(decoded.audio_format, Some(expected), "{name}: the decoder's reported layout");
    if let Some(i) = decoded.frame_formats.iter().position(|f| *f != Some(expected)) {
        panic!("{name}: frame {i} reported {:?}, expected {expected:?}", decoded.frame_formats[i]);
    }
    assert!(
        decoded.params.channels.is_none_or(|c| c == expected.channels),
        "{name}: the container declares {:?} channels",
        decoded.params.channels
    );
    assert!(
        decoded.params.sample_rate.is_none_or(|r| r == expected.sample_rate),
        "{name}: the container declares {:?} Hz",
        decoded.params.sample_rate
    );
    let ours = pcm_bytes(&decoded);
    let theirs = ffmpeg_pcm(path, expected.sample_format);
    assert!(!theirs.is_empty(), "{name}: FFmpeg decoded nothing");
    let width = expected.sample_format.bytes_per_sample();
    if let Some(i) = ours.chunks(width).zip(theirs.chunks(width)).position(|(a, b)| a != b) {
        panic!("{name}: sample {i} (channel {}) differs from FFmpeg's", i % usize::from(expected.channels));
    }
    assert_eq!(ours.len(), theirs.len(), "{name}: PCM bytes vs FFmpeg's");
}

/// FFmpeg's `fate-truehd-5.1`:
/// `md5pipe -f truehd -i truehd_5.1.raw -f s32le` (6 ch, s32,
/// ref 95d8aac39dd9f0d7fb83dc7b6f88df35).
#[test]
fn truehd_5_1_bit_exact() {
    let path = fate("lossless-audio/truehd_5.1.raw");
    assert_matches_ffmpeg(&path, &[codec_mlp::register], layout(6, 48_000, SampleFormat::S32));
}

/// `spdif-truehd`'s input: 4 substreams, 8-channel presentation (Atmos; the
/// 4th substream carries non-audio data and is skipped, like FFmpeg).
#[test]
fn truehd_atmos_8ch_bit_exact() {
    let path = fate("truehd/atmos.thd");
    assert_matches_ffmpeg(&path, &[codec_mlp::register], layout(8, 48_000, SampleFormat::S32));
}

/// FFmpeg's `fate-truehd-mono1726`:
/// `md5pipe -f truehd -i ticket-1726-monocut.thd -f s32le` (1 ch,
/// ref 9be9551fac418440bb02101bfdb11df9). The sample exercises the
/// `max_channel + 1 < min_channel` quirk in the restart header range check
/// (two substreams; the second carries the mono presentation with
/// `max_channel == min_channel - 1`).
#[test]
fn truehd_mono_1726_bit_exact() {
    let path = fate("truehd/ticket-1726-monocut.thd");
    assert_matches_ffmpeg(&path, &[codec_mlp::register], layout(1, 48_000, SampleFormat::S32));
}

/// `spdif-truehd-branch-padding`'s input (tests/fate/spdif.mak:43–44):
/// 2 ch, 40 AUs with branch padding the decoder must shorten correctly.
#[test]
fn truehd_branch_padding_bit_exact() {
    let path = fate("truehd/spdifenc-branch-padding.thd");
    assert_matches_ffmpeg(&path, &[codec_mlp::register], layout(2, 48_000, SampleFormat::S32));
}

/// FFmpeg's `fate-lossless-meridianaudio`:
/// `md5 -i lossless-audio/luckynight-partial.mlp -f s16le` (2 ch, s16).
#[test]
fn mlp_meridian_bit_exact() {
    let path = fate("lossless-audio/luckynight-partial.mlp");
    assert_matches_ffmpeg(&path, &[codec_mlp::register], layout(2, 44_100, SampleFormat::S16));
}

/// The corpus' TrueHD (FFmpeg's encoder: mono, 24 bits) in MPEG-TS, through
/// the oxideav-mpegts fork. The TS stream declares no sample format, so a
/// consumer reads the frames in the layout the decoder reports; with no
/// report it read the s32 samples as f32, and small negative samples
/// became NaN.
#[test]
fn truehd_in_mpegts_bit_exact() {
    let path = corpus("h264_truehd.ts");
    let registrars: &[Registrar] = &[codec_mlp::register, oxideav_mpegts::__oxideav_entry];
    assert_matches_ffmpeg(&path, registrars, layout(1, 48_000, SampleFormat::S32));
}

/// The same TrueHD stream in Matroska (`A_TRUEHD`), through oxideav-mkv.
#[test]
fn truehd_in_matroska_bit_exact() {
    let path = corpus("h264_truehd.mkv");
    let registrars: &[Registrar] = &[codec_mlp::register, oxideav_mkv::register];
    assert_matches_ffmpeg(&path, registrars, layout(1, 48_000, SampleFormat::S32));
}

/// The same TrueHD stream in MP4 (`mlpa`), through oxideav-mp4.
#[test]
fn truehd_in_mp4_bit_exact() {
    let path = corpus("h264_truehd.mp4");
    let registrars: &[Registrar] = &[codec_mlp::register, oxideav_mp4::__oxideav_entry];
    assert_matches_ffmpeg(&path, registrars, layout(1, 48_000, SampleFormat::S32));
}

/// The demuxers must cut the same packets ffprobe reports: packet count,
/// every pts, and the time base (ffprobe: both files demux at 1/48000
/// with pts stepping by the 40-sample access unit; luckynight at 1/44100
/// likewise stepping 40 — ffprobe's per-packet time_base for raw MLP/TrueHD
/// is the source rate set by mlp_read_header).
#[test]
fn raw_demuxer_packet_metadata() {
    for (rel, packets, last_pts, rate) in [
        ("truehd/atmos.thd", 128usize, 5080i64, 48_000u32),
        ("truehd/spdifenc-branch-padding.thd", 40, 1560, 48_000),
        ("lossless-audio/luckynight-partial.mlp", 8967, 358640, 44_100),
    ] {
        let path = fate(rel);
        let (count, pts_list, time_base) = demux_packets(&path);
        assert_eq!(count, packets, "{rel}: packet count differs from ffprobe");
        assert_eq!(
            pts_list.last().copied().unwrap_or(0),
            last_pts,
            "{rel}: last packet pts differs from ffprobe"
        );
        // ffprobe reports pts as sample indices at the source rate.
        assert_eq!(
            (time_base.0.num, time_base.0.den),
            (1, i64::from(rate)),
            "{rel}: time base differs from ffprobe"
        );
        // Every packet steps by exactly 40 samples, like ffprobe shows.
        for pair in pts_list.windows(2) {
            assert_eq!(pair[1] - pair[0], 40, "{rel}: pts step differs from ffprobe");
        }
    }
}

/// Demux `path` with only this crate registered; return the packet count,
/// every pts, and the stream time base.
fn demux_packets(path: &std::path::Path) -> (usize, Vec<i64>, oxideav_core::TimeBase) {
    use oxideav_core::{ProbeData, RuntimeContext};
    use std::fs::File;
    use std::io::Read;

    let mut ctx = RuntimeContext::new();
    codec_mlp::register(&mut ctx);
    let mut head = vec![0u8; 256 * 1024];
    let n = File::open(path)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let probe = ProbeData {
        buf: &head[..n],
        ext: ext.as_deref(),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    let format = match candidates.first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
        _ => {
            let ext = ext.as_deref().expect("sample has no extension");
            ctx.containers
                .container_for_extension(ext)
                .unwrap_or_else(|| panic!("no container claims {path:?}"))
                .to_string()
        }
    };
    let file = File::open(path).unwrap();
    let mut demuxer = ctx
        .containers
        .open_demuxer(&format, Box::new(file), &ctx.codecs)
        .unwrap_or_else(|e| panic!("open {format}: {e}"));
    let time_base = demuxer.streams()[0].time_base;
    let mut count = 0usize;
    let mut pts = Vec::new();
    while let Ok(packet) = demuxer.next_packet() {
        pts.push(packet.pts.unwrap_or(0));
        count += 1;
    }
    (count, pts, time_base)
}

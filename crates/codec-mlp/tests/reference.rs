//! Reference tests: decode every FATE TrueHD / MLP sample through this
//! crate's decoders and the raw `truehd` / `mlp` demuxers, and compare
//! against FFmpeg. The decoders are integer ports of FFmpeg's, so the
//! comparison is byte-exact: the interleaved PCM stream MD5 must equal
//! FFmpeg's `-f s32le` / `-f s16le` md5 (the same hash FFmpeg's own FATE
//! tests use for these samples).

use oxideav_core::{Frame, MediaType};
use refcheck::{decode, fate};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_mlp::register]
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

/// FFmpeg's md5 of stream `0:a:nth` as interleaved little-endian PCM
/// (`-f s32le` when `bytes` is 4, `-f s16le` when 2).
fn ffmpeg_pcm_md5(path: &std::path::Path, bytes: usize) -> String {
    let fmt = if bytes == 4 { "s32le" } else { "s16le" };
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", &format!("pcm_{fmt}"), "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(
        out.status.success(),
        "ffmpeg failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    refcheck::md5_hex(&out.stdout)
}

/// FFmpeg's decoded PCM byte count for stream `0:a:0`.
fn ffmpeg_pcm_len(path: &std::path::Path, bytes: usize) -> usize {
    let fmt = if bytes == 4 { "s32le" } else { "s16le" };
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", &format!("pcm_{fmt}"), "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    out.stdout.len()
}

/// FFmpeg's `fate-truehd-5.1`:
/// `md5pipe -f truehd -i truehd_5.1.raw -f s32le` (6 ch, s32,
/// ref 95d8aac39dd9f0d7fb83dc7b6f88df35).
#[test]
fn truehd_5_1_bit_exact() {
    let path = fate("lossless-audio/truehd_5.1.raw");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(6), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes(&decoded);
    assert_eq!(ours.len(), ffmpeg_pcm_len(&path, 4), "pcm byte count");
    assert_eq!(
        refcheck::md5_hex(&ours),
        ffmpeg_pcm_md5(&path, 4),
        "pcm md5 differs from FFmpeg"
    );
}

/// `spdif-truehd`'s input: 4 substreams, 8-channel presentation (Atmos; the
/// 4th substream carries non-audio data and is skipped, like FFmpeg).
#[test]
fn truehd_atmos_8ch_bit_exact() {
    let path = fate("truehd/atmos.thd");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(8), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes(&decoded);
    assert_eq!(ours.len(), ffmpeg_pcm_len(&path, 4), "pcm byte count");
    assert_eq!(
        refcheck::md5_hex(&ours),
        ffmpeg_pcm_md5(&path, 4),
        "pcm md5 differs from FFmpeg"
    );
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
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(1), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes(&decoded);
    assert_eq!(ours.len(), ffmpeg_pcm_len(&path, 4), "pcm byte count");
    assert_eq!(
        refcheck::md5_hex(&ours),
        ffmpeg_pcm_md5(&path, 4),
        "pcm md5 differs from FFmpeg"
    );
}

/// `spdif-truehd-branch-padding`'s input (tests/fate/spdif.mak:43–44):
/// 2 ch, 40 AUs with branch padding the decoder must shorten correctly.
#[test]
fn truehd_branch_padding_bit_exact() {
    let path = fate("truehd/spdifenc-branch-padding.thd");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes(&decoded);
    assert_eq!(ours.len(), ffmpeg_pcm_len(&path, 4), "pcm byte count");
    assert_eq!(
        refcheck::md5_hex(&ours),
        ffmpeg_pcm_md5(&path, 4),
        "pcm md5 differs from FFmpeg"
    );
}

/// FFmpeg's `fate-lossless-meridianaudio`:
/// `md5 -i lossless-audio/luckynight-partial.mlp -f s16le` (2 ch, s16).
#[test]
fn mlp_meridian_bit_exact() {
    let path = fate("lossless-audio/luckynight-partial.mlp");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(44_100), "sample rate");
    let ours = pcm_bytes(&decoded);
    assert_eq!(ours.len(), ffmpeg_pcm_len(&path, 2), "pcm byte count");
    assert_eq!(
        refcheck::md5_hex(&ours),
        ffmpeg_pcm_md5(&path, 2),
        "pcm md5 differs from FFmpeg"
    );
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

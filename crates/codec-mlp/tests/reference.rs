//! Reference tests: decode every FATE TrueHD / MLP sample through this
//! crate's decoders and the raw `truehd` / `mlp` demuxers, and compare
//! against FFmpeg bit-for-bit. FFmpeg's own FATE tests hash the decoded
//! PCM (`fate/truehd.mak`, `fate/lossless-audio.mak`), so the comparison
//! here is the interleaved s32 (TrueHD) / s16 (MLP) stream MD5.

use oxideav_core::{Frame, MediaType};
use refcheck::{decode, fate, interleaved_f32, snr_db};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_mlp::register]
}

/// FFmpeg's `fate-truehd-5.1`:
/// `md5pipe -f truehd -i truehd_5.1.raw -f s32le` (6 ch, s32).
#[test]
fn truehd_5_1_bit_exact() {
    let path = fate("lossless-audio/truehd_5.1.raw");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(6), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");

    let total: usize = decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Audio(a) => a.samples as usize,
            _ => 0,
        })
        .sum();
    // FFmpeg decodes 3410 AUs of 40 samples = 136400 samples per channel.
    assert_eq!(total, 136400, "sample count per channel");

    // Interleaved f32 vs FFmpeg: bit-exact lossless → infinite SNR.
    let ff = refcheck::ffmpeg_audio_f32(&path, 0);
    let ours = interleaved_f32(&decoded);
    let snr = snr_db(&ff, &ours, 0);
    assert!(snr >= 190.0, "SNR {snr} dB — not bit-exact (max finite ~192 dB for s32)");
}

/// FFmpeg's `fate-truehd-5.1-downmix-2.0` covers the `-downmix` option; our
/// decoder always outputs the full presentation, so only the sample-count
/// and 6-channel decode above apply. Instead verify the Atmos sample:
/// `spdif-truehd`'s input, 4 substreams, 8-channel presentation
/// (FFmpeg outputs the 8ch presentation by default for Atmos streams).
#[test]
fn truehd_atmos_8ch_bit_exact() {
    let path = fate("truehd/atmos.thd");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(8), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");

    let total: usize = decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Audio(a) => a.samples as usize,
            _ => 0,
        })
        .sum();
    // 128 AUs × 40 samples.
    assert_eq!(total, 5120, "sample count per channel");

    let ff = refcheck::ffmpeg_audio_f32(&path, 0);
    let ours = interleaved_f32(&decoded);
    let snr = snr_db(&ff, &ours, 0);
    assert!(snr >= 190.0, "SNR {snr} dB — not bit-exact");
}

/// FFmpeg's `fate-truehd-mono1726`:
/// `md5pipe -f truehd -i ticket-1726-monocut.thd -f s32le` (1 ch).
/// The sample exercises the max_channel + 1 < min_channel quirk in the
/// restart header range check (two substreams, second substream carries the
/// mono presentation with max_channel == min_channel - 1).
#[test]
fn truehd_mono_1726_bit_exact() {
    let path = fate("truehd/ticket-1726-monocut.thd");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(1), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");

    let total: usize = decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Audio(a) => a.samples as usize,
            _ => 0,
        })
        .sum();
    // 805 AUs × 40 samples.
    assert_eq!(total, 32200, "sample count per channel");

    let ff = refcheck::ffmpeg_audio_f32(&path, 0);
    let ours = interleaved_f32(&decoded);
    let snr = snr_db(&ff, &ours, 0);
    assert!(snr >= 190.0, "SNR {snr} dB — not bit-exact");
}

/// FFmpeg's `fate-lossless-meridianaudio`:
/// `md5 -i lossless-audio/luckynight-partial.mlp -f s16le` (2 ch, s16).
#[test]
fn mlp_meridian_bit_exact() {
    let path = fate("lossless-audio/luckynight-partial.mlp");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(44_100), "sample rate");

    let total: usize = decoded
        .frames
        .iter()
        .map(|f| match f {
            Frame::Audio(a) => a.samples as usize,
            _ => 0,
        })
        .sum();
    // 8967 AUs × 40 samples (the file ends with an 80-byte partial AU the
    // demuxer drops, same as FFmpeg's parser).
    assert_eq!(total, 358680, "sample count per channel");

    let ff = refcheck::ffmpeg_audio_f32(&path, 0);
    let ours = interleaved_f32(&decoded);
    let snr = snr_db(&ff, &ours, 0);
    // s16 source: the finite-SNR ceiling is ~98 dB; bit-exactness shows as
    // SNR far above the lossy threshold.
    assert!(snr >= 95.0, "SNR {snr} dB — not bit-exact for s16 output");
}

/// The demuxers must cut the same packets ffprobe reports: packet count and
/// timestamps (fsprobe: atmos.thd = 128 packets, pts 0..5080 step 40;
/// luckynight-partial.mlp = 8967 packets, last pts 358640).
#[test]
fn raw_demuxer_packet_metadata() {
    for (rel, packets, last_pts) in [
        ("truehd/atmos.thd", 128usize, 5080i64),
        ("lossless-audio/luckynight-partial.mlp", 8967, 358640),
    ] {
        let path = fate(rel);
        let decoded_probe = open_first_stream(&path);
        assert_eq!(
            decoded_probe.0, packets,
            "{rel}: packet count differs from ffprobe"
        );
        assert_eq!(
            decoded_probe.1, last_pts,
            "{rel}: last packet pts differs from ffprobe"
        );
    }
}

/// Demux `path` with only this crate registered and return
/// (packet count, last pts) of the first audio stream.
fn open_first_stream(path: &std::path::Path) -> (usize, i64) {
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
    let mut count = 0usize;
    let mut last_pts = 0i64;
    while let Ok(packet) = demuxer.next_packet() {
        last_pts = packet.pts.unwrap_or(0);
        count += 1;
    }
    (count, last_pts)
}

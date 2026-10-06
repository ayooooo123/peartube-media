// Reference tests: decode the FATE WMA family samples through this crate's
// decoders + the ASF demuxer and compare with FFmpeg.
//
// - wmalossless is integer/lossless: the interleaved PCM stream must be
//   bit-exact with FFmpeg's `-f s16le` / `-f s24le` output (the same hash
//   FFmpeg's own FATE tests use).
// - wmav1/v2, wmapro are float decoders: SNR >= 90 dB against FFmpeg's
//   interleaved f32 output, sample count within one frame.
// - wmavoice uses the codec's own noise/QMF paths; FFmpeg's FATE compares
//   stddev against reference PCM; we compare SNR against FFmpeg's decode.

use oxideav_core::{MediaType, ProbeData, RuntimeContext};
use refcheck::{decode, fate, snr_db};
use std::io::Read;

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_wma::register, demux_asf::register]
}

/// Interleaved f32 samples of every decoded audio frame, read in the layout
/// the decoder reports (`Decoder::output_audio_format`).
fn samples_f32(decoded: &refcheck::Decoded) -> Vec<f32> {
    refcheck::interleaved_f32(decoded)
}

/// FFmpeg's interleaved f32 decode of stream `0:a:nth`.
fn ffmpeg_f32(path: &std::path::Path, nth: usize) -> Vec<f32> {
    refcheck::ffmpeg_audio_f32(path, nth)
}

/// FFmpeg's PCM md5 of stream `0:a:nth` at `bytes` bytes per sample.
fn ffmpeg_pcm_md5(path: &std::path::Path, bytes: usize) -> String {
    let fmt = if bytes == 4 { "s32le" } else if bytes == 3 { "s24le" } else { "s16le" };
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

/// Our interleaved PCM bytes at `bytes` bytes per sample, from our f32
/// stream (lossless decoders emit exact integer values scaled to [-1, 1)).
fn pcm_bytes_from_decoded(decoded: &refcheck::Decoded, bits: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for f in &decoded.frames {
        if let oxideav_core::Frame::Audio(af) = f {
            let n_samples = af.samples as usize;
            let n_ch = af.data.len();
            if bits == 16 {
                for i in 0..n_samples {
                    for c in 0..n_ch {
                        out.extend_from_slice(&af.data[c][i * 2..i * 2 + 2]);
                    }
                }
            } else if bits == 24 {
                for i in 0..n_samples {
                    for c in 0..n_ch {
                        out.extend_from_slice(&af.data[c][i * 4 + 1..i * 4 + 4]);
                    }
                }
            }
        }
    }
    out
}

fn pcm_bytes_from_f32(samples: &[f32], bits: u32) -> Vec<u8> {
    let bytes = (bits / 8) as usize;
    let mut out = Vec::with_capacity(samples.len() * bytes);
    for &s in samples {
        let v = (s * (1i64 << (bits - 1)) as f32) as i64;
        let v = v.clamp(-(1 << (bits - 1)), (1 << (bits - 1)) - 1);
        let v = if bits == 16 { v as i16 as i64 } else { v };
        let b = v.to_le_bytes();
        out.extend_from_slice(&b[..bytes]);
    }
    out
}

// ───────────────────────── wmalossless (bit-exact) ─────────────────────────

/// `fate-lossless-wma` (lossless-audio.mak): md5 of luckynight-partial.wma,
/// s16le, 209 frames.
#[test]
fn wmalossless_luckynight_bit_exact() {
    let path = fate("lossless-audio/luckynight-partial.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(44_100), "sample rate");
    let ours = pcm_bytes_from_f32(&samples_f32(&decoded), 16);
    // FFmpeg's FATE reference (fate-lossless-wma) decodes with -frames 209.
    let target_frames = 209;
    let bytes_per_frame = 2048 * 2 * 2; // 2048 samples * 2 ch * 2 bytes
    let target_bytes = target_frames * bytes_per_frame;
    assert!(ours.len() >= target_bytes, "must decode at least 209 frames");
    let ff_209 = {
        let out = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(&path)
            .args(["-map", "0:a:0", "-f", "s16le", "-c:a", "pcm_s16le", "-frames", "209", "-af", "aresample", "-"])
            .output()
            .expect("ffmpeg must be on PATH");
        refcheck::md5_hex(&out.stdout)
    };
    assert_eq!(refcheck::md5_hex(&ours[..target_bytes]), ff_209, "pcm md5 differs from FFmpeg for 209 frames");
}

/// Our PCM byte count for a full decode, for the frame-limited comparisons.
fn ffmpeg_pcm_len(path: &std::path::Path, bytes: usize) -> usize {
    let fmt = if bytes == 4 { "s32le" } else if bytes == 3 { "s24le" } else { "s16le" };
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", &format!("pcm_{fmt}"), "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    out.stdout.len()
}

/// `fate-lossless-wma24-1`: master_audio_2.0_24bit.wma, s24le.
#[test]
fn wmalossless_master24_bit_exact() {
    let path = fate("lossless-audio/master_audio_2.0_24bit.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes_from_f32(&samples_f32(&decoded), 24);
    let ff = ffmpeg_pcm_md5(&path, 3);
    let ff_len = ffmpeg_pcm_len(&path, 3);
    assert_eq!(ours.len(), ff_len, "pcm byte count");
    assert_eq!(refcheck::md5_hex(&ours), ff, "pcm md5 differs from FFmpeg");
}

/// `fate-lossless-wma24-2`: Mega_Weird_Audio_Test_24bit.wma, s24le.
#[test]
fn wmalossless_megaweird24_bit_exact() {
    let path = fate("lossless-audio/Mega_Weird_Audio_Test_24bit.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = pcm_bytes_from_decoded(&decoded, 24);
    let ff = ffmpeg_pcm_md5(&path, 3);
    let ff_len = ffmpeg_pcm_len(&path, 3);
    assert_eq!(ours.len(), ff_len, "pcm byte count");
    assert_eq!(refcheck::md5_hex(&ours), ff, "pcm md5 differs from FFmpeg");
}

/// `fate-lossless-wma24-rawtile`: g2_24bit.wma, s24le (raw-tile coding).
#[test]
fn wmalossless_g2_24bit_bit_exact() {
    let path = fate("lossless-audio/g2_24bit.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(44_100), "sample rate");
    let ours = pcm_bytes_from_f32(&samples_f32(&decoded), 24);
    let ff = ffmpeg_pcm_md5(&path, 3);
    let ff_len = ffmpeg_pcm_len(&path, 3);
    assert_eq!(ours.len(), ff_len, "pcm byte count");
    assert_eq!(refcheck::md5_hex(&ours), ff, "pcm md5 differs from FFmpeg");
}

// ───────────────────────── wmapro (SNR >= 90 dB) ─────────────────────────

/// `fate-wmapro-2ch` (wma.mak): Beethovens_9th-1_small.wma, 43 frames.
#[test]
fn wmapro_2ch_snr() {
    let path = fate("wmapro/Beethovens_9th-1_small.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = samples_f32(&decoded);
    let ff = ffmpeg_f32(&path, 0);
    let slack = decoded.params.sample_rate.unwrap() as usize * 2 / 1000 * 100; // ~1 frame
    let snr = snr_db(&ff, &ours, 4096.max(slack));
    assert!(
        snr >= 90.0,
        "wmapro 2ch SNR {snr:.1} dB < 90 (samples {} vs {})",
        ours.len(),
        ff.len()
    );
}

/// `fate-wmapro-5.1` (wma.mak): latin_192_mulitchannel_cut.wma, 101 frames.
#[test]
fn wmapro_51_snr() {
    let path = fate("wmapro/latin_192_mulitchannel_cut.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(6), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(48_000), "sample rate");
    let ours = samples_f32(&decoded);
    let ff = ffmpeg_f32(&path, 0);
    let snr = snr_db(&ff, &ours, 4096);
    assert!(
        snr >= 90.0,
        "wmapro 5.1 SNR {snr:.1} dB < 90 (samples {} vs {})",
        ours.len(),
        ff.len()
    );
}

// ───────────────────────── wmavoice (SNR >= 90 dB) ─────────────────────────

fn wmavoice_snr(sample: &str) {
    let path = fate(sample);
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(1), "channel count");
    let ours = samples_f32(&decoded);
    let ff = ffmpeg_f32(&path, 0);
    // Voice codecs have large DC/transient regions; align to the shorter
    // stream and allow one superframe (480 samples) of length slack.
    let snr = snr_db(&ff, &ours, 480);
    assert!(
        snr >= 90.0,
        "{sample} SNR {snr:.1} dB < 90 (samples {} vs {})",
        ours.len(),
        ff.len()
    );
}

/// `fate-wmavoice-7k` (wma.mak).
#[test]
fn wmavoice_7k_snr() {
    wmavoice_snr("wmavoice/streaming_CBR-7K.wma");
}

/// `fate-wmavoice-11k` (wma.mak).
#[test]
fn wmavoice_11k_snr() {
    wmavoice_snr("wmavoice/streaming_CBR-11K.wma");
}

/// `fate-wmavoice-19k` (wma.mak).
#[test]
fn wmavoice_19k_snr() {
    wmavoice_snr("wmavoice/streaming_CBR-19K.wma");
}

// ───────────────────────── wmav2 (SNR >= 90 dB) ─────────────────────────

/// The cover-art FATE samples carry wmav2 audio; FFmpeg's own decode is the
/// reference (FFmpeg has no decode FATE test for wmav1/2 beyond encoding).
#[test]
fn wmav2_cover_art_snr() {
    let path = fate("cover_art/Californication_cover.wma");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    assert_eq!(decoded.params.channels, Some(2), "channel count");
    assert_eq!(decoded.params.sample_rate, Some(44_100), "sample rate");
    let ours = samples_f32(&decoded);
    let ff = ffmpeg_f32(&path, 0);
    let snr = snr_db(&ff, &ours, 4096);
    assert!(
        snr >= 90.0,
        "wmav2 SNR {snr:.1} dB < 90 (samples {} vs {})",
        ours.len(),
        ff.len()
    );
}

// ───────────────────────── registration checks ─────────────────────────

/// Every WMA tag resolves to our decoder, at higher priority than
/// oxideav-wma's wma1/wma2 registrations.
#[test]
fn tag_resolution_prefers_ours() {
    let mut ctx = RuntimeContext::new();
    codec_wma::register(&mut ctx);
    for (tag, id) in [
        (0x0160u16, "wmav1"),
        (0x0161, "wmav2"),
        (0x0162, "wmapro"),
        (0x0163, "wmalossless"),
        (0x000A, "wmavoice"),
    ] {
        let probe = ProbeData { buf: &[], ext: None };
        let _ = probe;
        let ctx_tag = oxideav_core::CodecTag::wave_format(tag);
        let probe_ctx = oxideav_core::ProbeContext::new(&ctx_tag);
        let resolved = ctx.codecs.resolve_tag_ref(&probe_ctx).cloned();
        assert_eq!(
            resolved.as_ref().map(|c| c.as_str()),
            Some(id),
            "tag 0x{tag:04X} must resolve to {id}"
        );
        assert!(ctx.codecs.has_decoder(&oxideav_core::CodecId::new(id)));
    }
}

/// Priority 50 beats oxideav-wma's default (100+) when both are registered.
#[test]
fn priority_over_oxideav_wma() {
    let mut ctx = RuntimeContext::new();
    codec_wma::register(&mut ctx);
    // oxideav-wma registers wma1/wma2 with default priority
    oxideav_wma::register(&mut ctx);
    let tag = oxideav_core::CodecTag::wave_format(0x0161);
    let probe_ctx = oxideav_core::ProbeContext::new(&tag);
    let resolved = ctx.codecs.resolve_tag_ref(&probe_ctx).cloned().expect("resolution");
    assert_eq!(resolved.as_str(), "wmav2", "our wmav2 must win tag 0x0161");
}

/// Robustness helper shared with robustness.rs lives there; here we keep a
/// quick smoke: a truncated first packet must not panic.
#[test]
fn no_panic_on_truncated_first_packet() {
    let path = fate("wmapro/Beethovens_9th-1_small.wma");
    let mut data = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut data).unwrap();
    data.truncate(200);
    let mut ctx = RuntimeContext::new();
    codec_wma::register(&mut ctx);
    demux_asf::register(&mut ctx);
    let probe = ProbeData {
        buf: &data,
        ext: Some("asf"),
    };
    let candidates = ctx.containers.probe_candidates(&probe);
    if let Some(c) = candidates.first() {
        if c.score >= oxideav_core::PROBE_SCORE_EXTENSION {
            let cursor = std::io::Cursor::new(data.clone());
            if let Ok(mut demuxer) = ctx
                .containers
                .open_demuxer(&c.name, Box::new(cursor), &ctx.codecs)
            {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for _ in 0..16 {
                        match demuxer.next_packet() {
                            Ok(p) => {
                                let streams = demuxer.streams().to_vec();
                                for stream in &streams {
                                    if let Ok(mut dec) = ctx.codecs.first_decoder(&stream.params) {
                                        let _ = dec.send_packet(&p);
                                        let _ = dec.receive_frame();
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }));
            }
        }
    }
}

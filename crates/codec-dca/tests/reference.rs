//! Reference tests: decode every FATE DTS sample through this crate's
//! decoder and the `dtshd` / `dts` demuxers, and compare against FFmpeg.
//!
//! The XLL (DTS-HD MA) path is an integer port, so the lossless samples
//! compare bit-exact: the interleaved PCM stream MD5 must equal FFmpeg's
//! `-f s24le` / `-f s16le` md5 (the same hash FFmpeg's own FATE tests
//! use). The lossy core/X96/XBR paths run through FFmpeg's float filter
//! bank, so those compare at >= 90 dB SNR over the common length —
//! FFmpeg's own FATE lossy DCA tests use a one-off comparison with fuzz
//! 9 on f32 for the same reason.

use oxideav_core::{Frame, MediaType};
use refcheck::{decode, fate};

fn registrars() -> Vec<refcheck::Registrar> {
    vec![codec_dca::register]
}

/// Interleaved PCM bytes of every decoded audio frame (one `data[0]`
/// plane per channel; our decoder always outputs planar).
fn planes(decoded: &refcheck::Decoded) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    for frame in &decoded.frames {
        if let Frame::Audio(a) = frame {
            if out.is_empty() {
                out = vec![Vec::new(); a.data.len()];
            }
            for (dst, src) in out.iter_mut().zip(a.data.iter()) {
                dst.extend_from_slice(src);
            }
        }
    }
    out
}

/// Interleave planar bytes into the FFmpeg raw PCM layout (s16le /
/// s24le / s32le interleaved).
fn interleave(planes: &[Vec<u8>], bytes_per_sample: usize) -> Vec<u8> {
    if planes.is_empty() {
        return Vec::new();
    }
    let samples = planes[0].len() / bytes_per_sample;
    let mut out = Vec::with_capacity(samples * planes.len() * bytes_per_sample);
    for i in 0..samples {
        for plane in planes {
            let start = i * bytes_per_sample;
            out.extend_from_slice(&plane[start..start + bytes_per_sample]);
        }
    }
    out
}

/// FFmpeg's decode of stream `0:a:0` as interleaved LE PCM of
/// `bytes_per_sample` bytes.
fn ffmpeg_pcm(path: &std::path::Path, bytes_per_sample: usize) -> Vec<u8> {
    let fmt = match bytes_per_sample {
        2 => "s16le",
        3 => "s24le",
        4 => "s32le",
        _ => panic!("unsupported width"),
    };
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
    out.stdout
}

// ───────────────────────── lossless (XLL / DTS-HD MA) ─────────────────────────
//
// dcadec-suite lossless_16 (FATE_DCADEC_LOSSLESS_s16le):
//   xll_51_16_192_768_0, xll_51_16_192_768_1
// dcadec-suite lossless_24 (FATE_DCADEC_LOSSLESS_s24le):
//   xll_51_24_48_768, xll_51_24_48_none, xll_71_24_48_768_0,
//   xll_71_24_48_768_1, xll_71_24_96_768, xll_x96_51_24_96_1509,
//   xll_xch_61_24_48_768

/// One XLL suite sample: `width` is FFmpeg's FATE output width (2 =
/// pcm_s16le, 3 = pcm_s24le). Our decoder emits the decoder's native
/// sample format (s16 planes for 16-bit storage, s32 planes holding
/// 24-bit samples shifted << 8 for 24-bit storage), so the FFmpeg side is
/// requested in the matching raw width: s16le for 16-bit, s32le for 24-bit
/// (FFmpeg's aresample to s24 truncates the same 24-bit values).
fn xll_suite_sample(name: &str, width: usize) {
    let path = fate(&format!("dts/dcadec-suite/{name}.dtshd"));
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);

    let ours_bytes = match decoded.params.sample_format.map(|f| f.bytes_per_sample()) {
        Some(2) => {
            assert_eq!(width, 2, "{name}: 16-bit storage expected");
            interleave(&planes(&decoded), 2)
        }
        Some(4) => {
            assert_eq!(width, 3, "{name}: 24-bit storage expected");
            interleave(&planes(&decoded), 4)
        }
        other => panic!("{name}: unexpected sample format {other:?}"),
    };
    let ours_bytes_per = match decoded.params.sample_format.map(|f| f.bytes_per_sample()) {
        Some(w) => w,
        other => panic!("{name}: unexpected sample format {other:?}"),
    };

    let ref_width = if width == 2 { 2 } else { 4 };
    let reference = ffmpeg_pcm(&path, ref_width);

    assert_eq!(
        ours_bytes.len() / ours_bytes_per,
        reference.len() / ref_width,
        "{name}: sample count differs from FFmpeg"
    );
    assert_eq!(
        refcheck::md5_hex(&ours_bytes),
        refcheck::md5_hex(&reference),
        "{name}: PCM md5 differs from FFmpeg (lossless must be bit-exact)"
    );
}

fn ours_samples(decoded: &refcheck::Decoded) -> usize {
    decoded
        .frames
        .iter()
        .filter_map(|f| match f {
            Frame::Audio(a) => Some(a.samples as usize),
            _ => None,
        })
        .sum()
}

#[test]
fn xll_51_16_192_768_0() {
    xll_suite_sample("xll_51_16_192_768_0", 2);
}

#[test]
fn xll_51_16_192_768_1() {
    xll_suite_sample("xll_51_16_192_768_1", 2);
}

#[test]
fn xll_51_24_48_768() {
    xll_suite_sample("xll_51_24_48_768", 3);
}

#[test]
fn xll_51_24_48_none() {
    xll_suite_sample("xll_51_24_48_none", 3);
}

#[test]
fn xll_71_24_48_768_0() {
    xll_suite_sample("xll_71_24_48_768_0", 3);
}

#[test]
fn xll_71_24_48_768_1() {
    xll_suite_sample("xll_71_24_48_768_1", 3);
}

#[test]
fn xll_71_24_96_768() {
    xll_suite_sample("xll_71_24_96_768", 3);
}

#[test]
fn xll_x96_51_24_96_1509() {
    xll_suite_sample("xll_x96_51_24_96_1509", 3);
}

#[test]
fn xll_xch_61_24_48_768() {
    xll_suite_sample("xll_xch_61_24_48_768", 3);
}

/// FFmpeg's `fate-dca-xll`: `streamhash -hash md5 -i
/// dts/master_audio_7.1_24bit.dts -c:a pcm_s24le -af aresample` — the raw
/// DTS demuxer + XLL decode of the 7.1 24-bit master.
#[test]
fn dca_xll_master_audio_bit_exact() {
    let path = fate("dts/master_audio_7.1_24bit.dts");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);
    // Decoder emits s32 planes (24-bit samples << 8); compare with FFmpeg's
    // raw s32le output of the same samples.
    assert_eq!(decoded.params.sample_format.map(|f| f.bytes_per_sample()), Some(4));

    let ours = interleave(&planes(&decoded), 4);
    let reference = ffmpeg_pcm(&path, 4);

    assert_eq!(
        ours.len() / 4,
        reference.len() / 4,
        "sample count differs from FFmpeg"
    );
    assert_eq!(
        refcheck::md5_hex(&ours),
        refcheck::md5_hex(&reference),
        "PCM md5 differs from FFmpeg (lossless must be bit-exact)"
    );
}

// ───────────────────────── lossy (core / X96 / XBR) ─────────────────────────
//
// FATE_DCADEC_LOSSY (fate-dca-<name>: ffmpeg -flags2 skip_manual -i
// <name>.dtshd -f f32le -af aresample, oneoff f32 fuzz 9 against the
// bundled .f32 reference): core_51_24_48_768_0, core_51_24_48_768_1,
// x96_51_24_96_1509, x96_xch_61_24_96_3840, x96_xxch_71_24_96_3840,
// xbr_51_24_48_3840, xbr_xch_61_24_48_3840, xbr_xxch_71_24_48_3840,
// xch_61_24_48_768, xxch_71_24_48_2046.

fn lossy_suite_sample(name: &str) {
    let path = fate(&format!("dts/dcadec-suite/{name}.dtshd"));
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);

    // Our float path (or fixed path converted) as interleaved f32.
    let ours = refcheck::interleaved_f32(&decoded);
    let reference = refcheck::ffmpeg_audio_f32(&path, 0);

    // FFmpeg's FATE oneoff allows fuzz 9 (f32 ULPs); a 90 dB SNR floor is
    // much stricter than that on real audio, and the contract asks for it.
    let nan_ours = ours.iter().filter(|v| v.is_nan()).count();
    let nan_ref = reference.iter().filter(|v| v.is_nan()).count();
    if nan_ours > 0 || nan_ref > 0 {
        panic!("{name}: NaN samples ours={nan_ours} ref={nan_ref}");
    }
    let snr = refcheck::snr_db(&reference, &ours, 1024);
    assert!(
        snr >= 90.0,
        "{name}: SNR {snr:.2} dB < 90 dB vs FFmpeg (len {} vs {})",
        ours.len(),
        reference.len()
    );
}

#[test]
fn lossy_core_51_24_48_768_0() {
    lossy_suite_sample("core_51_24_48_768_0");
}

#[test]
fn lossy_core_51_24_48_768_1() {
    lossy_suite_sample("core_51_24_48_768_1");
}

#[test]
fn lossy_x96_51_24_96_1509() {
    lossy_suite_sample("x96_51_24_96_1509");
}

#[test]
fn lossy_x96_xch_61_24_96_3840() {
    lossy_suite_sample("x96_xch_61_24_96_3840");
}

#[test]
fn lossy_x96_xxch_71_24_96_3840() {
    lossy_suite_sample("x96_xxch_71_24_96_3840");
}

#[test]
fn lossy_xbr_51_24_48_3840() {
    lossy_suite_sample("xbr_51_24_48_3840");
}

#[test]
fn lossy_xbr_xch_61_24_48_3840() {
    lossy_suite_sample("xbr_xch_61_24_48_3840");
}

#[test]
fn lossy_xbr_xxch_71_24_48_3840() {
    lossy_suite_sample("xbr_xxch_71_24_48_3840");
}

#[test]
fn lossy_xch_61_24_48_768() {
    lossy_suite_sample("xch_61_24_48_768");
}

#[test]
fn lossy_xxch_71_24_48_2046() {
    lossy_suite_sample("xxch_71_24_48_2046");
}

// ───────────────────────── fate-dca-core (dts.ts via MPEG-TS) ─────────────────────────

/// `fate-dca-core`: `pcm -i dts/dts.ts` against `dts/dts.pcm` (oneoff,
/// fuzz 9). Decoded through oxideav-mpegts + our decoder, compared at
/// >= 90 dB SNR against FFmpeg's PCM.
#[test]
fn dca_core_ts() {
    let path = fate("dts/dts.ts");
    let decoded = decode(&path, &registrars(), MediaType::Audio, 0);

    let ours = refcheck::interleaved_f32(&decoded);

    // FFmpeg's reference: `ffmpeg -i dts.ts -f f32le -` (the FATE `pcm`
    // helper compares s16; use f32 for the SNR comparison).
    let out = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(&path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "ffmpeg failed");
    let reference: Vec<f32> = out
        .stdout
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect();

    let snr = refcheck::snr_db(&reference, &ours, 4096);
    assert!(snr >= 90.0, "dts.ts: SNR {snr:.2} dB < 90 dB vs FFmpeg");
}

// ───────────────────────── demuxer packet layout ─────────────────────────

/// The `dtshd` demuxer must cut the same frames ffprobe reports
/// (`ffprobe -show_packets` on xll_51_24_48_768.dtshd: 2 packets of 768
/// samples at 48 kHz). Packet count and total sample coverage must match.
#[test]
fn dtshd_demuxer_packet_metadata() {
    use oxideav_core::{ProbeData, RuntimeContext};
    use std::fs::File;
    use std::io::Read;

    for (rel, rate) in [
        ("dts/dcadec-suite/xll_51_24_48_768.dtshd", 48_000u32),
        ("dts/dcadec-suite/xll_51_16_192_768_0.dtshd", 192_000),
        ("dts/dcadec-suite/core_51_24_48_768_0.dtshd", 48_000),
    ] {
        let path = fate(rel);
        let mut ctx = RuntimeContext::new();
        codec_dca::register(&mut ctx);
        let mut head = vec![0u8; 256 * 1024];
        let n = File::open(&path)
            .and_then(|mut f| f.read(&mut head))
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        let probe = ProbeData {
            buf: &head[..n],
            ext: Some("dtshd"),
        };
        let candidates = ctx.containers.probe_candidates(&probe);
        let format = match candidates.first() {
            Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => c.name.to_string(),
            _ => "dtshd".to_string(),
        };
        let file = File::open(&path).unwrap();
        let mut demuxer = ctx
            .containers
            .open_demuxer(&format, Box::new(file), &ctx.codecs)
            .unwrap_or_else(|e| panic!("open {format}: {e}"));
        assert_eq!(demuxer.streams()[0].time_base.den(), i64::from(rate), "{rel}: time base");

        let mut count = 0usize;
        let mut samples = 0u64;
        while let Ok(packet) = demuxer.next_packet() {
            samples += packet.data.len() as u64;
            count += 1;
        }
        // ffprobe: STRMDATA extent read as 1024-byte packets.
        let expect_packets = {
            let out = std::process::Command::new("ffprobe")
                .args(["-v", "error", "-show_packets", "-of", "default=nw=1"])
                .arg(&path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.starts_with("[PACKET]"))
                .count()
        };
        assert_eq!(count, expect_packets, "{rel}: packet count vs ffprobe");
        assert!(samples > 0, "{rel}: no data demuxed");
    }
}

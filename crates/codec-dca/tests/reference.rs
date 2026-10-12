//! Reference tests: decode every FATE DTS sample of FFmpeg's dca.mak and
//! compare against FFmpeg. The DTS-HD suite and the raw master go
//! through this crate's decoder and its `dtshd` / `dts` demuxers;
//! fate-dca-core (dts/dts.ts) and fate-dts_es (dts/dts_es.dts) go
//! through the player's whole registry, container probe included.
//!
//! The XLL (DTS-HD MA) path is an integer port, so the lossless samples
//! compare bit-exact: the interleaved PCM stream MD5 must equal FFmpeg's
//! `-f s24le` / `-f s16le` md5 (the same hash FFmpeg's own FATE tests
//! use). The lossy core/X96/XBR paths run FFmpeg's float filter banks;
//! they compare bit-exact with FFmpeg's C code paths (`-cpuflags 0`),
//! since its NEON synthesis filters round differently. That needs the
//! fused multiply-adds clang makes of the C (`-ffp-contract=on`).

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
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", fmt, "-c:a", &format!("pcm_{fmt}"), "-"])
        .output()
        .expect("the pinned FFmpeg runs");
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

    let ours_bytes = match decoded.audio_format.map(|f| f.sample_format.bytes_per_sample()) {
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
    let ours_bytes_per = match decoded.audio_format.map(|f| f.sample_format.bytes_per_sample()) {
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
    assert_eq!(decoded.audio_format.map(|f| f.sample_format.bytes_per_sample()), Some(4));

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
    let reference = ffmpeg_c_f32(&path);
    assert_eq!(ours.len(), reference.len(), "{name}: interleaved samples vs FFmpeg");
    let first = ours.iter().zip(&reference).position(|(a, b)| a.to_bits() != b.to_bits());
    assert_eq!(first, None, "{name}: first sample unlike FFmpeg's C path");
}

/// FFmpeg 2da55bf's decode of stream `0:a:0` on its C code paths
/// (`-cpuflags 0`), interleaved f32.
fn ffmpeg_c_f32(path: &std::path::Path) -> Vec<f32> {
    let out = std::process::Command::new(refcheck::pinned_ffmpeg())
        .args(["-v", "error", "-nostdin", "-cpuflags", "0", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-f", "f32le", "-c:a", "pcm_f32le", "-"])
        .output()
        .expect("the pinned FFmpeg runs");
    assert!(out.status.success(), "ffmpeg {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
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

// ─────────────── fate-dca-core and fate-dts_es, through the player ───────────────
//
// dts/dts.ts carries DTS as MPEG-TS stream type 0x06 (private PES) with no
// ES_info descriptor at all; the TS demuxer identifies it from the payload
// the way FFmpeg probes such streams. dts/dts_es.dts is raw core + XCh.
// Both open with the production registry: the probe picks the container,
// the registry resolves the decoder, and every decoded sample is compared
// with FFmpeg's decode of the same file.

/// The channel count FFmpeg's decoder reports for stream `0:a:0` (MPEG-TS
/// input prints the stream once more inside its program).
fn ffprobe_channels(path: &std::path::Path) -> usize {
    let out = std::process::Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=channels", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    assert!(out.status.success(), "ffprobe {} failed", path.display());
    let text = String::from_utf8_lossy(&out.stdout);
    let first = text.lines().find(|l| !l.trim().is_empty()).expect("ffprobe reported no audio stream");
    first.trim().parse().expect("ffprobe channel count")
}

fn production_decode_matches_ffmpeg(rel: &str, container: &str) {
    let path = fate(rel);
    let ctx = codecs::context();
    assert_eq!(refcheck::probe_container(&ctx, &path).as_deref(), Ok(container), "{rel}: container");
    let decoded = decode(&path, &[codecs::register_all], MediaType::Audio, 0);
    assert_eq!(decoded.params.codec_id.as_str(), "dts", "{rel}: codec");
    let ours = refcheck::interleaved_f32(&decoded);
    let reference = ffmpeg_c_f32(&path);
    let channels = decoded.audio_format.map(|f| usize::from(f.channels));
    assert_eq!(
        (ours.len(), channels),
        (reference.len(), Some(ffprobe_channels(&path))),
        "{rel}: decoded samples and channels vs FFmpeg"
    );
    let first = ours.iter().zip(&reference).position(|(a, b)| a.to_bits() != b.to_bits());
    assert_eq!(first, None, "{rel}: first sample unlike FFmpeg's C path");
}

/// dca.mak fate-dca-core: `pcm -i dts/dts.ts`.
#[test]
fn dca_core_mpegts() {
    production_decode_matches_ffmpeg("dts/dts.ts", "mpegts");
}

/// dca.mak fate-dts_es: `pcm -i dts/dts_es.dts`.
#[test]
fn dts_es_raw() {
    production_decode_matches_ffmpeg("dts/dts_es.dts", "dts");
}

// ───────────────────────── demuxer packet layout ─────────────────────────

/// One `ffprobe -show_packets` row.
struct FfPacket {
    stream: u32,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    md5: String,
}

/// One `ffprobe -show_streams` row.
struct FfStream {
    codec_name: String,
    time_base: (i64, i64),
    start_pts: Option<i64>,
    duration_ts: Option<i64>,
}

/// FFmpeg's demuxed-and-parsed packet table for `path`, as `ffprobe
/// -show_packets -show_streams` prints it. Fails unless ffprobe succeeds
/// and reports at least one stream and one packet.
fn ffprobe_table(path: &std::path::Path) -> (Vec<FfStream>, Vec<FfPacket>) {
    let out = std::process::Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-show_data_hash", "md5", "-show_entries"])
        .arg(
            "stream=codec_name,time_base,start_pts,duration_ts:\
             packet=stream_index,pts,dts,duration,size,data_hash",
        )
        .args(["-of", "compact"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    assert!(
        out.status.success(),
        "ffprobe {} failed: {}",
        path.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let ts = |v: &str| if v == "N/A" { None } else { Some(v.parse::<i64>().unwrap()) };
    let (mut streams, mut packets) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: std::collections::HashMap<&str, &str> =
            fields.filter_map(|f| f.split_once('=')).collect();
        match section {
            "packet" => packets.push(FfPacket {
                stream: kv["stream_index"].parse().unwrap(),
                pts: ts(kv["pts"]),
                dts: ts(kv["dts"]),
                duration: ts(kv["duration"]),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
            }),
            "stream" => {
                let (num, den) = kv["time_base"].split_once('/').unwrap();
                streams.push(FfStream {
                    codec_name: kv["codec_name"].to_string(),
                    time_base: (num.parse().unwrap(), den.parse().unwrap()),
                    start_pts: ts(kv["start_pts"]),
                    duration_ts: ts(kv["duration_ts"]),
                });
            }
            _ => {}
        }
    }
    assert!(
        !streams.is_empty() && !packets.is_empty(),
        "ffprobe {}: empty oracle ({} streams, {} packets)",
        path.display(),
        streams.len(),
        packets.len()
    );
    (streams, packets)
}

/// `a` ticks of time base `ta` and `b` ticks of `tb` name the same instant.
fn same_time(a: Option<i64>, ta: (i64, i64), b: Option<i64>, tb: (i64, i64)) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            i128::from(a) * i128::from(ta.0) * i128::from(tb.1)
                == i128::from(b) * i128::from(tb.0) * i128::from(ta.1)
        }
        (None, None) => true,
        _ => false,
    }
}

/// Every DTS-HD input of FFmpeg's dca.mak (DCADEC_SUITE_LOSSLESS_16,
/// DCADEC_SUITE_LOSSLESS_24 and DCADEC_SUITE_LOSSY).
const DTSHD_SUITE: [&str; 19] = [
    "xll_51_16_192_768_0",
    "xll_51_16_192_768_1",
    "xll_51_24_48_768",
    "xll_51_24_48_none",
    "xll_71_24_48_768_0",
    "xll_71_24_48_768_1",
    "xll_71_24_96_768",
    "xll_x96_51_24_96_1509",
    "xll_xch_61_24_48_768",
    "core_51_24_48_768_0",
    "core_51_24_48_768_1",
    "x96_51_24_96_1509",
    "x96_xch_61_24_96_3840",
    "x96_xxch_71_24_96_3840",
    "xbr_51_24_48_3840",
    "xbr_xch_61_24_48_3840",
    "xbr_xxch_71_24_48_3840",
    "xch_61_24_48_768",
    "xxch_71_24_48_2046",
];

/// The `dtshd` demuxer cuts the frames FFmpeg's dca parser cuts and times
/// them as libavformat does (FATE's dca-xll tests check the same
/// `packet=pts,duration` table): for every DTS-HD sample, the stream's
/// codec, time base, start time and duration match `ffprobe`, and each
/// packet's size, payload MD5, pts, dts and duration match, rescaled
/// through both time bases. The packet durations cover the stream's whole
/// duration.
#[test]
fn dtshd_demuxer_packet_tables_match_ffprobe() {
    use oxideav_core::{RuntimeContext, TimeBase};

    for name in DTSHD_SUITE {
        let path = fate(&format!("dts/dcadec-suite/{name}.dtshd"));
        let (ff_streams, ff_packets) = ffprobe_table(&path);

        let mut ctx = RuntimeContext::new();
        codec_dca::register(&mut ctx);
        let format = refcheck::probe_container(&ctx, &path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(format, "dtshd", "{name}: container");
        let file = std::fs::File::open(&path).unwrap();
        let mut demuxer = ctx
            .containers
            .open_demuxer(&format, Box::new(file), &ctx.codecs)
            .unwrap_or_else(|e| panic!("{name}: open: {e}"));

        assert_eq!(demuxer.streams().len(), ff_streams.len(), "{name}: stream count");
        let stream = demuxer.streams()[0].clone();
        let ff = &ff_streams[0];
        let tb = |t: TimeBase| (t.num(), t.den());
        assert_eq!(stream.params.codec_id.as_str(), ff.codec_name, "{name}: codec");
        assert_eq!(tb(stream.time_base), ff.time_base, "{name}: time base");
        assert!(
            same_time(stream.start_time, tb(stream.time_base), ff.start_pts, ff.time_base),
            "{name}: start time {:?} vs ffprobe {:?}",
            stream.start_time,
            ff.start_pts
        );
        assert!(
            same_time(stream.duration, tb(stream.time_base), ff.duration_ts, ff.time_base),
            "{name}: duration {:?} vs ffprobe {:?}",
            stream.duration,
            ff.duration_ts
        );

        let mut ours = Vec::new();
        loop {
            match demuxer.next_packet() {
                Ok(p) => ours.push(p),
                Err(oxideav_core::Error::Eof) => break,
                Err(e) => panic!("{name}: demux: {e}"),
            }
        }
        assert_eq!(ours.len(), ff_packets.len(), "{name}: packet count");
        for (i, (p, f)) in ours.iter().zip(&ff_packets).enumerate() {
            let ptb = tb(p.time_base);
            assert_eq!(p.stream_index, f.stream, "{name}: packet {i} stream");
            assert_eq!(p.data.len(), f.size, "{name}: packet {i} size");
            assert_eq!(refcheck::md5_hex(&p.data), f.md5, "{name}: packet {i} payload");
            for (what, a, b) in [("pts", p.pts, f.pts), ("dts", p.dts, f.dts), ("duration", p.duration, f.duration)] {
                assert!(
                    same_time(a, ptb, b, ff.time_base),
                    "{name}: packet {i} {what} {a:?} vs ffprobe {b:?}"
                );
            }
        }
        let covered: i64 = ff_packets.iter().map(|f| f.duration.unwrap_or(0)).sum();
        assert_eq!(Some(covered), ff.duration_ts, "{name}: packet durations vs stream duration");
    }
}

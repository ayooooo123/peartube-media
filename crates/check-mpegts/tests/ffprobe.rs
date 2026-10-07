//! The `mpegts` demuxer the player opens against `ffprobe` (FFmpeg's
//! demuxer with the parser it runs on each stream): every packet's PTS,
//! DTS, duration, size, key flag and payload MD5 with the interleaving
//! across streams, and the stream parameters a decoder and sink start
//! from.
//!
//! Needs `ffprobe`, the generated corpus (`PEARTUBE_CORPUS_DIR`) and the
//! FATE suite (`FATE_SUITE`).

use std::path::PathBuf;

use check_mpegts::{corpus, differences, ffprobe_packets, ffprobe_streams, open, our_packets};

/// H.264 with AAC, AC-3 (0x81) and E-AC-3 (ATSC 0x87); FATE's H.264,
/// MPEG-2 and MPEG audio streams, one ending in a partial packet, and an
/// AC-3 PID its PMT lists twice.
fn samples() -> Vec<PathBuf> {
    let generated = ["h264_aac.ts", "h264_ac3.ts", "h264_eac3.ts"].map(corpus);
    let fate = [
        "mpegts/h264small.ts",
        "h264/h264_intra_first-small.ts",
        "ac3/mp3ac325-4864-small.ts",
        "mpeg2/xdcam8mp2-1s_small.ts",
        "mpeg2/mpeg2_field_encoding.ts",
        "mpegts/mpegts_sdt_data_stream.ts",
        "sub/scte20.ts",
        "lcevc/L_H264_640x360p_8bit8bit_2D_dd.ts",
    ]
    .map(refcheck::fate);
    generated.into_iter().chain(fate).collect()
}

#[test]
fn packets_equal_ffprobe() {
    let mut failures = Vec::new();
    for path in samples() {
        let ours = our_packets(&path);
        let diff = differences(&ours.packets, &ffprobe_packets(&path, &[]), &ours.error);
        eprintln!("{}: {} packets{}", path.display(), ours.packets.len(), if diff.is_empty() { "" } else { " DIFFER" });
        if !diff.is_empty() {
            failures.push(format!("{}: {diff}", path.display()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn stream_parameters_equal_ffprobe() {
    let mut failures = Vec::new();
    for path in samples() {
        let demuxer = open(Box::new(std::fs::File::open(&path).unwrap())).unwrap();
        let streams = demuxer.streams();
        let entries = "index,codec_type,codec_name,width,height,pix_fmt,sample_rate,channels";
        for theirs in ffprobe_streams(&path, entries) {
            let kind = theirs["codec_type"].as_str();
            if kind != "video" && kind != "audio" {
                continue;
            }
            let index: usize = theirs["index"].parse().unwrap();
            let Some(stream) = streams.get(index) else {
                failures.push(format!("{}: no stream {index}", path.display()));
                continue;
            };
            let p = &stream.params;
            let show = |v: Option<String>| v.unwrap_or_else(|| "N/A".into());
            let mut ours = vec![("codec_name", p.codec_id.as_str().to_string())];
            if kind == "video" {
                ours.push(("width", show(p.width.map(|v| v.to_string()))));
                ours.push(("height", show(p.height.map(|v| v.to_string()))));
                let pix_fmt = p.pixel_format.and_then(refcheck::ffmpeg_pix_fmt_name).map(str::to_string);
                ours.push(("pix_fmt", show(pix_fmt)));
            } else {
                ours.push(("sample_rate", show(p.sample_rate.map(|v| v.to_string()))));
                // FFmpeg's E-AC-3 channel count comes from its decoder, not
                // the parser.
                if theirs["codec_name"] != "eac3" {
                    ours.push(("channels", show(p.channels.map(|v| v.to_string()))));
                }
            }
            for (key, value) in ours {
                if theirs[key] != value {
                    failures.push(format!("{} s{index} {key}: ours {value}, ffprobe {}", path.display(), theirs[key]));
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

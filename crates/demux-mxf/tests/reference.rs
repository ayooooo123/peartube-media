//! Every FATE sample FFmpeg tests MXF with (tests/fate/mxf.mak, demux.mak,
//! h264.mak, video.mak): the streams (codec, type, time base) and the
//! whole packet table (stream, pts, dts, duration, size, key flag, MD5 of
//! the data) equal `ffprobe`'s.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use oxideav_core::{Error, MediaType, RuntimeContext};
use refcheck::fate;

#[derive(Debug, PartialEq)]
struct Pkt {
    stream: u32,
    pts: Option<i64>,
    dts: Option<i64>,
    duration: Option<i64>,
    size: usize,
    key: bool,
    md5: String,
}

/// (codec name, media type, time base) per stream, as ffprobe names them.
fn ffprobe_streams(path: &Path) -> Vec<(String, String, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,codec_type,time_base", "-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    assert!(out.status.success(), "ffprobe {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("stream|"))
        .map(|l| {
            let kv: HashMap<&str, &str> = l.split('|').filter_map(|f| f.split_once('=')).collect();
            (kv["codec_name"].to_string(), kv["codec_type"].to_string(), kv["time_base"].to_string())
        })
        .collect()
}

fn ffprobe_packets(path: &Path) -> Vec<Pkt> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,dts,duration,size,flags,data_hash", "-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe on PATH");
    assert!(out.status.success(), "ffprobe {}", path.display());
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("packet|"))
        .map(|l| {
            let kv: HashMap<&str, &str> = l.split('|').filter_map(|f| f.split_once('=')).collect();
            Pkt {
                stream: kv["stream_index"].parse().unwrap(),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
                duration: num(kv.get("duration")),
                size: kv["size"].parse().unwrap(),
                key: kv["flags"].starts_with('K'),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
            }
        })
        .collect()
}

fn check(sample: &str) {
    let path = fate(sample);
    let mut ctx = RuntimeContext::new();
    demux_mxf::register(&mut ctx);
    let mut d = ctx.containers.open_demuxer("mxf", Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs).unwrap();

    let streams: Vec<(String, String, String)> = d
        .streams()
        .iter()
        .map(|s| {
            let codec = match s.params.codec_id.as_str() {
                "none" => "unknown".to_string(),
                c => c.to_string(),
            };
            let media = match s.params.media_type {
                MediaType::Video => "video",
                MediaType::Audio => "audio",
                MediaType::Subtitle => "subtitle",
                _ => "data",
            };
            let tb = s.time_base.as_rational();
            (codec, media.to_string(), format!("{}/{}", tb.num, tb.den))
        })
        .collect();
    assert_eq!(streams, ffprobe_streams(&path), "{sample}: streams");

    let mut got = Vec::new();
    loop {
        match d.next_packet() {
            Ok(p) => got.push(Pkt {
                stream: p.stream_index,
                pts: p.pts,
                dts: p.dts,
                duration: p.duration,
                size: p.data.len(),
                key: p.flags.keyframe,
                md5: refcheck::md5_hex(&p.data),
            }),
            Err(Error::Eof) => break,
            Err(e) => panic!("{sample}: {e} after {} packets", got.len()),
        }
    }
    let want = ffprobe_packets(&path);
    let first = got.iter().zip(&want).position(|(g, w)| g != w);
    if let Some(i) = first {
        panic!("{sample}: packet {i} is {:?}, FFmpeg's {:?}", got[i], want[i]);
    }
    assert_eq!(got.len(), want.len(), "{sample}: packet count");
    assert!(!want.is_empty(), "{sample}: FFmpeg's packets");
}

macro_rules! samples {
    ($($name:ident: $path:literal,)*) => {
        $(
            #[test]
            fn $name() {
                check($path);
            }
        )*
    };
}

samples! {
    avid_dv25: "mxf/Avid-00005.mxf",
    c0023_mpeg4_clip_wrapped_by_unknown_wrapping: "mxf/C0023S01.mxf",
    meridian_prores_hdr10: "mxf/Meridian-Apple_ProResProxy-HDR10.mxf",
    sony_d10: "mxf/Sony-00001.mxf",
    multiple_components_dnxhd: "mxf/multiple_components.mxf",
    omneon_xdcam_temporal_offsets: "mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf",
    opatom_essence_group: "mxf/opatom_essencegroup_alpha_raw.mxf",
    opatom_missing_index: "mxf/opatom_missing_index.mxf",
    track_01_v02_dnxhd: "mxf/track_01_v02.mxf",
    track_02_a01_pcm: "mxf/track_02_a01.mxf",
    sony_xavc_long_gop_h264: "h264/SonyXAVC_LongGOP_green_pixelation_early_Frames.MXF",
    imf_countdown_jpeg2000: "imf/countdown/countdown-small.mxf",
    dcinema_jpeg2000: "jpeg2000/chiens_dcinema2K.mxf",
}

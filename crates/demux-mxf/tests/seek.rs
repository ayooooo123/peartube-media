//! Seeking as `ffprobe -read_intervals T%+#N` does (avformat_seek_file of
//! the default stream, mxf_read_seek with AVSEEK_FLAG_BACKWARD): the next
//! N packets equal FFmpeg's, on index tables with temporal offsets (long
//! GOP MPEG-2 and H.264), constant byte count indexes (DV, D-10), and a
//! clip-wrapped OPAtom track whose index the demuxer makes up.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use oxideav_core::{MediaType, RuntimeContext};
use refcheck::fate;

type Pkt = (u32, Option<i64>, Option<i64>, usize, bool, String);

fn ffprobe_after_seek(path: &Path, seconds: &str, n: usize) -> Vec<Pkt> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-read_intervals", &format!("{seconds}%+#{n}"), "-show_data_hash", "md5"])
        .args(["-show_entries", "packet=stream_index,pts,dts,size,flags,data_hash", "-of", "compact"])
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
            (
                kv["stream_index"].parse().unwrap(),
                num(kv.get("pts")),
                num(kv.get("dts")),
                kv["size"].parse().unwrap(),
                kv["flags"].starts_with('K'),
                kv["data_hash"].trim_start_matches("MD5:").to_string(),
            )
        })
        .collect()
}

/// After reading `read` packets, a seek of the stream FFmpeg picks
/// (av_find_default_stream_index) to `seconds` gives FFmpeg's next `n`
/// packets.
fn check(sample: &str, read: usize, seconds: &str, n: usize) {
    let path = fate(sample);
    let mut ctx = RuntimeContext::new();
    demux_mxf::register(&mut ctx);
    let mut d = ctx.containers.open_demuxer("mxf", Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs).unwrap();
    for _ in 0..read {
        d.next_packet().unwrap();
    }
    let streams = d.streams();
    let stream = streams
        .iter()
        .position(|s| s.params.media_type == MediaType::Video && s.params.width.is_some() && s.params.height.is_some())
        .or_else(|| streams.iter().position(|s| s.params.media_type == MediaType::Audio && s.params.sample_rate.is_some()))
        .unwrap_or(0);
    let tb = streams[stream].time_base.as_rational();
    // av_seek_frame: AV_TIME_BASE to the stream's time base, rounded.
    let micros = (seconds.parse::<f64>().unwrap() * 1e6).round() as i128;
    let target = ((micros * tb.den as i128 + 500_000 * tb.num as i128) / (1_000_000 * tb.num as i128)) as i64;
    d.seek_to(stream as u32, target).unwrap();
    let got: Vec<Pkt> = (0..n)
        .map(|_| {
            let p = d.next_packet().unwrap();
            (p.stream_index, p.pts, p.dts, p.data.len(), p.flags.keyframe, refcheck::md5_hex(&p.data))
        })
        .collect();
    let want = ffprobe_after_seek(&path, seconds, n);
    assert_eq!(want.len(), n, "{sample}: FFmpeg's {n} packets after the seek to {seconds}");
    assert_eq!(got, want, "{sample}: the {n} packets after the seek to {seconds}");
}

#[test]
fn long_gop_mpeg2_lands_on_the_key_frame_its_temporal_offsets_give() {
    check("mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf", 30, "0.3", 12);
    check("mxf/omneon_8.3.0.0_xdcam_startc_footer.mxf", 0, "0.5", 12);
}

#[test]
fn long_gop_h264_lands_where_ffmpeg_does() {
    check("h264/SonyXAVC_LongGOP_green_pixelation_early_Frames.MXF", 12, "0.2", 10);
}

#[test]
fn constant_byte_count_indexes_land_on_the_edit_unit() {
    check("mxf/Avid-00005.mxf", 20, "0.48", 9);
}

/// Sony-00001's index ends before its essence does: FFmpeg cannot seek
/// it ("Operation not permitted"), and neither does this demuxer; reading
/// goes on where it was.
#[test]
fn an_index_that_does_not_reach_the_target_fails_the_seek_as_ffmpegs() {
    let path = fate("mxf/Sony-00001.mxf");
    let out = Command::new("ffprobe").args(["-v", "error", "-read_intervals", "0.04%+#2", "-show_packets"]).arg(&path).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("Could not seek"), "FFmpeg's seek fails");
    let open = || {
        let mut ctx = RuntimeContext::new();
        demux_mxf::register(&mut ctx);
        ctx.containers.open_demuxer("mxf", Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs).unwrap()
    };
    let (mut plain, mut seeked) = (open(), open());
    for d in [&mut plain, &mut seeked] {
        d.next_packet().unwrap();
    }
    assert!(seeked.seek_to(0, 1).is_err(), "the seek fails");
    for _ in 0..6 {
        let (a, b) = (plain.next_packet().unwrap(), seeked.next_packet().unwrap());
        assert_eq!((a.stream_index, a.pts, a.dts, a.data), (b.stream_index, b.pts, b.dts, b.data), "reading goes on");
    }
}

#[test]
fn opatom_audio_seeks_by_the_index_it_was_given() {
    check("mxf/opatom_missing_index.mxf", 1, "0.02", 2);
    check("mxf/track_02_a01.mxf", 3, "0.1", 6);
}

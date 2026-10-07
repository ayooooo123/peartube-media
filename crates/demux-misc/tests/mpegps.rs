//! MPEG-PS demuxer against FFmpeg's mpegps demuxer and parsers.
//!
//! Inputs: every FATE sample FFmpeg's tests demux with mpegps
//! (tests/fate/*.mak: video.mak fate-cavs, ffmpeg.mak's bsf, trim and
//! time_base tests, mpegps.mak, pcm.mak fate-pcm_dvd and fate-pcm_dvda),
//! the DVD still-frame VOB of the suite, and sub/vobsub.sub, the raw
//! program stream of FATE's VobSub pair.
//!
//! Streams exist at open and demuxing never changes them; they match
//! ffprobe's in order, type, codec and the parameters FFmpeg reports.
//! FFmpeg's demuxer returns PES payloads that its parsers then re-frame.
//! This demuxer re-frames what its decoders need whole, with FFmpeg's
//! parsers: DVD subpictures (dvdsub), MPEG audio (mpegaudio), AC-3 /
//! E-AC-3 (ac3) and H.264 (h264). Those streams compare with ffprobe's
//! parsed packet table, timestamps FFmpeg fills in included; every other
//! stream compares with the unparsed one (`-fflags +noparse+nofillin`).
//! Every packet's payload MD5, size, pts and dts, and the interleaving
//! of the unparsed streams.

use std::collections::HashMap;
use std::path::Path;

use oxideav_core::{Demuxer, MediaType, StreamInfo};
use refcheck::fate;

struct FfStream {
    codec_type: String,
    codec_name: String,
    width: Option<u32>,
    height: Option<u32>,
    sample_rate: Option<u32>,
    channels: Option<u16>,
}

#[derive(Debug, PartialEq)]
struct Pkt {
    stream: u32,
    size: usize,
    md5: String,
    pts: Option<i64>,
    dts: Option<i64>,
}

/// `ffprobe -f mpeg` on `path`: streams, then packets (`parsed` false adds
/// `-fflags +noparse+nofillin`). Fails unless ffprobe succeeds with at
/// least one stream and one packet.
fn ffprobe(path: &Path, parsed: bool) -> (Vec<FfStream>, Vec<Pkt>) {
    let mut cmd = std::process::Command::new("ffprobe");
    cmd.args(["-v", "error", "-f", "mpeg"]);
    if !parsed {
        cmd.args(["-fflags", "+noparse+nofillin"]);
    }
    let out = cmd
        .args(["-show_data_hash", "md5", "-show_entries"])
        .arg(
            "stream=codec_name,codec_type,width,height,sample_rate,channels:\
             packet=stream_index,pts,dts,size,data_hash",
        )
        .args(["-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    let (mut streams, mut packets) = (Vec::new(), Vec::new());
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut fields = line.split('|');
        let section = fields.next().unwrap_or("");
        let kv: HashMap<&str, &str> = fields.filter_map(|f| f.split_once('=')).collect();
        match section {
            "packet" => packets.push(Pkt {
                stream: kv["stream_index"].parse().unwrap(),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            }),
            "stream" => streams.push(FfStream {
                codec_type: kv["codec_type"].to_string(),
                codec_name: kv["codec_name"].to_string(),
                width: num(kv.get("width")).filter(|&w| w > 0).map(|w| w as u32),
                height: num(kv.get("height")).filter(|&h| h > 0).map(|h| h as u32),
                sample_rate: num(kv.get("sample_rate")).filter(|&r| r > 0).map(|r| r as u32),
                channels: num(kv.get("channels")).filter(|&c| c > 0).map(|c| c as u16),
            }),
            _ => {}
        }
    }
    assert!(!streams.is_empty() && !packets.is_empty(), "ffprobe {}: empty oracle", path.display());
    (streams, packets)
}

fn media_name(t: MediaType) -> &'static str {
    match t {
        MediaType::Video => "video",
        MediaType::Audio => "audio",
        MediaType::Subtitle => "subtitle",
        MediaType::Data => "data",
        MediaType::Unknown => "unknown",
    }
}

/// The container the player's probe rule picks (engine-api.md): the best
/// content probe that scores at least an extension match on the first
/// 256 KiB, else the extension's container.
fn probe(ctx: &oxideav_core::RuntimeContext, path: &Path) -> Option<String> {
    use std::io::Read;
    let mut head = Vec::new();
    std::fs::File::open(path).unwrap().take(256 * 1024).read_to_end(&mut head).unwrap();
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let data = oxideav_core::ProbeData { buf: &head, ext: ext.as_deref() };
    match ctx.containers.probe_candidates(&data).first() {
        Some(c) if c.score >= oxideav_core::PROBE_SCORE_EXTENSION => Some(c.name.to_string()),
        _ => ext.as_deref().and_then(|e| ctx.containers.container_for_extension(e)).map(str::to_string),
    }
}

/// Open `path` the way the player does (probe, then the winning demuxer
/// of the full registry) and read every packet.
fn demux(path: &Path) -> (Vec<StreamInfo>, Vec<StreamInfo>, Vec<Pkt>) {
    let ctx = codecs::context();
    let format = probe(&ctx, path).unwrap_or_default();
    assert_eq!(format, "mpeg", "{}: container", path.display());
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer: Box<dyn Demuxer> = ctx.containers.open_demuxer(&format, Box::new(file), &ctx.codecs).unwrap();
    let at_open = demuxer.streams().to_vec();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => packets.push(Pkt {
                stream: p.stream_index,
                size: p.data.len(),
                md5: refcheck::md5_hex(&p.data),
                pts: p.pts,
                dts: p.dts,
            }),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
    (at_open, demuxer.streams().to_vec(), packets)
}

/// The streams whose packets are parsed units.
fn reframed(stream: &StreamInfo) -> bool {
    matches!(stream.params.codec_id.as_str(), "dvd_subtitle" | "mp1" | "mp2" | "mp3" | "ac3" | "eac3" | "h264")
}

fn check(rel: &str) {
    let path = fate(rel);
    let (ff_streams, unparsed) = ffprobe(&path, false);
    let (_, parsed) = ffprobe(&path, true);
    let (at_open, at_end, ours) = demux(&path);

    assert_eq!(
        at_open.iter().map(|s| (s.index, s.params.codec_id.as_str().to_string())).collect::<Vec<_>>(),
        at_end.iter().map(|s| (s.index, s.params.codec_id.as_str().to_string())).collect::<Vec<_>>(),
        "{rel}: streams changed after open"
    );
    assert_eq!(at_open.len(), ff_streams.len(), "{rel}: stream count");
    for (s, f) in at_open.iter().zip(&ff_streams) {
        let i = s.index;
        assert_eq!(media_name(s.params.media_type), f.codec_type, "{rel}: stream {i} type");
        assert_eq!(s.params.codec_id.as_str(), f.codec_name, "{rel}: stream {i} codec");
        match s.params.media_type {
            MediaType::Video => {
                assert_eq!((s.params.width, s.params.height), (f.width, f.height), "{rel}: stream {i} size")
            }
            MediaType::Audio => assert_eq!(
                (s.params.sample_rate, s.params.channels),
                (f.sample_rate, f.channels),
                "{rel}: stream {i} rate/channels"
            ),
            _ => {}
        }
    }

    for s in &at_open {
        let oracle = if reframed(s) { &parsed } else { &unparsed };
        let want: Vec<&Pkt> = oracle.iter().filter(|p| p.stream == s.index).collect();
        let got: Vec<&Pkt> = ours.iter().filter(|p| p.stream == s.index).collect();
        let what = if reframed(s) { "parsed" } else { "PES" };
        assert_eq!(got.len(), want.len(), "{rel}: stream {} {what} packet count", s.index);
        for (n, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{rel}: stream {} {what} packet {n}", s.index);
        }
    }
    let pes_streams: Vec<u32> = at_open.iter().filter(|s| !reframed(s)).map(|s| s.index).collect();
    let order = |pkts: &[Pkt]| pkts.iter().filter(|p| pes_streams.contains(&p.stream)).map(|p| (p.stream, p.md5.clone())).collect::<Vec<_>>();
    assert_eq!(order(&ours), order(&unparsed), "{rel}: PES interleaving");
}

/// video.mak fate-cavs: DVD navigation data, CAVS video, MP2 audio.
#[test]
fn cavs_mpg() {
    check("cavs/cavs.mpg");
}

/// ffmpeg.mak fate-ffmpeg-bsf-remove-*, fate-ffmpeg-trim-bsf-*.
#[test]
fn matrixbench_mpeg2() {
    check("mpeg2/matrixbench_mpeg2.lq1.mpg");
}

/// ffmpeg.mak fate-time_base: MPEG-2 still frame with two DVD subpicture
/// streams.
#[test]
fn dvd_single_frame() {
    check("mpeg2/dvd_single_frame.vob");
}

/// DVD still frame with subpictures and AC-3 audio.
#[test]
fn dvd_still_frame() {
    check("mpeg2/dvd_still_frame.vob");
}

/// mpegps.mak fate-mpegps-remuxed-pcm-demux.
#[test]
fn pcm_aud() {
    check("mpegps/pcm_aud.mpg");
}

/// pcm.mak fate-pcm_dvd.
#[test]
fn pcm_dvd_vob() {
    check("pcm-dvd/coolitnow-partial.vob");
}

/// pcm.mak fate-pcm_dvda: DVD-Audio LPCM in an AOB. PCM_DVDA exists in
/// FFmpeg's source tree (2da55bf), whose mpeg.c this demuxer ports, but
/// not in the installed release, which reads the stream as MLP. The
/// oracle is that tree's own ffmpeg: its stream line, and the unparsed
/// packets through `-c copy -copyts -f framemd5`.
#[test]
fn pcm_dvda_aob() {
    let rel = "pcm-dvda/pcm_dvda-96k24bit.aob";
    let path = fate(rel);
    let src = std::env::var_os("FFMPEG_SRC")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(&std::env::var("HOME").unwrap()).join("projects/ffmpeg-src"));
    let ffmpeg = src.join("ffmpeg");
    assert!(ffmpeg.is_file(), "build ffmpeg in FFmpeg's source tree {} (commit 2da55bf)", src.display());
    let run = |args: &[&str]| {
        let out = std::process::Command::new(&ffmpeg)
            .args(["-hide_banner", "-f", "mpeg", "-fflags", "+noparse", "-i"])
            .arg(&path)
            .args(args)
            .output()
            .unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
    };
    let (ok, table, _) = run(&["-map", "0", "-c", "copy", "-copyts", "-f", "framemd5", "-"]);
    assert!(ok, "{rel}: 2da55bf ffmpeg framemd5 failed");
    // "#stream#, dts, pts, duration, size, hash"
    let want: Vec<Pkt> = table
        .lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<&str> = l.split(',').map(str::trim).collect();
            Pkt {
                stream: f[0].parse().unwrap(),
                size: f[4].parse().unwrap(),
                md5: f[5].to_string(),
                pts: f[2].parse().ok(),
                dts: f[1].parse().ok(),
            }
        })
        .collect();
    assert!(!want.is_empty(), "{rel}: empty oracle");
    // "Stream #0:0[0xa0]: Audio: pcm_dvda, 96000 Hz, stereo, s32 (24 bit), ..."
    let (_, _, banner) = run(&["-f", "null", "-"]);
    let line = banner.lines().find(|l| l.contains("Stream #0:0")).expect("stream line");
    let fields: Vec<&str> = line.split("Audio: ").nth(1).expect("audio stream").split(", ").collect();
    let rate: u32 = fields[1].trim_end_matches(" Hz").parse().unwrap();
    let channels = match fields[2] {
        "mono" => 1,
        "stereo" => 2,
        other => other.trim_end_matches(" channels").parse().unwrap(),
    };

    let (at_open, at_end, ours) = demux(&path);
    assert_eq!(at_open.len(), 1, "{rel}: streams");
    assert_eq!(at_end.len(), 1, "{rel}: streams after demuxing");
    let s = &at_open[0];
    assert_eq!(s.params.codec_id.as_str(), fields[0], "{rel}: codec");
    assert_eq!((s.params.sample_rate, s.params.channels), (Some(rate), Some(channels)), "{rel}: rate/channels");
    assert_eq!(ours, want, "{rel}: PES packets");
}

/// FATE's VobSub pair as a raw program stream: 87 subpicture PES packets
/// that the dvdsub parser assembles into 43 units, a PES without a PTS
/// continuing the unit before it.
#[test]
fn vobsub_sub() {
    check("sub/vobsub.sub");
}

// ─── H.264 ───

/// 0.8 s of 720x480 testsrc at 25 fps (20 frames) through the libx264 of
/// the ffmpeg on PATH, yuv444p as it encodes RGB input by default, with
/// B-frames, and `args`, muxed by FFmpeg's VOB muxer: units of a few
/// hundred bytes, several to a 2048-byte pack, the larger ones split
/// across packs.
fn libx264_vob(name: &str, args: &[&str]) -> std::path::PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("demux-misc-mpegps-{}-{name}", std::process::id()));
    let out = std::process::Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=720x480:rate=25:duration=0.8"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv444p"])
        .args(args)
        .args(["-f", "vob"])
        .arg(&path)
        .output()
        .expect("ffmpeg must be on PATH");
    assert!(out.status.success(), "{name}: ffmpeg: {}", String::from_utf8_lossy(&out.stderr));
    path
}

/// ffprobe's parsed packets of `path` with their key flags.
fn ffprobe_keyed(path: &Path) -> Vec<(Pkt, bool)> {
    let out = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-f", "mpeg", "-show_data_hash", "md5", "-show_entries"])
        .arg("packet=stream_index,pts,dts,size,flags,data_hash")
        .args(["-of", "compact"])
        .arg(path)
        .output()
        .expect("ffprobe must be on PATH");
    assert!(out.status.success(), "ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr));
    let num = |v: Option<&&str>| v.and_then(|v| v.parse::<i64>().ok());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("packet|"))
        .map(|fields| {
            let kv: HashMap<&str, &str> = fields.split('|').filter_map(|f| f.split_once('=')).collect();
            let packet = Pkt {
                stream: kv["stream_index"].parse().unwrap(),
                size: kv["size"].parse().unwrap(),
                md5: kv["data_hash"].trim_start_matches("MD5:").to_string(),
                pts: num(kv.get("pts")),
                dts: num(kv.get("dts")),
            };
            (packet, kv["flags"].starts_with('K'))
        })
        .collect()
}

/// Every packet of `path` with its key flag, through the player's
/// registry.
fn demux_keyed(path: &Path) -> Vec<(Pkt, bool)> {
    let ctx = codecs::context();
    let file = std::fs::File::open(path).unwrap();
    let mut demuxer = ctx.containers.open_demuxer("mpeg", Box::new(file), &ctx.codecs).unwrap();
    let mut packets = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => packets.push((
                Pkt { stream: p.stream_index, size: p.data.len(), md5: refcheck::md5_hex(&p.data), pts: p.pts, dts: p.dts },
                p.flags.keyframe,
            )),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{}: demux: {e}", path.display()),
        }
    }
    packets
}

/// H.264 comes out as FFmpeg's h264 parser cuts it (mpeg.c gives every
/// stream AVSTREAM_PARSE_FULL): whole access units, keyed as the parser
/// keys them. A unit takes the timestamps of the PES it starts in when no
/// unit before it started there (parser.c ff_fetch_timestamp); the others
/// have none, unless buffering period and picture timing SEIs let the
/// parser time every unit (h264_parser.c h264_parse).
#[test]
fn h264_units_are_ffmpeg_parser_units() {
    let hrd = ["-x264-params", "nal-hrd=vbr", "-b:v", "1M", "-maxrate", "1M", "-bufsize", "2M"];
    for (name, args, untimed) in [("h264.vob", &[][..], true), ("h264-hrd.vob", &hrd[..], false)] {
        let path = libx264_vob(name, args);
        let want = ffprobe_keyed(&path);
        assert_eq!(want.len(), 20, "{name}: FFmpeg's units");
        assert_eq!(want.iter().any(|(p, _)| p.pts.is_none()), untimed, "{name}: FFmpeg leaves units untimed");
        let got = demux_keyed(&path);
        assert_eq!(got.len(), want.len(), "{name}: packet count");
        for (n, (g, w)) in got.iter().zip(&want).enumerate() {
            assert_eq!(g, w, "{name}: packet {n} (packet, key)");
        }
    }
}

/// The Player's video sink, hashing each frame the engine presents,
/// packed at the 720x480 4:4:4 picture FFmpeg decodes (the container
/// declares no picture size, which the headless sink packs at).
mod hashed {
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::time::Duration;

    use oxideav_core::{CodecParameters, Packet, VideoFrame};
    use player::backend::{AudioSink, Backend, Clock, SinkError, SubtitleSink, VideoSink};
    use player::Headless;

    pub struct Hashing {
        pub headless: Arc<Headless>,
        pub md5: Sender<String>,
    }

    impl Backend for Hashing {
        fn audio(&self) -> Box<dyn AudioSink> {
            self.headless.audio()
        }
        fn video(&self, clock: Arc<dyn Clock>) -> Box<dyn VideoSink> {
            Box::new(HashedVideo { sink: self.headless.video(clock), md5: self.md5.clone() })
        }
        fn subtitles(&self) -> Box<dyn SubtitleSink> {
            self.headless.subtitles()
        }
    }

    struct HashedVideo {
        sink: Box<dyn VideoSink>,
        md5: Sender<String>,
    }

    impl VideoSink for HashedVideo {
        fn open_compressed(&mut self, params: &CodecParameters) -> bool {
            self.sink.open_compressed(params)
        }
        fn push_packet(&mut self, packet: &Packet, pts: Duration, random_access: bool) -> Result<(), SinkError> {
            self.sink.push_packet(packet, pts, random_access)
        }
        fn open_frames(&mut self, params: &CodecParameters) -> Result<(), SinkError> {
            self.sink.open_frames(params)
        }
        fn push_frame(&mut self, frame: &VideoFrame, pts: Duration) -> Result<(), SinkError> {
            let _ = self.md5.send(refcheck::md5_hex(&refcheck::pack(frame, &[(720, 480); 3])));
            self.sink.push_frame(frame, pts)
        }
        fn frame_lead(&self) -> Duration {
            self.sink.frame_lead()
        }
        fn finish(&mut self) -> Result<(), SinkError> {
            self.sink.finish()
        }
        fn flush(&mut self) {
            self.sink.flush()
        }
        fn set_playing(&mut self, playing: bool) {
            self.sink.set_playing(playing)
        }
    }
}

/// The Player (headless backend, every codec of the app) presents every
/// frame FFmpeg decodes from the VOB, as FFmpeg decodes it, none dropped.
/// Played as fast as the pipeline goes: the engine presents a frame at
/// its packet's pts, which FFmpeg leaves unset for most of these units.
#[test]
fn player_presents_every_ffmpeg_frame_of_an_h264_vob() {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let path = libx264_vob("h264-player.vob", &[]);
    let want = refcheck::ffmpeg_video_md5s(&path, 0, "yuv444p");
    assert_eq!(want.len(), 20, "FFmpeg's frames");
    let (md5, presented) = std::sync::mpsc::channel();
    let backend = Arc::new(hashed::Hashing { headless: player::Headless::new(), md5 });
    let options = player::PlayerOptions { realtime: false, ..player::PlayerOptions::default() };
    let player = player::Player::open(path.to_str().unwrap(), backend, Arc::new(codecs::context()), options, |_| {});
    let deadline = Instant::now() + Duration::from_secs(60);
    let state = loop {
        let state = player.state();
        assert!(state.error.is_none(), "{state:?}");
        if state.ended {
            break state;
        }
        assert!(Instant::now() < deadline, "playback did not end: {state:?}");
        std::thread::sleep(Duration::from_millis(5));
    };
    drop(player);
    assert_eq!(state.dropped_frames, 0, "frames dropped");
    assert_eq!(presented.try_iter().collect::<Vec<_>>(), want, "every presented frame");
}

// ─── private stream 1 substream routing ───

/// An MPEG-2 pack header (SCR 0, mux rate 1, no stuffing).
const PACK: [u8; 14] = [0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 0, 0, 3, 0xF8];

/// A private stream 1 PES with a PTS, carrying `payload` (substream id
/// first).
fn private_pes(pts: u32, payload: &[u8]) -> Vec<u8> {
    pes(0xBD, Some(pts), payload)
}

/// An MPEG-2 PES of `stream_id`, with a PTS when given.
fn pes(stream_id: u8, pts: Option<u32>, payload: &[u8]) -> Vec<u8> {
    let header = if pts.is_some() { 5 } else { 0 };
    let len = 3 + header + payload.len();
    let mut pes = vec![0, 0, 1, stream_id, (len >> 8) as u8, len as u8, 0x81, if pts.is_some() { 0x80 } else { 0 }, header as u8];
    if let Some(pts) = pts {
        // '0010' PTS[32..30] '1' PTS[29..15] '1' PTS[14..0] '1'
        let pts = u64::from(pts);
        pes.push(0x21 | (((pts >> 30) & 7) << 1) as u8);
        pes.extend_from_slice(&((((pts >> 15) & 0x7FFF) << 1 | 1) as u16).to_be_bytes());
        pes.extend_from_slice(&(((pts & 0x7FFF) << 1 | 1) as u16).to_be_bytes());
    }
    pes.extend_from_slice(payload);
    pes
}

/// VLC's ps.h routes private stream 1 substreams 0x00-0x03 to its CVD
/// decoder and 0x70 to its OGT decoder, both reading the substream id at
/// the head of the payload (cvdsub.c strips one byte, svcdsub.c the five
/// of 0x70, channel, packet number and image number). FFmpeg skips both;
/// DVD subpictures and AC-3 keep FFmpeg's routing.
#[test]
fn private_stream_1_substreams_keep_their_ids_for_cvd_and_ogt() {
    let cvd = [0x00, 0x00, 0x0C, 0x34, 0x56];
    let ogt = [0x70, 0x00, 0x80, 0x00, 0x01, 0x00, 0x09, 0xAB];
    let spu = [0x20, 0x00, 0x04, 0x01, 0x02];
    let ac3 = [0x80, 0x01, 0x00, 0x01, 0x0B, 0x77, 0xAA, 0xBB];
    let mut ps = Vec::new();
    for (i, payload) in [&cvd[..], &ogt[..], &spu[..], &ac3[..]].iter().enumerate() {
        ps.extend_from_slice(&PACK);
        ps.extend(private_pes(9000 * (i as u32 + 1), payload));
    }
    ps.extend_from_slice(&[0, 0, 1, 0xB9]);

    let ctx = codecs::context();
    let mut demuxer = ctx
        .containers
        .open_demuxer("mpeg", Box::new(std::io::Cursor::new(ps)), &ctx.codecs)
        .unwrap();
    let streams: Vec<(MediaType, String)> =
        demuxer.streams().iter().map(|s| (s.params.media_type, s.params.codec_id.as_str().to_string())).collect();
    assert_eq!(
        streams,
        [
            (MediaType::Subtitle, "cvd_subtitle".to_string()),
            (MediaType::Subtitle, "ogt".to_string()),
            (MediaType::Subtitle, "dvd_subtitle".to_string()),
            (MediaType::Audio, "ac3".to_string()),
        ]
    );
    let mut got = Vec::new();
    while let Ok(p) = demuxer.next_packet() {
        got.push((p.stream_index, p.pts, p.data));
    }
    assert_eq!(
        got,
        [
            (0, Some(9000), cvd.to_vec()),
            (1, Some(18000), ogt.to_vec()),
            // the subpicture unit without its substream id
            (2, Some(27000), spu[1..].to_vec()),
            // AC-3 without the substream id and its 3-byte header
            (3, Some(36000), ac3[4..].to_vec()),
        ]
    );
}

// ─── discovery budget ───

/// FFmpeg's default probesize: the input stream discovery may read.
const PROBE_SIZE: usize = 5_000_000;

/// An MPEG-1 Layer II frame, 48 kHz 192 kbit/s stereo without CRC (576
/// bytes, 1152 samples = 2160 ticks), its body all `fill`.
fn mp2_frame(fill: u8) -> Vec<u8> {
    let mut frame = vec![fill; 576];
    frame[..4].copy_from_slice(&[0xFF, 0xFD, 0xA4, 0x00]);
    frame
}

/// `len` bytes or more of input the demuxer passes over without a
/// packet, in one of the forms it skips.
fn skipped(form: &str, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 64 * 1024);
    while out.len() < len {
        match form {
            // no start code anywhere
            "junk" => out.resize(len, 0xA5),
            "padding" => {
                out.extend_from_slice(&[0, 0, 1, 0xBE, 0xEA, 0x60]);
                out.resize(out.len() + 0xEA60, 0xFF);
            }
            // private stream 1 substream 0xFF: no codec, never a stream
            "unsupported substream" => {
                let mut payload = vec![0x11; 60_000];
                payload[0] = 0xFF;
                out.extend(pes(0xBD, None, &payload));
            }
            // not Sofdec, not DVD navigation
            "private stream 2" => {
                out.extend_from_slice(&[0, 0, 1, 0xBF, 0xEA, 0x60]);
                out.resize(out.len() + 0xEA60, 0x22);
            }
            "pack headers" => out.extend_from_slice(&PACK),
            _ => unreachable!(),
        }
    }
    out
}

/// A reader that records how far into the input it has read.
struct Watched {
    inner: std::io::Cursor<Vec<u8>>,
    furthest: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl std::io::Read for Watched {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut self.inner, buf)?;
        self.furthest.fetch_max(self.inner.position(), std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

impl std::io::Seek for Watched {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        std::io::Seek::seek(&mut self.inner, pos)
    }
}

/// MP2 stream 0xC0, then input the demuxer skips, then the rest of 0xC0
/// and a stream first met past the skipped input (0xC1), before the rest
/// when `new_stream_first`. 0xC0's fourth frame starts before the skipped
/// input and ends after it. Opening reads at most the probe size whatever
/// the form of the skipped input, and does not take the budget for the end
/// of the input: 0xC0 is the only stream, and all six of its frames come
/// out whole, timed from their PES.
fn discovery_stops_at_the_probe_size(form: &str, skipped_len: impl Fn(usize) -> usize, new_stream_first: bool) {
    let frames: Vec<Vec<u8>> = (0..6).map(|k| mp2_frame(0x50 + k)).collect();
    let other = [PACK.to_vec(), pes(0xC1, Some(9000), &mp2_frame(0x60))].concat();
    let mut ps = PACK.to_vec();
    ps.extend(pes(0xC0, Some(9000), &[frames[0].clone(), frames[1].clone()].concat()));
    ps.extend_from_slice(&PACK);
    ps.extend(pes(0xC0, Some(9000 + 2 * 2160), &[&frames[2][..], &frames[3][..300]].concat()));
    let region = skipped(form, skipped_len(ps.len()));
    ps.extend_from_slice(&region);
    if new_stream_first {
        ps.extend_from_slice(&other);
    }
    ps.extend(pes(0xC0, None, &[&frames[3][300..], &frames[4][..]].concat()));
    if !new_stream_first {
        ps.extend_from_slice(&other);
    }
    ps.extend(pes(0xC0, Some(9000 + 5 * 2160), &frames[5]));
    ps.extend_from_slice(&[0, 0, 1, 0xB9]);

    let furthest = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let input = Watched { inner: std::io::Cursor::new(ps), furthest: furthest.clone() };
    let ctx = codecs::context();
    let mut demuxer = ctx.containers.open_demuxer("mpeg", Box::new(input), &ctx.codecs).unwrap();
    let read_at_open = furthest.load(std::sync::atomic::Ordering::Relaxed);
    let streams: Vec<(u32, String)> =
        demuxer.streams().iter().map(|s| (s.index, s.params.codec_id.as_str().to_string())).collect();
    assert_eq!(streams, [(0, "mp2".to_string())], "{form}: streams at open");
    assert!(
        read_at_open <= (PROBE_SIZE + 256 * 1024) as u64,
        "{form}: open read {read_at_open} bytes of input, probe size {PROBE_SIZE}"
    );
    let mut got = Vec::new();
    loop {
        match demuxer.next_packet() {
            Ok(p) => got.push((p.stream_index, p.pts, p.data)),
            Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("{form}: demux: {e}"),
        }
    }
    let want: Vec<(u32, Option<i64>, Vec<u8>)> =
        frames.iter().enumerate().map(|(k, f)| (0, Some(9000 + 2160 * k as i64), f.clone())).collect();
    assert_eq!(got.len(), want.len(), "{form}: packets {:?}", got.iter().map(|g| (g.0, g.1, g.2.len())).collect::<Vec<_>>());
    for (n, (g, w)) in got.iter().zip(&want).enumerate() {
        assert!(g == w, "{form}: packet {n}: stream {} pts {:?} {} bytes, want pts {:?}", g.0, g.1, g.2.len(), w.1);
    }
}

#[test]
fn discovery_stops_at_the_probe_size_in_junk() {
    discovery_stops_at_the_probe_size("junk", |_| 6_000_000, true);
}

#[test]
fn discovery_stops_at_the_probe_size_in_padding() {
    discovery_stops_at_the_probe_size("padding", |_| 6_000_000, true);
}

#[test]
fn discovery_stops_at_the_probe_size_in_unsupported_substreams() {
    discovery_stops_at_the_probe_size("unsupported substream", |_| 6_000_000, true);
}

#[test]
fn discovery_stops_at_the_probe_size_in_private_stream_2() {
    discovery_stops_at_the_probe_size("private stream 2", |_| 6_000_000, true);
}

#[test]
fn discovery_stops_at_the_probe_size_in_pack_headers() {
    discovery_stops_at_the_probe_size("pack headers", |_| 6_000_000, true);
}

/// The start code of 0xC0's continuation straddles the probe size: the
/// scan that hit the budget leaves it to playback whole.
#[test]
fn discovery_keeps_a_start_code_across_the_probe_size() {
    discovery_stops_at_the_probe_size("junk", |head| PROBE_SIZE - 2 - head, false);
}

/// CRC-16/ANSI (x^16 + x^15 + x^2 + 1, MSB first), as AC-3 syncframes use.
fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x8005 } else { crc << 1 };
        }
    }
    crc
}

/// 64 KiB of AC-3 syncframe headers whose CRC fails, then about 300 000
/// one-byte AC-3 PES packets: an input that keeps an audio stream's
/// parameters unknown for the whole probe size. Discovery must look at
/// what each packet adds, not scan the stream's head again per packet.
#[test]
fn discovery_does_not_rescan_a_parameterless_head_per_packet() {
    // AC-3, 48 kHz, frmsizecod 0 (128-byte frames), bsid 8: a header the
    // parser accepts every 8 bytes, each frame failing its CRC.
    let unit = [0x0B, 0x77, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00];
    let frame: Vec<u8> = unit.iter().copied().cycle().take(128).collect();
    assert_ne!(crc16(&frame[2..]), 0, "the fixture's syncframes must fail their CRC");
    let syncs: Vec<u8> = unit.iter().copied().cycle().take(32 * 1024).collect();
    let mut ps = Vec::new();
    for _ in 0..2 {
        ps.extend_from_slice(&PACK);
        ps.extend(private_pes(9000, &[&[0x80, 0x01, 0x00, 0x01][..], &syncs].concat()));
    }
    let tiny = pes(0xBD, None, &[0x80, 0x01, 0x00, 0x01, 0x77]);
    for _ in 0..300_000 {
        ps.extend_from_slice(&tiny);
    }
    ps.extend_from_slice(&[0, 0, 1, 0xB9]);
    assert!(ps.len() < PROBE_SIZE, "discovery reads the whole input");

    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ctx = codecs::context();
        let demuxer = ctx.containers.open_demuxer("mpeg", Box::new(std::io::Cursor::new(ps)), &ctx.codecs).unwrap();
        let streams: Vec<(String, Option<u32>)> =
            demuxer.streams().iter().map(|s| (s.params.codec_id.as_str().to_string(), s.params.sample_rate)).collect();
        let _ = done.send(streams);
    });
    let streams = finished
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("open did not finish within 20 s");
    assert_eq!(streams, [("ac3".to_string(), None)]);
}

/// 100,000 one-byte MPEG audio PES without a timestamp, within the probe
/// size: discovery delivers each to a stream whose head never holds an
/// audio header (0xFF bytes). Whether a delivery completed one depends
/// on the bytes it added, so opening takes time linear in the PES count:
/// it ends within 12 s, where rescanning the head (up to 64 KiB) after
/// every PES takes over half a minute.
#[test]
fn discovery_stays_linear_in_one_byte_audio_pes() {
    let mut ps = PACK.to_vec();
    for _ in 0..100_000 {
        ps.extend(pes(0xC0, None, &[0xFF]));
    }
    ps.extend_from_slice(&[0, 0, 1, 0xB9]);
    let (opened, result) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let ctx = codecs::context();
        let demuxer = ctx.containers.open_demuxer("mpeg", Box::new(std::io::Cursor::new(ps)), &ctx.codecs);
        let _ = opened.send(demuxer.map(|d| d.streams().iter().map(|s| s.params.codec_id.as_str().to_string()).collect::<Vec<_>>()));
    });
    let streams = result.recv_timeout(std::time::Duration::from_secs(12)).expect("open ends within 12 s");
    assert_eq!(streams.unwrap(), ["mp2"], "the audio stream");
}

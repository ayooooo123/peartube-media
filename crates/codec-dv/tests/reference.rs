//! dvvideo and the raw `dv` demuxer against FFmpeg 2da55bf's.
//!
//! Video: every frame's MD5 equals FFmpeg's (its C simple IDCT, `-idct
//! simple`, as its DV decoder uses that IDCT off arm64), in the pixel
//! format and size FFmpeg's decoder reports, which the decoder reports too.
//! Samples: the FATE ones (DVCPRO HD 1080i50, 1080p25 and 720p50 in MOV, DV
//! NTSC 4:1:1 in MOV, DV PAL in MXF) and DV of every profile FFmpeg's
//! vsynth tests encode, made by FFmpeg: DV25 4:2:0 and 4:1:1 (NTSC and PAL),
//! DVCPRO50 PAL and NTSC, DVCPRO HD 720p50, 720p60, 1080i50 and 1080i60, in
//! raw DV and in AVI.
//!
//! Raw DV: the packet table (stream, pts, dts, duration, size, key, data
//! MD5) equals ffprobe's, and every stereo pair's PCM equals FFmpeg's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;

use oxideav_core::{Error, Frame, MediaType, PixelFormat, RuntimeContext};
use refcheck::{fate, Registrar};

fn mov(ctx: &mut RuntimeContext) {
    oxideav_mov::registry::register(ctx);
}

fn avi(ctx: &mut RuntimeContext) {
    oxideav_avi::__oxideav_entry(ctx);
}

fn mxf(ctx: &mut RuntimeContext) {
    demux_mxf::register(ctx);
}

fn raw(ctx: &mut RuntimeContext) {
    codec_dv::register(ctx);
    oxideav_basic::__oxideav_entry(ctx);
}

/// FFmpeg's pixel format and size for stream `0:v:0`.
fn ffprobe_video(path: &Path) -> (String, usize, usize) {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=pix_fmt,width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let f: Vec<&str> = text.lines().next().expect("a video stream").split(',').collect();
    let (w, h) = (f[0].parse().unwrap(), f[1].parse().unwrap());
    (f[2].to_string(), w, h)
}

/// The planes FFmpeg's `pix_fmt` has at `w`x`h`.
fn plane_dims(pix_fmt: &str, w: usize, h: usize) -> Vec<(usize, usize)> {
    let (cw, ch) = match pix_fmt {
        "yuv420p" => (w / 2, h / 2),
        "yuv411p" => (w / 4, h),
        "yuv422p" => (w / 2, h),
        other => panic!("not a DV pixel format: {other}"),
    };
    vec![(w, h), (cw, ch), (cw, ch)]
}

/// The decoder's reports for the first frame of the first video stream.
fn decoder_reports(path: &Path, registrars: &[Registrar]) -> (Option<PixelFormat>, Option<(u32, u32)>) {
    let mut ctx = RuntimeContext::new();
    for r in registrars {
        r(&mut ctx);
    }
    let format = refcheck::probe_container(&ctx, path).unwrap();
    let mut d = ctx.containers.open_demuxer(&format, Box::new(std::fs::File::open(path).unwrap()), &ctx.codecs).unwrap();
    let stream = d.streams().iter().find(|s| s.params.media_type == MediaType::Video).unwrap().clone();
    let mut decoder = ctx.codecs.first_decoder(&stream.params).unwrap();
    loop {
        let p = d.next_packet().unwrap();
        if p.stream_index == stream.index {
            decoder.send_packet(&p).unwrap();
            decoder.receive_frame().unwrap();
            return (decoder.output_pixel_format(), decoder.output_video_dimensions());
        }
    }
}

fn check_video(path: &Path, registrars: &[Registrar]) {
    let name = path.display().to_string();
    let (pix_fmt, w, h) = ffprobe_video(path);
    let (format, dims) = decoder_reports(path, registrars);
    assert_eq!(format.map(refcheck::ffmpeg_pix_fmt), Some(pix_fmt.as_str()), "{name}: the decoder's pixel format");
    assert_eq!(dims, Some((w as u32, h as u32)), "{name}: the decoder's size");
    let decoded = refcheck::decode(path, registrars, MediaType::Video, 0);
    let dims = plane_dims(&pix_fmt, w, h);
    let got: Vec<String> = decoded
        .frames
        .iter()
        .map(|f| {
            let Frame::Video(vf) = f else { panic!("{name}: not a video frame") };
            refcheck::md5_hex(&refcheck::pack(vf, &dims))
        })
        .collect();
    let want = refcheck::ffmpeg_video_md5s_with(path, 0, &pix_fmt, &["-idct", "simple"]);
    assert!(!want.is_empty(), "{name}: FFmpeg's frames");
    let first = got.iter().zip(&want).position(|(g, w)| g != w);
    assert!(first.is_none(), "{name}: frame {first:?} of {} differs from FFmpeg's", got.len());
    assert_eq!(got.len(), want.len(), "{name}: frames");
}

#[test]
fn dvcpro_hd_1080i50_in_mov() {
    check_video(&fate("dv/dvcprohd_1080i50.mov"), &[codec_dv::register, mov]);
}

#[test]
fn dvcpro_hd_1080p25_in_mov() {
    check_video(&fate("dv/dvcprohd_1080p25.mov"), &[codec_dv::register, mov]);
}

#[test]
fn dvcpro_hd_720p50_in_mov() {
    check_video(&fate("dv/dvcprohd_720p50.mov"), &[codec_dv::register, mov]);
}

#[test]
fn dv_ntsc_411_in_mov() {
    check_video(&fate("mov/fcp_export8-236.mov"), &[codec_dv::register, mov]);
}

#[test]
fn dv_pal_420_in_mxf() {
    check_video(&fate("mxf/Avid-00005.mxf"), &[codec_dv::register, mxf]);
}

/// The DV FFmpeg makes once per test run: (file, testsrc size and rate,
/// sine sample rate where it has audio, encoder pixel format); `.dv` files
/// are raw DV.
const MADE: [(&str, &str, Option<&str>, &str); 10] = [
    ("pal420.dv", "720x576:rate=25", Some("48000"), "yuv420p"),
    ("pal411_44k.dv", "720x576:rate=25", Some("44100"), "yuv411p"),
    ("ntsc411.dv", "720x480:rate=30000/1001", Some("48000"), "yuv411p"),
    ("dv50_pal.dv", "720x576:rate=25", Some("48000"), "yuv422p"),
    ("dv50_ntsc.dv", "720x480:rate=30000/1001", Some("48000"), "yuv422p"),
    ("hd720p50.dv", "960x720:rate=50", None, "yuv422p"),
    ("hd720p60.dv", "960x720:rate=60000/1001", None, "yuv422p"),
    ("hd1080i50.dv", "1440x1080:rate=25", Some("48000"), "yuv422p"),
    ("hd1080i60.dv", "1280x1080:rate=30000/1001", Some("48000"), "yuv422p"),
    ("pal420.avi", "720x576:rate=25", Some("48000"), "yuv420p"),
];

static GENERATED: LazyLock<PathBuf> = LazyLock::new(|| {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("codec-dv-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, video, audio, pix_fmt) in MADE {
        let mut cmd = Command::new(refcheck::system_ffmpeg());
        cmd.args(["-nostdin", "-v", "error", "-y", "-f", "lavfi", "-i"]).arg(format!("testsrc=size={video}:duration=1"));
        if let Some(rate) = audio {
            cmd.args(["-f", "lavfi", "-i"]).arg(format!("sine=frequency=1000:duration=1:sample_rate={rate}"));
        }
        cmd.args(["-c:v", "dvvideo", "-pix_fmt", pix_fmt]);
        if audio.is_some() {
            cmd.args(["-c:a", "pcm_s16le", "-ac", "2"]);
        }
        if name.ends_with(".dv") {
            cmd.args(["-f", "dv"]);
        }
        let out = cmd.arg(dir.join(name)).output().expect("ffmpeg on PATH");
        assert!(out.status.success(), "{name}: {}", String::from_utf8_lossy(&out.stderr));
    }
    dir
});

fn made(name: &str) -> PathBuf {
    GENERATED.join(name)
}

fn raw_names() -> impl Iterator<Item = &'static str> {
    MADE.iter().map(|m| m.0).filter(|n| n.ends_with(".dv"))
}

#[test]
fn every_profile_in_raw_dv_decodes_as_ffmpeg() {
    for name in raw_names() {
        check_video(&made(name), &[raw]);
    }
}

#[test]
fn dv_in_avi_decodes_as_ffmpeg() {
    check_video(&made("pal420.avi"), &[codec_dv::register, avi]);
}

type Pkt = (u32, Option<i64>, Option<i64>, Option<i64>, usize, bool, String);

/// ffprobe's codecs and packet table.
fn ffprobe_packets(path: &Path) -> (Vec<String>, Vec<Pkt>) {
    let out = Command::new(refcheck::pinned_ffprobe())
        .args(["-v", "error", "-show_data_hash", "md5", "-of", "compact"])
        .args(["-show_entries", "stream=codec_name:packet=stream_index,pts,dts,duration,size,flags,data_hash"])
        .arg(path)
        .output()
        .expect("the pinned ffprobe runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let kv = |l: &str| l.split('|').filter_map(|f| f.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect::<HashMap<_, _>>();
    let num = |m: &HashMap<String, String>, k: &str| m.get(k).and_then(|v| v.parse::<i64>().ok());
    let codecs = text.lines().filter_map(|l| l.strip_prefix("stream|")).map(|l| kv(l)["codec_name"].clone()).collect();
    let packets = text
        .lines()
        .filter_map(|l| l.strip_prefix("packet|"))
        .map(|l| {
            let m = kv(l);
            (
                m["stream_index"].parse().unwrap(),
                num(&m, "pts"),
                num(&m, "dts"),
                num(&m, "duration"),
                m["size"].parse().unwrap(),
                m["flags"].starts_with('K'),
                m["data_hash"].trim_start_matches("MD5:").to_string(),
            )
        })
        .collect();
    (codecs, packets)
}

#[test]
fn raw_dv_packets_equal_ffprobe() {
    for name in raw_names() {
        let path = made(name);
        let mut ctx = RuntimeContext::new();
        raw(&mut ctx);
        assert_eq!(refcheck::probe_container(&ctx, &path).unwrap(), "dv", "{name}: probed as raw DV");
        let mut d = ctx.containers.open_demuxer("dv", Box::new(std::fs::File::open(&path).unwrap()), &ctx.codecs).unwrap();
        let codecs: Vec<String> = d.streams().iter().map(|s| s.params.codec_id.as_str().to_string()).collect();
        let mut got = Vec::new();
        loop {
            match d.next_packet() {
                Ok(p) => got.push((p.stream_index, p.pts, p.dts, p.duration, p.data.len(), p.flags.keyframe, refcheck::md5_hex(&p.data))),
                Err(Error::Eof) => break,
                Err(e) => panic!("{name}: {e} after {} packets", got.len()),
            }
        }
        let (want_codecs, want) = ffprobe_packets(&path);
        assert_eq!(codecs, want_codecs, "{name}: streams");
        let first = got.iter().zip(&want).position(|(g, w)| g != w);
        if let Some(i) = first {
            panic!("{name}: packet {i} is {:?}, FFmpeg's {:?}", got[i], want[i]);
        }
        assert_eq!(got.len(), want.len(), "{name}: packets");
    }
}

/// Every stereo pair the raw demuxer pulls out of the audio DIF blocks
/// equals FFmpeg's, sample for sample.
#[test]
fn raw_dv_audio_equals_ffmpeg() {
    for (name, _, audio, _) in MADE {
        if audio.is_none() || !name.ends_with(".dv") {
            continue;
        }
        let path = made(name);
        let (codecs, _) = ffprobe_packets(&path);
        let pairs = codecs.iter().filter(|c| c.as_str() == "pcm_s16le").count();
        assert!(pairs > 0, "{name}: FFmpeg's audio");
        for nth in 0..pairs {
            let decoded = refcheck::decode(&path, &[raw], MediaType::Audio, nth);
            let ours = refcheck::interleaved_f32(&decoded);
            let reference = refcheck::ffmpeg_audio_f32(&path, nth);
            assert_eq!(ours.len(), reference.len(), "{name} pair {nth}: samples");
            let snr = refcheck::snr_db(&reference, &ours, 0);
            assert!(snr.is_infinite(), "{name} pair {nth}: equals FFmpeg's ({snr} dB)");
        }
    }
}

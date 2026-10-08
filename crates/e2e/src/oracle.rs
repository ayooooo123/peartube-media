//! FFmpeg's side of every comparison: its view of the streams, and its
//! decode of one stream, addressed by `-map` specifier.

use std::path::Path;
use std::time::Duration;

use crate::tool;

/// How long one FFmpeg reference may take. Real samples decode in seconds;
/// a hang means a parser loop.
const TIMEOUT: Duration = Duration::from_secs(180);

fn path_arg(path: &Path) -> Result<String, String> {
    path.to_str().map(str::to_string).ok_or_else(|| format!("{} is not UTF-8", path.display()))
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| s.to_string()).collect()
}

/// One stream as `ffprobe -show_streams` reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct FfStream {
    /// Absolute stream index (`-map 0:<index>`).
    pub index: u32,
    /// `video`, `audio`, `subtitle`, `data`, `attachment`.
    pub codec_type: String,
    pub codec_name: String,
    /// The container tag, e.g. `XVID` or `[0][0][0][0]`.
    pub codec_tag: String,
    pub sample_fmt: Option<String>,
    pub sample_rate: Option<u32>,
    pub channels: Option<u16>,
    /// Cover art: FFmpeg lists it as a video stream, OxideAV as an attached
    /// picture of the container.
    pub attached_pic: bool,
}

impl FfStream {
    /// `-map` specifier of this stream.
    pub fn map(&self) -> String {
        format!("0:{}", self.index)
    }
}

/// Every stream of `path`, in FFmpeg's order.
pub fn streams(path: &Path) -> Result<Vec<FfStream>, String> {
    let out = tool::ffprobe(
        &[
            "-show_entries".to_string(),
            "stream=index,codec_type,codec_name,codec_tag_string,sample_fmt,sample_rate,channels:stream_disposition=attached_pic"
                .into(),
            "-of".into(),
            "json".into(),
            path_arg(path)?,
        ],
        Duration::from_secs(60),
    )?;
    let json: serde_json::Value =
        serde_json::from_slice(&out).map_err(|e| format!("ffprobe streams output: {e}"))?;
    let list = json["streams"].as_array().cloned().unwrap_or_default();
    Ok(list
        .iter()
        .map(|s| FfStream {
            index: s["index"].as_u64().unwrap_or(0) as u32,
            codec_type: s["codec_type"].as_str().unwrap_or("").to_string(),
            codec_name: s["codec_name"].as_str().unwrap_or("").to_string(),
            codec_tag: s["codec_tag_string"].as_str().unwrap_or("").to_string(),
            sample_fmt: s["sample_fmt"].as_str().map(str::to_string),
            sample_rate: s["sample_rate"].as_str().and_then(|r| r.parse().ok()),
            channels: s["channels"].as_u64().map(|c| c as u16),
            attached_pic: s["disposition"]["attached_pic"].as_u64() == Some(1),
        })
        .collect())
}

/// The streams of `codec_type` a player can select: FFmpeg's streams of that
/// type without cover art, in order.
pub fn of_type<'a>(streams: &'a [FfStream], codec_type: &str) -> Vec<&'a FfStream> {
    streams.iter().filter(|s| s.codec_type == codec_type && !s.attached_pic).collect()
}

/// How many packets FFmpeg's demuxer reads for stream `index` of `path`.
/// With `-count_packets`, ffprobe states `N/A` for a stream it read no
/// packet of, and lists a program's streams again in its program.
pub fn packet_count(path: &Path, index: u32) -> Result<u64, String> {
    let out = tool::ffprobe(
        &[
            "-count_packets".to_string(),
            "-select_streams".into(),
            index.to_string(),
            "-show_entries".into(),
            "stream=nb_read_packets".into(),
            "-of".into(),
            "csv=p=0".into(),
            path_arg(path)?,
        ],
        Duration::from_secs(60),
    )?;
    let text = String::from_utf8_lossy(&out);
    match text.lines().next().map(str::trim) {
        Some("N/A") => Ok(0),
        Some(n) => n.parse().map_err(|e| format!("ffprobe nb_read_packets {n:?}: {e}")),
        None => Err(format!("ffprobe lists no stream {index}")),
    }
}

/// MD5 of every frame the reference decodes from stream `map` (codec
/// `codec_name`), through refcheck's video oracle. AV1 comes from libdav1d
/// in the system FFmpeg: the pinned build has no software AV1 decoder.
/// Everything else comes from the pinned build with FFmpeg's C IDCT pinned
/// (`-idct simple`, 6ac540e): the IDCT codecs port FFmpeg's C
/// `simple_idct`, which arm64 FFmpeg replaces with NEON assembly that
/// rounds differently by default, and decoders without an IDCT ignore the
/// option.
pub fn video_md5s(path: &Path, map: &str, codec_name: &str, pix_fmt: &str) -> Result<Vec<String>, String> {
    let out = if codec_name == "av1" {
        tool::system_ffmpeg(&refcheck::ffmpeg_video_md5_args(path, map, pix_fmt, &["-c:v", "libdav1d"]), TIMEOUT)?
    } else {
        tool::ffmpeg(&refcheck::ffmpeg_video_md5_args(path, map, pix_fmt, &["-idct", "simple"]), TIMEOUT)?
    };
    Ok(refcheck::parse_framemd5(&String::from_utf8_lossy(&out)))
}

/// A canonical interleaved PCM encoding FFmpeg can write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pcm {
    U8,
    S16,
    S32,
    F32,
    F64,
}

impl Pcm {
    /// The encoding of FFmpeg's decoder output `sample_fmt` (packed or
    /// planar), interleaved.
    pub fn of_sample_fmt(sample_fmt: &str) -> Option<Pcm> {
        Some(match sample_fmt.trim_end_matches('p') {
            "u8" => Pcm::U8,
            "s16" => Pcm::S16,
            "s32" => Pcm::S32,
            "flt" => Pcm::F32,
            "dbl" => Pcm::F64,
            _ => return None,
        })
    }

    pub fn bytes(self) -> usize {
        match self {
            Pcm::U8 => 1,
            Pcm::S16 => 2,
            Pcm::S32 | Pcm::F32 => 4,
            Pcm::F64 => 8,
        }
    }

    fn ffmpeg(self) -> (&'static str, &'static str) {
        match self {
            Pcm::U8 => ("u8", "pcm_u8"),
            Pcm::S16 => ("s16le", "pcm_s16le"),
            Pcm::S32 => ("s32le", "pcm_s32le"),
            Pcm::F32 => ("f32le", "pcm_f32le"),
            Pcm::F64 => ("f64le", "pcm_f64le"),
        }
    }
}

/// The code paths a stream's reference decode runs: the pinned FFmpeg's
/// defaults, or, for the decoders ported from its C code (AC-3 and
/// E-AC-3), its C paths (`-cpuflags 0`), since its assembly rounds
/// differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paths {
    Default,
    C,
}

impl Paths {
    pub fn of(ff: &FfStream) -> Paths {
        match ff.codec_name.as_str() {
            "ac3" | "eac3" => Paths::C,
            _ => Paths::Default,
        }
    }

    fn ffmpeg(self, args: &[String]) -> Result<Vec<u8>, String> {
        match self {
            Paths::Default => tool::ffmpeg(args, TIMEOUT),
            Paths::C => tool::ffmpeg_c(args, TIMEOUT),
        }
    }

    fn ffprobe(self, args: &[String]) -> Result<Vec<u8>, String> {
        match self {
            Paths::Default => tool::ffprobe(args, TIMEOUT),
            Paths::C => tool::ffprobe_c(args, TIMEOUT),
        }
    }
}

/// FFmpeg's decode of stream `ff` as interleaved little-endian `pcm`.
pub fn audio_pcm(path: &Path, ff: &FfStream, pcm: Pcm) -> Result<Vec<u8>, String> {
    let (format, codec) = pcm.ffmpeg();
    let p = path_arg(path)?;
    Paths::of(ff).ffmpeg(&strings(&["-i", &p, "-map", &ff.map(), "-f", format, "-c:a", codec, "-"]))
}

/// FFmpeg's decode of stream `ff` as interleaved f32.
pub fn audio_f32(path: &Path, ff: &FfStream) -> Result<Vec<f32>, String> {
    let bytes = audio_pcm(path, ff, Pcm::F32)?;
    Ok(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
}

/// One frame of FFmpeg's decoder output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFrameInfo {
    pub nb_samples: u32,
    pub channels: u16,
}

/// The frames FFmpeg's decoder emits for stream `ff` (`ffprobe
/// -show_frames`): their sizes bound how far a lossy decode may run long
/// or short.
pub fn audio_frames(path: &Path, ff: &FfStream) -> Result<Vec<AudioFrameInfo>, String> {
    let p = path_arg(path)?;
    let index = ff.index.to_string();
    let out = Paths::of(ff).ffprobe(&strings(&[
        "-select_streams",
        &index,
        "-show_entries",
        "frame=nb_samples,channels",
        "-of",
        "csv=p=0",
        &p,
    ]))?;
    let text = String::from_utf8_lossy(&out);
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut it = l.split(',').map(str::trim);
            let nb = it.next().and_then(|v| v.parse().ok());
            let ch = it.next().and_then(|v| v.parse().ok());
            match (nb, ch) {
                (Some(nb_samples), Some(channels)) => Ok(AudioFrameInfo { nb_samples, channels }),
                _ => Err(format!("ffprobe frame line {l:?}")),
            }
        })
        .collect()
}

/// Whether FFmpeg decodes stream `map` without error: a `decodes` policy is
/// only for streams FFmpeg cannot produce a reference for.
pub fn decodes(path: &Path, map: &str) -> bool {
    let Ok(p) = path_arg(path) else { return false };
    tool::ffmpeg(&strings(&["-i", &p, "-map", map, "-f", "null", "-"]), TIMEOUT).is_ok()
}

/// One cue of FFmpeg's decode, as its `srt` encoder writes it.
#[derive(Clone, Debug, PartialEq)]
pub struct SrtCue {
    /// `hh:mm:ss,mmm --> hh:mm:ss,mmm`
    pub timing: String,
    pub body: String,
}

/// FFmpeg's decode of the text subtitle stream `map`, re-encoded as SubRip:
/// every cue's timing (to the millisecond) and body, in order.
pub fn subtitle_srt(path: &Path, map: &str) -> Result<Vec<SrtCue>, String> {
    let p = path_arg(path)?;
    let out = tool::ffmpeg(&strings(&["-i", &p, "-map", map, "-c:s", "srt", "-f", "srt", "-"]), TIMEOUT)?;
    Ok(parse_srt(&String::from_utf8_lossy(&out)))
}

/// The cues of a SubRip document: a counter line, a timing line, then body
/// lines up to the next counter-and-timing pair.
pub fn parse_srt(text: &str) -> Vec<SrtCue> {
    let lines: Vec<&str> = text.lines().collect();
    let starts_cue = |i: usize| {
        let l = lines[i].trim();
        !l.is_empty() && l.chars().all(|c| c.is_ascii_digit()) && lines.get(i + 1).is_some_and(|n| n.contains("-->"))
    };
    let mut cues = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !starts_cue(i) {
            i += 1;
            continue;
        }
        let timing = lines[i + 1].trim().to_string();
        i += 2;
        let mut body = Vec::new();
        while i < lines.len() && !starts_cue(i) {
            body.push(lines[i]);
            i += 1;
        }
        cues.push(SrtCue { timing, body: body.join("\n").trim().to_string() });
    }
    cues
}

/// One subtitle FFmpeg's decoder emitted (`ffprobe -show_frames`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SubEvent {
    pub start_us: i64,
    /// `None` when the subtitle stays up until the next one (PGS).
    pub end_us: Option<i64>,
    /// Bitmaps shown; 0 clears the screen.
    pub rects: u32,
}

/// The subtitles FFmpeg decodes from stream `index`.
pub fn subtitle_events(path: &Path, index: u32) -> Result<Vec<SubEvent>, String> {
    let p = path_arg(path)?;
    let index = index.to_string();
    let out = tool::ffprobe(&strings(&["-select_streams", &index, "-show_frames", "-of", "json", &p]), TIMEOUT)?;
    let json: serde_json::Value = serde_json::from_slice(&out).map_err(|e| format!("ffprobe frames output: {e}"))?;
    let frames = json["frames"].as_array().cloned().unwrap_or_default();
    frames
        .iter()
        .map(|f| {
            // AVSubtitle.pts is in microseconds; display times in ms after it.
            let pts = f["pts"].as_i64().ok_or("subtitle frame without pts")?;
            let start = f["start_display_time"].as_i64().unwrap_or(0);
            let end = f["end_display_time"].as_i64().filter(|&e| e != u32::MAX as i64);
            Ok(SubEvent {
                start_us: pts + start * 1000,
                end_us: end.map(|e| pts + e * 1000),
                rects: f["num_rects"].as_u64().unwrap_or(0) as u32,
            })
        })
        .collect()
}

/// The canvases FFmpeg composes stream `index`'s bitmaps on (sub2video, as
/// RGBA): the canvas size and the MD5 of each canvas state, in order.
pub fn subtitle_canvases(path: &Path, index: u32) -> Result<((usize, usize), Vec<String>), String> {
    let p = path_arg(path)?;
    let graph = format!("[0:{index}]format=rgba[o]");
    let out = tool::ffmpeg(
        &strings(&["-i", &p, "-filter_complex", &graph, "-map", "[o]", "-fps_mode", "passthrough", "-f", "framemd5", "-"]),
        TIMEOUT,
    )?;
    let text = String::from_utf8_lossy(&out);
    let dims = text
        .lines()
        .find_map(|l| l.strip_prefix("#dimensions 0:"))
        .and_then(|d| d.trim().split_once('x'))
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or("sub2video output without dimensions")?;
    Ok((dims, refcheck::parse_framemd5(&text)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_lists_fate_sample_streams_in_ffmpeg_order() {
        let s = streams(&refcheck::fate("wmv8/wmv8_x8intra.wmv")).unwrap();
        let summary: Vec<(u32, &str, &str)> =
            s.iter().map(|s| (s.index, s.codec_type.as_str(), s.codec_name.as_str())).collect();
        assert_eq!(summary, [(0, "audio", "wmav2"), (1, "video", "wmv2")]);
        assert_eq!(s[0].sample_fmt.as_deref(), Some("fltp"));
        assert_eq!(s[0].channels, Some(1));
    }

    #[test]
    fn the_video_oracle_pins_the_c_idct_for_idct_codecs_only() {
        // MPEG-2 builds its IDCT through ff_idctdsp_init(avctx->idct_algo).
        let mpeg2 = refcheck::fate("mpeg2/matrixbench_mpeg2.lq1.mpg");
        let pinned = video_md5s(&mpeg2, "0:0", "mpeg2video", "yuv420p").unwrap();
        assert_eq!(pinned, refcheck::ffmpeg_video_md5s_with(&mpeg2, 0, "yuv420p", &["-idct", "simple"]));
        #[cfg(target_arch = "aarch64")]
        assert_ne!(pinned, refcheck::ffmpeg_video_md5s(&mpeg2, 0, "yuv420p"), "arm64's default NEON IDCT rounds differently");
        // H.264 has no IDCT option: the pin changes nothing.
        let h264 = refcheck::fate("mkv/1242-small.mkv");
        assert_eq!(video_md5s(&h264, "0:1", "h264", "yuv420p").unwrap(), refcheck::ffmpeg_video_md5s(&h264, 0, "yuv420p"));
    }

    #[test]
    fn subtitle_srt_reads_ffmpegs_decoded_cues() {
        let cues = subtitle_srt(&refcheck::fate("sub/SubRip_capability_tester.srt"), "0:0").unwrap();
        assert!(cues.len() > 10, "{}", cues.len());
        assert!(cues.iter().all(|c| c.timing.contains(" --> ")));
    }

    #[test]
    fn parse_srt_keeps_multi_line_bodies_and_digit_lines() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\nline one\n2024\n\n2\n00:00:03,000 --> 00:00:04,500\nlast\n";
        let cues = parse_srt(srt);
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].body, "line one\n2024");
        assert_eq!(cues[1].timing, "00:00:03,000 --> 00:00:04,500");
    }

    #[test]
    fn pgs_events_and_canvases_come_from_ffmpegs_decoder() {
        let path = refcheck::fate("sub/pgs_sub.sup");
        let events = subtitle_events(&path, 0).unwrap();
        assert!(events.iter().any(|e| e.rects > 0), "{events:?}");
        let ((w, h), canvases) = subtitle_canvases(&path, 0).unwrap();
        assert_eq!((w, h), (1920, 1080));
        assert!(!canvases.is_empty());
    }

    #[test]
    fn pcm_follows_the_decoder_sample_format() {
        assert_eq!(Pcm::of_sample_fmt("s16p"), Some(Pcm::S16));
        assert_eq!(Pcm::of_sample_fmt("s32"), Some(Pcm::S32));
        assert_eq!(Pcm::of_sample_fmt("fltp"), Some(Pcm::F32));
        assert_eq!(Pcm::of_sample_fmt("u8"), Some(Pcm::U8));
        assert_eq!(Pcm::of_sample_fmt("s64"), None);
    }

    #[test]
    fn audio_frames_reports_decoder_frame_sizes() {
        // cook: 1024-sample stereo frames.
        let path = refcheck::fate("real/ra_cook.rm");
        let ff = streams(&path).unwrap().into_iter().find(|s| s.codec_type == "audio").unwrap();
        let frames = audio_frames(&path, &ff).unwrap();
        assert!(!frames.is_empty());
        assert!(frames.iter().all(|f| f.nb_samples == 1024 && f.channels == 2), "{:?}", &frames[..3]);
    }
}
